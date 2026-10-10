use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ide::{Analysis, RootDatabaseImpl};
use vfs::{FileId, Vfs, VfsPath};

use super::workspace_sweep::{CodeAggregate, SweepOptions, WorkspaceSweep};
use crate::cancel::RequestCancel;

fn partitioned_baseline_error(
    path: &str,
    set: &ide::partitioned_diagnostics_baseline::DiagnosticsBaselineSetSnapshot,
    detail: String,
) -> ide::diagnostics_baseline::DiagnosticsBaselineSummary {
    ide::diagnostics_baseline::DiagnosticsBaselineSummary {
        state: ide::diagnostics_baseline::DiagnosticsBaselineState::Error,
        selection: None,
        partitions_enabled: None,
        partitions_unsuppressed: None,
        unsuppressed: None,
        new: None,
        known: None,
        resolved: None,
        path: Some(path.to_owned()),
        schema_version: Some(
            ide::partitioned_diagnostics_baseline::DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION,
        ),
        manifest_schema_version: Some(set.manifest.schema_version),
        complete: false,
        error_code: Some("classification_error".to_owned()),
        detail: Some(detail),
        partitions: vec![],
        errors: vec![],
    }
}

/// Adapts the resident's owned [`Vfs`] to the lock-neutral [`ide_host_core::VfsWrite`]
/// the shared metadata policy expects. The resident is only ever touched while the
/// caller holds the state mutex (the db is `!Sync`), so a single-threaded `RefCell`
/// gives the interning critical section its interior mutability without a second lock —
/// the same discipline the LSP's `parking_lot`-locked adapter has, minus the lock.
pub(super) struct ResidentVfs(pub(super) RefCell<Vfs>);

impl ide_host_core::VfsWrite for ResidentVfs {
    fn with_write<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

/// Retract everything a path's registration owns: its text input, its file-set entry
/// and its `by_path` back-link. Returns whether the file set moved.
///
/// Where the id lives depends on whether the path was still serving, and the two
/// removal routes disagree about that: the retry list owns paths that left `by_path`
/// the moment they stopped serving, while the drift classifier hands over paths that
/// may still be in it. Keying on `by_path` alone therefore reads an `Admitted` hole as
/// "never indexed" and retracts nothing — the file set keeps mapping the deleted file's
/// id, so `module_index_query` holds it as an empty module while the hole count drops
/// to zero and the workspace calls itself fresh.
///
/// `registered` is what makes the interner safe to ask: `Vfs` keeps a `FileId` forever,
/// so a path it merely REMEMBERS from an earlier life looks exactly like one this
/// resident registered. Only the caller knows which it is.
fn retire_registration(
    resident: &mut DiagnosticsResident,
    file_set: &mut vfs::FileSet,
    key: &str,
    registered: bool,
) -> bool {
    use ide_host_core::{set_file_text_source, FileTextSource, VfsWrite};

    let file_id = match resident.by_path.get(key) {
        Some(&file_id) => Some(file_id),
        None if registered => {
            resident.vfs.with_write(|vfs| vfs.file_id(&VfsPath::new(PathBuf::from(key))))
        }
        None => None,
    };
    let Some(file_id) = file_id else { return false };
    set_file_text_source(&mut resident.db, file_id, FileTextSource::Deleted);
    resident.by_path.remove(key);
    if file_set.path_for_file(&file_id).is_some() {
        file_set.remove(file_id);
        return true;
    }
    false
}

/// Re-read every held hole. Returns `(healed, vanished)` by canonical key, so the
/// caller can move each one's baseline entry and bump what a baseline move obliges.
///
/// The full add sequence runs for EVERY heal, never a shortened "it was registered
/// before, just re-register the text" path. `Vfs` keeps a `FileId` forever while a
/// removal drops the file-set entry, so a deleted-then-recreated path is
/// indistinguishable from one that never left — and the short path would leave it
/// with an id no `path_for_file` can resolve. Every step is idempotent, so paying the
/// whole sequence costs nothing but is safe on the case that cannot be detected.
pub(super) fn retry_resident_holes(
    resident: &mut DiagnosticsResident,
    config_is_current: bool,
    only: Option<&str>,
) -> (Vec<(String, Option<u64>)>, Vec<String>) {
    use base_db::{SourceDatabase, SourceRoot};
    use ide_host_core::{set_file_text_source, FileTextSource, VfsWrite};

    let mut healed = Vec::new();
    let mut vanished = Vec::new();
    let candidates: Vec<(String, HoleOrigin)> = resident
        .holes
        .iter()
        .filter(|(key, _)| only.is_none_or(|wanted| wanted == key.as_str()))
        .map(|(k, o)| (k.clone(), *o))
        .collect();

    let mut file_set = {
        let db = &resident.db;
        db.source_root_input(crate::graph::input::GRAPH_SOURCE_ROOT).root(db).file_set().clone()
    };
    let mut file_set_modified = false;

    for (key, origin) in candidates {
        let path = Path::new(&key);
        // Stat BEFORE the read, the same order every other applier uses. The baseline
        // must describe the bytes actually applied: stat-after-read would record a
        // write that landed between the two, so the baseline would match a disk state
        // whose text was never served, no scan would ever see drift again, and the
        // file would serve the older text at `stale: false` forever.
        let fp_before = crate::graph::scan::file_fingerprint(path);
        // Absence is established by trying to open the file, not by its absence from
        // someone else's listing — an incomplete walk must not retire a hole.
        match base_db::read_disk_text(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Gone. For a path that was serving, this is a removal and owes
                // everything the ordinary removal branch does — otherwise the
                // metadata back-link keeps pointing at a tombstoned FileId.
                file_set_modified |= retire_registration(
                    resident,
                    &mut file_set,
                    &key,
                    origin == HoleOrigin::Admitted,
                );
                resident.holes.remove(&key);
                vanished.push(key);
            }
            Err(_) => {} // still unreadable → stays a hole
            Ok(text) => {
                // Returning a NEW path to service asserts it belongs to the
                // configuration being served, which is the one thing the retry list
                // must not assume: it is memory, and the gate is deliberately asked
                // of the disk. A path that was already serving is not re-admitted.
                if origin == HoleOrigin::Pending && !config_is_current {
                    continue;
                }
                let vfs_path = VfsPath::new(path.to_path_buf());
                let file_id = resident.vfs.with_write(|vfs| vfs.alloc_file_id(vfs_path.clone()));
                resident.db.set_file_source_root(file_id, crate::graph::input::GRAPH_SOURCE_ROOT);
                set_file_text_source(&mut resident.db, file_id, FileTextSource::Disk(&text));
                if file_set.path_for_file(&file_id).is_none() {
                    file_set.insert(file_id, vfs_path);
                    file_set_modified = true;
                }
                resident.by_path.insert(key.clone(), file_id);
                resident.holes.remove(&key);
                healed.push((key, fp_before));
            }
        }
    }

    // The clone is published ONCE, after the loop — the same shape the add/remove
    // branches use. An insert that never reaches the db leaves `path_for_file` empty
    // and the first query panics.
    if file_set_modified {
        resident.db.set_source_root(
            crate::graph::input::GRAPH_SOURCE_ROOT,
            SourceRoot::new_local(file_set),
        );
    }

    // The substrate is re-issued for every transition, not just for paths that were
    // never registered. Whether a module's back-link is currently `None` depends on
    // whether a NEIGHBOUR in the same config root drifted while the hole was held —
    // which no per-hole flag can know. Skipping it would leave a healed common module
    // serving ordinary findings forever while its module-level diagnostics stay mute.
    let touched: Vec<PathBuf> = healed
        .iter()
        .map(|(key, _)| key)
        .chain(&vanished)
        .map(PathBuf::from)
        .filter(|p| project_model::is_substrate_listed_body_path(p))
        .collect();
    if !touched.is_empty() {
        ide_host_core::refresh_metadata_substrate(&mut resident.db, &resident.vfs, &touched);
    }

    (healed, vanished)
}

/// Whether a hole's path was already admitted into the resident that owns it.
///
/// Healing an `Admitted` hole returns a file whose membership in this configuration
/// was already asserted; healing a `Pending` one asserts it for the first time and is
/// therefore an admission, gated on the configuration still being current. The
/// distinction is RECORDED when the hole is created because it cannot be derived
/// later: `Vfs` keeps a `FileId` forever, so a deleted-then-recreated path looks
/// exactly like one that was never removed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum HoleOrigin {
    Admitted,
    Pending,
}

/// The built resident database plus the path→FileId index needed to resolve a request
/// path to the Salsa input it set. Held behind the [`std::sync::Mutex`]; reads borrow it,
/// a reload mutates `db` in place.
pub(crate) struct DiagnosticsResident {
    pub(super) db: RootDatabaseImpl,
    /// Trims [`Self::trim_after_read`] has run, for a test that every read trims.
    #[cfg(test)]
    pub(super) read_trims: u32,
    /// This resident's OWN pool for the diagnostics fan-out. Dedicated because salsa
    /// attaches at most one database per thread while every job carries its own db
    /// clone: a worker shared with another resident's sweep would be asked to attach a
    /// second database mid-query and panic. Keeping the pool per resident also confines
    /// any salsa query that parallelises internally to this resident's threads.
    ///
    /// Built on the first sweep, not with the resident: a resident that only ever serves
    /// single-file requests — and one rebuilt while the old one still serves — would
    /// otherwise each hold a full pool of idle threads for nothing.
    pub(super) sweep_pool: std::sync::OnceLock<rayon::ThreadPool>,
    /// The VFS pre-seeded with the resident's `.bsl` FileIds and grown by the metadata
    /// bootstrap with the metadata-XML ids. Kept alongside the db so a drift-driven
    /// substrate refresh can intern new composing files onto the same id space without
    /// rebuilding it.
    pub(super) vfs: ResidentVfs,
    /// Canonical-path string → FileId for every SERVED resident `.bsl`. A file whose
    /// bytes could not be read is absent here even though it exists on disk — see
    /// `holes`.
    pub(super) by_path: HashMap<String, FileId>,
    /// Workspace `.bsl` files that exist but could not be read, by the same canonical
    /// key as `by_path`. Doubles as the RETRY LIST: every reconciliation window tries
    /// to re-read them, which is what makes healing independent of both the drift
    /// fingerprint (only `(mtime, len)`) and hub health (a healthy hub runs no scan).
    /// The value records whether the path was ever admitted into THIS resident —
    /// re-admission has to ask the configuration gate, a return to service does not,
    /// and the VFS interner cannot tell the two apart because it never forgets an id.
    pub(super) holes: HashMap<String, HoleOrigin>,
    /// The project's effective diagnostics settings, loaded from `bsl-analyzer.toml` /
    /// `.bsl-analyzer.json` the same way LSP and CLI do — so `file`/`workspace` honour
    /// the project's disabled rules and thresholds, not analyzer defaults.
    pub(super) config: ide::DiagnosticsConfig,
    /// The workspace root the resident was built against — the SAME root the graph build
    /// uses (`source_dir`), so an absolute finding path strips to the graph encoder's rel
    /// and the `method/file/<rel>::<name>` graph bridge resolves.
    pub(super) workspace_root: PathBuf,
    /// The registered source roots, built from the SAME project read that gave this
    /// resident its file universe. A request path is relative to the root that owns it,
    /// and only this table can say which directory that is.
    ///
    /// It is held here, rather than read from the search engine, because the two must not
    /// be able to disagree: the engine's copy is published only after a full cold index
    /// (and never at all if that fails), while `by_path` exists from the moment the
    /// resident does. A table from the other subsystem could name a root whose files this
    /// resident never enumerated — resolution would then point at a file it cannot serve.
    pub(super) workspace_roots: bsl_search::WorkspaceRoots,
    /// `[analysis].diff_base` from the project config; drives the drift-time
    /// rescope so the vendor-diff filter tracks the moving working copy.
    pub(super) diff_base: Option<String>,
    /// Resolved (base, HEAD) OIDs the current scope was built against; the
    /// drift poll rebuilds when the live refs no longer match (ref-only moves).
    pub(super) scope_identity: Option<(String, String)>,
    /// `[analysis].ignored_authors` from the project config, kept for the
    /// drift-time filter rebuild when HEAD moves.
    pub(super) ignored_authors: Vec<String>,
    /// Blame-backed line filter pinned to one HEAD state; `None` when not
    /// configured or when the repository cannot support it (fail-open).
    pub(super) author_filter: Option<std::sync::Arc<vcs::AuthorFilter>>,
    /// Independently reloadable diagnostics-baseline snapshot. It is not a Salsa input.
    pub(super) diagnostics_baseline:
        ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot,
    pub(super) project: project_model::Project,
}

