//! The analysis host now lives in the shared [`ide_host_core`] crate so the LSP
//! server, the MCP server, and the CLI drive one host with one memory model. This
//! re-export keeps the existing `crate::analysis_host::AnalysisHost` path stable.

pub use ide_host_core::AnalysisHost;

use ide::{Analysis, RootDatabaseImpl};
use ide_host_core::{CatchUp, EditorView, WorkspaceSnapshot};
use salsa::Database as _;
use std::path::PathBuf;

/// Storage used by one LSP session. Standalone sessions keep the traditional host;
/// shared sessions own an independent editor storage from the moment the owner has a
/// workspace to seed it with, and borrow the owner's disk analysis only before that.
pub enum SessionAnalysisHost {
    Standalone(AnalysisHost),
    Shared {
        fallback: AnalysisHost,
        owner: Box<mcp_server::WorkspaceHandle>,
        view: Option<Box<EditorView>>,
    },
}

impl Default for SessionAnalysisHost {
    fn default() -> Self {
        Self::Standalone(AnalysisHost::default())
    }
}

impl SessionAnalysisHost {
    pub fn shared(owner: mcp_server::WorkspaceHandle) -> Self {
        Self::Shared { fallback: AnalysisHost::default(), owner: Box::new(owner), view: None }
    }

    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Shared { .. })
    }

    pub fn owner(&self) -> Option<&mcp_server::WorkspaceHandle> {
        match self {
            Self::Standalone(_) => None,
            Self::Shared { owner, .. } => Some(owner),
        }
    }

    pub fn view(&self) -> Option<&EditorView> {
        match self {
            Self::Standalone(_) => None,
            Self::Shared { view, .. } => view.as_deref(),
        }
    }

    pub fn view_mut(&mut self) -> Option<&mut EditorView> {
        match self {
            Self::Standalone(_) => None,
            Self::Shared { view, .. } => view.as_deref_mut(),
        }
    }

    pub fn ensure_view(&mut self, snapshot: WorkspaceSnapshot) {
        if let Self::Shared { view, .. } = self {
            view.get_or_insert_with(|| Box::new(EditorView::new(snapshot)));
        }
    }

    pub fn prepare_view(&mut self) {
        if !self.is_shared() || self.view().is_some() {
            return;
        }
        let snapshot = WorkspaceSnapshot::capture(&RootDatabaseImpl::new(), 0);
        self.ensure_view(snapshot);
    }

    pub fn catch_up(&mut self) -> bool {
        let result = match self.owner() {
            Some(owner) => match owner.catch_up(self.view().map_or(0, EditorView::generation)) {
                mcp_server::WorkspaceRead::Ready(result) => result,
                _ => return false,
            },
            None => return false,
        };
        match result {
            CatchUp::Current => false,
            CatchUp::Changes(changes) => {
                let expected_generation = changes.last().map_or_else(
                    || self.view().map_or(0, EditorView::generation),
                    |change| change.generation,
                );
                let Some(snapshot) = self.owner().and_then(|owner| match owner.snapshot() {
                    mcp_server::WorkspaceRead::Ready(snapshot) => Some(snapshot),
                    _ => None,
                }) else {
                    return false;
                };
                if snapshot.generation != expected_generation {
                    // The owner published between catch_up and snapshot. Do not
                    // stamp this view ahead of changes it has not applied.
                    return false;
                }
                if let Some(view) = self.view_mut() {
                    view.catch_up_deltas(snapshot, &changes)
                } else {
                    false
                }
            }
            CatchUp::Reseed(_) => false,
        }
    }

    pub fn take_view(&mut self) -> Option<EditorView> {
        match self {
            Self::Standalone(_) => None,
            Self::Shared { view, .. } => view.take().map(|view| *view),
        }
    }

    pub fn analysis(&self) -> Analysis {
        match self {
            Self::Standalone(host) => host.analysis(),
            Self::Shared { fallback, owner, view } => view.as_deref().map_or_else(
                || match owner.request_analysis() {
                    mcp_server::WorkspaceRead::Ready(analysis) => analysis,
                    _ => fallback.analysis(),
                },
                EditorView::analysis,
            ),
        }
    }

    /// [`Self::analysis`] for answering a request, or `None` while a shared session without
    /// an editor view has no published owner analysis: the empty fallback store would answer
    /// as an empty workspace instead of as one that is not ready.
    pub fn serving_analysis(&self) -> Option<Analysis> {
        match self {
            Self::Shared { owner, view: None, .. } => match owner.request_analysis() {
                mcp_server::WorkspaceRead::Ready(analysis) => Some(analysis),
                _ => None,
            },
            _ => Some(self.analysis()),
        }
    }

    /// The revision of the store [`Self::analysis`] reads, which is the store a request takes
    /// its own revision from. Without an editor view that is the owner's database, not the
    /// fallback [`Self::raw_database`] returns, and the two count revisions independently.
    pub fn request_revision(&self) -> salsa::Revision {
        salsa::plumbing::current_revision(self.analysis().database())
    }

    pub fn raw_database(&self) -> &RootDatabaseImpl {
        match self {
            Self::Standalone(host) => host.raw_database(),
            Self::Shared { fallback, view, .. } => {
                view.as_deref().map_or_else(|| fallback.raw_database(), EditorView::database)
            }
        }
    }

    pub fn raw_database_mut(&mut self) -> &mut RootDatabaseImpl {
        match self {
            Self::Standalone(host) => host.raw_database_mut(),
            Self::Shared { fallback, view, .. } => view
                .as_deref_mut()
                .map_or_else(|| fallback.raw_database_mut(), EditorView::database_mut),
        }
    }

    pub fn request_cancellation(&mut self) {
        match self {
            Self::Standalone(host) => host.request_cancellation(),
            Self::Shared { fallback, view, .. } => {
                if let Some(view) = view {
                    view.database_mut().trigger_cancellation();
                } else {
                    fallback.request_cancellation();
                }
            }
        }
    }

    pub fn enforce_lru(&mut self, deep: bool) {
        if let Self::Shared { owner, view: None, .. } = self {
            if !matches!(owner.enforce_lru(deep), mcp_server::WorkspaceRead::Ready(())) {
                tracing::debug!("shared disk owner is unavailable for batch trim");
            }
            return;
        }
        let db = self.raw_database_mut();
        if deep {
            ide::sweep_lru_deep(db);
        } else {
            db.enforce_lru();
        }
    }

    pub fn bootstrap_metadata_substrate(&mut self, vfs: &impl ide_host_core::VfsWrite) {
        if let Self::Standalone(host) = self {
            host.bootstrap_metadata_substrate(vfs);
        }
    }

    pub fn refresh_metadata_substrate(
        &mut self,
        vfs: &impl ide_host_core::VfsWrite,
        changed_paths: &[PathBuf],
    ) -> bool {
        match self {
            Self::Standalone(host) => host.refresh_metadata_substrate(vfs, changed_paths),
            Self::Shared { .. } => false,
        }
    }
}
