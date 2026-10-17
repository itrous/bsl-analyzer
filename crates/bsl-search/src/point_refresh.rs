//! Point refresh of the workspace overlay: re-read the files the watcher marked dirty and
//! settle them, with the reading done off every lock.
//!
//! Three phases, so the only work under the engine mutex and the ownership fence is a
//! bounded commit of something already prepared:
//!
//! - **A — capture**, under the engine mutex for a moment: which keys (at most
//!   [`POINT_BATCH_KEYS`], each with the sequence of its mark), the fences a later publication
//!   is judged against, and the inputs the reading needs.
//! - **B — prepare**, under no lock at all: each key's baseline is read POINTWISE through the
//!   caller's own read-only connection, and the file is read, chunked and classified.
//! - **C — publish**, under the engine mutex and the fence: in-memory checks, one short SQLite
//!   transaction (one `SELECT` of the baseline version and at most one `DELETE` per key), and
//!   the settlement of the keys whose marks did not move meanwhile.
//!
//! A refusal in C leaves the prepared batch with the caller, who publishes the SAME batch again
//! without reading a file twice.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::error::SearchError;
use crate::ports::{GraphContextProvider, ModuleSnapshotSource, SnapshotFetch};
use crate::store::Store;
use crate::workspace_overlay::{BaselineHashMode, PointAction, PointBaseline};
use crate::workspace_roots::{FileKey, WorkspaceRoots};

/// The most keys one point batch takes.
pub const POINT_BATCH_KEYS: usize = 64;

/// Bytes of file content one prepared batch may hold beyond its first key: a single key larger
/// than this still makes a batch of its own, and nothing joins it.
pub const PREPARED_BYTES: u64 = 8 * 1024 * 1024;

/// How long phase C waits for SQLite's writer lock before it gives the batch back. A live
/// store otherwise waits out the operational budget — thirty seconds — under the engine mutex
/// and the fence, while the embedding pass holds the writer for as long as it likes.
pub const C_BUSY_TIMEOUT: Duration = Duration::from_millis(100);

thread_local! {
    static BOUNDED_PUBLICATION: Cell<usize> = const { Cell::new(0) };
}

/// Marks the calling thread as inside a bounded publication until dropped: only a commit of
/// prepared work may run there, and each operation that loads or reads a whole baseline, or
/// reads and parses a file, refuses loudly (see [`forbidden_under_a_bounded_publication`]).
pub struct BoundedPublication(());

impl BoundedPublication {
    pub fn enter() -> Self {
        BOUNDED_PUBLICATION.with(|depth| depth.set(depth.get() + 1));
        Self(())
    }
}

impl Drop for BoundedPublication {
    fn drop(&mut self) {
        BOUNDED_PUBLICATION.with(|depth| depth.set(depth.get() - 1));
    }
}

/// Refuse `operation` inside a bounded publication. Checked in debug builds, where every test
/// runs: a slip that puts a whole-baseline load back under the fence fails the suite instead
/// of stalling a workspace that happens to be large.
pub(crate) fn forbidden_under_a_bounded_publication(operation: &str) {
    if cfg!(debug_assertions) && BOUNDED_PUBLICATION.with(Cell::get) > 0 {
        panic!("{operation} inside a bounded publication");
    }
}

/// One captured key: the key, the sequence of the mark being answered, and its failure streak.
#[derive(Debug, Clone)]
pub(crate) struct CapturedKey {
    pub(crate) key: FileKey,
    pub(crate) seq: u64,
    pub(crate) prior_failures: u32,
}

/// Phase A's output: the keys and every fence phase C re-checks.
pub struct PointCapture {
    pub(crate) keys: Vec<CapturedKey>,
    pub(crate) fence: u64,
    pub(crate) wholesale_seq: u64,
    pub(crate) roots_epoch: u64,
    pub(crate) baseline_version: i64,
    pub(crate) roots: WorkspaceRoots,
    pub(crate) hash_mode: BaselineHashMode,
    pub(crate) serves_external_baseline: bool,
    pub(crate) provider: Option<Arc<dyn GraphContextProvider>>,
    pub(crate) source: Option<Arc<dyn ModuleSnapshotSource>>,
    pub(crate) batch_size: usize,
    pub(crate) db_path: PathBuf,
}