impl DiagnosticsResident {
    pub(crate) fn diagnostics_baseline(
        &self,
    ) -> &ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot {
        &self.diagnostics_baseline
    }

    /// The file a request names, given the root its path is spelled against.
    ///
    /// The rule itself is shared with every other file-addressed tool
    /// ([`crate::tools::file_request::resolve_rooted_path`]); the resident holds nothing but
    /// the table it is applied to. Two readings of one pair would address two files while
    /// reporting the same address.
    pub(crate) fn resolve_rooted_path(
        &self,
        root_id: Option<&str>,
        path: &Path,
    ) -> Result<PathBuf, crate::tools::file_request::RootedPathError> {
        crate::tools::file_request::resolve_rooted_path(&self.workspace_roots, root_id, path)
    }

    /// Resolve a request path to the resident FileId, canonicalising it the same way
    /// the loader did. A relative path is resolved against the workspace root (not the
    /// process CWD), so `diagnostics file` works regardless of where the server was
    /// started. `None` when the path is not a resident workspace `.bsl`.
    pub(crate) fn file_id_for(&self, path: &Path) -> Option<FileId> {
        let resolved;
        let abs: &Path = if path.is_absolute() {
            path
        } else {
            resolved = self.workspace_root.join(path);
            &resolved
        };
        self.by_path.get(&canonical_key(abs)).copied()
    }

    /// Whether `path` is a workspace `.bsl` that exists but could not be read.
    ///
    /// Callers that get `None` from [`Self::file_id_for`] must ask this before
    /// answering "not a workspace file": for a hole that answer is a lie about an
    /// existing file, and replacing the old lie ("the file is clean") with a new one
    /// is not the point of holding it out of service.
    pub(crate) fn is_unread(&self, path: &Path) -> bool {
        self.hole_key_of(path).is_some()
    }

    /// The hole this path names, in the spelling the hole list is keyed by; `None` when the
    /// path is not being held out of service. One resolution for both questions: a caller
    /// that heals what an answer calls unreadable has to name the same file.
    pub(super) fn hole_key_of(&self, path: &Path) -> Option<String> {
        let resolved;
        let abs: &Path = if path.is_absolute() {
            path
        } else {
            resolved = self.workspace_root.join(path);
            &resolved
        };
        let key = canonical_key(abs);
        self.holes.contains_key(&key).then_some(key)
    }

    /// How many workspace `.bsl` files exist but could not be read.
    pub(crate) fn unread_count(&self) -> usize {
        self.holes.len()
    }

    /// Whether the VFS interner already holds an id for `path`.
    ///
    /// Test-only, and deliberately so: it is the ONLY way to tell "not registered"
    /// from "registered but filtered out downstream", and those two states differ by
    /// whether a later query panics.
    #[cfg(test)]
    pub(super) fn vfs_file_id_for_test(&self, path: &Path) -> Option<FileId> {
        use ide_host_core::VfsWrite;
        self.vfs.with_write(|vfs| vfs.file_id(&VfsPath::new(path.to_path_buf())))
    }

    /// Whether the source root still maps `file_id` to a path.
    ///
    /// Test-only. The file-set entry is what a removal must actually retract: the
    /// interner keeps the id regardless, and a deleted file disappears from metadata
    /// discovery on its own — so neither of those can tell a complete removal from a
    /// partial one.
    #[cfg(test)]
    pub(super) fn file_set_has_for_test(&self, file_id: FileId) -> bool {
        use base_db::SourceDatabase;
        let db = &self.db;
        db.source_root_input(crate::graph::input::GRAPH_SOURCE_ROOT)
            .root(db)
            .file_set()
            .path_for_file(&file_id)
            .is_some()
    }

    /// The workspace root the resident was built against (the graph's `source_dir`),
    /// used to bridge findings to durable `method/file/<rel>::<name>` graph ids.
    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// This resident's own root table — the one its files were enumerated under, so a
    /// location built from it addresses a file this resident can actually serve.
    pub(crate) fn workspace_roots(&self) -> &bsl_search::WorkspaceRoots {
        &self.workspace_roots
    }

    /// An `Analysis` view over a cloned db handle. The clone shares the Salsa storage
    /// (memo/LRU cache), and is dropped before the read guard is released.
    pub(crate) fn analysis(&self) -> Analysis {
        Analysis::from_database(self.db.clone())
    }

    /// The resident Salsa database, for the `metadata` tool's root-scoped metadata
    /// reads (`resolve_*_across_roots` point-lookups and the Channel-2
    /// `configuration_for_root` header/enumeration). Borrowed under the state lock, so
    /// the borrow cannot outlive the read and a reload can never alias it.
    pub(crate) fn db(&self) -> &RootDatabaseImpl {
        &self.db
    }

    /// Every served file, with the absolute path it was indexed under.
    ///
    /// The spelling matters: it is the CANONICAL one, and the attributor that
    /// mints published pairs asks both spellings — so a caller narrowing by root
    /// gets the files this resident actually holds, not the ones a declared
    /// prefix would have matched.
    pub(crate) fn files(&self) -> impl Iterator<Item = (&Path, FileId)> + '_ {
        self.by_path.iter().map(|(path, file_id)| (Path::new(path.as_str()), *file_id))
    }

    pub(crate) fn file_count(&self) -> usize {
        self.by_path.len()
    }

    /// The project's effective diagnostics config, the single source of truth shared
    /// with LSP and CLI. `file` and `workspace` analyse against this, never defaults.
    pub(crate) fn config(&self) -> &ide::DiagnosticsConfig {
        &self.config
    }

    /// Whether `path` has any line in the vendor-diff analysis scope. Resolves
    /// the path the same way [`Self::file_id_for`] does (relative → workspace
    /// root, canonicalised). `true` when no scope is configured.
    pub(crate) fn path_in_scope(&self, path: &Path) -> bool {
        let Some(scope) = self.config.scope.as_ref() else { return true };
        scope.is_file_in_scope(&self.abs_path_for(path))
    }

    /// The blame-backed `ignored_authors` filter, when active.
    pub(crate) fn author_filter(&self) -> Option<&std::sync::Arc<vcs::AuthorFilter>> {
        self.author_filter.as_ref()
    }

    /// Resolve a request path to the absolute canonical form the resident and
    /// the git workdir agree on.
    pub(crate) fn abs_path_for(&self, path: &Path) -> PathBuf {
        let abs =
            if path.is_absolute() { path.to_path_buf() } else { self.workspace_root.join(path) };
        abs.canonicalize().unwrap_or(abs)
    }
}

