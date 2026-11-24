//! Public in-process adapter for clients that share the MCP workspace owner.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ide_host_core::{CatchUp, WorkspaceCatalog, WorkspaceSnapshot};

use crate::diagnostics_state::{lock_recover, DiagnosticsState, DiagnosticsStatus};

/// Whether the disk owner can currently serve a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceReadiness {
    Disabled,
    Loading,
    Ready {
        generation: u64,
    },
    /// Project inputs are invalid; the last accepted snapshot is read-only.
    Frozen {
        generation: u64,
        error: String,
    },
    RejectedScope(String),
    Failed(String),
}

/// Result of a read against the current disk owner.
pub enum WorkspaceRead<T> {
    Ready(T),
    Loading,
    /// A resident read or replacement is in flight, so no stable snapshot can be returned.
    Busy,
    RejectedScope(String),
    Disabled,
    Failed(String),
}

struct HandleLease(Arc<AtomicUsize>);

impl HandleLease {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::AcqRel);
        Self(active)
    }
}

impl Drop for HandleLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Cloneable MCP-facing handle to a workspace's shared disk owner.
///
/// The catalog is available while metadata loading runs. Each returned `Analysis` is
/// a request-owned clone of the owner's Salsa storage; callers must drop it when that
/// request ends. Holding this handle prevents idle eviction while an LSP view is live.
#[derive(Clone)]
pub struct WorkspaceHandle {
    diagnostics: DiagnosticsState,
    _lease: Arc<HandleLease>,
}

impl WorkspaceHandle {
    pub(crate) fn new(diagnostics: DiagnosticsState) -> Self {
        let lease = Arc::new(HandleLease::new(Arc::clone(&diagnostics.active_workspace_handles)));
        diagnostics.mark_in_use();
        diagnostics.ensure_loading();
        Self { diagnostics, _lease: lease }
    }

    /// The canonical path interner is available before the owner is ready.
    pub fn catalog(&self) -> WorkspaceCatalog {
        self.diagnostics.catalog.clone()
    }

    /// Admission shared by disk readers, editor requests, and metadata filesystem writes.
    pub fn disk_gate(&self) -> ide_host_core::WorkspaceDiskGate {
        self.diagnostics.disk_gate.clone()
    }

    pub fn readiness(&self) -> WorkspaceReadiness {
        if let Some((generation, error)) = self.diagnostics.invalid_configuration() {
            return WorkspaceReadiness::Frozen { generation, error };
        }
        match self.diagnostics.status() {
            DiagnosticsStatus::Disabled => WorkspaceReadiness::Disabled,
            DiagnosticsStatus::Idle | DiagnosticsStatus::Loading => WorkspaceReadiness::Loading,
            DiagnosticsStatus::Ready { .. } => self
                .diagnostics
                .workspace_generation()
                .map(|generation| WorkspaceReadiness::Ready { generation })
                .unwrap_or(WorkspaceReadiness::Loading),
            DiagnosticsStatus::Failed(message) if message.starts_with("invalid project at ") => {
                WorkspaceReadiness::RejectedScope(message)
            }
            DiagnosticsStatus::Failed(message) => WorkspaceReadiness::Failed(message),
        }
    }

    /// A storage-neutral, immutable view of the currently published disk owner.
    pub fn snapshot(&self) -> WorkspaceRead<WorkspaceSnapshot> {
        self.diagnostics.workspace_snapshot().map_or_else(read_error, WorkspaceRead::Ready)
    }

    /// Rescan terminal disk state while the caller retains exclusive filesystem ownership.
    pub fn publish_disk_transaction(
        &self,
        writer: &ide_host_core::WorkspaceDiskWrite,
    ) -> Result<WorkspaceSnapshot, String> {
        self.diagnostics.publish_disk_transaction(writer)
    }

    /// Repeat the terminal wake after reopening read admission, for views that woke while frozen.
    pub fn notify_disk_transaction_finished(&self) {
        if let Ok(generation) = self.diagnostics.workspace_generation() {
            notify_generation(&self.diagnostics, generation);
        }
    }

    /// Refresh one already-admitted saved disk path through the resident's normal source
    /// batch, then return the accepted owner snapshot. Non-members return the current snapshot
    /// unchanged; this method never admits a path from an editor buffer or an excluded root.
    pub fn refresh_saved_path(&self, path: &std::path::Path) -> WorkspaceRead<WorkspaceSnapshot> {
        self.diagnostics.refresh_saved_path(path)
    }