/// One prepared key, ready to settle.
pub(crate) struct PreparedKey {
    pub(crate) captured: CapturedKey,
    pub(crate) action: PointAction,
    /// The key's persisted fingerprint row must be retracted with the settlement.
    pub(crate) retract: bool,
    pub(crate) resident_fed: bool,
}

/// Phase B's output. Held by the caller across a refused publication.
pub struct PreparedPointBatch {
    pub(crate) keys: Vec<PreparedKey>,
    pub(crate) fence: u64,
    pub(crate) wholesale_seq: u64,
    pub(crate) roots_epoch: u64,
    pub(crate) baseline_version: i64,
    /// The baseline mode phase B classified these keys under. Phase C re-checks it, because
    /// `set_serves_external_baseline` moves none of the other fences: not `roots_epoch`, not
    /// `wholesale_seq`, not `baseline_version`. Without this a batch classified against a
    /// manifest could settle after the engine had gone local, retracting an overlay entry in
    /// favour of a `files` row the mode change made stale — and the other way round.
    pub(crate) serves_external_baseline: bool,
    pub(crate) bytes: u64,
}

/// What phase C did with a prepared batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointPublish {
    /// Settled `settled` keys, of which `cleared` lost their marks for good — the rest were
    /// marked again by a fault of their own. `stale` keys were re-marked while the batch was
    /// prepared and left for the next one. `remaining` marks are still dirty.
    Applied { settled: usize, cleared: usize, stale: usize, remaining: usize },
    /// The batch was prepared against a state that no longer holds — roots, a wholesale
    /// publication, or the baseline moved. Nothing was applied; the marks stay.
    Discarded,
    /// SQLite's writer lock was held past [`C_BUSY_TIMEOUT`]. Nothing was applied; publish
    /// the same batch again.
    Busy,
}

impl PointCapture {
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Phase B: classify the captured keys off every lock, through `reader`, a connection of
    /// the caller's own that no one else uses. Stops adding keys once the batch holds more
    /// than [`PREPARED_BYTES`] of content, or once `cancelled` says so — asked before each
    /// key, so an owner told to leave reads no further file.
    pub fn prepare(
        self,
        reader: &Store,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PreparedPointBatch, SearchError> {
        let manifest = self.serves_external_baseline && reader.has_baseline_manifest()?;
        if let Some(source) = &self.source {
            source.catch_up();
        }
        let mut keys = Vec::with_capacity(self.keys.len());
        let mut bytes = 0u64;
        for captured in self.keys {
            if cancelled() {
                break;
            }
            // Sized by a stat before the read, so a batch never holds more than the budget or
            // its single largest key: the first key always goes in, and nothing joins a key
            // that alone exceeds the budget.
            let size = self
                .roots
                .resolve(&captured.key)
                .and_then(|path| std::fs::metadata(path).ok())
                .map_or(0, |metadata| metadata.len());
            if !keys.is_empty() && bytes.saturating_add(size) > PREPARED_BYTES {
                break;
            }
            let baseline = if manifest {
                PointBaseline::Manifest {
                    fingerprint: reader.manifest_fingerprint("code", &captured.key)?,
                }
            } else {
                PointBaseline::Raw {
                    hash: reader.file_hash(&captured.key.root_id, &captured.key.path)?,
                    mode: self.hash_mode,
                }
            };
            let mut snapshots = HashMap::new();
            if let (Some(source), Some(path)) = (&self.source, self.roots.resolve(&captured.key)) {
                if let SnapshotFetch::Fetched(snapshot) =
                    source.text_and_parse(&path.to_string_lossy())
                {
                    snapshots.insert(captured.key.clone(), snapshot);
                }
            }
            // Vectors are attached in phase C from the live cache: cloning the whole cache
            // under the mutex in phase A would be the unbounded work this module removes.
            let mut no_vectors = HashMap::new();
            let read = crate::workspace_overlay::classify_point(
                &captured.key,
                &baseline,
                &self.roots,
                &snapshots,
                self.provider.as_deref(),
                None,
                self.batch_size,
                &mut no_vectors,
            );
            bytes = bytes.saturating_add(read.bytes);
            keys.push(PreparedKey {
                captured,
                action: read.action,
                retract: manifest,
                resident_fed: read.resident_fed,
            });
        }
        Ok(PreparedPointBatch {
            keys,
            fence: self.fence,
            wholesale_seq: self.wholesale_seq,
            roots_epoch: self.roots_epoch,
            baseline_version: self.baseline_version,
            serves_external_baseline: self.serves_external_baseline,
            bytes,
        })
    }

    /// Where the caller opens its reader.
    pub fn db_path(&self) -> &std::path::Path {
        &self.db_path
    }
}

impl PreparedPointBatch {
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Bytes of file content the batch holds.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Run all three phases at once, for a test that holds the engine and needs no fence.
#[cfg(test)]
pub(crate) fn point_refresh_for_test(engine: &crate::SearchEngine) -> Option<PointPublish> {
    let capture = engine.capture_point_refresh(POINT_BATCH_KEYS).unwrap()?;
    let reader = Store::open_reader(capture.db_path()).unwrap();
    let mut batch = capture.prepare(&reader, &|| false).unwrap();
    Some(engine.publish_point_refresh(&mut batch).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SearchEngine;
    use std::fs;
    use std::path::Path;

    const OLD: &str = "Процедура Старая() Экспорт\nКонецПроцедуры\n";

    fn raw_engine(workspace: &Path, files: &[(&str, &str)]) -> SearchEngine {
        for (name, text) in files {
            fs::write(workspace.join(name), text).unwrap();
        }
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace);
        engine.index_directory_fts(workspace).unwrap();
        engine.enable_workspace_watcher_mode();
        engine.initialize_workspace_overlay_clean().unwrap();
        engine
    }

    fn manifest_engine(workspace: &Path, files: &[(&str, &str)]) -> SearchEngine {
        for (name, text) in files {
            fs::write(workspace.join(name), text).unwrap();
        }
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace);
        engine.set_serves_external_baseline(true).unwrap();
        engine
            .store()
            .save_baseline_manifest(&crate::WorkspaceBaselineManifest {
                snapshot_id: "snap".to_owned(),
                snapshot_fingerprint: None,
                files: files
                    .iter()
                    .map(|(name, text)| crate::BaselineManifestFile {
                        root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                        collection: "code".to_owned(),
                        path: (*name).to_owned(),
                        file_fingerprint: crate::workspace_overlay::fingerprint_content(text, name),
                        document_count: 1,
                        file_object_id: format!("object-{name}"),
                    })
                    .collect(),
            })
            .unwrap();
        engine.enable_workspace_watcher_mode();
        engine.initialize_workspace_overlay_clean().unwrap();
        engine
    }

