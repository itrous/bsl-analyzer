use std::hash::BuildHasherDefault;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use rustc_hash::FxHasher;
use vfs::{FileId, VfsPath};

mod change;
mod input;
mod locale;
mod queries;
pub mod scope;

pub use change::FileChange;
pub use input::{
    content_revision, DiagnosticsConfigId, DiagnosticsConfigInput, FileIdInput, FileRevisionInput,
    FileSourceRootInput, FileTextInput, SourceRoot, SourceRootId, SourceRootInput, BSL_SOURCE_ROOT,
    METADATA_SOURCE_ROOT,
};
pub use locale::{Locale, UnknownLocale};
pub use queries::{
    decode_disk_bytes, file_text_query, file_text_unmemoized, method_regions_query, parse_query,
    read_disk_text, resolve_vfs_path_ci_query, resolve_vfs_path_query, set_parse_lru_sweep_mode,
};
pub use scope::AnalysisScope;

#[salsa::db]
pub trait SourceDatabase: salsa::Database {
    fn file_text_input(&self, file_id: FileId) -> FileTextInput;

    fn try_file_text_input(&self, file_id: FileId) -> Option<FileTextInput>;

    fn file_revision_input(&self, file_id: FileId) -> FileRevisionInput;

    fn try_file_revision_input(&self, file_id: FileId) -> Option<FileRevisionInput>;

    fn source_root_input(&self, source_root_id: SourceRootId) -> SourceRootInput;

    /// The same, for a caller that is ASKING whether a root is registered rather
    /// than asserting it. A database with no roots at all is an ordinary state —
    /// a tool answering from the platform alone before any workspace is built
    /// holds one — and such a caller must not have to risk the panic above to
    /// find out.
    fn try_source_root_input(&self, source_root_id: SourceRootId) -> Option<SourceRootInput>;

    fn file_source_root_input(&self, file_id: FileId) -> FileSourceRootInput;

    fn set_file_text(&mut self, file_id: FileId, text: &str);

    /// Register a file's content revision without storing its text; the text is
    /// read from disk on demand by [`file_text`](Self::file_text). See
    /// [`Files::set_file_revision_from_disk`].
    fn set_file_revision_from_disk(&mut self, file_id: FileId, revision: u64);

    /// Register a file that exists but whose bytes could not be read. See
    /// [`Files::set_file_unreadable`].
    fn set_file_unreadable(&mut self, file_id: FileId);

    /// Whether the file's empty text stands for ignorance rather than content. See
    /// [`Files::file_is_unread`] for what an unregistered file answers, and why.
    fn file_is_unread(&self, file_id: FileId) -> bool;

    /// The file's source text, as a version-keyed tracked query: returns the
    /// in-memory overlay when present, otherwise reads disk and verifies the
    /// bytes against the file's content revision. LRU-evictable.
    fn file_text(&self, file_id: FileId) -> Arc<str>;

    /// Borrowed variant of [`file_text`](Self::file_text) for read-only paths:
    /// no `Arc` refcount traffic per read.
    fn file_text_ref(&self, file_id: FileId) -> &Arc<str>;

    fn set_file_source_root(&mut self, file_id: FileId, source_root_id: SourceRootId);

    fn set_source_root(&mut self, source_root_id: SourceRootId, source_root: SourceRoot);

    fn resolve_vfs_path(&self, source_root_id: SourceRootId, vfs_path: &VfsPath) -> Option<FileId>;

    /// Предыдущий разбор файла, если он открыт. См. [`Files::parse_snapshot`].
    fn parse_snapshot(&self, file_id: FileId) -> Option<ParseSnapshot>;

    /// Запомнить разбор открытого файла как подсказку следующему. См.
    /// [`Files::store_parse_snapshot`].
    fn store_parse_snapshot(&self, file_id: FileId, snapshot: ParseSnapshot);

    /// Учесть исход одного исполнения `parse_query`.
    fn count_parse(&self, outcome: ParseOutcome);

    /// Счётчики исходов `parse_query` этой базы. См. [`ParseStats`].
    fn parse_stats(&self) -> ParseStats;
}

/// Предыдущий разбор открытого файла и текст, из которого он получен —
/// подсказка `parse_query`, чтобы правку внутри метода разобрать фрагментом.
/// На значение запроса не влияет, только на цену его получения.
#[derive(Debug, Clone)]
pub struct ParseSnapshot {
    pub text: Arc<str>,
    pub parse: syntax::Parse<syntax::SyntaxNode>,
}

/// Чем кончилось одно исполнение `parse_query`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Файл разобран целиком: снимка не было или тексты равны.
    Full,
    /// Фрагмент метода разобран и вклеен в старое дерево.
    Spliced,
    /// Снимок был, но гард отверг вклейку — файл разобран целиком.
    Refused(parser::reparse::Refusal),
    /// Проверка под `BSL_REPARSE_VERIFY` нашла расхождение вклейки с полным
    /// разбором; в значение ушёл полный разбор.
    Mismatched,
}

