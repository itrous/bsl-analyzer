use bsl_search::{
    fingerprint_documents, BaselineRef, CorpusId, Document, ExternalBaselineAdapter,
    ExternalBaselineBackend, ExternalBaselineConfig, IndexedDocument, ResolvedView,
    SnapshotCatalog, SnapshotContentStore, WorkspaceBaselineManifest,
};
use project_model::{
    current_git_branch, evaluate_workspace_baseline_support_now, parse_timestamp_utc,
    resolve_postgres_url, resolve_workspace_branch_policy, PostgresAccessMode, ProjectConfig,
    ResolvedWorkspaceBaselineSupport, SearchBaselineBackend, SearchBaselineConfig,
    SearchBaselinePolicyConfig, SearchBaselineSupportState, SearchBaselineTargetConfig,
    SearchPostgresConfig,
};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Answer a local test connection with a valid PostgreSQL startup error instead of
/// leaving the client waiting on a peer that only accepted TCP.
#[cfg(test)]
pub(crate) fn reject_postgres_startup_for_test(
    stream: &mut std::net::TcpStream,
) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind, Read, Write};

    // Accepted sockets inherit the listener's nonblocking mode on Windows.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if !(8..=64 * 1024).contains(&length) {
        return Err(Error::new(ErrorKind::InvalidData, "invalid PostgreSQL startup length"));
    }
    let mut startup = vec![0; length - 4];
    stream.read_exact(&mut startup)?;

    let fields = b"SFATAL\0C08006\0Msynthetic PostgreSQL test endpoint\0\0";
    stream.write_all(b"E")?;
    stream.write_all(&((fields.len() + 4) as u32).to_be_bytes())?;
    stream.write_all(fields)?;
    stream.flush()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineResolutionSummary {
    pub backend: String,
    pub selection: String,
    pub issue: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineConfigDiagnostics {
    pub workspace: BaselineResolutionSummary,
    pub reference: BaselineResolutionSummary,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BaselineSnapshotDocuments {
    pub snapshot_id: String,
    pub fingerprint: Option<String>,
    pub documents: Vec<IndexedDocument>,
    pub shared_embeddings: HashMap<String, Vec<f32>>,
}

#[derive(Debug, Clone)]
pub(crate) struct BaselineRuntime {
    pub configured_baseline: ConfiguredBaselineStatus,
    pub external_baseline: Option<Arc<ExternalBaselineService>>,
}

/// The outcome of baseline resolution. Ordinary entrypoints keep resolving the configured
/// credential helper synchronously; shared disk-only owners may instead retain a captured
/// credential plan until a search consumer activates. The PostgreSQL source/connect work
/// remains deferred behind `Connect` in either case.
// No `Debug`: `Connect` carries the resolved connection URL, credentials included —
// a stray `{:?}` in a log line must not be able to print it.
pub(crate) enum BaselineBootstrap {
    /// Resolution finished locally (sqlite backend, incomplete postgres config, or a
    /// credential failure): the runtime is final and carries the issue, if any.
    Immediate(BaselineRuntime),
    /// Credentials resolved; connecting to the baseline still requires network work.
    Connect(Box<BaselineConnectPlan>),
    /// Configuration and selection are captured, but the credential helper has not run.
    /// Used only by lazy shared-workspace consumers; ordinary entrypoints keep using
    /// `Immediate`/`Connect` and their historical synchronous credential resolution.
    ResolveCredentials(Box<BaselineCredentialPlan>),
}

impl BaselineBootstrap {
    pub(crate) fn with_token_layout_claim(mut self, claim: Option<String>) -> Self {
        match &mut self {
            Self::Connect(plan) => plan.context.token_layout_claim = claim,
            Self::ResolveCredentials(plan) => plan.context.token_layout_claim = claim,
            Self::Immediate(_) => {}
        }
        self
    }
    /// Run the deferred part (if any) right here. Sync callers — CLI config
    /// diagnostics, tests — keep the historical one-call behaviour through this.
    pub(crate) fn connect_now(self) -> BaselineRuntime {
        match self {
            Self::Immediate(runtime) => runtime,
            Self::Connect(plan) => plan.connect(),
            Self::ResolveCredentials(plan) => plan.resolve(),
        }
    }
}

/// A shared slot for a baseline runtime whose network part may still be connecting.
///
/// Mirrors the resident lifecycle idiom (`DiagnosticsStatus`/`GraphStatus`): an explicit
/// pending state under a mutex, filled exactly once by the `bsl-baseline-init` thread.
/// Readers never block (tools render "connecting" instead of erroring); the search-init
/// threads, which genuinely need the outcome, wait on the condvar with a bounded
/// timeout. `Pending` is deliberately distinct from "ready without a baseline": request
/// gates must answer "warming — retry" during the connect, not "fix config and restart".
#[derive(Clone, Debug)]
pub(crate) struct DeferredBaselineRuntime {
    inner: Arc<DeferredBaselineInner>,
}

#[derive(Debug)]
struct DeferredBaselineInner {
    slot: StdMutex<BaselineSlot>,
    ready: Condvar,
    closed: AtomicBool,
}

#[derive(Debug)]
enum BaselineSlot {
    Pending,
    Ready(Option<BaselineRuntime>),
}

/// A coherent single-lock snapshot of [`DeferredBaselineRuntime`], for handlers that
/// need the pending flag and the runtime pieces to describe the same instant.
pub(crate) struct BaselineView {
    pub pending: bool,
    pub configured: Option<ConfiguredBaselineStatus>,
    pub external: Option<Arc<ExternalBaselineService>>,
}

impl DeferredBaselineRuntime {
    /// A slot that is ready from the start (sqlite / misconfigured / test runtimes).
    pub(crate) fn ready(runtime: BaselineRuntime) -> Self {
        Self::with_slot(BaselineSlot::Ready(Some(runtime)))
    }

    /// A slot for profiles that have no baseline at all (disabled/shared state).
    pub(crate) fn absent() -> Self {
        Self::with_slot(BaselineSlot::Ready(None))
    }

    fn with_slot(slot: BaselineSlot) -> Self {
        Self {
            inner: Arc::new(DeferredBaselineInner {
                slot: StdMutex::new(slot),
                ready: Condvar::new(),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// A pending slot that does not start the deferred network work.
    pub(crate) fn pending() -> Self {
        Self::with_slot(BaselineSlot::Pending)
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test() -> Self {
        Self::pending()
    }

    /// Run the plan's network connect on a background thread and publish the outcome.
    ///
    /// The thread is deliberately detached (its `JoinHandle` is dropped): it performs
    /// exactly one bounded action and publishes into the slot, whose closed-flag
    /// handshake with [`Self::shutdown`] guarantees the produced service never
    /// outlives the owning state unmanaged — joining would add a shutdown stall for
    /// no extra safety.
    pub(crate) fn spawn(plan: BaselineConnectPlan) -> Self {
        let deferred = Self::pending();
        deferred.start(plan);
        deferred
    }

    /// Start a previously pending slot's one-shot network initialization.
    ///
    /// This lets disk-only workspace attachment preserve a resolved connection plan
    /// until an MCP search service actually activates. Publication and shutdown use
    /// the same close-flag handshake as [`Self::spawn`].
    pub(crate) fn start(&self, plan: BaselineConnectPlan) {
        if self.inner.closed.load(Ordering::Acquire) {
            return;
        }
        let publish_into = self.clone();
        let spawned = std::thread::Builder::new()
            .name("bsl-baseline-init".to_owned())
            .spawn(move || publish_into.publish(plan.connect()));
        if let Err(e) = spawned {
            // No thread — resolve the slot immediately so waiters cannot hang forever.
            tracing::warn!("could not spawn baseline connect thread: {e}");
            self.publish(BaselineRuntime {
                configured_baseline: ConfiguredBaselineStatus {
                    backend: "postgres",
                    selection: "shared baseline".to_owned(),
                    issue: Some(format!("could not spawn baseline connect thread: {e}")),
                    support: None,
                },
                external_baseline: None,
            });
        }
    }

    /// Start credential resolution and the resulting baseline connection off-thread.
    /// The worker rechecks closure before launching the external helper; publication
    /// retains the same close handshake as `start`/`spawn`.
    pub(crate) fn start_resolution(&self, plan: BaselineCredentialPlan) {
        if self.inner.closed.load(Ordering::Acquire) {
            return;
        }
        let publish_into = self.clone();
        let spawned =
            std::thread::Builder::new().name("bsl-baseline-init".to_owned()).spawn(move || {
                if publish_into.inner.closed.load(Ordering::Acquire) {
                    return;
                }
                publish_into.publish(plan.resolve());
            });
        if let Err(error) = spawned {
            if self.inner.closed.load(Ordering::Acquire) {
                return;
            }
            tracing::warn!("could not spawn baseline credential resolver: {error}");
            self.publish(BaselineRuntime {
                configured_baseline: ConfiguredBaselineStatus {
                    backend: "postgres",
                    selection: "shared baseline".to_owned(),
                    issue: Some(format!("could not spawn baseline credential resolver: {error}")),
                    support: None,
                },
                external_baseline: None,
            });
        }
    }

    fn publish(&self, runtime: BaselineRuntime) {
        let service = runtime.external_baseline.clone();
        let mut slot = self.inner.slot.lock().unwrap_or_else(|e| e.into_inner());
        *slot = BaselineSlot::Ready(Some(runtime));
        drop(slot);
        self.inner.ready.notify_all();
        // The closed check runs AFTER the store: a shutdown that flagged `closed` while
        // the slot was still pending found no service to stop, so stopping it is this
        // thread's job. Ordering guarantees no leak either way — whichever of the two
        // sides observes the other's write shuts the service down, and the service's
        // own shutdown is idempotent, so both doing it is harmless.
        if self.inner.closed.load(Ordering::SeqCst) {
            if let Some(service) = service {
                service.shutdown();
            }
        }
    }

    /// One consistent view of the slot under a single lock hold. Request handlers that
    /// combine the pending flag with the runtime pieces must use this: reading them
    /// through separate calls lets a publish land in between, and a torn
    /// `pending=false / configured=None / external=Some` mix reads as a config error.
    pub(crate) fn view(&self) -> BaselineView {
        match &*self.inner.slot.lock().unwrap_or_else(|e| e.into_inner()) {
            BaselineSlot::Pending => {
                BaselineView { pending: true, configured: None, external: None }
            }
            BaselineSlot::Ready(runtime) => BaselineView {
                pending: false,
                configured: runtime.as_ref().map(|r| r.configured_baseline.clone()),
                external: runtime.as_ref().and_then(|r| r.external_baseline.clone()),
            },
        }
    }

    /// Telemetry copies only the handle, never arbitrary configuration/error strings.
    /// Outer None means contention; inner None means no available baseline.
    pub(crate) fn try_external(&self) -> Option<Option<Arc<ExternalBaselineService>>> {
        let slot = self.inner.slot.try_lock().ok()?;
        Some(match &*slot {
            BaselineSlot::Ready(runtime) => {
                runtime.as_ref().and_then(|runtime| runtime.external_baseline.clone())
            }
            BaselineSlot::Pending => None,
        })
    }

    /// The service handle, if the runtime is ready and has one. Never blocks.
    pub(crate) fn external(&self) -> Option<Arc<ExternalBaselineService>> {
        match &*self.inner.slot.lock().unwrap_or_else(|e| e.into_inner()) {
            BaselineSlot::Ready(Some(runtime)) => runtime.external_baseline.clone(),
            _ => None,
        }
    }

    /// Block until the slot is resolved or `timeout` elapses. Returns whether it is
    /// resolved. For background init threads only — request paths must not wait.
    pub(crate) fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut slot = self.inner.slot.lock().unwrap_or_else(|e| e.into_inner());
        while matches!(*slot, BaselineSlot::Pending) && !self.inner.closed.load(Ordering::Acquire) {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return false;
            };
            let (guard, _timeout) =
                self.inner.ready.wait_timeout(slot, remaining).unwrap_or_else(|e| e.into_inner());
            slot = guard;
        }
        matches!(*slot, BaselineSlot::Ready(_))
    }

    /// Close the slot: a ready service shuts down now; a still-connecting one is shut
    /// down by [`Self::publish`] the moment it lands.
    pub(crate) fn shutdown(&self) {
        {
            let _slot = self.inner.slot.lock().unwrap_or_else(|e| e.into_inner());
            self.inner.closed.store(true, Ordering::Release);
        }
        self.inner.ready.notify_all();
        if let Some(service) = self.external() {
            service.shutdown();
        }
    }
}

/// Everything the deferred network step needs, captured by value so it can run on a
/// background thread after `SharedState` construction returned. No `Debug`:
/// `connection` is the resolved URL with live credentials.
pub(crate) struct BaselineConnectPlan {
    corpus: CorpusId,
    connection: String,
    schema: Option<String>,
    context: RefreshContext,
    selection: String,
    project_root: Option<PathBuf>,
    policy: SearchBaselinePolicyConfig,
}

/// A captured PostgreSQL configuration and selected corpus, waiting to invoke the
/// configured credential helper. Deliberately has no `Debug`: postgres config args
/// and the resolved connection may contain private credentials.
pub(crate) struct BaselineCredentialPlan {
    corpus: CorpusId,
    project_root: Option<PathBuf>,
    postgres: SearchPostgresConfig,
    schema: Option<String>,
    context: RefreshContext,
    selection: String,
    policy: SearchBaselinePolicyConfig,
}

impl BaselineCredentialPlan {
    pub(crate) fn corpus(&self) -> &CorpusId {
        &self.corpus
    }

    fn resolve(self) -> BaselineRuntime {
        let Self { corpus, project_root, postgres, schema, context, selection, policy } = self;
        match resolve_postgres_url(&postgres, PostgresAccessMode::Reader) {
            Ok(resolved) => BaselineConnectPlan {
                corpus,
                connection: resolved.url,
                schema,
                context,
                selection,
                project_root,
                policy,
            }
            .connect(),
            Err(error) => BaselineRuntime {
                configured_baseline: ConfiguredBaselineStatus {
                    backend: "postgres",
                    selection,
                    issue: Some(format!(
                        "failed to resolve PostgreSQL reader credentials: {error}"
                    )),
                    support: None,
                },
                external_baseline: None,
            },
        }
    }
}

impl BaselineConnectPlan {
    pub(crate) fn corpus(&self) -> &CorpusId {
        &self.corpus
    }

    /// The slow tail of baseline resolution: build the PG-backed source (network),
    /// resolve workspace support, and spawn the service actor. Mirrors the historical
    /// in-line tail of `BaselineRuntime::for_corpus` exactly, including its
    /// failure-to-issue folding.
    pub(crate) fn connect(self) -> BaselineRuntime {
        let Self { corpus, connection, schema, context, selection, project_root, policy } = self;
        match RefreshableExternalBaselineSource::new(connection, schema, context) {
            Ok(source) => {
                let support = if matches!(corpus, CorpusId::WorkspaceCode) {
                    resolve_workspace_support_status(project_root.as_deref(), &policy, &source)
                } else {
                    None
                };
                let service = ExternalBaselineService::spawn(source);
                tracing::info!(corpus = %corpus, "refreshable external baseline source configured");
                BaselineRuntime {
                    configured_baseline: ConfiguredBaselineStatus {
                        backend: "postgres",
                        selection,
                        issue: None,
                        support,
                    },
                    external_baseline: Some(Arc::new(service)),
                }
            }
            Err(error) => {
                tracing::warn!(corpus = %corpus, "failed to configure refreshable external baseline source: {error}");
                BaselineRuntime {
                    configured_baseline: ConfiguredBaselineStatus {
                        backend: "postgres",
                        selection,
                        issue: Some(error.to_string()),
                        support: None,
                    },
                    external_baseline: None,
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExternalBaselineService {
    corpus: CorpusId,
    schema: String,
    selection: String,
    local_reference_fingerprint: Option<String>,
    sender: mpsc::Sender<BaselineServiceRequest>,
    worker: StdMutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
    /// Shared with the actor worker so the status probe can run off the request queue:
    /// a slow `probe_status` (aggregate PG queries) must not delay queued searches, and
    /// searches must not delay `search status`.
    source: Arc<RefreshableExternalBaselineSource>,
    status_probe: Arc<StatusProbeState>,
}

/// How long a successful (`Ready`/`Missing`) status probe keeps serving from cache.
/// Baseline snapshots change only on publish, so a minute of staleness is honest —
/// the render says how old the probe is.
const STATUS_PROBE_TTL: Duration = Duration::from_secs(60);
/// A probe that ended in an error retries much sooner: transient auth/network failures
/// recover quickly after a credential refresh, and a minute of a stale error would
/// misreport a healthy baseline. The window still bounds probe storms under polling.
const STATUS_PROBE_ERROR_RETRY: Duration = Duration::from_secs(5);

/// A completed background status probe, served from cache by `probe_status_cached`.
#[derive(Debug, Clone)]
pub(crate) struct CachedBaselineStatus {
    pub status: ExternalBaselineStatus,
    pub probed_at: Instant,
    /// `refresh_generation` of the source observed after the probe; a credential
    /// refresh bumps it and invalidates the slot.
    generation: usize,
}

impl CachedBaselineStatus {
    pub(crate) fn age(&self) -> Duration {
        self.probed_at.elapsed()
    }
}

/// What a non-blocking status probe can report: the last completed probe, or nothing
/// yet (first call; a background probe has been kicked).
#[derive(Debug, Clone)]
pub(crate) enum BaselineStatusProbe {
    Cached(Box<CachedBaselineStatus>),
    Pending,
}

/// Owned by the service and captured by detached probe threads. Deliberately NOT a
/// reference to the service itself: a probe in flight must not keep the service (and
/// its actor worker) alive, and after `closed` it must drop its result on the floor.
#[derive(Debug)]
struct StatusProbeState {
    slot: StdMutex<Option<CachedBaselineStatus>>,
    refreshing: AtomicBool,
    closed: AtomicBool,
}

/// How a call to the baseline actor ended short of an answer.
///
/// Cancellation has a variant of its own rather than a [`bsl_search::SearchError`]: every
/// caller classifies a search error as terminal or transient and answers a transient one
/// with a fallback or a retry envelope — work and a body that a cancelled call must not
/// produce. An exhaustive match on this enum is what keeps `Withdrawn` out of those arms.
#[derive(Debug)]
pub(crate) enum BaselineCall {
    /// The caller's request was cancelled while it waited for the actor. The actor was
    /// not interrupted: a request still queued is skipped, one in flight completes and
    /// answers into a receiver nobody holds.
    Withdrawn,
    /// The actor answered with a failure.
    Failed(bsl_search::SearchError),
}

impl BaselineCall {
    /// The failure, for a caller that cannot be cancelled (a background pass holding a
    /// never-cancelled token). `Withdrawn` is unreachable for such a caller and is worded
    /// as the service refusing, so a misuse surfaces as an error rather than a silent skip.
    pub(crate) fn into_error(self) -> bsl_search::SearchError {
        match self {
            Self::Failed(error) => error,
            Self::Withdrawn => bsl_search::SearchError::Index(
                "baseline call withdrawn by a caller that cannot be cancelled".to_owned(),
            ),
        }
    }
}

/// The token of a caller that has no request to be cancelled by — bootstrap and the
/// background passes. Never cancelled, so such a caller only ever sees the actor's own
/// answer.
pub(crate) fn uncancellable() -> tokio_util::sync::CancellationToken {
    tokio_util::sync::CancellationToken::new()
}

/// One request to the actor, carrying the token of the caller that queued it: the worker
/// skips a request whose caller has already gone, so a cancelled search does not delay the
/// queue behind it with a query nobody will read.
#[derive(Debug)]
struct BaselineServiceRequest {
    cancel: tokio_util::sync::CancellationToken,
    kind: BaselineRequestKind,
}

#[derive(Debug)]
pub(crate) enum BaselineRequestKind {
    ResolveSnapshot {
        reply: mpsc::Sender<
            Result<Option<(BaselineRef, bsl_search::Snapshot)>, bsl_search::SearchError>,
        >,
    },
    LexicalSearch {
        snapshot_id: String,
        query: String,
        collection: Option<String>,
        limit: usize,
        reply: mpsc::Sender<Result<Vec<bsl_search::LexicalHit>, bsl_search::SearchError>>,
    },
    SemanticSearch {
        snapshot_id: String,
        query_embedding: Vec<f32>,
        model_id: String,
        dimension: usize,
        collection: Option<String>,
        limit: usize,
        reply: mpsc::Sender<Result<Vec<bsl_search::SemanticHit>, bsl_search::SearchError>>,
    },
    LoadReferenceSnapshotDocuments {
        model_id: Option<String>,
        dimension: Option<usize>,
        reply: mpsc::Sender<Result<Option<BaselineSnapshotDocuments>, bsl_search::SearchError>>,
    },
    LoadBaselineManifest {
        snapshot_id: String,
        reply: mpsc::Sender<Result<WorkspaceBaselineManifest, bsl_search::SearchError>>,
    },
    EmbeddingIdentity {
        reply: mpsc::Sender<Result<Option<(String, usize)>, bsl_search::SearchError>>,
    },
    Shutdown {
        reply: mpsc::Sender<()>,
    },
}

impl BaselineRuntime {
    pub(crate) fn workspace(project_root: Option<&Path>, project_config: &ProjectConfig) -> Self {
        Self::workspace_bootstrap(project_root, project_config).connect_now()
    }

    pub(crate) fn workspace_bootstrap(
        project_root: Option<&Path>,
        project_config: &ProjectConfig,
    ) -> BaselineBootstrap {
        Self::workspace_bootstrap_with_resolution(project_root, project_config, false)
    }

    /// Capture a workspace baseline plan without invoking its external credential helper.
    /// The shared disk-only owner starts this plan only when MCP workspace services activate.
    pub(crate) fn workspace_bootstrap_deferred(
        project_root: Option<&Path>,
        project_config: &ProjectConfig,
    ) -> BaselineBootstrap {
        Self::workspace_bootstrap_with_resolution(project_root, project_config, true)
    }

    fn workspace_bootstrap_with_resolution(
        project_root: Option<&Path>,
        project_config: &ProjectConfig,
        defer_credentials: bool,
    ) -> BaselineBootstrap {
        Self::bootstrap_for_corpus(
            CorpusId::WorkspaceCode,
            project_root,
            Some(&project_config.search.baseline),
            "BSL_SEARCH_BASELINE",
            &["BSL_SEARCH_BASELINE_PG_SCHEMA"],
            false,
            defer_credentials,
        )
    }

    pub(crate) fn reference(project_config: Option<&ProjectConfig>) -> Self {
        Self::reference_bootstrap(project_config).connect_now()
    }

    pub(crate) fn reference_bootstrap(project_config: Option<&ProjectConfig>) -> BaselineBootstrap {
        Self::reference_bootstrap_with_resolution(project_config, false)
    }

    /// Capture the reference baseline without running credentials; shared workspaces call
    /// this from the reference search's existing lazy activation path.
    pub(crate) fn reference_bootstrap_deferred(
        project_config: Option<&ProjectConfig>,
    ) -> BaselineBootstrap {
        Self::reference_bootstrap_with_resolution(project_config, true)
    }

    fn reference_bootstrap_with_resolution(
        project_config: Option<&ProjectConfig>,
        defer_credentials: bool,
    ) -> BaselineBootstrap {
        Self::bootstrap_for_corpus(
            CorpusId::Reference,
            None,
            project_config.map(|config| &config.search.baseline),
            "BSL_SEARCH_REFERENCE",
            &["BSL_SEARCH_REFERENCE_PG_SCHEMA", "BSL_SEARCH_BASELINE_PG_SCHEMA"],
            project_config.is_none(),
            defer_credentials,
        )
    }

    #[allow(clippy::too_many_arguments, reason = "private resolution chain uses all inputs")]
    fn bootstrap_for_corpus(
        corpus: CorpusId,
        project_root: Option<&Path>,
        project_config: Option<&SearchBaselineConfig>,
        selection_prefix: &str,
        schema_keys: &[&str],
        allow_env_backend_without_config: bool,
        defer_credentials: bool,
    ) -> BaselineBootstrap {
        let configured_backend = project_config.map(|config| config.backend.clone());
        let use_postgres = matches!(configured_backend, Some(SearchBaselineBackend::Postgres));
        if !use_postgres && !allow_env_backend_without_config {
            return BaselineBootstrap::Immediate(BaselineRuntime {
                configured_baseline: ConfiguredBaselineStatus {
                    backend: "sqlite",
                    selection: local_baseline_description(&corpus),
                    issue: None,
                    support: None,
                },
                external_baseline: None,
            });
        }

        let default_target = SearchBaselineTargetConfig::default();
        let baseline_target = project_config
            .map(|config| match corpus {
                CorpusId::WorkspaceCode => &config.workspace_code,
                CorpusId::Reference => &config.reference,
                CorpusId::Custom(_) => &config.workspace_code,
            })
            .unwrap_or(&default_target);
        let explicit_baseline =
            baseline_ref_from_config(corpus.clone(), selection_prefix, baseline_target);
        let (baselines, selection) = resolve_baseline_selection(
            &corpus,
            project_root,
            selection_prefix,
            baseline_target,
            &explicit_baseline,
        );

        let default_postgres = SearchPostgresConfig::default();
        let postgres = project_config.map(|config| &config.postgres).unwrap_or(&default_postgres);

        if !postgres.is_configured() {
            if use_postgres {
                return BaselineBootstrap::Immediate(BaselineRuntime {
                    configured_baseline: ConfiguredBaselineStatus {
                        backend: "postgres",
                        selection,
                        issue: Some(
                            "search.baseline.postgres is not configured; set host, dbname, schema, vault_role_base, and credential_helper.program"
                                .to_owned(),
                        ),
                        support: None,
                    },
                    external_baseline: None,
                });
            }

            return BaselineBootstrap::Immediate(BaselineRuntime {
                configured_baseline: ConfiguredBaselineStatus {
                    backend: "sqlite",
                    selection: local_baseline_description(&corpus),
                    issue: None,
                    support: None,
                },
                external_baseline: None,
            });
        }

        let schema = resolve_schema(schema_keys, postgres);
        let context = RefreshContext {
            token_layout_claim: None,
            postgres: postgres.clone(),
            baselines: baselines.clone(),
            selection: selection.clone(),
            schema_keys: schema_keys.iter().map(|s| s.to_string()).collect(),
        };

        let credential_plan = BaselineCredentialPlan {
            corpus,
            project_root: project_root.map(Path::to_path_buf),
            postgres: postgres.clone(),
            schema,
            context,
            selection,
            policy: baseline_target.policy.clone(),
        };
        if defer_credentials {
            BaselineBootstrap::ResolveCredentials(Box::new(credential_plan))
        } else {
            match resolve_postgres_url(&credential_plan.postgres, PostgresAccessMode::Reader) {
                Ok(resolved) => BaselineBootstrap::Connect(Box::new(BaselineConnectPlan {
                    corpus: credential_plan.corpus,
                    connection: resolved.url,
                    schema: credential_plan.schema,
                    context: credential_plan.context,
                    selection: credential_plan.selection,
                    project_root: credential_plan.project_root,
                    policy: credential_plan.policy,
                })),
                Err(error) => BaselineBootstrap::Immediate(BaselineRuntime {
                    configured_baseline: ConfiguredBaselineStatus {
                        backend: "postgres",
                        selection: credential_plan.selection,
                        issue: Some(format!(
                            "failed to resolve PostgreSQL reader credentials: {error}"
                        )),
                        support: None,
                    },
                    external_baseline: None,
                }),
            }
        }
    }

    fn summary(&self) -> BaselineResolutionSummary {
        BaselineResolutionSummary {
            backend: self.configured_baseline.backend.to_owned(),
            selection: self.configured_baseline.selection.clone(),
            issue: self.configured_baseline.issue.clone(),
        }
    }
}

pub fn resolve_project_baseline_diagnostics(
    project_root: Option<&Path>,
    project_config: &ProjectConfig,
) -> BaselineConfigDiagnostics {
    let workspace = BaselineRuntime::workspace(project_root, project_config).summary();
    let reference = BaselineRuntime::reference(Some(project_config)).summary();
    BaselineConfigDiagnostics { workspace, reference }
}

impl ExternalBaselineService {
    fn spawn(source: RefreshableExternalBaselineSource) -> Self {
        let corpus = source.corpus().clone();
        let schema = source._schema_for_status();
        let selection = source._selection();
        let local_reference_fingerprint = source.local_reference_fingerprint();
        let source = Arc::new(source);
        let (sender, receiver) = mpsc::channel();
        let worker_source = Arc::clone(&source);
        let worker = std::thread::Builder::new()
            .name(format!("baseline-service-{}", corpus.as_str()))
            .spawn(move || Self::worker_loop(worker_source, receiver))
            .expect("failed to spawn external baseline service worker");

        Self {
            corpus,
            schema,
            selection,
            local_reference_fingerprint,
            sender,
            worker: StdMutex::new(Some(worker)),
            closed: AtomicBool::new(false),
            source,
            status_probe: Arc::new(StatusProbeState {
                slot: StdMutex::new(None),
                refreshing: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(source: RefreshableExternalBaselineSource) -> Arc<Self> {
        Arc::new(Self::spawn(source))
    }

    fn worker_loop(
        source: Arc<RefreshableExternalBaselineSource>,
        receiver: mpsc::Receiver<BaselineServiceRequest>,
    ) {
        Self::serve(receiver, |kind| Self::dispatch(&source, kind));
    }

    /// The actor loop proper: one request at a time, in queue order, each handed to
    /// `handle` — the real dispatch in production, a scripted one in tests. A request
    /// whose caller has already been cancelled is skipped here, before any work: the
    /// answer would land on a receiver nobody holds, and the callers queued behind it
    /// would wait for nothing. A request already in `handle` is never interrupted.
    fn serve(
        receiver: mpsc::Receiver<BaselineServiceRequest>,
        mut handle: impl FnMut(BaselineRequestKind) -> std::ops::ControlFlow<()>,
    ) {
        while let Ok(BaselineServiceRequest { cancel, kind }) = receiver.recv() {
            if cancel.is_cancelled() && !matches!(kind, BaselineRequestKind::Shutdown { .. }) {
                continue;
            }
            if handle(kind).is_break() {
                break;
            }
        }
    }

    fn dispatch(
        source: &RefreshableExternalBaselineSource,
        kind: BaselineRequestKind,
    ) -> std::ops::ControlFlow<()> {
        match kind {
            BaselineRequestKind::ResolveSnapshot { reply } => {
                let _ = reply.send(source.resolve_snapshot());
            }
            BaselineRequestKind::LexicalSearch { snapshot_id, query, collection, limit, reply } => {
                let result =
                    source.lexical_search(&snapshot_id, &query, collection.as_deref(), limit);
                let _ = reply.send(result);
            }
            BaselineRequestKind::SemanticSearch {
                snapshot_id,
                query_embedding,
                model_id,
                dimension,
                collection,
                limit,
                reply,
            } => {
                let result = source.semantic_search(
                    &snapshot_id,
                    &query_embedding,
                    &model_id,
                    dimension,
                    collection.as_deref(),
                    limit,
                );
                let _ = reply.send(result);
            }
            BaselineRequestKind::LoadReferenceSnapshotDocuments { model_id, dimension, reply } => {
                let result =
                    source.load_reference_snapshot_documents(model_id.as_deref(), dimension);
                let _ = reply.send(result);
            }
            BaselineRequestKind::LoadBaselineManifest { snapshot_id, reply } => {
                let result = source.load_baseline_manifest(&snapshot_id);
                let _ = reply.send(result);
            }
            BaselineRequestKind::EmbeddingIdentity { reply } => {
                let _ = reply.send(source.embedding_identity());
            }
            BaselineRequestKind::Shutdown { reply } => {
                let _ = reply.send(());
                return std::ops::ControlFlow::Break(());
            }
        }
        std::ops::ControlFlow::Continue(())
    }

    /// A service whose actor runs `handle` instead of the PostgreSQL source, through the
    /// SAME `serve` loop as production — so a test that scripts the answers (or latches
    /// them) exercises the real queue discipline, cancelled-request skipping included.
    #[cfg(test)]
    pub(crate) fn with_worker_for_test(
        handle: impl FnMut(BaselineRequestKind) -> std::ops::ControlFlow<()> + Send + 'static,
    ) -> Arc<Self> {
        let source = RefreshableExternalBaselineSource::for_test(
            bsl_search::ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::WorkspaceCode,
                snapshot_id: None,
                branch: Some("main".to_owned()),
                commit: None,
            },
        )
        .expect("a test source builds without connecting");
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("baseline-service-test".to_owned())
            .spawn(move || Self::serve(receiver, handle))
            .expect("failed to spawn the test baseline worker");
        Arc::new(Self {
            corpus: CorpusId::WorkspaceCode,
            schema: "test".to_owned(),
            selection: "test".to_owned(),
            local_reference_fingerprint: None,
            sender,
            worker: StdMutex::new(Some(worker)),
            closed: AtomicBool::new(false),
            source: Arc::new(source),
            status_probe: Arc::new(StatusProbeState {
                slot: StdMutex::new(None),
                refreshing: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Queue one request and wait for its answer on behalf of `cancel`'s request.
    ///
    /// The wait ends at the answer or at the cancellation, whichever comes first. A
    /// cancelled caller drops its receiver and returns [`BaselineCall::Withdrawn`]; the
    /// request it queued is skipped by the worker if it has not started, and completes
    /// into the dead receiver if it has. Neither disturbs the requests of other callers.
    fn request<R>(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
        build: impl FnOnce(mpsc::Sender<R>) -> BaselineRequestKind,
    ) -> Result<R, BaselineCall>
    where
        R: Send + 'static,
    {
        if self.closed.load(Ordering::Acquire) {
            return Err(BaselineCall::Failed(service_closed_error(&self.corpus)));
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.sender
            .send(BaselineServiceRequest { cancel: cancel.clone(), kind: build(reply_tx) })
            .map_err(|_| BaselineCall::Failed(service_closed_error(&self.corpus)))?;
        match crate::tools::search::await_reply(&reply_rx, cancel, crate::tools::search::REPLY_POLL)
        {
            Ok(Some(answer)) => Ok(answer),
            Ok(None) => Err(BaselineCall::Failed(service_closed_error(&self.corpus))),
            Err(crate::tools::search::Withdrawn) => Err(BaselineCall::Withdrawn),
        }
    }

    /// Flatten the actor's own `Result` into the call outcome.
    fn answered<R>(
        answer: Result<Result<R, bsl_search::SearchError>, BaselineCall>,
    ) -> Result<R, BaselineCall> {
        answer?.map_err(BaselineCall::Failed)
    }

    pub(crate) fn lexical_search(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
        snapshot_id: &str,
        query: &str,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<bsl_search::LexicalHit>, BaselineCall> {
        Self::answered(self.request(cancel, |reply| BaselineRequestKind::LexicalSearch {
            snapshot_id: snapshot_id.to_owned(),
            query: query.to_owned(),
            collection: collection.map(ToOwned::to_owned),
            limit,
            reply,
        }))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the actor call takes the search's own identity values plus the caller's token; a one-use context struct would only rename them"
    )]
    pub(crate) fn semantic_search(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
        snapshot_id: &str,
        query_embedding: &[f32],
        model_id: &str,
        dimension: usize,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<bsl_search::SemanticHit>, BaselineCall> {
        Self::answered(self.request(cancel, |reply| BaselineRequestKind::SemanticSearch {
            snapshot_id: snapshot_id.to_owned(),
            query_embedding: query_embedding.to_vec(),
            model_id: model_id.to_owned(),
            dimension,
            collection: collection.map(ToOwned::to_owned),
            limit,
            reply,
        }))
    }

    /// Non-blocking status probe: returns the last completed probe immediately and,
    /// when it is stale (or absent), kicks at most one background re-probe. The probe
    /// runs off the actor queue on the shared PG pool, so `search status` never waits
    /// for the aggregate status queries and queued searches never wait for the probe.
    pub(crate) fn probe_status_cached(&self) -> BaselineStatusProbe {
        let cached =
            self.status_probe.slot.lock().expect("baseline status probe slot poisoned").clone();

        let generation = self.source.refresh_generation();
        let fresh = cached.as_ref().is_some_and(|entry| {
            let ttl = if matches!(entry.status.state, ExternalBaselineState::Error(_)) {
                STATUS_PROBE_ERROR_RETRY
            } else {
                STATUS_PROBE_TTL
            };
            entry.generation == generation && entry.age() < ttl
        });

        if !fresh
            && !self.status_probe.closed.load(Ordering::Acquire)
            && self
                .status_probe
                .refreshing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let probe = Arc::clone(&self.status_probe);
            let source = Arc::clone(&self.source);
            let spawned = std::thread::Builder::new()
                .name(format!("bsl-baseline-status-{}", self.corpus.as_str()))
                .spawn(move || {
                    // Re-arms the CAS gate on every exit path, panic included, so a
                    // failed probe can never wedge the cache in "refreshing" forever.
                    struct RearmOnDrop(Arc<StatusProbeState>);
                    impl Drop for RearmOnDrop {
                        fn drop(&mut self) {
                            self.0.refreshing.store(false, Ordering::Release);
                        }
                    }
                    let _rearm = RearmOnDrop(Arc::clone(&probe));
                    if probe.closed.load(Ordering::Acquire) {
                        return;
                    }
                    // The generation is read BEFORE the probe: a credential refresh landing
                    // mid-probe then makes this entry immediately stale (one extra re-probe)
                    // instead of stamping a result from the old source with the new
                    // generation and serving it as fresh for a whole TTL.
                    let generation = source.refresh_generation();
                    let status = source.probe_status();
                    if probe.closed.load(Ordering::Acquire) {
                        return;
                    }
                    *probe.slot.lock().expect("baseline status probe slot poisoned") =
                        Some(CachedBaselineStatus {
                            status,
                            probed_at: Instant::now(),
                            generation,
                        });
                });
            if spawned.is_err() {
                self.status_probe.refreshing.store(false, Ordering::Release);
            }
        }

        match cached {
            Some(entry) => BaselineStatusProbe::Cached(Box::new(entry)),
            None => BaselineStatusProbe::Pending,
        }
    }

    /// Observe existing evidence only; never trigger a probe or wait for an owner.
    pub(crate) fn indexing_publication(
        &self,
        model: &str,
        dimension: usize,
        overlay_identity: Option<&(String, Option<String>)>,
    ) -> BaselineIndexingPublication {
        use BaselineIndexingPublication::*;
        let Ok(slot) = self.status_probe.slot.try_lock() else { return SnapshotUnavailable };
        if self.status_probe.closed.load(Ordering::Acquire)
            || self.status_probe.refreshing.load(Ordering::Acquire)
        {
            return Stale;
        }
        let Some(cached) = slot.as_ref() else { return Stale };
        if cached.generation != self.source.refresh_generation() || cached.age() >= STATUS_PROBE_TTL
        {
            return Stale;
        }
        let ExternalBaselineState::Ready { snapshot_id, fingerprint, .. } = &cached.status.state
        else {
            return match cached.status.state {
                ExternalBaselineState::Missing => Unavailable,
                _ => SnapshotUnavailable,
            };
        };
        if !overlay_identity.is_some_and(|(id, fp)| id == snapshot_id && fp == fingerprint) {
            return Stale;
        }
        let Some(details) = &cached.status.semantic_details else { return UnverifiedCoverage };
        if details.snapshot_id != *snapshot_id || details.fingerprint != *fingerprint {
            return Stale;
        }
        let Some(publication) = &details.publication else { return UnverifiedIdentity };
        if publication.model_id != model || publication.dimension != dimension {
            return UnverifiedIdentity;
        }
        if publication.complete {
            Ready
        } else {
            UnverifiedCoverage
        }
    }

    /// Schema as rendered by `search status` while the first probe is still pending.
    pub(crate) fn schema_for_status(&self) -> &str {
        &self.schema
    }

    pub(crate) fn selection(&self) -> &str {
        &self.selection
    }

    #[cfg(test)]
    pub(crate) fn seed_status_cache_for_test(&self, status: ExternalBaselineStatus, age: Duration) {
        let probed_at = Instant::now().checked_sub(age).expect("test age under process uptime");
        *self.status_probe.slot.lock().expect("baseline status probe slot poisoned") =
            Some(CachedBaselineStatus {
                status,
                probed_at,
                generation: self.source.refresh_generation(),
            });
    }

    #[cfg(test)]
    pub(crate) fn status_probe_refreshing_for_test(&self) -> bool {
        self.status_probe.refreshing.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn status_probe_slot_for_test(&self) -> Option<CachedBaselineStatus> {
        self.status_probe.slot.lock().expect("baseline status probe slot poisoned").clone()
    }

    pub(crate) fn resolve_reference_view(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Option<ResolvedView>, BaselineCall> {
        let Some(snapshot) = self.load_reference_snapshot_documents(cancel, None, None)? else {
            return Ok(None);
        };
        let baseline = BaselineRef::for_snapshot(self.corpus.clone(), snapshot.snapshot_id);
        Ok(Some(ResolvedView::new(baseline, snapshot.documents)))
    }

    pub(crate) fn load_reference_snapshot_documents(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
        model_id: Option<&str>,
        dimension: Option<usize>,
    ) -> Result<Option<BaselineSnapshotDocuments>, BaselineCall> {
        Self::answered(self.request(cancel, |reply| {
            BaselineRequestKind::LoadReferenceSnapshotDocuments {
                model_id: model_id.map(ToOwned::to_owned),
                dimension,
                reply,
            }
        }))
    }

    pub(crate) fn load_baseline_manifest(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
        snapshot_id: &str,
    ) -> Result<WorkspaceBaselineManifest, BaselineCall> {
        Self::answered(self.request(cancel, |reply| BaselineRequestKind::LoadBaselineManifest {
            snapshot_id: snapshot_id.to_owned(),
            reply,
        }))
    }

    pub fn embedding_identity(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Option<(String, usize)>, BaselineCall> {
        Self::answered(
            self.request(cancel, |reply| BaselineRequestKind::EmbeddingIdentity { reply }),
        )
    }

    pub(crate) fn corpus(&self) -> CorpusId {
        self.corpus.clone()
    }

    pub(crate) fn local_reference_fingerprint(&self) -> Option<String> {
        self.local_reference_fingerprint.clone()
    }

    pub(crate) fn resolve_snapshot(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Option<(BaselineRef, bsl_search::Snapshot)>, BaselineCall> {
        Self::answered(self.request(cancel, |reply| BaselineRequestKind::ResolveSnapshot { reply }))
    }

    pub(crate) fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // A status probe still in flight now drops its result instead of publishing
        // into a cache nobody will read. The thread itself is not cancelled: it holds
        // the source Arc and possibly an in-flight PG call until that call returns —
        // an accepted trade-off; it never keeps the service or its actor worker alive.
        // The last published slot stays readable, but nothing renders it after
        // shutdown in the normal lifecycle.
        self.status_probe.closed.store(true, Ordering::Release);

        let (reply_tx, reply_rx) = mpsc::channel();
        let acknowledged = match self.sender.send(BaselineServiceRequest {
            cancel: tokio_util::sync::CancellationToken::new(),
            kind: BaselineRequestKind::Shutdown { reply: reply_tx },
        }) {
            Ok(()) => match reply_rx.recv_timeout(shutdown_ack_timeout()) {
                Ok(()) => true,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!(
                        corpus = %self.corpus,
                        timeout_ms = shutdown_ack_timeout().as_millis(),
                        "external baseline service shutdown timed out waiting for worker acknowledgement"
                    );
                    false
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::warn!(
                        corpus = %self.corpus,
                        "external baseline service shutdown acknowledgement channel disconnected"
                    );
                    false
                }
            },
            Err(_) => {
                tracing::warn!(
                    corpus = %self.corpus,
                    "external baseline service shutdown request could not be sent"
                );
                false
            }
        };

        if let Ok(mut worker) = self.worker.lock() {
            if let Some(handle) = worker.take() {
                if acknowledged || handle.is_finished() {
                    let _ = handle.join();
                } else {
                    drop(handle);
                }
            }
        }
    }
}

impl Drop for ExternalBaselineService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn service_closed_error(corpus: &CorpusId) -> bsl_search::SearchError {
    bsl_search::SearchError::ExternalBaseline(format!(
        "baseline_service_closed: external baseline service for {} is not available",
        corpus.as_str()
    ))
}

#[cfg(test)]
fn shutdown_ack_timeout() -> Duration {
    Duration::from_millis(100)
}

#[cfg(not(test))]
fn shutdown_ack_timeout() -> Duration {
    Duration::from_secs(2)
}

#[derive(Debug)]
pub(crate) struct ExternalBaselineSource {
    adapter: ExternalBaselineAdapter,
    baselines: Vec<BaselineRef>,
    selection: String,
}

#[derive(Debug)]
struct RefreshContext {
    token_layout_claim: Option<String>,
    postgres: SearchPostgresConfig,
    baselines: Vec<BaselineRef>,
    selection: String,
    schema_keys: Vec<String>,
}

/// A baseline's recorded embedding identity: model name and vector dimension.
type EmbeddingIdentity = (String, usize);

/// Memoized embedding identity keyed by `refresh_generation`. Outer `Option` = not yet
/// populated; inner `Option` = the recorded identity (`None` when the baseline has none).
type EmbeddingIdentityCache = StdMutex<Option<(usize, Option<EmbeddingIdentity>)>>;

/// A resolved baseline snapshot: which candidate matched and the snapshot it points at.
type ResolvedSnapshot = (BaselineRef, bsl_search::Snapshot);

/// Memoized `resolve_snapshot` result. The tuple is `(generation, resolved_at, snapshot)`.
/// Only a resolved snapshot is cached — a `None` resolution (no baseline yet) and errors are
/// never stored, so a caller that must see the current state (the workspace boot path fails
/// closed on `None`) always re-resolves and a freshly published snapshot is picked up at once.
type SnapshotCache = StdMutex<Option<(usize, Instant, ResolvedSnapshot)>>;

/// How long a resolved snapshot keeps serving from cache. A published snapshot changes the
/// resolution without bumping `refresh_generation` (only a credential refresh bumps it), so a
/// TTL — not just the generation key — bounds the staleness window: an *updated* snapshot on an
/// already-resolved baseline is picked up within a minute. This is the documented fresh-search
/// window. (A baseline going from unresolved to resolved is picked up immediately, since `None`
/// is never cached.)
const SNAPSHOT_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct RefreshableExternalBaselineSource {
    inner: StdRwLock<ExternalBaselineSource>,
    context: RefreshContext,
    refresh_generation: AtomicUsize,
    refresh_lock: StdMutex<()>,
    /// Invalidated by `refresh_inner` whenever credentials are refreshed (the generation bumps).
    embedding_identity_cache: EmbeddingIdentityCache,
    /// Memoized `resolve_snapshot`, keyed by `refresh_generation` plus a TTL. Every search
    /// resolves the snapshot, and each resolution is a PG round-trip; the cache keeps that off
    /// the hot path. See [`SNAPSHOT_CACHE_TTL`] for why the TTL is required on top of the key.
    snapshot_cache: SnapshotCache,
}

impl RefreshableExternalBaselineSource {
    fn new(
        connection_url: String,
        schema: Option<String>,
        context: RefreshContext,
    ) -> Result<Self, bsl_search::SearchError> {
        let mut config = ExternalBaselineConfig::postgres(connection_url);
        if let Some(schema) = schema {
            config = config.with_schema(schema);
        }
        let inner = StdRwLock::new(
            ExternalBaselineSource::new_with_candidates(
                config,
                context.baselines.clone(),
                context.selection.clone(),
            )?
            .with_token_layout_claim(context.token_layout_claim.as_deref())?,
        );
        Ok(Self {
            inner,
            context,
            refresh_generation: AtomicUsize::new(0),
            refresh_lock: StdMutex::new(()),
            embedding_identity_cache: StdMutex::new(None),
            snapshot_cache: StdMutex::new(None),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        config: ExternalBaselineConfig,
        baseline: BaselineRef,
    ) -> Result<Self, bsl_search::SearchError> {
        let selection = baseline_description(&baseline);
        let inner = StdRwLock::new(ExternalBaselineSource::new_with_candidates(
            config,
            vec![baseline.clone()],
            selection.clone(),
        )?);
        let context = RefreshContext {
            token_layout_claim: None,
            postgres: SearchPostgresConfig::default(),
            baselines: vec![baseline],
            selection,
            schema_keys: vec![],
        };
        Ok(Self {
            inner,
            context,
            refresh_generation: AtomicUsize::new(0),
            embedding_identity_cache: StdMutex::new(None),
            snapshot_cache: StdMutex::new(None),
            refresh_lock: StdMutex::new(()),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_refresh_context(
        config: ExternalBaselineConfig,
        baseline: BaselineRef,
        postgres: SearchPostgresConfig,
    ) -> Result<Self, bsl_search::SearchError> {
        let selection = baseline_description(&baseline);
        let inner = StdRwLock::new(ExternalBaselineSource::new_with_candidates(
            config,
            vec![baseline.clone()],
            selection.clone(),
        )?);
        let context = RefreshContext {
            postgres,
            baselines: vec![baseline],
            selection,
            schema_keys: vec![],
            token_layout_claim: None,
        };
        Ok(Self {
            inner,
            context,
            refresh_generation: AtomicUsize::new(0),
            embedding_identity_cache: StdMutex::new(None),
            snapshot_cache: StdMutex::new(None),
            refresh_lock: StdMutex::new(()),
        })
    }

    fn refresh_generation(&self) -> usize {
        self.refresh_generation.load(Ordering::Acquire)
    }

    fn run_with_refresh<F, T>(&self, operation: F) -> Result<T, RefreshOrTerminalError>
    where
        F: Fn(&ExternalBaselineSource) -> Result<T, bsl_search::SearchError>,
    {
        let first_error = {
            let reader = self.inner.read().expect("baseline source lock poisoned");
            match operation(&reader) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if !error.is_retryable() {
                        return Err(RefreshOrTerminalError::Terminal(error));
                    }
                    error
                }
            }
        };

        let generation_before = self.refresh_generation.load(Ordering::Acquire);
        let _refresh_guard = self.refresh_lock.lock().expect("baseline refresh lock poisoned");

        if self.refresh_generation.load(Ordering::Acquire) != generation_before {
            let reader = self.inner.read().expect("baseline source lock poisoned");
            return match operation(&reader) {
                Ok(value) => Ok(value),
                Err(error) => {
                    tracing::warn!(
                        generation = generation_before,
                        "refreshable external baseline source retry after concurrent refresh failed: {error}"
                    );
                    if error.is_retryable() {
                        Err(RefreshOrTerminalError::Terminal(
                            RefreshAttemptError::RetryExhausted { source: error }
                                .into_search_error(),
                        ))
                    } else {
                        Err(RefreshOrTerminalError::Terminal(error))
                    }
                }
            };
        }

        match self.refresh_inner() {
            Ok(()) => {
                let reader = self.inner.read().expect("baseline source lock poisoned");
                match operation(&reader) {
                    Ok(value) => {
                        tracing::info!(
                            generation = generation_before,
                            "refreshable external baseline source recovered after credential refresh"
                        );
                        Ok(value)
                    }
                    Err(error) => {
                        tracing::warn!(
                            generation = generation_before,
                            "refreshable external baseline source retry after refresh failed: {error}"
                        );
                        if error.is_retryable() {
                            Err(RefreshOrTerminalError::Terminal(
                                RefreshAttemptError::RetryExhausted { source: error }
                                    .into_search_error(),
                            ))
                        } else {
                            Err(RefreshOrTerminalError::Terminal(error))
                        }
                    }
                }
            }
            Err(refresh_err) => {
                tracing::warn!(
                    generation = generation_before,
                    first_error = %first_error,
                    "refreshable external baseline source re-resolve failed: {refresh_err}"
                );
                Err(RefreshOrTerminalError::Terminal(refresh_err.into_search_error()))
            }
        }
    }

    fn refresh_inner(&self) -> Result<(), RefreshAttemptError> {
        let resolved = resolve_postgres_url(&self.context.postgres, PostgresAccessMode::Reader)
            .map_err(RefreshAttemptError::Resolve)?;
        let schema = resolve_schema_vec(&self.context.schema_keys, &self.context.postgres);

        let mut fresh_config = ExternalBaselineConfig::postgres(resolved.url);
        if let Some(schema) = schema {
            fresh_config = fresh_config.with_schema(schema);
        }

        let fresh_source = ExternalBaselineSource::new_with_candidates(
            fresh_config,
            self.context.baselines.clone(),
            self.context.selection.clone(),
        )
        .and_then(|source| {
            source.with_token_layout_claim(self.context.token_layout_claim.as_deref())
        })
        .map_err(RefreshAttemptError::Build)?;

        {
            let mut writer = self.inner.write().expect("baseline source lock poisoned");
            *writer = fresh_source;
        }

        let old = self.refresh_generation.fetch_add(1, Ordering::SeqCst);
        tracing::info!(
            old_generation = old,
            new_generation = old + 1,
            "refreshable external baseline source credentials refreshed"
        );

        Ok(())
    }

    fn delegate<F, T>(&self, operation: F) -> Result<T, bsl_search::SearchError>
    where
        F: Fn(&ExternalBaselineSource) -> Result<T, bsl_search::SearchError>,
    {
        self.run_with_refresh(operation).map_err(|e| match e {
            RefreshOrTerminalError::Terminal(err) => err,
        })
    }

    pub(crate) fn lexical_search(
        &self,
        snapshot_id: &str,
        query: &str,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<bsl_search::LexicalHit>, bsl_search::SearchError> {
        self.delegate(|source| source.lexical_search(snapshot_id, query, collection, limit))
    }

    pub(crate) fn semantic_search(
        &self,
        snapshot_id: &str,
        query_embedding: &[f32],
        model_id: &str,
        dimension: usize,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<bsl_search::SemanticHit>, bsl_search::SearchError> {
        self.delegate(|source| {
            source.semantic_search(
                snapshot_id,
                query_embedding,
                model_id,
                dimension,
                collection,
                limit,
            )
        })
    }

    pub(crate) fn probe_status(&self) -> ExternalBaselineStatus {
        let (backend, schema, selection) = {
            let reader = self.inner.read().expect("baseline source lock poisoned");
            let backend = match reader.adapter.config().backend {
                ExternalBaselineBackend::Postgres => "postgres",
            };
            let schema =
                reader.adapter.config().schema.clone().unwrap_or_else(|| "bsl_search".to_owned());
            (backend, schema, reader.selection.clone())
        };

        match self.run_with_refresh(|source| source.probe_status_result()) {
            Ok(status) => status,
            Err(RefreshOrTerminalError::Terminal(error)) => ExternalBaselineStatus {
                backend,
                schema,
                selection,
                resolved: None,
                semantic_details: None,
                state: ExternalBaselineState::Error(error.to_string()),
            },
        }
    }

    pub(crate) fn load_reference_snapshot_documents(
        &self,
        model_id: Option<&str>,
        dimension: Option<usize>,
    ) -> Result<Option<BaselineSnapshotDocuments>, bsl_search::SearchError> {
        self.delegate(|source| source.load_reference_snapshot_documents(model_id, dimension))
    }

    pub(crate) fn load_baseline_manifest(
        &self,
        snapshot_id: &str,
    ) -> Result<WorkspaceBaselineManifest, bsl_search::SearchError> {
        self.delegate(|source| source.load_baseline_manifest(snapshot_id))
    }

    pub(crate) fn corpus(&self) -> CorpusId {
        let reader = self.inner.read().expect("baseline source lock poisoned");
        reader.corpus().clone()
    }

    pub(crate) fn snapshot_details(
        &self,
        snapshot_id: &str,
    ) -> Result<Option<bsl_search::BaselineSnapshotDetails>, bsl_search::SearchError> {
        self.delegate(|source| source.snapshot_details(snapshot_id))
    }

    pub(crate) fn local_reference_fingerprint(&self) -> Option<String> {
        let reader = self.inner.read().expect("baseline source lock poisoned");
        reader.local_reference_fingerprint()
    }

    pub(crate) fn resolve_snapshot(
        &self,
    ) -> Result<Option<(BaselineRef, bsl_search::Snapshot)>, bsl_search::SearchError> {
        // Every search resolves the snapshot, and each resolution is a PG round-trip. A resolved
        // snapshot is near-immutable, so memoize it keyed by `refresh_generation` (a credential
        // refresh bumps it and invalidates the slot) plus a TTL (a snapshot publish does NOT bump
        // the generation, so the TTL bounds the staleness — see `SNAPSHOT_CACHE_TTL`). `None` and
        // errors are never cached: callers that must observe the current state re-resolve every
        // call, so an unresolved→resolved transition is picked up immediately.
        //
        // The generation is sampled before the resolve. A credential refresh racing the round-trip
        // can only make the sampled generation stale, never fresher, so the worst case is a slot
        // stamped with a superseded generation — the next lookup sees the bumped generation, misses,
        // and re-resolves. A stale value is therefore never served past the refresh.
        let generation = self.refresh_generation.load(Ordering::Acquire);
        {
            let cache = self.snapshot_cache.lock().expect("baseline snapshot cache poisoned");
            if let Some((cached_generation, resolved_at, resolved)) = cache.as_ref() {
                if *cached_generation == generation && resolved_at.elapsed() < SNAPSHOT_CACHE_TTL {
                    tracing::debug!(
                        generation,
                        age_ms = resolved_at.elapsed().as_millis() as u64,
                        "resolve_snapshot served from cache"
                    );
                    return Ok(Some(resolved.clone()));
                }
            }
        }

        tracing::debug!(generation, "resolve_snapshot cache miss, resolving from storage");
        let resolved = self.delegate(|source| source.resolve_snapshot())?;

        if let Some(resolved) = &resolved {
            let mut cache = self.snapshot_cache.lock().expect("baseline snapshot cache poisoned");
            *cache = Some((generation, Instant::now(), resolved.clone()));
        }
        Ok(resolved)
    }

    #[cfg(test)]
    pub(crate) fn seed_snapshot_cache_for_test(&self, resolved: ResolvedSnapshot, age: Duration) {
        let resolved_at = Instant::now().checked_sub(age).expect("test age under process uptime");
        *self.snapshot_cache.lock().expect("baseline snapshot cache poisoned") =
            Some((self.refresh_generation(), resolved_at, resolved));
    }

    #[cfg(test)]
    pub(crate) fn snapshot_cache_slot_for_test(
        &self,
    ) -> Option<(usize, Instant, ResolvedSnapshot)> {
        self.snapshot_cache.lock().expect("baseline snapshot cache poisoned").clone()
    }

    #[cfg(test)]
    pub(crate) fn bump_refresh_generation_for_test(&self) {
        self.refresh_generation.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn _selection(&self) -> String {
        self.context.selection.clone()
    }

    pub(crate) fn _schema_for_status(&self) -> String {
        let reader = self.inner.read().expect("baseline source lock poisoned");
        reader.adapter.config().schema.clone().unwrap_or_else(|| "bsl_search".to_owned())
    }

    fn embedding_identity(&self) -> Result<Option<(String, usize)>, bsl_search::SearchError> {
        // The identity is immutable for a baseline, so memoize it and re-read only after a
        // refresh swaps the adapter (keyed by `refresh_generation`). This keeps the per-query
        // semantic path off the DB. A read error is not cached, so a transient failure recovers
        // on the next call. `refresh_inner` only takes `inner.write()` (never this cache lock),
        // so there is no lock-ordering cycle with the read lock taken below.
        let generation = self.refresh_generation.load(Ordering::Acquire);
        let mut cache =
            self.embedding_identity_cache.lock().expect("embedding identity cache poisoned");
        if let Some((cached_generation, value)) = cache.as_ref() {
            if *cached_generation == generation {
                return Ok(value.clone());
            }
        }
        let value = {
            let reader = self.inner.read().expect("baseline source lock poisoned");
            reader.adapter.read_embedding_identity()?
        };
        *cache = Some((generation, value.clone()));
        Ok(value)
    }
}

enum RefreshOrTerminalError {
    Terminal(bsl_search::SearchError),
}

#[derive(Debug)]
enum RefreshAttemptError {
    Resolve(project_model::ResolvePostgresUrlError),
    Build(bsl_search::SearchError),
    RetryExhausted { source: bsl_search::SearchError },
}

impl RefreshAttemptError {
    fn into_search_error(self) -> bsl_search::SearchError {
        match self {
            Self::Resolve(error) => bsl_search::SearchError::ExternalBaseline(format!(
                "{}: {error}",
                resolve_reason_code(&error)
            )),
            Self::Build(error) => error,
            Self::RetryExhausted { source } => bsl_search::SearchError::ExternalBaseline(format!(
                "refresh_retry_exhausted: {source}"
            )),
        }
    }
}

impl std::fmt::Display for RefreshAttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolve(err) => write!(f, "resolve: {err}"),
            Self::Build(err) => write!(f, "build: {err}"),
            Self::RetryExhausted { source } => write!(f, "retry exhausted: {source}"),
        }
    }
}

fn resolve_reason_code(error: &project_model::ResolvePostgresUrlError) -> &'static str {
    use project_model::ResolvePostgresUrlError;

    match error {
        ResolvePostgresUrlError::MissingField(_)
        | ResolvePostgresUrlError::MissingCredentialHelper => "missing_config",
        ResolvePostgresUrlError::HelperSpawn { .. } => "helper_spawn_failed",
        ResolvePostgresUrlError::HelperTimeout { .. } => "helper_timeout",
        ResolvePostgresUrlError::HelperProtocol { .. } => "helper_protocol_error",
        ResolvePostgresUrlError::HelperRejected { .. } => "helper_rejected",
        ResolvePostgresUrlError::UnsupportedUrlScheme(_)
        | ResolvePostgresUrlError::InvalidResolvedUrl(_) => "helper_protocol_error",
        ResolvePostgresUrlError::TargetMismatch { .. } => "resolved_target_mismatch",
    }
}

impl ExternalBaselineSource {
    fn with_token_layout_claim(
        mut self,
        claim: Option<&str>,
    ) -> Result<Self, bsl_search::SearchError> {
        if let Some(claim) = claim {
            self.adapter =
                self.adapter.with_token_layout_claim(bsl_search::SEGMENTATION_VERSION, claim)?;
        }
        Ok(self)
    }
    #[cfg(test)]
    pub(crate) fn new(
        config: ExternalBaselineConfig,
        baseline: BaselineRef,
    ) -> Result<Self, bsl_search::SearchError> {
        let selection = baseline_description(&baseline);
        Self::new_with_candidates(config, vec![baseline], selection)
    }

    pub(crate) fn new_with_candidates(
        config: ExternalBaselineConfig,
        baselines: Vec<BaselineRef>,
        selection: String,
    ) -> Result<Self, bsl_search::SearchError> {
        let adapter = ExternalBaselineAdapter::new(config)?;
        Ok(Self { adapter, baselines, selection })
    }

    pub(crate) fn lexical_search(
        &self,
        snapshot_id: &str,
        query: &str,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<bsl_search::LexicalHit>, bsl_search::SearchError> {
        self.adapter.lexical_search_baseline(snapshot_id, query, collection, limit)
    }

    pub(crate) fn semantic_search(
        &self,
        snapshot_id: &str,
        query_embedding: &[f32],
        model_id: &str,
        dimension: usize,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<bsl_search::SemanticHit>, bsl_search::SearchError> {
        self.adapter.semantic_search_baseline(
            snapshot_id,
            query_embedding,
            model_id,
            dimension,
            collection,
            limit,
        )
    }

    #[cfg(test)]
    pub(crate) fn probe_status(&self) -> ExternalBaselineStatus {
        match self.probe_status_result() {
            Ok(status) => status,
            Err(error) => ExternalBaselineStatus {
                backend: match self.adapter.config().backend {
                    ExternalBaselineBackend::Postgres => "postgres",
                },
                schema: self
                    .adapter
                    .config()
                    .schema
                    .clone()
                    .unwrap_or_else(|| "bsl_search".to_owned()),
                selection: self.selection.clone(),
                resolved: None,
                semantic_details: None,
                state: ExternalBaselineState::Error(error.to_string()),
            },
        }
    }

    fn probe_status_result(&self) -> Result<ExternalBaselineStatus, bsl_search::SearchError> {
        let backend = match self.adapter.config().backend {
            ExternalBaselineBackend::Postgres => "postgres",
        };
        let schema =
            self.adapter.config().schema.clone().unwrap_or_else(|| "bsl_search".to_owned());
        let selection = self.selection.clone();

        match self.resolve_snapshot()? {
            Some((resolved_baseline, snapshot)) => {
                // Counts only — do NOT load the serving rows. `load_snapshot_documents` pulls the
                // entire baseline corpus from Postgres (~228K rows) just to count it, which on
                // the status path runs under the engine lock and stalls `search status` past the
                // client timeout. `snapshot_details` returns aggregated counts from
                // snapshot/file-object metadata (O(files), not O(serving rows)) instead.
                let snapshot_id_str = snapshot.id.0.clone();
                let (documents, files, semantic_details) = match self
                    .adapter
                    .snapshot_details(&snapshot_id_str)?
                {
                    Some(details) => (
                        details.snapshot.documents,
                        details.snapshot.files,
                        Some(BaselineSemanticDetails {
                            snapshot_id: details.snapshot.snapshot_id,
                            fingerprint: details.snapshot.fingerprint,
                            publication: details.semantic_publication,
                        }),
                    ),
                    None => {
                        // The snapshot resolved above but its detail row was not found —
                        // a brief metadata race. Report 0/0 (counts unavailable, not an empty
                        // baseline) rather than failing the whole status call.
                        tracing::warn!(
                            snapshot_id = %snapshot_id_str,
                            "probe_status: snapshot_details returned None for a resolved snapshot; \
                             reporting counts as 0 (metadata race)"
                        );
                        (0, 0, None)
                    }
                };
                Ok(ExternalBaselineStatus {
                    backend,
                    schema,
                    selection,
                    resolved: Some(baseline_description(&resolved_baseline)),
                    semantic_details,
                    state: ExternalBaselineState::Ready {
                        snapshot_id: snapshot.id.0,
                        fingerprint: snapshot.fingerprint,
                        documents,
                        files,
                    },
                })
            }
            None => Ok(ExternalBaselineStatus {
                backend,
                schema,
                selection,
                resolved: None,
                semantic_details: None,
                state: ExternalBaselineState::Missing,
            }),
        }
    }

    pub(crate) fn snapshot_details(
        &self,
        snapshot_id: &str,
    ) -> Result<Option<bsl_search::BaselineSnapshotDetails>, bsl_search::SearchError> {
        self.adapter.snapshot_details(snapshot_id)
    }

    pub(crate) fn load_reference_snapshot_documents(
        &self,
        model_id: Option<&str>,
        dimension: Option<usize>,
    ) -> Result<Option<BaselineSnapshotDocuments>, bsl_search::SearchError> {
        if !matches!(self.corpus(), CorpusId::Reference) {
            return Ok(None);
        }
        self.load_snapshot_documents(model_id, dimension)
    }

    fn load_snapshot_documents(
        &self,
        model_id: Option<&str>,
        dimension: Option<usize>,
    ) -> Result<Option<BaselineSnapshotDocuments>, bsl_search::SearchError> {
        let Some((_, snapshot)) = self.resolve_snapshot()? else {
            return Ok(None);
        };
        let documents = self.adapter.load_snapshot_documents(&snapshot)?;
        let shared_embeddings = if let (Some(model_id), Some(dimension)) = (model_id, dimension) {
            let embedding_keys = documents
                .iter()
                .map(bsl_search::semantic_key_for_indexed_document)
                .collect::<Vec<_>>();
            self.adapter.load_embeddings(&embedding_keys, model_id, dimension)?
        } else {
            HashMap::new()
        };
        Ok(Some(BaselineSnapshotDocuments {
            snapshot_id: snapshot.id.0,
            fingerprint: snapshot.fingerprint,
            documents,
            shared_embeddings,
        }))
    }

    pub(crate) fn load_baseline_manifest(
        &self,
        snapshot_id: &str,
    ) -> Result<WorkspaceBaselineManifest, bsl_search::SearchError> {
        self.adapter.load_baseline_manifest(snapshot_id)
    }

    pub(crate) fn corpus(&self) -> &CorpusId {
        &self.baselines[0].corpus
    }

    pub(crate) fn local_reference_fingerprint(&self) -> Option<String> {
        if !matches!(self.corpus(), CorpusId::Reference) {
            return None;
        }
        Some(fingerprint_documents(&platform_reference_documents()))
    }

    pub(crate) fn resolve_snapshot(
        &self,
    ) -> Result<Option<(BaselineRef, bsl_search::Snapshot)>, bsl_search::SearchError> {
        for baseline in &self.baselines {
            if let Some(snapshot) = self.adapter.resolve_baseline(baseline)? {
                return Ok(Some((baseline.clone(), snapshot)));
            }
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalBaselineStatus {
    pub backend: &'static str,
    pub schema: String,
    pub selection: String,
    pub resolved: Option<String>,
    pub state: ExternalBaselineState,
    pub semantic_details: Option<BaselineSemanticDetails>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaselineSemanticDetails {
    pub snapshot_id: String,
    pub fingerprint: Option<String>,
    pub publication: Option<bsl_search::BaselineSemanticPublication>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaselineIndexingPublication {
    Ready,
    UnverifiedIdentity,
    UnverifiedCoverage,
    Stale,
    Unavailable,
    SnapshotUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalBaselineState {
    Ready { snapshot_id: String, fingerprint: Option<String>, documents: usize, files: usize },
    Missing,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfiguredBaselineStatus {
    pub backend: &'static str,
    pub selection: String,
    pub issue: Option<String>,
    pub support: Option<ResolvedWorkspaceBaselineSupport>,
}

impl ConfiguredBaselineStatus {
    pub fn search_is_expired(&self) -> bool {
        self.support
            .as_ref()
            .is_some_and(|support| matches!(support.state, SearchBaselineSupportState::Expired))
    }
}

pub(crate) fn baseline_description(baseline: &BaselineRef) -> String {
    if let Some(snapshot_id) = &baseline.snapshot_id {
        return format!("snapshot {}", snapshot_id.0);
    }
    if let (Some(branch), Some(commit)) = (&baseline.branch, &baseline.commit) {
        return format!("branch {branch} @ {commit}");
    }
    if let Some(branch) = &baseline.branch {
        return format!("branch {branch}");
    }
    if let Some(commit) = &baseline.commit {
        return format!("commit {commit}");
    }
    format!("latest {}", baseline.corpus.as_str())
}

fn local_baseline_description(corpus: &CorpusId) -> String {
    match corpus {
        CorpusId::WorkspaceCode => "local workspace index".to_owned(),
        CorpusId::Reference => "local reference index".to_owned(),
        CorpusId::Custom(id) => format!("local {id} index"),
    }
}

fn baseline_ref_from_config(
    corpus: CorpusId,
    selection_prefix: &str,
    target: &SearchBaselineTargetConfig,
) -> BaselineRef {
    BaselineRef {
        corpus,
        snapshot_id: env::var(format!("{selection_prefix}_SNAPSHOT_ID"))
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| target.snapshot_id.clone())
            .map(bsl_search::SnapshotId::new),
        branch: env::var(format!("{selection_prefix}_BRANCH"))
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| target.branch.clone()),
        commit: env::var(format!("{selection_prefix}_COMMIT"))
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| target.commit.clone()),
    }
}

fn resolve_baseline_selection(
    corpus: &CorpusId,
    project_root: Option<&Path>,
    selection_prefix: &str,
    target: &SearchBaselineTargetConfig,
    explicit_baseline: &BaselineRef,
) -> (Vec<BaselineRef>, String) {
    if baseline_has_explicit_selection(explicit_baseline) {
        return (vec![explicit_baseline.clone()], baseline_description(explicit_baseline));
    }

    if matches!(corpus, CorpusId::WorkspaceCode) && target.policy.is_configured() {
        if let Some(policy_selection) =
            resolve_workspace_policy_selection(project_root, &target.policy)
        {
            let baselines = policy_selection
                .candidate_branches()
                .into_iter()
                .map(|branch| BaselineRef {
                    corpus: corpus.clone(),
                    snapshot_id: None,
                    branch: Some(branch),
                    commit: None,
                })
                .collect::<Vec<_>>();
            return (baselines, policy_selection.selection_description());
        }
    }

    let baseline = baseline_ref_from_config(corpus.clone(), selection_prefix, target);
    (vec![baseline.clone()], baseline_description(&baseline))
}

fn baseline_has_explicit_selection(baseline: &BaselineRef) -> bool {
    baseline.snapshot_id.is_some() || baseline.branch.is_some() || baseline.commit.is_some()
}

fn resolve_workspace_policy_selection(
    project_root: Option<&Path>,
    policy: &SearchBaselinePolicyConfig,
) -> Option<project_model::ResolvedWorkspaceBranchPolicy> {
    let workspace_branch = resolve_workspace_branch(project_root);
    resolve_workspace_branch_policy(policy, workspace_branch.as_deref())
}

fn resolve_workspace_branch(project_root: Option<&Path>) -> Option<String> {
    project_root
        .and_then(current_git_branch)
        .or_else(|| resolve_env_value(&["CI_COMMIT_BRANCH", "CI_COMMIT_REF_NAME"]))
}

fn resolve_workspace_support_status(
    project_root: Option<&Path>,
    policy: &SearchBaselinePolicyConfig,
    source: &RefreshableExternalBaselineSource,
) -> Option<ResolvedWorkspaceBaselineSupport> {
    if !policy.is_configured() {
        return None;
    }

    let workspace_branch = resolve_workspace_branch(project_root);
    let (resolved_baseline, snapshot) = source.resolve_snapshot().ok().flatten()?;
    let details = source.snapshot_details(&snapshot.id.0).ok().flatten()?;
    let snapshot_created_at = parse_timestamp_utc(&details.snapshot.created_at);
    let selected_branch =
        resolved_baseline.branch.as_deref().or(details.snapshot.branch.as_deref());

    evaluate_workspace_baseline_support_now(
        policy,
        workspace_branch.as_deref(),
        selected_branch,
        snapshot_created_at,
    )
}

fn resolve_schema(schema_keys: &[&str], postgres: &SearchPostgresConfig) -> Option<String> {
    resolve_env_value(schema_keys)
        .filter(|v| !v.trim().is_empty())
        .or_else(|| postgres.schema.clone())
}

fn resolve_schema_vec(schema_keys: &[String], postgres: &SearchPostgresConfig) -> Option<String> {
    resolve_env_value_from_vec(schema_keys)
        .filter(|v| !v.trim().is_empty())
        .or_else(|| postgres.schema.clone())
}

fn resolve_env_value_from_vec(keys: &[String]) -> Option<String> {
    keys.iter().find_map(|key| env::var(key.as_str()).ok().filter(|value| !value.trim().is_empty()))
}

fn resolve_env_value(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| env::var(key).ok().filter(|value| !value.trim().is_empty()))
}

fn platform_reference_documents() -> Vec<Document> {
    crate::build_reference_documents()
}

#[cfg(test)]
mod tests {
    use super::{
        baseline_description, reject_postgres_startup_for_test,
        resolve_project_baseline_diagnostics, uncancellable, BaselineBootstrap,
        BaselineConnectPlan, BaselineRequestKind, BaselineRuntime, BaselineServiceRequest,
        BaselineSlot, BaselineStatusProbe, ConfiguredBaselineStatus, DeferredBaselineRuntime,
        ExternalBaselineService, ExternalBaselineSource, ExternalBaselineState,
        ExternalBaselineStatus, RefreshContext, RefreshOrTerminalError,
        RefreshableExternalBaselineSource, ResolvedSnapshot, SearchBaselinePolicyConfig,
        StatusProbeState, SNAPSHOT_CACHE_TTL,
    };
    use bsl_search::{BaselineRef, CorpusId, ExternalBaselineConfig, SearchError};
    use project_model::{
        ProjectConfig, SearchBaselineBackend, SearchBaselineConfig, SearchBaselineTargetConfig,
        SearchConfig, SearchPostgresConfig, SearchPostgresCredentialHelperConfig,
    };
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tempfile::tempdir;

    /// A postgres project config whose credential helper resolves synchronously (tiny
    /// python shim, same pattern as project-model's resolver tests) — enough for the
    /// bootstrap to classify it as connectable without any server.
    fn resolvable_postgres_project_config() -> ProjectConfig {
        ProjectConfig {
            search: SearchConfig {
                baseline: SearchBaselineConfig {
                    backend: SearchBaselineBackend::Postgres,
                    postgres: SearchPostgresConfig {
                        host: Some("localhost".to_owned()),
                        port: Some(5433),
                        dbname: Some("mydb".to_owned()),
                        schema: Some("bsl_search".to_owned()),
                        vault_role_base: Some("search/base".to_owned()),
                        credential_helper: SearchPostgresCredentialHelperConfig {
                            program: Some("python3".to_owned()),
                            args: vec![
                                "-c".to_owned(),
                                "import sys; sys.stdin.readline(); sys.stdout.write(sys.argv[1])"
                                    .to_owned(),
                                r#"{"protocol":"bsl-analyzer.postgres-helper.v1","ok":true,"url":"postgres://user:pass@localhost:5433/mydb","lease_id":"vault/lease/123","expires_at":"2026-06-01T00:00:00Z","renewable":false}"#
                                    .to_owned(),
                            ],
                        },
                    },
                    ..SearchBaselineConfig::default()
                },
            },
            ..ProjectConfig::default()
        }
    }

    #[test]
    fn workspace_bootstrap_defers_only_the_network_connect() {
        let bootstrap =
            BaselineRuntime::workspace_bootstrap(None, &resolvable_postgres_project_config())
                .with_token_layout_claim(Some("profile-v1:token-layout".to_owned()));
        let BaselineBootstrap::Connect(plan) = bootstrap else {
            panic!("resolvable postgres config must classify as a deferred Connect plan");
        };
        assert!(matches!(plan.corpus(), CorpusId::WorkspaceCode));
        assert_eq!(plan.context.token_layout_claim.as_deref(), Some("profile-v1:token-layout"));
    }

    #[test]
    fn pending_baseline_connects_only_after_start() {
        use std::io::ErrorKind;
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let deferred = DeferredBaselineRuntime::pending();
        assert!(deferred.view().pending);
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == ErrorKind::WouldBlock
        ));

        let baseline = BaselineRef {
            corpus: CorpusId::WorkspaceCode,
            snapshot_id: None,
            branch: Some("main".to_owned()),
            commit: None,
        };
        let plan = BaselineConnectPlan {
            corpus: CorpusId::WorkspaceCode,
            connection: format!("postgres://test:test@{address}/bsl_test?connect_timeout=1"),
            schema: Some("bsl_search".to_owned()),
            context: RefreshContext {
                token_layout_claim: None,
                postgres: SearchPostgresConfig::default(),
                baselines: vec![baseline],
                selection: "branch main".to_owned(),
                schema_keys: vec![],
            },
            selection: "branch main".to_owned(),
            project_root: None,
            policy: SearchBaselinePolicyConfig {
                publish_branches: vec!["main".to_owned()],
                ..SearchBaselinePolicyConfig::default()
            },
        };

        deferred.start(plan);

        // The adapter's r2d2 pool retries failed connects until its 5s connection_timeout;
        // allow that configured bound plus worker scheduling, not an arbitrary longer wait.
        let deadline =
            std::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(8));
        let mut accepted = 0;
        let mut protocol_error = None;
        while std::time::Instant::now() < deadline && deferred.view().pending {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    accepted += 1;
                    if let Err(error) = reject_postgres_startup_for_test(&mut stream) {
                        protocol_error = Some(error.to_string());
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                }
                Err(error) => {
                    protocol_error = Some(error.to_string());
                    break;
                }
            }
        }
        let ready = !deferred.view().pending;
        deferred.shutdown();
        assert!(accepted > 0, "activation should reach only the local PostgreSQL mock");
        assert!(protocol_error.is_none(), "test PostgreSQL peer failed: {protocol_error:?}");
        assert!(ready, "the bounded PostgreSQL refusal should publish a terminal baseline result");
    }

    #[test]
    fn workspace_bootstrap_resolves_config_errors_immediately() {
        let bootstrap = BaselineRuntime::workspace_bootstrap(
            None,
            &ProjectConfig {
                search: SearchConfig {
                    baseline: SearchBaselineConfig {
                        backend: SearchBaselineBackend::Postgres,
                        ..SearchBaselineConfig::default()
                    },
                },
                ..ProjectConfig::default()
            },
        );
        let BaselineBootstrap::Immediate(runtime) = bootstrap else {
            panic!("an unconfigured postgres backend must not defer anything");
        };
        assert_eq!(runtime.configured_baseline.backend, "postgres");
        assert!(runtime.configured_baseline.issue.is_some());
        assert!(runtime.external_baseline.is_none());
    }

    #[test]
    fn deferred_slot_pending_then_publish_wakes_waiters() {
        let deferred = DeferredBaselineRuntime::with_slot(BaselineSlot::Pending);
        let view = deferred.view();
        assert!(view.pending);
        assert!(view.external.is_none());
        assert!(view.configured.is_none());
        assert!(!deferred.wait_ready(Duration::ZERO), "a pending slot is not ready");

        let waiter = deferred.clone();
        let handle = std::thread::spawn(move || {
            waiter.wait_ready(test_utils::wait::hang_guard(Duration::from_secs(60)))
        });
        deferred.publish(BaselineRuntime {
            configured_baseline: ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "branch main".to_owned(),
                issue: None,
                support: None,
            },
            external_baseline: None,
        });
        assert!(
            crate::test_wait::join(handle, "published baseline slot wakes waiter").unwrap(),
            "publish must wake the condvar waiter"
        );
        let view = deferred.view();
        assert!(!view.pending);
        assert_eq!(view.configured.map(|status| status.backend), Some("postgres"));
    }

    #[test]
    fn deferred_slot_shutdown_ends_a_pending_wait() {
        // A budget expiry also returns false, so only the outer completion guard may expire.
        // Missing the shutdown wake must fail the test rather than imitate a closed slot.
        let deferred = DeferredBaselineRuntime::pending_for_test();
        let waiter = deferred.clone();
        let handle = std::thread::spawn(move || waiter.wait_ready(test_utils::wait::NO_BOUND));

        deferred.shutdown();

        assert!(
            !crate::test_wait::join(handle, "shutdown wakes baseline waiter").unwrap(),
            "a shut-down slot never becomes ready"
        );
    }

    #[test]
    #[ignore = "необязательный тест: the waiter must wake within a wall-clock bound; запуск: CARGO_TARGET_DIR=target-timing RUSTFLAGS='--cfg timing_contract' cargo test -p mcp-server --lib baseline::tests::deferred_slot_shutdown_wakes_a_pending_waiter -- --exact --ignored"]
    fn deferred_slot_shutdown_wakes_a_pending_waiter() {
        // The waiter's budget runs out with the same `false` a wake produces. An ordinary run
        // gives it the same sixty-second timing bound and a ten-minute hang guard otherwise.
        const BUDGET: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));
        let deferred = DeferredBaselineRuntime::pending_for_test();
        let waiter = deferred.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            tx.send(waiter.wait_ready(BUDGET)).unwrap();
        });
        // Nothing tells when the waiter has parked, and a shutdown that lands before it does
        // must end its wait just the same; the timing profile aims at the parked waiter.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(20));
        }

        let woken = std::time::Instant::now();
        deferred.shutdown();
        let result = rx.recv_timeout(BUDGET / 2);
        let waited = woken.elapsed();
        deferred.publish(BaselineRuntime {
            configured_baseline: ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "test".to_owned(),
                issue: None,
                support: None,
            },
            external_baseline: None,
        });
        crate::test_wait::join(handle, "shutdown waiter reports its result").unwrap();

        assert!(!result.expect("the shutdown left the waiter to its budget"));
        if cfg!(timing_contract) {
            assert!(waited < BUDGET / 2, "the waiter woke only after {waited:?}");
            assert!(waited < Duration::from_millis(200), "the waiter woke only after {waited:?}");
        }
    }

    #[test]
    fn deferred_slot_ready_and_absent_never_report_pending() {
        let absent = DeferredBaselineRuntime::absent();
        assert!(!absent.view().pending);
        assert!(absent.view().configured.is_none());

        let ready = DeferredBaselineRuntime::ready(BaselineRuntime {
            configured_baseline: ConfiguredBaselineStatus {
                backend: "sqlite",
                selection: "local workspace index".to_owned(),
                issue: None,
                support: None,
            },
            external_baseline: None,
        });
        assert!(!ready.view().pending);
        assert!(ready.wait_ready(Duration::from_millis(1)));
        assert_eq!(ready.view().configured.map(|status| status.backend), Some("sqlite"));
    }

    #[test]
    fn baseline_description_prefers_snapshot_id() {
        let baseline = BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "snapshot-1");
        assert_eq!(baseline_description(&baseline), "snapshot snapshot-1");
    }

    #[test]
    fn external_baseline_probe_reports_connection_errors() {
        let source = ExternalBaselineSource::new(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::WorkspaceCode,
                snapshot_id: None,
                branch: Some("main".to_owned()),
                commit: None,
            },
        )
        .unwrap();

        let status = source.probe_status();
        assert_eq!(status.backend, "postgres");
        assert!(matches!(status.state, ExternalBaselineState::Error(_)));
    }

    #[test]
    fn workspace_uses_sqlite_when_search_backend_is_default() {
        let runtime = BaselineRuntime::workspace(None, &ProjectConfig::default());

        assert_eq!(
            runtime.configured_baseline,
            ConfiguredBaselineStatus {
                backend: "sqlite",
                selection: "local workspace index".to_owned(),
                issue: None,
                support: None,
            }
        );
        assert!(runtime.external_baseline.is_none());
    }

    #[test]
    fn workspace_reports_error_when_postgres_backend_lacks_postgres_config() {
        let runtime = BaselineRuntime::workspace(
            None,
            &ProjectConfig {
                search: SearchConfig {
                    baseline: SearchBaselineConfig {
                        backend: SearchBaselineBackend::Postgres,
                        ..SearchBaselineConfig::default()
                    },
                },
                ..ProjectConfig::default()
            },
        );

        assert_eq!(runtime.configured_baseline.backend, "postgres");
        assert_eq!(runtime.configured_baseline.selection, "latest workspace-code");
        assert_eq!(
            runtime.configured_baseline.issue.as_deref(),
            Some(
                "search.baseline.postgres is not configured; set host, dbname, schema, vault_role_base, and credential_helper.program"
            )
        );
        assert!(runtime.external_baseline.is_none());
    }

    #[test]
    fn workspace_uses_postgres_when_helper_configured() {
        let runtime = BaselineRuntime::workspace(
            None,
            &ProjectConfig {
                search: SearchConfig {
                    baseline: SearchBaselineConfig {
                        backend: SearchBaselineBackend::Postgres,
                        postgres: SearchPostgresConfig {
                            host: Some("pg-central.company.com".to_owned()),
                            port: Some(5432),
                            dbname: Some("bsl_search".to_owned()),
                            schema: Some("corp_search".to_owned()),
                            vault_role_base: Some("prod/search/bsl-analyzer".to_owned()),
                            credential_helper: SearchPostgresCredentialHelperConfig {
                                program: Some("echo".to_owned()),
                                args: vec![],
                            },
                        },
                        workspace_code: SearchBaselineTargetConfig {
                            branch: Some("main".to_owned()),
                            ..SearchBaselineTargetConfig::default()
                        },
                        ..SearchBaselineConfig::default()
                    },
                },
                ..ProjectConfig::default()
            },
        );

        assert_eq!(runtime.configured_baseline.backend, "postgres");
        assert_eq!(runtime.configured_baseline.selection, "branch main");
        assert!(runtime.configured_baseline.issue.is_some());
        assert!(runtime.external_baseline.is_none());
    }

    #[test]
    fn workspace_reports_missing_credential_helper() {
        let runtime = BaselineRuntime::workspace(
            None,
            &ProjectConfig {
                search: SearchConfig {
                    baseline: SearchBaselineConfig {
                        backend: SearchBaselineBackend::Postgres,
                        postgres: SearchPostgresConfig {
                            host: Some("pg-central.company.com".to_owned()),
                            dbname: Some("bsl_search".to_owned()),
                            port: None,
                            schema: Some("bsl_search".to_owned()),
                            vault_role_base: Some("prod/search/bsl-analyzer".to_owned()),
                            credential_helper: SearchPostgresCredentialHelperConfig {
                                program: None,
                                args: vec![],
                            },
                        },
                        workspace_code: SearchBaselineTargetConfig {
                            branch: Some("main".to_owned()),
                            ..SearchBaselineTargetConfig::default()
                        },
                        ..SearchBaselineConfig::default()
                    },
                },
                ..ProjectConfig::default()
            },
        );

        assert_eq!(runtime.configured_baseline.backend, "postgres");
        assert_eq!(runtime.configured_baseline.selection, "branch main");
        assert!(runtime
            .configured_baseline
            .issue
            .as_deref()
            .is_some_and(|issue| issue.contains("credential_helper")));
        assert!(runtime.external_baseline.is_none());
    }

    #[test]
    fn workspace_policy_selection_uses_branch_chain() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/demo\n").unwrap();

        let postgres_config = build_dummy_postgres_config();

        let runtime = BaselineRuntime::workspace(
            Some(dir.path()),
            &ProjectConfig {
                search: SearchConfig {
                    baseline: SearchBaselineConfig {
                        backend: SearchBaselineBackend::Postgres,
                        postgres: postgres_config,
                        workspace_code: serde_json::from_value(serde_json::json!({
                            "policy": {
                                "publishBranches": ["vendor", "develop"],
                                "branches": [
                                    {
                                        "match": "feature/*",
                                        "selectBranch": "develop",
                                        "fallbackBranch": "vendor"
                                    },
                                    {
                                        "match": "*",
                                        "selectBranch": "develop",
                                        "fallbackBranch": "vendor"
                                    }
                                ]
                            }
                        }))
                        .unwrap(),
                        ..SearchBaselineConfig::default()
                    },
                },
                ..ProjectConfig::default()
            },
        );

        assert_eq!(runtime.configured_baseline.backend, "postgres");
        assert_eq!(
            runtime.configured_baseline.selection,
            "workspace branch feature/demo -> branch develop -> branch vendor"
        );
        assert!(runtime.external_baseline.is_none());
    }

    #[test]
    fn project_baseline_diagnostics_returns_workspace_and_reference_summaries() {
        let postgres_config = build_dummy_postgres_config();

        let diagnostics = resolve_project_baseline_diagnostics(
            None,
            &ProjectConfig {
                search: SearchConfig {
                    baseline: SearchBaselineConfig {
                        backend: SearchBaselineBackend::Postgres,
                        postgres: postgres_config,
                        workspace_code: SearchBaselineTargetConfig {
                            branch: Some("main".to_owned()),
                            ..SearchBaselineTargetConfig::default()
                        },
                        reference: SearchBaselineTargetConfig {
                            snapshot_id: Some("reference:0.1.104".to_owned()),
                            ..SearchBaselineTargetConfig::default()
                        },
                        ..SearchBaselineConfig::default()
                    },
                },
                ..ProjectConfig::default()
            },
        );

        assert_eq!(diagnostics.workspace.backend, "postgres");
        assert_eq!(diagnostics.workspace.selection, "branch main");
        assert!(diagnostics.workspace.issue.is_some());
        assert_eq!(diagnostics.reference.backend, "postgres");
        assert_eq!(diagnostics.reference.selection, "snapshot reference:0.1.104");
        assert!(diagnostics.reference.issue.is_some());
    }

    fn build_dummy_postgres_config() -> SearchPostgresConfig {
        SearchPostgresConfig {
            host: Some("pg-central.company.com".to_owned()),
            port: Some(5432),
            dbname: Some("bsl_search".to_owned()),
            schema: Some("corp_search".to_owned()),
            vault_role_base: Some("prod/search/bsl-analyzer".to_owned()),
            credential_helper: SearchPostgresCredentialHelperConfig {
                program: Some("echo".to_owned()),
                args: vec![],
            },
        }
    }

    #[test]
    fn refreshable_source_constructs_from_test_config() {
        let source = RefreshableExternalBaselineSource::for_test(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::WorkspaceCode,
                snapshot_id: None,
                branch: Some("main".to_owned()),
                commit: None,
            },
        )
        .unwrap();

        assert!(matches!(source.corpus(), CorpusId::WorkspaceCode));
    }

    #[test]
    fn refreshable_source_probe_status_reports_connection_error() {
        let source = RefreshableExternalBaselineSource::for_test(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::Reference,
                snapshot_id: Some(bsl_search::SnapshotId::new("ref:0.1.0")),
                branch: None,
                commit: None,
            },
        )
        .unwrap();

        let status = source.probe_status();
        assert_eq!(status.backend, "postgres");
        assert!(matches!(status.state, ExternalBaselineState::Error(_)));
    }

    #[test]
    fn refreshable_source_probe_status_refreshes_retryable_failures() {
        let postgres = SearchPostgresConfig {
            host: Some("127.0.0.1".to_owned()),
            port: Some(1),
            dbname: Some("bsl_search".to_owned()),
            schema: Some("bsl_search".to_owned()),
            vault_role_base: Some("prod/search/bsl-analyzer".to_owned()),
            credential_helper: SearchPostgresCredentialHelperConfig {
                program: Some("sh".to_owned()),
                args: vec![
                    "-c".to_owned(),
                    "cat >/dev/null; printf '%s\\n' '{\"protocol\":\"bsl-analyzer.postgres-helper.v1\",\"ok\":true,\"url\":\"postgres://127.0.0.1:1/bsl_search\"}'".to_owned(),
                ],
            },
        };
        let source = RefreshableExternalBaselineSource::for_test_with_refresh_context(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1/bsl_search"),
            BaselineRef {
                corpus: CorpusId::Reference,
                snapshot_id: Some(bsl_search::SnapshotId::new("ref:0.1.0")),
                branch: None,
                commit: None,
            },
            postgres,
        )
        .unwrap();

        let generation_before = source.refresh_generation.load(Ordering::SeqCst);
        let status = source.probe_status();

        assert!(matches!(status.state, ExternalBaselineState::Error(_)));
        assert_eq!(source.refresh_generation.load(Ordering::SeqCst), generation_before + 1);
    }

    #[test]
    fn refreshable_source_terminal_error_does_not_trigger_refresh() {
        let source = RefreshableExternalBaselineSource::for_test(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::WorkspaceCode,
                snapshot_id: None,
                branch: Some("main".to_owned()),
                commit: None,
            },
        )
        .unwrap();

        let generation_before = source.refresh_generation.load(Ordering::SeqCst);
        let result: Result<(), RefreshOrTerminalError> = source.run_with_refresh(|_| {
            Err(SearchError::StorageNotInitialized { schema: "bsl_search".to_owned() })
        });

        assert!(matches!(
            result,
            Err(RefreshOrTerminalError::Terminal(SearchError::StorageNotInitialized { .. }))
        ));
        assert_eq!(source.refresh_generation.load(Ordering::SeqCst), generation_before);
    }

    #[test]
    fn refreshable_source_retryable_error_refresh_failure_surfaces_missing_config() {
        let source = RefreshableExternalBaselineSource::for_test(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::WorkspaceCode,
                snapshot_id: None,
                branch: Some("main".to_owned()),
                commit: None,
            },
        )
        .unwrap();

        let generation_before = source.refresh_generation.load(Ordering::SeqCst);
        let result: Result<(), RefreshOrTerminalError> = source.run_with_refresh(|_| {
            Err(SearchError::from(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused",
            )))
        });

        match result {
            Err(RefreshOrTerminalError::Terminal(SearchError::ExternalBaseline(message))) => {
                assert!(message.starts_with("missing_config:"), "unexpected message: {message}");
            }
            Err(RefreshOrTerminalError::Terminal(other)) => {
                panic!("expected missing_config terminal error, got {other}");
            }
            Ok(()) => panic!("expected refresh attempt to fail"),
        }
        assert_eq!(source.refresh_generation.load(Ordering::SeqCst), generation_before);
    }

    #[test]
    fn refreshable_source_preserves_non_terminal_fallback_error_after_refresh() {
        let postgres = SearchPostgresConfig {
            host: Some("127.0.0.1".to_owned()),
            port: Some(1),
            dbname: Some("bsl_search".to_owned()),
            schema: Some("bsl_search".to_owned()),
            vault_role_base: Some("prod/search/bsl-analyzer".to_owned()),
            credential_helper: SearchPostgresCredentialHelperConfig {
                program: Some("sh".to_owned()),
                args: vec![
                    "-c".to_owned(),
                    "cat >/dev/null; printf '%s\\n' '{\"protocol\":\"bsl-analyzer.postgres-helper.v1\",\"ok\":true,\"url\":\"postgres://127.0.0.1:1/bsl_search\"}'".to_owned(),
                ],
            },
        };
        let source = RefreshableExternalBaselineSource::for_test_with_refresh_context(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1/bsl_search"),
            BaselineRef {
                corpus: CorpusId::Reference,
                snapshot_id: Some(bsl_search::SnapshotId::new("ref:0.1.0")),
                branch: None,
                commit: None,
            },
            postgres,
        )
        .unwrap();

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result: Result<(), RefreshOrTerminalError> =
            source.run_with_refresh(|_| match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err(SearchError::from(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "refused",
                ))),
                _ => Err(SearchError::ExternalBaseline(
                    "serving_lexical_unavailable: serving_lexical is empty".to_owned(),
                )),
            });

        match result {
            Err(RefreshOrTerminalError::Terminal(SearchError::ExternalBaseline(message))) => {
                assert!(
                    message.starts_with("serving_lexical_unavailable:"),
                    "unexpected message: {message}"
                );
            }
            Err(RefreshOrTerminalError::Terminal(other)) => {
                panic!("expected serving_lexical_unavailable error, got {other}");
            }
            Ok(()) => panic!("expected refresh attempt to surface fallback-worthy error"),
        }
    }

    #[test]
    fn refreshable_source_resolve_snapshot_returns_error_for_unreachable_host() {
        let source = RefreshableExternalBaselineSource::for_test(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::Reference,
                snapshot_id: Some(bsl_search::SnapshotId::new("nonexistent:0.1.0")),
                branch: None,
                commit: None,
            },
        )
        .unwrap();

        let result = source.resolve_snapshot();

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("credential refresh")
                || err_msg.contains("postgres")
                || err_msg.contains("missing_config"),
            "expected wrapper to surface error, got: {err_msg}"
        );
    }

    fn unreachable_snapshot_source() -> RefreshableExternalBaselineSource {
        RefreshableExternalBaselineSource::for_test(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::Reference,
                snapshot_id: Some(bsl_search::SnapshotId::new("ref:1.0.0")),
                branch: None,
                commit: None,
            },
        )
        .unwrap()
    }

    fn seeded_resolution() -> ResolvedSnapshot {
        (
            BaselineRef {
                corpus: CorpusId::Reference,
                snapshot_id: Some(bsl_search::SnapshotId::new("ref:1.0.0")),
                branch: None,
                commit: None,
            },
            bsl_search::Snapshot::new("ref:1.0.0", CorpusId::Reference),
        )
    }

    #[test]
    fn resolve_snapshot_serves_fresh_slot_without_round_trip() {
        // A fresh cache slot is served without touching the (unreachable) DB: a round-trip would
        // error, so an `Ok` proves the value came from cache.
        let source = unreachable_snapshot_source();
        source.seed_snapshot_cache_for_test(seeded_resolution(), Duration::from_secs(1));

        let resolved = source.resolve_snapshot().expect("fresh slot served from cache");
        assert_eq!(resolved, Some(seeded_resolution()));
    }

    #[test]
    fn resolve_snapshot_cache_expires_after_ttl() {
        // Past the TTL the slot is stale, so the resolve falls through to the DB and errors.
        let source = unreachable_snapshot_source();
        source.seed_snapshot_cache_for_test(
            seeded_resolution(),
            SNAPSHOT_CACHE_TTL + Duration::from_secs(1),
        );

        assert!(
            source.resolve_snapshot().is_err(),
            "stale slot must re-resolve, not serve the expired snapshot",
        );
    }

    #[test]
    fn resolve_snapshot_generation_bump_invalidates_slot() {
        // A credential refresh bumps the generation; the slot no longer matches and is bypassed.
        let source = unreachable_snapshot_source();
        source.seed_snapshot_cache_for_test(seeded_resolution(), Duration::from_secs(1));
        assert!(source.resolve_snapshot().is_ok(), "sanity: fresh slot serves");

        source.bump_refresh_generation_for_test();

        assert!(
            source.resolve_snapshot().is_err(),
            "generation bump must invalidate the slot and re-resolve",
        );
    }

    #[test]
    fn resolve_snapshot_does_not_cache_errors_and_retries() {
        // An errored resolve never populates the slot, so every call re-resolves — a baseline that
        // recovers is picked up on the next call rather than waiting out a cached failure.
        let source = unreachable_snapshot_source();
        assert!(source.resolve_snapshot().is_err());
        assert!(
            source.snapshot_cache_slot_for_test().is_none(),
            "errors must not populate the cache",
        );
        assert!(
            source.resolve_snapshot().is_err(),
            "a second call must re-resolve, not serve a cached error",
        );
        assert!(source.snapshot_cache_slot_for_test().is_none());
    }

    #[test]
    fn external_baseline_service_shutdown_times_out_without_blocking_future_requests() {
        let (sender, receiver) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let worker = std::thread::Builder::new()
            .name("baseline-service-test-timeout".to_owned())
            .spawn(move || {
                while let Ok(BaselineServiceRequest { kind, .. }) = receiver.recv() {
                    match kind {
                        BaselineRequestKind::ResolveSnapshot { reply } => {
                            let _ = entered_tx.send(());
                            // Held until the test has seen shutdown return: the resolve is
                            // still running when it does, and a shutdown that waited for it
                            // would never return here.
                            let _ = gate_rx.recv_timeout(test_utils::wait::hang_guard(
                                Duration::from_secs(60),
                            ));
                            let _ = reply.send(Ok(None));
                        }
                        BaselineRequestKind::Shutdown { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .unwrap();
        let source = RefreshableExternalBaselineSource::for_test(
            ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
            BaselineRef {
                corpus: CorpusId::WorkspaceCode,
                snapshot_id: None,
                branch: Some("main".to_owned()),
                commit: None,
            },
        )
        .unwrap();
        let service = Arc::new(ExternalBaselineService {
            corpus: CorpusId::WorkspaceCode,
            schema: "test".to_owned(),
            selection: "test".to_owned(),
            local_reference_fingerprint: None,
            sender,
            worker: StdMutex::new(Some(worker)),
            closed: AtomicBool::new(false),
            source: Arc::new(source),
            status_probe: Arc::new(StatusProbeState {
                slot: StdMutex::new(None),
                refreshing: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
        });

        let probe_service = Arc::clone(&service);
        let probe_thread = std::thread::spawn(move || {
            let _ = probe_service.resolve_snapshot(&uncancellable());
        });
        entered_rx
            .recv_timeout(test_utils::wait::hang_guard(Duration::from_secs(60)))
            .expect("the resolve reached the worker");

        let shutdown_service = Arc::clone(&service);
        let (shut_tx, shut_rx) = mpsc::channel();
        std::thread::spawn(move || {
            shutdown_service.shutdown();
            let _ = shut_tx.send(());
        });
        // The gate proves shutdown does not wait for the blocked resolve: it opens only after
        // shutdown has returned, so a shutdown that waited for the resolve never returns and
        // the bound, a hang watchdog, runs out.
        assert!(
            shut_rx.recv_timeout(test_utils::wait::hang_guard(Duration::from_secs(60))).is_ok(),
            "shutdown waited for a resolve that was still blocked"
        );
        gate_tx.send(()).unwrap();

        let error = service.resolve_snapshot(&uncancellable()).unwrap_err().into_error();
        assert!(
            error
                .to_string()
                .contains("baseline_service_closed: external baseline service for workspace-code"),
            "unexpected error: {error}"
        );

        crate::test_wait::join(probe_thread, "baseline resolve exits after release").unwrap();
    }

    fn unreachable_workspace_service() -> Arc<ExternalBaselineService> {
        ExternalBaselineService::for_test(
            RefreshableExternalBaselineSource::for_test(
                ExternalBaselineConfig::postgres("postgres://127.0.0.1:1"),
                BaselineRef {
                    corpus: CorpusId::WorkspaceCode,
                    snapshot_id: None,
                    branch: Some("main".to_owned()),
                    commit: None,
                },
            )
            .unwrap(),
        )
    }

    fn ready_status() -> ExternalBaselineStatus {
        ExternalBaselineStatus {
            backend: "postgres",
            schema: "test".to_owned(),
            selection: "branch main".to_owned(),
            resolved: Some("branch main @ abc".to_owned()),
            semantic_details: None,
            state: ExternalBaselineState::Ready {
                snapshot_id: "snapshot:test".to_owned(),
                fingerprint: Some("fp".to_owned()),
                documents: 10,
                files: 2,
            },
        }
    }

    fn error_status() -> ExternalBaselineStatus {
        ExternalBaselineStatus {
            backend: "postgres",
            schema: "test".to_owned(),
            selection: "branch main".to_owned(),
            resolved: None,
            semantic_details: None,
            state: ExternalBaselineState::Error("connection refused".to_owned()),
        }
    }

    #[test]
    fn indexing_remote_cache() {
        use super::{BaselineIndexingPublication::*, BaselineSemanticDetails};
        let service = unreachable_workspace_service();
        let mut status = ready_status();
        status.semantic_details = Some(BaselineSemanticDetails {
            snapshot_id: "snapshot:test".to_owned(),
            fingerprint: Some("fp".to_owned()),
            publication: Some(bsl_search::BaselineSemanticPublication {
                model_id: "model".to_owned(),
                dimension: 4,
                complete: true,
            }),
        });
        let seed = |status, age| service.seed_status_cache_for_test(status, age);
        let identity = ("snapshot:test".into(), Some("fp".into()));
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), Stale);
        seed(status.clone(), Duration::ZERO);
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), Ready);
        assert_eq!(service.indexing_publication("other", 4, Some(&identity)), UnverifiedIdentity);
        assert_eq!(service.indexing_publication("model", 8, Some(&identity)), UnverifiedIdentity);
        {
            let _locked = service.status_probe.slot.lock().unwrap();
            assert_eq!(
                service.indexing_publication("model", 4, Some(&identity)),
                SnapshotUnavailable
            );
        }
        seed(status.clone(), super::STATUS_PROBE_TTL);
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), Stale);
        seed(status.clone(), Duration::ZERO);
        service.source.bump_refresh_generation_for_test();
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), Stale);
        seed(status.clone(), Duration::ZERO);
        service.status_probe.refreshing.store(true, Ordering::Release);
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), Stale);
        service.status_probe.refreshing.store(false, Ordering::Release);
        let mut changed = status.clone();
        changed.semantic_details.as_mut().unwrap().fingerprint = Some("different".to_owned());
        seed(changed, Duration::ZERO);
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), Stale);
        let mut changed = status.clone();
        changed.semantic_details.as_mut().unwrap().publication.as_mut().unwrap().complete = false;
        seed(changed, Duration::ZERO);
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), UnverifiedCoverage);
        let mut changed = status.clone();
        changed.semantic_details.as_mut().unwrap().publication = None;
        seed(changed, Duration::ZERO);
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), UnverifiedIdentity);
        status.semantic_details = None;
        seed(status.clone(), Duration::ZERO);
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), UnverifiedCoverage);
        status.state = ExternalBaselineState::Missing;
        seed(status, Duration::ZERO);
        assert_eq!(service.indexing_publication("model", 4, Some(&identity)), Unavailable);
        // Every path above only peeks: neither a network probe nor an actor request runs.
        assert!(!service.status_probe_refreshing_for_test());
        assert!(service.source.snapshot_cache_slot_for_test().is_none());
    }

    #[test]
    fn status_probe_first_call_returns_pending_without_blocking() {
        let service = unreachable_workspace_service();

        let started = std::time::Instant::now();
        let probe = service.probe_status_cached();
        if cfg!(timing_contract) {
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "probe_status_cached blocked for {:?}",
                started.elapsed()
            );
        }

        // The cached call leaves the probing to the background thread it kicked, so it has
        // nothing to answer with but Pending. A call that waited for the probe would answer
        // with the probe's outcome instead, which is why this is the whole assertion: no clock
        // is needed to tell the two apart.
        assert!(matches!(probe, BaselineStatusProbe::Pending), "expected Pending: {probe:?}");
        // The kicked probe is either still in flight or already finished and published
        // its (error) result — asserting the disjunction keeps the test independent of
        // how fast the connection attempt fails.
        assert!(
            service.status_probe_refreshing_for_test()
                || service.status_probe_slot_for_test().is_some(),
            "background probe must be kicked"
        );

        // A second call while the first background probe is still in flight must not
        // stack another probe; it still answers Pending immediately.
        let probe = service.probe_status_cached();
        assert!(matches!(probe, BaselineStatusProbe::Pending));
    }

    #[test]
    fn status_probe_serves_fresh_slot_without_reprobing() {
        let service = unreachable_workspace_service();
        service.seed_status_cache_for_test(ready_status(), Duration::from_secs(1));

        let probe = service.probe_status_cached();

        let BaselineStatusProbe::Cached(cached) = probe else {
            panic!("expected cached status");
        };
        assert!(matches!(cached.status.state, ExternalBaselineState::Ready { .. }));
        assert!(
            !service.status_probe_refreshing_for_test(),
            "a fresh slot must not trigger a background probe"
        );
        let slot = service.status_probe_slot_for_test().expect("seeded slot");
        assert!(
            matches!(slot.status.state, ExternalBaselineState::Ready { .. }),
            "the seeded slot must not be overwritten by a stray probe"
        );
    }

    #[test]
    fn status_probe_reprobes_stale_slot_but_serves_the_old_value() {
        let service = unreachable_workspace_service();
        service.seed_status_cache_for_test(ready_status(), Duration::from_secs(120));

        let probe = service.probe_status_cached();

        let BaselineStatusProbe::Cached(cached) = probe else {
            panic!("expected the stale value to be served while the re-probe runs");
        };
        assert!(matches!(cached.status.state, ExternalBaselineState::Ready { .. }));
        assert!(cached.age() >= Duration::from_secs(120));
        // Kicked = still in flight, or already finished and overwrote the stale slot.
        assert!(
            service.status_probe_refreshing_for_test()
                || service
                    .status_probe_slot_for_test()
                    .is_some_and(|slot| slot.age() < Duration::from_secs(60)),
            "a stale slot must kick a background re-probe"
        );
    }

    #[test]
    fn status_probe_error_result_retries_before_the_ready_ttl() {
        // 10s-old error: inside the 60s Ready TTL but past the 5s error retry window,
        // so a cached transient failure does not linger for a minute after recovery.
        let service = unreachable_workspace_service();
        service.seed_status_cache_for_test(error_status(), Duration::from_secs(10));

        let probe = service.probe_status_cached();

        assert!(matches!(probe, BaselineStatusProbe::Cached(_)));
        assert!(
            service.status_probe_refreshing_for_test()
                || service
                    .status_probe_slot_for_test()
                    .is_some_and(|slot| slot.age() < Duration::from_secs(10)),
            "an aged error slot must re-probe sooner than the Ready TTL"
        );

        let service = unreachable_workspace_service();
        service.seed_status_cache_for_test(error_status(), Duration::from_secs(1));
        let _ = service.probe_status_cached();
        assert!(
            !service.status_probe_refreshing_for_test(),
            "a just-probed error must not be hammered on every poll"
        );
    }

    #[test]
    fn status_probe_does_not_spawn_after_shutdown() {
        let service = unreachable_workspace_service();
        service.shutdown();

        let probe = service.probe_status_cached();

        assert!(matches!(probe, BaselineStatusProbe::Pending));
        assert!(
            !service.status_probe_refreshing_for_test(),
            "a closed service must not spawn probe threads"
        );
    }

    /// A caller that withdraws while its request is queued stops waiting at once, its
    /// request is skipped by the worker, and the request in flight ahead of it is answered
    /// untouched. Both halves matter: the wait ending proves the cancellation reaches the
    /// caller, the skip proves it reaches the queue, and the untouched neighbour proves
    /// neither one disturbs anybody else.
    #[test]
    fn a_withdrawn_caller_stops_waiting_and_its_queued_request_is_skipped() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Instant;

        let (release_tx, release_rx) = mpsc::channel::<()>();
        let executed = Arc::new(StdMutex::new(Vec::<String>::new()));
        let started = Arc::new(AtomicUsize::new(0));
        let service = {
            let executed = Arc::clone(&executed);
            let started = Arc::clone(&started);
            ExternalBaselineService::with_worker_for_test(move |kind| {
                started.fetch_add(1, Ordering::SeqCst);
                match kind {
                    BaselineRequestKind::LexicalSearch { query, reply, .. } => {
                        // Every lexical query is latched until the test releases it.
                        let _ = release_rx
                            .recv_timeout(test_utils::wait::hang_guard(Duration::from_secs(60)));
                        executed.lock().unwrap().push(query.clone());
                        let _ = reply.send(Ok(vec![]));
                    }
                    BaselineRequestKind::Shutdown { reply } => {
                        let _ = reply.send(());
                        return std::ops::ControlFlow::Break(());
                    }
                    _ => {}
                }
                std::ops::ControlFlow::Continue(())
            })
        };

        // A: in flight, latched inside the worker.
        let first = {
            let service = Arc::clone(&service);
            std::thread::spawn(move || {
                service.lexical_search(&uncancellable(), "snap", "first", None, 5)
            })
        };
        crate::test_wait::wait_until("the first baseline request starts", || {
            started.load(Ordering::SeqCst) >= 1
        });

        // B: queued behind A, then cancelled while it waits.
        let cancel = tokio_util::sync::CancellationToken::new();
        let (second_tx, second_rx) = mpsc::channel();
        {
            let service = Arc::clone(&service);
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let waited = Instant::now();
                let out = service.lexical_search(&cancel, "snap", "second", None, 5);
                let _ = second_tx.send((out, waited.elapsed()));
            });
        }
        // Nothing tells when B has queued, and a cancellation that reaches B before it does
        // must withdraw it just the same; the timing profile aims at the queued B.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(50));
        }
        cancel.cancel();
        // A stays latched until B is back, so a B that waits for A's latched query instead of
        // its own cancellation never comes back: the hang guard fails the test.
        let (out, waited) = second_rx
            .recv_timeout(test_utils::wait::hang_guard(Duration::from_secs(60)))
            .expect("B never came back from its wait");
        assert!(matches!(out, Err(super::BaselineCall::Withdrawn)), "B must withdraw");
        if cfg!(timing_contract) {
            assert!(
                waited < Duration::from_millis(500),
                "B waited {waited:?} past its cancellation, i.e. for A's query to finish"
            );
        }
        assert_eq!(started.load(Ordering::SeqCst), 1, "A is still the only request started");
        assert!(
            !first.is_finished(),
            "A finished before B came back, so B could have waited for it"
        );

        // Release A: it answers, and the worker moves on to B's request — and skips it.
        release_tx.send(()).unwrap();
        let first = crate::test_wait::join(first, "released baseline query returns").unwrap();
        assert!(
            matches!(first, Ok(ref hits) if hits.is_empty()),
            "A's answer is untouched: {first:?}"
        );
        service.shutdown();
        assert_eq!(*executed.lock().unwrap(), vec!["first".to_owned()], "B's query never ran");
    }
}
