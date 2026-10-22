use std::path::{Path, PathBuf};
use std::sync::Arc;

use base_db::{DiagnosticsConfigInput, SourceDatabase};
use crossbeam_channel::Sender;
use ide::Analysis;
use lsp_server::Message;
use lsp_types::Url;
use project_model::Project;
use rustc_hash::FxHashMap;
use vfs::{FileId, Vfs};

use crate::call_hierarchy_index_state::CallHierarchyIndexState;
use crate::global_state::{CallHierarchyWaitPolicy, Task};
use crate::lsp::to_proto::ClientTags;
use crate::lsp::PositionEncoding;
use crate::mem_docs::FrozenMemDocs;

#[derive(Debug, Clone, Default)]
pub struct FrozenFilePaths {
    forward: Arc<FxHashMap<PathBuf, FileId>>,
    reverse: Arc<FxHashMap<FileId, PathBuf>>,
    open_uris: Arc<FxHashMap<Url, FileId>>,
    open_urls: Arc<FxHashMap<FileId, Url>>,
}

impl FrozenFilePaths {
    /// Paths of every file the database holds, BSL and metadata XML alike: answers
    /// place metadata objects in their XML, so a snapshot without the metadata root
    /// cannot locate them.
    pub fn freeze_database(db: &ide::RootDatabaseImpl) -> Self {
        let mut forward = FxHashMap::default();
        let mut reverse = FxHashMap::default();
        for id in [base_db::BSL_SOURCE_ROOT, base_db::METADATA_SOURCE_ROOT] {
            let Some(input) = db.try_source_root_input(id) else { continue };
            let root = input.root(db);
            for file_id in root.iter() {
                let Some(path) = root.file_set().path_for_file(&file_id) else { continue };
                let path = path.as_path().to_path_buf();
                forward.insert(path.clone(), file_id);
                reverse.insert(file_id, path);
            }
        }
        Self { forward: Arc::new(forward), reverse: Arc::new(reverse), ..Self::default() }
    }

    pub fn freeze(vfs: &Vfs) -> Self {
        let n = vfs.num_file_ids();
        let mut forward: FxHashMap<PathBuf, FileId> = FxHashMap::default();
        let mut reverse: FxHashMap<FileId, PathBuf> = FxHashMap::default();
        for i in 0..n {
            let file_id = FileId(i);
            if !vfs.exists(file_id) {
                continue;
            }
            let path = vfs.file_path(file_id).as_path().to_path_buf();
            forward.insert(path.clone(), file_id);
            reverse.insert(file_id, path);
        }
        Self { forward: Arc::new(forward), reverse: Arc::new(reverse), ..Self::default() }
    }

    /// Adds the exact URI spellings of open documents without changing their canonical paths.
    pub fn with_open_uri_ids(mut self, bindings: impl IntoIterator<Item = (Url, FileId)>) -> Self {
        let mut open_uris = FxHashMap::default();
        let mut open_urls: FxHashMap<FileId, Option<Url>> = FxHashMap::default();
        for (uri, file_id) in bindings {
            open_uris.insert(uri.clone(), file_id);
            match open_urls.entry(file_id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Some(uri));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.insert(None);
                }
            }
        }
        self.open_uris = Arc::new(open_uris);
        self.open_urls = Arc::new(
            open_urls
                .into_iter()
                .filter_map(|(file_id, uri)| uri.map(|uri| (file_id, uri)))
                .collect(),
        );
        self
    }

    pub fn file_id_for_url(&self, url: &Url) -> anyhow::Result<FileId> {
        let path = url.to_file_path().map_err(|_| anyhow::anyhow!("Invalid file URL: {}", url))?;
        if !project_model::is_bsl_source_path(&path) {
            return Err(anyhow::anyhow!("File is not BSL, request unsupported: {}", url));
        }
        self.open_uris
            .get(url)
            .or_else(|| self.forward.get(&path))
            .or_else(|| self.forward.get(&crate::workspace::canonicalize_shared_path(&path)))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("File not in frozen VFS snapshot: {}", url))
    }

    pub fn url_for_file_id(&self, file_id: FileId) -> anyhow::Result<Url> {
        if let Some(url) = self.open_urls.get(&file_id) {
            return Ok(url.clone());
        }
        let path = self
            .reverse
            .get(&file_id)
            .ok_or_else(|| anyhow::anyhow!("FileId {} not in frozen VFS snapshot", file_id.0))?;
        Url::from_file_path(path)
            .map_err(|_| anyhow::anyhow!("Failed to convert path to URL: {:?}", path))
    }

    pub fn path_for_file_id(&self, file_id: FileId) -> Option<&Path> {
        self.reverse.get(&file_id).map(PathBuf::as_path)
    }

    pub fn len(&self) -> usize {
        self.reverse.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reverse.is_empty()
    }

    /// All files in the frozen snapshot as `(FileId, path)` pairs. Used by the
    /// `workspace/diagnostic` sweep to enumerate its in-scope file set.
    pub fn iter(&self) -> impl Iterator<Item = (FileId, &std::path::Path)> {
        self.reverse.iter().map(|(&file_id, path)| (file_id, path.as_path()))
    }
}