    fn edit_and_mark(engine: &SearchEngine, workspace: &Path, name: &str, text: &str) {
        fs::write(workspace.join(name), text).unwrap();
        assert!(engine.mark_workspace_path_dirty(workspace.join(name)).unwrap());
    }

    fn prepare(engine: &SearchEngine) -> PreparedPointBatch {
        let capture = engine.capture_point_refresh(POINT_BATCH_KEYS).unwrap().expect("dirty keys");
        let reader = Store::open_reader(capture.db_path()).unwrap();
        capture.prepare(&reader, &|| false).unwrap()
    }

    fn found(engine: &SearchEngine, symbol: &str) -> bool {
        !engine.text_search_read_only(symbol, 10, Some("code")).unwrap().is_empty()
    }

    fn dirty(engine: &SearchEngine) -> usize {
        engine.workspace_overlay_dirty_paths().unwrap().len()
    }

    /// A batch classified in one baseline mode may not settle in the other.
    ///
    /// Phase B decides each key against a manifest fingerprint or against the local `files`
    /// hash, and which of the two it used follows from `serves_external_baseline`. That flag
    /// moves NONE of the other fences phase C re-checks — not `roots_epoch`, not
    /// `wholesale_seq`, not `baseline_version` — so before this check a batch prepared in
    /// external mode could settle after the engine had gone local, retracting an overlay entry
    /// in favour of a `files` row the mode change had just made stale. The roots plan has
    /// treated a mode change as superseding all along; the point path was reading a
    /// classification made under rules that no longer held.
    #[test]
    fn a_baseline_mode_change_discards_a_prepared_point_batch() {
        for (from, to) in [(true, false), (false, true)] {
            let dir = tempfile::tempdir().unwrap();
            let files = [("Module.bsl", OLD)];
            let mut engine = if from {
                manifest_engine(dir.path(), &files)
            } else {
                raw_engine(dir.path(), &files)
            };
            edit_and_mark(
                &engine,
                dir.path(),
                "Module.bsl",
                "Процедура Новая() Экспорт\nКонецПроцедуры\n",
            );
            let mut batch = prepare(&engine);

            engine.set_serves_external_baseline(to).unwrap();

            assert_eq!(
                engine.publish_point_refresh(&mut batch).unwrap(),
                PointPublish::Discarded,
                "{from} → {to}: a batch classified in the other mode was applied",
            );
            assert_eq!(dirty(&engine), 1, "{from} → {to}: the discarded batch dropped its mark");
        }
    }

