mod bootstrap;
pub use bootstrap::{resolve_embedding_token_profile_values, WorkspaceInitError};
mod embed;
pub(crate) mod overlay_backlog;
pub(crate) mod overlay_retry;
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
    shared_engine, OverlayWarmupState, SemanticRuntimeStatus, SharedSearchEngine,
    WorkspaceSearchMode,
};
pub use types::{EmbeddingPrefixes, EmbeddingTokenProfile};

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
    embedding_prefixes: types::EmbeddingPrefixes,
    lifecycle: Arc<Mutex<ReferenceSearchLifecycle>>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    /// The reference profile's own stop, for the one thing it shares with the workspace one:
    /// an engine acquisition that has to be callable off.
    stop: OwnerStop,
    worker: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

/// The "analyzed without its main configuration" advisory, and whether anybody still keeps it
/// current.
///
/// Seeded at boot from the project the boot already parsed, and kept in step by the graph's
/// drift watcher, which sees every config edit. A status read is a lock and a clone and never
/// reads the project from disk. When the watcher has left, the last value is still served —
/// with a note saying it is no longer tracked, because a config edit since then would go
/// unnoticed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StandaloneNotice {
    notice: Option<String>,
    abandoned: bool,
}

/// Appended to an advisory nobody keeps current any more.
const UNTRACKED_ADVISORY_NOTE: &str =
    "(no longer tracked: the workspace watcher has stopped; restart the server to refresh it)";

impl StandaloneNotice {
    /// A value someone keeps current: the boot's seed, or the watcher's latest derivation.
    pub(crate) fn tracked(notice: Option<String>) -> Self {
        Self { notice, abandoned: false }
    }

    /// Nobody keeps this value current from now on.
    pub(crate) fn abandon(&mut self) {
        self.abandoned = true;
    }

    fn rendered(&self) -> Option<String> {
        let notice = self.notice.as_ref()?;
        Some(if self.abandoned {
            format!("{notice}\n{UNTRACKED_ADVISORY_NOTE}")
        } else {
            notice.clone()
        })
    }
}

/// Every "analyzed without its main configuration" advisory `project` carries, joined for
/// one status line.
pub(crate) fn standalone_notice_of(project: &project_model::Project) -> Option<String> {
    let notices: Vec<String> =
        [project.standalone_extension_notice(), project.standalone_external_notice()]
            .into_iter()
            .flatten()
            .collect();
    (!notices.is_empty()).then(|| notices.join("\n"))
}

/// [`standalone_notice_of`] for the project at `root`, read from disk — for the watcher,
/// never for a request.
pub(crate) fn derive_standalone_notice(root: &std::path::Path) -> Option<String> {
    standalone_notice_of(&crate::project::at(root).ok()?)
}

/// The daemon's stop request to every background owner, and the count of owners still running.
///
/// One object for both, because a stop that cannot be observed is a stop nobody can prove: the
/// count is how `shutdown` (and a test) learns the owners actually left. Every wait an owner
/// makes is a wait on this signal or on the hub, which `shutdown` interrupts too, so no owner
/// sleeps out a backoff after the daemon has asked it to go.
#[derive(Clone, Default)]
pub(crate) struct OwnerStop(Arc<OwnerStopInner>);

#[derive(Default)]
struct OwnerStopInner {
    /// The stop as a lock-free fact, and the mirror `stopped` below waits on.
    ///
    /// Read without taking anything, because the readers are predicates handed to someone
    /// else's wait — the hub's, the backlog's — and they run with that owner's lock held. A
    /// read that took this stop's mutex would invert the two locks against `stop()`, which
    /// holds this one while it wakes them.
    raised: std::sync::atomic::AtomicBool,
    stopped: Mutex<bool>,
    wake: std::sync::Condvar,
    live: std::sync::atomic::AtomicUsize,
    /// Everything that has a wait of its own and must be released by the same call: the hub's
    /// `closing`, an owner's signal condvar, the engine's admission queue. A stop that wakes
    /// only its own condvar leaves an owner asleep on someone else's, and the order of the
    /// shutdown steps then decides whether that owner leaves in a second or in thirty.
    wakers: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
    /// Scope invalidation has a stronger terminal action than ordinary shutdown or
    /// supersession. Keep its transport cancellation separate so a normal owner stop still
    /// observes the daemon's existing drain ordering.
    scope_transport_stop: Mutex<Option<tokio_util::sync::CancellationToken>>,
}

/// Taken by whoever spawns an owner, before the thread starts, and moved into it: an owner
/// about to run already counts, and the guard's drop is its exit on every way out, a panic
/// or a thread that never started included.
pub(crate) struct OwnerLive(OwnerStop);

impl Drop for OwnerLive {
    fn drop(&mut self) {
        self.0 .0.live.fetch_sub(1, Ordering::SeqCst);
        self.0 .0.wake.notify_all();
    }
}

impl crate::tools::search::OwnerWait for OwnerStop {
    fn stopped(&self) -> bool {
        self.is_stopped()
    }
}

impl OwnerStop {
    pub(crate) fn set_scope_transport_stop(
        &self,
        transport_stop: tokio_util::sync::CancellationToken,
    ) {
        *self.0.scope_transport_stop.lock().unwrap_or_else(|poison| poison.into_inner()) =
            Some(transport_stop);
    }

    /// Scope mismatch is terminal: cancel its transport before stopping owners. Ordinary
    /// `stop()` deliberately leaves the transport alone for supersession/drain callers.
    pub(crate) fn stop_for_scope_change(&self) {
        {
            let transport_stop =
                self.0.scope_transport_stop.lock().unwrap_or_else(|poison| poison.into_inner());
            match transport_stop.as_ref() {
                Some(transport_stop) if !transport_stop.is_cancelled() => {
                    tracing::warn!("workspace source composition changed; restart/reconnect MCP");
                    transport_stop.cancel();
                }
                _ => {}
            }
        }
        self.stop();
    }

    /// Ask every owner to leave, and release every wait they could be in — this call, not the
    /// steps around it. What "release" means is the waiter's own business: a condvar notify, a
    /// hub that starts closing, a queue that stops admitting.
    pub(crate) fn stop(&self) {
        self.0.raised.store(true, Ordering::SeqCst);
        {
            let mut stopped = self.0.stopped.lock().unwrap_or_else(|poison| poison.into_inner());
            *stopped = true;
        }
        self.0.wake.notify_all();
        // Outside this stop's own lock: a waker reaches into another owner's lock, and that
        // owner's wait may be evaluating a predicate that reads this stop.
        let wakers = self.0.wakers.lock().unwrap_or_else(|poison| poison.into_inner());
        for wake in wakers.iter() {
            wake();
        }
    }

