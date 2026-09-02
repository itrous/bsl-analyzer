mod bootstrap;
pub use bootstrap::WorkspaceInitError;
mod embed;
mod overlay_retry;
pub(crate) mod retry_window;
mod sync;
#[cfg(test)]
pub(crate) mod test_support;
mod types;

use crate::baseline::DeferredBaselineRuntime;
use crate::change_hub::WorkspaceChangeHub;
use crate::diagnostics_state::DiagnosticsState;
use crate::graph::GraphState;
use bsl_search::IndexProgress;
use onec_client::Client as OnecClient;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) use types::{
    OverlayWarmupState, SemanticRuntimeStatus, SharedSearchEngine, WorkspaceSearchMode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReferenceSearchLifecycle {
    Uninitialized,
    Loading,
    Ready,
    Failed { message: String, reason_code: String },
}

#[derive(Clone)]
pub(crate) struct ReferenceSearchState {
    engine: SharedSearchEngine,
    progress: Arc<IndexProgress>,
    semantic_runtime: Arc<Mutex<SemanticRuntimeStatus>>,
    baseline: DeferredBaselineRuntime,
    lifecycle: Arc<Mutex<ReferenceSearchLifecycle>>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    worker: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

/// Per-query cap on how many dirty overlay paths [`SharedState::prefetch_resident_overlay`]
/// resolves from the shared resident parse. A branch switch can dirty thousands of paths;
/// prefetching them all on the query thread would be unbounded work. Paths beyond the cap stay
/// dirty and are served by the query's own lazy disk refresh and by subsequent queries' prefetch
/// passes, so nothing is lost — the cap is purely a per-query budget. 64 keeps the pre-pass cheap
/// while covering the common "edit a handful of files, then search" case in one shot.
#[cfg(test)]
const MAX_RESIDENT_PREFETCH_PATHS_PER_QUERY: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WorkspaceSearchApply<T, E> {
    Applied(T),
    TransientRefusal,
    Superseded,
    Released,
    OperationError(E),
}

#[derive(Clone)]
pub struct SharedState {
    workspace_root: Option<PathBuf>,
    /// The configuration root (the `Configuration.xml`-bearing directory, e.g. `src/cf`),
    /// which may be nested under `workspace_root`. File-tree lookups such as
    /// `metadata(form)` resolve object directories relative to THIS root, not the repo root.
    source_root: Option<PathBuf>,
    standalone_notice: Option<String>,
    onec_client: Option<OnecClient>,
    onec_connections: BTreeMap<String, OnecConnection>,
    debug_session: Arc<Mutex<Option<bsl_debug::session::DebugSession>>>,
    search_engine: SharedSearchEngine,
    workspace_search_initializing: Arc<AtomicBool>,
    index_progress: Arc<IndexProgress>,
    semantic_runtime: Arc<Mutex<SemanticRuntimeStatus>>,
    /// Outcome of the startup overlay warmup, so `search status` can distinguish "no local
    /// diffs" from "warmup failed" instead of leaving a bare `Ready` ambiguous.
    overlay_warmup: Arc<Mutex<OverlayWarmupState>>,
    workspace_search_mode: WorkspaceSearchMode,
    /// Baseline runtime behind its connect lifecycle: the PG source is built on a
    /// background thread, so construction (and thus the MCP `initialize` handshake)
    /// never waits on the network. Readers see an explicit pending state meanwhile.
    baseline: DeferredBaselineRuntime,
    reference_search: ReferenceSearchState,
    graph: GraphState,
    diagnostics: DiagnosticsState,
    /// Daemon-owned filesystem change hub. Created before any consumer subscribes
    /// so its lifecycle is independent of the search engine's (which starts later,
    /// in a background init thread). `None` for the reference/shared profiles,
    /// which have no workspace tree to watch. Held so additional sinks (diagnostics
    /// drain-on-read, graph invalidation) can subscribe once they land; the search
    /// sink already runs off a clone taken at construction.
    #[allow(dead_code)]
    change_hub: Option<WorkspaceChangeHub>,
    /// This daemon's claim on the workspace's derived caches, held so the serve loop can retire
    /// a superseded backend early. Unmanaged for profiles with no workspace to coordinate over.
    workspace_lease: crate::workspace_lease::WorkspaceLease,
    /// The overlay retry driver — the one owner of every Embed pass (startup included).
    /// `None` outside PostgresRemoteOverlay-with-embedder, where no such pass exists.
    overlay_retry: Option<Arc<overlay_retry::OverlayRetry>>,
    /// Registry of the tasks opened by the `io.modelcontextprotocol/tasks` extension.
    ///
    /// It lives on the daemon process, not on the session: a task handle that a client
    /// picks up again after its connection dropped is the whole point of the extension,
    /// and a registry owned by `McpServer`'s per-session clone would die with the wire.
    /// The manager is itself `Arc`-backed, so every session of a backend addresses the
    /// same tasks. Its durability ends where the process does — an idle-TTL exit takes
    /// the handles with it, and the contract names that as a lawful `-32602`.
    tasks: rmcp::task_manager::TaskManager,
}

#[derive(Clone)]
pub struct OnecConnection {
    client: OnecClient,
    allow_execute: bool,
}

impl OnecConnection {
    pub fn new(client: OnecClient, allow_execute: bool) -> Self {
        Self { client, allow_execute }
    }

    pub fn client(&self) -> &OnecClient {
        &self.client
    }

    pub fn allow_execute(&self) -> bool {
        self.allow_execute
    }
}

impl SharedState {
    pub(super) fn search_fence_outcome<T>(
        outcome: crate::workspace_lease::LeaseOperationOutcome<T, bsl_search::SearchError>,
    ) -> bsl_search::FenceOutcome<Result<T, bsl_search::SearchError>> {
        match outcome {
            crate::workspace_lease::LeaseOperationOutcome::Applied(value) => {
                bsl_search::FenceOutcome::Applied(Ok(value))
            }
            crate::workspace_lease::LeaseOperationOutcome::OperationError(error) => {
                let error = match error {
                    crate::workspace_lease::LeaseOperationError::Lease(error) => error.into(),
                    crate::workspace_lease::LeaseOperationError::Operation(error) => error,
                };
                bsl_search::FenceOutcome::Applied(Err(error))
            }
            crate::workspace_lease::LeaseOperationOutcome::TransientRefusal => {
                bsl_search::FenceOutcome::TransientRefusal
            }
            crate::workspace_lease::LeaseOperationOutcome::Superseded => {
                bsl_search::FenceOutcome::Superseded
            }
            crate::workspace_lease::LeaseOperationOutcome::Released => {
                bsl_search::FenceOutcome::Released
            }
        }
    }

    pub(super) fn apply_workspace_search<T>(
        shared: &SharedSearchEngine,
        lease: &crate::workspace_lease::WorkspaceLease,
        apply: impl FnOnce(&mut bsl_search::SearchEngine) -> Result<T, bsl_search::SearchError>,
    ) -> WorkspaceSearchApply<T, bsl_search::SearchError> {
        let mut guard = match shared.lock() {
            Ok(guard) => guard,
            Err(error) => {
                return WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(
                    format!("workspace search engine lock poisoned: {error}"),
                ));
            }
        };
        let Some(engine) = guard.as_mut() else {
            return WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(
                "workspace search engine is not published".to_owned(),
            ));
        };
        match lease.publish_short(&mut (), |_| apply(engine)) {
            crate::workspace_lease::LeaseOperationOutcome::Applied(result) => {
                WorkspaceSearchApply::Applied(result)
            }
            crate::workspace_lease::LeaseOperationOutcome::OperationError(error) => {
                WorkspaceSearchApply::OperationError(match error {
                    crate::workspace_lease::LeaseOperationError::Lease(error) => error.into(),
                    crate::workspace_lease::LeaseOperationError::Operation(error) => error,
                })
            }
            crate::workspace_lease::LeaseOperationOutcome::TransientRefusal => {
                WorkspaceSearchApply::TransientRefusal
            }
            crate::workspace_lease::LeaseOperationOutcome::Superseded => {
                WorkspaceSearchApply::Superseded
            }
            crate::workspace_lease::LeaseOperationOutcome::Released => {
                WorkspaceSearchApply::Released
            }
        }
    }

    pub(super) fn apply_workspace_search_checkpointed<T>(
        shared: &SharedSearchEngine,
        lease: &crate::workspace_lease::WorkspaceLease,
        apply: impl FnOnce(
            &mut bsl_search::SearchEngine,
            &mut dyn FnMut() -> std::ops::ControlFlow<()>,
        ) -> std::ops::ControlFlow<(), Result<T, bsl_search::SearchError>>,
    ) -> WorkspaceSearchApply<T, bsl_search::SearchError> {
        let mut guard = match shared.lock() {
            Ok(guard) => guard,
            Err(error) => {
                return WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(
                    format!("workspace search engine lock poisoned: {error}"),
                ));
            }
        };
        Self::apply_to_engine(&mut guard, lease, apply)
    }

    /// The lease-gated write against a guard the caller already holds. Background writers
    /// take the guard with a plain `lock()` above; a request path takes it through the
    /// cancellable acquire and applies here, so the two differ only in how they waited.
    pub(super) fn apply_to_engine<T>(
        guard: &mut std::sync::MutexGuard<'_, Option<bsl_search::SearchEngine>>,
        lease: &crate::workspace_lease::WorkspaceLease,
        apply: impl FnOnce(
            &mut bsl_search::SearchEngine,
            &mut dyn FnMut() -> std::ops::ControlFlow<()>,
        ) -> std::ops::ControlFlow<(), Result<T, bsl_search::SearchError>>,
    ) -> WorkspaceSearchApply<T, bsl_search::SearchError> {
        let Some(engine) = guard.as_mut() else {
            return WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(
                "workspace search engine is not published".to_owned(),
            ));
        };
        match lease.publish_checkpointed(|checkpoint| apply(engine, checkpoint)) {
            crate::workspace_lease::LeaseOperationOutcome::Applied(result) => {
                WorkspaceSearchApply::Applied(result)
            }
            crate::workspace_lease::LeaseOperationOutcome::OperationError(error) => {
                WorkspaceSearchApply::OperationError(match error {
                    crate::workspace_lease::LeaseOperationError::Lease(error) => error.into(),
                    crate::workspace_lease::LeaseOperationError::Operation(error) => error,
                })
            }
            crate::workspace_lease::LeaseOperationOutcome::TransientRefusal => {
                WorkspaceSearchApply::TransientRefusal
            }
            crate::workspace_lease::LeaseOperationOutcome::Superseded => {
                WorkspaceSearchApply::Superseded
            }
            crate::workspace_lease::LeaseOperationOutcome::Released => {
                WorkspaceSearchApply::Released
            }
        }
    }

    pub(crate) fn graph(&self) -> &GraphState {
        &self.graph
    }

    /// Whether a newer daemon generation has taken this workspace's derived caches over (see
    /// [`crate::workspace_lease`]). Such a backend still serves everything it holds, but it
    /// produces no new derived state — so once its last session leaves there is nothing left
    /// to stay warm for.
    /// Every "analyzed without its main configuration" advisory the project
    /// carries, joined for one status line — the state in which valid calls into
    /// that configuration are reported as unresolved. An extension's and an
    /// external object's are distinct conditions and both can hold at once.
    /// Derived from the project as it is now, not from the root captured at
    /// bootstrap: a config edit can move the resolved root between a main
    /// configuration and an extension, and everything else — diagnostics, graph,
    /// drift — already rebuilds through `crate::project::at` when it does.
    pub(crate) fn standalone_notice(&self) -> Option<String> {
        self.standalone_notice.clone()
    }

    pub(crate) fn superseded(&self) -> bool {
        let _ = self.workspace_lease.owns_caches();
        self.workspace_lease.is_superseded()
    }

    #[cfg(test)]
    pub(crate) fn owns_caches(&self) -> bool {
        self.workspace_lease.owns_caches()
    }

    pub(crate) fn background_work_active(&self) -> bool {
        self.workspace_search_initializing.load(Ordering::Relaxed)
            || self.reference_search.loading()
            || self.index_progress.is_active()
            || self.semantic_runtime.try_lock().map_or(true, |status| {
                matches!(
                    *status,
                    SemanticRuntimeStatus::Indexing | SemanticRuntimeStatus::OverlaySyncing
                )
            })
    }

    /// Cached request-path view backed only by lease atomics.
    pub(crate) fn owns_caches_cached(&self) -> bool {
        self.workspace_lease.owns_caches_cached()
    }

    /// Start building the diagnostics resident now instead of on the first tool call.
    ///
    /// A serve path calls this right after construction so the resident (seconds of
    /// enumerate + metadata substrate on a large configuration) is ready before the
    /// agent's first `diagnostics` request rather than billed to it. Deliberately not
    /// part of [`Self::workspace`]: state is also constructed by tests and short-lived
    /// commands that never serve diagnostics, and those must not pay for (or race) a
    /// background resident build. No-op without a workspace root.
    pub fn warm_start(&self) {
        self.diagnostics.ensure_loading();
    }

    pub(crate) fn diagnostics(&self) -> &DiagnosticsState {
        &self.diagnostics
    }

    pub(crate) fn tasks(&self) -> &rmcp::task_manager::TaskManager {
        &self.tasks
    }

    // Consumed by the diagnostics/graph sinks once they subscribe; exposed now so
    // the hub the daemon owns is reachable from the tool layer.
    #[allow(dead_code)]
    pub(crate) fn change_hub(&self) -> Option<&WorkspaceChangeHub> {
        self.change_hub.as_ref()
    }

    pub fn set_onec_client(&mut self, client: OnecClient) {
        self.onec_client = Some(client);
    }

    pub fn onec_client(&self) -> Option<&OnecClient> {
        self.onec_client.as_ref()
    }

    pub fn add_onec_connection(&mut self, name: String, connection: OnecConnection) {
        self.onec_connections.insert(name, connection);
    }

    pub fn onec_connection(&self, name: Option<&str>) -> Result<OnecConnection, String> {
        if let Some(name) = name {
            return self.onec_connections.get(name).cloned().ok_or_else(|| {
                if self.onec_connections.is_empty() {
                    format!(
                        "Unknown 1C connection '{name}'. No named connections are configured; \
                         omit `connection` to use the --onec-url client."
                    )
                } else {
                    let available =
                        self.onec_connections.keys().cloned().collect::<Vec<_>>().join(", ");
                    format!("Unknown 1C connection '{name}'. Available: {available}")
                }
            });
        }
        if let Some(client) = &self.onec_client {
            // The legacy `--onec-url` client predates per-connection gating; keep run/eval
            // enabled for it — execution is still guarded by the 1C-side role split.
            return Ok(OnecConnection::new(client.clone(), true));
        }
        if self.onec_connections.len() == 1 {
            return Ok(self.onec_connections.values().next().expect("one connection").clone());
        }
        if self.onec_connections.is_empty() {
            return Err(
                "1C HTTP клиент не настроен. Укажите --onec-url или BSL_ONEC_CONNECTIONS_FILE."
                    .to_string(),
            );
        }
        let available = self.onec_connections.keys().cloned().collect::<Vec<_>>().join(", ");
        Err(format!("1C connection is required. Available: {available}"))
    }

    pub fn set_workspace_root(&mut self, root: PathBuf) {
        self.workspace_root = Some(root);
    }

    pub fn workspace_root(&self) -> Option<&PathBuf> {
        self.workspace_root.as_ref()
    }

    /// The real configuration root (`Configuration.xml`-bearing directory), when this
    /// project has one. Extension-only projects deliberately return `None`; the workspace
    /// directory is not a synthetic base configuration.
    pub fn source_root(&self) -> Option<&PathBuf> {
        self.source_root.as_ref()
    }

    pub fn debug_session(&self) -> &Arc<Mutex<Option<bsl_debug::session::DebugSession>>> {
        &self.debug_session
    }

    pub fn search_engine(&self) -> &SharedSearchEngine {
        &self.search_engine
    }

    pub fn index_progress(&self) -> &Arc<IndexProgress> {
        &self.index_progress
    }

    pub(crate) fn semantic_runtime(&self) -> Arc<Mutex<SemanticRuntimeStatus>> {
        Arc::clone(&self.semantic_runtime)
    }

    pub(crate) fn overlay_warmup(&self) -> Arc<Mutex<OverlayWarmupState>> {
        Arc::clone(&self.overlay_warmup)
    }

    /// Coalesce a request-observed stale resident into the existing background owner.
    pub(crate) fn request_overlay_refresh(&self) {
        if let Some(retry) = &self.overlay_retry {
            retry.kick_fresh();
        }
    }

    pub(crate) fn workspace_search_mode(&self) -> WorkspaceSearchMode {
        self.workspace_search_mode.clone()
    }

    #[cfg(test)]
    pub(crate) fn workspace_lease(&self) -> &crate::workspace_lease::WorkspaceLease {
        &self.workspace_lease
    }

    /// A single-lock snapshot of the baseline lifecycle — the only read surface for
    /// tool handlers. While the deferred connect is `pending`, gates answer "warming —
    /// retry shortly" instead of a config error; one snapshot per request keeps the
    /// pending flag and the runtime pieces describing the same instant.
    pub(crate) fn baseline_view(&self) -> crate::baseline::BaselineView {
        self.baseline.view()
    }

    pub(crate) fn ensure_reference_loading(&self) {
        self.reference_search.ensure_loading();
    }

    pub(crate) fn reference_search_engine(&self) -> SharedSearchEngine {
        Arc::clone(&self.reference_search.engine)
    }

    pub(crate) fn reference_baseline_view(&self) -> crate::baseline::BaselineView {
        self.reference_search.baseline.view()
    }

    pub(crate) fn reference_lifecycle(&self) -> ReferenceSearchLifecycle {
        self.reference_search.lifecycle()
    }

    pub fn shutdown(&self) {
        // The reference worker stops FIRST: in the reference profile its baseline and this one
        // are the same `Arc`, and closing the baseline before the worker is told to stop leaves
        // it working against a shut-down service — its own `shutdown` closes the baseline in the
        // right order (stop, then close, then join).
        self.reference_search.shutdown();
        self.baseline.shutdown();
        self.diagnostics.shutdown();
        // The retry driver stops BEFORE the lease is released: its Arc-held worker would
        // otherwise outlive the handover and publish over the next owner's caches.
        if let Some(retry) = &self.overlay_retry {
            retry.stop();
        }
        // Handing the workspace back on the way out is what keeps a short-lived server (a
        // stdio session, a broker fallback) from demoting a long-running daemon for the whole
        // staleness window just by having started later.
        self.workspace_lease.release();
    }

    /// Prefetch resident snapshots for the overlay's dirty paths and feed them into the
    /// incremental reindex, so a following query serves chunks cut from the SHARED resident
    /// parse instead of a second disk read+parse. Called at the top of a code-search request,
    /// before the query acquires the engine lock.
    ///
    /// Bounded to [`MAX_RESIDENT_PREFETCH_PATHS_PER_QUERY`] paths per call.
    ///
    /// Lock discipline: the resident read must never overlap the engine lock. So this
    /// reads the dirty-path list and the source handle under a brief engine lock, RELEASES it,
    /// fetches the snapshots with NO lock held, then applies them under a second brief engine
    /// lock that only touches the overlay cache (never the resident). A resident that is
    /// absent/loading, or a path it cannot serve, is simply missing from the map and the
    /// reindex disk-reads it — so search never regresses when the resident is unavailable.
    #[cfg(test)]
    pub(crate) fn prefetch_resident_overlay_fenced(
        engine: &SharedSearchEngine,
        lease: &crate::workspace_lease::WorkspaceLease,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::tools::search::Withdrawn> {
        sync::prefetch_resident_overlay(engine, lease, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::{SharedSearchEngine, SharedState, WorkspaceSearchApply};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn workspace_search_missing_engine_is_an_operation_error() {
        let shared: SharedSearchEngine = Arc::new(Mutex::new(None));
        let outcome = SharedState::apply_workspace_search(
            &shared,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            |_| Ok(()),
        );

        assert!(matches!(
            outcome,
            WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(message))
                if message == "workspace search engine is not published"
        ));
    }

    #[test]
    fn workspace_search_poisoned_mutex_is_an_operation_error() {
        let shared: SharedSearchEngine = Arc::new(Mutex::new(None));
        let poison = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("poison the search engine mutex");
        })
        .join();

        let outcome = SharedState::apply_workspace_search(
            &shared,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            |_| Ok(()),
        );

        assert!(matches!(
            outcome,
            WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(message))
                if message.starts_with("workspace search engine lock poisoned:")
        ));
    }

    #[test]
    fn workspace_search_flattens_callback_error_once() {
        let dir = tempfile::tempdir().unwrap();
        let engine = bsl_search::SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let shared: SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let outcome = SharedState::apply_workspace_search(
            &shared,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err::<(), _>(bsl_search::SearchError::Index("store failed".to_owned()))
            },
        );

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            outcome,
            WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(message))
                if message == "store failed"
        ));
    }

    #[test]
    fn terminal_supersession_is_not_transient_nonownership() {
        let transient_dir = tempfile::tempdir().unwrap();
        let transient_cache =
            crate::cache::WorkspaceCacheLayout::for_workspace(transient_dir.path());
        let holder = crate::workspace_lease::WorkspaceLease::hold_cache_lock_for(
            &transient_cache,
            Duration::from_secs(6),
        );
        let transient = crate::workspace_lease::WorkspaceLease::claim_cache(&transient_cache);
        let mut state = SharedState::shared();
        state.workspace_lease = transient.clone();
        assert!(!state.superseded(), "temporary UNCLAIMED is not terminal");
        assert!(!transient.is_superseded());
        holder.join().unwrap();

        let released_dir = tempfile::tempdir().unwrap();
        let released = crate::workspace_lease::WorkspaceLease::claim(released_dir.path());
        state.workspace_lease = released.clone();
        released.release();
        assert!(!state.superseded(), "normal release is not supersession");
        assert!(!state.owns_caches());

        let terminal_dir = tempfile::tempdir().unwrap();
        let old = crate::workspace_lease::WorkspaceLease::claim(terminal_dir.path());
        state.workspace_lease = old.clone();
        let newer = crate::workspace_lease::WorkspaceLease::claim(terminal_dir.path());
        old.invalidate_verdict_for_test();
        assert!(state.superseded(), "the refreshed live foreign token is terminal");
        newer.release();
        assert!(state.superseded(), "owner release cannot clear the terminal flag");
        assert!(!state.owns_caches());
    }
}