/// Счётчики исходов `parse_query` — на базу, а не на процесс: тесты одного
/// бинаря идут параллельно и делить счётчик не могут.
#[derive(Debug, Default)]
struct ParseCounters {
    full: AtomicU64,
    spliced: AtomicU64,
    mismatched: AtomicU64,
    refused: [AtomicU64; parser::reparse::Refusal::ALL.len()],
}

/// Снимок [`ParseCounters`]; `refused` индексируется
/// [`Refusal::index`](parser::reparse::Refusal::index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParseStats {
    pub full: u64,
    pub spliced: u64,
    pub mismatched: u64,
    pub refused: [u64; parser::reparse::Refusal::ALL.len()],
}

#[salsa::db]
pub trait RootQueryDb: SourceDatabase {
    fn parse(&self, file_id: FileId) -> syntax::Parse<syntax::SyntaxNode>;

    /// Borrow the parsed tree directly from the memo, skipping the `Vec<SyntaxError>`
    /// deep clone that [`parse`](Self::parse) pays on every read. Prefer this on hot
    /// per-file paths that only read the syntax node or errors; [`parse`](Self::parse)
    /// stays for callers that must own the `Parse`. The borrow is tied to `&self`, and
    /// Salsa only evicts a memo under `&mut`, so it cannot dangle.
    fn parse_ref(&self, file_id: FileId) -> &syntax::Parse<syntax::SyntaxNode>;

    fn method_regions(
        &self,
        file_id: FileId,
    ) -> Arc<std::collections::HashMap<syntax::TextRange, String>>;
}

#[derive(Debug, Default, Clone)]
pub struct Files {
    file_texts: Arc<DashMap<FileId, FileTextInput, BuildHasherDefault<FxHasher>>>,
    file_revisions: Arc<DashMap<FileId, FileRevisionInput, BuildHasherDefault<FxHasher>>>,
    source_roots: Arc<DashMap<SourceRootId, SourceRootInput, BuildHasherDefault<FxHasher>>>,
    file_source_roots: Arc<DashMap<FileId, FileSourceRootInput, BuildHasherDefault<FxHasher>>>,
    parse_snapshots: Arc<DashMap<FileId, ParseSnapshot, BuildHasherDefault<FxHasher>>>,
    parse_counters: Arc<ParseCounters>,
}

impl Files {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn file_text(&self, file_id: FileId) -> FileTextInput {
        self.file_texts.get(&file_id).map(|entry| *entry.value()).unwrap_or_else(|| {
            tracing::error!(?file_id, "file text not set — this is a programming error, all files must be loaded before queries run");
            panic!("file text not set for {:?}", file_id)
        })
    }

    pub fn try_file_text(&self, file_id: FileId) -> Option<FileTextInput> {
        self.file_texts.get(&file_id).map(|entry| *entry.value())
    }

    /// Предыдущий разбор файла. Есть только у файла с оверлеем: снимок
    /// пришпиливает дерево и текст, и для каждого разобранного файла с диска
    /// это была бы вторая копия рабочего множества.
    pub fn parse_snapshot(&self, file_id: FileId) -> Option<ParseSnapshot> {
        self.parse_snapshots.get(&file_id).map(|entry| entry.value().clone())
    }

    pub fn store_parse_snapshot(&self, file_id: FileId, snapshot: ParseSnapshot) {
        self.parse_snapshots.insert(file_id, snapshot);
    }

    pub fn count_parse(&self, outcome: ParseOutcome) {
        let counters = &self.parse_counters;
        let cell = match outcome {
            ParseOutcome::Full => &counters.full,
            ParseOutcome::Spliced => &counters.spliced,
            ParseOutcome::Mismatched => &counters.mismatched,
            ParseOutcome::Refused(refusal) => &counters.refused[refusal.index()],
        };
        cell.fetch_add(1, Ordering::Relaxed);
    }

    pub fn parse_stats(&self) -> ParseStats {
        let counters = &self.parse_counters;
        let mut refused = [0u64; parser::reparse::Refusal::ALL.len()];
        for (slot, cell) in refused.iter_mut().zip(&counters.refused) {
            *slot = cell.load(Ordering::Relaxed);
        }
        ParseStats {
            full: counters.full.load(Ordering::Relaxed),
            spliced: counters.spliced.load(Ordering::Relaxed),
            mismatched: counters.mismatched.load(Ordering::Relaxed),
            refused,
        }
    }

    pub fn set_file_text(&self, db: &mut dyn SourceDatabase, file_id: FileId, text: &str) {
        self.set_file_text_marked(db, file_id, text, salsa::Durability::LOW, false);
    }

    pub fn set_file_text_with_durability(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: FileId,
        text: &str,
        durability: salsa::Durability,
    ) {
        self.set_file_text_marked(db, file_id, text, durability, false);
    }

