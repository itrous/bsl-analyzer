//! Shared disk ownership and independent editor storages.

use std::collections::{HashMap, VecDeque};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base_db::{SourceDatabase, SourceRoot, SourceRootId, BSL_SOURCE_ROOT};
use ide::{Analysis, RootDatabaseImpl};
use ide_db::metadata::{MetadataListingSnapshot, WorkspaceConfigsSnapshot};
use vfs::{file_set::FileSet, FileId, Vfs, VfsPath};

use crate::{apply_source_batch, AnalysisHost, BatchTextSource, SourceBatchChange};

const JOURNAL_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskFileState {
    Readable { revision: u64 },
    Unreadable,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogFile {
    pub file_id: FileId,
    pub path: PathBuf,
    pub state: DiskFileState,
}

#[derive(Clone)]
pub struct SnapshotSourceRoot {
    pub id: SourceRootId,
    pub root: SourceRoot,
}

#[derive(Clone)]
pub struct SnapshotFile {
    pub file_id: FileId,
    pub path: PathBuf,
    pub root_id: SourceRootId,
    pub state: DiskFileState,
}

#[derive(Clone)]
pub struct WorkspaceSnapshot {
    pub generation: u64,
    pub metadata_ready: bool,
    pub configs: WorkspaceConfigsSnapshot,
    pub source_roots: Vec<SnapshotSourceRoot>,
    pub files: Vec<SnapshotFile>,
    pub catalog: Vec<CatalogFile>,
    pub metadata: Vec<(PathBuf, MetadataListingSnapshot)>,
}

struct CatalogState {
    vfs: Vfs,
    files: HashMap<PathBuf, CatalogFile>,
    /// Shared-VFS ids below this value each have a listing in `files`.
    listed: u32,
}

impl CatalogState {
    fn new(vfs: Vfs) -> Self {
        let mut state = Self { vfs, files: HashMap::new(), listed: 0 };
        state.list_allocated();
        state
    }

    fn alloc(&mut self, path: PathBuf) -> FileId {
        let id = self.vfs.alloc_file_id(VfsPath::new(path));
        self.list_allocated();
        id
    }

    /// Every id the shared VFS hands out is listed under its path at once. A consumer that
    /// mirrors the ids walks the listing in id order, so a VFS id without a listing is a gap
    /// it cannot reproduce. A new listing stays `Deleted` until a publication reconciles it,
    /// so an interned but unpublished path is not served.
    fn list_allocated(&mut self) {
        while self.listed < self.vfs.num_file_ids() {
            let file_id = FileId(self.listed);
            let path = self.vfs.file_path(file_id).as_path().to_path_buf();
            self.files.entry(path.clone()).or_insert(CatalogFile {
                file_id,
                path,
                state: DiskFileState::Deleted,
            });
            self.listed += 1;
        }
    }
}

/// One path interner shared by every adapter for a workspace, available before
/// the disk database and its source roots are ready.
#[derive(Clone)]
pub struct WorkspaceCatalog(Arc<Mutex<CatalogState>>);

impl Default for WorkspaceCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceCatalog {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(CatalogState::new(Vfs::new()))))
    }

    pub fn from_vfs(vfs: Vfs) -> Self {
        Self(Arc::new(Mutex::new(CatalogState::new(vfs))))
    }

    pub fn intern_path(&self, path: PathBuf) -> FileId {
        self.0.lock().expect("workspace catalog mutex poisoned").alloc(path)
    }

    pub fn register_path(&self, path: PathBuf, file_id: FileId) -> Result<(), FileId> {
        let mut state = self.0.lock().expect("workspace catalog mutex poisoned");
        if let Some(existing) = state.files.get(&path) {
            return if existing.file_id == file_id { Ok(()) } else { Err(existing.file_id) };
        }
        let id = state.alloc(path);
        if id != file_id {
            return Err(id);
        }
        Ok(())
    }

    pub fn file_id(&self, path: &Path) -> Option<FileId> {
        self.0.lock().expect("workspace catalog mutex poisoned").files.get(path).map(|f| f.file_id)
    }

    pub fn path(&self, file_id: FileId) -> Option<PathBuf> {
        self.0
            .lock()
            .expect("workspace catalog mutex poisoned")
            .files
            .values()
            .find(|f| f.file_id == file_id)
            .map(|f| f.path.clone())
    }

    pub fn files(&self) -> Vec<CatalogFile> {
        self.0.lock().expect("workspace catalog mutex poisoned").files.values().cloned().collect()
    }

    pub fn set_state(&self, file: CatalogFile) {
        self.0
            .lock()
            .expect("workspace catalog mutex poisoned")
            .files
            .insert(file.path.clone(), file);
    }

    /// Ids allocated inside `f` are listed before the lock is released, so no other caller
    /// can observe the VFS ahead of the listing.
    pub fn with_vfs<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
        let mut state = self.0.lock().expect("workspace catalog mutex poisoned");
        let result = f(&mut state.vfs);
        state.list_allocated();
        result
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedChange {
    pub generation: u64,
    pub file_ids: Vec<FileId>,
    pub paths: Vec<PathBuf>,
    pub metadata_changed: bool,
}

pub enum CatchUp {
    Current,
    Changes(Vec<PublishedChange>),
    Reseed(Box<WorkspaceSnapshot>),
}

/// The single writer and disk database for one validated workspace. Its caller
/// provides the existing event-loop/resident serialization point.
pub struct WorkspaceBackend {
    host: AnalysisHost,
    generation: u64,
    journal: VecDeque<PublishedChange>,
    catalog: WorkspaceCatalog,
}