    #[test]
    fn a_point_refresh_settles_an_edit_in_both_baseline_modes() {
        for manifest in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let files = [("Module.bsl", OLD)];
            let engine = if manifest {
                manifest_engine(dir.path(), &files)
            } else {
                raw_engine(dir.path(), &files)
            };
            edit_and_mark(
                &engine,
                dir.path(),
                "Module.bsl",
                "Процедура Новая() Экспорт\nКонецПроцедуры\n",
            );
            let mut batch = prepare(&engine);
            assert_eq!(
                engine.publish_point_refresh(&mut batch).unwrap(),
                PointPublish::Applied { settled: 1, cleared: 1, stale: 0, remaining: 0 },
                "manifest={manifest}"
            );
            assert!(found(&engine, "Новая"), "manifest={manifest}: the edit is not served");
        }
    }

    /// Phase C is one transaction: one `SELECT` of the baseline version and at most one
    /// `DELETE` per key — a fingerprint row per key in manifest mode, none in raw mode.
    #[test]
    fn phase_c_is_one_transaction_with_bounded_statements() {
        for (manifest, deletes) in [(false, 0), (true, 3)] {
            let dir = tempfile::tempdir().unwrap();
            let files = [("A.bsl", OLD), ("B.bsl", OLD), ("C.bsl", OLD)];
            let engine = if manifest {
                manifest_engine(dir.path(), &files)
            } else {
                raw_engine(dir.path(), &files)
            };
            for name in ["A.bsl", "B.bsl", "C.bsl"] {
                edit_and_mark(&engine, dir.path(), name, "Процедура Другая()\nКонецПроцедуры\n");
            }
            let mut batch = prepare(&engine);
            let before = crate::store::POINT_SQL.with(Cell::get);
            engine.publish_point_refresh(&mut batch).unwrap();
            let after = crate::store::POINT_SQL.with(Cell::get);
            assert_eq!(
                (after.0 - before.0, after.1 - before.1, after.2 - before.2),
                (1, 1, deletes),
                "manifest={manifest}: (transactions, selects, deletes)"
            );
        }
    }