    /// Pin `text` as this file's resident overlay, recording whether the text stands
    /// for the file's real content or for the fact that it could not be read.
    fn set_file_text_marked(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: FileId,
        text: &str,
        durability: salsa::Durability,
        unreadable: bool,
    ) {
        use salsa::Setter;

        let existing = self.file_texts.get(&file_id).map(|e| *e.value());
        match existing {
            Some(input) => {
                input.set_text(db).with_durability(durability).to(text.to_string());
            }
            None => {
                let input = FileTextInput::builder(text.to_string()).durability(durability).new(db);
                let previous = self.file_texts.insert(file_id, input);
                debug_assert!(
                    previous.is_none(),
                    "concurrent set_file_text violates single-mutator invariant"
                );
            }
        }
        // Set the revision in the SAME exclusive `&mut db` op so a snapshot never
        // observes overlay-and-revision out of step. The revision is the
        // invalidation trigger for `file_text_query` and the token a later disk
        // re-read (when this file is closed) must match.
        self.set_file_revision_with_durability(
            db,
            file_id,
            input::content_revision(text),
            durability,
            unreadable,
        );
    }

    /// The content-revision input handle for a file (panics if neither
    /// [`set_file_text`](Self::set_file_text) nor
    /// [`set_file_revision_from_disk`](Self::set_file_revision_from_disk) ran for it).
    pub fn file_revision(&self, file_id: FileId) -> FileRevisionInput {
        self.file_revisions.get(&file_id).map(|entry| *entry.value()).unwrap_or_else(|| {
            tracing::error!(?file_id, "file revision not set — this is a programming error, all files must be registered before queries run");
            panic!("file revision not set for {:?}", file_id)
        })
    }

    pub fn try_file_revision(&self, file_id: FileId) -> Option<FileRevisionInput> {
        self.file_revisions.get(&file_id).map(|entry| *entry.value())
    }

    pub fn try_source_root(&self, source_root_id: SourceRootId) -> Option<SourceRootInput> {
        self.source_roots.get(&source_root_id).map(|entry| *entry.value())
    }

    /// Register a file's content revision WITHOUT storing its text (the
    /// disk-backed path): `file_text_query` will read the file from disk on
    /// demand and verify the bytes hash to this revision. Used by batch analysis
    /// and for closed LSP files to keep them evictable instead of resident.
    ///
    /// Drops any existing in-memory overlay for the file in the SAME exclusive
    /// update so `file_text_query` (which prefers the overlay) actually falls
    /// through to the disk read. Without this, a once-open file's stale overlay
    /// would be hash-checked against the new disk revision and panic.
    pub fn set_file_revision_from_disk(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: FileId,
        revision: u64,
    ) {
        self.file_texts.remove(&file_id);
        self.parse_snapshots.remove(&file_id);
        let durability = self.durability_for_file(db, file_id).unwrap_or(salsa::Durability::LOW);
        self.set_file_revision_with_durability(db, file_id, revision, durability, false);
    }

    /// The durability of a file's inputs, decided by its source root (see
    /// [`SourceRoot::durability`]); `None` when the file has no root mapping yet.
    fn durability_for_file(
        &self,
        db: &dyn SourceDatabase,
        file_id: FileId,
    ) -> Option<salsa::Durability> {
        let mapping = self.file_source_roots.get(&file_id).map(|e| *e.value())?;
        let source_root_id = mapping.source_root_id(db);
        let root_input = self.source_roots.get(&source_root_id).map(|e| *e.value())?;
        Some(root_input.root(db).durability())
    }

    /// The single sink every text/revision registration funnels through, and so the
    /// only place `unreadable` is written. Taking it as a parameter here — rather
    /// than listing the writers and setting it in each — means a new writer cannot
    /// forget it: the compiler asks. The list is easy to get wrong, and was: the
    /// only production `SourceDatabase` routes `set_file_text` through
    /// `set_file_text_smart`, not through `Files::set_file_text`.
    fn set_file_revision_with_durability(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: FileId,
        revision: u64,
        durability: salsa::Durability,
        unreadable: bool,
    ) {
        use salsa::Setter;

        let existing = self.file_revisions.get(&file_id).map(|e| *e.value());
        match existing {
            Some(input) => {
                input.set_revision(db).with_durability(durability).to(revision);
                // An input write is a change to salsa whatever the value, and a
                // memo that read the flag directly — cross-module resolution asks
                // it for every candidate body — would be re-run by every edit of
                // that file. Write it only when the answer actually flips.
                if input.unreadable(db) != unreadable {
                    input.set_unreadable(db).with_durability(durability).to(unreadable);
                }
            }
            None => {
                let input =
                    FileRevisionInput::builder(revision, unreadable).durability(durability).new(db);
                let previous = self.file_revisions.insert(file_id, input);
                debug_assert!(
                    previous.is_none(),
                    "concurrent set_file_revision violates single-mutator invariant"
                );
            }
        }
    }

    /// Register a file whose bytes could not be read: an empty text input (so a
    /// later query yields `""` instead of panicking on a disk re-read) plus the
    /// mark that says the emptiness is ignorance, not content.
    pub fn set_file_unreadable(&self, db: &mut dyn SourceDatabase, file_id: FileId) {
        let durability = self.durability_for_file(db, file_id).unwrap_or(salsa::Durability::LOW);
        self.set_file_text_marked(db, file_id, "", durability, true);
    }