impl WorkspaceBackend {
    pub fn new(host: AnalysisHost) -> Self {
        let mut host = host;
        if host.raw_database().try_source_root_input(BSL_SOURCE_ROOT).is_none() {
            host.raw_database_mut()
                .set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(FileSet::new()));
        }
        let mut backend = Self {
            host,
            generation: 0,
            journal: VecDeque::new(),
            catalog: WorkspaceCatalog::new(),
        };
        backend.rebuild_catalog();
        backend
    }

    pub fn with_catalog(host: AnalysisHost, catalog: WorkspaceCatalog) -> Self {
        let mut host = host;
        if host.raw_database().try_source_root_input(BSL_SOURCE_ROOT).is_none() {
            host.raw_database_mut()
                .set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(FileSet::new()));
        }
        Self { host, generation: 0, journal: VecDeque::new(), catalog }
    }

    pub fn from_database(db: RootDatabaseImpl) -> Self {
        Self::new(AnalysisHost::from_database(db))
    }

    pub fn from_database_with_catalog(db: RootDatabaseImpl, catalog: WorkspaceCatalog) -> Self {
        Self::with_catalog(AnalysisHost::from_database(db), catalog)
    }

    pub fn database(&self) -> &RootDatabaseImpl {
        self.host.raw_database()
    }

    pub fn database_mut(&mut self) -> &mut RootDatabaseImpl {
        self.host.raw_database_mut()
    }

    pub fn into_database(self) -> RootDatabaseImpl {
        self.host.into_database()
    }

    pub fn catalog(&self) -> WorkspaceCatalog {
        self.catalog.clone()
    }

    /// Register the canonical VFS identity. The caller supplies the FileId from
    /// the shared VFS so all views use the same path identity.
    pub fn register_path(&self, path: PathBuf, file_id: FileId) -> Result<(), FileId> {
        self.catalog.register_path(path, file_id)
    }

    pub fn file_id(&self, canonical_path: &Path) -> Option<FileId> {
        self.catalog.file_id(canonical_path)
    }

    pub fn analysis(&self) -> Analysis {
        self.host.analysis()
    }

    pub fn snapshot(&self) -> WorkspaceSnapshot {
        let mut snapshot = WorkspaceSnapshot::capture(self.database(), self.generation);
        snapshot.catalog = self.catalog.files();
        snapshot
    }

    /// Apply the source portion first, then let the owner update prepared
    /// metadata inputs before the generation becomes visible.
    pub fn apply_source_batch(
        &mut self,
        changes: impl IntoIterator<Item = SourceBatchChange>,
        update_metadata: impl FnOnce(&mut RootDatabaseImpl, &crate::SourceBatchEffects) -> bool,
    ) -> Option<PublishedChange> {
        self.apply_batches([(BSL_SOURCE_ROOT, changes)], update_metadata)
    }

    /// Apply source changes in root order, then refresh metadata and publish one
    /// generation for the complete source/metadata batch.
    pub fn apply_batches<I>(
        &mut self,
        batches: impl IntoIterator<Item = (SourceRootId, I)>,
        update_metadata: impl FnOnce(&mut RootDatabaseImpl, &crate::SourceBatchEffects) -> bool,
    ) -> Option<PublishedChange>
    where
        I: IntoIterator<Item = SourceBatchChange>,
    {
        let mut batches: Vec<_> = batches.into_iter().collect();
        batches.sort_by_key(|(root_id, _)| *root_id);
        let db = self.host.raw_database_mut();
        let mut effects = crate::SourceBatchEffects::default();
        for (root_id, changes) in batches {
            let batch = apply_source_batch(db, root_id, changes);
            effects.file_set_changed |= batch.file_set_changed;
            effects.bsl_source_changed |= batch.bsl_source_changed;
            effects.changed_file_ids.extend(batch.changed_file_ids);
            effects.affected_paths.extend(batch.affected_paths);
        }
        let metadata_changed = update_metadata(db, &effects);
        self.publish_change(effects.changed_file_ids, effects.affected_paths, metadata_changed)
    }

    /// Publish changes made through the compatibility resident API while its
    /// caller holds the same writer lock. Use after its lazy source iterator and
    /// metadata refresh have both completed.
    pub fn publish_change(
        &mut self,
        file_ids: impl IntoIterator<Item = FileId>,
        paths: impl IntoIterator<Item = PathBuf>,
        metadata_changed: bool,
    ) -> Option<PublishedChange> {
        let mut file_ids: Vec<_> = file_ids.into_iter().collect();
        let mut paths: Vec<_> = paths.into_iter().collect();
        file_ids.sort_unstable();
        file_ids.dedup();
        paths.sort();
        paths.dedup();
        if file_ids.is_empty() && paths.is_empty() && !metadata_changed {
            return None;
        }
        self.generation = self.generation.saturating_add(1);
        let change =
            PublishedChange { generation: self.generation, file_ids, paths, metadata_changed };
        self.reconcile_catalog();
        self.journal.push_back(change.clone());
        if self.journal.len() > JOURNAL_CAPACITY {
            self.journal.pop_front();
        }
        Some(change)
    }

    /// Publish completion of an initial or rebuilt owner snapshot. Clearing the
    /// journal forces every older view to seed from the complete snapshot.
    pub fn publish_full_rebuild(&mut self) -> u64 {
        self.reconcile_catalog();
        self.generation = self.generation.saturating_add(1);
        self.journal.clear();
        self.generation
    }

    /// Publish a replacement backend after rebuilding from owner state that was
    /// previously at `previous_generation`. Every older view must reseed, while
    /// generations remain monotonic across replacement of the backend value.
    pub fn publish_full_rebuild_from(&mut self, previous_generation: u64) -> u64 {
        self.reconcile_catalog();
        self.generation = previous_generation.saturating_add(1);
        self.journal.clear();
        self.generation
    }

    pub fn changes_since(&self, generation: u64) -> CatchUp {
        if generation == self.generation {
            return CatchUp::Current;
        }
        if generation > self.generation {
            return CatchUp::Reseed(Box::new(self.snapshot()));
        }
        let oldest = self.journal.front().map_or(self.generation + 1, |change| change.generation);
        if generation.saturating_add(1) < oldest {
            return CatchUp::Reseed(Box::new(self.snapshot()));
        }
        CatchUp::Changes(
            self.journal.iter().filter(|change| change.generation > generation).cloned().collect(),
        )
    }

    fn rebuild_catalog(&mut self) {
        let db = self.database();
        for id in [base_db::BSL_SOURCE_ROOT, base_db::METADATA_SOURCE_ROOT] {
            let Some(input) = db.try_source_root_input(id) else { continue };
            let root = input.root(db);
            for file_id in root.iter() {
                let Some(path) = root.file_set().path_for_file(&file_id) else { continue };
                let path = path.as_path().to_path_buf();
                let file_state = db.try_file_revision_input(file_id).map_or(
                    DiskFileState::Unreadable,
                    |revision| {
                        if revision.unreadable(db) {
                            DiskFileState::Unreadable
                        } else {
                            DiskFileState::Readable { revision: revision.revision(db) }
                        }
                    },
                );
                let _ = self.catalog.register_path(path.clone(), file_id);
                self.catalog.set_state(CatalogFile { file_id, path, state: file_state });
            }
        }
    }

    fn reconcile_catalog(&mut self) {
        let db = self.database();
        let mut present = HashMap::new();
        for id in [base_db::BSL_SOURCE_ROOT, base_db::METADATA_SOURCE_ROOT] {
            let Some(input) = db.try_source_root_input(id) else { continue };
            let root = input.root(db);
            for file_id in root.iter() {
                let Some(path) = root.file_set().path_for_file(&file_id) else { continue };
                let path = path.as_path().to_path_buf();
                let file_state = db.try_file_revision_input(file_id).map_or(
                    DiskFileState::Unreadable,
                    |revision| {
                        if revision.unreadable(db) {
                            DiskFileState::Unreadable
                        } else {
                            DiskFileState::Readable { revision: revision.revision(db) }
                        }
                    },
                );
                present.insert(path.clone(), CatalogFile { file_id, path, state: file_state });
            }
        }
        for file in present.into_values() {
            self.catalog.set_state(file);
        }
        for mut file in self.catalog.files() {
            if ![base_db::BSL_SOURCE_ROOT, base_db::METADATA_SOURCE_ROOT].iter().any(|id| {
                db.try_source_root_input(*id).is_some_and(|root| {
                    root.root(db).file_set().path_for_file(&file.file_id).is_some()
                })
            }) {
                file.state = DiskFileState::Deleted;
                self.catalog.set_state(file);
            }
        }
    }
}

