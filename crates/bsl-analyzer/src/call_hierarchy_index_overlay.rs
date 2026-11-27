use std::path::PathBuf;
use std::sync::Arc;

use base_db::{FrozenFileContentSnapshot, SourceDatabase, SourceRoot, SourceRootId};
use ide::{GraphConfigCache, ModuleId, RootDatabaseImpl};
use lsp_types::Url;
use rustc_hash::FxHashMap;
use salsa::Database as _;
use vfs::FileId;

use crate::mem_docs::FrozenMemDocs;

#[derive(Debug, Clone)]
pub struct CallHierarchyIndexFrozenSnapshot {
    pub(crate) disk_read: ide_host_core::WorkspaceDiskRead,
    pub source_root_id: SourceRootId,
    pub file_set: Arc<FxHashMap<FileId, PathBuf>>,
    pub disk_revisions: Arc<FxHashMap<FileId, u64>>,
    pub open_texts: Arc<FxHashMap<FileId, Arc<str>>>,
    /// The workspace's roots as registered — kinds and closures included. A
    /// flat label list would re-read an external object as an extension.
    pub configs: Arc<ide::WorkspaceConfigsSnapshot>,
    pub creation_generation: u64,
    source_root: SourceRoot,
    file_content: Arc<FrozenFileContentSnapshot>,
    config_cache: Arc<GraphConfigCache>,
}

impl CallHierarchyIndexFrozenSnapshot {
    pub fn capture(
        db: &RootDatabaseImpl,
        source_root_id: SourceRootId,
        mem_docs: &FrozenMemDocs,
        creation_generation: u64,
        disk_read: ide_host_core::WorkspaceDiskRead,
    ) -> Self {
        let source_root = db.source_root_input(source_root_id).root(db).clone();
        let file_set: FxHashMap<_, _> = source_root
            .iter()
            .filter_map(|file_id| {
                source_root
                    .file_set()
                    .path_for_file(&file_id)
                    .filter(|path| project_model::is_bsl_source_path(path.as_path()))
                    .map(|path| (file_id, path.as_path().to_path_buf()))
            })
            .collect();
        let open_texts = Self::open_texts(&file_set, mem_docs);
        let disk_revisions: FxHashMap<_, _> = file_set
            .keys()
            .map(|&file_id| (file_id, db.file_revision_input(file_id).revision(db)))
            .collect();
        let file_content = Arc::new(FrozenFileContentSnapshot::new(
            Arc::new(disk_revisions.clone()),
            Arc::new(open_texts.clone()),
        ));
        Self {
            disk_read,
            source_root_id,
            file_set: Arc::new(file_set),
            disk_revisions: Arc::new(disk_revisions),
            open_texts: Arc::new(open_texts),
            configs: db.workspace_configs_snapshot(),
            creation_generation,
            source_root,
            file_content,
            config_cache: Arc::new(GraphConfigCache::default()),
        }
    }

    pub(crate) fn materialize(&self) -> Self {
        self.materialize_with_open_texts(Arc::clone(&self.open_texts))
    }

    pub fn refresh(&self, mem_docs: &FrozenMemDocs) -> Self {
        self.materialize_with_open_texts(Arc::new(Self::open_texts(&self.file_set, mem_docs)))
    }

    fn materialize_with_open_texts(&self, open_texts: Arc<FxHashMap<FileId, Arc<str>>>) -> Self {
        let file_content = Arc::new(FrozenFileContentSnapshot::new(
            Arc::clone(&self.disk_revisions),
            Arc::clone(&open_texts),
        ));
        Self {
            disk_read: self.disk_read.clone(),
            source_root_id: self.source_root_id,
            file_set: Arc::clone(&self.file_set),
            disk_revisions: Arc::clone(&self.disk_revisions),
            open_texts,
            configs: Arc::clone(&self.configs),
            creation_generation: self.creation_generation,
            source_root: self.source_root.clone(),
            file_content,
            config_cache: Arc::clone(&self.config_cache),
        }
    }

    fn open_texts(
        file_set: &FxHashMap<FileId, PathBuf>,
        mem_docs: &FrozenMemDocs,
    ) -> FxHashMap<FileId, Arc<str>> {
        file_set
            .iter()
            .filter_map(|(&file_id, path)| {
                let url = Url::from_file_path(path).ok()?;
                let document = mem_docs.get(&url)?;
                Some((file_id, Arc::from(document.text())))
            })
            .collect()
    }

    pub fn modules(&self) -> Vec<ModuleId> {
        let mut modules: Vec<_> = self.file_set.keys().copied().map(ModuleId::new).collect();
        modules.sort_unstable_by_key(|module| module.file_id);
        modules
    }

    pub fn open_batch(&self, batch: &[ModuleId]) -> RootDatabaseImpl {
        let mut db = RootDatabaseImpl::default();
        self.disk_read.register_cancel(db.cancellation_token());
        db.set_graph_config_cache(Arc::clone(&self.config_cache));
        db.set_source_root(self.source_root_id, self.source_root.clone());
        db.set_file_source_root_fallback(self.source_root_id);
        db.set_frozen_file_content(Arc::clone(&self.file_content));
        db.set_workspace_configs_snapshot((*self.configs).clone());

        let batch_files = self.register_files(&mut db, batch.iter().map(|module| module.file_id));
        ide::warm_batch_config_roots(&db, &batch_files);
        db
    }

    fn register_files(
        &self,
        db: &mut RootDatabaseImpl,
        file_ids: impl IntoIterator<Item = FileId>,
    ) -> Vec<(FileId, PathBuf)> {
        let mut files = Vec::new();
        for file_id in file_ids {
            let Some(path) = self.file_set.get(&file_id) else { continue };
            db.set_file_source_root(file_id, self.source_root_id);
            if let Some(text) = self.open_texts.get(&file_id) {
                db.set_file_text(file_id, text.as_ref());
            } else if let Some(revision) = self.disk_revisions.get(&file_id) {
                db.set_file_revision_from_disk(file_id, *revision);
            } else {
                // The snapshot has neither an open buffer nor a disk revision for
                // this file, so its content is unknown rather than unreadable.
                db.set_file_text(file_id, "");
            }
            files.push((file_id, path.clone()));
        }
        files
    }

    pub fn freeze_config_inputs(&self) {
        let mut representatives = Vec::new();
        for (_, config_root) in self.configs.paths.iter() {
            let root = config_root.canonicalize().unwrap_or_else(|_| config_root.clone());
            if let Some(file_id) = self
                .file_set
                .iter()
                .find_map(|(&file_id, path)| path.starts_with(&root).then_some(file_id))
            {
                representatives.push(ModuleId::new(file_id));
            }
        }
        representatives.sort_unstable_by_key(|module| module.file_id);
        representatives.dedup();
        if representatives.is_empty() {
            return;
        }
        drop(self.open_batch(&representatives));
    }
}

#[cfg(test)]
#[path = "call_hierarchy_index_overlay_tests.rs"]
mod tests;