/// Build the `ignored_authors` blame filter for `root`. `None` — with a
/// warning — when the repository cannot support attribution (missing, bare,
/// shallow, unborn HEAD): MCP fails open and reports everything, matching the
/// scope policy; only the CLI treats these as hard errors.
pub(crate) fn build_author_filter(
    root: &Path,
    authors: &[String],
) -> Option<std::sync::Arc<vcs::AuthorFilter>> {
    if authors.is_empty() {
        return None;
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match vcs::AuthorFilter::new(&root, authors.to_vec()) {
        Ok(filter) => {
            tracing::info!(
                authors = authors.len(),
                head = %filter.head_identity(),
                "ignored-authors filter active"
            );
            Some(std::sync::Arc::new(filter))
        }
        Err(error) => {
            tracing::warn!(%error, "ignored-authors filter unavailable; reporting all findings");
            None
        }
    }
}

/// Whether a diagnostic on `range` survives the author filter: any covered
/// line kept → survives. Uses the scope-gate line mapping (half-open range →
/// last line from `end - 1`, empty range anchors at `start`).
pub(crate) fn diagnostic_survives_authors(
    keep: &vcs::LineKeep,
    index: &line_index::LineIndex,
    range: syntax::TextRange,
) -> bool {
    let start = index.line_col(range.start()).line;
    let end_offset =
        if range.is_empty() { range.start() } else { range.end() - line_index::TextSize::from(1) };
    let end = index.line_col(end_offset).line;
    keep.range_survives(start + 1, end + 1)
}

/// Drop diagnostics attributed to ignored authors, counting what was dropped.
/// Any blame failure keeps the file's findings intact (fail-open) — MCP must
/// degrade to noise, never to silence.
fn filter_by_author(
    analysis: &Analysis,
    file_id: FileId,
    path: Option<&Path>,
    filter: &vcs::AuthorFilter,
    diagnostics: Vec<ide::Diagnostic>,
    ignored: &std::sync::atomic::AtomicUsize,
) -> Vec<ide::Diagnostic> {
    let Some(path) = path else { return diagnostics };
    let text = analysis.file_text(file_id);
    match filter.lines_kept_cached(path, text.as_bytes()) {
        Ok(keep) => {
            if keep.ignored_line_count() == 0 {
                return diagnostics;
            }
            let index = line_index::LineIndex::new(&text);
            let before = diagnostics.len();
            let kept: Vec<_> = diagnostics
                .into_iter()
                .filter(|d| diagnostic_survives_authors(&keep, &index, d.range))
                .collect();
            ignored.fetch_add(before - kept.len(), std::sync::atomic::Ordering::Relaxed);
            kept
        }
        Err(error) => {
            tracing::warn!(%error, "blame failed; keeping every finding for the file");
            diagnostics
        }
    }
}

/// Compute the vendor-diff scope for `root` against `base` (workdir mode, so
/// uncommitted and untracked edits count as changed), plus the resolved
/// (base, HEAD) identity the drift poll compares against. Scope `None` — and
/// a warning — when the repo or ref cannot be resolved: MCP fails open,
/// matching LSP.
pub(crate) type ScopeBuild =
    (Option<std::sync::Arc<base_db::AnalysisScope>>, Option<(String, String)>);

pub(crate) fn build_scope(root: &Path, base: &str) -> ScopeBuild {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let identity = vcs::scope_ref_identity(&root, base).ok();
    match vcs::generate_workdir_diff_report(&root, base, true) {
        Ok(diff) => {
            let scope = std::sync::Arc::new(base_db::AnalysisScope::from_report(
                diff.report.base_ref,
                &diff.workdir,
                diff.report.files.into_iter().map(|(path, change)| (path, change.hunks)),
            ));
            tracing::info!(
                base,
                files_in_scope = scope.in_scope_file_count(),
                "vendor-diff analysis scope active"
            );
            (Some(scope), identity)
        }
        Err(error) => {
            tracing::warn!(base, %error, "vendor-diff scope unavailable; analyzing everything");
            (None, identity)
        }
    }
}

/// Files and source bytes one sweep chunk hands to the pool before the memo caches
/// are trimmed. The in-chunk working set is the sweep's peak memory and scales with
/// the chunk's bytes, so both are capped (the CLI `analyze` discipline); the sweep
/// answers the same union whatever the chunking.
const SWEEP_CHUNK: stdx::batch::BatchBudget =
    stdx::batch::BatchBudget::files(500).with_bytes(32 << 20);

impl DiagnosticsResident {
    /// Evict memos beyond their interactive caps and drop the parser's thread-local
    /// green-node caches on the sweep pool. The freed pages are left to the allocator:
    /// the next chunk reuses them, and an explicit purge here would only make it fault
    /// them back in. Needs `&mut`: salsa's trim takes the exclusive handle and waits
    /// for the chunk's worker clones, which the pool has already dropped by the time
    /// this runs.
    fn trim_between_chunks(&mut self) {
        self.db.enforce_lru();
        self.clear_pool_node_caches();
    }

    /// The sweep's final trim: the swept files are batch working set nothing will read
    /// again, so the heavy per-file chains go down to the small sweep caps instead of
    /// pinning a full interactive window of them until the next request.
    fn trim_after_sweep(&mut self) {
        ide::sweep_lru_deep(&mut self.db);
        self.clear_pool_node_caches();
        profile::purge_allocator();
    }

    /// Warm the per-file halves of the name indexes the `references` tool reads, in
    /// chunks with a trim between them. The indexes are one query each over every
    /// workspace file, so a cold one would otherwise parse the whole workspace inside
    /// the request and keep every syntax tree until it answers; warmed this way, the
    /// trees of a chunk go at its end and only the small per-file memos stay. The pool
    /// discipline is the sweep's: each job on its own db clone, registered with the
    /// request's cancellation, nested parallelism refused.
    pub(crate) fn warm_name_indexes(&mut self, cancel: &RequestCancel) {
        use rayon::prelude::*;
        use std::panic::AssertUnwindSafe;

        // Owned paths, not borrows of the table: the trim between chunks takes the
        // resident mutably.
        let mut files: Vec<(FileId, String)> =
            self.by_path.iter().map(|(path, id)| (*id, path.clone())).collect();
        files.sort_by_key(|(id, _)| id.0);
        let chunks = stdx::batch::chunks_by_budget(
            &files,
            |(_, path)| std::fs::metadata(path).map_or(0, |meta| meta.len()),
            SWEEP_CHUNK,
        );
        for &chunk in &chunks {
            if cancel.is_cancelled() {
                return;
            }
            let seed = SweepWorker::new(self.db.clone());
            let outcome = self.sweep_pool.get_or_init(build_sweep_pool).install(move || {
                chunk.par_iter().try_for_each_with(seed, |worker, (file_id, _)| {
                    let file_id = *file_id;
                    let _no_nesting = stdx::par_guard::enter_no_nested_parallelism();
                    if !worker.registered {
                        cancel.register(salsa::Database::cancellation_token(
                            worker.analysis.database(),
                        ));
                        worker.registered = true;
                    }
                    if cancel.is_cancelled() {
                        return Err(salsa::Cancelled::Local);
                    }
                    salsa::Cancelled::catch(AssertUnwindSafe(|| {
                        worker.analysis.warm_name_indexes(&[file_id]);
                    }))
                })
            });
            match outcome {
                Ok(()) => {}
                Err(salsa::Cancelled::Local) => return,
                Err(other) => std::panic::resume_unwind(Box::new(other)),
            }
            self.trim_between_chunks();
        }
    }

    fn clear_pool_node_caches(&self) {
        syntax::clear_shared_node_cache();
        if let Some(pool) = self.sweep_pool.get() {
            pool.broadcast(|_| syntax::clear_shared_node_cache());
        }
    }

    /// Trim the memo caches to their interactive caps after one served read. A
    /// request leaves its file's syntax tree, lowered bodies and inference memoised,
    /// and nothing else moves the revision, so without this every file an agent ever
    /// asked about would stay resident; with it the resident holds at most the caps'
    /// worth of recent files, and a repeat request on one of them is still a cache
    /// hit. Evicting nothing costs nothing, and the freed pages go back to the OS on
    /// the allocator's own decay rather than by a purge here, whose walk of every
    /// arena grows with the heap and would be paid on every request. Called by the
    /// lifecycle after the read's closure returned and its database clone was dropped,
    /// under the resident lock. The parser cache cleared here is the calling thread's:
    /// a request's parse runs on the thread that serves it, and a blocking-pool thread
    /// that falls idle takes its cache with it.
    pub(super) fn trim_after_read(&mut self) {
        self.db.enforce_lru();
        syntax::clear_shared_node_cache();
        #[cfg(test)]
        {
            self.read_trims += 1;
        }
    }
}

impl DiagnosticsResident {
    /// Workspace-wide diagnostics aggregated per code (the `workspace` action). Runs
    /// rayon over per-worker db clones (shared Salsa storage, the CLI `analyze`
    /// discipline). The caller MUST hold the state lock for the whole sweep so no
    /// reload mutates the master db mid-flight — that would cancel the cloned queries.
    /// Bounded by `opts.max_files` over a stable FileId order, so a cap is deterministic.
    ///
    /// `cancel` is the sweep's cancellation bridge: each worker registers its clone's
    /// salsa token before its first query, so `cancel_all` unwinds in-flight queries at
    /// their next salsa boundary and the file-boundary check skips the rest. Only
    /// worker-clone tokens are ever cancelled — the master db handle stays untouched,
    /// so concurrent `diagnostics` calls and later sweeps are unaffected.
    pub(crate) fn workspace_aggregates(
        &mut self,
        config: &ide::DiagnosticsConfig,
        opts: &SweepOptions,
        cancel: &RequestCancel,
    ) -> WorkspaceSweep {
        use rayon::prelude::*;
        use std::collections::HashSet;
        use std::panic::AssertUnwindSafe;

        let lacks_owning_configuration = {
            let snapshot = self.db.workspace_configs_snapshot();
            !snapshot.has_base() && snapshot.has_externals()
        };

        // Vendor-diff file-gate: unchanged-vs-base files are excluded up front so the
        // sweep never walks thousands of files whose report is guaranteed empty;
        // `files_out_of_scope` keeps the coverage bookkeeping honest about the gap.
        // The workers below observe cancellation through their own salsa handles, but
        // this selection runs before the first of them exists — and with `max_files` at
        // zero no worker runs at all. Left unchecked, a cancelled sweep would walk and
        // sort the whole workspace under the resident lock for an answer nobody reads.
        //
        // It stops the way the sweep stops everywhere else — an empty report marked
        // cancelled — and not by unwinding: a sweep that reports what it managed is
        // this call's whole shape, and the coverage numbers beside it stay honest.
        if cancel.is_cancelled() {
            let baseline = self.diagnostics_baseline.error_summary().unwrap_or_else(|| match self
                .diagnostics_baseline
                .project_path()
            {
                Some(path) => ide::diagnostics_baseline::DiagnosticsBaselineSummary::interrupted(
                    Some(path.to_owned()),
                ),
                None => ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled(),
            });
            return WorkspaceSweep::nothing_swept(
                self.by_path.len() + self.holes.len(),
                self.holes.len(),
                baseline,
                self.diagnostics_baseline.epoch().to_owned(),
                lacks_owning_configuration,
            );
        }
        let mut files: Vec<FileId> = Vec::with_capacity(self.by_path.len());
        let mut files_out_of_scope = 0usize;
        for (path, file_id) in &self.by_path {
            if config.scope.as_ref().is_none_or(|s| s.is_file_in_scope(Path::new(path))) {
                files.push(*file_id);
            } else {
                files_out_of_scope += 1;
            }
        }
        files.sort_by_key(|f| f.0);
        // Holes stay in the DENOMINATOR. They are not served, so they cannot be swept,
        // but shrinking the total would make an existing workspace file simply absent
        // from the coverage bookkeeping — the one thing `files_out_of_scope` exists to
        // prevent for the skips beside it.
        let files_total = self.by_path.len() + self.holes.len();
        let in_scope = files.len();
        let truncated = in_scope > opts.max_files;
        let swept = &files[..opts.max_files.min(in_scope)];

        let path_of: HashMap<FileId, String> =
            self.by_path.iter().map(|(path, id)| (*id, path.clone())).collect();
        // `by_path` keys are canonical, so the root must be too: given a workspace root
        // through a symlink, a literal strip fails for EVERY file and the baseline stops
        // matching wholesale.
        let workspace_root =
            self.workspace_root.canonicalize().unwrap_or_else(|_| self.workspace_root.clone());
        if let Some(error) = self.diagnostics_baseline.error_summary() {
            return WorkspaceSweep {
                aggregates: Vec::new(),
                files_swept: 0,
                files_total,
                files_out_of_scope,
                files_unread: self.holes.len(),
                findings_ignored_by_author: 0,
                author_head: self.author_filter.as_ref().map(|filter| filter.short_identity()),
                truncated,
                cancelled: false,
                baseline: error,
                baseline_epoch: self.diagnostics_baseline.epoch().to_owned(),
                lacks_owning_configuration,
            };
        }

        // Compute the unfiltered candidates in parallel. Baseline classification happens
        // before severity/code/author shaping, so those presentation filters cannot hide
        // a new finding or manufacture a resolved entry.
        type Candidate =
            ide::diagnostics_baseline::BaselineDiagnosticCandidate<(FileId, ide::Diagnostic)>;
        // The pool takes an owned db clone: `&self` is not `Sync` (the resident holds a
        // `RefCell` VFS), and each worker clones its own handle from this seed anyway.
        // Borrow what the jobs read, so `move` takes the references and leaves the values
        // to the code after the sweep.
        let path_of = &path_of;
        let workspace_root = &workspace_root;
        // The files go through the pool in chunks, and the resident's memo caches are
        // trimmed between chunks: salsa evicts beyond a query's `lru` cap only at a
        // revision boundary or on an explicit trim, and a sweep never moves the
        // revision, so one pass over thousands of files would otherwise keep every
        // file's syntax tree, lowered bodies and inference resident at once (the CLI
        // `analyze` chunk discipline). Chunking changes nothing in the answer — the
        // aggregates are a union over files — only how many files' working set is
        // live at a time. Nothing to sweep makes no chunk and so raises no pool:
        // building one costs a thread per core, and a request that asked for no files
        // must not pay for it under the resident lock.
        let mut prepared: Vec<Option<Vec<Candidate>>> = Vec::with_capacity(swept.len());
        let chunks = stdx::batch::chunks_by_budget(
            swept,
            |file_id| {
                path_of
                    .get(file_id)
                    .and_then(|path| std::fs::metadata(path).ok())
                    .map_or(0, |meta| meta.len())
            },
            SWEEP_CHUNK,
        );
        for &chunk in &chunks {
            if cancel.is_cancelled() {
                prepared.extend(chunk.iter().map(|_| None));
                continue;
            }
            let mut seed = SweepWorker::new(self.db.clone());
            // Warm the configuration inventory HERE, before the pool opens: a job that
            // first-loads a config root fans out over the loader's own rayon scope, parks,
            // and can steal a sibling job carrying a different db clone onto this thread.
            // Per chunk, not once per sweep: the trim between chunks can leave it cold.
            // Registered like any worker, so a cancellation arriving mid-warm unwinds it too.
            #[cfg(test)]
            sweep_probe::record_warm();
            cancel.register(salsa::Database::cancellation_token(seed.analysis.database()));
            seed.registered = true;
            seed.analysis.warm_configuration_inventory();
            let chunk_results: Vec<Option<Vec<Candidate>>> =
                self.sweep_pool.get_or_init(build_sweep_pool).install(move || {
                    chunk
                        .par_iter()
                        .map_with(seed, |worker, &file_id| {
                            // Belt-and-suspenders behind the warm-up: should a query still reach an
                            // internally parallel path, it runs serially instead of stealing a
                            // sibling job and attaching a second database to this thread.
                            let _no_nesting = stdx::par_guard::enter_no_nested_parallelism();
                            #[cfg(test)]
                            sweep_probe::record_job(worker.origin);
                            if !worker.registered {
                                cancel.register(salsa::Database::cancellation_token(
                                    worker.analysis.database(),
                                ));
                                worker.registered = true;
                            }
                            if cancel.is_cancelled() {
                                return None;
                            }
                            let caught = salsa::Cancelled::catch(AssertUnwindSafe(|| {
                                let diagnostics = worker.analysis.diagnostics(file_id, config);
                                let text = worker.analysis.file_text(file_id);
                                let path =
                                    path_of.get(&file_id).expect("swept file has a resident path");
                                let relative = Path::new(path)
                                    .strip_prefix(workspace_root)
                                    .unwrap_or(Path::new(path))
                                    .to_string_lossy()
                                    .replace(std::path::MAIN_SEPARATOR, "/");
                                let source_lines: Vec<_> = text.lines().collect();
                                diagnostics
                                    .into_iter()
                                    .map(|d| {
                                        let output = d.to_output(&text);
                                        ide::diagnostics_baseline::BaselineDiagnosticCandidate {
                                        diagnostic: (file_id, d),
                                        path: relative.clone(),
                                        code: output.code,
                                        snippet: Some(
                                            ide::diagnostics_baseline::diagnostic_line_snippet(
                                                &source_lines,
                                                output.start_line,
                                            ),
                                        ),
                                        message: output.message,
                                        severity: output.severity,
                                        range:
                                            ide::diagnostics_baseline::DiagnosticsBaselineRange {
                                                start_line: output.start_line as u32,
                                                start_column: output.start_column as u32,
                                                end_line: output.end_line as u32,
                                                end_column: output.end_column as u32,
                                            },
                                    }
                                    })
                                    .collect()
                            }));
                            match caught {
                                Ok(diags) => Some(diags),
                                // Only the request's own cancellation may degrade to a skipped
                                // file. A pending write cannot exist under the resident mutex
                                // and a propagated panic is a real defect in a sibling worker —
                                // re-raise both instead of hiding them behind valid aggregates.
                                Err(salsa::Cancelled::Local) => None,
                                Err(other) => std::panic::resume_unwind(Box::new(other)),
                            }
                        })
                        .collect()
                });
            prepared.extend(chunk_results);
            self.trim_between_chunks();
        }

        let cancelled = cancel.is_cancelled();
        let files_swept = prepared.iter().filter(|result| result.is_some()).count();
        let completed_files: std::collections::BTreeSet<String> = if config.scope.is_some() {
            std::collections::BTreeSet::new()
        } else {
            swept
                .iter()
                .zip(&prepared)
                .filter(|(_, result)| result.is_some())
                .filter_map(|(file_id, _)| path_of.get(file_id))
                .filter_map(|path| Path::new(path).strip_prefix(workspace_root).ok())
                .map(|path| path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
                .collect()
        };
        // Holes belong in this denominator for the same reason they belong in
        // `files_total`: a partition may only be upgraded to full coverage when every
        // file it owns was actually analysed, and an unreadable file was not. Omitting
        // it would report that file's baseline entries as resolved — "already fixed" —
        // on the strength of never having looked at it.
        let all_project_files: std::collections::BTreeSet<String> = path_of
            .values()
            .map(String::as_str)
            .chain(self.holes.keys().map(String::as_str))
            .filter_map(|path| Path::new(path).strip_prefix(workspace_root).ok())
            .map(|path| path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
            .collect();
        let complete = !cancelled
            && !truncated
            && files_out_of_scope == 0
            && self.holes.is_empty()
            && self.author_filter.is_none()
            // An analysis scope gates LINES, not just files: a file may be swept whole
            // and still yield only the diagnostics of its changed hunks. Calling that
            // full coverage would report every unmatched baseline entry as resolved.
            && config.scope.is_none()
            && files_swept == files_total;
        let coverage = if complete {
            ide::diagnostics_baseline::DiagnosticsBaselineCoverage::Full
        } else {
            ide::diagnostics_baseline::DiagnosticsBaselineCoverage::Partial { completed_files }
        };
        let candidates: Vec<_> = prepared.into_iter().flatten().flatten().collect();
        let snapshot = &self.diagnostics_baseline;
        let (active, baseline) = if let Some((set, plan, baseline_path)) = snapshot.ready_set() {
            let wrapped: Result<Vec<_>, _> = candidates
                .into_iter()
                .map(|candidate| {
                    let owner = plan.owner_for_project_path(&candidate.path).ok_or_else(|| {
                        format!("diagnostics file has no partition owner: {}", candidate.path)
                    })?;
                    Ok(ide::partitioned_diagnostics_baseline::PartitionedBaselineDiagnosticCandidate {
                        partition_id: owner.to_owned(),
                        candidate,
                    })
                })
                .collect();
            let classified = wrapped.and_then(|wrapped| {
                let coverage = ide::partitioned_diagnostics_baseline::partitioned_coverage(
                    plan,
                    &coverage,
                    (config.scope.is_none() && self.author_filter.is_none())
                        .then_some(&all_project_files),
                )?;
                ide::partitioned_diagnostics_baseline::classify_partitioned_diagnostics(
                    set,
                    plan,
                    baseline_path.to_owned(),
                    wrapped,
                    &coverage,
                )
                .map_err(|error| error.to_string())
            });
            match classified {
                Ok(classified) => (
                    classified
                        .new
                        .into_iter()
                        .chain(classified.unsuppressed)
                        .map(|item| item.diagnostic)
                        .collect::<Vec<_>>(),
                    classified.summary,
                ),
                Err(error) => (Vec::new(), partitioned_baseline_error(baseline_path, set, error)),
            }
        } else if let Some((baseline, baseline_path)) = snapshot.ready() {
            match ide::diagnostics_baseline::classify_diagnostics(
                baseline,
                baseline_path.to_owned(),
                candidates,
                &coverage,
            ) {
                Ok(classified) => (
                    classified.new.into_iter().map(|item| item.diagnostic).collect::<Vec<_>>(),
                    classified.summary,
                ),
                Err(error) => (
                    Vec::new(),
                    ide::diagnostics_baseline::DiagnosticsBaselineSummary {
                        state: ide::diagnostics_baseline::DiagnosticsBaselineState::Error,
                        selection: None,
                        partitions_enabled: None,
                        partitions_unsuppressed: None,
                        unsuppressed: None,
                        new: None,
                        known: None,
                        resolved: None,
                        path: Some(baseline_path.to_owned()),
                        schema_version: Some(baseline.schema_version),
                        manifest_schema_version: None,
                        complete: false,
                        error_code: Some("missing_snippet".to_owned()),
                        detail: Some(error.to_string()),
                        partitions: vec![],
                        errors: vec![],
                    },
                ),
            }
        } else if let Some(summary) = snapshot.error_summary() {
            (Vec::new(), summary)
        } else {
            (
                candidates.into_iter().map(|candidate| candidate.diagnostic).collect::<Vec<_>>(),
                ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled(),
            )
        };
        let mut active_by_file: HashMap<FileId, Vec<ide::Diagnostic>> = HashMap::new();
        for (file_id, diagnostic) in active {
            active_by_file.entry(file_id).or_default().push(diagnostic);
        }

        // The author pass queries Salsa through its own db clones, so it observes
        // cancellation exactly the way the candidate pass above does: register the
        // clone's token, stop between files, and keep an unwind inside the worker.
        // Without that this pass is unstoppable, and a cancellation unwind would
        // cross rayon into the state lock as a panic.
        // An owned handle, not a borrow of the resident: the deep trim after the pass
        // below needs the resident mutably.
        let author_filter = self.author_filter.clone();
        let author_filter = author_filter.as_ref();
        let author_ignored = std::sync::atomic::AtomicUsize::new(0);
        let author_ignored = &author_ignored;
        let active_by_file = &active_by_file;
        // The seed clone is made only where the pool consumes it: left in this frame by
        // a sweep with nothing to sweep, it would be a live handle the deep trim below
        // waits on forever.
        let per_file: Vec<Vec<(String, ide::SeverityBucket)>> = if swept.is_empty() {
            Vec::new()
        } else {
            let seed = SweepWorker::new(self.db.clone());
            self.sweep_pool.get_or_init(build_sweep_pool).install(move || {
                swept
                    .par_iter()
                    .map_with(seed, |worker, &file_id| {
                        let _no_nesting = stdx::par_guard::enter_no_nested_parallelism();
                        #[cfg(test)]
                        sweep_probe::record_job(worker.origin);
                        if !worker.registered {
                            cancel.register(salsa::Database::cancellation_token(
                                worker.analysis.database(),
                            ));
                            worker.registered = true;
                        }
                        if cancel.is_cancelled() {
                            return Vec::new();
                        }
                        let diagnostics = active_by_file.get(&file_id).cloned().unwrap_or_default();
                        let caught = salsa::Cancelled::catch(AssertUnwindSafe(|| {
                            let diagnostics = match author_filter {
                                Some(filter) if !diagnostics.is_empty() => filter_by_author(
                                    &worker.analysis,
                                    file_id,
                                    path_of.get(&file_id).map(Path::new),
                                    filter,
                                    diagnostics,
                                    author_ignored,
                                ),
                                _ => diagnostics,
                            };
                            diagnostics
                                .iter()
                                .map(|diagnostic| {
                                    (
                                        diagnostic.code.as_str().to_owned(),
                                        ide::SeverityBucket::from(diagnostic.severity),
                                    )
                                })
                                .collect()
                        }));
                        match caught {
                            Ok(codes) => codes,
                            // Same discipline as the candidate pass above: only this request's
                            // own cancellation may degrade to an empty file. Anything else is a
                            // real defect and must not hide behind valid-looking aggregates.
                            Err(salsa::Cancelled::Local) => Vec::new(),
                            Err(other) => std::panic::resume_unwind(Box::new(other)),
                        }
                    })
                    .collect()
            })
        };

        // Fold: code -> (bucket, total count, files-affected). All occurrences of a code
        // share a bucket under one config, so first-seen is representative.
        let mut map: HashMap<String, (ide::SeverityBucket, usize, usize)> = HashMap::new();
        for file_diags in &per_file {
            let mut seen_here: HashSet<&str> = HashSet::new();
            for (code, bucket) in file_diags {
                let entry = map.entry(code.clone()).or_insert((*bucket, 0, 0));
                entry.1 += 1;
                if seen_here.insert(code.as_str()) {
                    entry.2 += 1;
                }
            }
        }

        let mut aggregates: Vec<CodeAggregate> = map
            .into_iter()
            .filter(|(_, (bucket, _, _))| *bucket >= opts.min_severity)
            .filter(|(code, _)| opts.codes.is_empty() || opts.codes.iter().any(|c| c == code))
            .map(|(code, (severity, count, files_affected))| CodeAggregate {
                code,
                severity,
                count,
                files_affected,
            })
            .collect();
        // Most-severe first, then most-frequent, then code for a stable order.
        aggregates.sort_by(|a, b| {
            b.severity.cmp(&a.severity).then(b.count.cmp(&a.count)).then(a.code.cmp(&b.code))
        });
        // The author pass above ran on clones of its own, so the deep trim here sees
        // the whole sweep's working set and no live handle.
        self.trim_after_sweep();

        WorkspaceSweep {
            aggregates,
            files_swept,
            files_total,
            files_out_of_scope,
            files_unread: self.holes.len(),
            findings_ignored_by_author: author_ignored.load(std::sync::atomic::Ordering::Relaxed),
            author_head: author_filter.map(|f| f.short_identity()),
            truncated,
            // Re-read: cancellation can also arrive during the author pass, and a
            // report that hid it would present a partial sweep as a complete one.
            cancelled: cancelled || cancel.is_cancelled(),
            baseline,
            baseline_epoch: self.diagnostics_baseline.epoch().to_owned(),
            lacks_owning_configuration,
        }
    }
}

/// Thread-name prefix of the sweep's own rayon pool. The global pool's workers are
/// unnamed, so the prefix is what tells a dedicated worker from a shared one.
const SWEEP_THREAD_PREFIX: &str = "bsl-diag-fanout-";

/// Build a resident's fan-out pool.
///
/// Named so a test can tell a dedicated worker from a shared one, and so a stack in a
/// report names the pool that owns the frame. Sized BELOW the core count: the sweep is a
/// batch job and must leave a core for the interactive requests it runs beside.
fn build_sweep_pool() -> rayon::ThreadPool {
    let threads =
        std::thread::available_parallelism().map(|n| n.get().saturating_sub(1).max(1)).unwrap_or(1);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("{SWEEP_THREAD_PREFIX}{i}"))
        .build()
        .expect("a rayon pool with an explicit thread count")
}

/// Where the diagnostics fan-out actually ran.
///
/// Salsa attaches at most one database per thread, and every job of this sweep carries
/// its OWN db clone — so a worker shared with anything else, or a job free to open nested
/// parallel work, can attach a second database mid-query and panic. Neither property is
/// visible in a result, so the jobs report them here and a test reads them back.
///
/// Everything is keyed by the thread that STARTED the sweep, not by a process-wide
/// counter: dozens of unit tests call `workspace_aggregates`, they run in parallel, and a
/// shared tally would make these gates report on whatever else the binary happened to be
/// doing — a gate that goes red on correct code proves nothing.
#[cfg(test)]
pub(super) mod sweep_probe {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::thread::ThreadId;

    #[derive(Default, Clone)]
    struct Observed {
        /// One entry per fan-out job: the thread it landed on, and whether nested
        /// parallel work was barred there.
        jobs: Vec<(String, bool)>,
        /// How many times this sweep warmed the configuration inventory. Counted because
        /// the warm-up loads EVERY config root: a sweep with nothing to sweep, or one
        /// whose request is already cancelled, must not pay for it.
        warms: usize,
    }

    fn observed() -> &'static Mutex<HashMap<ThreadId, Observed>> {
        static OBSERVED: OnceLock<Mutex<HashMap<ThreadId, Observed>>> = OnceLock::new();
        OBSERVED.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn with_entry<T>(origin: ThreadId, f: impl FnOnce(&mut Observed) -> T) -> T {
        let mut map = observed().lock().unwrap_or_else(|e| e.into_inner());
        f(map.entry(origin).or_default())
    }

    /// Called from inside a fan-out job, which runs on a pool worker and therefore has to
    /// be told which sweep it belongs to.
    pub(super) fn record_job(origin: ThreadId) {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_owned();
        let guarded = stdx::par_guard::no_nested_parallelism();
        with_entry(origin, |entry| entry.jobs.push((name, guarded)));
    }

    /// Called where the sweep warms the inventory — on the thread that started it.
    pub(super) fn record_warm() {
        with_entry(std::thread::current().id(), |entry| entry.warms += 1);
    }

    /// Watch the sweeps started by THIS thread, from an empty slate.
    pub(crate) fn watch() -> Watch {
        let origin = std::thread::current().id();
        with_entry(origin, |entry| *entry = Observed::default());
        Watch { origin }
    }

    pub(crate) struct Watch {
        origin: ThreadId,
    }

    impl Watch {
        pub(crate) fn jobs(&self) -> Vec<(String, bool)> {
            with_entry(self.origin, |entry| entry.jobs.clone())
        }

        pub(crate) fn warms(&self) -> usize {
            with_entry(self.origin, |entry| entry.warms)
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            observed().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.origin);
        }
    }
}

/// Per-rayon-worker sweep state: an [`Analysis`] over an owned db clone plus whether
/// that clone's salsa cancellation token has been registered with the sweep's
/// [`RequestCancel`]. A rayon split clones the worker; the fresh db handle carries a
/// FRESH token, so `Clone` resets `registered` and the split re-registers before its
/// first query.
struct SweepWorker {
    analysis: Analysis,
    registered: bool,
    /// The thread that started this sweep. A rayon split runs on a worker thread, so a
    /// job cannot name its own sweep without carrying this along.
    #[cfg(test)]
    origin: std::thread::ThreadId,
}

impl SweepWorker {
    fn new(db: RootDatabaseImpl) -> Self {
        Self {
            analysis: Analysis::from_database(db),
            registered: false,
            #[cfg(test)]
            origin: std::thread::current().id(),
        }
    }
}

impl Clone for SweepWorker {
    fn clone(&self) -> Self {
        Self {
            analysis: Analysis::from_database(self.analysis.database().clone()),
            registered: false,
            // Inherited, not re-read: the clone happens ON a pool worker, and reading the
            // current thread there would name the worker instead of the sweep.
            #[cfg(test)]
            origin: self.origin,
        }
    }
}

/// Resolve a path to the same key the loader indexed by (`enumerate_bsl_files`
/// canonicalises). Lets a request path in any form resolve to the resident FileId.
///
/// Resolved as far as the file system allows rather than all-or-nothing: a directory
/// above the file turning unreadable refuses the traversal `canonicalize` needs while
/// every link above that door still resolves. Keying the file by its raw spelling there
/// names it by the way it is written rather than the way it lies, and through a
/// symlinked ancestor those are two different keys — so a live, indexed file would read
/// as unknown, answered as "not a workspace file" rather than as one that cannot be
/// read right now, for as long as the door stays shut.
pub(super) fn canonical_key(path: &Path) -> String {
    crate::change_hub::resolve_as_far_as_it_goes(path).to_string_lossy().into_owned()
}

/// Apply drifted XML metadata + modified BSL bodies to the resident under an
/// already-held lock, shared by the scan and event-driven drift paths so both mutate
/// the resident identically. Returns `(needs_rebuild, moved)`: a full rebuild is
/// needed when an XML path resolves outside every config root (a symlink the
/// point-refresh cannot express) or a modified `.bsl` has no resident FileId (the file
/// universe moved); `moved` is whether any Salsa input actually changed. `fp_of` yields
/// the on-disk fingerprint of a `modified_bsl` path so an already-current body is
/// skipped. The caller owns the drift-baseline update (full rebase vs incremental).
pub(super) fn apply_resident_changes(
    resident: &mut DiagnosticsResident,
    xml_paths: &[PathBuf],
    added_bsl: &[String],
    modified_bsl: &[String],
    removed_bsl: &[String],
    fp_of: impl Fn(&str) -> Option<u64>,
    stats: &HashMap<String, u64>,
) -> (bool, bool) {
    use base_db::{SourceDatabase, SourceRoot};
    use ide_host_core::{set_file_text_source, FileTextSource, VfsWrite};

    // Pre-classification: an XML path resolving outside every registered config root is
    // drift the point-refresh cannot express — `refresh_metadata_substrate` gates its
    // re-discovery on `changed.starts_with(root)`, so it would silently no-op. Bail to a
    // full rebuild, which re-reads through the discovery joins, symlinks and all.
    let config_roots = resident.db.all_config_paths();
    let xml_outside_roots =
        xml_paths.iter().any(|p| !config_roots.iter().any(|(_, root)| p.starts_with(root)));
    if xml_outside_roots {
        return (true, false);
    }

    let mut moved = false;

    // (1) Reconcile the file universe FIRST — mirrors the LSP's `process_changes`
    // discipline (one FileSet clone, per-file inputs, one `set_source_root`), so the
    // substrate refresh below resolves module back-links through an up-to-date VFS and
    // root. Per-file ordering matters: the source-root + content-revision inputs are
    // registered BEFORE the file becomes visible through the FileSet
    // (`file_text_query` panics on a visible file with no revision).
    let mut file_set_modified = false;
    let mut file_set = {
        let db = &resident.db;
        db.source_root_input(crate::graph::input::GRAPH_SOURCE_ROOT).root(db).file_set().clone()
    };
    for path in added_bsl {
        // Vanished again before we got here (create+delete coalesced apart): the
        // removal pass — or the next drift window — settles it.
        if fp_of(path).is_none() && !Path::new(path).is_file() {
            continue;
        }
        // Read BEFORE interning. A file that cannot be read is not registered at all,
        // and "at all" has to include the VFS: `alloc_file_id` used to run first, so
        // skipping only from the read onwards would leave a `FileId` with no file-set
        // entry, which the next query resolves into a `path_for_file` panic.
        let text = match base_db::read_disk_text(Path::new(path)) {
            Ok(text) => text,
            Err(_) => {
                // Never admitted into this resident, so healing it later must ask the
                // configuration gate first.
                resident.holes.insert(path.clone(), HoleOrigin::Pending);
                moved = true;
                continue;
            }
        };
        let vfs_path = VfsPath::new(path.clone());
        let file_id = resident.vfs.with_write(|vfs| vfs.alloc_file_id(vfs_path.clone()));
        if let Some(&known) = resident.by_path.get(path.as_str()) {
            if known != file_id {
                // The path is already registered under a different id — an aliasing
                // (symlink/canonicalisation) case registration cannot express safely.
                return (true, moved);
            }
        }
        resident.db.set_file_source_root(file_id, crate::graph::input::GRAPH_SOURCE_ROOT);
        set_file_text_source(&mut resident.db, file_id, FileTextSource::Disk(&text));
        if file_set.path_for_file(&file_id).is_none() {
            file_set.insert(file_id, vfs_path);
            file_set_modified = true;
        }
        // The classifier's `key` IS the canonical by_path spelling (both come from the
        // scan-universe canonicalisation), so insert it verbatim — re-canonicalising
        // here could diverge on a path that vanished between classify and apply.
        resident.by_path.insert(path.clone(), file_id);
        // A path returning to service leaves the hole list, whichever branch brings it
        // back. `by_path` and `holes` must not intersect: the workspace denominator is
        // their sum, and the hole list drives the retry pass — a served path left in it
        // would be re-read from disk every drift window for nothing.
        resident.holes.remove(path.as_str());
        moved = true;
    }
    for path in removed_bsl {
        // A deleted file is no longer a hole either: the retry list must not keep
        // probing a path the workspace no longer has, and leaving it there would
        // report a phantom in `unread_files` for as long as it is never recreated.
        // The origin decides whether anything was ever registered under this path —
        // a hole is not always the retry list's to retire, because the retry window
        // is throttled and `reconcile_tick` never opens it at all.
        let was_hole = resident.holes.remove(path.as_str());
        let registered = was_hole == Some(HoleOrigin::Admitted);
        // The resident moved if the removal changed anything the workspace REPORTS,
        // and the hole list is reported: it is `unread_files`, it is half of
        // `files_total`, and a non-empty one is what makes the answer stale. A
        // `Pending` hole has no registration by construction, so counting only
        // registrations would let all three change under an unmoved generation — the
        // same `result_id` answering "unreadable" and then "not in workspace". The
        // creation of that hole bumps the generation; its removal owes the same.
        // Never indexed and never a hole → nothing moved (an untracked removal is not
        // drift).
        let moved_here = was_hole.is_some() || resident.by_path.contains_key(path.as_str());
        file_set_modified |= retire_registration(resident, &mut file_set, path, registered);
        moved |= moved_here;
    }
    if file_set_modified {
        resident.db.set_source_root(
            crate::graph::input::GRAPH_SOURCE_ROOT,
            SourceRoot::new_local(file_set),
        );
    }

    // (2) Refresh the per-MDO substrate. Beside the drifted `.xml`, a created or
    // deleted common-module/service body changes its listing's `module_file`
    // reverse-index entry (the body is ordinary source, so it never flows through the
    // metadata-XML path) — include those bodies in the same re-discovery, exactly as
    // the LSP does. The config-revision bump stays `.xml`-only: a body add/remove does
    // not change the whole-config metadata content.
    let structural_listing_bodies: Vec<PathBuf> = added_bsl
        .iter()
        .chain(removed_bsl)
        .map(PathBuf::from)
        .filter(|p| project_model::is_substrate_listed_body_path(p))
        .filter(|p| config_roots.iter().any(|(_, root)| p.starts_with(root)))
        .collect();
    if !xml_paths.is_empty() || !structural_listing_bodies.is_empty() {
        let mut refresh: Vec<PathBuf> = xml_paths.to_vec();
        refresh.extend(structural_listing_bodies);
        ide_host_core::refresh_metadata_substrate(&mut resident.db, &resident.vfs, &refresh);
        if !xml_paths.is_empty() {
            resident.db.bump_config_for_paths(xml_paths.iter().map(|p| p.as_path()));
        }
        moved = true;
    }

    // `.bsl` bodies: disk-backed re-key. A body already at its on-disk fingerprint (a
    // racing caller beat us) is skipped.
    let mut became_holes: Vec<PathBuf> = Vec::new();
    for path in modified_bsl {
        let Some(fp) = fp_of(path) else { continue };
        if stats.get(path).copied() == Some(fp) {
            continue;
        }
        let Some(&file_id) = resident.by_path.get(path) else {
            // A path already held as a hole is not "never indexed": the retry cycle
            // owns it, and healing it HERE would be wrong twice over — the file set
            // was already published above, so the insert would not reach the db, and
            // an admission would slip past the configuration gate.
            if resident.holes.contains_key(path) {
                continue;
            }
            return (true, moved); // a modified `.bsl` we never indexed → structural
        };
        match base_db::read_disk_text(Path::new(path)) {
            Ok(text) => {
                set_file_text_source(&mut resident.db, file_id, FileTextSource::Disk(&text))
            }
            Err(_) => {
                // Unreadable now. The empty overlay is mandatory (a disk-backed re-read
                // would panic), but it is no longer passed off as content: the file
                // leaves service and joins the retry list, so consumers see "known
                // but unreadable" instead of an empty module. `Admitted` — it was
                // already serving under this configuration.
                set_file_text_source(&mut resident.db, file_id, FileTextSource::Unreadable);
                resident.by_path.remove(path);
                resident.holes.insert(path.clone(), HoleOrigin::Admitted);
                became_holes.push(PathBuf::from(path));
            }
        }
        moved = true;
    }

    // A body that just became a hole has to LOSE its `module_file` back-link, and the
    // substrate pass above already ran — it keys off `added_bsl`/`removed_bsl`, and
    // this transition is neither. Without re-issuing it here, the same disk state
    // answers differently depending on WHEN the file became unreadable: at build time
    // the back-link is `None`, at drift time it still points at the tombstoned FileId.
    // Consumers of a non-empty back-link over an empty symbol tree conclude the module
    // has no API and emit blocking findings ("create procedure …") against innocent
    // files, where `None` makes them return silently.
    let became_holes: Vec<PathBuf> = became_holes
        .into_iter()
        .filter(|p| project_model::is_substrate_listed_body_path(p))
        .filter(|p| config_roots.iter().any(|(_, root)| p.starts_with(root)))
        .collect();
    if !became_holes.is_empty() {
        ide_host_core::refresh_metadata_substrate(&mut resident.db, &resident.vfs, &became_holes);
    }

    (false, moved)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{module_path, sample_workspace, wait_ready, write};
    use super::super::{DiagnosticsState, ResidentOutcome};
    use super::*;
    use crate::tools::file_request::RootedPathError;
    use ide::DiagnosticsConfig;

    /// A file's key is a statement about where it lies, so nothing that merely blocks
    /// the way to it may change the key. Closing a directory above the file refuses the
    /// traversal `canonicalize` needs while leaving every link above the door resolvable
    /// — and through a symlinked ancestor the raw spelling is a DIFFERENT key, so the
    /// table would stop recognising a file it holds.
    ///
    /// Both halves of the shape are load-bearing: the link is what makes the two
    /// spellings differ at all, and the closed directory is what refuses the full
    /// resolution. Either one alone leaves the key trivially unchanged.
    #[cfg(unix)]
    #[test]
    fn a_key_survives_a_door_closed_above_the_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let closed = real.join("closed");
        std::fs::create_dir_all(&closed).unwrap();
        std::fs::write(closed.join("Module.bsl"), "x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let through_the_link = link.join("closed").join("Module.bsl");
        let open_key = canonical_key(&through_the_link);
        assert_ne!(
            open_key,
            through_the_link.to_string_lossy(),
            "the link has to make the two spellings differ, or the case under test is absent",
        );

        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Permissions do not bind UID 0, and then the input this test needs cannot exist.
        let bound = std::fs::read_dir(&closed).is_err();
        let closed_key = canonical_key(&through_the_link);
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !bound {
            return;
        }

        assert_eq!(closed_key, open_key, "a closed door above the file moved its key");
    }

    /// Every fan-out job of the sweep must land on THIS resident's own pool and must be
    /// barred from opening nested parallel work.
    ///
    /// Both are salsa's rule, not a preference: a job carries its own db clone, so a
    /// worker shared with another sweep — or a job that parks in a nested `rayon::scope`
    /// and steals a sibling — attaches a second database to a thread mid-query, and salsa
    /// panics with `Cannot change database mid-query`. On the shared global pool the jobs
    /// run on unnamed workers with no guard, which is exactly what this refuses.
    #[test]
    fn the_sweep_runs_only_on_its_own_guarded_pool() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let watch = super::sweep_probe::watch();
        let out = state.read_mut(|resident, _| {
            resident.workspace_aggregates(
                &resident.config().clone(),
                &sweep_opts(),
                &RequestCancel::default(),
            )
        });
        assert!(matches!(out, ResidentOutcome::Ready(..)), "the sweep runs");

        let seen = watch.jobs();
        assert!(!seen.is_empty(), "the sweep has to have run jobs for this to mean anything");
        let strays: Vec<_> = seen
            .iter()
            .filter(|(thread, guarded)| !thread.starts_with(SWEEP_THREAD_PREFIX) || !*guarded)
            .cloned()
            .collect();
        assert!(
            strays.is_empty(),
            "every job belongs on this resident's guarded pool, but these did not: {strays:?}",
        );
    }

    /// A sweep with nothing to sweep does no work at all.
    ///
    /// The fan-out's safety warm-up loads EVERY configuration root, so running it for a
    /// request that asked for no files turns `max_files: 0` — or a scope that admitted
    /// nothing — into a full configuration parse under the resident lock. A cancelled
    /// request is the same case: it wants an answer no longer.
    #[test]
    fn a_sweep_with_nothing_to_sweep_warms_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let watch = super::sweep_probe::watch();

        let mut empty = sweep_opts();
        empty.max_files = 0;
        let pooled = state.read_mut(|resident, _| {
            resident.workspace_aggregates(
                &resident.config().clone(),
                &empty,
                &RequestCancel::default(),
            );
            resident.sweep_pool.get().is_some()
        });
        assert_eq!(watch.warms(), 0, "a zero-file sweep loads no configuration");
        assert!(
            matches!(pooled, ResidentOutcome::Ready(false, _)),
            "a zero-file sweep does not raise the fan-out pool either",
        );

        let cancelled = RequestCancel::default();
        cancelled.cancel_all();
        let _ = state.read_mut(|resident, _| {
            resident.workspace_aggregates(&resident.config().clone(), &sweep_opts(), &cancelled)
        });
        assert_eq!(watch.warms(), 0, "an already-cancelled sweep loads no configuration");

        // The positive control: with files to sweep and a live request, the warm-up DOES
        // run — otherwise the two zeroes above would hold for a sweep that never warms.
        let _ = state.read_mut(|resident, _| {
            resident.workspace_aggregates(
                &resident.config().clone(),
                &sweep_opts(),
                &RequestCancel::default(),
            )
        });
        assert_eq!(watch.warms(), 1, "a real sweep warms exactly once");
        let pooled = state.read(|resident, _| resident.sweep_pool.get().is_some());
        assert!(
            matches!(pooled, ResidentOutcome::Ready(true, _)),
            "the control has to raise the pool, or the two refusals above prove nothing",
        );
    }

    /// First use builds the resident db over the workspace and resolves a request
    /// path to a FileId, then computes diagnostics for it.
    #[test]
    fn builds_resident_and_serves_file_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let path = module_path(root, "Сервер");
        let out = state.read(|resident, _| {
            let file_id = resident.file_id_for(&path).expect("path resolves to a resident FileId");
            resident.analysis().diagnostics(file_id, &DiagnosticsConfig::default()).len()
        });
        match out {
            ResidentOutcome::Ready(_, _) => {}
            _ => panic!("expected Ready outcome from a loaded db"),
        }
    }

    /// The resident is disk-backed: a workspace file is registered by content revision,
    /// not pinned as a `FileTextInput` overlay, so `file_text_query` re-reads it from disk
    /// under the LRU cap. This is what keeps a whole-workspace resident from OOMing. The
    /// file's text must still be queryable (diagnostics ran above), it just must not be
    /// held resident as a salsa input.
    #[test]
    fn resident_text_is_disk_backed_not_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let path = module_path(root, "Сервер");
        let out = state.read(|resident, _| {
            let file_id = resident.file_id_for(&path).expect("path resolves to a resident FileId");
            let pinned = resident.db.try_file_text(file_id).is_some();
            let len = resident.analysis().file_text(file_id).len();
            (pinned, len)
        });
        match out {
            ResidentOutcome::Ready((pinned, len), _) => {
                assert!(!pinned, "workspace file must be disk-backed, not pinned as an overlay");
                assert!(len > 0, "disk-backed text must still be readable on demand");
            }
            _ => panic!("expected Ready outcome"),
        }
    }

    /// The resident loads the project's `bsl-analyzer.toml` and exposes it as the
    /// effective config, so `file`/`workspace` honour the same disabled rules and tuned
    /// thresholds as LSP and CLI — not analyzer defaults.
    #[test]
    fn resident_config_reflects_project_toml() {
        use ide::DiagnosticCode;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write(
            root,
            "bsl-analyzer.toml",
            "[source]\nroot = \".\"\n\n\
             [diagnostics.parameters]\n\
             Typo = false\n\n\
             [diagnostics.parameters.LineLength]\n\
             maxLineLength = 200\n",
        );

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let out = state.read(|resident, _| {
            let config = resident.config();
            (
                config.is_disabled(DiagnosticCode::Typo),
                config.get_int(DiagnosticCode::LineLength, "maxLineLength"),
            )
        });
        match out {
            ResidentOutcome::Ready((typo_disabled, line_len), _) => {
                assert!(typo_disabled, "project toml disables Typo");
                assert_eq!(line_len, Some(200), "project toml sets the LineLength threshold");
            }
            _ => panic!("expected Ready outcome"),
        }
    }

    /// A `diagnostics file` request may pass a workspace-relative path; it must resolve
    /// against the workspace root, not the process CWD.
    #[test]
    fn file_id_resolves_relative_path_against_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let rel = Path::new("CommonModules/Сервер/Ext/Module.bsl");
        let abs = module_path(root, "Сервер");
        let found = state.read(|resident, _| {
            (resident.file_id_for(rel).is_some(), resident.file_id_for(&abs).is_some())
        });
        match found {
            ResidentOutcome::Ready((rel_ok, abs_ok), _) => {
                assert!(rel_ok, "relative path resolves against the workspace root");
                assert!(abs_ok, "absolute path still resolves");
            }
            _ => panic!("expected Ready"),
        }
    }

    /// An absolute path already names one file, so a root cannot also be honoured: joining
    /// them keeps the absolute path and drops the root SILENTLY, which is how a request
    /// naming an extension gets answered from the configuration. The refusal is what keeps
    /// that from happening, and it names which of the two facts was the problem — an
    /// unregistered root calls for a different correction than a self-contradicting pair.
    #[test]
    fn a_rooted_pair_is_refused_rather_than_resolved_against_the_wrong_root() {
        use super::super::test_support::{
            extension_root_id, workspace_with_an_outside_extension, SHARED_MODULE_REL,
        };
        let (_dir, workspace, extension) = workspace_with_an_outside_extension();
        let root_id = extension_root_id(&workspace, &extension);

        let state = DiagnosticsState::for_workspace(workspace.clone());
        state.ensure_loading();
        wait_ready(&state);

        let outcome = state.read(|resident, _| {
            let absolute = workspace.join(SHARED_MODULE_REL);
            (
                resident.resolve_rooted_path(Some(&root_id), &absolute),
                resident.resolve_rooted_path(Some("нет-такого"), Path::new(SHARED_MODULE_REL)),
                resident.resolve_rooted_path(Some(""), &absolute),
                resident.resolve_rooted_path(None, Path::new(SHARED_MODULE_REL)),
                resident.resolve_rooted_path(Some(&root_id), Path::new(SHARED_MODULE_REL)),
            )
        });
        let ResidentOutcome::Ready((absolute_under_root, unknown, empty_root, no_root, rooted), _) =
            outcome
        else {
            panic!("expected Ready");
        };

        assert_eq!(
            absolute_under_root,
            Err(RootedPathError::AbsolutePathWithRootId(root_id.clone())),
            "an absolute path under a root is refused, not silently read as itself",
        );
        assert_eq!(
            unknown,
            Err(RootedPathError::RootNotRegistered("нет-такого".to_owned())),
            "an unregistered root is refused by its own name",
        );
        assert_eq!(
            empty_root.as_deref(),
            Ok(workspace.join(SHARED_MODULE_REL).as_path()),
            "the configuration's id over an absolute path is not a contradiction",
        );
        assert_eq!(
            no_root.as_deref(),
            Ok(Path::new(SHARED_MODULE_REL)),
            "saying nothing about roots keeps the path exactly as it came",
        );
        assert_eq!(
            rooted.as_deref(),
            Ok(extension.join(SHARED_MODULE_REL).as_path()),
            "and the pair resolves through the root's own declared spelling",
        );
    }

    /// A request path is read against a root, so it has to stay inside it. `Path::join` keeps
    /// `..` and the lookup canonicalises afterwards, so without the check the pair
    /// (extension root, `../ws/<модуль>`) resolves to the CONFIGURATION's module — the same
    /// silent mis-answer this node exists to prevent, arriving through the other half of the
    /// pair.
    #[test]
    fn a_path_that_climbs_out_of_its_root_is_refused() {
        use super::super::test_support::{
            extension_root_id, workspace_with_an_outside_extension, SHARED_MODULE_REL,
        };
        let (_dir, workspace, extension) = workspace_with_an_outside_extension();
        let root_id = extension_root_id(&workspace, &extension);
        let state = DiagnosticsState::for_workspace(workspace.clone());
        state.ensure_loading();
        wait_ready(&state);

        let escaping = format!("../ws/{SHARED_MODULE_REL}");
        let outcome = state.read(|resident, _| {
            (
                resident.resolve_rooted_path(Some(&root_id), Path::new(&escaping)),
                // The stand is only honest if that spelling really does reach a served file:
                // refusing a path that resolves to nothing would prove nothing at all.
                resident.file_id_for(&extension.join(&escaping)),
                resident.file_id_for(&workspace.join(SHARED_MODULE_REL)),
            )
        });
        let ResidentOutcome::Ready((refused, escaped_to, configuration), _) = outcome else {
            panic!("expected Ready");
        };

        assert!(
            escaped_to.is_some() && escaped_to == configuration,
            "the stand is real: joined and canonicalised, that spelling names the \
             configuration's module — a file the resident serves, under the other root",
        );
        assert_eq!(refused, Err(RootedPathError::PathIsNotPlainRelative(root_id)));
    }

    /// Two spellings that look like corner cases and are decided by one rule. `..` is refused
    /// whether or not it would have come back inside the root — resolving it is the kernel's
    /// job, and no producer of keys emits it. A link is not refused at all: it sits inside the
    /// root, so it is a file of that root, and it reads as its target the way it would for
    /// anything else opening that path.
    #[cfg(unix)]
    #[test]
    fn a_link_inside_the_root_is_its_file_while_any_dotdot_is_refused() {
        use super::super::test_support::{
            extension_root_id, workspace_with_an_outside_extension, SHARED_MODULE_REL,
        };
        let (_dir, workspace, extension) = workspace_with_an_outside_extension();
        let root_id = extension_root_id(&workspace, &extension);
        // A link inside the extension pointing at the configuration's module: no `..`, and the
        // lookup canonicalises straight into the other root.
        let alias = extension.join("Alias.bsl");
        std::os::unix::fs::symlink(workspace.join(SHARED_MODULE_REL), &alias).unwrap();

        let state = DiagnosticsState::for_workspace(workspace.clone());
        state.ensure_loading();
        wait_ready(&state);

        let detour = "CommonModules/Общий/../Общий/Ext/Module.bsl";
        let outcome = state.read(|resident, _| {
            (
                resident.resolve_rooted_path(Some(&root_id), Path::new(detour)),
                resident.resolve_rooted_path(Some(&root_id), Path::new("Alias.bsl")),
                resident.file_id_for(&workspace.join(SHARED_MODULE_REL)),
                resident.file_id_for(&alias),
            )
        });
        let ResidentOutcome::Ready((detour, aliased, configuration, through_alias), _) = outcome
        else {
            panic!("expected Ready");
        };

        assert_eq!(
            detour,
            Err(RootedPathError::PathIsNotPlainRelative(root_id.clone())),
            "`..` is refused by its name, not by where it would have landed: the same file is \
             reachable by its plain spelling, and that is the one to send",
        );
        assert!(
            through_alias.is_some() && through_alias == configuration,
            "the stand is real: followed, the link reaches the configuration's module",
        );
        // A link sitting in the extension is a file of the extension, and reading it yields
        // its target — the same answer any tool opening that path gives. The pair still names
        // one file; what it does not promise is that the bytes live in this directory tree.
        assert_eq!(
            aliased.as_deref(),
            Ok(extension.join("Alias.bsl").as_path()),
            "a name that stays inside its root is honoured, link or not",
        );
    }

    /// An escape does not have to land in ANOTHER root to be an escape. Asking who owns the
    /// landing place misses exactly this: the target belongs to no root canonically, so the
    /// attribution falls back to the walked spelling — which is this very join — and the root
    /// the caller named matches itself, `..` and all. The question that catches it is asked of
    /// the name, not of the destination.
    #[cfg(unix)]
    #[test]
    fn an_escape_into_ground_no_root_owns_is_still_an_escape() {
        use super::super::test_support::{
            extension_root_id, workspace_with_an_outside_extension, write, SHARED_MODULE_REL,
        };
        let (dir, workspace, extension) = workspace_with_an_outside_extension();
        let root_id = extension_root_id(&workspace, &extension);
        // Served by the resident, under the configuration, but canonically outside every root.
        let outside = dir.path().join("served-outside");
        std::fs::create_dir_all(&outside).unwrap();
        write(
            &outside,
            SHARED_MODULE_REL,
            "&НаСервере\nФункция Наружная() Экспорт Возврат 1; КонецФункции\n",
        );
        std::os::unix::fs::symlink(&outside, workspace.join("Linked")).unwrap();

        let state = DiagnosticsState::for_workspace(workspace.clone());
        state.ensure_loading();
        wait_ready(&state);

        let escaping = format!("../ws/Linked/{SHARED_MODULE_REL}");
        let outcome = state.read(|resident, _| {
            (
                resident.resolve_rooted_path(Some(&root_id), Path::new(&escaping)),
                resident.file_id_for(&outside.join(SHARED_MODULE_REL)).is_some(),
            )
        });
        let ResidentOutcome::Ready((refused, served), _) = outcome else {
            panic!("expected Ready")
        };

        assert!(served, "the stand is real: that file is one the resident serves");
        assert_eq!(refused, Err(RootedPathError::PathIsNotPlainRelative(root_id)));
    }

    /// The rule is about the SPELLING, and every way of spelling something the root does not
    /// contain is refused by it — not just `..`. The Windows cases are the reason it is written
    /// that way: neither a leading separator nor a drive-relative name counts as absolute
    /// there, so the earlier `..`-only rule let both through, and `join` throws the base away
    /// for each. Checked here on every platform, because it is the spelling that is rejected,
    /// not the filesystem's reading of it.
    #[test]
    fn only_plain_relative_names_are_read_against_a_root() {
        use super::super::test_support::{
            extension_root_id, workspace_with_an_outside_extension, SHARED_MODULE_REL,
        };
        let (_dir, workspace, extension) = workspace_with_an_outside_extension();
        let root_id = extension_root_id(&workspace, &extension);
        let state = DiagnosticsState::for_workspace(workspace.clone());
        state.ensure_loading();
        wait_ready(&state);

        let refused = |resident: &DiagnosticsResident, spelling: &str| {
            resident.resolve_rooted_path(Some(&root_id), Path::new(spelling))
                == Err(RootedPathError::PathIsNotPlainRelative(root_id.clone()))
        };
        let outcome = state.read(|resident, _| {
            (
                refused(resident, "../ws/CommonModules/Общий/Ext/Module.bsl"),
                refused(resident, "./CommonModules/Общий/Ext/Module.bsl"),
                // Parsed as a root and a drive only on Windows; on Unix a backslash is an
                // ordinary character, so these are plain file names that stay inside the root.
                // The rule reads COMPONENTS for exactly that reason — the hazard is in how the
                // platform splits a path, not in the characters.
                cfg!(windows) == refused(resident, "\\Windows\\M.bsl"),
                cfg!(windows) == refused(resident, "C:M.bsl"),
                resident.resolve_rooted_path(Some(&root_id), Path::new(SHARED_MODULE_REL)),
                // An unregistered root is named FIRST: fixing the path would not help.
                resident.resolve_rooted_path(Some("нет-такого"), Path::new("../M.bsl")),
            )
        });
        let ResidentOutcome::Ready((climbing, dotted, rooted, drive, plain, unknown), _) = outcome
        else {
            panic!("expected Ready");
        };

        assert!(climbing, "`..` cannot be resolved here, so it is not read at all");
        assert!(dotted, "`.` survives into the graph id, which was built without one");
        assert!(rooted, "a leading separator is refused wherever the platform reads it as one");
        assert!(drive, "and so is a drive-relative spelling, absolute or not");
        assert_eq!(
            plain.as_deref(),
            Ok(extension.join(SHARED_MODULE_REL).as_path()),
            "while a plain name — the only shape a key is ever built in — resolves",
        );
        assert_eq!(unknown, Err(RootedPathError::RootNotRegistered("нет-такого".to_owned())));
    }

    /// The table keeps a file that lies outside every root under the root the walk reached it
    /// through, and the index hands out exactly such keys: a directory link out of the tree is
    /// walked (the enumerator follows links), so the hit reads `(configuration, Linked/M.bsl)`
    /// while the file itself canonicalises far away. A test for containment would refuse the
    /// index's own key — asking the table who OWNS the path accepts it and still refuses the
    /// escapes.
    #[cfg(unix)]
    #[test]
    fn a_key_of_a_file_reached_through_a_link_out_of_the_tree_still_resolves() {
        use super::super::test_support::{sample_workspace, write};
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let outside = dir.path().join("outside-tree");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        sample_workspace(&workspace);
        write(
            &outside,
            "CommonModules/Связанный/Ext/Module.bsl",
            "&НаСервере\nФункция Связанная() Экспорт Возврат 1; КонецФункции\n",
        );
        std::os::unix::fs::symlink(&outside, workspace.join("Linked")).unwrap();

        let state = DiagnosticsState::for_workspace(workspace.clone());
        state.ensure_loading();
        wait_ready(&state);

        let through_the_link = "Linked/CommonModules/Связанный/Ext/Module.bsl";
        let outcome = state.read(|resident, _| {
            let resolved = resident.resolve_rooted_path(Some(""), Path::new(through_the_link));
            let served = resolved.as_ref().ok().and_then(|path| resident.file_id_for(path));
            (resolved.is_ok(), served.is_some())
        });
        let ResidentOutcome::Ready((accepted, served), _) = outcome else {
            panic!("expected Ready")
        };

        assert!(accepted, "the configuration's key for a linked file is not an escape");
        assert!(served, "and it names a file this resident serves");
    }

    /// The resident's metadata substrate resolves a common module's `Ext/Module.bsl`
    /// back to the SAME FileId the resident indexed for it. This guards the seeding
    /// invariant: the VFS is pre-seeded with the resident's `.bsl` ids before the
    /// bootstrap interns the metadata XML on top, so the reverse index carries the
    /// resident's own id. Were the ids unseeded, the bootstrap would drop the back-link
    /// and `common_module_for_file_id` would return `None`.
    #[test]
    fn resident_substrate_backlinks_common_module_to_its_own_file_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let module = module_path(root, "Сервер");
        let project = project_model::Project::new(root).expect("valid test project");
        let config_root = project
            .source_path()
            .canonicalize()
            .unwrap_or_else(|_| project.source_path().to_path_buf());
        let root_key = config_root.to_string_lossy().into_owned();

        let out = state.read(|resident, _| {
            let file_id = resident.file_id_for(&module).expect("module .bsl resolves to a FileId");
            let listing_present = resident.db.metadata_listing(&root_key).is_some();
            let resolved = resident.db.common_module_for_file_id(file_id).is_some();
            (listing_present, resolved)
        });
        match out {
            ResidentOutcome::Ready((listing_present, resolved), _) => {
                assert!(
                    listing_present,
                    "the metadata substrate must be bootstrapped for the config root"
                );
                assert!(
                    resolved,
                    "the substrate must resolve the common module through the resident's own id"
                );
            }
            _ => panic!("expected Ready outcome from a loaded db"),
        }
    }

    fn sweep_opts() -> super::super::SweepOptions {
        super::super::SweepOptions {
            min_severity: ide::SeverityBucket::Hint,
            codes: Vec::new(),
            max_files: 1000,
        }
    }

    /// A sweep cancelled before it starts does not select its files either.
    ///
    /// The workers observe the cancel through their own salsa handles, but the
    /// selection runs ahead of them — and with `max_files` at zero no worker runs at
    /// all. The out-of-scope counter is what makes the difference visible: a sweep that
    /// walked the workspace after the cancel would have counted the excluded files.
    #[test]
    fn a_cancelled_sweep_does_not_even_select_its_files() {
        use std::sync::Arc;

        use crate::cancel::RequestCancel;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        super::super::test_support::write_common_module(
            root,
            "Клиент",
            false,
            "&НаКлиенте\nПроцедура Показать() Экспорт КонецПроцедуры",
        );

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let cancelled = RequestCancel::default();
        cancelled.cancel_all();
        let out = state.read_mut(|resident, _| {
            let workdir =
                resident.workspace_root().canonicalize().expect("workspace root canonicalizes");
            let module = module_path(root, "Сервер").canonicalize().expect("module exists");
            let rel = module
                .strip_prefix(&workdir)
                .expect("module under workspace root")
                .to_string_lossy()
                .into_owned();
            let scope =
                Arc::new(base_db::AnalysisScope::from_report("vendor", &workdir, [(rel, None)]));

            let mut config = resident.config().clone();
            config.scope = Some(scope);
            let mut opts = sweep_opts();
            // No worker runs at all: whatever observes the cancel here is the selection.
            opts.max_files = 0;
            let sweep = resident.workspace_aggregates(&config, &opts, &cancelled);
            (resident.file_count(), sweep)
        });
        match out {
            ResidentOutcome::Ready((file_count, sweep), _) => {
                assert!(sweep.cancelled, "the sweep must report the cancellation");
                assert_eq!(sweep.files_swept, 0);
                assert_eq!(sweep.files_total, file_count, "coverage still describes the config");
                assert_eq!(
                    sweep.files_out_of_scope, 0,
                    "the cancelled sweep walked and classified the workspace anyway"
                );
            }
            _ => panic!("expected Ready"),
        }
    }

    /// Under a vendor-diff scope the sweep analyses only files with changed lines;
    /// the excluded rest is counted so the coverage bookkeeping stays honest.
    #[test]
    fn sweep_under_scope_excludes_unchanged_files_and_counts_them() {
        use std::sync::Arc;

        use crate::cancel::RequestCancel;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // A second module, so the scope has something to exclude.
        super::super::test_support::write_common_module(
            root,
            "Клиент",
            false,
            "&НаКлиенте\nПроцедура Показать() Экспорт КонецПроцедуры",
        );

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let out = state.read_mut(|resident, _| {
            let workdir =
                resident.workspace_root().canonicalize().expect("workspace root canonicalizes");
            let module = module_path(root, "Сервер").canonicalize().expect("module exists");
            let rel = module
                .strip_prefix(&workdir)
                .expect("module under workspace root")
                .to_string_lossy()
                .into_owned();
            let scope =
                Arc::new(base_db::AnalysisScope::from_report("vendor", &workdir, [(rel, None)]));

            let mut config = resident.config().clone();
            config.scope = Some(scope);
            let sweep =
                resident.workspace_aggregates(&config, &sweep_opts(), &RequestCancel::default());
            (resident.file_count(), sweep)
        });
        match out {
            ResidentOutcome::Ready((file_count, sweep), _) => {
                assert!(file_count > 1, "the fixture must contain more than one .bsl");
                assert_eq!(sweep.files_total, file_count);
                assert_eq!(
                    sweep.files_out_of_scope,
                    file_count - 1,
                    "everything except the admitted module must be excluded"
                );
                assert_eq!(sweep.files_swept, 1, "only the in-scope module is analysed");
            }
            _ => panic!("expected Ready"),
        }
    }

    /// A sweep whose cancellation was requested before it started produces an honest
    /// partial result (no files, `cancelled` set) and leaves the resident fully
    /// usable: a follow-up sweep with a fresh token registry completes normally —
    /// cancellation touches only per-worker clone tokens, never the master db.
    #[test]
    fn pre_cancelled_sweep_is_partial_and_leaves_the_resident_usable() {
        use crate::cancel::RequestCancel;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let cancelled_sweep = RequestCancel::default();
        cancelled_sweep.cancel_all();
        let out = state.read_mut(|resident, _| {
            resident.workspace_aggregates(
                &resident.config().clone(),
                &sweep_opts(),
                &cancelled_sweep,
            )
        });
        match out {
            ResidentOutcome::Ready(sweep, _) => {
                assert!(sweep.cancelled, "the sweep must report the cancellation");
                assert_eq!(sweep.files_swept, 0, "no file completes under a pre-cancelled sweep");
                assert!(sweep.aggregates.is_empty());
                assert_eq!(sweep.files_total, 1, "coverage bookkeeping still describes the config");
            }
            _ => panic!("expected Ready outcome"),
        }

        let out = state.read_mut(|resident, _| {
            resident.workspace_aggregates(
                &resident.config().clone(),
                &sweep_opts(),
                &RequestCancel::default(),
            )
        });
        match out {
            ResidentOutcome::Ready(sweep, _) => {
                assert!(!sweep.cancelled);
                assert_eq!(sweep.files_swept, 1, "a fresh sweep over the same resident completes");
            }
            _ => panic!("expected Ready outcome"),
        }
    }

    /// The core mechanism the sweep relies on, exercised deterministically: a
    /// cancelled salsa token of a worker-style db clone unwinds an in-flight
    /// diagnostics computation with `Cancelled::Local` (mid-file, not just at the
    /// file-boundary check), the catch contains the unwind, and the master handle
    /// keeps serving queries afterwards.
    #[test]
    fn cancelled_clone_token_unwinds_a_diagnostics_query() {
        use ide::DiagnosticsConfig;
        use std::panic::AssertUnwindSafe;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let path = module_path(root, "Сервер");
        let out = state.read(|resident, _| {
            let file_id = resident.file_id_for(&path).expect("path resolves to a resident FileId");

            let analysis = resident.analysis();
            salsa::Database::cancellation_token(analysis.database()).cancel();
            let caught = salsa::Cancelled::catch(AssertUnwindSafe(|| {
                analysis.diagnostics(file_id, &DiagnosticsConfig::default()).len()
            }));
            let unwound = matches!(caught, Err(salsa::Cancelled::Local));

            // A fresh clone is a different salsa handle with its own token: the
            // same query must complete normally after the first clone's cancel —
            // reaching the return proves it did not unwind.
            let _ = resident.analysis().diagnostics(file_id, &DiagnosticsConfig::default());
            unwound
        });
        match out {
            ResidentOutcome::Ready(unwound, _) => {
                assert!(unwound, "a cancelled clone token must unwind the query with Local");
            }
            _ => panic!("expected Ready outcome"),
        }
    }

    /// A cancel arriving after the sweep completed is a no-op: the result is already
    /// final and the resident keeps serving per-file diagnostics.
    #[test]
    fn late_cancel_after_sweep_completion_is_a_noop() {
        use crate::cancel::RequestCancel;
        use ide::DiagnosticsConfig;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let cancel = RequestCancel::default();
        let out = state.read_mut(|resident, _| {
            resident.workspace_aggregates(&resident.config().clone(), &sweep_opts(), &cancel)
        });
        let sweep = match out {
            ResidentOutcome::Ready(sweep, _) => sweep,
            _ => panic!("expected Ready outcome"),
        };
        assert!(!sweep.cancelled);
        assert_eq!(sweep.files_swept, 1);

        cancel.cancel_all();

        let path = module_path(root, "Сервер");
        let out = state.read(|resident, _| {
            let file_id = resident.file_id_for(&path).expect("path resolves to a resident FileId");
            resident.analysis().diagnostics(file_id, &DiagnosticsConfig::default()).len()
        });
        assert!(
            matches!(out, ResidentOutcome::Ready(_, _)),
            "the resident must keep serving per-file diagnostics after a late cancel"
        );
    }

    /// A symlink inside the config tree must not drop the common module's back-link.
    #[cfg(unix)]
    #[test]
    fn resident_substrate_backlinks_common_module_through_symlinked_dir() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let root = base.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let real = base.join("real");
        super::super::test_support::write_common_module(
            &real,
            "Сервер",
            true,
            "&НаСервере\nФункция Ч() Экспорт КонецФункции",
        );
        std::os::unix::fs::symlink(real.join("CommonModules"), root.join("CommonModules")).unwrap();

        let state = DiagnosticsState::for_workspace(root.clone());
        state.ensure_loading();
        wait_ready(&state);

        let module = root.join("CommonModules/Сервер/Ext/Module.bsl");
        let out = state.read(|resident, _| {
            let file_id = resident.file_id_for(&module).expect("module .bsl resolves to a FileId");
            resident.db.common_module_for_file_id(file_id).is_some()
        });
        match out {
            ResidentOutcome::Ready(resolved, _) => assert!(
                resolved,
                "back-link must resolve through a symlinked config subtree via the canonicalising fallback"
            ),
            _ => panic!("expected Ready outcome from a loaded db"),
        }
    }
}