impl Deref for WorkspaceBackend {
    type Target = RootDatabaseImpl;

    fn deref(&self) -> &Self::Target {
        self.database()
    }
}

impl DerefMut for WorkspaceBackend {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.database_mut()
    }
}

impl WorkspaceSnapshot {
    pub fn capture(db: &RootDatabaseImpl, generation: u64) -> Self {
        let mut source_roots = Vec::new();
        let mut files = Vec::new();
        for id in [base_db::BSL_SOURCE_ROOT, base_db::METADATA_SOURCE_ROOT] {
            let Some(input) = db.try_source_root_input(id) else { continue };
            let root = input.root(db);
            for file_id in root.iter() {
                let Some(path) = root.file_set().path_for_file(&file_id) else { continue };
                let Some(revision) = db.try_file_revision_input(file_id) else { continue };
                let path = path.as_path().to_path_buf();
                files.push(SnapshotFile {
                    file_id,
                    path: path.clone(),
                    root_id: id,
                    state: if revision.unreadable(db) {
                        DiskFileState::Unreadable
                    } else {
                        DiskFileState::Readable { revision: revision.revision(db) }
                    },
                });
            }
            source_roots.push(SnapshotSourceRoot { id, root: root.clone() });
        }
        let configs = db.workspace_configs_snapshot().as_ref().clone();
        let metadata = db
            .all_config_paths()
            .into_iter()
            .filter_map(|(_, path)| {
                db.metadata_listing_snapshot(&path.to_string_lossy()).map(|data| (path, data))
            })
            .collect();
        let catalog = files
            .iter()
            .map(|file| CatalogFile {
                file_id: file.file_id,
                path: file.path.clone(),
                state: file.state,
            })
            .collect();
        WorkspaceSnapshot {
            generation,
            metadata_ready: db.workspace_load_complete(),
            configs,
            source_roots,
            files,
            catalog,
            metadata,
        }
    }
}

/// One independent Salsa storage for a single editor connection.
pub struct EditorView {
    db: RootDatabaseImpl,
    generation: u64,
    disk: WorkspaceSnapshot,
    open: HashMap<FileId, (PathBuf, Arc<str>)>,
}

impl EditorView {
    pub fn new(snapshot: WorkspaceSnapshot) -> Self {
        Self::prepare(snapshot, || false).expect("an uncancelled editor view must prepare")
    }

    pub fn prepare(
        snapshot: WorkspaceSnapshot,
        mut cancelled: impl FnMut() -> bool,
    ) -> Option<Self> {
        let mut view = Self {
            db: RootDatabaseImpl::new(),
            generation: 0,
            disk: snapshot.clone(),
            open: HashMap::new(),
        };
        view.seed_cancellable(&snapshot, &mut cancelled).then_some(view)
    }