pub struct LatencyRequestContext {
    pub analysis: Analysis,
    pub workspace_root: Option<PathBuf>,
    pub project: Option<Project>,
    pub diagnostics_baseline: Arc<ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot>,
    pub diagnostics_config: DiagnosticsConfigInput,
    pub position_encoding: PositionEncoding,
    /// Whether the client advertised support for `Diagnostic.codeDescription`, so the
    /// standard behind a rule may be attached as a property rather than only as the
    /// message suffix every client can render.
    pub supports_code_description: bool,
    /// Tags the client is willing to receive in `Diagnostic.tags`
    /// (`publishDiagnostics.tagSupport`), negotiated at `initialize`; `None`-like
    /// `ClientTags::NONE` means the property is not published at all.
    pub supports_diagnostic_tags: ClientTags,
    /// Whether the client advertised support for `InsertTextMode::ADJUST_INDENTATION`,
    /// so the completion handler may ask it to indent snippet continuation lines
    /// to the cursor column.
    pub supports_insert_text_mode_adjust_indentation: bool,
    /// Whether the client honors versioned `WorkspaceEdit.documentChanges`, so the
    /// rename handler can attach open-document versions to its edits.
    pub supports_workspace_edit_document_changes: bool,
    /// The client's `rangeLimit` for folding ranges, if it named one; an
    /// over-long `textDocument/foldingRange` response is truncated to it.
    pub folding_range_limit: Option<u32>,
    pub task_sender: Sender<Task>,
    pub call_hierarchy_index: CallHierarchyIndexState,
    pub call_hierarchy_wait_policy: CallHierarchyWaitPolicy,
    /// Direct channel to the client, so a long-running handler (the `workspace/diagnostic`
    /// sweep) can stream `$/progress` notifications — partial results and work-done progress —
    /// while it runs, instead of only returning one final response.
    pub client_sender: Sender<Message>,
    pub mem_docs: FrozenMemDocs,
    pub file_paths: FrozenFilePaths,
    /// Open documents edited since their last save; under a vendor-diff scope
    /// they count as whole-file in scope (the disk diff no longer matches).
    pub scope_dirty_docs: std::collections::HashSet<Url>,
}

impl LatencyRequestContext {
    pub fn file_id_for_url(&self, url: &Url) -> anyhow::Result<FileId> {
        self.file_paths.file_id_for_url(url)
    }

    pub fn url_for_file_id(&self, file_id: FileId) -> anyhow::Result<Url> {
        self.file_paths.url_for_file_id(file_id)
    }