    /// Nothing that loads a whole baseline, or reads a file, may run inside a bounded
    /// publication — and phase C, in both modes, runs there without tripping it. The old
    /// point path, which loaded the baseline under the fence, trips it.
    #[test]
    fn a_bounded_publication_refuses_whole_baseline_loads_and_phase_c_needs_none() {
        for manifest in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let files = [("Module.bsl", OLD)];
            let engine = if manifest {
                manifest_engine(dir.path(), &files)
            } else {
                raw_engine(dir.path(), &files)
            };
            edit_and_mark(
                &engine,
                dir.path(),
                "Module.bsl",
                "Процедура Под() Экспорт\nКонецПроцедуры\n",
            );
            let mut batch = prepare(&engine);
            let published = {
                let _bounded = BoundedPublication::enter();
                engine.publish_point_refresh(&mut batch).unwrap()
            };
            assert!(matches!(published, PointPublish::Applied { settled: 1, .. }));

            edit_and_mark(
                &engine,
                dir.path(),
                "Module.bsl",
                "Процедура Снова() Экспорт\nКонецПроцедуры\n",
            );
            // The refreshing status read settles marks over a whole-baseline load — what the old
            // point path did under the fence.
            let whole_load = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _bounded = BoundedPublication::enter();
                engine.workspace_overlay_stats()
            }));
            assert!(
                whole_load.is_err(),
                "manifest={manifest}: a whole-baseline load went unnoticed"
            );
        }
        let store = Store::in_memory().unwrap();
        for load in [&(|| drop(store.all_files_in_collection("code"))) as &dyn Fn(), &|| {
            drop(store.load_baseline_manifest_fingerprints("code"))
        }] {
            let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _bounded = BoundedPublication::enter();
                load();
            }));
            assert!(refused.is_err());
        }
    }

    /// A store fault inside phase C rolls the transaction back and leaves the overlay and the
    /// marks exactly as they were.
    #[test]
    fn a_store_fault_in_phase_c_rolls_back_and_keeps_the_marks() {
        let dir = tempfile::tempdir().unwrap();
        let engine = manifest_engine(dir.path(), &[("Module.bsl", OLD)]);
        let key = crate::FileKey::configuration("Module.bsl");
        let mut rows = HashMap::new();
        rows.insert(
            key.clone(),
            crate::store::PersistedFingerprint {
                file_size: 1,
                file_mtime_secs: 0,
                file_mtime_nanos: 0,
                content_fingerprint: "stale".to_owned(),
                canonical: String::new(),
            },
        );
        engine.store().save_overlay_fingerprint_cache("snap", &rows).unwrap();
        rusqlite::Connection::open(engine.db_path())
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER deny_retraction BEFORE DELETE ON overlay_fingerprint_cache \
                 BEGIN SELECT RAISE(ABORT, 'denied'); END;",
            )
            .unwrap();
        edit_and_mark(
            &engine,
            dir.path(),
            "Module.bsl",
            "Процедура Отказ() Экспорт\nКонецПроцедуры\n",
        );
        let mut batch = prepare(&engine);

        assert!(engine.publish_point_refresh(&mut batch).is_err());
        assert_eq!(dirty(&engine), 1, "the mark was lost with the rolled-back transaction");
        assert!(!found(&engine, "Отказ"), "memory moved although the commit did not");
        assert_eq!(engine.store().overlay_fingerprint_keys().unwrap().len(), 1);
    }

    /// A batch prepared against a state that moved — the baseline written through another
    /// connection, the roots, a wholesale publication — publishes nothing, and the marks stay.
    #[test]
    fn a_batch_prepared_against_a_moved_state_is_discarded() {
        type Shift<'a> = &'a dyn Fn(&mut SearchEngine, &Path);
        let moves: [(&str, Shift); 2] = [
            ("baseline", &|engine, _| {
                let other = Store::open(engine.db_path()).unwrap();
                other.upsert_file("", "Other.bsl", b"h", "code").unwrap();
            }),
            ("wholesale", &|engine, _| engine.initialize_workspace_overlay_clean().unwrap()),
        ];
        for (label, shift) in moves {
            let dir = tempfile::tempdir().unwrap();
            let mut engine = raw_engine(dir.path(), &[("Module.bsl", OLD)]);
            edit_and_mark(
                &engine,
                dir.path(),
                "Module.bsl",
                "Процедура Сдвиг() Экспорт\nКонецПроцедуры\n",
            );
            let mut batch = prepare(&engine);
            shift(&mut engine, dir.path());
            if label == "wholesale" {
                assert!(engine.mark_workspace_path_dirty(dir.path().join("Module.bsl")).unwrap());
            }
            assert_eq!(
                engine.publish_point_refresh(&mut batch).unwrap(),
                PointPublish::Discarded,
                "{label}"
            );
            assert_eq!(dirty(&engine), 1, "{label}: the mark went with the discarded batch");
            assert!(!found(&engine, "Сдвиг"), "{label}: a stale batch published");
        }
    }

    /// Keys are spelled against the root table the batch was captured under; once the table
    /// moves, the batch describes keys that may no longer exist as captured.
    #[test]
    fn a_batch_prepared_before_the_roots_moved_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let (cf, ext) = (workspace.join("cf"), workspace.join("ext"));
        fs::create_dir_all(&cf).unwrap();
        fs::create_dir_all(&ext).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        fs::write(ext.join("Configuration.xml"), "<Configuration/>").unwrap();
        fs::write(cf.join("Module.bsl"), OLD).unwrap();
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_roots(crate::WorkspaceRoots::build(workspace, &cf, &[]).0);
        engine.index_directory_fts(&cf).unwrap();
        engine.enable_workspace_watcher_mode();
        engine.initialize_workspace_overlay_clean().unwrap();
        edit_and_mark(&engine, &cf, "Module.bsl", "Процедура Корни() Экспорт\nКонецПроцедуры\n");
        let mut batch = prepare(&engine);
        fs::write(cf.join("Module.bsl"), "Процедура Текущие() Экспорт\nКонецПроцедуры\n").unwrap();

        let epoch = engine.workspace_roots_epoch();
        engine.set_workspace_roots(crate::WorkspaceRoots::build(workspace, &cf, &[ext]).0);
        assert_ne!(engine.workspace_roots_epoch(), epoch, "the stand needs roots that moved");

        assert_eq!(engine.publish_point_refresh(&mut batch).unwrap(), PointPublish::Discarded);
        assert!(!found(&engine, "Корни"), "a batch from the old roots published");
        assert!(found(&engine, "Текущие"), "the roots transition indexed the current body");
    }

    /// A store fault while phase B reads the baseline leaves every mark, and every failure
    /// budget, as it was: phase A took nothing, and nothing was settled.
    #[test]
    fn a_store_fault_in_phase_b_keeps_the_marks_and_their_budgets() {
        let dir = tempfile::tempdir().unwrap();
        let engine = manifest_engine(dir.path(), &[("Module.bsl", OLD)]);
        edit_and_mark(&engine, dir.path(), "Module.bsl", "Процедура Б() Экспорт\nКонецПроцедуры\n");
        let capture = engine.capture_point_refresh(POINT_BATCH_KEYS).unwrap().unwrap();
        rusqlite::Connection::open(engine.db_path())
            .unwrap()
            .execute_batch(
                "PRAGMA foreign_keys = OFF; DROP TABLE baseline_manifest_files; \
                 DROP TABLE baseline_manifest;",
            )
            .unwrap();
        let reader = Store::open_reader(capture.db_path()).unwrap();
        assert!(
            capture.prepare(&reader, &|| false).is_err(),
            "the dropped baseline surfaced no error"
        );
        assert_eq!(dirty(&engine), 1, "a store-wide fault stranded the mark");
    }

    /// A key re-marked while its batch was prepared is not settled from the older read.
    #[test]
    fn a_key_re_marked_during_preparation_keeps_its_mark() {
        let dir = tempfile::tempdir().unwrap();
        let engine = raw_engine(dir.path(), &[("Module.bsl", OLD)]);
        edit_and_mark(
            &engine,
            dir.path(),
            "Module.bsl",
            "Процедура Первая() Экспорт\nКонецПроцедуры\n",
        );
        let mut batch = prepare(&engine);
        edit_and_mark(
            &engine,
            dir.path(),
            "Module.bsl",
            "Процедура Вторая() Экспорт\nКонецПроцедуры\n",
        );

        assert_eq!(
            engine.publish_point_refresh(&mut batch).unwrap(),
            PointPublish::Applied { settled: 0, cleared: 0, stale: 1, remaining: 1 }
        );
        assert!(!found(&engine, "Первая"), "the superseded read was published");
    }

    /// SQLite's writer lock held by someone else: phase C gives the batch back within its
    /// short budget, and the SAME batch publishes once the lock is free — without reading the
    /// file again.
    #[test]
    fn a_busy_store_gives_the_batch_back_and_the_retry_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let engine = raw_engine(dir.path(), &[("Module.bsl", OLD)]);
        edit_and_mark(
            &engine,
            dir.path(),
            "Module.bsl",
            "Процедура Готовая() Экспорт\nКонецПроцедуры\n",
        );
        let mut batch = prepare(&engine);

        let holder = rusqlite::Connection::open(engine.db_path()).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = std::time::Instant::now();
        assert_eq!(engine.publish_point_refresh(&mut batch).unwrap(), PointPublish::Busy);
        let waited = started.elapsed();
        // The refusal sat out a busy wait at all: a slow machine only lengthens it.
        assert!(waited >= C_BUSY_TIMEOUT / 2, "{waited:?}");
        // Which busy budget the refusal spent is a matter of the clock, read by the timing
        // profile: a publication that fell back to the store's ordinary busy wait would sit out
        // half a minute, and half of that is beyond any slow machine.
        if cfg!(timing_contract) {
            assert!(waited < crate::store::DEFAULT_BUSY_TIMEOUT / 2, "{waited:?}");
            assert!(waited < Duration::from_secs(1), "{waited:?}");
        }
        assert_eq!(batch.len(), 1, "the refused batch was spent");

        fs::write(dir.path().join("Module.bsl"), "Процедура Поздняя() Экспорт\nКонецПроцедуры\n")
            .unwrap();
        holder.execute_batch("ROLLBACK").unwrap();
        assert!(matches!(
            engine.publish_point_refresh(&mut batch).unwrap(),
            PointPublish::Applied { settled: 1, .. }
        ));
        assert!(found(&engine, "Готовая"), "the retry did not publish the prepared read");
        assert!(!found(&engine, "Поздняя"), "the retry read the file again");
    }

    /// A batch holds at most the budget or its single largest key: the first key always goes
    /// in, and a key over the budget travels alone.
    #[test]
    fn a_batch_holds_the_budget_or_one_large_key() {
        let dir = tempfile::tempdir().unwrap();
        let body = |size: usize, name: &str| {
            let mut text = format!("Процедура {name}()\n");
            text.push_str(&"// ".repeat(size / 3));
            text.push_str("\nКонецПроцедуры\n");
            text
        };
        let five = 5 * 1024 * 1024;
        let files = [
            ("A.bsl", body(five, "А")),
            ("B.bsl", body(five, "Б")),
            ("C.bsl", body(4 * five, "В")),
        ];
        let engine = raw_engine(dir.path(), &[("A.bsl", OLD), ("B.bsl", OLD), ("C.bsl", OLD)]);
        for (name, text) in &files {
            edit_and_mark(&engine, dir.path(), name, text);
        }
        let mut sizes = Vec::new();
        while dirty(&engine) > 0 {
            let mut batch = prepare(&engine);
            sizes.push((batch.len(), batch.bytes()));
            assert!(batch.bytes() <= PREPARED_BYTES.max(4 * five as u64 + 1024));
            engine.publish_point_refresh(&mut batch).unwrap();
        }
        assert_eq!(sizes.iter().map(|(len, _)| *len).collect::<Vec<_>>(), vec![1, 1, 1]);
    }

    /// A preparation asks whether it is cancelled before every key and reads nothing more
    /// once it is: the keys left out keep their marks for the next batch.
    #[test]
    fn a_cancelled_preparation_reads_no_further_key() {
        let dir = tempfile::tempdir().unwrap();
        let engine = raw_engine(dir.path(), &[("A.bsl", OLD), ("B.bsl", OLD), ("C.bsl", OLD)]);
        for name in ["A.bsl", "B.bsl", "C.bsl"] {
            edit_and_mark(&engine, dir.path(), name, "Процедура Новая() Экспорт\nКонецПроцедуры\n");
        }
        let capture = engine.capture_point_refresh(POINT_BATCH_KEYS).unwrap().unwrap();
        let reader = Store::open_reader(capture.db_path()).unwrap();
        let asked = std::cell::Cell::new(0);
        let cancelled = || {
            asked.set(asked.get() + 1);
            asked.get() > 1
        };
        let mut batch = capture.prepare(&reader, &cancelled).unwrap();
        assert_eq!((batch.len(), asked.get()), (1, 2), "the preparation went on after the cancel");
        engine.publish_point_refresh(&mut batch).unwrap();
        assert_eq!(dirty(&engine), 2, "the keys left out lost their marks");
    }
}