    pub fn analysis(&self) -> Analysis {
        Analysis::from_database(self.db.clone())
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn database(&self) -> &RootDatabaseImpl {
        &self.db
    }

    pub fn database_mut(&mut self) -> &mut RootDatabaseImpl {
        &mut self.db
    }

    /// Replace this storage with a fully prepared independent view. The current
    /// editor buffers are replayed after the prepared disk snapshot is installed.
    /// A stale prepared view is rejected without changing this view.
    pub fn replace_from_prepared(&mut self, mut prepared: EditorView) -> bool {
        if prepared.generation < self.generation {
            return false;
        }
        for (file_id, (path, text)) in self.open.drain() {
            prepared.open(file_id, path, text);
        }
        *self = prepared;
        true
    }

    pub fn into_host(self) -> AnalysisHost {
        AnalysisHost::from_database(self.db)
    }

    pub fn open(&mut self, file_id: FileId, path: PathBuf, text: Arc<str>) {
        apply_source_batch(
            &mut self.db,
            BSL_SOURCE_ROOT,
            [SourceBatchChange::Upsert {
                file_id,
                path: VfsPath::new(path.clone()),
                source: Some(BatchTextSource::Overlay(Arc::clone(&text))),
            }],
        );
        self.open.insert(file_id, (path, text));
    }

    pub fn change(&mut self, file_id: FileId, text: Arc<str>) -> bool {
        let Some((path, _)) = self.open.get(&file_id) else { return false };
        apply_source_batch(
            &mut self.db,
            BSL_SOURCE_ROOT,
            [SourceBatchChange::Upsert {
                file_id,
                path: VfsPath::new(path.clone()),
                source: Some(BatchTextSource::Overlay(Arc::clone(&text))),
            }],
        );
        self.open.insert(file_id, (path.clone(), text));
        true
    }

    /// Saving is an owner-side disk event. This records no disk write and keeps
    /// the editor's current buffer authoritative until close or a later edit.
    pub fn saved(&self, file_id: FileId) -> bool {
        self.open.contains_key(&file_id)
    }

    pub fn close(&mut self, file_id: FileId) -> bool {
        if self.open.remove(&file_id).is_none() {
            return false;
        }
        self.restore_disk_file(file_id);
        true
    }

    pub fn catch_up(&mut self, snapshot: WorkspaceSnapshot, changes: &[PublishedChange]) {
        if snapshot.generation < self.generation {
            return;
        }
        if !self.catch_up_deltas(snapshot.clone(), changes) {
            let open = self.open.clone();
            self.db = RootDatabaseImpl::new();
            self.seed(&snapshot);
            self.open.clear();
            for (file_id, (path, text)) in open {
                self.open(file_id, path, text);
            }
        }
    }

    /// Apply a complete, contiguous publication suffix without rebuilding the
    /// Salsa storage. Returns false without mutation when the caller must prepare
    /// a fresh view off-loop (journal gap, stale or mixed generation).
    pub fn catch_up_deltas(
        &mut self,
        snapshot: WorkspaceSnapshot,
        changes: &[PublishedChange],
    ) -> bool {
        if snapshot.generation < self.generation {
            return false;
        }
        if snapshot.generation == self.generation {
            return changes.is_empty();
        }
        if changes.is_empty()
            || changes.first().is_none_or(|change| change.generation != self.generation + 1)
            || changes.windows(2).any(|pair| pair[1].generation != pair[0].generation + 1)
            || changes.last().is_none_or(|change| change.generation != snapshot.generation)
        {
            return false;
        }
        let mut changed: std::collections::HashSet<_> =
            changes.iter().flat_map(|c| c.file_ids.iter().copied()).collect();
        let next: HashMap<_, _> = snapshot.files.iter().map(|file| (file.file_id, file)).collect();
        if changes.iter().any(|change| change.metadata_changed) {
            let previous_metadata: HashMap<_, _> = self
                .disk
                .files
                .iter()
                .filter(|file| file.root_id == base_db::METADATA_SOURCE_ROOT)
                .map(|file| (file.file_id, file))
                .collect();
            let next_metadata: HashMap<_, _> = snapshot
                .files
                .iter()
                .filter(|file| file.root_id == base_db::METADATA_SOURCE_ROOT)
                .map(|file| (file.file_id, file))
                .collect();
            changed.extend(previous_metadata.iter().filter_map(|(file_id, previous)| {
                next_metadata
                    .get(file_id)
                    .is_none_or(|next| next.path != previous.path || next.state != previous.state)
                    .then_some(*file_id)
            }));
            changed.extend(next_metadata.iter().filter_map(|(file_id, next)| {
                previous_metadata
                    .get(file_id)
                    .is_none_or(|previous| {
                        previous.path != next.path || previous.state != next.state
                    })
                    .then_some(*file_id)
            }));
        }
        for root_id in [base_db::BSL_SOURCE_ROOT, base_db::METADATA_SOURCE_ROOT] {
            let next_root = snapshot.source_roots.iter().find(|root| root.id == root_id);
            if next_root.is_some() && self.db.try_source_root_input(root_id).is_none() {
                let empty = if root_id == base_db::METADATA_SOURCE_ROOT {
                    SourceRoot::new_metadata(FileSet::new())
                } else {
                    SourceRoot::new_local(FileSet::new())
                };
                self.db.set_source_root(root_id, empty);
            }
            let current = self.db.try_source_root_input(root_id).map(|input| input.root(&self.db));
            let current_ids: Vec<_> =
                current.as_ref().map(|root| root.iter().collect()).unwrap_or_default();
            let mut batch = Vec::new();
            for file_id in &current_ids {
                if changed.contains(file_id)
                    && next.get(file_id).is_none_or(|file| file.root_id != root_id)
                    && !self.open.contains_key(file_id)
                {
                    batch.push(SourceBatchChange::Remove { file_id: *file_id });
                }
            }
            for file_id in &changed {
                if self.open.contains_key(file_id) {
                    continue;
                }
                if let Some(file) = next.get(file_id).filter(|file| file.root_id == root_id) {
                    batch.push(SourceBatchChange::Upsert {
                        file_id: *file_id,
                        path: VfsPath::new(file.path.clone()),
                        source: Some(source_for_state(file.state)),
                    });
                }
            }
            if self.db.try_source_root_input(root_id).is_some() {
                apply_source_batch(&mut self.db, root_id, batch);
            }
        }
        if self.db.workspace_configs_snapshot().as_ref() != &snapshot.configs {
            self.db.set_workspace_configs_snapshot(snapshot.configs.clone());
        }
        for (root, listing) in &snapshot.metadata {
            self.db.set_metadata_listing_snapshot(&root.to_string_lossy(), listing.clone());
        }
        self.db.set_workspace_load_complete(snapshot.metadata_ready);
        self.generation = snapshot.generation;
        self.disk = snapshot;
        for file_id in changed {
            if self.open.contains_key(&file_id) {
                if let Some((path, text)) = self.open.get(&file_id).cloned() {
                    self.open(file_id, path, text);
                }
            }
        }
        true
    }

    fn restore_disk_file(&mut self, file_id: FileId) {
        if let Some(file) = self.disk.files.iter().find(|file| file.file_id == file_id) {
            apply_source_batch(
                &mut self.db,
                file.root_id,
                [SourceBatchChange::Upsert {
                    file_id,
                    path: VfsPath::new(file.path.clone()),
                    source: Some(source_for_state(file.state)),
                }],
            );
        } else {
            apply_source_batch(
                &mut self.db,
                BSL_SOURCE_ROOT,
                [SourceBatchChange::Remove { file_id }],
            );
        }
    }

    fn seed(&mut self, snapshot: &WorkspaceSnapshot) {
        let mut never_cancelled = || false;
        assert!(self.seed_cancellable(snapshot, &mut never_cancelled));
    }

    fn seed_cancellable(
        &mut self,
        snapshot: &WorkspaceSnapshot,
        cancelled: &mut impl FnMut() -> bool,
    ) -> bool {
        if self.db.try_source_root_input(BSL_SOURCE_ROOT).is_none() {
            self.db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(FileSet::new()));
        }
        self.db.set_workspace_configs_snapshot(snapshot.configs.clone());
        for root in &snapshot.source_roots {
            if cancelled() {
                return false;
            }
            self.db.set_source_root(root.id, root.root.clone());
            let changes =
                snapshot.files.iter().filter(|file| file.root_id == root.id).map(|file| {
                    SourceBatchChange::Upsert {
                        file_id: file.file_id,
                        path: VfsPath::new(file.path.clone()),
                        source: Some(source_for_state(file.state)),
                    }
                });
            if crate::load::apply_source_batch_cancellable(
                &mut self.db,
                root.id,
                changes,
                &mut *cancelled,
            )
            .is_none()
            {
                return false;
            }
            if root.id == BSL_SOURCE_ROOT && snapshot.generation > 0 {
                hold_shared_test_gate("BSL_TEST_SHARED_EDITOR_SEED_GATE");
            }
        }
        for (root, listing) in &snapshot.metadata {
            if cancelled() {
                return false;
            }
            self.db.set_metadata_listing_snapshot(&root.to_string_lossy(), listing.clone());
        }
        if cancelled() {
            return false;
        }
        self.db.set_workspace_load_complete(snapshot.metadata_ready);
        self.db.use_prepared_metadata_inputs();
        self.generation = snapshot.generation;
        self.disk = snapshot.clone();
        true
    }
}