    /// Register a wait this stop must release. Called once per waiter, at wiring time.
    pub(crate) fn wakes(&self, wake: impl Fn() + Send + Sync + 'static) {
        let wake = Arc::new(wake);
        let registered = Arc::clone(&wake);
        self.0
            .wakers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(Box::new(move || registered()));
        if self.is_stopped() {
            // Registered after the stop was raised: released at once rather than never. Called
            // with no lock of this stop's held, for the reason `stop` gives.
            wake();
        }
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.0.raised.load(Ordering::SeqCst)
    }

    /// Sleep for `delay` unless the stop arrives first; says whether it did.
    pub(crate) fn sleep(&self, delay: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + delay;
        let mut stopped = self.0.stopped.lock().unwrap_or_else(|poison| poison.into_inner());
        loop {
            if *stopped {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return false;
            };
            stopped = self
                .0
                .wake
                .wait_timeout(stopped, remaining)
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
    }

    /// Count an owner as running until the returned guard drops.
    pub(crate) fn enter(&self) -> OwnerLive {
        self.0.live.fetch_add(1, Ordering::SeqCst);
        OwnerLive(self.clone())
    }

    /// Wait for every admitted owner to leave before releasing shared derived files.
    pub(crate) fn wait_empty(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut stopped = self.0.stopped.lock().unwrap_or_else(|poison| poison.into_inner());
        while self.0.live.load(Ordering::SeqCst) != 0 {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return false;
            };
            let (guard, result) = self
                .0
                .wake
                .wait_timeout(stopped, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            stopped = guard;
            if result.timed_out() && self.0.live.load(Ordering::SeqCst) != 0 {
                return false;
            }
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.0.live.load(Ordering::SeqCst)
    }
}

/// Where the workspace search consumer is in its life — what `search_code`'s drift watch
/// rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsumerPhase {
    /// Its cursor collects facts; it starts once an engine is published.
    Pending,
    /// Its thread runs, but its first fenced step has not passed: what it reads does not yet
    /// reach the index.
    Attaching,
    /// Running: every fact on its cursor reaches the index.
    Attached,
    /// It has left: stopped, superseded or released.
    Stopped,
    /// The engine it would feed never came: the init failed or published nothing.
    Abandoned,
}

/// Abandons a consumer that never got past `.1`: dropped while the phase is still that, it
/// names the consumer abandoned. Moved into the closure of the thread that would take the
/// consumer further, it fires on every way out that does not — the thread's own early exits,
/// and a thread that never started, whose closure is dropped unrun.
pub(super) struct AbandonIfStill(pub(super) Arc<Mutex<ConsumerPhase>>, pub(super) ConsumerPhase);

impl Drop for AbandonIfStill {
    fn drop(&mut self) {
        let mut phase = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if *phase == self.1 {
            *phase = ConsumerPhase::Abandoned;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WorkspaceSearchApply<T, E> {
    Applied(T),
    TransientRefusal,
    Superseded,
    Released,
    OperationError(E),
    /// The owner was told to leave before it could apply anything. Its own outcome, because
    /// the two things a caller does about it are the two things it must never confuse: a
    /// transient refusal is slept on and retried, and a stop is answered by going.
    Stopping,
}

#[derive(Clone)]
pub struct SharedState {
    workspace_root: Option<PathBuf>,
    /// The configuration root (the `Configuration.xml`-bearing directory, e.g. `src/cf`),
    /// which may be nested under `workspace_root`. File-tree lookups such as
    /// `metadata(form)` resolve object directories relative to THIS root, not the repo root.
    source_root: Option<PathBuf>,
    standalone_notice: Arc<Mutex<StandaloneNotice>>,
    onec_client: Option<OnecClient>,
    onec_connections: BTreeMap<String, OnecConnection>,
    debug_session: Arc<Mutex<Option<bsl_debug::session::DebugSession>>>,
    search_engine: SharedSearchEngine,
    workspace_search_initializing: Arc<AtomicBool>,
    /// The embed pass's claim, and with it the answer to "is embedding running right now".
    /// Read for the backend's lifetime because it is owned by the pass itself: no peer's
    /// status write can erase it, and it is released on every exit including a panic.
    embed_flight: Arc<embed::EmbedFlight>,
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
    /// The stop every background owner of this backend waits on.
    owners: OwnerStop,
    /// Scope drift is terminal for this backend's transports. Kept separate from ordinary
    /// owner stop so supersession can finish its existing graph drain before closing sessions.
    scope_transport_stop: tokio_util::sync::CancellationToken,
    /// The owner of the overlay's point backlog; started once an engine is published.
    overlay_backlog: overlay_backlog::OverlayBacklog,
    /// Where the workspace search consumer is (see [`ConsumerPhase`]).
    search_consumer: Arc<Mutex<ConsumerPhase>>,
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

/// Who watches the workspace for the index. An attached consumer vouches for what reaches it,
/// and a hub still arming delivers nothing yet: until it has armed or fallen back to polling,
/// the consumer is still starting.
fn consumer_drift_watch(
    phase: ConsumerPhase,
    hub: &crate::change_hub::WorkspaceChangeHub,
) -> crate::tools::location::DriftWatch {
    use crate::tools::location::DriftWatch;
    match phase {
        ConsumerPhase::Pending | ConsumerPhase::Attaching => DriftWatch::Starting,
        ConsumerPhase::Attached if hub.is_polling() || hub.is_partially_blind() => {
            DriftWatch::Polling
        }
        ConsumerPhase::Attached if hub.is_watching() => DriftWatch::Watching,
        ConsumerPhase::Attached => DriftWatch::Starting,
        ConsumerPhase::Stopped | ConsumerPhase::Abandoned => DriftWatch::Unobserved,
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
        stop: &OwnerStop,
        lease: &crate::workspace_lease::WorkspaceLease,
        apply: impl FnOnce(&mut bsl_search::SearchEngine) -> Result<T, bsl_search::SearchError>,
    ) -> WorkspaceSearchApply<T, bsl_search::SearchError> {
        let mut guard = match shared.acquire_for_owner(stop) {
            Ok(guard) => guard,
            // Leaving is not failing, and it is not a refusal either: an owner told to go must
            // not sleep a backoff and come back. Its own outcome, so no caller can mistake it.
            Err(crate::tools::search::OwnerLockRefused::Closing) => {
                return WorkspaceSearchApply::Stopping;
            }
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
        stop: &OwnerStop,
        lease: &crate::workspace_lease::WorkspaceLease,
        apply: impl FnOnce(
            &mut bsl_search::SearchEngine,
            &mut dyn FnMut() -> std::ops::ControlFlow<()>,
        ) -> std::ops::ControlFlow<(), Result<T, bsl_search::SearchError>>,
    ) -> WorkspaceSearchApply<T, bsl_search::SearchError> {
        let mut guard = match shared.acquire_for_owner(stop) {
            Ok(guard) => guard,
            Err(crate::tools::search::OwnerLockRefused::Closing) => {
                return WorkspaceSearchApply::Stopping;
            }
            Err(error) => {
                return WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(
                    format!("workspace search engine lock poisoned: {error}"),
                ));
            }
        };
        Self::apply_to_engine(&mut guard, lease, apply)
    }

    /// The lease-gated write against a guard the caller already holds. Background writers
    /// take the guard through the admission above; a request path takes it through the
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

    /// Every "analyzed without its main configuration" advisory the project
    /// carries, joined for one status line — the state in which valid calls into
    /// that configuration are reported as unresolved. An extension's and an
    /// external object's are distinct conditions and both can hold at once.
    ///
    /// Read from the slot the boot seeded and the drift watcher keeps current; see
    /// [`StandaloneNotice`]. The request path never reads the project from disk.
    pub(crate) fn standalone_notice(&self) -> Option<String> {
        self.standalone_notice.lock().unwrap_or_else(|p| p.into_inner()).rendered()
    }

    /// Whether a newer daemon generation has taken this workspace's derived caches over (see
    /// [`crate::workspace_lease`]). Such a backend still serves everything it holds, but it
    /// produces no new derived state — so once its last session leaves there is nothing left
    /// to stay warm for.
    pub(crate) fn superseded(&self) -> bool {
        self.workspace_lease.is_superseded()
    }

    #[cfg(test)]
    pub(crate) fn owns_caches(&self) -> bool {
        self.workspace_lease.owns_caches()
    }

    /// Whether this backend has handed the graph over after losing the workspace.
    pub(crate) fn graph_released(&self) -> bool {
        self.graph.released()
    }

    /// The unthrottled refresh the graph's owners make per pass ([`crate::graph`] reaches it
    /// through `is_superseded`). A stand about what a COMPLETED refresh leaves behind needs
    /// the check to actually run: the paced entry point above returns the cached verdict for
    /// as long as the pacing lasts, without asking disk anything.
    #[cfg(test)]
    pub(crate) fn refresh_ownership_now(&self) -> bool {
        self.workspace_lease.owns_caches_now()
    }

    /// The threads those questions were asked on. Background owners are named; a request is
    /// served on a runtime worker or on the caller's own thread, and neither may appear.
    #[cfg(test)]
    pub(crate) fn lease_disk_check_threads(&self) -> Vec<String> {
        self.workspace_lease.disk_check_threads()
    }

    /// Whether the lease's own lifecycle lock is held right now — for a test that must put a
    /// request against a background check that is inside it. Test-only introspection: nothing
    /// in production decides anything on a `try_lock`.
    #[cfg(test)]
    pub(crate) fn lease_lifecycle_is_busy(&self) -> bool {
        self.workspace_lease.lifecycle_is_busy()
    }

    /// Whether this backend's ownership verdict has been established at all — for a test that
    /// must tell the unknown window from an answer.
    #[cfg(test)]
    pub(crate) fn ownership_verdict_established(&self) -> bool {
        self.workspace_lease.ownership_was_checked()
    }

    /// Whether the boot's workspace-search initialization is still running. A test that needs
    /// the graph to stay unbuilt has to know when the thread that would claim it has gone.
    #[cfg(test)]
    pub(crate) fn search_init_running(&self) -> bool {
        self.workspace_search_initializing.load(Ordering::Relaxed)
    }

    /// Observe qualified indexing evidence without waiting or starting work.
    pub(crate) fn workspace_indexing(&self) -> crate::indexing::Indexing {
        use crate::baseline::BaselineIndexingPublication;
        use crate::indexing::{Indexing, Kind, Reason, State, Target};
        let sample = self.index_progress.snapshot();
        let unknown = || {
            Indexing::workspace(
                Target::unknown(Kind::Lexical),
                Target::unknown(Kind::Semantic).with_attempt(sample.as_ref()),
                sample.as_ref(),
            )
        };
        let (runtime_disabled, runtime_failed) = {
            let Ok(runtime) = self.semantic_runtime.try_lock() else { return unknown() };
            (matches!(*runtime, SemanticRuntimeStatus::Disabled), runtime.is_failed())
        };
        let Some(engine) = self.search_engine.try_snapshot() else {
            let semantic = match &sample {
                Some(sample)
                    if sample.pass_id.is_some()
                        && sample.state != bsl_search::IndexPassState::Ready =>
                {
                    Target::native(Kind::Semantic, sample)
                }
                _ => Target::unknown(Kind::Semantic).with_attempt(sample.as_ref()),
            };
            return Indexing::workspace(Target::unknown(Kind::Lexical), semantic, sample.as_ref());
        };
        let lexical = if engine.is_some() {
            Target::new(Kind::Lexical, State::Ready, None)
        } else if self.workspace_search_initializing.load(Ordering::Relaxed) {
            Target::new(Kind::Lexical, State::Waiting, Some(Reason::Initializing))
        } else if runtime_failed {
            Target::new(Kind::Lexical, State::Failed, Some(Reason::NativeFailure))
        } else {
            Target::new(Kind::Lexical, State::Waiting, Some(Reason::Initializing))
        };
        let remote =
            matches!(self.workspace_search_mode, WorkspaceSearchMode::PostgresRemoteOverlay);
        let warmup = if remote {
            self.overlay_warmup.try_lock().ok().map(|state| match &*state {
                OverlayWarmupState::Failed(_) | OverlayWarmupState::EmbeddingFailed(_) => {
                    State::Failed
                }
                OverlayWarmupState::Superseded => State::Superseded,
                OverlayWarmupState::Synced { .. } | OverlayWarmupState::NoLocalDiffs => {
                    State::Ready
                }
                _ => State::Waiting,
            })
        } else {
            None
        };
        let semantic = if sample.is_none() {
            Target::unknown(Kind::Semantic)
        } else if runtime_disabled {
            Target::new(Kind::Semantic, State::Disabled, Some(Reason::SemanticDisabled))
        } else if self.workspace_lease.is_superseded() {
            Target::new(Kind::Semantic, State::Superseded, Some(Reason::Superseded))
        } else if self.workspace_lease.is_released() {
            Target::new(Kind::Semantic, State::Cancelled, Some(Reason::Cancelled))
        } else if let Some(sample) = sample.as_ref().filter(|sample| {
            sample.pass_id.is_some()
                && matches!(
                    sample.state,
                    bsl_search::IndexPassState::Running
                        | bsl_search::IndexPassState::Failed
                        | bsl_search::IndexPassState::Cancelled
                        | bsl_search::IndexPassState::Superseded
                )
        }) {
            Target::native(Kind::Semantic, sample)
        } else if runtime_failed {
            Target::new(Kind::Semantic, State::Failed, Some(Reason::NativeFailure))
        } else if self.embed_flight.is_in_flight()
            || self.overlay_retry.as_ref().is_some_and(|retry| retry.pass_active())
        {
            Target::new(Kind::Semantic, State::Running, None)
        } else if remote && warmup.is_none() {
            Target::unknown(Kind::Semantic)
        } else if matches!(warmup, Some(State::Failed)) {
            Target::new(Kind::Semantic, State::Failed, Some(Reason::NativeFailure))
        } else if matches!(warmup, Some(State::Superseded)) {
            Target::new(Kind::Semantic, State::Superseded, Some(Reason::Superseded))
        } else if let Some(engine) = engine.as_ref() {
            match engine.try_workspace_overlay_retry_signals() {
                None => Target::unknown(Kind::Semantic),
                Some(signals) if signals.demands_a_pass() || signals.pending_dirty_paths > 0 => {
                    Target::new(
                        Kind::Semantic,
                        State::Waiting,
                        Some(if remote { Reason::OverlayPending } else { Reason::PendingWork }),
                    )
                }
                Some(_) => match self.workspace_search_mode {
                    WorkspaceSearchMode::SqliteLocal => {
                        let evidence = engine.semantic_index_qualification();
                        if evidence.context_pending || evidence.pending_work {
                            Target::new(Kind::Semantic, State::Waiting, Some(Reason::PendingWork))
                        } else if !evidence.identity_verified {
                            Target::new(
                                Kind::Semantic,
                                State::Unknown,
                                Some(Reason::IdentityUnverified),
                            )
                        } else if !evidence.coverage_complete {
                            Target::new(
                                Kind::Semantic,
                                State::Unknown,
                                Some(Reason::CoverageUnverified),
                            )
                        } else {
                            Target::new(Kind::Semantic, State::Ready, None)
                        }
                    }
                    WorkspaceSearchMode::PostgresRemoteOverlay => {
                        if !matches!(warmup, Some(State::Ready)) {
                            Target::new(
                                Kind::Semantic,
                                State::Waiting,
                                Some(Reason::OverlayPending),
                            )
                        } else {
                            let Some(baseline) = self.baseline.try_external() else {
                                return unknown();
                            };
                            match (
                                baseline,
                                engine.embedding_storage_identity(),
                                engine.embedding_dimension(),
                            ) {
                                (Some(baseline), Some(model), Some(dim)) => match baseline
                                    .indexing_publication(
                                        model,
                                        dim,
                                        engine.try_workspace_overlay_baseline_identity().as_ref(),
                                    ) {
                                    BaselineIndexingPublication::Ready => {
                                        Target::new(Kind::Semantic, State::Ready, None)
                                    }
                                    BaselineIndexingPublication::UnverifiedIdentity => Target::new(
                                        Kind::Semantic,
                                        State::Unknown,
                                        Some(Reason::IdentityUnverified),
                                    ),
                                    BaselineIndexingPublication::UnverifiedCoverage => Target::new(
                                        Kind::Semantic,
                                        State::Unknown,
                                        Some(Reason::CoverageUnverified),
                                    ),
                                    BaselineIndexingPublication::Stale => Target::new(
                                        Kind::Semantic,
                                        State::Unknown,
                                        Some(Reason::StaleGeneration),
                                    ),
                                    BaselineIndexingPublication::Unavailable => Target::new(
                                        Kind::Semantic,
                                        State::Waiting,
                                        Some(Reason::BaselineUnavailable),
                                    ),
                                    BaselineIndexingPublication::SnapshotUnavailable => {
                                        Target::unknown(Kind::Semantic)
                                    }
                                },
                                _ => Target::new(
                                    Kind::Semantic,
                                    State::Waiting,
                                    Some(Reason::BaselineUnavailable),
                                ),
                            }
                        }
                    }
                },
            }
        } else {
            Target::new(Kind::Semantic, State::Waiting, Some(Reason::Initializing))
        };
        let lexical =
            if matches!(self.workspace_search_mode, WorkspaceSearchMode::PostgresRemoteOverlay)
                && lexical.state == State::Ready
            {
                match (self.baseline.try_external(), engine.as_ref()) {
                    (Some(baseline), Some(engine)) => match baseline {
                        Some(baseline) => {
                            // Semantic identity and coverage do not gate lexical search, so an
                            // unverified publication still serves lexical hits.
                            match baseline.indexing_publication(
                                engine.embedding_storage_identity().unwrap_or(""),
                                engine.embedding_dimension().unwrap_or(0),
                                engine.try_workspace_overlay_baseline_identity().as_ref(),
                            ) {
                                BaselineIndexingPublication::Ready
                                | BaselineIndexingPublication::UnverifiedIdentity
                                | BaselineIndexingPublication::UnverifiedCoverage => {
                                    match engine.try_workspace_overlay_retry_signals() {
                                        None => Target::unknown(Kind::Lexical),
                                        Some(s)
                                            if s.initialized
                                                && !s.needs_full_rescan
                                                && s.pending_dirty_paths == 0
                                                && s.unread_keys == 0 =>
                                        {
                                            lexical
                                        }
                                        Some(_) => Target::new(
                                            Kind::Lexical,
                                            State::Waiting,
                                            Some(Reason::OverlayPending),
                                        ),
                                    }
                                }
                                BaselineIndexingPublication::Stale => Target::new(
                                    Kind::Lexical,
                                    State::Unknown,
                                    Some(Reason::StaleGeneration),
                                ),
                                BaselineIndexingPublication::SnapshotUnavailable => {
                                    Target::unknown(Kind::Lexical)
                                }
                                BaselineIndexingPublication::Unavailable => Target::new(
                                    Kind::Lexical,
                                    State::Waiting,
                                    Some(Reason::BaselineUnavailable),
                                ),
                            }
                        }
                        None => Target::new(
                            Kind::Lexical,
                            State::Waiting,
                            Some(Reason::BaselineUnavailable),
                        ),
                    },
                    _ => Target::unknown(Kind::Lexical),
                }
            } else {
                lexical
            };
        Indexing::workspace(lexical, semantic.with_attempt(sample.as_ref()), sample.as_ref())
    }

    pub(crate) fn reference_indexing(&self) -> crate::indexing::Indexing {
        crate::indexing::Indexing::single(self.reference_search.indexing_snapshot())
    }

    /// Whether work this backend owns is still running, and so whether the broker must keep
    /// the process alive past its idle TTL.
    ///
    /// Every term is a signal the work itself owns and releases — an atomic it raises, or a
    /// claim it holds — never the reported status. `semantic_runtime` looks like the obvious
    /// source and is the wrong one: embed and overlay share that single slot and each writes
    /// it whole, so an embed finishing mid-overlay writes `Ready` over `OverlaySyncing` and
    /// erases a running pass from view. A signal that a peer can overwrite cannot decide a
    /// process's lifetime.
    ///
    /// Nothing here blocks: the serve loop asks this on every tick.
    pub(crate) fn background_work_active(&self) -> bool {
        self.workspace_search_initializing.load(Ordering::Relaxed)
            || self.reference_search.loading()
            || self.index_progress.is_active()
            || self.embed_flight.is_in_flight()
            || self.overlay_retry.as_ref().is_some_and(|retry| retry.pass_active())
            || self.overlay_backlog.is_active()
    }

    /// Ownership for a STATUS answer: the cached verdict, when there IS one.
    ///
    /// Two things this must not do. It must not read the lease — that is a file, and a lock a
    /// peer may hold for seconds, on the thread serving a request. And it must not answer from
    /// the cached value before anything has established it: the startup claim can fail — a peer
    /// holding the lock, a record that could not be written — and the initial `false` then says
    /// "a newer generation owns these caches" about a question nobody has asked yet. The two
    /// are indistinguishable to a client, and only one of them is true.
    ///
    /// So the unknown window answers nothing at all, and the background check that runs anyway
    /// closes it: the value appears once it means something, exactly as every other
    /// present-when-known field in that envelope does.
    pub(crate) fn owns_caches_for_status(&self) -> Option<bool> {
        self.workspace_lease
            .ownership_was_checked()
            .then(|| self.workspace_lease.owns_caches_cached())
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

    pub(crate) fn search_engine(&self) -> &SharedSearchEngine {
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

    /// What `search_code` says about its own currency beyond the hits: who watches the
    /// workspace for the index, whether the backlog is being read back, whether the poll
    /// standing in for the watch is overdue. Process-local reads only.
    pub(crate) fn search_watch(&self) -> crate::tools::search::WorkspaceFacts {
        let Some(hub) = &self.change_hub else {
            return crate::tools::search::WorkspaceFacts::default();
        };
        let phase = *self.search_consumer.lock().unwrap_or_else(|poison| poison.into_inner());
        let drift_watch = consumer_drift_watch(phase, hub);
        let backlog_stalled = match self.overlay_backlog.state() {
            overlay_backlog::BacklogState::Exhausted { .. } => {
                Some("its retry budget ran out or a batch failed")
            }
            overlay_backlog::BacklogState::Stopped { reason } => Some(reason),
            overlay_backlog::BacklogState::Running
            | overlay_backlog::BacklogState::Backoff { .. } => None,
        };
        crate::tools::search::WorkspaceFacts {
            drift_watch: Some(drift_watch),
            backlog_stalled,
            poll_overdue: hub.poll_overdue(),
            ..Default::default()
        }
    }

    /// How long an edit that keeps its size and mtime can go unnoticed while the hub polls.
    pub(crate) fn poll_cycle(&self) -> Option<std::time::Duration> {
        self.change_hub.as_ref()?.poll_report().map(|(_, cycle)| cycle)
    }

    /// Where the overlay's backlog owner is.
    pub(crate) fn overlay_backlog_state(&self) -> overlay_backlog::BacklogState {
        self.overlay_backlog.state()
    }

    /// How many overlay batches held the engine past their bound.
    pub(crate) fn overlay_backlog_slow_holds(&self) -> u64 {
        self.overlay_backlog.slow_holds()
    }

    /// Coalesce a request-observed stale resident into the existing background owner.
    pub(crate) fn request_overlay_refresh(&self) {
        // A wake, not a fact: the owner may not step around its backoff on this.
        self.overlay_backlog.wake();
        if let Some(retry) = &self.overlay_retry {
            // Coalesce, not kick: a search has observed no new fact about the workspace, so it
            // may wake the driver but must not reset its backoff or revive an obligation that
            // already ran out of budget.
            retry.coalesce();
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

    pub(crate) fn reference_semantic_runtime(&self) -> Arc<Mutex<SemanticRuntimeStatus>> {
        Arc::clone(&self.reference_search.semantic_runtime)
    }

    pub(crate) fn reference_baseline_view(&self) -> crate::baseline::BaselineView {
        self.reference_search.baseline.view()
    }

    pub(crate) fn reference_lifecycle(&self) -> ReferenceSearchLifecycle {
        self.reference_search.lifecycle()
    }

    /// Stop the daemon's background work, in an order whose OUTCOME does not depend on the
    /// order: every wait an owner can be in is released by `owners.stop()` itself (the hub's
    /// `closing`, the backlog's signal, the retry driver's condvar, the admission queue), so
    /// no step here is load-bearing for how long an owner takes to leave.
    ///
    /// What the order still decides is who is told first, and that only matters for the
    /// subsystems with protocols of their own.
    pub fn shutdown(&self) {
        // The reference worker stops FIRST: in the reference profile its baseline and this one
        // are the same `Arc`, and closing the baseline before the worker is told to stop leaves
        // it working against a shut-down service — its own `shutdown` closes the baseline in the
        // right order (stop, then close, then join).
        self.reference_search.shutdown();
        self.baseline.shutdown();
        self.diagnostics.shutdown();
        // One call, every owner, every wait they could be in.
        self.owners.stop();
        // An owner queued behind someone else's hold of the engine leaves instead of waiting it
        // out. Redundant with the stop above by design: a queued owner is released by either.
        self.search_engine.close();
        if let Some(hub) = &self.change_hub {
            // Stops the transport, not just the waiting: the hub's own threads — the event
            // thread and the blind poll — go with it. Left running, a hub in fallback mode
            // keeps walking the workspace after the daemon it serves has stopped.
            hub.shutdown();
        }
        // Handing the workspace back on the way out is what keeps a short-lived server (a
        // stdio session, a broker fallback) from demoting a long-running daemon for the whole
        // staleness window just by having started later. LAST, so nothing this daemon still
        // has in flight can publish over the next owner's caches.
        if self.owners.wait_empty(std::time::Duration::from_secs(5)) {
            self.workspace_lease.release();
        } else {
            tracing::error!("workspace owners did not stop; retaining the workspace lease");
        }
    }

    pub(crate) fn scope_transport_stop(&self) -> tokio_util::sync::CancellationToken {
        self.scope_transport_stop.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{SharedSearchEngine, SharedState, WorkspaceSearchApply};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn workspace_cache_scope_change_logs_restart_once() {
        use tracing_subscriber::prelude::*;

        struct Capture(Arc<std::sync::Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct Visitor<'a>(&'a mut Vec<String>);
                impl tracing::field::Visit for Visitor<'_> {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        if field.name() == "message" {
                            self.0.push(format!("{value:?}"));
                        }
                    }

                    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                        if field.name() == "message" {
                            self.0.push(value.to_owned());
                        }
                    }
                }
                event.record(&mut Visitor(&mut self.0.lock().unwrap()));
            }
        }

        let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let owners = super::OwnerStop::default();
        let scope_stop = tokio_util::sync::CancellationToken::new();
        owners.set_scope_transport_stop(scope_stop.clone());
        test_utils::with_subscriber(
            tracing_subscriber::registry().with(Capture(messages.clone())),
            || {
                owners.stop_for_scope_change();
                owners.stop_for_scope_change();
            },
        );

        assert!(scope_stop.is_cancelled());
        assert_eq!(
            messages
                .lock()
                .unwrap()
                .iter()
                .filter(|message| {
                    message.contains("workspace source composition changed; restart/reconnect MCP")
                })
                .count(),
            1
        );
    }

    #[test]
    fn workspace_cache_scope_shutdown_waits_for_owners_before_releasing_lease() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = SharedState::shared();
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        state.workspace_lease = lease.clone();
        let owners = state.owners.clone();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        owners.wakes(move || {
            let _ = stop_tx.send(());
        });
        let owner = std::thread::spawn(move || {
            let _live = owners.enter();
            stop_rx.recv().unwrap();
            finish_rx.recv().unwrap();
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            state.shutdown();
            done_tx.send(()).unwrap();
        });

        done_rx.recv_timeout(Duration::from_secs(1)).expect_err("shutdown must await its owner");
        assert!(lease.owns_caches(), "the lease remains held while the owner may still write");
        finish_tx.send(()).unwrap();
        owner.join().unwrap();
        done_rx.recv_timeout(Duration::from_secs(1)).expect("shutdown completes after owner exit");
        shutdown.join().unwrap();
        assert!(!lease.owns_caches(), "lease release follows the last owner exit");
    }

    #[test]
    fn workspace_search_missing_engine_is_an_operation_error() {
        let shared: SharedSearchEngine = crate::state::shared_engine(None);
        let outcome = SharedState::apply_workspace_search(
            &shared,
            &super::OwnerStop::default(),
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            |_| Ok(()),
        );

        assert!(matches!(
            outcome,
            WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(message))
                if message == "workspace search engine is not published"
        ));
    }

    /// Leaving is neither a refusal nor a failure, and the outcome says which it is. Read as
    /// a transient refusal an owner sleeps a backoff and comes back; read as an operation
    /// error the daemon reports its own shutdown as a broken engine.
    #[test]
    fn an_owner_told_to_leave_answers_stopping_even_with_the_engine_held() {
        let shared: SharedSearchEngine = crate::state::shared_engine(None);
        let stop = super::OwnerStop::default();
        let lease = crate::workspace_lease::WorkspaceLease::unmanaged();

        // Someone else holds the engine, which is the case that used to cost an owner the
        // whole hold: the queue is what it gives up, not the daemon's time.
        let (holding_tx, holding) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel();
        let held = Arc::clone(&shared);
        let holder = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            holding_tx.send(()).unwrap();
            release.recv().unwrap();
        });
        holding.recv_timeout(Duration::from_secs(5)).unwrap();