/// The point path lives in this module and in the handful of methods it calls, so it can be
/// held to its shape by reading it: the phases that run under the engine lock (A, C) read no
/// file and embed nothing, no phase loads a whole baseline, and phase B has nothing to lock —
/// what it is handed carries no engine, cache or lease. The probe under
/// [`BoundedPublication`] catches a forbidden call that runs; this catches one that is written.
#[cfg(test)]
mod structure {
    /// The sources as a checkout spells them. A Windows checkout may turn every newline into
    /// CRLF, and a gate that cut on `\n` alone would scan nothing there.
    struct Sources {
        module: String,
        engine: String,
        overlay: String,
        store: String,
    }

    impl Sources {
        fn checked_in() -> Self {
            Self {
                module: include_str!("point_refresh.rs").to_owned(),
                engine: include_str!("engine.rs").to_owned(),
                overlay: include_str!("workspace_overlay.rs").to_owned(),
                store: include_str!("store.rs").to_owned(),
            }
        }

        fn normalized(self) -> Self {
            let lf = |source: String| source.replace("\r\n", "\n");
            Self {
                module: lf(self.module),
                engine: lf(self.engine),
                overlay: lf(self.overlay),
                store: lf(self.store),
            }
        }
    }

    /// Production text of this module: everything before its test module, which must be where
    /// this gate expects it — a cut that moved would scan nothing and pass.
    fn production(source: &str) -> &str {
        let cut = ["\n#[cfg(test)]\n", "mod tests {"].concat();
        assert_eq!(source.matches(&cut).count(), 1, "the production/test cut moved");
        source.split(&cut).next().expect("split yields a piece")
    }