    /// Whether `file_id` is a file that exists but could not be read. A file with
    /// no revision input at all answers `false`: the batch databases of the graph
    /// build register inputs only for their own batch, yet resolve call targets
    /// through a module index built over the whole source root, so an unregistered
    /// candidate is routine there. Panicking would turn every cross-batch call into
    /// a crash; `false` keeps the pre-existing behaviour for a file we know nothing
    /// about.
    pub fn file_is_unread(&self, db: &dyn SourceDatabase, file_id: FileId) -> bool {
        self.file_revisions.get(&file_id).map(|e| e.value().unreadable(db)).unwrap_or(false)
    }

    pub fn set_file_text_smart(&self, db: &mut dyn SourceDatabase, file_id: FileId, text: &str) {
        let durability = self.durability_for_file(db, file_id);

        match durability {
            Some(d) => {
                tracing::debug!(
                    ?file_id,
                    durability = ?d,
                    "set_file_text_smart: determined durability from source root"
                );
                self.set_file_text_with_durability(db, file_id, text, d);
            }
            None => {
                tracing::debug!(
                    ?file_id,
                    "set_file_text_smart: fallback to LOW durability (source root not set)"
                );
                self.set_file_text_with_durability(db, file_id, text, salsa::Durability::LOW);
            }
        }
    }

    pub fn source_root(&self, source_root_id: SourceRootId) -> SourceRootInput {
        self.source_roots.get(&source_root_id).map(|entry| *entry.value()).unwrap_or_else(|| {
            tracing::error!(?source_root_id, "source root not set — this is a programming error");
            panic!("source root not set for {:?}", source_root_id)
        })
    }

    /// The `SourceRootInput` carries the root's own durability: `file_text_query`'s
    /// disk branch reads it (path resolution), so a LOW root input would floor every
    /// disk-backed text memo under the root at LOW regardless of the file inputs.
    pub fn set_source_root(
        &self,
        db: &mut dyn SourceDatabase,
        source_root_id: SourceRootId,
        source_root: SourceRoot,
    ) {
        use salsa::Setter;

        let durability = source_root.durability();
        let existing = self.source_roots.get(&source_root_id).map(|e| *e.value());
        match existing {
            Some(input) => {
                input.set_root(db).with_durability(durability).to(source_root);
            }
            None => {
                let input = SourceRootInput::builder(source_root).durability(durability).new(db);
                let previous = self.source_roots.insert(source_root_id, input);
                debug_assert!(
                    previous.is_none(),
                    "concurrent set_source_root violates single-mutator invariant"
                );
            }
        }
    }

    pub fn file_source_root(&self, file_id: FileId) -> FileSourceRootInput {
        self.file_source_roots.get(&file_id).map(|entry| *entry.value()).unwrap_or_else(|| {
            tracing::error!(?file_id, "file source root not set — this is a programming error");
            panic!("file source root not set for {:?}", file_id)
        })
    }

    /// Whether an explicit file→root registration exists, without weakening the
    /// strict [`Self::file_source_root`] accessor used by source and text queries.
    pub fn try_file_source_root(&self, file_id: FileId) -> Option<FileSourceRootInput> {
        self.file_source_roots.get(&file_id).map(|entry| *entry.value())
    }