    /// Emit a `$/progress` notification carrying an arbitrary payload (a work-done progress
    /// value or a partial-result literal) under the given client-supplied token.
    pub fn send_progress(&self, token: &lsp_types::ProgressToken, value: serde_json::Value) {
        let params = serde_json::json!({ "token": token, "value": value });
        let notification = lsp_server::Notification::new("$/progress".to_string(), params);
        self.client_sender.send(notification.into()).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::VfsPath;

    #[test]
    fn frozen_file_paths_roundtrip() {
        let mut vfs = Vfs::new();
        let path = crate::test_uri::file_path("tmp/test.bsl");
        vfs.set_file_contents(VfsPath::new(path.clone()), Some(Arc::from("content")));
        let file_id = vfs.file_id(&VfsPath::new(path.clone())).unwrap();

        let frozen = FrozenFilePaths::freeze(&vfs);

        let url = Url::from_file_path(&path).unwrap();
        assert_eq!(frozen.file_id_for_url(&url).unwrap(), file_id);
        assert_eq!(frozen.url_for_file_id(file_id).unwrap(), url);
        assert_eq!(frozen.len(), 1);
    }

    #[test]
    fn frozen_file_paths_survive_later_mutation() {
        let mut vfs = Vfs::new();
        let path_a = crate::test_uri::file_path("tmp/a.bsl");
        vfs.set_file_contents(VfsPath::new(path_a.clone()), Some(Arc::from("a")));
        let frozen = FrozenFilePaths::freeze(&vfs);

        let path_b = crate::test_uri::file_path("tmp/b.bsl");
        vfs.set_file_contents(VfsPath::new(path_b.clone()), Some(Arc::from("b")));

        let url_b = Url::from_file_path(&path_b).unwrap();
        assert!(
            frozen.file_id_for_url(&url_b).is_err(),
            "frozen snapshot must not see later-added files"
        );
        assert_eq!(frozen.len(), 1);
    }

    #[test]
    fn frozen_file_paths_excludes_deleted() {
        let mut vfs = Vfs::new();
        let path = crate::test_uri::file_path("tmp/gone.bsl");
        vfs.set_file_contents(VfsPath::new(path.clone()), Some(Arc::from("x")));
        vfs.set_file_contents(VfsPath::new(path.clone()), None);

        let frozen = FrozenFilePaths::freeze(&vfs);
        assert!(frozen.is_empty());
    }

    #[test]
    fn shared_paths_include_disk_members_without_a_session_vfs_copy() {
        let mut db = ide::RootDatabaseImpl::new();
        let disk_path = crate::test_uri::file_path("tmp/closed.bsl");
        let open_path = crate::test_uri::file_path("tmp/open.bsl");
        let mut files = vfs::file_set::FileSet::new();
        files.insert(FileId(4), VfsPath::new(disk_path.clone()));
        files.insert(FileId(9), VfsPath::new(open_path.clone()));
        db.set_source_root(base_db::BSL_SOURCE_ROOT, base_db::SourceRoot::new_local(files));
        let frozen = FrozenFilePaths::freeze_database(&db);
        assert_eq!(frozen.len(), 2);
        let uri = Url::from_file_path(&disk_path).unwrap();
        assert_eq!(frozen.file_id_for_url(&uri).unwrap(), FileId(4));
        assert_eq!(frozen.url_for_file_id(FileId(4)).unwrap(), uri);
        db.set_source_root(
            base_db::BSL_SOURCE_ROOT,
            base_db::SourceRoot::new_local(Default::default()),
        );
        assert_eq!(frozen.file_id_for_url(&uri).unwrap(), FileId(4));
        assert!(FrozenFilePaths::freeze_database(&db).is_empty());
    }

    #[test]
    fn shared_admission_open_uri_alias_resolves_to_canonical_file_id_and_path() {
        let mut vfs = Vfs::new();
        let path = crate::test_uri::file_path("tmp/Canonical/Module.bsl");
        vfs.set_file_contents(VfsPath::new(path.clone()), Some(Arc::from("content")));
        let file_id = vfs.file_id(&VfsPath::new(path.clone())).unwrap();
        let alias_path = path.parent().unwrap().parent().unwrap().join("cANONICAL/mODULE.bSL");
        let alias_uri = Url::from_file_path(alias_path).unwrap();

        let frozen = FrozenFilePaths::freeze(&vfs);
        assert!(frozen.file_id_for_url(&alias_uri).is_err());
        let frozen = frozen.with_open_uri_ids([(alias_uri.clone(), file_id)]);
        assert_eq!(frozen.file_id_for_url(&alias_uri).unwrap(), file_id);
        assert_eq!(frozen.url_for_file_id(file_id).unwrap(), alias_uri);
        assert_eq!(frozen.path_for_file_id(file_id), Some(path.as_path()));

        let canonical_uri = Url::from_file_path(&path).unwrap();
        let ambiguous = frozen
            .with_open_uri_ids([(alias_uri.clone(), file_id), (canonical_uri.clone(), file_id)]);
        assert_eq!(ambiguous.file_id_for_url(&alias_uri).unwrap(), file_id);
        assert_eq!(ambiguous.url_for_file_id(file_id).unwrap(), canonical_uri);
        assert_eq!(ambiguous.path_for_file_id(file_id), Some(path.as_path()));
    }
}