#[cfg(test)]
mod onec_connection_tests {
    use super::*;

    #[test]
    fn named_connection_is_selected_and_carries_execute_policy() {
        let mut state = SharedState::shared();
        state.add_onec_connection(
            "test".into(),
            OnecConnection::new(OnecClient::new("http://localhost/test", "", ""), true),
        );
        assert!(state.onec_connection(Some("test")).unwrap().allow_execute());
        let error = match state.onec_connection(Some("missing")) {
            Ok(_) => panic!("missing connection must fail"),
            Err(error) => error,
        };
        assert!(error.contains("test"));
    }

    #[test]
    fn legacy_client_keeps_execute_enabled() {
        let mut state = SharedState::shared();
        state.set_onec_client(OnecClient::new("http://localhost/legacy", "", ""));
        assert!(state.onec_connection(None).unwrap().allow_execute());
    }

    #[test]
    fn sole_named_connection_is_default() {
        let mut state = SharedState::shared();
        state.add_onec_connection(
            "only".into(),
            OnecConnection::new(OnecClient::new("http://localhost/only", "", ""), false),
        );
        assert!(!state.onec_connection(None).unwrap().allow_execute());
    }
}

#[cfg(test)]
mod standalone_extension_tests {
    use super::SharedState;

    fn configuration(root: &std::path::Path, rel: &str, extension: bool) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        let purpose = if extension {
            "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
        } else {
            ""
        };
        std::fs::write(
            dir.join("Configuration.xml"),
            format!(
                "<MetaDataObject><Configuration><Properties>{purpose}</Properties>\
                 </Configuration></MetaDataObject>"
            ),
        )
        .unwrap();
    }

    #[test]
    fn the_notice_is_cached_for_request_status() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configuration(root, "cf", false);
        configuration(root, "ext", true);
        let config = root.join("bsl-analyzer.toml");
        std::fs::write(&config, "[source]\nroot = \"cf\"\nextensions = []\n").unwrap();

        let state = SharedState::workspace(root.to_path_buf()).unwrap();
        assert!(state.standalone_notice().is_none(), "a main configuration stays silent");

        std::fs::write(&config, "[source]\nroot = \"ext\"\nextensions = []\n").unwrap();
        assert!(state.standalone_notice().is_none(), "request status does not reparse the project");

        let extension = SharedState::workspace(root.to_path_buf()).unwrap();
        assert!(extension.standalone_notice().is_some());
    }
}

#[cfg(test)]
mod background_lifetime_tests {
    use super::SharedState;
    use std::sync::atomic::Ordering;

    #[test]
    fn active_indexing_keeps_the_backend_alive() {
        let state = SharedState::shared();
        assert!(!state.background_work_active());

        state.index_progress().active.store(true, Ordering::Relaxed);

        assert!(state.background_work_active());
    }
}