        stop.stop();

        let asked = std::time::Instant::now();
        assert!(matches!(
            SharedState::apply_workspace_search(&shared, &stop, &lease, |_| Ok(())),
            WorkspaceSearchApply::Stopping
        ));
        assert!(matches!(
            SharedState::apply_workspace_search_checkpointed(&shared, &stop, &lease, |_, _| {
                std::ops::ControlFlow::Continue(Ok(()))
            }),
            WorkspaceSearchApply::Stopping
        ));
        assert!(
            asked.elapsed() < Duration::from_secs(1),
            "the owner waited out someone else's hold instead of leaving"
        );

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn workspace_search_poisoned_mutex_is_an_operation_error() {
        let shared: SharedSearchEngine = crate::state::shared_engine(None);
        let poison = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("poison the search engine mutex");
        })
        .join();

        let outcome = SharedState::apply_workspace_search(
            &shared,
            &super::OwnerStop::default(),
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
        let shared: SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let outcome = SharedState::apply_workspace_search(
            &shared,
            &super::OwnerStop::default(),
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
        assert!(!state.superseded(), "the cached view performs no ownership refresh");
        assert!(!old.owns_caches(), "the heartbeat-owned refresh observes takeover");
        assert!(state.superseded(), "the refreshed live foreign token is terminal");
        newer.release();
        assert!(state.superseded(), "owner release cannot clear the terminal flag");
        assert!(!state.owns_caches());
    }

    #[test]
    fn superseded_check_never_waits_for_the_lease_lifecycle_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let mut state = SharedState::shared();
        state.workspace_lease = lease.clone();
        let held = lease.hold_lifecycle_lock_for_test();
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || tx.send(state.superseded()).unwrap());