    /// The file→root mapping inherits the target root's durability (the root input
    /// must already be registered; an unknown root falls back to LOW). Same reason
    /// as [`Self::set_source_root`]: `file_text_query`'s disk branch reads the
    /// mapping, so it must not undercut the root's durability floor.
    pub fn set_file_source_root(
        &self,
        db: &mut dyn SourceDatabase,
        file_id: FileId,
        source_root_id: SourceRootId,
    ) {
        use salsa::Setter;

        let durability = self
            .source_roots
            .get(&source_root_id)
            .map(|e| *e.value())
            .map(|input| input.root(db).durability())
            .unwrap_or(salsa::Durability::LOW);
        let existing = self.file_source_roots.get(&file_id).map(|e| *e.value());
        match existing {
            // The host re-registers a file's root on every change to it, and an
            // input write is a change to salsa whatever the value: every memo
            // that read the file's root directly — path resolution under
            // inference does — would re-run per edit. Write only what changed.
            Some(input) if input.source_root_id(db) == source_root_id => {}
            Some(input) => {
                input.set_source_root_id(db).with_durability(durability).to(source_root_id);
            }
            None => {
                let input =
                    FileSourceRootInput::builder(source_root_id).durability(durability).new(db);
                let previous = self.file_source_roots.insert(file_id, input);
                debug_assert!(
                    previous.is_none(),
                    "concurrent set_file_source_root violates single-mutator invariant"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::file_set::FileSet;
    use vfs::VfsPath;

    #[salsa::db]
    #[derive(Clone)]
    struct TestDatabase {
        storage: salsa::Storage<Self>,
        files: Files,
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Default for TestDatabase {
        fn default() -> Self {
            let events: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
            Self {
                storage: salsa::Storage::new(Some(Box::new({
                    let events = events.clone();
                    move |event| events.lock().unwrap().push(format!("{:?}", event.kind))
                }))),
                files: Files::default(),
                events,
            }
        }
    }

    impl TestDatabase {
        fn take_events(&self) -> Vec<String> {
            std::mem::take(&mut self.events.lock().unwrap())
        }
    }

    #[salsa::db]
    impl salsa::Database for TestDatabase {}

    #[salsa::db]
    impl SourceDatabase for TestDatabase {
        fn file_text_input(&self, file_id: FileId) -> FileTextInput {
            self.files.file_text(file_id)
        }

        fn try_file_text_input(&self, file_id: FileId) -> Option<FileTextInput> {
            self.files.try_file_text(file_id)
        }

        fn file_revision_input(&self, file_id: FileId) -> FileRevisionInput {
            self.files.file_revision(file_id)
        }

        fn try_file_revision_input(&self, file_id: FileId) -> Option<FileRevisionInput> {
            self.files.try_file_revision(file_id)
        }

        fn file_text(&self, file_id: FileId) -> Arc<str> {
            self.file_text_ref(file_id).clone()
        }

        fn file_text_ref(&self, file_id: FileId) -> &Arc<str> {
            let input = FileIdInput::new(self, file_id);
            file_text_query(self, input)
        }

        fn set_file_revision_from_disk(&mut self, file_id: FileId, revision: u64) {
            let files = self.files.clone();
            files.set_file_revision_from_disk(self, file_id, revision);
        }

        fn set_file_unreadable(&mut self, file_id: FileId) {
            let files = self.files.clone();
            files.set_file_unreadable(self, file_id);
        }

        fn file_is_unread(&self, file_id: FileId) -> bool {
            self.files.file_is_unread(self, file_id)
        }

        fn parse_snapshot(&self, file_id: FileId) -> Option<ParseSnapshot> {
            self.files.parse_snapshot(file_id)
        }

        fn store_parse_snapshot(&self, file_id: FileId, snapshot: ParseSnapshot) {
            self.files.store_parse_snapshot(file_id, snapshot);
        }

        fn count_parse(&self, outcome: ParseOutcome) {
            self.files.count_parse(outcome);
        }

        fn parse_stats(&self) -> ParseStats {
            self.files.parse_stats()
        }

        fn source_root_input(&self, source_root_id: SourceRootId) -> SourceRootInput {
            self.files.source_root(source_root_id)
        }

        fn try_source_root_input(&self, source_root_id: SourceRootId) -> Option<SourceRootInput> {
            self.files.try_source_root(source_root_id)
        }

        fn file_source_root_input(&self, file_id: FileId) -> FileSourceRootInput {
            self.files.file_source_root(file_id)
        }

        fn set_file_text(&mut self, file_id: FileId, text: &str) {
            let files = self.files.clone();
            files.set_file_text(self, file_id, text);
        }

        fn set_file_source_root(&mut self, file_id: FileId, source_root_id: SourceRootId) {
            let files = self.files.clone();
            files.set_file_source_root(self, file_id, source_root_id);
        }

        fn set_source_root(&mut self, source_root_id: SourceRootId, source_root: SourceRoot) {
            let files = self.files.clone();
            files.set_source_root(self, source_root_id, source_root);
        }

        fn resolve_vfs_path(
            &self,
            source_root_id: SourceRootId,
            vfs_path: &VfsPath,
        ) -> Option<FileId> {
            let source_root_input = self.source_root_input(source_root_id);
            let vfs_path_str = vfs_path.as_path().to_string_lossy().to_string();
            resolve_vfs_path_query(self, source_root_input, vfs_path_str)
        }
    }

    #[salsa::db]
    impl RootQueryDb for TestDatabase {
        fn parse(&self, file_id: FileId) -> syntax::Parse<syntax::SyntaxNode> {
            self.parse_ref(file_id).clone()
        }

        fn parse_ref(&self, file_id: FileId) -> &syntax::Parse<syntax::SyntaxNode> {
            let input = FileIdInput::new(self, file_id);
            parse_query(self, input)
        }

        fn method_regions(
            &self,
            file_id: FileId,
        ) -> Arc<std::collections::HashMap<syntax::TextRange, String>> {
            let input = FileIdInput::new(self, file_id);
            method_regions_query(self, input)
        }
    }

    #[salsa::tracked(lru = 10, returns(copy))]
    fn test_fileid_query<'db>(
        db: &'db dyn salsa::Database,
        file_id_input: FileIdInput<'db>,
    ) -> u32 {
        file_id_input.file_id(db).0
    }

    static UNREAD_PROBE_RUNS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// A memo whose only dependency is one file's unreadability, so re-execution
    /// counts as the observable for invalidation.
    #[salsa::tracked(returns(copy))]
    fn unread_probe_query<'db>(
        db: &'db dyn SourceDatabase,
        file_id_input: FileIdInput<'db>,
    ) -> bool {
        UNREAD_PROBE_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        db.file_is_unread(file_id_input.file_id(db))
    }

    fn register_disk_backed(db: &mut TestDatabase, file_id: FileId, text: &str) {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_revision_from_disk(file_id, input::content_revision(text));
    }

    #[test]
    fn a_registered_file_is_readable_until_marked_and_readable_again_once_reread() {
        let mut db = TestDatabase::default();
        let overlaid = FileId(0);
        let disk_backed = FileId(1);

        let mut file_set = FileSet::new();
        file_set.insert(overlaid, VfsPath::new("/Overlaid.bsl"));
        file_set.insert(disk_backed, VfsPath::new("/DiskBacked.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));

        db.set_file_source_root(overlaid, SourceRootId(0));
        db.set_file_text(overlaid, "Процедура П() КонецПроцедуры");
        register_disk_backed(&mut db, disk_backed, "Процедура П() КонецПроцедуры");

        assert!(!db.file_is_unread(overlaid), "an overlay is content, not ignorance");
        assert!(!db.file_is_unread(disk_backed), "a disk-backed revision is content too");

        db.set_file_unreadable(overlaid);
        db.set_file_unreadable(disk_backed);
        assert!(db.file_is_unread(overlaid));
        assert!(db.file_is_unread(disk_backed));

        // Healing goes through the ordinary registration, so it must clear the mark
        // without a second call — otherwise a file that came back stays mute forever.
        db.set_file_text(overlaid, "Процедура П() КонецПроцедуры");
        register_disk_backed(&mut db, disk_backed, "Процедура П() КонецПроцедуры");
        assert!(!db.file_is_unread(overlaid), "re-registration clears the mark");
        assert!(!db.file_is_unread(disk_backed), "re-registration clears the mark");
    }

    #[test]
    fn an_empty_file_is_not_an_unread_one() {
        let mut db = TestDatabase::default();
        let file_id = FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/Empty.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, "");
        assert_eq!(&*db.file_text(file_id), "");
        assert!(!db.file_is_unread(file_id), "an honestly empty module is readable");

        db.set_file_unreadable(file_id);
        assert_eq!(&*db.file_text(file_id), "", "a hole still answers with empty text");
        assert!(db.file_is_unread(file_id), "and only the mark tells the two apart");
    }

    #[test]
    fn a_memo_reading_unreadability_is_recomputed_only_for_its_own_file() {
        let mut db = TestDatabase::default();
        let watched = FileId(0);
        let other = FileId(1);

        let mut file_set = FileSet::new();
        file_set.insert(watched, VfsPath::new("/Watched.bsl"));
        file_set.insert(other, VfsPath::new("/Other.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(watched, SourceRootId(0));
        db.set_file_source_root(other, SourceRootId(0));
        db.set_file_text(watched, "Процедура П() КонецПроцедуры");
        db.set_file_text(other, "Процедура П() КонецПроцедуры");

        let probe = |db: &TestDatabase| {
            let input = FileIdInput::new(db, watched);
            unread_probe_query(db, input)
        };

        UNREAD_PROBE_RUNS.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(!probe(&db));
        assert!(!probe(&db));
        assert_eq!(UNREAD_PROBE_RUNS.load(std::sync::atomic::Ordering::SeqCst), 1, "memoised");

        db.set_file_unreadable(watched);
        assert!(probe(&db), "the memo follows its own file");
        assert_eq!(UNREAD_PROBE_RUNS.load(std::sync::atomic::Ordering::SeqCst), 2);

        // Positive control: without it the test would pass on an implementation that
        // recomputes on every write anywhere, proving nothing about the dependency.
        db.set_file_unreadable(other);
        assert!(probe(&db));
        assert_eq!(
            UNREAD_PROBE_RUNS.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "another file's mark is not this memo's business"
        );
    }

    /// A text edit re-registers the file, and re-registration carries the
    /// unreadable flag. Writing the flag when it did not change would re-run
    /// every memo that read it — cross-module resolution reads it for each
    /// candidate body — on every edit of that file.
    // Its own probe and counter: the tests run in parallel, and a counter shared
    // with `a_memo_reading_unreadability_is_recomputed_only_for_its_own_file`
    // would count that test's runs here.
    static EDIT_PROBE_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[salsa::tracked(returns(copy))]
    fn edit_probe_query<'db>(db: &'db dyn SourceDatabase, file_id_input: FileIdInput<'db>) -> bool {
        EDIT_PROBE_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        db.file_is_unread(file_id_input.file_id(db))
    }

    #[test]
    fn a_text_edit_does_not_touch_the_unreadable_mark() {
        let mut db = TestDatabase::default();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/Edited.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, "Процедура П() КонецПроцедуры");

        let probe = |db: &TestDatabase| edit_probe_query(db, FileIdInput::new(db, file_id));
        EDIT_PROBE_RUNS.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(!probe(&db));

        db.set_file_text(file_id, "Процедура П() Х = 1; КонецПроцедуры");
        assert!(!probe(&db));
        assert_eq!(
            EDIT_PROBE_RUNS.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an edit that leaves the file readable must not re-run the reader"
        );

        // Positive control: the mark itself still moves the memo.
        db.set_file_unreadable(file_id);
        assert!(probe(&db));
        assert_eq!(EDIT_PROBE_RUNS.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    static ROOT_PROBE_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[salsa::tracked(returns(copy))]
    fn root_probe_query<'db>(db: &'db dyn SourceDatabase, file_id_input: FileIdInput<'db>) -> u32 {
        ROOT_PROBE_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        db.file_source_root_input(file_id_input.file_id(db)).source_root_id(db).0
    }

    /// The host re-registers a changed file's root on every change. Path
    /// resolution under inference reads the root input directly, so a write
    /// that changes nothing would re-run inference for every method that
    /// resolved a path.
    #[test]
    fn re_registering_the_same_source_root_is_not_a_write() {
        let mut db = TestDatabase::default();
        let file_id = FileId(0);
        let mut set0 = FileSet::new();
        set0.insert(file_id, VfsPath::new("/Same.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(set0));
        db.set_source_root(SourceRootId(1), SourceRoot::new_local(FileSet::new()));
        db.set_file_source_root(file_id, SourceRootId(0));

        let probe = |db: &TestDatabase| root_probe_query(db, FileIdInput::new(db, file_id));
        ROOT_PROBE_RUNS.store(0, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(probe(&db), 0);

        db.set_file_source_root(file_id, SourceRootId(0));
        assert_eq!(probe(&db), 0);
        assert_eq!(
            ROOT_PROBE_RUNS.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the same root again must leave the reader's memo standing"
        );

        // Positive control: a real move re-runs the reader.
        db.set_file_source_root(file_id, SourceRootId(1));
        assert_eq!(probe(&db), 1);
        assert_eq!(ROOT_PROBE_RUNS.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn an_unregistered_file_answers_readable_where_asking_its_revision_panics() {
        let db = TestDatabase::default();
        let stranger = FileId(7);

        assert!(!db.file_is_unread(stranger), "nothing known means nothing claimed");

        // Positive control: the same id through the revision input DOES panic, so the
        // assertion above is about the contract and not about an id that is somehow
        // registered anyway.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let asked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.file_revision_input(stranger)
        }));
        std::panic::set_hook(previous);
        assert!(asked.is_err(), "an unregistered file has no revision input at all");
    }

    #[test]
    fn test_fileid_salsa_compatible() {
        let db = TestDatabase::default();

        let file_id = FileId(42);
        let file_id_input = FileIdInput::new(&db, file_id);

        let result = test_fileid_query(&db, file_id_input);
        assert_eq!(result, 42);

        let result2 = test_fileid_query(&db, file_id_input);
        assert_eq!(result2, 42);

        let file_id2 = FileId(100);
        let file_id_input2 = FileIdInput::new(&db, file_id2);
        let result3 = test_fileid_query(&db, file_id_input2);
        assert_eq!(result3, 100);

        let file_id_input3 = FileIdInput::new(&db, file_id);
        assert_eq!(file_id_input, file_id_input3);
    }

    #[test]
    fn test_parse_query() {
        let mut db = TestDatabase::default();
        let file_id = FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");

        let result = db.parse(file_id);
        assert!(!result.has_errors());
    }

    #[test]
    fn test_incremental_reparse() {
        let mut db = TestDatabase::default();
        let file_id = FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");
        let parse1 = db.parse(file_id);
        assert!(!parse1.has_errors());

        let parse2 = db.parse(file_id);
        assert!(!parse2.has_errors());
        assert_eq!(parse1.syntax_node().text(), parse2.syntax_node().text());

        db.set_file_text(file_id, "Процедура Тест2() КонецПроцедуры");
        let parse3 = db.parse(file_id);
        assert!(!parse3.has_errors());
        assert_ne!(parse1.syntax_node().text(), parse3.syntax_node().text());
    }

    #[test]
    fn test_file_change_apply() {
        let mut db = TestDatabase::default();
        let file_id = FileId(0);

        let mut change = FileChange::new();
        change.change_file(file_id, Some(Arc::from("Процедура Тест() КонецПроцедуры")));

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        change.set_roots(vec![source_root]);

        change.apply(&mut db);

        let result = db.parse(file_id);
        assert!(!result.has_errors());
    }

    #[test]
    fn read_disk_text_preserves_bom_verbatim() {
        let dir = std::env::temp_dir().join(format!("bsl_rdt_bom_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bom.bsl");
        // A leading BOM plus body; read_disk_text must not strip it, so the
        // revision computed here matches what file_text_query recomputes on read.
        let raw = "\u{FEFF}Процедура Т() КонецПроцедуры";
        std::fs::write(&path, raw).unwrap();

        let got = queries::read_disk_text(&path).unwrap();
        assert_eq!(got, raw);
        assert_eq!(input::content_revision(&got), input::content_revision(raw));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn decode_disk_bytes_matches_read_disk_text_verbatim() {
        // The VFS loader decodes watcher bytes via `decode_disk_bytes`; its output
        // must hash identically to `read_disk_text`'s disk re-read, or a BOM-led
        // file's recorded revision (from the loader) diverges from the on-read hash
        // and `file_text_query` trips `assert_revision`. 1C BSL files are saved with
        // a UTF-8 BOM, so the BOM must survive both paths.
        let raw = "\u{FEFF}Процедура Т() КонецПроцедуры";
        let decoded = queries::decode_disk_bytes(raw.as_bytes()).unwrap();
        assert_eq!(decoded, raw);
        assert_eq!(input::content_revision(&decoded), input::content_revision(raw));
    }

    #[test]
    fn content_revision_folds_in_length() {
        assert_eq!(input::content_revision("abc"), input::content_revision("abc"));
        assert_ne!(input::content_revision("ab"), input::content_revision("ba"));
        // length is folded in so a prefix is not aliased with the longer text
        assert_ne!(input::content_revision("a"), input::content_revision("aa"));
    }

    #[test]
    fn file_text_query_returns_overlay() {
        let mut db = TestDatabase::default();
        let file_id = FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/ov.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");
        assert_eq!(&*db.file_text(file_id), "Процедура Тест() КонецПроцедуры");
    }

    #[test]
    fn file_text_query_reads_disk_without_overlay() {
        let dir = std::env::temp_dir().join(format!("bsl_ft_disk_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.bsl");
        let content = "Функция Ф() Возврат 1; КонецФункции";
        std::fs::write(&path, content).unwrap();

        let mut db = TestDatabase::default();
        let file_id = FileId(7);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new(path.clone()));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        // disk-backed: no overlay text, only the content revision
        db.set_file_revision_from_disk(file_id, input::content_revision(content));

        assert_eq!(&*db.file_text(file_id), content);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A memo whose whole input cone lives under the metadata root must survive a
    /// LOW-durability write with a shallow O(1) verification — no walk into its
    /// dependencies — while the same memo shape under a local root deep-verifies.
    /// Deep verification is observable as a `DidValidateMemoizedValue` event for
    /// the nested `file_text_query` memo when `parse_query` is re-validated.
    #[test]
    fn metadata_root_cone_shallow_verifies_after_low_write() {
        use salsa::Database;

        let dir = std::env::temp_dir().join(format!("bsl_durability_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let xml_path = dir.join("Товары.xml");
        let bsl_path = dir.join("Модуль.bsl");
        let xml_content = "<MetaDataObject/>";
        let bsl_content = "Процедура А() КонецПроцедуры";
        std::fs::write(&xml_path, xml_content).unwrap();
        std::fs::write(&bsl_path, bsl_content).unwrap();

        let mut db = TestDatabase::default();
        let xml_file = FileId(0);
        let bsl_file = FileId(1);

        let mut metadata_set = FileSet::new();
        metadata_set.insert(xml_file, VfsPath::new(xml_path.clone()));
        db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(metadata_set));
        let mut bsl_set = FileSet::new();
        bsl_set.insert(bsl_file, VfsPath::new(bsl_path.clone()));
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(bsl_set));

        db.set_file_source_root(xml_file, METADATA_SOURCE_ROOT);
        db.set_file_source_root(bsl_file, BSL_SOURCE_ROOT);
        db.set_file_revision_from_disk(xml_file, input::content_revision(xml_content));
        db.set_file_revision_from_disk(bsl_file, input::content_revision(bsl_content));

        let _ = db.parse(xml_file);
        let _ = db.parse(bsl_file);

        db.synthetic_write(salsa::Durability::LOW);

        db.take_events();
        let _ = db.parse(xml_file);
        let xml_events = db.take_events();
        assert!(
            !xml_events.iter().any(|e| {
                e.contains("DidValidateMemoizedValue") && e.contains("file_text_query")
            }),
            "metadata-rooted parse must shallow-verify after a LOW write, \
             but its file_text dependency was walked: {xml_events:#?}"
        );

        db.take_events();
        let _ = db.parse(bsl_file);
        let bsl_events = db.take_events();
        assert!(
            bsl_events.iter().any(|e| {
                e.contains("DidValidateMemoizedValue") && e.contains("file_text_query")
            }),
            "local-rooted parse is LOW and must deep-verify its file_text dependency \
             after a LOW write: {bsl_events:#?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[should_panic(expected = "revision mismatch")]
    fn file_text_query_panics_on_disk_drift() {
        let dir = std::env::temp_dir().join(format!("bsl_ft_drift_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("drift.bsl");
        std::fs::write(&path, "actual on-disk bytes").unwrap();

        let mut db = TestDatabase::default();
        let file_id = FileId(3);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new(path));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        // revision computed from DIFFERENT content than what is on disk → drift
        db.set_file_revision_from_disk(file_id, input::content_revision("a stale snapshot"));

        let _ = db.file_text(file_id);
    }
}