    /// Submit a watched/config/save path hint to the current disk owner. The path is checked
    /// against the accepted project roots and exclusions; the owner classifies it from current
    /// disk state, so the event's source type cannot turn a live file into a deletion.
    /// Returns false when the owner is not ready or the path is outside its accepted scope.
    pub fn notify_changed_path(&self, path: &std::path::Path) -> bool {
        self.diagnostics.notify_changed_path(path)
    }

    /// The bootstrap-validated project, available during loading. Once the resident is
    /// ready this returns the project accepted with that resident generation.
    pub fn project(&self) -> WorkspaceRead<project_model::Project> {
        self.diagnostics.workspace_project().map_or_else(read_error, WorkspaceRead::Ready)
    }

    /// The validated `[source].exclude` set, available before the disk seed finishes.
    pub fn source_exclusions(&self) -> WorkspaceRead<project_model::ExcludedPaths> {
        self.project().map_ready(|project| project.source_exclusions().clone())
    }

    /// Return bounded source changes since `generation`, or a full seed when the
    /// journal no longer covers that point.
    pub fn catch_up(&self, generation: u64) -> WorkspaceRead<CatchUp> {
        self.diagnostics
            .workspace_catch_up(generation)
            .map_or_else(read_error, WorkspaceRead::Ready)
    }

    /// Clone the current disk `Analysis` under the resident lock. The lock is released
    /// before this method returns; the caller owns the clone only for that request.
    pub fn request_analysis(&self) -> WorkspaceRead<ide::Analysis> {
        if self.diagnostics.disk_gate.is_frozen() {
            if self.diagnostics.disk_gate.recovery_required() {
                return WorkspaceRead::Failed(
                    crate::diagnostics_state::DISK_RECOVERY_REQUIRED.to_owned(),
                );
            }
            return WorkspaceRead::Busy;
        }
        self.diagnostics.mark_in_use();
        self.diagnostics.workspace_analysis().map_or_else(read_error, WorkspaceRead::Ready)
    }

    /// Trim the resident's memo caches under the owner's FIFO admission lock.
    /// This changes no source inputs and does not publish a workspace generation.
    pub fn enforce_lru(&self, deep: bool) -> WorkspaceRead<()> {
        self.diagnostics.enforce_lru(deep)
    }

    /// Request a change-hub rescan before the next owner read. The existing storm guard
    /// controls how often a full walk can be requested.
    pub fn request_update(&self) -> Option<std::time::Duration> {
        self.diagnostics.mark_in_use();
        self.diagnostics.ensure_loading();
        self.diagnostics.force_rescan()
    }

    /// Receive generation wakeups. Consumers should call `catch_up` after each wake;
    /// notifications may coalesce while the bounded channel is full.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<u64> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        lock_recover(&self.diagnostics.generation_subscribers).push(sender);
        receiver
    }
}

fn read_error<T>(status: DiagnosticsStatus) -> WorkspaceRead<T> {
    match status {
        DiagnosticsStatus::Disabled => WorkspaceRead::Disabled,
        DiagnosticsStatus::Idle | DiagnosticsStatus::Loading => WorkspaceRead::Loading,
        DiagnosticsStatus::Failed(message) if message.starts_with("invalid project at ") => {
            WorkspaceRead::RejectedScope(message)
        }
        DiagnosticsStatus::Failed(message) => WorkspaceRead::Failed(message),
        DiagnosticsStatus::Ready { .. } => WorkspaceRead::Loading,
    }
}

impl<T> WorkspaceRead<T> {
    fn map_ready<U>(self, map: impl FnOnce(T) -> U) -> WorkspaceRead<U> {
        match self {
            Self::Ready(value) => WorkspaceRead::Ready(map(value)),
            Self::Loading => WorkspaceRead::Loading,
            Self::Busy => WorkspaceRead::Busy,
            Self::RejectedScope(message) => WorkspaceRead::RejectedScope(message),
            Self::Disabled => WorkspaceRead::Disabled,
            Self::Failed(message) => WorkspaceRead::Failed(message),
        }
    }
}

