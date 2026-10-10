use std::path::{Path, PathBuf};
use std::sync::Arc;

use base_db::DiagnosticsConfigInput;
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
}

impl FrozenFilePaths {
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
        Self { forward: Arc::new(forward), reverse: Arc::new(reverse) }
    }

    pub fn file_id_for_url(&self, url: &Url) -> anyhow::Result<FileId> {
        let path = url.to_file_path().map_err(|_| anyhow::anyhow!("Invalid file URL: {}", url))?;
        if !project_model::is_bsl_source_path(&path) {
            return Err(anyhow::anyhow!("File is not BSL, request unsupported: {}", url));
        }
        self.forward
            .get(&path)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("File not in frozen VFS snapshot: {}", url))
    }

    pub fn url_for_file_id(&self, file_id: FileId) -> anyhow::Result<Url> {
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
}