        assert!(!rx.recv_timeout(std::time::Duration::from_millis(100)).unwrap());
        drop(held);
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
    use super::{SharedState, StandaloneNotice};

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

    /// Scope drift retires the session; its advisory stays bound to the boot Project.
    #[test]
    fn the_advisory_is_seeded_at_boot_and_scope_drift_stops_the_session() {
        let _env = super::test_support::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configuration(root, "cf", false);
        configuration(root, "ext", true);
        let config = root.join("bsl-analyzer.toml");
        std::fs::write(&config, "[source]\nroot = \"ext\"\nextensions = []\n").unwrap();

        let state = SharedState::workspace(root.to_path_buf()).unwrap();
        assert!(
            state.standalone_notice().is_some(),
            "the boot parsed an extension root, and the seed says so before anything else runs"
        );

        std::fs::write(&config, "[source]\nroot = \"cf\"\nextensions = []\n").unwrap();
        assert!(
            crate::change_hub::test_support::eventually(std::time::Duration::from_secs(20), || {
                state.scope_transport_stop().is_cancelled()
            },),
            "changing the selected root retires the session bound to the old scope"
        );
        assert!(
            state.standalone_notice().is_some(),
            "the old Project remains the only analyzed one"
        );
        state.shutdown();
    }

    /// A status read is the slot and nothing else. Once the watcher has gone, a config edit
    /// changes nothing the read can see — reading the project from disk is exactly what a
    /// request may not do — and the answer says it is no longer tracked.
    #[test]
    fn a_status_read_never_reads_the_project_from_disk() {
        let _env = super::test_support::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configuration(root, "cf", false);
        configuration(root, "ext", true);
        let config = root.join("bsl-analyzer.toml");
        std::fs::write(&config, "[source]\nroot = \"ext\"\nextensions = []\n").unwrap();

        let state = SharedState::workspace(root.to_path_buf()).unwrap();
        let seeded = state.standalone_notice().expect("an extension root carries the advisory");
        state.shutdown();
        assert!(
            crate::change_hub::test_support::eventually(
                std::time::Duration::from_secs(5),
                || state.owners.live() == 0
            ),
            "the watcher did not leave"
        );

        std::fs::write(&config, "[source]\nroot = \"cf\"\nextensions = []\n").unwrap();
        let served = state.standalone_notice().expect("the last value is still served");
        assert!(served.starts_with(&seeded), "the read re-derived the advisory: {served}");
        assert!(served.contains("no longer tracked"), "an untracked value must say so: {served}");
    }