pub(crate) fn notify_generation(diagnostics: &DiagnosticsState, generation: u64) {
    let mut senders = lock_recover(&diagnostics.generation_subscribers);
    senders.retain(|sender| match sender.try_send(generation) {
        Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => true,
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_transaction_rejects_a_writer_from_another_owner() {
        use crate::diagnostics_state::test_support::{sample_workspace, wait_ready};

        let workspace = tempfile::tempdir().unwrap();
        sample_workspace(workspace.path());
        let state = DiagnosticsState::for_workspace(workspace.path().to_path_buf());
        let handle = WorkspaceHandle::new(state.clone());
        wait_ready(&state);
        let generation = state.generation();
        let unrelated = ide_host_core::WorkspaceDiskGate::default();
        let writer = unrelated.begin_write(std::time::Duration::from_secs(1), &|| false).unwrap();
        assert!(handle.publish_disk_transaction(&writer).is_err());
        assert_eq!(state.generation(), generation);
    }

    #[test]
    fn disk_transaction_hides_intermediate_bytes_and_publishes_one_generation() {
        use crate::diagnostics_state::test_support::{module_path, sample_workspace, wait_ready};

        let workspace = tempfile::tempdir().unwrap();
        sample_workspace(workspace.path());
        let state = DiagnosticsState::for_workspace(workspace.path().to_path_buf());
        let handle = WorkspaceHandle::new(state.clone());
        wait_ready(&state);
        let generation = state.generation();
        let path = module_path(workspace.path(), "Сервер");
        let gate = handle.disk_gate();
        let mut writer = gate.begin_write(std::time::Duration::from_secs(1), &|| false).unwrap();
        writer.mark_writing();
        std::fs::write(&path, "intermediate").unwrap();
        assert!(matches!(handle.refresh_saved_path(&path), WorkspaceRead::Busy));
        assert!(matches!(handle.request_analysis(), WorkspaceRead::Busy));
        state.poll_pending_drift();
        assert_eq!(state.generation(), generation);

        std::fs::write(&path, "Процедура Завершено() КонецПроцедуры").unwrap();
        let snapshot = handle.publish_disk_transaction(&writer).unwrap();
        assert_eq!(snapshot.generation, generation + 1);
        assert!(matches!(handle.request_analysis(), WorkspaceRead::Busy));
        writer.finish();
        handle.notify_disk_transaction_finished();
        let canonical_path = path.canonicalize().unwrap();
        let file = snapshot.files.iter().find(|file| file.path == canonical_path).unwrap();
        let WorkspaceRead::Ready(analysis) = handle.request_analysis() else {
            panic!("terminal disk state must be readable after publication");
        };
        assert_eq!(
            analysis.file_text(file.file_id).as_str(),
            "Процедура Завершено() КонецПроцедуры"
        );
        assert_eq!(state.generation(), generation + 1);
    }

    #[test]
    fn disk_transaction_recovery_is_reported_as_a_persistent_failure() {
        let state = DiagnosticsState::disabled();
        let handle = WorkspaceHandle::new(state.clone());
        let gate = handle.disk_gate();
        let mut writer = gate.begin_write(std::time::Duration::from_secs(1), &|| false).unwrap();
        writer.mark_writing();
        drop(writer);
        assert!(matches!(handle.readiness(), WorkspaceReadiness::Failed(message)
            if message == crate::diagnostics_state::DISK_RECOVERY_REQUIRED));
        assert!(matches!(handle.request_analysis(), WorkspaceRead::Failed(_)));
        let report = state.status_report();
        assert_eq!(report.state, "failed");
        assert_eq!(report.retry_after_ms, None);
        assert_eq!(report.error.as_deref(), Some(crate::diagnostics_state::DISK_RECOVERY_REQUIRED));
    }

    #[test]
    fn validated_project_and_exclusions_are_available_before_disk_seed() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            workspace.path().join("bsl-analyzer.toml"),
            "[source]\nexclude = [\"generated\"]\n",
        )
        .unwrap();
        let project = crate::project::at(workspace.path()).unwrap();
        let diagnostics =
            DiagnosticsState::for_workspace(workspace.path().to_path_buf()).with_project(project);
        let lease = Arc::new(HandleLease::new(Arc::clone(&diagnostics.active_workspace_handles)));
        let handle = WorkspaceHandle { diagnostics, _lease: lease };

        let project = match handle.project() {
            WorkspaceRead::Ready(project) => project,
            _ => panic!("validated bootstrap project is available while disk seed is idle"),
        };
        assert!(project
            .source_exclusions()
            .is_excluded(&workspace.path().join("generated/module.bsl")));
        assert!(matches!(handle.snapshot(), WorkspaceRead::Loading));
    }
}