#[cfg(test)]
mod trim_tests {
    use std::path::Path;

    use super::super::test_support::{module_path, wait_ready, write_common_module};
    use super::super::{DiagnosticsState, ResidentOutcome, SweepOptions};
    use super::DiagnosticsResident;
    use crate::cancel::RequestCancel;

    /// More modules than the sweep window of syntax trees (`parse_query`'s sweep cap is
    /// 64), so a sweep that does not trim leaves a count the assertion can see.
    const MODULES: usize = 100;

    fn workspace_with_modules(root: &Path) {
        for i in 0..MODULES {
            write_common_module(
                root,
                &format!("Модуль{i}"),
                true,
                "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции",
            );
        }
    }

    fn ready_state(root: &Path) -> DiagnosticsState {
        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);
        state
    }

    /// Live memos of the ingredient whose output type name contains `output`: salsa
    /// names an ingredient's row by its output type, not by the query.
    fn memos(resident: &DiagnosticsResident, output: &str) -> usize {
        resident
            .db()
            .memory_report()
            .into_iter()
            .filter(|(name, ..)| name.contains(output))
            .map(|(_, count, ..)| count)
            .sum()
    }

    /// Heap bytes the resident's syntax trees hold. Eviction drops a memo's value and
    /// keeps its slot, so the live count never moves; the trees' heap does.
    fn syntax_tree_bytes(resident: &DiagnosticsResident) -> usize {
        resident
            .db()
            .memory_report()
            .into_iter()
            .filter(|(name, ..)| name.contains("syntax::Parse<"))
            .map(|(.., heap)| heap.unwrap_or(0))
            .sum()
    }

    fn ready<T>(outcome: ResidentOutcome<T>) -> T {
        match outcome {
            ResidentOutcome::Ready(value, _) => value,
            _ => panic!("the resident is ready"),
        }
    }

    fn sweep_all() -> SweepOptions {
        SweepOptions { min_severity: ide::SeverityBucket::Hint, codes: Vec::new(), max_files: 1000 }
    }

    /// A workspace sweep leaves at most the sweep window of syntax trees resident:
    /// every swept module was parsed, and without the final deep trim all of them
    /// would still be memoised (the count would equal the module count).
    #[test]
    fn a_sweep_trims_the_swept_syntax_trees_down_to_the_sweep_window() {
        let dir = tempfile::tempdir().unwrap();
        workspace_with_modules(dir.path());
        let state = ready_state(dir.path());

        // The positive control: served one by one, every module's tree stays within
        // the interactive window, so all of them are resident before the sweep.
        for i in 0..MODULES {
            let config = ready(state.read(|resident, _| resident.config().clone()));
            let _ = state.read(|resident, _| {
                let file_id = resident.file_id_for(&module_path(dir.path(), &format!("Модуль{i}")));
                let analysis = resident.analysis();
                file_id.map(|file_id| analysis.diagnostics(file_id, &config).len())
            });
        }
        let before =
            ready(state.read(|resident, _| {
                (memos(resident, "syntax::Parse<"), syntax_tree_bytes(resident))
            }));
        assert_eq!(before.0, MODULES, "the control: every module was parsed");
        assert!(before.1 > 0, "the control: the trees hold heap");

        let (swept, after) = ready(state.read_mut(|resident, _| {
            let config = resident.config().clone();
            let sweep =
                resident.workspace_aggregates(&config, &sweep_all(), &RequestCancel::default());
            (sweep.files_swept, syntax_tree_bytes(resident))
        }));
        assert_eq!(swept, MODULES, "every module was swept");
        // The modules are identical, so the trees weigh the same: at most the sweep
        // window of 64 out of 100 trees is still held, within one tree of slack.
        assert!(
            after > 0 && after <= before.1 * 65 / 100,
            "a sweep keeps at most the sweep window of syntax trees: {after} of {} bytes",
            before.1
        );
    }

    /// The warm-up memoises both halves of the name indexes for every resident file,
    /// from a resident that held none of them.
    #[test]
    fn warm_name_indexes_memoises_both_halves_for_every_file() {
        let dir = tempfile::tempdir().unwrap();
        workspace_with_modules(dir.path());
        let state = ready_state(dir.path());

        let before = ready(state.read(|resident, _| {
            (memos(resident, "SymbolTree>"), memos(resident, "FileNameUsage>"))
        }));
        assert_eq!(before, (0, 0), "the control: a fresh resident holds neither index");

        let after = ready(state.read_mut(|resident, _| {
            resident.warm_name_indexes(&RequestCancel::default());
            (memos(resident, "SymbolTree>"), memos(resident, "FileNameUsage>"))
        }));
        assert_eq!(after, (MODULES, MODULES), "every file's symbol tree and name set");
    }

    /// Every served read trims: the syntax trees of files read earlier fall out once
    /// more than the interactive window of them has been read.
    #[test]
    fn every_read_trims_the_caches() {
        let dir = tempfile::tempdir().unwrap();
        workspace_with_modules(dir.path());
        let state = ready_state(dir.path());

        // The probe is a read of its own: it sees the trims of the reads BEFORE it,
        // and its own trim runs only after it returned.
        let trims = ready(state.read(|resident, _| resident.read_trims));
        assert_eq!(trims, 0, "a fresh resident has trimmed nothing before its first read");
        for _ in 0..3 {
            let _ = state.read(|_, _| ());
        }
        let trims = ready(state.read(|resident, _| resident.read_trims));
        assert_eq!(trims, 4, "one trim per read, after the read: the first probe and three more");
    }

    /// A sweep with nothing to sweep still returns: the author pass makes its db
    /// clone only where the pool consumes it, so the deep trim at the end finds no
    /// live handle. Run on its own thread so a regression fails the test instead of
    /// hanging the suite.
    #[test]
    fn an_empty_sweep_returns_through_its_final_trim() {
        let dir = tempfile::tempdir().unwrap();
        workspace_with_modules(dir.path());
        let state = std::sync::Arc::new(ready_state(dir.path()));

        let (done, finished) = std::sync::mpsc::channel();
        let sweeping = std::sync::Arc::clone(&state);
        std::thread::spawn(move || {
            let empty = SweepOptions {
                min_severity: ide::SeverityBucket::Hint,
                codes: Vec::new(),
                max_files: 0,
            };
            let out = sweeping.read_mut(|resident, _| {
                let config = resident.config().clone();
                resident
                    .workspace_aggregates(&config, &empty, &RequestCancel::default())
                    .files_swept
            });
            let _ = done.send(matches!(out, ResidentOutcome::Ready(0, _)));
        });
        match finished.recv_timeout(std::time::Duration::from_secs(120)) {
            Ok(returned) => assert!(returned, "the empty sweep reports zero files swept"),
            Err(_) => {
                panic!("the empty sweep did not return: its final trim waits on a live db handle")
            }
        }
    }
}