    #[test]
    fn a_tracked_notice_is_served_verbatim_and_an_abandoned_one_says_so() {
        let mut notice = StandaloneNotice::tracked(Some("standalone".to_owned()));
        assert_eq!(notice.rendered().as_deref(), Some("standalone"));
        notice.abandon();
        let rendered = notice.rendered().unwrap();
        assert!(rendered.starts_with("standalone\n") && rendered.contains("no longer tracked"));
        assert_eq!(StandaloneNotice::tracked(None).rendered(), None);
    }
}

#[cfg(test)]
mod consumer_phase_tests {
    use super::{consumer_drift_watch, AbandonIfStill, ConsumerPhase};
    use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
    use crate::tools::location::DriftWatch;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A thread that never starts drops its closure unrun, and the guard moved into it names
    /// the consumer abandoned — the phase does not stay "starting" for the life of the process.
    /// A consumer that got past the guarded phase is left alone.
    #[test]
    fn a_consumer_whose_thread_never_ran_is_abandoned() {
        for (from, then) in [
            (ConsumerPhase::Pending, ConsumerPhase::Pending),
            (ConsumerPhase::Attaching, ConsumerPhase::Attaching),
            (ConsumerPhase::Pending, ConsumerPhase::Attaching),
        ] {
            let phase = Arc::new(Mutex::new(then));
            let guard = AbandonIfStill(Arc::clone(&phase), from);
            let never_run = move || drop(guard);
            drop(never_run);
            let expected = if from == then { ConsumerPhase::Abandoned } else { then };
            assert_eq!(*phase.lock().unwrap(), expected, "guarding {from:?}, found {then:?}");
        }
    }