    /// The body of the one method named `name` in `source`, up to its closing brace at impl
    /// indentation.
    fn method<'a>(source: &'a str, name: &str) -> &'a str {
        let signature = format!("fn {name}(");
        assert_eq!(source.matches(&signature).count(), 1, "{name}: not exactly one definition");
        let start = source.find(&signature).expect("counted above");
        let end = source[start..].find("\n    }\n").expect("the method closes") + start;
        &source[start..end]
    }

    fn forbid(region: &str, what: &str, needles: &[String]) {
        for needle in needles {
            assert!(!region.contains(needle.as_str()), "{what} contains {needle}");
        }
    }

    fn needles(parts: &[[&str; 2]]) -> Vec<String> {
        parts.iter().map(|parts| parts.concat()).collect()
    }

    fn check(sources: &Sources) {
        // Assembled at run time, so this module's own text is no match.
        let whole_baseline = needles(&[
            ["all_files_in_", "collection("],
            ["load_baseline_manifest_", "fingerprints("],
            ["dispatched_manifest_", "fingerprints("],
            ["refresh_with_", "manifest("],
            ["full_", "refresh"],
            ["refresh_workspace_", "overlay"],
            ["embed", "der"],
        ]);
        let reading = needles(&[
            ["std::", "fs"],
            ["read_to_", "string("],
            ["classify_", "point("],
            ["text_and_", "parse("],
            ["catch_", "up("],
        ]);
        let (engine, overlay, store) = (&sources.engine, &sources.overlay, &sources.store);
        let under_the_lock = [
            ("A", method(engine, "capture_point_refresh")),
            ("A", method(overlay, "capture_point_keys")),
            ("A", method(overlay, "point_fences")),
            ("A", method(overlay, "graph_context_provider_handle")),
            ("C", method(engine, "publish_point_refresh")),
            ("C", method(overlay, "point_batch_stands")),
            ("C", method(overlay, "applicable_point_keys")),
            ("C", method(overlay, "settle_point_batch")),
            ("C", method(store, "commit_point_refresh")),
            ("C", method(store, "commit_point_refresh_once")),
        ];
        for (phase, body) in under_the_lock {
            let what = format!("phase {phase}: {}", body.lines().next().unwrap_or(""));
            forbid(body, &what, &whole_baseline);
            forbid(body, &what, &reading);
        }

        let module = production(&sources.module);
        forbid(module, "point_refresh.rs", &whole_baseline);
        assert!(
            module.contains("reader: &Store,\n        cancelled: &dyn Fn() -> bool,"),
            "phase B's input moved"
        );
        let fields = &module[module.find("pub struct PointCapture {").expect("PointCapture")..];
        let fields = &fields[..fields.find("\n}\n").expect("PointCapture closes")];
        let lockable = needles(&[
            ["Mut", "ex"],
            ["Search", "Engine"],
            ["Workspace", "Overlay"],
            ["lea", "se"],
        ]);
        forbid(fields, "PointCapture, phase B's input", &lockable);
    }

    #[test]
    fn the_point_path_keeps_its_shape() {
        check(&Sources::checked_in().normalized());
    }

    /// The same gate over a CRLF checkout of the same sources.
    #[test]
    fn the_point_path_gate_reads_a_crlf_checkout() {
        let crlf = |source: String| source.replace("\r\n", "\n").replace('\n', "\r\n");
        let checked_in = Sources::checked_in();
        let sources = Sources {
            module: crlf(checked_in.module),
            engine: crlf(checked_in.engine),
            overlay: crlf(checked_in.overlay),
            store: crlf(checked_in.store),
        };
        assert!(sources.module.contains("\r\n"));
        check(&sources.normalized());
    }
}