fn hold_shared_test_gate(variable: &str) {
    if std::env::var_os("BSL_SHARED_BACKEND_MARKERS").is_none() {
        return;
    }
    let Some(gate) = std::env::var_os(variable).map(PathBuf::from) else { return };
    std::fs::create_dir_all(&gate).expect("create isolated shared-workspace test gate");
    let entered = gate.join("entered");
    let Ok(_marker) = std::fs::OpenOptions::new().write(true).create_new(true).open(entered) else {
        return;
    };
    let release = gate.join("release");
    // Held until the test that set the gate releases it, however long that takes on the
    // machine running it; a gate never released is a hang the test runner reports.
    while !release.is_file() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn source_for_state(state: DiskFileState) -> BatchTextSource {
    match state {
        DiskFileState::Readable { revision } => BatchTextSource::DiskRevision(revision),
        DiskFileState::Unreadable => BatchTextSource::Unreadable,
        DiskFileState::Deleted => BatchTextSource::Deleted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_db::{SourceRoot, BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT};
    use ide_db::metadata::MetadataListingData;
    use salsa::Database as _;
    use std::panic::AssertUnwindSafe;
    use vfs::file_set::FileSet;

    fn disk_backend() -> (tempfile::TempDir, WorkspaceBackend, PathBuf) {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bsl");
        std::fs::File::create(&path).unwrap().write_all(b"disk").unwrap();
        let mut db = RootDatabaseImpl::new();
        let mut files = FileSet::new();
        files.insert(FileId(0), VfsPath::new(path.clone()));
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(files));
        db.set_file_source_root(FileId(0), BSL_SOURCE_ROOT);
        db.set_file_revision_from_disk(FileId(0), base_db::content_revision("disk"));
        let catalog = WorkspaceCatalog::new();
        assert_eq!(catalog.intern_path(path.clone()), FileId(0));
        let mut backend = WorkspaceBackend::from_database_with_catalog(db, catalog);
        backend.publish_full_rebuild_from(0);
        (dir, backend, path)
    }

    #[test]
    fn independent_views_keep_their_open_text() {
        let (_dir, backend, _) = disk_backend();
        let snapshot = backend.snapshot();
        let mut a = EditorView::new(snapshot.clone());
        let mut b = EditorView::new(snapshot);
        a.open(FileId(0), backend.catalog().path(FileId(0)).unwrap(), Arc::from("A"));
        b.open(FileId(0), backend.catalog().path(FileId(0)).unwrap(), Arc::from("B"));
        assert_eq!(a.open[&FileId(0)].1.as_ref(), "A");
        assert_eq!(b.open[&FileId(0)].1.as_ref(), "B");
        assert_eq!(base_db::SourceDatabase::file_text(&a.db, FileId(0)).as_ref(), "A");
        assert_eq!(base_db::SourceDatabase::file_text(&b.db, FileId(0)).as_ref(), "B");
    }

    #[test]
    fn parallel_diagnostics_isolated_across_views_and_writer_cancels_only_its_view() {
        use ide::DiagnosticCode;

        const SELF_ASSIGN: &str = "Процедура Тест()\n    А = А;\nКонецПроцедуры\n";
        const CLEAN: &str = "Процедура Тест()\n    А = 1;\nКонецПроцедуры\n";

        let (_dir, backend, path) = disk_backend();
        let snapshot = backend.snapshot();
        let mut a = EditorView::new(snapshot.clone());
        let mut b = EditorView::new(snapshot);
        a.open(FileId(0), path.clone(), Arc::from(SELF_ASSIGN));
        b.open(FileId(0), path, Arc::from(SELF_ASSIGN));

        let config = ide::DiagnosticsConfig::all_enabled().to_input();
        let hang_guard = test_utils::wait::hang_guard(std::time::Duration::from_secs(60));
        let pool_a = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(2).build().unwrap());
        let pool_b = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(2).build().unwrap());

        let local = a.analysis();
        let local_token = local.database().cancellation_token();
        let independent = b.analysis();
        let independent_pool = Arc::clone(&pool_b);
        local.attached(|analysis| {
            local_token.cancel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let result = independent.workspace_diagnostics_parallel(
                    &[FileId(0)],
                    ide::DiagnosticsConfig::all_enabled().to_input(),
                    &independent_pool,
                );
                result_tx.send(result).expect("test receives independent analysis result");
            });
            let result = result_rx
                .recv_timeout(hang_guard)
                .expect("independent workspace diagnostics complete");
            worker.join().expect("independent analysis worker exits");
            assert!(result[0]
                .1
                .iter()
                .any(|diagnostic| diagnostic.code == DiagnosticCode::SelfAssign));
            assert!(matches!(
                salsa::Cancelled::catch(AssertUnwindSafe(|| {
                    salsa::Database::unwind_if_revision_cancelled(analysis.database());
                })),
                Err(salsa::Cancelled::Local)
            ));
        });
        drop(local);

        // Hold A's reader until its writer is pending; B must remain usable.
        let (admitted_tx, admitted_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let (request_done_tx, request_done_rx) = std::sync::mpsc::channel();
        let analysis_a = a.analysis();
        let observer = a.analysis();
        let request_pool = Arc::clone(&pool_a);
        let cancelled_request = std::thread::spawn(move || {
            let result = salsa::Cancelled::catch(AssertUnwindSafe(|| {
                analysis_a.attached(|analysis| {
                    admitted_tx.send(()).expect("test receives reader admission");
                    let _ = continue_rx.recv_timeout(hang_guard);
                    analysis.workspace_diagnostics_parallel(&[FileId(0)], config, &request_pool)
                })
            }));
            request_done_tx.send(result).expect("test receives reader completion");
        });

        admitted_rx.recv_timeout(hang_guard).expect("reader reaches the gate");
        let (writer_started_tx, writer_started_rx) = std::sync::mpsc::channel();
        let (writer_done_tx, writer_done_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            writer_started_tx.send(()).expect("test receives writer start");
            assert!(a.change(FileId(0), Arc::from(CLEAN)));
            writer_done_tx.send(a).expect("test receives writer completion");
        });
        writer_started_rx.recv_timeout(hang_guard).expect("writer thread starts");
        let deadline = std::time::Instant::now()
            + test_utils::wait::hang_guard(std::time::Duration::from_secs(10));
        loop {
            let cancelled = salsa::Cancelled::catch(AssertUnwindSafe(|| {
                salsa::Database::unwind_if_revision_cancelled(observer.database());
            }));
            if matches!(cancelled, Err(salsa::Cancelled::PendingWrite)) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "A's writer never became pending");
            std::thread::yield_now();
        }
        let before_b = b.analysis().workspace_diagnostics_parallel(
            &[FileId(0)],
            ide::DiagnosticsConfig::all_enabled().to_input(),
            &pool_b,
        );
        assert!(before_b[0]
            .1
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::SelfAssign));
        continue_tx.send(()).expect("reader is waiting for release");
        let request_result = request_done_rx
            .recv_timeout(hang_guard)
            .expect("cancelled diagnostics request completes");
        cancelled_request.join().expect("request worker exits");
        assert!(matches!(request_result, Err(salsa::Cancelled::PendingWrite)));
        drop(observer);
        let a = writer_done_rx.recv_timeout(hang_guard).expect("writer finishes after release");
        writer.join().expect("writer thread exits");

        // The same FileId means different files in these independent Salsa storages.
        // Run both views through the LSP batch's actual bounded parallel API at once;
        // A sees its just-written clean text and B retains its self-assignment diagnostic.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (start_a_tx, start_a_rx) = std::sync::mpsc::channel();
        let (start_b_tx, start_b_rx) = std::sync::mpsc::channel();
        let (result_a_tx, result_a_rx) = std::sync::mpsc::channel();
        let (result_b_tx, result_b_rx) = std::sync::mpsc::channel();
        let analysis_a = a.analysis();
        let analysis_b = b.analysis();
        let config_a = ide::DiagnosticsConfig::all_enabled().to_input();
        let config_b = config_a.clone();
        let ready_a_tx = ready_tx.clone();
        let sweep_a = std::thread::spawn(move || {
            ready_a_tx.send(()).expect("test receives A readiness");
            start_a_rx.recv_timeout(hang_guard).expect("test releases A diagnostics sweep");
            let result = analysis_a.workspace_diagnostics_parallel(&[FileId(0)], config_a, &pool_a);
            result_a_tx.send(result).expect("test receives A diagnostics");
        });
        let sweep_b = std::thread::spawn(move || {
            ready_tx.send(()).expect("test receives B readiness");
            start_b_rx.recv_timeout(hang_guard).expect("test releases B diagnostics sweep");
            let result = analysis_b.workspace_diagnostics_parallel(&[FileId(0)], config_b, &pool_b);
            result_b_tx.send(result).expect("test receives B diagnostics");
        });
        ready_rx.recv_timeout(hang_guard).expect("A diagnostics worker is ready");
        ready_rx.recv_timeout(hang_guard).expect("B diagnostics worker is ready");
        start_a_tx.send(()).expect("A diagnostics worker waits for release");
        start_b_tx.send(()).expect("B diagnostics worker waits for release");

        let result_a = result_a_rx.recv_timeout(hang_guard).expect("A diagnostics sweep completes");
        let result_b = result_b_rx.recv_timeout(hang_guard).expect("B diagnostics sweep completes");
        sweep_a.join().expect("A diagnostics worker exits");
        sweep_b.join().expect("B diagnostics worker exits");
        assert_eq!(result_a.len(), 1);
        assert_eq!(result_b.len(), 1);
        assert!(
            result_a[0].1.iter().all(|diagnostic| diagnostic.code != DiagnosticCode::SelfAssign),
            "A's writer must update only A's analysis storage"
        );
        assert!(
            result_b[0].1.iter().any(|diagnostic| diagnostic.code == DiagnosticCode::SelfAssign),
            "B must retain its independent overlay"
        );
    }

    #[test]
    fn cold_view_accepts_open_text_before_owner_source_seed() {
        let snapshot = WorkspaceSnapshot::capture(&RootDatabaseImpl::new(), 0);
        let mut view = EditorView::new(snapshot);

        view.open(FileId(0), PathBuf::from("/workspace/new.bsl"), Arc::from("FUNCTION X()"));

        assert_eq!(
            base_db::SourceDatabase::file_text(view.database(), FileId(0)).as_ref(),
            "FUNCTION X()"
        );
        assert!(view.database().try_source_root_input(BSL_SOURCE_ROOT).is_some());
        assert!(view.database().try_source_root_input(base_db::METADATA_SOURCE_ROOT).is_none());
    }

    #[test]
    fn prepared_view_cancels_at_a_file_checkpoint() {
        let mut db = RootDatabaseImpl::new();
        let mut files = FileSet::new();
        for index in 0..6 {
            let file_id = FileId(index);
            files.insert(file_id, VfsPath::new(PathBuf::from(format!("/workspace/{index}.bsl"))));
            db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
            db.set_file_revision_from_disk(file_id, u64::from(index) + 1);
        }
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(files));
        let snapshot = WorkspaceSnapshot::capture(&db, 3);
        let mut checkpoints = 0;

        let prepared = EditorView::prepare(snapshot, || {
            checkpoints += 1;
            checkpoints == 5
        });

        assert!(prepared.is_none());
        assert_eq!(checkpoints, 5, "preparation stops at the requested file checkpoint");
    }

    #[test]
    fn unchanged_open_text_wins_until_close_and_deleted_files_stay_open() {
        let (_dir, mut backend, path) = disk_backend();
        let mut a = EditorView::new(backend.snapshot());
        let mut b = EditorView::new(backend.snapshot());
        a.open(FileId(0), path.clone(), Arc::from("disk"));
        b.open(FileId(0), path.clone(), Arc::from("B"));

        std::fs::write(&path, "saved").unwrap();
        let change = backend
            .apply_source_batch(
                vec![SourceBatchChange::Upsert {
                    file_id: FileId(0),
                    path: VfsPath::new(path.clone()),
                    source: Some(BatchTextSource::DiskOwned("saved".to_owned())),
                }],
                |_, _| false,
            )
            .unwrap();
        let snapshot = backend.snapshot();
        a.catch_up(snapshot.clone(), std::slice::from_ref(&change));
        b.catch_up(snapshot, std::slice::from_ref(&change));
        assert_eq!(
            base_db::SourceDatabase::file_text(backend.database(), FileId(0)).as_ref(),
            "saved"
        );
        assert_eq!(base_db::SourceDatabase::file_text(a.database(), FileId(0)).as_ref(), "disk");
        assert!(a.saved(FileId(0)));
        assert!(a.close(FileId(0)));
        assert_eq!(base_db::SourceDatabase::file_text(a.database(), FileId(0)).as_ref(), "saved");
        assert_eq!(base_db::SourceDatabase::file_text(b.database(), FileId(0)).as_ref(), "B");

        std::fs::remove_file(&path).unwrap();
        let deleted = backend
            .apply_source_batch(vec![SourceBatchChange::Remove { file_id: FileId(0) }], |_, _| {
                false
            })
            .unwrap();
        b.catch_up(backend.snapshot(), std::slice::from_ref(&deleted));
        assert_eq!(base_db::SourceDatabase::file_text(b.database(), FileId(0)).as_ref(), "B");
        assert!(b.close(FileId(0)));
        assert_eq!(base_db::SourceDatabase::file_text(b.database(), FileId(0)).as_ref(), "");
    }

    #[test]
    fn catch_up_before_close_restores_latest_disk_revision_and_keeps_other_overlay() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("module.bsl");
        std::fs::File::create(&path).unwrap().write_all(b"Number").unwrap();
        let mut db = RootDatabaseImpl::new();
        let mut files = FileSet::new();
        files.insert(FileId(0), VfsPath::new(path.clone()));
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(files));
        db.set_file_source_root(FileId(0), BSL_SOURCE_ROOT);
        db.set_file_revision_from_disk(FileId(0), base_db::content_revision("Number"));
        let catalog = WorkspaceCatalog::new();
        assert_eq!(catalog.intern_path(path.clone()), FileId(0));
        let mut backend = WorkspaceBackend::from_database_with_catalog(db, catalog);
        backend.publish_full_rebuild_from(0);

        let mut saved_view = EditorView::new(backend.snapshot());
        let mut other_view = EditorView::new(backend.snapshot());
        saved_view.open(FileId(0), path.clone(), Arc::from("Number"));
        other_view.open(FileId(0), path.clone(), Arc::from("Other overlay"));

        std::fs::write(&path, "String").unwrap();
        let change = backend
            .apply_source_batch(
                vec![SourceBatchChange::Upsert {
                    file_id: FileId(0),
                    path: VfsPath::new(path),
                    source: Some(BatchTextSource::DiskOwned("String".to_owned())),
                }],
                |_, _| false,
            )
            .unwrap();
        let latest = backend.snapshot();
        assert!(saved_view.catch_up_deltas(latest.clone(), std::slice::from_ref(&change)));
        assert!(other_view.catch_up_deltas(latest, std::slice::from_ref(&change)));

        assert!(saved_view.close(FileId(0)));
        assert_eq!(
            base_db::SourceDatabase::try_file_revision_input(saved_view.database(), FileId(0))
                .unwrap()
                .revision(saved_view.database()),
            base_db::content_revision("String")
        );
        assert_eq!(
            base_db::SourceDatabase::file_text(saved_view.database(), FileId(0)).as_ref(),
            "String"
        );
        assert_eq!(
            base_db::SourceDatabase::file_text(other_view.database(), FileId(0)).as_ref(),
            "Other overlay"
        );
    }

    #[test]
    fn metadata_delta_updates_revision_and_deleted_membership_without_touching_open_source() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("module.bsl");
        let metadata_path = dir.path().join("Catalog.xml");
        std::fs::write(&source_path, "disk source").unwrap();
        std::fs::write(&metadata_path, "old metadata").unwrap();

        let mut db = RootDatabaseImpl::new();
        let mut source_files = FileSet::new();
        source_files.insert(FileId(0), VfsPath::new(source_path.clone()));
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(source_files));
        db.set_file_source_root(FileId(0), BSL_SOURCE_ROOT);
        db.set_file_revision_from_disk(FileId(0), base_db::content_revision("disk source"));

        let mut metadata_files = FileSet::new();
        metadata_files.insert(FileId(1), VfsPath::new(metadata_path.clone()));
        db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(metadata_files));
        db.set_file_source_root(FileId(1), METADATA_SOURCE_ROOT);
        db.set_file_revision_from_disk(FileId(1), base_db::content_revision("old metadata"));

        let catalog = WorkspaceCatalog::new();
        assert_eq!(catalog.intern_path(source_path.clone()), FileId(0));
        assert_eq!(catalog.intern_path(metadata_path.clone()), FileId(1));
        let mut backend = WorkspaceBackend::from_database_with_catalog(db, catalog);
        backend.publish_full_rebuild_from(0);
        let mut view = EditorView::new(backend.snapshot());
        view.open(FileId(0), source_path.clone(), Arc::from("open source overlay"));

        std::fs::write(&metadata_path, "new metadata").unwrap();
        apply_source_batch(
            backend.database_mut(),
            METADATA_SOURCE_ROOT,
            [SourceBatchChange::Upsert {
                file_id: FileId(1),
                path: VfsPath::new(metadata_path.clone()),
                source: Some(BatchTextSource::DiskRevision(base_db::content_revision(
                    "new metadata",
                ))),
            }],
        );
        let changed = backend
            .publish_change([], [metadata_path.clone()], true)
            .expect("metadata update publishes a generation");
        assert!(changed.file_ids.is_empty(), "metadata revisions are carried by the snapshot");
        assert!(view.catch_up_deltas(backend.snapshot(), std::slice::from_ref(&changed)));
        assert_eq!(
            base_db::SourceDatabase::try_file_revision_input(view.database(), FileId(1))
                .unwrap()
                .revision(view.database()),
            base_db::content_revision("new metadata")
        );
        assert_eq!(
            base_db::SourceDatabase::file_text(view.database(), FileId(1)).as_ref(),
            "new metadata"
        );
        assert_eq!(
            base_db::SourceDatabase::file_text(view.database(), FileId(0)).as_ref(),
            "open source overlay"
        );

        std::fs::remove_file(&metadata_path).unwrap();
        apply_source_batch(
            backend.database_mut(),
            METADATA_SOURCE_ROOT,
            [SourceBatchChange::Remove { file_id: FileId(1) }],
        );
        let deleted = backend
            .publish_change([], [metadata_path], true)
            .expect("metadata deletion publishes a generation");
        assert!(view.catch_up_deltas(backend.snapshot(), std::slice::from_ref(&deleted)));
        let metadata_root =
            view.database().source_root_input(METADATA_SOURCE_ROOT).root(view.database());
        assert!(
            metadata_root.file_set().path_for_file(&FileId(1)).is_none(),
            "the deleted metadata file leaves the view's membership"
        );
        assert_eq!(
            base_db::SourceDatabase::file_text(view.database(), FileId(0)).as_ref(),
            "open source overlay"
        );
    }

    #[test]
    fn delta_catch_up_rejects_a_gap_without_publishing_a_mixed_generation() {
        let (_dir, mut backend, path) = disk_backend();
        let mut view = EditorView::new(backend.snapshot());
        let first = backend
            .apply_source_batch(
                vec![SourceBatchChange::Upsert {
                    file_id: FileId(0),
                    path: VfsPath::new(path.clone()),
                    source: Some(BatchTextSource::DiskOwned("first".to_owned())),
                }],
                |_, _| false,
            )
            .unwrap();
        let second = backend
            .apply_source_batch(
                vec![SourceBatchChange::Upsert {
                    file_id: FileId(0),
                    path: VfsPath::new(path),
                    source: Some(BatchTextSource::DiskOwned("second".to_owned())),
                }],
                |_, _| false,
            )
            .unwrap();

        assert!(!view.catch_up_deltas(backend.snapshot(), &[second]));
        assert_eq!(view.generation(), first.generation - 1);
        assert_eq!(base_db::SourceDatabase::file_text(view.database(), FileId(0)).as_ref(), "disk");
    }

    #[test]
    fn prepared_replacement_keeps_latest_open_text_and_rejects_stale_storage() {
        let (_dir, backend, path) = disk_backend();
        let mut active = EditorView::new(backend.snapshot());
        active.open(FileId(0), path.clone(), Arc::from("first edit"));
        let mut prepared = EditorView::new(backend.snapshot());
        active.change(FileId(0), Arc::from("latest edit"));

        assert!(active.replace_from_prepared(prepared));
        assert_eq!(
            base_db::SourceDatabase::file_text(active.database(), FileId(0)).as_ref(),
            "latest edit"
        );

        let mut stale_snapshot = backend.snapshot();
        stale_snapshot.generation = 0;
        prepared = EditorView::new(stale_snapshot);
        assert!(!active.replace_from_prepared(prepared));
        assert_eq!(
            base_db::SourceDatabase::file_text(active.database(), FileId(0)).as_ref(),
            "latest edit"
        );
    }

    #[test]
    fn seed_uses_prepared_revision_without_reading_the_file() {
        let db = RootDatabaseImpl::new();
        let mut files = FileSet::new();
        files.insert(FileId(0), VfsPath::new(PathBuf::from("/missing/source.bsl")));
        let mut db = db;
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(files));
        db.set_file_source_root(FileId(0), BSL_SOURCE_ROOT);
        db.set_file_revision_from_disk(FileId(0), 42);
        let root = PathBuf::from("/workspace/configuration");
        db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot::from_paths(vec![(
            None,
            root.clone(),
        )]));
        db.set_metadata_listing(&root.to_string_lossy(), MetadataListingData::default());
        let snapshot = WorkspaceSnapshot::capture(&db, 7);
        assert_eq!(snapshot.metadata.len(), 1);
        let owner_entries = Arc::clone(&snapshot.metadata[0].1.entries);
        let view = EditorView::prepare(snapshot, || false).unwrap();
        assert_eq!(view.generation(), 7);
        assert!(base_db::SourceDatabase::try_file_text_input(view.database(), FileId(0)).is_none());
        assert_eq!(
            base_db::SourceDatabase::try_file_revision_input(view.database(), FileId(0))
                .unwrap()
                .revision(view.database()),
            42
        );
        assert_eq!(
            view.database().metadata_listing_data(&root.to_string_lossy()),
            Some(MetadataListingData::default())
        );
        let view_listing =
            view.database().metadata_listing_snapshot(&root.to_string_lossy()).unwrap();
        assert!(Arc::ptr_eq(&owner_entries, &view_listing.entries));
    }

    #[test]
    fn no_op_batches_do_not_publish_and_journal_overflow_reseeds() {
        let mut backend = WorkspaceBackend::new(AnalysisHost::default());
        assert!(backend.apply_source_batch(Vec::new(), |_, _| false).is_none());
        for index in 0..(JOURNAL_CAPACITY + 1) {
            let file_id = FileId(index as u32);
            let path = PathBuf::from(format!("/workspace/{index}.bsl"));
            let _ = backend.register_path(path.clone(), file_id);
            backend.apply_source_batch(
                vec![SourceBatchChange::Upsert {
                    file_id,
                    path: VfsPath::new(path.clone()),
                    source: Some(BatchTextSource::DiskOwned("x".to_owned())),
                }],
                |_, _| false,
            );
            if index == 0 {
                let generation = backend.snapshot().generation;
                assert!(backend
                    .apply_source_batch(
                        vec![SourceBatchChange::Upsert {
                            file_id,
                            path: VfsPath::new(path),
                            source: Some(BatchTextSource::DiskOwned("x".to_owned())),
                        }],
                        |_, _| false,
                    )
                    .is_none());
                assert_eq!(backend.snapshot().generation, generation);
            }
        }
        assert!(matches!(backend.changes_since(0), CatchUp::Reseed(_)));
    }

    #[test]
    fn replacement_rebuild_advances_from_previous_generation_and_forces_reseed() {
        let mut replacement = WorkspaceBackend::new(AnalysisHost::default());
        assert_eq!(replacement.publish_full_rebuild_from(41), 42);
        assert!(matches!(replacement.changes_since(41), CatchUp::Reseed(_)));
        assert!(matches!(replacement.changes_since(42), CatchUp::Current));
    }

    #[test]
    fn shared_constructor_does_not_publish_catalog_until_accepted() {
        let (_dir, backend, path) = disk_backend();
        let catalog = backend.catalog();
        let before = catalog.files().into_iter().find(|file| file.file_id == FileId(0)).unwrap();

        let mut db = RootDatabaseImpl::new();
        let mut files = FileSet::new();
        files.insert(FileId(0), VfsPath::new(path.clone()));
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(files));
        db.set_file_source_root(FileId(0), BSL_SOURCE_ROOT);
        db.set_file_revision_from_disk(FileId(0), 99);
        let mut candidate = WorkspaceBackend::from_database_with_catalog(db, catalog.clone());

        assert_eq!(
            catalog.files().into_iter().find(|file| file.file_id == FileId(0)).unwrap().state,
            before.state,
            "building a rejected candidate cannot change the published catalog"
        );
        assert_eq!(candidate.publish_full_rebuild_from(7), 8);
        assert!(matches!(
            catalog.files().into_iter().find(|file| file.file_id == FileId(0)).unwrap().state,
            DiskFileState::Readable { revision: 99 }
        ));
    }

    /// `bsl-analyzer` mirrors the shared catalog into its own VFS by walking the catalog
    /// in id order and allocating each path locally. That mirror is only correct when the
    /// catalog lists every id the shared VFS has handed out, so a gap is where it diverges.
    fn assert_catalog_lists_every_allocated_id(catalog: &WorkspaceCatalog) {
        let allocated = catalog.with_vfs(|vfs| vfs.num_file_ids());
        let mut listed: Vec<u32> = catalog.files().iter().map(|file| file.file_id.0).collect();
        listed.sort_unstable();
        assert_eq!(listed, (0..allocated).collect::<Vec<u32>>());
    }

    #[test]
    fn rejected_candidate_metadata_id_stays_catalogued() {
        let catalog = WorkspaceCatalog::new();
        assert_eq!(catalog.intern_path(PathBuf::from("/ws/Module.bsl")), FileId(0));
        // The candidate interns its new source file, then its metadata bootstrap allocates a
        // composing XML id directly in the shared VFS. The candidate is rejected, so no
        // publication ever lists that XML id.
        let _candidate_source = catalog.intern_path(PathBuf::from("/ws/New.bsl"));
        let _candidate_metadata = catalog
            .with_vfs(|vfs| vfs.alloc_file_id(VfsPath::new(PathBuf::from("/ws/Catalog.xml"))));
        assert_eq!(catalog.intern_path(PathBuf::from("/ws/Extra.bsl")), FileId(3));

        assert_catalog_lists_every_allocated_id(&catalog);
    }

    #[test]
    fn uncatalogued_metadata_id_survives_a_successful_publish_once_its_file_is_gone() {
        let catalog = WorkspaceCatalog::new();
        let _module = catalog.intern_path(PathBuf::from("/ws/Module.bsl"));
        let _metadata = catalog
            .with_vfs(|vfs| vfs.alloc_file_id(VfsPath::new(PathBuf::from("/ws/Catalog.xml"))));
        // The XML file is gone before the next candidate, which is accepted without it. Nothing
        // in that publication refers to the id, so it must not stay a gap in the catalog.
        let mut next = WorkspaceBackend::with_catalog(AnalysisHost::default(), catalog.clone());
        assert_eq!(next.publish_full_rebuild_from(0), 1);

        assert_catalog_lists_every_allocated_id(&catalog);
    }

    #[test]
    fn mismatched_register_path_does_not_leave_an_uncatalogued_id() {
        let catalog = WorkspaceCatalog::new();
        let _module = catalog.intern_path(PathBuf::from("/ws/Module.bsl"));
        assert_eq!(
            catalog.register_path(PathBuf::from("/ws/Other.bsl"), FileId(5)),
            Err(FileId(1))
        );

        assert_catalog_lists_every_allocated_id(&catalog);
    }

    #[test]
    fn unpublished_candidate_path_is_catalogued_deleted_and_not_served() {
        let (_dir, backend, path) = disk_backend();
        let catalog = backend.catalog();
        let generation = backend.snapshot().generation;
        let candidate_path = path.with_file_name("New.bsl");
        let candidate_id = catalog.intern_path(candidate_path.clone());

        let snapshot = backend.snapshot();
        let pending = snapshot
            .catalog
            .iter()
            .find(|file| file.path == candidate_path)
            .expect("an interned path is listed by the catalog");
        assert_eq!(pending.file_id, candidate_id);
        assert_eq!(pending.state, DiskFileState::Deleted);
        assert_eq!(snapshot.generation, generation);
        assert!(base_db::SourceDatabase::try_file_revision_input(backend.database(), candidate_id)
            .is_none());
    }
}