    /// Attached vouches only for what reaches it: until the hub has armed (or fallen back to
    /// polling) nothing does, so the consumer still reads as starting.
    #[test]
    fn an_attached_consumer_on_a_hub_still_arming_is_starting() {
        let dir = tempfile::tempdir().unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);
        assert_eq!(consumer_drift_watch(ConsumerPhase::Attached, &hub), DriftWatch::Starting);
        assert_eq!(consumer_drift_watch(ConsumerPhase::Attaching, &hub), DriftWatch::Starting);
        hold.release();
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        assert_eq!(consumer_drift_watch(ConsumerPhase::Attached, &hub), DriftWatch::Watching);
        assert_eq!(consumer_drift_watch(ConsumerPhase::Attaching, &hub), DriftWatch::Starting);
        hub.shutdown();
    }
}

#[cfg(test)]
mod background_lifetime_tests {
    use super::{SemanticRuntimeStatus, SharedState};

    /// A shutdown is a request every background owner answers at once. The search consumer
    /// used to park in a 30-second hub wait and in retry sleeps of up to half an hour, so a
    /// daemon asked to stop kept writing into the workspace long after it had said goodbye.
    #[test]
    fn shutdown_stops_every_owner_within_a_second() {
        use crate::change_hub::test_support::eventually;
        use std::time::Duration;

        let _env = super::test_support::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let cf = dir.path().join("cf");
        std::fs::create_dir_all(cf.join("CommonModules").join("Общий").join("Ext")).unwrap();
        std::fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            cf.join("CommonModules").join("Общий").join("Ext").join("Module.bsl"),
            "Процедура П() Экспорт КонецПроцедуры\n",
        )
        .unwrap();
        let state = SharedState::workspace(dir.path().to_path_buf()).unwrap();
        assert!(
            eventually(Duration::from_secs(60), || state.owners.live() >= 1),
            "no background owner ever started, so the stop below would prove nothing"
        );
        // Parked in its wait: a consumer caught mid-batch leaves on its own anyway.
        std::thread::sleep(Duration::from_millis(300));

        state.shutdown();

        assert!(
            eventually(Duration::from_secs(1), || state.owners.live() == 0),
            "{} owner(s) still running a second after shutdown",
            state.owners.live()
        );
    }

    /// An owner waiting for the engine behind someone else's hold leaves on shutdown all the
    /// same: its place in the queue is given up, and the hold is not waited out.
    #[test]
    fn shutdown_does_not_wait_out_an_engine_hold() {
        use crate::change_hub::test_support::eventually;
        use std::time::Duration;

        let _env = super::test_support::env_lock();
        let _embedding_url = super::test_support::EnvVarGuard::unset("EMBEDDING_URL");
        let dir = tempfile::tempdir().unwrap();
        let cf = dir.path().join("cf");
        let module = cf.join("CommonModules").join("Общий").join("Ext").join("Module.bsl");
        std::fs::create_dir_all(module.parent().unwrap()).unwrap();
        std::fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(&module, "Процедура П() Экспорт КонецПроцедуры\n").unwrap();
        let state = SharedState::workspace(dir.path().to_path_buf()).unwrap();
        assert!(eventually(Duration::from_secs(30), || {
            state.search_watch().drift_watch == Some(crate::tools::location::DriftWatch::Watching)
        }));

        let hold = state.search_engine().lock().unwrap();
        std::fs::write(&module, "Процедура Н() Экспорт КонецПроцедуры\n").unwrap();
        assert!(
            eventually(Duration::from_secs(10), || state.search_engine().queued() >= 1),
            "no owner came for the engine, so the stop below would prove nothing"
        );
        state.shutdown();
        let left = eventually(Duration::from_secs(1), || state.owners.live() == 0);
        let live = state.owners.live();
        drop(hold);
        assert!(left, "{live} owner(s) still waiting for the engine a second after shutdown");
    }

    /// The daemon's own shutdown stops the hub's fallback poll, through the production path a
    /// daemon actually takes. A hub left running walks the whole workspace on a schedule
    /// nobody owns any more; interrupting its waiters alone does not stop a poll that never
    /// waits on them.
    #[test]
    fn shutdown_stops_a_polling_hub() {
        use crate::change_hub::test_support::eventually;
        use crate::change_hub::{PollConfig, WatchTarget, WorkspaceChangeHub};
        use std::time::Duration;

        const PERIOD: Duration = Duration::from_millis(20);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Модуль.bsl"), "Процедура П() КонецПроцедуры\n").unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_polling(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            PollConfig { period: PERIOD, verify_bytes: 64 * 1024 },
        );
        hold.release();
        assert!(
            eventually(Duration::from_secs(5), || hub.poll_count() >= 2),
            "the poll never ran, so stopping it would prove nothing"
        );

        let mut state = SharedState::shared();
        state.change_hub = Some(hub.clone());
        state.diagnostics.spawn_sweeper_for_test();
        assert!(eventually(Duration::from_secs(5), || state.diagnostics.sweeper_running()));

        state.shutdown();

        let stopped = hub.poll_count();
        std::thread::sleep(PERIOD * 6);
        assert_eq!(hub.poll_count(), stopped, "the poll outlived the daemon that owned it");
        assert!(
            eventually(Duration::from_secs(1), || !state.diagnostics.sweeper_running()),
            "the daemon's shutdown left the diagnostics sweeper running"
        );
    }

    /// The same for a hub that watches most of the tree and polls the part it cannot watch:
    /// that poller is a second thread with a stop of its own, and a shutdown that raises only
    /// the hub thread's leaves it walking the blind roots for ever.
    #[cfg(unix)]
    #[test]
    fn shutdown_stops_the_poll_of_a_blind_root() {
        use crate::change_hub::test_support::eventually;
        use crate::change_hub::{PollConfig, RefusedWatches, WatchTarget, WorkspaceChangeHub};
        use std::time::Duration;

        const PERIOD: Duration = Duration::from_millis(20);
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("наблюдаемый");
        let blind = dir.path().join("слепой");
        std::fs::create_dir_all(&watched).unwrap();
        std::fs::create_dir_all(&blind).unwrap();
        std::fs::write(blind.join("Модуль.bsl"), "Процедура П() КонецПроцедуры\n").unwrap();
        let refusals = RefusedWatches::refusing(vec![blind.clone()]);
        let hub = WorkspaceChangeHub::start_targets_refusing_polled(
            vec![WatchTarget::recursive(watched), WatchTarget::recursive(blind)],
            Duration::from_secs(3600),
            &refusals,
            PollConfig { period: PERIOD, verify_bytes: 64 * 1024 },
        );
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        assert!(eventually(Duration::from_secs(5), || hub.is_partially_blind()));
        assert!(
            eventually(Duration::from_secs(5), || hub.poll_count() >= 2),
            "the blind poll never ran, so stopping it would prove nothing"
        );
        assert!(hub.blind_poll_running());

        let mut state = SharedState::shared();
        state.change_hub = Some(hub.clone());

        state.shutdown();

        let stopped = hub.poll_count();
        std::thread::sleep(PERIOD * 6);
        assert_eq!(hub.poll_count(), stopped, "the blind poll outlived its daemon");
        assert!(
            eventually(Duration::from_secs(1), || !hub.blind_poll_running()),
            "the blind poller thread is still there"
        );
    }

    /// An owner asleep in a hub wait is released by the stop itself, not by the hub's own
    /// timeout. That wait is thirty seconds long, so a stop the hub never hears about leaves
    /// the daemon's search consumer parked for the rest of it — writing into the workspace
    /// long after the daemon said goodbye.
    #[test]
    fn a_stop_releases_an_owner_asleep_in_a_hub_wait() {
        use crate::change_hub::WorkspaceChangeHub;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));

        // Positive control: with nothing stopped and no event coming, the wait runs out on its
        // own — so the release below is the stop's doing and not the hub returning anyway.
        let quiet = hub.clone();
        let ran_out = {
            let since = quiet.generation();
            let started = Instant::now();
            quiet.wait_for_change(since, Duration::from_millis(300));
            started.elapsed()
        };
        assert!(
            ran_out >= Duration::from_millis(250),
            "the hub wait returned on its own in {ran_out:?}; it proves nothing about a stop"
        );

        let stop = super::OwnerStop::default();
        let woken = hub.clone();
        stop.wakes(move || woken.interrupt_waiters());

        let (left_tx, left) = std::sync::mpsc::channel();
        let owner_stop = stop.clone();
        let owner_hub = hub.clone();
        let live = stop.enter();
        std::thread::spawn(move || {
            let _live = live;
            let since = owner_hub.generation();
            owner_hub
                .wait_for_change_or(since, Duration::from_secs(30), || owner_stop.is_stopped());
            let _ = left_tx.send(());
        });
        // Asleep before the stop is raised: the ordering that used to cost a whole wait.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(stop.live(), 1, "the owner never started, so its exit proves nothing");

        let asked = Instant::now();
        stop.stop();

        assert!(
            left.recv_timeout(Duration::from_secs(1)).is_ok(),
            "the owner slept through its stop"
        );
        assert!(asked.elapsed() < Duration::from_secs(1));
        hub.shutdown();
    }

    #[test]
    fn active_indexing_keeps_the_backend_alive() {
        let state = SharedState::shared();
        assert!(!state.background_work_active());

        let _pass = state.index_progress().begin_pass();

        assert!(state.background_work_active());
    }

    /// The semantic-runtime status has two independent writers — the embed pass and the
    /// overlay worker — and each writes the whole slot. An embed finishing mid-overlay writes
    /// `Ready` over `OverlaySyncing`; while the backend's lifetime was read off that slot, the
    /// clobber erased the only sign of work and the daemon could exit in the middle of it.
    /// Activity is read from the work itself, which a peer's status write cannot touch.
    #[test]
    fn a_peer_status_write_cannot_release_a_running_pass() {
        let state = SharedState::shared();
        assert!(!state.background_work_active());

        // Positive control: an unclaimed flight would satisfy the assertions below whatever
        // the predicate reads.
        assert!(state.embed_flight.claim_for_test(), "the stand never took the flight");
        assert!(state.background_work_active(), "a claimed embed pass is not held");

        *state.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Ready;

        assert!(
            state.background_work_active(),
            "a peer's status write released a pass that is still running"
        );
    }
}

#[cfg(test)]
mod indexing_tests;
