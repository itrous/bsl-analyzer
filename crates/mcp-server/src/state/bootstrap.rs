use super::embed::EmbedFlight;
use super::types::{
    OverlayInit, OverlayWarmupState, PendingEmbed, SemanticRuntimeStatus, SharedSearchEngine,
    WorkspaceSearchInit, WorkspaceSearchMode,
};
use super::{ReferenceSearchLifecycle, ReferenceSearchState, SharedState};
use crate::baseline::{
    BaselineBootstrap, BaselineRuntime, DeferredBaselineRuntime, ExternalBaselineService,
};
use crate::change_hub::WorkspaceChangeHub;
use crate::diagnostics_state::DiagnosticsState;
use crate::graph::GraphState;
use bsl_platform::PlatformDataInner;
use bsl_search::{
    BaselineHashMode, CorpusId, EmbeddingFailure, IndexProgress, SearchEngine, SearchError,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::{
    env,
    path::{Path, PathBuf},
};

/// How long a search-init thread waits for the deferred baseline connect before
/// proceeding degraded. Generous by design: the wait sits on a background thread and
/// only unusually slow networks ever reach it; the connect itself typically lands in
/// seconds and wakes the waiter through the slot's condvar immediately.
const BASELINE_CONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
pub(super) const DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET: std::time::Duration =
    std::time::Duration::from_secs(600);
const EMBEDDING_PUBLISH_RETRY_BUDGET_ENV: &str = "EMBEDDING_PUBLISH_RETRY_BUDGET_SECS";
#[cfg(test)]
static EMBEDDING_PUBLISH_RETRY_BUDGET_WARNINGS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

type ReferenceInitError = (String, String, Option<EmbeddingFailure>);

struct OpenedSearchEngine {
    engine: SearchEngine,
    semantic_failure: Option<EmbeddingFailure>,
}

fn is_token_layout_refusal(error: &SearchError) -> bool {
    matches!(error, SearchError::Index(message) if message == "token layout mismatch")
}

fn is_invalid_embedding_config(error: &SearchError) -> bool {
    error.embedding_failure().is_some_and(|failure| {
        failure.code == bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig
    })
}

fn token_profile_failure(prefixes: &super::types::EmbeddingPrefixes) -> Option<EmbeddingFailure> {
    prefixes
        .token_profile
        .as_ref()
        .filter(|profile| profile.token_policy.is_none())
        .map(|_| EmbeddingFailure::new(bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig))
}

fn is_invalid_token_profile_only(
    error: &SearchError,
    prefixes: &super::types::EmbeddingPrefixes,
) -> bool {
    is_invalid_embedding_config(error)
        && token_profile_failure(prefixes).is_some()
        && bsl_search::EmbedderConfig::request_bytes_from_env().is_ok()
}

fn initial_semantic_runtime_status(
    prefixes: &super::types::EmbeddingPrefixes,
) -> SemanticRuntimeStatus {
    token_profile_failure(prefixes)
        .map_or(SemanticRuntimeStatus::Disabled, SemanticRuntimeStatus::EmbeddingFailed)
}

fn semantic_runtime_status_after_publish(
    engine: &SearchEngine,
    mode: &WorkspaceSearchMode,
    indexing: bool,
    prefixes: &super::types::EmbeddingPrefixes,
    open_failure: Option<EmbeddingFailure>,
) -> SemanticRuntimeStatus {
    open_failure
        .or_else(|| token_profile_failure(prefixes))
        .map(SemanticRuntimeStatus::EmbeddingFailed)
        .unwrap_or_else(|| {
            if indexing {
                SemanticRuntimeStatus::Indexing
            } else {
                SharedState::semantic_runtime_status_for_mode(engine, mode)
            }
        })
}

fn resolve_token_profile(
    project: Option<&project_model::ProjectConfig>,
    workspace: Option<&Path>,
) -> Result<Option<super::types::EmbeddingTokenProfile>, SearchError> {
    resolve_embedding_token_profile_values(
        project,
        workspace,
        [
            env::var_os(crate::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV),
            env::var_os(crate::broker::EMBEDDING_TOKENIZER_FILE_ENV),
            env::var_os(crate::broker::EMBEDDING_TOKENIZER_SHA256_ENV),
        ],
        [
            env::var_os("EMBEDDING_MAX_INPUT_TOKENS"),
            env::var_os("EMBEDDING_TOKENIZER_FILE"),
            env::var_os("EMBEDDING_TOKENIZER_SHA256"),
        ],
    )
}

fn invalid_token_profile() -> super::types::EmbeddingTokenProfile {
    super::types::EmbeddingTokenProfile {
        max_input_tokens: 0,
        tokenizer_file: PathBuf::new(),
        tokenizer_sha256: String::new(),
        token_policy: None,
    }
}

fn token_profile_for_bootstrap(
    project: Option<&project_model::ProjectConfig>,
    workspace: Option<&Path>,
) -> Option<super::types::EmbeddingTokenProfile> {
    resolve_token_profile(project, workspace).unwrap_or_else(|_| Some(invalid_token_profile()))
}

/// Resolve the optional token profile once, honoring broker-frozen values before project and
/// environment configuration. The tokenizer is loaded here so later workers clone the frozen
/// policy instead of reopening the artifact.
pub fn resolve_embedding_token_profile_values(
    project: Option<&project_model::ProjectConfig>,
    workspace: Option<&Path>,
    frozen: [Option<std::ffi::OsString>; 3],
    environment: [Option<std::ffi::OsString>; 3],
) -> Result<Option<super::types::EmbeddingTokenProfile>, SearchError> {
    let invalid = || {
        SearchError::from(EmbeddingFailure::new(
            bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig,
        ))
    };
    let [frozen_limit, frozen_file, frozen_hash] = frozen;
    let [environment_limit, environment_file, environment_hash] = environment;
    let frozen_any = frozen_limit.is_some() || frozen_file.is_some() || frozen_hash.is_some();
    let project_embedding = project.map(|project| &project.search.baseline.embedding);
    let configured_any = project_embedding.is_some_and(|embedding| {
        embedding.max_input_tokens.is_some()
            || embedding.tokenizer_file.is_some()
            || embedding.tokenizer_sha256.is_some()
    }) || environment_limit.is_some()
        || environment_file.is_some()
        || environment_hash.is_some();
    if !frozen_any && !configured_any {
        return Ok(None);
    }
    let limit = if frozen_any {
        frozen_limit
            .as_deref()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<usize>().ok())
    } else if let Some(value) =
        project_embedding.and_then(|embedding| embedding.max_input_tokens.as_ref())
    {
        value.as_integer().and_then(|value| usize::try_from(value).ok())
    } else {
        environment_limit
            .as_deref()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<usize>().ok())
    };
    let root = workspace.unwrap_or(Path::new("."));
    let path = if frozen_any {
        frozen_file.map(PathBuf::from)
    } else if let (Some(project), Some(declared)) =
        (project, project_embedding.and_then(|embedding| embedding.tokenizer_file.as_deref()))
    {
        Some(project.search_tokenizer_file(root).unwrap_or_else(|| root.join(declared)))
    } else {
        environment_file.map(PathBuf::from).map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
    };
    let expected_hash = if frozen_any {
        frozen_hash.as_deref().and_then(|value| value.to_str()).map(str::to_owned)
    } else if let Some(hash) =
        project_embedding.and_then(|embedding| embedding.tokenizer_sha256.clone())
    {
        Some(hash)
    } else {
        environment_hash.as_deref().and_then(|value| value.to_str()).map(str::to_owned)
    };
    let (Some(max_input_tokens), Some(path), Some(tokenizer_sha256)) =
        (limit.filter(|value| *value > 0), path, expected_hash.filter(|hash| !hash.is_empty()))
    else {
        return Err(invalid());
    };
    let tokenizer_file = std::fs::canonicalize(path).map_err(|_| invalid())?;
    if !tokenizer_file.is_file() {
        return Err(invalid());
    }
    let token_policy =
        bsl_search::TokenPolicy::load(&tokenizer_file, &tokenizer_sha256, max_input_tokens)
            .map_err(|_| invalid())?;
    Ok(Some(super::types::EmbeddingTokenProfile {
        max_input_tokens,
        tokenizer_file,
        tokenizer_sha256,
        token_policy: Some(token_policy),
    }))
}

fn search_failure(error: SearchError) -> ReferenceInitError {
    let reason = error.reason_code().unwrap_or("search_error").to_owned();
    (error.to_string(), reason, error.embedding_failure())
}

/// Writes the verdict a reference-search worker could not write for itself.
///
/// `Loading` reads to the broker as live background work, so a worker that ends without
/// reaching `Ready` or `Failed` — a panic on the way, or the stop path during shutdown —
/// would hold the backend process for good. Every exit leaves a terminal state behind.
struct LoadingVerdict(ReferenceSearchState);

impl Drop for LoadingVerdict {
    fn drop(&mut self) {
        let mut lifecycle =
            self.0.lifecycle.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(*lifecycle, ReferenceSearchLifecycle::Loading) {
            *lifecycle = ReferenceSearchLifecycle::Failed {
                message: "reference search initialization ended without a verdict".to_owned(),
                reason_code: "worker_gone".to_owned(),
            };
        }
    }
}

impl ReferenceSearchState {
    fn new(project_root: Option<&Path>) -> Self {
        Self::new_with_reference_cache(
            project_root,
            SharedState::reference_search_db_path().as_deref(),
        )
    }

    fn new_with_reference_cache(
        project_root: Option<&Path>,
        reference_cache: Option<&Path>,
    ) -> Self {
        let (project_config, mut lifecycle) = match project_root {
            Some(root) => match project_model::ProjectConfig::load(root) {
                Ok(config) => (config, ReferenceSearchLifecycle::Uninitialized),
                Err(error) => {
                    tracing::error!(%error, "reference search rejects unreadable project config");
                    (
                        None,
                        ReferenceSearchLifecycle::Failed {
                            message: error.to_string(),
                            reason_code: "project_config_error".to_owned(),
                        },
                    )
                }
            },
            None => (None, ReferenceSearchLifecycle::Uninitialized),
        };
        if matches!(lifecycle, ReferenceSearchLifecycle::Uninitialized) {
            if let Some(root) = project_root {
                if let Some(message) = reference_storage_overlap(root, reference_cache) {
                    tracing::warn!(error = %message, "reference search cache is unavailable; MCP startup continues");
                    lifecycle = ReferenceSearchLifecycle::Failed {
                        message,
                        reason_code: "baseline_unavailable".to_owned(),
                    };
                }
            }
        }
        let token_profile = token_profile_for_bootstrap(project_config.as_ref(), project_root);
        let embedding_prefixes = project_config.as_ref().map_or_else(
            || super::types::EmbeddingPrefixes {
                query: env::var("EMBEDDING_QUERY_PREFIX").unwrap_or_default(),
                document: env::var("EMBEDDING_DOCUMENT_PREFIX").unwrap_or_default(),
                token_profile: token_profile.clone(),
            },
            |config| {
                let embedding = &config.search.baseline.embedding;
                super::types::EmbeddingPrefixes {
                    query: embedding
                        .resolve_query_prefix(env::var("EMBEDDING_QUERY_PREFIX").ok().as_deref()),
                    document: embedding.resolve_document_prefix(
                        env::var("EMBEDDING_DOCUMENT_PREFIX").ok().as_deref(),
                    ),
                    token_profile: token_profile.clone(),
                }
            },
        );
        let baseline = match &lifecycle {
            ReferenceSearchLifecycle::Failed { .. } => DeferredBaselineRuntime::absent(),
            _ => match BaselineRuntime::reference_bootstrap(project_config.as_ref())
                .with_token_layout_claim(
                    SharedState::embedding_config_with_prefixes(Some(&embedding_prefixes))
                        .ok()
                        .flatten()
                        .and_then(|config| {
                            bsl_search::Embedder::new(config.embedder)
                                .token_layout_claim()
                                .map(str::to_owned)
                        }),
                ) {
                BaselineBootstrap::Immediate(runtime) => DeferredBaselineRuntime::ready(runtime),
                BaselineBootstrap::Connect(plan) => DeferredBaselineRuntime::spawn(*plan),
            },
        };
        Self {
            engine: super::shared_engine(None),
            progress: IndexProgress::new(),
            semantic_runtime: Arc::new(Mutex::new(initial_semantic_runtime_status(
                &embedding_prefixes,
            ))),
            baseline,
            embedding_prefixes,
            lifecycle: Arc::new(Mutex::new(lifecycle)),
            stopped: Arc::new(AtomicBool::new(false)),
            stop: super::OwnerStop::default(),
            worker: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn ensure_loading(&self) {
        self.ensure_loading_with_wait(BASELINE_CONNECT_WAIT);
    }

    fn ensure_loading_with_wait(&self, baseline_wait: std::time::Duration) {
        let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(*lifecycle, ReferenceSearchLifecycle::Uninitialized) {
            return;
        }
        *lifecycle = ReferenceSearchLifecycle::Loading;
        SharedState::set_semantic_runtime_status(
            &self.semantic_runtime,
            initial_semantic_runtime_status(&self.embedding_prefixes),
        );
        drop(lifecycle);

        let state = self.clone();
        let spawn = std::thread::Builder::new().name("bsl-search-reference-init".to_owned()).spawn(
            move || {
                let _verdict = LoadingVerdict(state.clone());
                while !state.baseline.wait_ready(baseline_wait) {
                    if state.stopped.load(Ordering::Acquire) {
                        return;
                    }
                    tracing::debug!(
                        timeout_ms = baseline_wait.as_millis(),
                        "reference search still waits for configured baseline"
                    );
                }
                let baseline = state.baseline.view();
                let initialization = if baseline
                    .configured
                    .as_ref()
                    .is_some_and(|configured| configured.backend == "postgres")
                    && baseline.external.is_none()
                {
                    Err((
                        baseline
                            .configured
                            .as_ref()
                            .and_then(|configured| configured.issue.clone())
                            .unwrap_or_else(|| {
                                "configured reference baseline is unavailable".to_owned()
                            }),
                        "baseline_unavailable".to_owned(),
                        None,
                    ))
                } else {
                    SharedState::init_reference_search_engine(
                        &state.progress,
                        baseline.external,
                        &state.embedding_prefixes,
                    )
                };
                state.finish_initialization(initialization);
            },
        );
        match spawn {
            Ok(handle) => {
                *self.worker.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
            }
            Err(error) => {
                *self.lifecycle.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    ReferenceSearchLifecycle::Failed {
                        message: error.to_string(),
                        reason_code: "worker_spawn_failed".to_owned(),
                    };
            }
        }
    }

    pub(crate) fn indexing_snapshot(&self) -> crate::indexing::Target {
        use crate::indexing::{Kind, Reason, State, Target};
        if self.stopped.load(Ordering::Acquire) {
            return Target::new(Kind::Reference, State::Cancelled, Some(Reason::Cancelled));
        }
        let Ok(lifecycle) = self.lifecycle.try_lock() else {
            return Target::unknown(Kind::Reference);
        };
        match &*lifecycle {
            ReferenceSearchLifecycle::Uninitialized => {
                Target::new(Kind::Reference, State::Waiting, Some(Reason::Initializing))
            }
            ReferenceSearchLifecycle::Loading => Target::new(Kind::Reference, State::Running, None),
            ReferenceSearchLifecycle::Ready => Target::new(Kind::Reference, State::Ready, None),
            ReferenceSearchLifecycle::Failed { .. } => {
                Target::new(Kind::Reference, State::Failed, Some(Reason::NativeFailure))
            }
        }
    }

    fn finish_initialization(
        &self,
        initialization: Result<(SearchEngine, Option<EmbeddingFailure>), ReferenceInitError>,
    ) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        let lock_lifecycle =
            || self.lifecycle.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match initialization {
            Ok((engine, embedding_failure)) => {
                let status = token_profile_failure(&self.embedding_prefixes)
                    .or(embedding_failure)
                    .map_or_else(
                        || {
                            SharedState::semantic_runtime_status_for_mode(
                                &engine,
                                &WorkspaceSearchMode::SqliteLocal,
                            )
                        },
                        SemanticRuntimeStatus::EmbeddingFailed,
                    );
                // `Ready` is the claim that the engine is in the slot, so it is made only where
                // the engine reaches it. A refused admission — the daemon stopping, or a
                // poisoned slot — published nothing, and saying ready over an empty slot sends
                // every reader to a profile that cannot answer.
                match self.engine.acquire_for_owner(&self.stop) {
                    Ok(mut slot) => {
                        let mut lifecycle = lock_lifecycle();
                        *slot = Some(engine);
                        SharedState::set_semantic_runtime_status(&self.semantic_runtime, status);
                        *lifecycle = ReferenceSearchLifecycle::Ready;
                    }
                    Err(error) => {
                        let message = format!("reference search engine was not published: {error}");
                        SharedState::set_semantic_runtime_status(
                            &self.semantic_runtime,
                            SemanticRuntimeStatus::Failed(message.clone()),
                        );
                        // The worker ended without putting an engine in the slot, which is what
                        // this code has always named. A new one would reach
                        // `find_docs`/`search_docs` callers as `data.reasonCode`, and the
                        // reference profile is frozen.
                        *lock_lifecycle() = ReferenceSearchLifecycle::Failed {
                            message,
                            reason_code: "worker_gone".to_owned(),
                        };
                    }
                }
            }
            Err((message, reason_code, embedding_failure)) => {
                SharedState::set_semantic_runtime_status(
                    &self.semantic_runtime,
                    embedding_failure.map_or_else(
                        || SemanticRuntimeStatus::Failed(message.clone()),
                        SemanticRuntimeStatus::EmbeddingFailed,
                    ),
                );
                *lock_lifecycle() = ReferenceSearchLifecycle::Failed { message, reason_code };
            }
        }
    }

    pub(crate) fn lifecycle(&self) -> ReferenceSearchLifecycle {
        self.lifecycle.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Never blocks its caller: the broker's serve loop asks this on every tick.
    ///
    /// A contended lock reads as loading for that one tick, which costs an idle countdown.
    /// A poisoned one is forever, so it is stepped over the way [`Self::lifecycle`] and
    /// [`Self::shutdown`] already step over it — reading poison as live background work
    /// would hold the backend process for the rest of its life.
    pub(super) fn loading(&self) -> bool {
        match self.lifecycle.try_lock() {
            Ok(lifecycle) => matches!(*lifecycle, ReferenceSearchLifecycle::Loading),
            Err(TryLockError::Poisoned(poisoned)) => {
                matches!(*poisoned.into_inner(), ReferenceSearchLifecycle::Loading)
            }
            Err(TryLockError::WouldBlock) => true,
        }
    }

    pub(super) fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        // The same call for both: the flag the worker reads between steps, and the release of
        // any wait it is already in.
        self.stop.stop();
        self.baseline.shutdown();
        if let Some(worker) =
            self.worker.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
        {
            let _ = worker.join();
        }
        // The daemon is going and every owner has been told to leave, so the only hold left
        // to wait on is a request's. This is the one acquisition no stop can call off.
        if let Ok(mut engine) = self.engine.take_for_shutdown() {
            *engine = None;
        }
    }
}

fn reference_storage_overlap(project_root: &Path, path: Option<&Path>) -> Option<String> {
    let project = match crate::project::at(project_root) {
        Ok(project) => project,
        Err(error) => {
            return Some(format!("cannot validate project for reference storage: {error}"));
        }
    };
    let Some(path) = path else {
        return Some("reference cache path is unavailable".to_owned());
    };
    match crate::cache::WorkspaceCacheLayout::overlapping_source_root(&project, path) {
        Ok(Some(root)) => Some(format!(
            "reference cache {} overlaps source root {}",
            path.display(),
            root.display()
        )),
        Ok(None) => None,
        Err(error) => Some(format!("cannot validate reference cache path: {error}")),
    }
}

/// Why a workspace could not be brought up.
#[derive(Debug)]
pub enum WorkspaceInitError {
    /// The project config or its extension topology is invalid: a daemon must not come
    /// up analyzing a differently-shaped project than the one configured.
    Project(project_model::ProjectError),
    Cache(std::io::Error),
    /// The derived-cache root contains a scan root, so following that root and treating
    /// the cache as the server's own output are mutually exclusive.
    CacheCoversScanRoot {
        cache: std::path::PathBuf,
        root: std::path::PathBuf,
    },
    /// A scan root lies inside a service directory (`.git`, `target`, `node_modules`): the
    /// exclusion that keeps the directory out of the graph would swallow the root's sources,
    /// exactly as a cache above a root would.
    ScanRootInsideServiceDirectory {
        service: std::path::PathBuf,
        root: std::path::PathBuf,
    },
}

impl std::fmt::Display for WorkspaceInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkspaceInitError::Project(error) => error.fmt(f),
            WorkspaceInitError::Cache(error) => error.fmt(f),
            WorkspaceInitError::CacheCoversScanRoot { cache, root } => write!(
                f,
                "cache directory {} contains the scanned source root {}; \
                 choose a cache directory outside every source root",
                cache.display(),
                root.display()
            ),
            WorkspaceInitError::ScanRootInsideServiceDirectory { service, root } => write!(
                f,
                "the scanned source root {} lies inside the service directory {}, which is \
                 never read as sources; move the root out of it",
                root.display(),
                service.display()
            ),
        }
    }
}

impl std::error::Error for WorkspaceInitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorkspaceInitError::Project(error) => Some(error),
            WorkspaceInitError::Cache(error) => Some(error),
            WorkspaceInitError::CacheCoversScanRoot { .. }
            | WorkspaceInitError::ScanRootInsideServiceDirectory { .. } => None,
        }
    }
}

impl From<project_model::ProjectError> for WorkspaceInitError {
    fn from(error: project_model::ProjectError) -> Self {
        WorkspaceInitError::Project(error)
    }
}

impl SharedState {
    /// The workspace search mode implied by the baseline bootstrap: configured INTENT,
    /// never connect success. EVERY postgres-configured outcome — a deferred connect
    /// AND the immediate failures (unconfigured section, credential rejection) — stays
    /// in Postgres mode. Mapping a failure to `SqliteLocal` would route `search_code`
    /// into a silent full local reindex of the configuration, hiding exactly the
    /// failure the issue text reports; in Postgres mode the gates surface that issue.
    fn workspace_mode_for(bootstrap: &BaselineBootstrap) -> WorkspaceSearchMode {
        match bootstrap {
            BaselineBootstrap::Connect(plan)
                if matches!(plan.corpus(), CorpusId::WorkspaceCode) =>
            {
                WorkspaceSearchMode::PostgresRemoteOverlay
            }
            BaselineBootstrap::Immediate(runtime)
                if runtime.configured_baseline.backend == "postgres" =>
            {
                WorkspaceSearchMode::PostgresRemoteOverlay
            }
            _ => WorkspaceSearchMode::SqliteLocal,
        }
    }

    /// Errors when the project config or its extension topology is invalid: a
    /// daemon must not come up analyzing a differently-shaped project than the
    /// one configured.
    pub fn workspace(source_dir: PathBuf) -> Result<Self, WorkspaceInitError> {
        let root = source_dir.canonicalize().map_err(WorkspaceInitError::Cache)?;
        let project = crate::project::at(&root)?;
        let cache = crate::cache::WorkspaceCacheLayout::for_project_in_current_dir(
            &project,
            None,
            crate::cache::expected_scope_from_env().map_err(WorkspaceInitError::Cache)?.as_deref(),
        )
        .map_err(WorkspaceInitError::Cache)?;
        Self::workspace_with_cache(root, cache)
    }

    /// Construct workspace state with all rebuildable derived files rooted in `cache`.
    pub fn workspace_with_cache(
        source_dir: PathBuf,
        cache: crate::cache::WorkspaceCacheLayout,
    ) -> Result<Self, WorkspaceInitError> {
        Self::workspace_with_cache_and_prefixes(source_dir, cache, None)
    }

    /// Construct workspace state with the embedding input profile frozen by its launcher.
    pub fn workspace_with_cache_and_prefixes(
        source_dir: PathBuf,
        cache: crate::cache::WorkspaceCacheLayout,
        frozen_prefixes: Option<super::types::EmbeddingPrefixes>,
    ) -> Result<Self, WorkspaceInitError> {
        let project = crate::project::at(&source_dir)?;
        cache.verify_project(&project).map_err(WorkspaceInitError::Cache)?;
        if cache.origin() == crate::cache::CacheOrigin::Explicit {
            cache.ensure().map_err(WorkspaceInitError::Cache)?;
        }
        // Explicit preparation may take long enough for the project declaration to change.
        // Re-read it at the last point before any workspace owner or derived store is started.
        let project = crate::project::at(&source_dir)?;
        cache.verify_project(&project).map_err(WorkspaceInitError::Cache)?;
        let embedding_prefixes = match frozen_prefixes {
            Some(prefixes) => prefixes,
            None => {
                let embedding = &project.config.search.baseline.embedding;
                super::types::EmbeddingPrefixes {
                    query: embedding
                        .resolve_query_prefix(env::var("EMBEDDING_QUERY_PREFIX").ok().as_deref()),
                    document: embedding.resolve_document_prefix(
                        env::var("EMBEDDING_DOCUMENT_PREFIX").ok().as_deref(),
                    ),
                    token_profile: token_profile_for_bootstrap(
                        Some(&project.config),
                        Some(&project.root),
                    ),
                }
            }
        };
        let source_root = project.configuration_path().map(Path::to_path_buf);
        // One value, used by everything that reads the tree: the watch, the walk that
        // feeds the graph and the search index, and the roots the engine registers. Two
        // derivations of "where my cache is" would be two chances to disagree, and a
        // disagreement here reads as a file that is indexed but never updated.
        // Besides the cache's own directory the list states the service directories of the
        // workspace root (`.git`, `target`, `node_modules`): a flat layout makes the
        // workspace root the watched scan root, and a fetch, a build or a package-manager
        // run inside them would otherwise arrive as a burst of workspace events, be
        // classified as sources, and walk the graph over files that are not sources.
        let source_exclusions: Vec<PathBuf> = cache.exclusions(&project.root);

        // A cache that contains a watched root would exclude that whole root from the
        // watch (see the hub below), leaving the server serving a tree it silently
        // stopped following. Refused rather than excluded, because the two are
        // indistinguishable from the outside: a typo in `--cache-dir` and a deliberate
        // choice look alike, and the deliberate one has no use.
        //
        // Asked of the same type the watch and the walk ask, and asked about the targets
        // the hub is actually given rather than the scan roots alone: the workspace root
        // rides along as a non-recursive target so config edits are delivered even in a
        // nested layout, and the exclusion is consulted before the config-file branch. A
        // check over scan roots only would let a cache covering the workspace root
        // swallow every config edit in silence — the same failure the check exists to
        // refuse, one target over.
        let scan_roots = project.source_roots();
        let watched: Vec<PathBuf> =
            crate::change_hub::watch_targets_for(&project.root, &scan_roots)
                .into_iter()
                .map(|target| target.path)
                .collect();
        let placement_exclusions = cache.placement_exclusions(&project.root);
        if let Some((hole, root)) =
            project_model::PathScope::new(&watched, &placement_exclusions).hole_covering_a_root()
        {
            // The list holds two kinds of hole, and the advice differs: a cache above a root
            // is moved by pointing --cache-dir elsewhere, while a service directory above a
            // root is not the user's to move — the root is. A cache placed onto a service
            // directory is the service directory first: moving the cache would not free the root.
            let is_a_service_directory =
                crate::cache::WorkspaceCacheLayout::is_service_directory(&project.root, &hole);
            return Err(if is_a_service_directory {
                WorkspaceInitError::ScanRootInsideServiceDirectory { service: hole, root }
            } else {
                WorkspaceInitError::CacheCoversScanRoot { cache: hole, root }
            });
        }

        // Claimed before any background pass starts, so the graph's very first build already
        // knows whether this daemon owns the workspace's derived caches or is the superseded
        // generation of a pair that overlaps over them.
        let workspace_lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);

        // Said once, here, because this is the only point EVERY boot passes. The engine's own
        // initialization has early exits before it builds a table — an unreachable shared
        // baseline, a store that will not open — and on those a dropped root would never be
        // named at all, which is the silence the warning exists to break. The resident builds
        // the same table on every config drift and deliberately says nothing: a line repeated
        // per rebuild buries the one that is new.
        crate::project::warn_about_rejected_roots(
            &crate::project::workspace_roots(&project, &source_exclusions).1,
        );

        let search_engine: SharedSearchEngine = super::shared_engine(None);
        let workspace_search_initializing = Arc::new(AtomicBool::new(true));
        let index_progress = IndexProgress::new();
        let semantic_runtime =
            Arc::new(Mutex::new(initial_semantic_runtime_status(&embedding_prefixes)));
        let overlay_warmup = Arc::new(Mutex::new(OverlayWarmupState::Pending));
        // Only the cheap, local part of baseline resolution runs here (config, env,
        // credential helper); the PG connect itself is deferred to a background thread
        // so a slow or unreachable server never delays the daemon's socket. The search
        // mode is therefore decided by configured INTENT (credentials resolved for a
        // postgres workspace baseline), not by connect success: a PG outage keeps the
        // workspace in Postgres mode with a visible issue instead of silently falling
        // back to (re)building a local index of the whole configuration.
        let token_layout_claim = Self::embedding_config_with_prefixes(Some(&embedding_prefixes))
            .ok()
            .flatten()
            .and_then(|config| {
                bsl_search::Embedder::new(config.embedder).token_layout_claim().map(str::to_owned)
            });
        let bootstrap = BaselineRuntime::workspace_bootstrap(Some(&project.root), &project.config)
            .with_token_layout_claim(token_layout_claim);
        let workspace_search_mode = Self::workspace_mode_for(&bootstrap);
        let embedding_publish_retry_budget = Self::embedding_publish_retry_budget();
        let baseline = match bootstrap {
            BaselineBootstrap::Immediate(runtime) => DeferredBaselineRuntime::ready(runtime),
            BaselineBootstrap::Connect(plan) => DeferredBaselineRuntime::spawn(*plan),
        };

        // The change hub owns the recursive workspace watcher and starts before any
        // consumer subscribes: the search engine is built on a background thread and must
        // not gate the watcher's lifecycle. It watches the whole drift-scan universe — the
        // config source root plus every extension root — so diagnostics/graph drift in
        // extensions is event-delivered, not left to the reconciler. Search subscribes as a
        // sink and preserves its prior behavior (mark only source-root `.bsl` paths dirty).
        //
        // Witnessed by `a_second_boot_over_a_matching_cache_declares_every_source_root_to_the_hub`,
        // and only a second boot can witness it: every graph path that BUILDS re-declares the
        // hub onto its own snapshot's roots, so a narrower set here would be repaired on a cold
        // boot before a reader could reach it. The publish that reuses a matching cache is the
        // one path that declares nothing. The window belongs to that test rather than to a
        // session: a serving daemon calls `warm_start` right after this constructor, and the
        // resident's publish re-declares the roots too.
        // The hub takes the same exclusion list every walk takes: the derived cache — the
        // server's own output, whose owned `<base>/workspaces/v1` family may sit inside the
        // recursive watch, where every index write would otherwise come back as an event
        // about the tree being analyzed — and the service directories of the workspace root.
        // The user's `[source].exclude` is kept apart from that list: it follows the
        // project on every re-declaration, and nothing declared inside it is carved out.
        let change_hub = WorkspaceChangeHub::start_targets_scoped(
            crate::change_hub::watch_targets_for(&project.root, &scan_roots),
            source_exclusions.clone(),
            project.source_exclusions().clone(),
        );

        // Subscribed here, synchronously, before the thread that reads disk even exists —
        // the same order the graph and the resident already keep, where the cursor is taken
        // first and the baseline scan second, so only what lands after the baseline needs
        // replaying. Search used to take it last, on a third thread, which left the window
        // between the two paid for by a full rescan of every root on essentially every boot.
        let sink_lease = crate::change_hub::CursorLease::new(change_hub.clone());

        // Created before the search-init thread so it can own the workspace graph: for
        // a local SQLite workspace the search-init drives a single fused parse pass
        // that builds the graph AND the search index, then publishes the graph through
        // this handle. A clone (cheap, shared `Arc`s) goes to the search thread; this
        // copy stays in `SharedState` for graph-tool serving and drift/reload. It carries
        // the hub so a graph freshness check invalidates its fingerprint cache on delivery.
        // On each graph publish/adopt (on the graph's own background thread) re-render the
        // search chunks marked context-dirty by an `.xml` drift, now that the graph has
        // caught up. Captures only shared handles; the closure never runs on a query path.
        // The daemon's one stop, created before the first owner that needs it: the retry
        // driver, the publish hook, the consumer and the backlog owner all take it, and every
        // wait any of them makes is released by the one call that raises it.
        let owners = super::OwnerStop::default();
        let scope_transport_stop = tokio_util::sync::CancellationToken::new();
        owners.set_scope_transport_stop(scope_transport_stop.clone());
        {
            // The hub's wait returns on a new generation, on `closing`, or on its caller's own
            // predicate — never on a bare wake. So the stop sets `closing`: without it an owner
            // parked on the hub sleeps out its whole timeout after the daemon has gone.
            let hub = change_hub.clone();
            owners.wakes(move || hub.interrupt_waiters());
        }
        SharedState::start_scope_guard(
            change_hub.clone(),
            source_dir.clone(),
            cache.clone(),
            owners.clone(),
            scope_transport_stop.clone(),
        );

        // The overlay retry driver exists only where an Embed pass exists: Postgres mode
        // with an embedder. It is created before the graph hook so a root transition can kick
        // the SAME owner; no second warmup worker is introduced.
        let overlay_retry = if !workspace_lease.coordination_failed()
            && matches!(workspace_search_mode, WorkspaceSearchMode::PostgresRemoteOverlay)
        {
            match Self::embedding_config_with_prefixes(Some(&embedding_prefixes)) {
                Ok(Some(_)) => Some(super::overlay_retry::OverlayRetry::spawn(
                    Arc::clone(&search_engine),
                    owners.clone(),
                    Arc::clone(&overlay_warmup),
                    Arc::clone(&semantic_runtime),
                    workspace_lease.clone(),
                    embedding_publish_retry_budget,
                )),
                Ok(None) => {
                    Self::set_overlay_warmup_state(
                        &overlay_warmup,
                        OverlayWarmupState::Skipped("no embedder configured".to_owned()),
                    );
                    None
                }
                Err(error) => {
                    Self::set_semantic_runtime_status(
                        &semantic_runtime,
                        SemanticRuntimeStatus::from_search_error(&error),
                    );
                    Self::set_overlay_warmup_state(
                        &overlay_warmup,
                        OverlayWarmupState::from_search_error(&error),
                    );
                    None
                }
            }
        } else {
            None
        };

        // ONE embed single-flight shared by the boot pass and the post-context/root refresh kick,
        // so overlapping passes collapse into one and the installed index is always built from
        // the latest store state. Root-relevant drift has its own epoch: it fences the narrow
        // validation→apply window without making an unrelated watched file reject the plan.
        let embed_flight = EmbedFlight::new();
        let root_drift_epoch = Arc::new(AtomicU64::new(0));
        // The hub first: the provider this hook installs reports owed marks through it.
        let graph = GraphState::for_workspace_with_cache(source_dir.clone(), cache.clone())
            .with_change_hub(change_hub.clone());
        let publish_hook = Self::build_publish_hook(
            Arc::clone(&search_engine),
            owners.clone(),
            graph.store().clone(),
            graph.owed_context_marks(),
            Arc::clone(&semantic_runtime),
            Arc::clone(&index_progress),
            Arc::clone(&embed_flight),
            overlay_retry.clone(),
            Arc::clone(&root_drift_epoch),
            embedding_prefixes.clone(),
            workspace_lease.clone(),
            embedding_publish_retry_budget,
        );
        let graph = graph
            .with_publish_hook(publish_hook)
            .with_lease(workspace_lease.clone())
            .with_owner_stop(owners.clone())
            .with_scope_transport_stop(scope_transport_stop.clone());
        // A test that has to ask a handler what it answers while the graph is genuinely
        // unconsulted takes the first build here, where the graph exists and no thread this
        // boot starts has run yet. It then holds production's own single flight, and every
        // other claimant is refused the way any losing one is.
        #[cfg(test)]
        crate::graph::test_support::hold_the_first_build(&graph);

        // The `metadata` tool reads the resident diagnostics host (per-MDO substrate for
        // `object`, Channel-2 `load_configuration` for `tree`/`info`); it is seeded and
        // kept fresh by the resident's own drift poll, so no separate configuration
        // snapshot is loaded here. The same resident serves the search overlay's incremental
        // reindex through the snapshot-source adapter.
        let diagnostics = DiagnosticsState::for_workspace(source_dir.clone())
            .with_excluded(source_exclusions.clone())
            .with_change_hub(change_hub.clone());
        let snapshot_source: Arc<dyn bsl_search::ModuleSnapshotSource> = Arc::new(
            crate::diagnostics_state::ResidentModuleSnapshotSource::new(diagnostics.clone()),
        );

        // The sink is started by this thread, once there is an engine to feed and a watch
        // to feed it from; the lease rides along so the cursor is released on every way out
        // that does not end in a running sink — including this spawn failing, where the
        // closure is dropped unrun.
        // Seeded from the project this boot has just parsed, so there is no "not computed
        // yet"; the graph watcher keeps it current from here on.
        let standalone_notice_slot = Arc::new(Mutex::new(super::StandaloneNotice::tracked(
            super::standalone_notice_of(&project),
        )));
        let overlay_backlog = super::overlay_backlog::OverlayBacklog::default();
        {
            // The backlog owner parks on a signal of its own, so the stop has to reach it too.
            let backlog = overlay_backlog.clone();
            owners.wakes(move || backlog.stop());
        }
        let search_consumer = Arc::new(Mutex::new(super::ConsumerPhase::Pending));
        // Started before the thread that builds the graph exists, so the watcher's cursor
        // predates the first pre-scan and no change can fall between the two.
        crate::graph::watcher::start(
            &graph,
            &change_hub,
            Some((source_dir.clone(), Arc::clone(&standalone_notice_slot))),
            owners.clone(),
        );
        Self::spawn_workspace_search_init(
            Arc::clone(&search_engine),
            Arc::clone(&workspace_search_initializing),
            Arc::clone(&index_progress),
            Arc::clone(&semantic_runtime),
            source_dir.clone(),
            change_hub.clone(),
            sink_lease,
            baseline.clone(),
            workspace_search_mode.clone(),
            graph.clone(),
            Arc::clone(&embed_flight),
            Arc::clone(&snapshot_source),
            workspace_lease.clone(),
            overlay_retry.clone(),
            Arc::clone(&root_drift_epoch),
            embedding_publish_retry_budget,
            owners.clone(),
            overlay_backlog.clone(),
            Arc::clone(&search_consumer),
            embedding_prefixes.clone(),
        );

        let reference_search = ReferenceSearchState::new(Some(&source_dir));
        Ok(Self {
            workspace_root: Some(source_dir),
            source_root,
            standalone_notice: standalone_notice_slot,
            onec_client: None,
            onec_connections: Default::default(),
            debug_session: Arc::new(Mutex::new(None)),
            search_engine,
            workspace_search_initializing,
            embed_flight,
            index_progress,
            semantic_runtime,
            overlay_warmup,
            workspace_search_mode,
            baseline,
            reference_search,
            graph,
            diagnostics,
            change_hub: Some(change_hub),
            workspace_lease,
            overlay_retry,
            tasks: rmcp::task_manager::TaskManager::new(),
            owners,
            scope_transport_stop,
            overlay_backlog,
            search_consumer,
        })
    }

    /// Retire this immutable workspace scope on a project-declaration event. Ordinary source
    /// body changes stay on the existing hot-reload path; only project inputs or a lost event
    /// batch trigger the inexpensive canonical project validation.
    fn start_scope_guard(
        hub: WorkspaceChangeHub,
        root: PathBuf,
        cache: crate::cache::WorkspaceCacheLayout,
        owners: super::OwnerStop,
        transport_stop: tokio_util::sync::CancellationToken,
    ) {
        let thread_owners = owners.clone();
        let spawn = std::thread::Builder::new()
            .name("workspace-cache-scope".to_owned())
            .spawn(move || {
                let _live = thread_owners.enter();
                let cursor = hub.subscribe();
                loop {
                    if thread_owners.is_stopped() {
                        break;
                    }
                    // Sample before draining: a delivery before this sample is
                    // included in the batch, and one after it makes the wait
                    // return. Sampling after drain would lose the window between
                    // the drain and the sample.
                    let generation = hub.generation();
                    let batch = hub.drain(cursor);
                    let project_input_changed = batch.rescan_required
                        || batch.entries.iter().any(|entry| {
                            entry
                                .canonical
                                .file_name()
                                .and_then(|name| name.to_str())
                                .is_some_and(project_model::is_project_input_file_name)
                        });
                    if project_input_changed {
                        let still_same_scope = crate::project::at(&root)
                            .map_err(|error| error.to_string())
                            .and_then(|project| {
                                cache.verify_project(&project).map_err(|error| error.to_string())
                            });
                        if let Err(error) = still_same_scope {
                            tracing::error!(%error, "workspace cache scope changed; stopping this backend");
                            thread_owners.stop_for_scope_change();
                            break;
                        }
                    }
                    hub.wait_for_change_or(
                        generation,
                        std::time::Duration::from_secs(24 * 60 * 60),
                        || thread_owners.is_stopped(),
                    );
                    if thread_owners.is_stopped() {
                        break;
                    }
                }
                hub.unsubscribe(cursor);
            });
        if let Err(error) = spawn {
            tracing::error!(%error, "workspace cache scope guard failed to start");
            owners.stop();
            transport_stop.cancel();
        }
    }

    // Each argument is a distinct shared handle the spawned init thread must own (engine,
    // progress, runtime status, indexer counter, roots, baseline, graph, embed flight).
    // Bundling them into a context struct would only move the same fields behind one name
    // without clarifying anything, so the small over-arity is accepted here.
    #[allow(clippy::too_many_arguments)]
    fn spawn_workspace_search_init(
        search_engine: SharedSearchEngine,
        initializing: Arc<AtomicBool>,
        index_progress: Arc<IndexProgress>,
        semantic_runtime: Arc<Mutex<SemanticRuntimeStatus>>,
        workspace_root: PathBuf,
        change_hub: WorkspaceChangeHub,
        sink_lease: crate::change_hub::CursorLease,
        baseline: DeferredBaselineRuntime,
        mode: WorkspaceSearchMode,
        graph: GraphState,
        embed_flight: Arc<EmbedFlight>,
        snapshot_source: Arc<dyn bsl_search::ModuleSnapshotSource>,
        lease: crate::workspace_lease::WorkspaceLease,
        overlay_retry: Option<Arc<super::overlay_retry::OverlayRetry>>,
        root_drift_epoch: Arc<AtomicU64>,
        embedding_publish_retry_budget: std::time::Duration,
        owners: super::OwnerStop,
        overlay_backlog: super::overlay_backlog::OverlayBacklog,
        search_consumer: Arc<Mutex<super::ConsumerPhase>>,
        embedding_prefixes: super::types::EmbeddingPrefixes,
    ) {
        let initializing_for_thread = Arc::clone(&initializing);
        // Every way out of the init that does not start the consumer abandons it — a thread
        // that never started included: its cursor is released and nothing will feed the index.
        let abandon =
            super::AbandonIfStill(Arc::clone(&search_consumer), super::ConsumerPhase::Pending);
        let spawned = std::thread::Builder::new()
            .name("bsl-search-init".to_owned())
            .spawn(move || {
                struct InitializingGuard(Arc<AtomicBool>);
                impl Drop for InitializingGuard {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Relaxed);
                    }
                }
                let _initializing = InitializingGuard(initializing_for_thread);
                let _abandon = abandon;
                tracing::info!("search engine initialization started in background");

                // The graph is a boot subsystem like the resident, not a lazy one. In
                // SqliteLocal the fused cold build below claims and builds it; the Postgres
                // branch never reaches that claim at all, which is why a PG workspace paid for
                // a whole-config graph build mid-session, on the first `graph`/`symbol_info`
                // call. Start it here instead — ahead of the baseline connect wait, which the
                // graph does not depend on. (Other early exits are covered by the catch-all
                // start after the init returns.)
                //
                // Mode-gated on purpose: in SqliteLocal an eager kick would win the
                // `Idle → Loading` transition that `try_begin_external_build` needs, the fused
                // claim would fail, and one parse pass producing both graph and search chunks
                // would degrade into two. A warm graph cache makes either start cheap —
                // `run_load` publishes the cached build instead of rebuilding.
                // Postgres mode needs the baseline connect's outcome before it can load
                // the manifest; waiting HERE keeps the wait on this background thread
                // (never a request path) and off the slot's lock. On timeout the init
                // proceeds without a service and fails exactly like today's PG-error
                // path — offline with a visible issue, never a local reindex.
                let external_baseline = match mode {
                    WorkspaceSearchMode::PostgresRemoteOverlay => {
                        if !baseline.wait_ready(BASELINE_CONNECT_WAIT) {
                            tracing::warn!(
                                timeout_secs = BASELINE_CONNECT_WAIT.as_secs(),
                                "baseline connect still pending; workspace search init proceeds degraded"
                            );
                        }
                        baseline.external()
                    }
                    WorkspaceSearchMode::SqliteLocal => None,
                };
                let mut sink_lease = sink_lease;
                let init = Self::init_workspace_search_engine(
                    &workspace_root,
                    Some((&change_hub, super::sync::WatchWaitPolicy::PRODUCTION)),
                    mode,
                    external_baseline,
                    &graph,
                    &lease,
                    &owners,
                    &embedding_prefixes,
                );

                let mut init = match init {
                    Ok(Some(init)) => init,
                    Ok(None) => {
                        if let Some(retry) = &overlay_retry {
                            retry.disarm();
                        }
                        tracing::info!(
                            superseded = lease.is_superseded(),
                            released = lease.is_released(),
                            "workspace search initialization stopped without publication"
                        );
                        return;
                    }
                    Err(error) => {
                        Self::set_semantic_runtime_status(
                            &semantic_runtime,
                            SemanticRuntimeStatus::from_search_error(&error),
                        );
                        // A product failure is not supersession: keep the graph independently
                        // available, but stop search retries because no engine can publish.
                        graph.ensure_loading();
                        if let Some(retry) = &overlay_retry {
                            retry.disarm();
                        }
                        tracing::warn!(%error, "workspace search engine initialization failed");
                        return;
                    }
                };

                let pending_embed = init.pending_embed.take();
                let needs_overlay_warmup =
                    matches!(init.mode, WorkspaceSearchMode::PostgresRemoteOverlay);

                // When the fused build deferred embeddings, mark the runtime `Indexing`
                // BEFORE the engine becomes visible. The published engine still has an
                // empty vector index; without this ordering a concurrent semantic query
                // could reach `engine.search` on that empty index and return a silent
                // zero instead of degrading to lexical.
                let status_after_publish = semantic_runtime_status_after_publish(
                    &init.engine,
                    &init.mode,
                    pending_embed.is_some(),
                    &embedding_prefixes,
                    init.semantic_failure,
                );

                // `context_dirty` persists across restarts, so a prior run may have left marks
                // nothing of this run placed. Capture whether any survive AND the mark
                // high-water at THIS instant — both read off the engine BEFORE it moves into the
                // shared handle, and before the search consumer can stamp a mark of its own.
                // They predate every fact this run's hub delivers, so they are handed to the
                // graph as fact `0` once the engine is published and the hook can reach it.
                let has_leftover_marks =
                    init.engine.context_dirty_paths("code").map(|m| !m.is_empty()).unwrap_or(false);
                let leftover_bound = init.engine.mark_seq_handle().load(Ordering::SeqCst);

                // Wire the resident snapshot source so the overlay reindex can read text+parse
                // from the shared resident host. Set before publish so the first query already
                // sees it; the resident read itself happens in a point refresh's phase B, off the
                // engine lock, so the two locks never nest.
                init.engine.set_module_snapshot_source(snapshot_source);

                // Bring the workspace overlay online BEFORE publishing. The overlay is inert until
                // initialized (a point refresh captures nothing before that), so the
                // resident-fed incremental reindex — and overlay edit-freshness generally — is
                // unreachable in local SQLite mode without this. Done here, on the still-owned
                // engine, so it holds NO engine lock: `Prime`'s disk scan must not serialize behind
                // the shared lock (I3), and the cold FTS branch already indexes disk before
                // publishing, so a warm prime delays publish no differently.
                let overlay_result = match init.overlay_init {
                    OverlayInit::Clean => Self::startup_apply_once(&lease, &owners, || {
                        init.engine.initialize_workspace_overlay_clean()
                    }),
                    OverlayInit::Prime => match init
                        .engine
                        .prime_workspace_overlay_fenced(|apply| Self::startup_apply(&lease, &owners, apply))
                    {
                        Ok(bsl_search::FenceOutcome::Applied(())) => Ok(Some(())),
                        Ok(bsl_search::FenceOutcome::TransientRefusal) => {
                            unreachable!("startup_apply retries transient refusals")
                        }
                        Ok(
                            bsl_search::FenceOutcome::Superseded
                            | bsl_search::FenceOutcome::Released,
                        ) => Ok(None),
                        // The overlay was installed; losing some vectors must not withhold the
                        // engine and with it the lexical search. Nothing is latched, since no
                        // local owner could clear it: the refused keys stay dirty and are rebuilt
                        // lexically, and their vectors wait for the next boot's prime, as before.
                        Err(error) => match error.embedding_failure() {
                            Some(_) => {
                                tracing::warn!(
                                    "workspace overlay prime left entries without vectors: {error}"
                                );
                                Ok(Some(()))
                            }
                            None => Err(error),
                        },
                    },
                    OverlayInit::RemoteWarmup => Ok(Some(())),
                };
                match overlay_result {
                    Ok(Some(())) => {}
                    Ok(None) => {
                        if let Some(retry) = &overlay_retry {
                            retry.disarm();
                        }
                        return;
                    }
                    Err(error) => {
                        Self::set_semantic_runtime_status(
                            &semantic_runtime,
                            SemanticRuntimeStatus::from_search_error(&error),
                        );
                        graph.ensure_loading();
                        if let Some(retry) = &overlay_retry {
                            retry.disarm();
                        }
                        return;
                    }
                }

                // Host publication follows the long-lived order used by incremental mutations:
                // engine mutex first, then the lease lifecycle mutex and file lock inside the
                // callback. Status becomes visible in the same admitted group as the engine.
                let mut engine_to_publish = Some(init.engine);
                let mut status_to_set = Some(status_after_publish);
                let published = Self::publish_engine_with_retry(
                    &search_engine,
                    &owners,
                    |slot| {
                        Self::search_fence_outcome(lease.publish_short(&mut (), |_| {
                            *slot = engine_to_publish.take();
                            if let Some(status) = status_to_set.take() {
                                Self::set_semantic_runtime_status(&semantic_runtime, status);
                            }
                            Ok(())
                        }))
                    },
                    std::time::Instant::now,
                    |delay| owners.sleep(delay),
                );
                match published {
                    Ok(Some(())) => {}
                    Ok(None) => {
                        // Superseded, handed over, or told to go. Leaving is not failing: the
                        // stop gets the terminal status the embed pass already writes for it,
                        // and a takeover writes nothing over the owner that won.
                        if owners.is_stopped() {
                            Self::set_semantic_runtime_status(
                                &semantic_runtime,
                                SemanticRuntimeStatus::Stopped,
                            );
                        }
                        if let Some(retry) = &overlay_retry {
                            retry.disarm();
                        }
                        return;
                    }
                    Err(error) => {
                        Self::set_semantic_runtime_status(
                            &semantic_runtime,
                            SemanticRuntimeStatus::from_search_error(&error),
                        );
                        return;
                    }
                }

                graph.ensure_loading();

                // Only now: the consumer drains into the published engine, and one started
                // before this point would drop every batch it read into an engine that was
                // not there yet. Whatever became of the watch — armed, polling, or still
                // arming — the cursor has been collecting since the boot subscribed it.
                if !overlay_backlog.start(
                    Arc::clone(&search_engine),
                    lease.clone(),
                    overlay_retry.clone(),
                    owners.clone(),
                ) {
                    // The marks still get placed; what is missing is the owner that reads
                    // them back. That state is reported — the backlog says `Stopped` and
                    // every search answer carries the degraded reason — but it is worth
                    // saying once in the log too, because nothing will start this owner
                    // again for the life of the daemon.
                    tracing::warn!(
                        "the overlay backlog owner could not start; changed files stay unread                          and search answers degrade until the daemon restarts"
                    );
                }
                if let Some(cursor) = sink_lease.cursor() {
                    if Self::spawn_search_sink(
                        change_hub.clone(),
                        cursor,
                        Arc::clone(&search_engine),
                        graph.clone(),
                        overlay_retry.clone(),
                        Arc::clone(&root_drift_epoch),
                        lease.clone(),
                        owners.clone(),
                        overlay_backlog.clone(),
                        Arc::clone(&search_consumer),
                    ) {
                        sink_lease.handed_over();
                    }
                }

                if has_leftover_marks {
                    graph.consume_leftover_marks(leftover_bound);
                }

                // A boot that found the cached graph built under a different extension topology
                // asked for a whole-collection re-render before this engine existed to run it.
                // The publish that followed could not hand the request anywhere, so it is
                // honoured here — otherwise files the build skipped as byte-identical keep the
                // contexts they were given under the old topology.
                graph.flush_hook_obligations();

                tracing::info!("search engine initialization complete");

                if let Some(pending) = pending_embed {
                    // The boot pass shares the ONE embed single-flight with the post-refresh
                    // kick, so a kick that lands while boot runs is absorbed (its NULL chunks
                    // picked up by boot's rerun loop) instead of racing a second index swap.
                    Self::spawn_embed_pass(
                        Arc::clone(&search_engine),
                        owners.clone(),
                        Arc::clone(&semantic_runtime),
                        Arc::clone(&index_progress),
                        Arc::clone(&embed_flight),
                        lease.clone(),
                        pending.db_path,
                        pending.config,
                        embedding_publish_retry_budget,
                    );
                }

                // The startup warmup goes through the SAME single-flight the retries do:
                // a direct spawn here would race a driver-triggered publication and let
                // last-writer-wins install the older plan. The driver owns the semantic
                // status transitions around each pass; the `!initialized` signal makes the
                // first pass unconditional.
                if needs_overlay_warmup {
                    if let Some(retry) = &overlay_retry {
                        // The fresh kick, not the bare one: the worker's first tick may have
                        // raced this publication, collected a transient "engine unavailable"
                        // and armed its backoff — the engine appearing is exactly the kind of
                        // new fact that resets it.
                        retry.kick_fresh();
                    }
                }
            });
        if spawned.is_err() {
            initializing.store(false, Ordering::Relaxed);
        }
    }

    pub fn reference(project_root: Option<PathBuf>) -> Self {
        let reference_search = ReferenceSearchState::new(project_root.as_deref());
        reference_search.ensure_loading();

        Self {
            workspace_root: None,
            source_root: None,
            standalone_notice: Arc::new(Mutex::new(super::StandaloneNotice::default())),
            onec_client: None,
            onec_connections: Default::default(),
            debug_session: Arc::new(Mutex::new(None)),
            search_engine: Arc::clone(&reference_search.engine),
            workspace_search_initializing: Arc::new(AtomicBool::new(false)),
            embed_flight: EmbedFlight::new(),
            index_progress: Arc::clone(&reference_search.progress),
            semantic_runtime: Arc::clone(&reference_search.semantic_runtime),
            overlay_warmup: Arc::new(Mutex::new(OverlayWarmupState::Pending)),
            workspace_search_mode: WorkspaceSearchMode::SqliteLocal,
            baseline: reference_search.baseline.clone(),
            reference_search,
            graph: GraphState::disabled(),
            diagnostics: DiagnosticsState::disabled(),
            change_hub: None,
            workspace_lease: crate::workspace_lease::WorkspaceLease::unmanaged(),
            overlay_retry: None,
            tasks: rmcp::task_manager::TaskManager::new(),
            owners: super::OwnerStop::default(),
            scope_transport_stop: tokio_util::sync::CancellationToken::new(),
            overlay_backlog: Default::default(),
            search_consumer: Arc::new(Mutex::new(super::ConsumerPhase::Stopped)),
        }
    }

    pub fn shared() -> Self {
        let reference_search = ReferenceSearchState::new(None);
        Self {
            workspace_root: None,
            source_root: None,
            standalone_notice: Arc::new(Mutex::new(super::StandaloneNotice::default())),
            onec_client: None,
            onec_connections: Default::default(),
            debug_session: Arc::new(Mutex::new(None)),
            search_engine: super::shared_engine(None),
            workspace_search_initializing: Arc::new(AtomicBool::new(false)),
            embed_flight: EmbedFlight::new(),
            index_progress: IndexProgress::new(),
            semantic_runtime: Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            overlay_warmup: Arc::new(Mutex::new(OverlayWarmupState::Pending)),
            workspace_search_mode: WorkspaceSearchMode::SqliteLocal,
            baseline: DeferredBaselineRuntime::absent(),
            reference_search,
            graph: GraphState::disabled(),
            diagnostics: DiagnosticsState::disabled(),
            change_hub: None,
            workspace_lease: crate::workspace_lease::WorkspaceLease::unmanaged(),
            overlay_retry: None,
            tasks: rmcp::task_manager::TaskManager::new(),
            owners: super::OwnerStop::default(),
            scope_transport_stop: tokio_util::sync::CancellationToken::new(),
            overlay_backlog: Default::default(),
            search_consumer: Arc::new(Mutex::new(super::ConsumerPhase::Stopped)),
        }
    }

    #[cfg(test)]
    pub(super) fn embedding_config() -> Result<Option<bsl_search::SearchConfig>, SearchError> {
        Self::embedding_config_with_prefixes(None)
    }

    pub(super) fn embedding_config_with_prefixes(
        prefixes: Option<&super::types::EmbeddingPrefixes>,
    ) -> Result<Option<bsl_search::SearchConfig>, SearchError> {
        let token_policy = prefixes
            .and_then(|prefixes| prefixes.token_profile.as_ref())
            .map(|profile| {
                profile.token_policy.clone().ok_or_else(|| {
                    SearchError::from(EmbeddingFailure::new(
                        bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig,
                    ))
                })
            })
            .transpose()?;
        // Окружение разработчика в тесты не протекает. `EMBEDDING_URL`, выставленный в
        // оболочке, уводил прогон к настоящему сервису эмбеддингов: запросы упирались в
        // сетевые таймауты и повторы, и завершение состояния ждало их десятками минут —
        // вместо проверки логики тест мерил доступность чужой сети. Внешний эмбеддер в
        // тестовой сборке включает только явная подмена (`test_support::mock_embedding_env`
        // либо `BSL_TEST_EMBEDDING` рядом с ручной установкой адреса).
        #[cfg(test)]
        if std::env::var_os("BSL_TEST_EMBEDDING").is_none() {
            return Ok(None);
        }

        let Ok(base_url) = std::env::var("EMBEDDING_URL") else { return Ok(None) };
        // The model must be declared explicitly: a wrong default would silently mix
        // vectors from different models into one index. Unset means FTS-only.
        let Ok(model) = std::env::var("EMBEDDING_MODEL") else { return Ok(None) };
        let max_request_bytes = bsl_search::EmbedderConfig::request_bytes_from_env()?;
        // No declared width means the request carries no `dimensions` field at all and
        // the model keeps its native one: OpenAI-compatible endpoints that refuse the
        // parameter (litellm's `UnsupportedParamsError` among them) answer only then.
        // An explicit width still wins and doubles as the expectation for the response,
        // so the index is never built for a width nobody declared.
        let dim = bsl_search::EmbedderConfig::dim_from_env()?;
        // Background index/embedding workers otherwise saturate every core and starve interactive
        // `search_code` for tens of seconds during the one-time build. Default to leaving two cores
        // free for queries; an explicit EMBEDDING_CONCURRENCY still wins (operators who want max
        // build throughput set it).
        let concurrency: usize = std::env::var("EMBEDDING_CONCURRENCY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get().saturating_sub(2).max(2))
                    .unwrap_or(4)
            });

        Ok(Some(bsl_search::SearchConfig {
            embedder: bsl_search::EmbedderConfig {
                base_url,
                model,
                dim,
                api_key: std::env::var("EMBEDDING_API_KEY").ok(),
                provider: std::env::var("EMBEDDING_PROVIDER").ok(),
                query_prefix: prefixes.map_or_else(
                    || std::env::var("EMBEDDING_QUERY_PREFIX").unwrap_or_default(),
                    |prefixes| prefixes.query.clone(),
                ),
                document_prefix: prefixes.map_or_else(
                    || std::env::var("EMBEDDING_DOCUMENT_PREFIX").unwrap_or_default(),
                    |prefixes| prefixes.document.clone(),
                ),
                max_request_bytes,
                token_policy,
            },
            execution: bsl_search::EmbeddingExecutionPolicy {
                batch_size: std::env::var("EMBEDDING_BATCH_SIZE")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(32),
                concurrency,
                progress_interval: std::env::var("EMBEDDING_PROGRESS_INTERVAL")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(20),
            },
        }))
    }

    fn embedding_publish_retry_budget() -> std::time::Duration {
        let Ok(raw) = std::env::var(EMBEDDING_PUBLISH_RETRY_BUDGET_ENV) else {
            return DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET;
        };
        let budget = raw
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds > 0)
            .map(std::time::Duration::from_secs);
        if let Some(budget) =
            budget.filter(|budget| std::time::Instant::now().checked_add(*budget).is_some())
        {
            return budget;
        }
        #[cfg(test)]
        EMBEDDING_PUBLISH_RETRY_BUDGET_WARNINGS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tracing::warn!(
            variable = EMBEDDING_PUBLISH_RETRY_BUDGET_ENV,
            default_secs = DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET.as_secs(),
            "invalid embedding publish retry budget; using default"
        );
        DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET
    }

    fn open_semantic_search_engine(
        db_path: &Path,
        config: bsl_search::SearchConfig,
    ) -> Option<SearchEngine> {
        let model = config.embedder.model.clone();
        match SearchEngine::new(db_path, config) {
            Ok(engine) => {
                tracing::info!(
                    files = engine.file_count().unwrap_or(0),
                    chunks = engine.chunk_count().unwrap_or(0),
                    vectors = engine.vector_count(),
                    model,
                    "search engine loaded (FTS + semantic)"
                );
                Some(engine)
            }
            Err(e) => {
                tracing::warn!("failed to init search engine with embedder: {e}");
                None
            }
        }
    }

    fn open_fts_only_search_engine(db_path: &Path) -> Option<SearchEngine> {
        match SearchEngine::fts_only(db_path) {
            Ok(engine) => {
                tracing::info!(
                    files = engine.file_count().unwrap_or(0),
                    chunks = engine.chunk_count().unwrap_or(0),
                    "search engine loaded (FTS-only)"
                );
                Some(engine)
            }
            Err(e) => {
                tracing::warn!("failed to init FTS-only search engine: {e}");
                None
            }
        }
    }

    #[cfg(test)]
    fn open_search_engine(db_path: &Path) -> Result<Option<SearchEngine>, SearchError> {
        Self::open_search_engine_with_prefixes(db_path, None)
    }

    fn open_search_engine_with_prefixes(
        db_path: &Path,
        prefixes: Option<&super::types::EmbeddingPrefixes>,
    ) -> Result<Option<SearchEngine>, SearchError> {
        match Self::embedding_config_with_prefixes(prefixes) {
            Ok(Some(config)) => Ok(Self::open_semantic_search_engine(db_path, config)
                .or_else(|| Self::open_fts_only_search_engine(db_path))),
            Ok(None) => Ok(Self::open_fts_only_search_engine(db_path)),
            Err(error)
                if prefixes
                    .is_some_and(|prefixes| is_invalid_token_profile_only(&error, prefixes)) =>
            {
                tracing::warn!("embedding configuration is invalid; opening lexical search only");
                Ok(Self::open_fts_only_search_engine(db_path))
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn startup_apply<T>(
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
        mut apply: impl FnMut() -> Result<T, bsl_search::SearchError>,
    ) -> bsl_search::FenceOutcome<Result<T, bsl_search::SearchError>> {
        Self::startup_retry(
            || Self::search_fence_outcome(lease.publish_short(&mut (), |_| apply())),
            std::time::Instant::now,
            // The boot is an owner like any other: its pauses end when the daemon stops, and
            // it leaves instead of trying again.
            |delay| stop.sleep(delay),
        )
    }

    pub(super) fn startup_apply_checkpointed(
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
        mut apply: impl FnMut(
            &mut dyn FnMut() -> std::ops::ControlFlow<()>,
        ) -> std::ops::ControlFlow<(), Result<(), bsl_search::SearchError>>,
    ) -> bsl_search::FenceOutcome<Result<(), bsl_search::SearchError>> {
        Self::startup_retry(
            || {
                Self::search_fence_outcome(
                    lease.publish_checkpointed(|checkpoint| apply(checkpoint)),
                )
            },
            std::time::Instant::now,
            |delay| stop.sleep(delay),
        )
    }

    /// `sleep` answers whether the daemon stopped while it waited. A pause that ends because
    /// the daemon is leaving is not a pause to retry after: the boot has nothing left to
    /// publish, and a loop that ignored the answer would spend its whole budget in an instant
    /// — `stop.sleep` returns at once once the stop is raised.
    fn startup_retry<T>(
        mut attempt: impl FnMut() -> bsl_search::FenceOutcome<Result<T, bsl_search::SearchError>>,
        mut now: impl FnMut() -> std::time::Instant,
        mut sleep: impl FnMut(std::time::Duration) -> bool,
    ) -> bsl_search::FenceOutcome<Result<T, bsl_search::SearchError>> {
        use super::retry_window::{RetryDecision, RetryOwner, RetryWindow};

        let mut retry = RetryWindow::new(RetryOwner::Startup);
        loop {
            match attempt() {
                bsl_search::FenceOutcome::TransientRefusal => {
                    match retry.refused(now(), std::time::Duration::from_secs(2)) {
                        RetryDecision::RetryAfter(delay) => {
                            if sleep(delay) {
                                // Left, not published: the same answer a handover gives, and
                                // every caller already reads it as "nothing was written".
                                return bsl_search::FenceOutcome::Released;
                            }
                        }
                        RetryDecision::Stop(_) => {
                            return bsl_search::FenceOutcome::Applied(Err(
                                bsl_search::SearchError::Index(
                                    "workspace lease startup retry budget exhausted".to_owned(),
                                ),
                            ));
                        }
                    }
                }
                outcome => return outcome,
            }
        }
    }

    /// Publish the engine into the shared slot, retrying the lease the way every startup
    /// publication does — and holding the engine for the ATTEMPT only.
    ///
    /// The order the long-lived mutations use, engine mutex first and then the lease, is a
    /// property of one attempt. The pause between attempts is not: a transient refusal means
    /// another process holds the lease lock, and sleeping that out under the engine mutex
    /// stops every graph publication, every mark consumption and every request that needs the
    /// engine — for as long as the FOREIGN holder lasts. A request path cannot wait that out:
    /// it is capped at thirty seconds and answers `TimedOut`.
    fn publish_engine_with_retry(
        shared: &super::SharedSearchEngine,
        owners: &super::OwnerStop,
        mut publish: impl FnMut(
            &mut Option<bsl_search::SearchEngine>,
        )
            -> bsl_search::FenceOutcome<Result<(), bsl_search::SearchError>>,
        now: impl FnMut() -> std::time::Instant,
        sleep: impl FnMut(std::time::Duration) -> bool,
    ) -> Result<Option<()>, bsl_search::SearchError> {
        let outcome = Self::startup_retry(
            || match shared.acquire_for_owner(owners) {
                Ok(mut guard) => publish(&mut guard),
                // Leaving is not failing. `Released` is what every caller already reads as
                // "nothing was written", and it is what a handover answers too.
                Err(crate::tools::search::OwnerLockRefused::Closing) => {
                    bsl_search::FenceOutcome::Released
                }
                Err(error) => {
                    bsl_search::FenceOutcome::Applied(Err(bsl_search::SearchError::Index(format!(
                        "workspace search engine lock poisoned: {error}"
                    ))))
                }
            },
            now,
            sleep,
        );
        match outcome {
            bsl_search::FenceOutcome::Applied(Ok(())) => Ok(Some(())),
            bsl_search::FenceOutcome::Applied(Err(error)) => Err(error),
            bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released => Ok(None),
            bsl_search::FenceOutcome::TransientRefusal => {
                unreachable!("startup_retry retries transient refusals")
            }
        }
    }

    fn startup_apply_once<T>(
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
        operation: impl FnOnce() -> Result<T, bsl_search::SearchError>,
    ) -> Result<Option<T>, bsl_search::SearchError> {
        let mut operation = Some(operation);
        let mut value = None;
        let outcome = Self::startup_apply(lease, stop, || {
            value = Some(operation.take().expect("startup apply runs once")()?);
            Ok(())
        });
        match outcome {
            bsl_search::FenceOutcome::Applied(Ok(())) => Ok(value),
            bsl_search::FenceOutcome::Applied(Err(error)) => Err(error),
            bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released => Ok(None),
            bsl_search::FenceOutcome::TransientRefusal => {
                unreachable!("startup_apply retries transient refusals")
            }
        }
    }

    fn startup_apply_checkpointed_value<T>(
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
        mut operation: impl FnMut(
            &mut dyn FnMut() -> std::ops::ControlFlow<()>,
        )
            -> std::ops::ControlFlow<(), Result<T, bsl_search::SearchError>>,
    ) -> Result<Option<T>, bsl_search::SearchError> {
        let mut value = None;
        let outcome = Self::startup_apply_checkpointed(lease, stop, |checkpoint| {
            match operation(checkpoint) {
                std::ops::ControlFlow::Break(()) => std::ops::ControlFlow::Break(()),
                std::ops::ControlFlow::Continue(Err(error)) => {
                    std::ops::ControlFlow::Continue(Err(error))
                }
                std::ops::ControlFlow::Continue(Ok(result)) => {
                    value = Some(result);
                    std::ops::ControlFlow::Continue(Ok(()))
                }
            }
        });
        match outcome {
            bsl_search::FenceOutcome::Applied(Ok(())) => Ok(value),
            bsl_search::FenceOutcome::Applied(Err(error)) => Err(error),
            bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released => Ok(None),
            bsl_search::FenceOutcome::TransientRefusal => {
                unreachable!("startup apply retries transient refusals")
            }
        }
    }

    fn open_search_engine_fenced(
        db_path: &Path,
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
        embedding_prefixes: &super::types::EmbeddingPrefixes,
    ) -> Result<Option<OpenedSearchEngine>, bsl_search::SearchError> {
        let token_profile_requested = embedding_prefixes.token_profile.is_some();
        let mut semantic_failure = None;
        let embedding = match Self::embedding_config_with_prefixes(Some(embedding_prefixes)) {
            Ok(config) => config,
            Err(error) if is_invalid_token_profile_only(&error, embedding_prefixes) => {
                semantic_failure = error.embedding_failure();
                None
            }
            Err(error) => return Err(error),
        };
        let opened = match embedding {
            Some(config) => match SearchEngine::new_fenced(db_path, config, |apply| {
                Self::startup_apply_checkpointed(lease, stop, apply)
            }) {
                Ok(opened) => Ok(opened),
                Err(error)
                    if is_token_layout_refusal(&error)
                        || is_invalid_token_profile_only(&error, embedding_prefixes) =>
                {
                    semantic_failure = Some(error.embedding_failure().unwrap_or_else(|| {
                        EmbeddingFailure::new(
                            bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig,
                        )
                    }));
                    SearchEngine::fts_only_fenced(db_path, |apply| {
                        Self::startup_apply_checkpointed(lease, stop, apply)
                    })
                }
                Err(error) => return Err(error),
            },
            None => SearchEngine::fts_only_fenced(db_path, |apply| {
                Self::startup_apply_checkpointed(lease, stop, apply)
            }),
        }?;
        Ok(match opened {
            bsl_search::FenceOutcome::Applied(engine) => {
                if token_profile_requested && semantic_failure.is_none() && !engine.has_semantic() {
                    semantic_failure = Some(EmbeddingFailure::new(
                        bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig,
                    ));
                }
                Some(OpenedSearchEngine { engine, semantic_failure })
            }
            bsl_search::FenceOutcome::TransientRefusal => {
                unreachable!("startup_apply retries transient refusals")
            }
            bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released => None,
        })
    }

    fn open_workspace_overlay_search_engine_fenced(
        db_path: &Path,
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
        embedding_prefixes: &super::types::EmbeddingPrefixes,
    ) -> Result<Option<OpenedSearchEngine>, bsl_search::SearchError> {
        let token_profile_requested = embedding_prefixes.token_profile.is_some();
        let mut semantic_failure = None;
        let embedding = match Self::embedding_config_with_prefixes(Some(embedding_prefixes)) {
            Ok(config) => config,
            Err(error) if is_invalid_token_profile_only(&error, embedding_prefixes) => {
                semantic_failure = error.embedding_failure();
                None
            }
            Err(error) => return Err(error),
        };
        let opened = match embedding {
            Some(config) => {
                match SearchEngine::semantic_overlay_only_fenced(db_path, config, |apply| {
                    Self::startup_apply_checkpointed(lease, stop, apply)
                }) {
                    Ok(opened) => Ok(opened),
                    Err(error)
                        if is_token_layout_refusal(&error)
                            || is_invalid_token_profile_only(&error, embedding_prefixes) =>
                    {
                        semantic_failure = Some(error.embedding_failure().unwrap_or_else(|| {
                            EmbeddingFailure::new(
                                bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig,
                            )
                        }));
                        SearchEngine::fts_only_fenced(db_path, |apply| {
                            Self::startup_apply_checkpointed(lease, stop, apply)
                        })
                    }
                    Err(error) => return Err(error),
                }
            }
            None => SearchEngine::fts_only_fenced(db_path, |apply| {
                Self::startup_apply_checkpointed(lease, stop, apply)
            }),
        }?;
        Ok(match opened {
            bsl_search::FenceOutcome::Applied(engine) => {
                if token_profile_requested && semantic_failure.is_none() && !engine.has_semantic() {
                    semantic_failure = Some(EmbeddingFailure::new(
                        bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig,
                    ));
                }
                Some(OpenedSearchEngine { engine, semantic_failure })
            }
            bsl_search::FenceOutcome::TransientRefusal => {
                unreachable!("startup_apply retries transient refusals")
            }
            bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released => None,
        })
    }

    /// Configure the engine and declare whether it serves an external baseline.
    ///
    /// Written as one named step because the two halves belong to different worlds: the
    /// configuration mutates PROCESS state (`initialize_workspace_roots` refuses a second
    /// table outright), while the declaration is a store transaction the startup retry may
    /// roll back and run again. Keeping the configuration inside that retried closure is what
    /// turns ordinary lock contention into `workspace roots are already initialized`.
    fn configure_and_declare_baseline(
        engine: &mut SearchEngine,
        roots: bsl_search::WorkspaceRoots,
        hash_mode: BaselineHashMode,
        serves_external_baseline: bool,
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
    ) -> Result<Option<()>, bsl_search::SearchError> {
        Self::configure_workspace_engine(engine, roots, hash_mode)?;
        Self::startup_apply_checkpointed_value(lease, stop, |checkpoint| {
            engine.set_serves_external_baseline_checkpointed(serves_external_baseline, checkpoint)
        })
    }

    fn configure_workspace_engine(
        engine: &mut SearchEngine,
        workspace_roots: bsl_search::WorkspaceRoots,
        hash_mode: BaselineHashMode,
    ) -> Result<(), bsl_search::SearchError> {
        engine.initialize_workspace_roots(workspace_roots)?;
        engine.set_workspace_baseline_hash_mode(hash_mode);
        Ok(())
    }

    fn roots_of(
        project: &project_model::Project,
        excluded: &[PathBuf],
    ) -> bsl_search::WorkspaceRoots {
        crate::project::workspace_roots(project, excluded).0
    }

    fn semantic_runtime_status_for_mode(
        engine: &SearchEngine,
        mode: &WorkspaceSearchMode,
    ) -> SemanticRuntimeStatus {
        match mode {
            WorkspaceSearchMode::SqliteLocal | WorkspaceSearchMode::PostgresRemoteOverlay => {
                if engine.has_semantic() {
                    SemanticRuntimeStatus::Ready
                } else {
                    SemanticRuntimeStatus::Disabled
                }
            }
        }
    }

    /// Whether the persisted manifest was fetched for exactly this snapshot. The
    /// snapshot id is the strong per-publish key; the fingerprint must ALSO agree
    /// (including a both-`None` pair from publishers that never stamp one) so a
    /// re-published snapshot that reused an id can never serve stale fingerprints.
    fn baseline_manifest_matches_snapshot(
        record: &bsl_search::BaselineManifestRecord,
        snapshot: &bsl_search::Snapshot,
    ) -> bool {
        record.snapshot_id == snapshot.id.0 && record.fingerprint == snapshot.fingerprint
    }

    /// A failed Postgres-mode init must not leave a manifest behind that a later boot
    /// could mistake for a valid warm cache. The clear itself failing only costs that
    /// boot a manifest re-download, so it is not worth failing over.
    fn clear_baseline_manifest_best_effort(
        store: &bsl_search::Store,
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
    ) {
        let cleared = Self::startup_apply_checkpointed_value(lease, stop, |checkpoint| match store
            .clear_baseline_manifest_checkpointed(checkpoint)
        {
            Ok(std::ops::ControlFlow::Continue(())) => std::ops::ControlFlow::Continue(Ok(())),
            Ok(std::ops::ControlFlow::Break(())) => std::ops::ControlFlow::Break(()),
            Err(error) => std::ops::ControlFlow::Continue(Err(error)),
        });
        if let Err(error) = cleared {
            tracing::warn!("failed to clear stale workspace baseline manifest: {error}");
        }
    }

    /// `watch` is the change hub this boot's baseline must not outrun, with how long to
    /// wait for it. Every read below is a baseline, and a baseline taken before the watch
    /// armed can be older than the oldest change anyone will ever report — a window that
    /// used to be paid for afterwards by a full rescan of every root. Waiting instead of
    /// paying costs the arming time once; the rescan cost a walk plus a stat and a full
    /// read of every file in the configuration, on every boot. `None` (tests, and the
    /// entry points that have no hub) waits for nothing and reports no watch.
    // Keep the frozen input profile explicit beside the existing workspace ownership controls.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn init_workspace_search_engine(
        workspace_root: &std::path::Path,
        watch: Option<(&WorkspaceChangeHub, super::sync::WatchWaitPolicy)>,
        mode: WorkspaceSearchMode,
        external_baseline: Option<Arc<ExternalBaselineService>>,
        graph: &GraphState,
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
        embedding_prefixes: &super::types::EmbeddingPrefixes,
    ) -> Result<Option<WorkspaceSearchInit>, bsl_search::SearchError> {
        // The graph carries the resolved cache layout, so this pass reads the tree
        // through the same holes the watch does — the cache and the service directories
        // of the workspace root — instead of re-deriving where the cache is.
        let excluded: Vec<PathBuf> =
            graph.cache().map(|cache| cache.exclusions(workspace_root)).unwrap_or_default();
        // Every read below is a baseline, so it waits for the watch first: from then on the
        // stream covers everything after it. A hub that cannot watch polls instead, and its
        // consumers reconcile once — either way the consumer below runs.
        if let Some((hub, policy)) = watch {
            Self::await_watch(hub, stop, policy);
        }
        // The wait above ends on a stop as readily as on an armed watch, and what follows is
        // the whole boot: opening the store, walking the tree, indexing it. A daemon that has
        // been told to go does none of that — the caller reads `None` as "nothing was
        // published", which is exactly what happened.
        if stop.is_stopped() {
            return Ok(None);
        }
        // The daemon only reaches this after `workspace()` validated the project;
        // a config broken by a mid-session edit keeps search down, loudly.
        let project = match crate::project::at(workspace_root) {
            Ok(project) => project,
            Err(e) => {
                tracing::error!(error = %e, "invalid project; workspace search stays offline");
                return Err(bsl_search::SearchError::Index(format!("invalid project: {e}")));
            }
        };
        if !graph.validate_workspace_scope() {
            return Err(bsl_search::SearchError::Index(
                "workspace cache scope changed before search preparation".to_owned(),
            ));
        }
        if lease.coordination_failed() {
            tracing::warn!("workspace cache lease unavailable; workspace search stays offline");
            return Err(bsl_search::SearchError::Index(
                "workspace cache lease unavailable".to_owned(),
            ));
        }
        let cache = if let Some(cache) = graph.cache().cloned() {
            cache
        } else {
            crate::cache::WorkspaceCacheLayout::for_project_in_current_dir(
                &project,
                None,
                crate::cache::expected_scope_from_env()
                    .map_err(|error| bsl_search::SearchError::Index(error.to_string()))?
                    .as_deref(),
            )
            .map_err(|error| bsl_search::SearchError::Index(error.to_string()))?
        };
        cache
            .verify_project(&project)
            .map_err(|error| bsl_search::SearchError::Index(error.to_string()))?;
        if !graph.validate_workspace_scope() {
            return Err(bsl_search::SearchError::Index(
                "workspace cache scope changed before search store preparation".to_owned(),
            ));
        }
        cache.ensure().map_err(|error| bsl_search::SearchError::Index(error.to_string()))?;
        let db_path = cache.search_db_path();
        let source_path = project.source_path().to_path_buf();

        // Branch by the configured MODE, never by baseline presence: in Postgres mode a
        // missing service (connect failed / still pending past the wait) must leave the
        // search offline with a visible issue. Falling through to the local branch here
        // would silently start a full local reindex of the whole configuration — the
        // exact cost Postgres mode exists to avoid.
        if matches!(mode, WorkspaceSearchMode::PostgresRemoteOverlay) {
            let Some(external_baseline) = external_baseline
                .as_ref()
                .filter(|baseline| matches!(baseline.corpus(), CorpusId::WorkspaceCode))
            else {
                tracing::warn!(
                    "Postgres workspace mode is configured but the shared baseline is \
                     unavailable; workspace search stays offline (no local fallback)"
                );
                return Err(bsl_search::SearchError::ExternalBaseline(
                    "workspace baseline is unavailable".to_owned(),
                ));
            };
            let roots = Self::roots_of(&project, &excluded);
            let Some(opened) = bsl_search::lifecycle::with_startup_roots(
                "postgres_remote_overlay",
                roots.entries().map(|(_, path)| path.to_path_buf()).collect(),
                || {
                    Self::open_workspace_overlay_search_engine_fenced(
                        &db_path,
                        lease,
                        stop,
                        embedding_prefixes,
                    )
                },
            )?
            else {
                return Ok(None);
            };
            let mut engine = opened.engine;
            let semantic_failure = opened.semantic_failure;
            let Some(()) = Self::configure_and_declare_baseline(
                &mut engine,
                roots,
                BaselineHashMode::NormalizedChunks,
                true,
                lease,
                stop,
            )?
            else {
                return Ok(None);
            };

            let store = engine.store();
            let Some(()) = Self::startup_apply_checkpointed_value(lease, stop, |checkpoint| {
                match store.clear_workspace_overlay_checkpointed("code", checkpoint) {
                    Ok(std::ops::ControlFlow::Continue(())) => {
                        std::ops::ControlFlow::Continue(Ok(()))
                    }
                    Ok(std::ops::ControlFlow::Break(())) => std::ops::ControlFlow::Break(()),
                    Err(error) => std::ops::ControlFlow::Continue(Err(error)),
                }
            })?
            else {
                return Ok(None);
            };

            // The persisted manifest is deliberately NOT cleared up front: it is
            // immutable for a given snapshot, so it doubles as a warm-boot disk cache.
            // Once the cheap snapshot resolution below confirms the baseline still
            // points at the snapshot the manifest was fetched for, the expensive
            // per-file manifest download from Postgres is skipped entirely. Every
            // failure path clears it instead, so a failed init stays fail-closed.
            let manifest_files = match external_baseline
                .resolve_snapshot(&crate::baseline::uncancellable())
                .map_err(crate::baseline::BaselineCall::into_error)
            {
                Ok(Some((_baseline_ref, snapshot))) => {
                    let cached = store.load_coherent_baseline_manifest().unwrap_or_else(|error| {
                        tracing::debug!(
                            "failed to read the persisted workspace baseline manifest: {error}"
                        );
                        None
                    });
                    let cached = cached.filter(|record| {
                        Self::baseline_manifest_matches_snapshot(record, &snapshot)
                    });
                    match cached {
                        Some(record) => {
                            tracing::info!(
                                snapshot_id = %snapshot.id.0,
                                manifest_files = record.manifest_files,
                                "workspace baseline manifest served from disk cache"
                            );
                            record.manifest_files
                        }
                        None => match external_baseline
                            .load_baseline_manifest(
                                &crate::baseline::uncancellable(),
                                &snapshot.id.0,
                            )
                            .map_err(crate::baseline::BaselineCall::into_error)
                        {
                            Ok(manifest) => {
                                let manifest_files = manifest.files.len();
                                match Self::startup_apply_checkpointed_value(
                                    lease,
                                    stop,
                                    |checkpoint| match store
                                        .save_baseline_manifest_checkpointed(&manifest, checkpoint)
                                    {
                                        Ok(std::ops::ControlFlow::Continue(())) => {
                                            std::ops::ControlFlow::Continue(Ok(()))
                                        }
                                        Ok(std::ops::ControlFlow::Break(())) => {
                                            std::ops::ControlFlow::Break(())
                                        }
                                        Err(error) => std::ops::ControlFlow::Continue(Err(error)),
                                    },
                                ) {
                                    Ok(Some(())) => {}
                                    Ok(None) => return Ok(None),
                                    Err(error) => {
                                        tracing::warn!(
                                            "failed to persist workspace baseline manifest: {error}"
                                        );
                                        Self::clear_baseline_manifest_best_effort(
                                            store, lease, stop,
                                        );
                                        return Err(error);
                                    }
                                }
                                tracing::info!(
                                    snapshot_id = %snapshot.id.0,
                                    manifest_files,
                                    "workspace baseline manifest loaded and persisted"
                                );
                                manifest_files
                            }
                            Err(error) => {
                                tracing::warn!(
                                    "failed to load workspace baseline manifest: {error}"
                                );
                                Self::clear_baseline_manifest_best_effort(store, lease, stop);
                                return Err(error);
                            }
                        },
                    }
                }
                Ok(None) => {
                    tracing::warn!(
                        "workspace baseline manifest unavailable for configured Postgres mode"
                    );
                    Self::clear_baseline_manifest_best_effort(store, lease, stop);
                    return Err(bsl_search::SearchError::ExternalBaseline(
                        "workspace baseline manifest is unavailable".to_owned(),
                    ));
                }
                Err(error) => {
                    tracing::warn!("failed to resolve workspace baseline snapshot: {error}");
                    Self::clear_baseline_manifest_best_effort(store, lease, stop);
                    return Err(error);
                }
            };

            tracing::info!(
                manifest_files,
                "workspace overlay-only baseline initialized; baseline search served from Postgres"
            );

            if !graph.validate_workspace_scope() {
                return Err(bsl_search::SearchError::Index(
                    "workspace cache scope changed before search publication".to_owned(),
                ));
            }
            return Ok(Some(WorkspaceSearchInit {
                engine,
                mode: WorkspaceSearchMode::PostgresRemoteOverlay,
                semantic_failure,
                pending_embed: None,
                overlay_init: OverlayInit::RemoteWarmup,
            }));
        }

        let roots = Self::roots_of(&project, &excluded);
        let Some(opened) = bsl_search::lifecycle::with_startup_roots(
            "sqlite_local",
            roots.entries().map(|(_, path)| path.to_path_buf()).collect(),
            || Self::open_search_engine_fenced(&db_path, lease, stop, embedding_prefixes),
        )?
        else {
            return Ok(None);
        };
        let mut engine = opened.engine;
        let semantic_failure = opened.semantic_failure;

        // A restart with partially embedded code must resume, not re-embed. The deferred
        // embedding pass already selects exactly the NULL-embedding chunks
        // (`load_pending_embedding_documents`), so an interrupted run picks up where it
        // left off regardless of file hashes. Clearing the hashes here would instead force
        // `index_directory_deferred` to DELETE+reinsert those files' chunks with NULL
        // embeddings, throwing away vectors already paid for — the opposite of resume.
        // Changed files are still detected and re-embedded via their content-hash mismatch.

        // Declaring the local mode also clears inherited fingerprint rows: they claim
        // "verified against the manifest", which this mode can neither honour nor refresh —
        // a row surviving the local period would suppress a same-stat edit after a switch
        // back to the same snapshot. A failed clear leaves that lie standing, so the boot
        // fails closed, exactly like the Postgres branch does on its own failed clears.
        let Some(()) = Self::configure_and_declare_baseline(
            &mut engine,
            roots,
            BaselineHashMode::RawFileBytes,
            false,
            lease,
            stop,
        )?
        else {
            return Ok(None);
        };

        // Fused cold-build: the graph owns the startup build decision. When it builds
        // the graph fresh it streams the search chunks (with graph context) from the
        // same parse pass, so this run only has to fill embeddings — no second parse,
        // no graph round-trip. On a warm cache, a missing embedder, or any failure it
        // returns `Standalone` and we fall through to the standalone indexer below.
        if let crate::graph::FusedStartup::Fused =
            graph.start_workspace_graph(&mut engine, &source_path)
        {
            // FTS chunks and graph context are written; embeddings are still NULL. Hand
            // the engine back immediately so lexical search and the graph go live in
            // minutes, and defer the ~hours-long embedding pass to a background thread
            // on its own connection (see `spawn_workspace_search_init`).
            let pending_embed = Self::embedding_config_with_prefixes(Some(embedding_prefixes))?
                .map(|config| PendingEmbed { db_path: db_path.clone(), config });
            // The fused parse pass ingested files present on disk but never removed rows for a `.bsl`
            // deleted while the daemon was down. Reconcile the store to disk so the overlay baseline
            // truly == working tree before asserting Clean; a walk that could not prove this
            // downgrades to a prime (which never asserts a false clean).
            let Some(reconciled) =
                Self::reconcile_boot_store_with_disk_fenced(&mut engine, lease, stop)
            else {
                return Ok(None);
            };
            let overlay_init = if reconciled { OverlayInit::Clean } else { OverlayInit::Prime };
            if !graph.validate_workspace_scope() {
                return Err(bsl_search::SearchError::Index(
                    "workspace cache scope changed before search publication".to_owned(),
                ));
            }
            return Ok(Some(WorkspaceSearchInit {
                engine,
                mode: WorkspaceSearchMode::SqliteLocal,
                semantic_failure,
                pending_embed,
                overlay_init,
            }));
        }

        // Standalone path (warm cache, no embedder, or fused fallback). Enrich semantic
        // embeddings with each method's call-graph context when the graph database is
        // already built; if absent (still building) the embeddings are graph-free this
        // run and pick up context on a later reindex.
        if engine.has_semantic() {
            // Load the project snapshot once and keep its roots paired with the graph
            // validation below. Loading topology and roots separately leaves a window in
            // which a config/root move can make the provider read a different generation
            // from the one that passed the check.
            let graph_project =
                crate::graph::ProjectSnapshot::load_excluding(workspace_root, &excluded);
            let current =
                graph.store().read(None, crate::graph::BACKGROUND_READ_WAIT, |snapshot| {
                    crate::graph::scan::graph_matches_live_project_strict(
                        &snapshot.graph,
                        &graph_project,
                    )
                    .then(|| snapshot.generation())
                });
            match current {
                Ok(None) => {
                    tracing::warn!(
                        "graph database is not current for the live project; \
                         embeddings without graph context"
                    );
                }
                Ok(Some(generation)) => {
                    engine.set_graph_context_provider(Arc::new(
                        crate::graph_query::GraphDbContextProvider::new(
                            graph.store().clone(),
                            generation,
                            graph_project.search_roots.as_ref(),
                            Some(graph.owed_context_marks()),
                        ),
                    ));
                    tracing::info!("graph-enriched embeddings enabled");
                }
                Err(e) => {
                    tracing::debug!(
                        "graph database unavailable; embeddings without graph context: {e}"
                    );
                }
            }
        }

        if engine.has_semantic() {
            // Same publish-early contract as the fused path, for the rare standalone
            // semantic cold start (fused build failed but an embedder is configured):
            // write FTS + chunks + graph context synchronously but WITHOUT embeddings (no
            // HTTP) so the engine publishes within minutes, then defer the hours-long
            // embedding to the background pass instead of blocking publication on a
            // synchronous `index_directory`. The graph context set above is persisted with
            // the chunks, so the deferred vectors are graph-enriched just as
            // `index_directory` would have produced.
            match engine.index_directory_deferred_fenced(&source_path, |apply| {
                Self::startup_apply_checkpointed(lease, stop, apply)
            }) {
                Ok(bsl_search::FenceOutcome::Applied(indexed)) => {
                    if indexed > 0 {
                        tracing::info!(indexed, "FTS + graph context written; embedding deferred");
                    }
                }
                Ok(bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released) => {
                    return Ok(None);
                }
                Ok(bsl_search::FenceOutcome::TransientRefusal) => {
                    unreachable!("startup_apply retries transient refusals")
                }
                Err(error) => return Err(error),
            }

            // Schedule the background pass only when chunks actually lack vectors. A warm
            // restart has none pending, so it stays `Ready` with no transient downgrade.
            let chunks_result = engine.chunk_count();
            let embeddings_result = engine.embedding_count_by_collection("code");
            engine.observe_semantic_boot_coverage(
                chunks_result.as_ref().ok().copied(),
                embeddings_result.as_ref().ok().copied(),
            );
            let code_chunks = chunks_result.unwrap_or(0);
            let code_embeddings = embeddings_result.unwrap_or(0);
            let pending_embed = (code_chunks > code_embeddings)
                .then(|| Self::embedding_config_with_prefixes(Some(embedding_prefixes)))
                .transpose()?
                .flatten()
                .map(|config| PendingEmbed { db_path: db_path.clone(), config });

            // `index_directory_deferred` above re-ingested every file whose content hash changed
            // (incl. edits made while the daemon was down) but did not remove rows for a `.bsl`
            // deleted while down. Reconcile the store to disk so the overlay baseline == working tree
            // before asserting Clean; a walk that could not prove this downgrades to a prime.
            let Some(reconciled) =
                Self::reconcile_boot_store_with_disk_fenced(&mut engine, lease, stop)
            else {
                return Ok(None);
            };
            let overlay_init = if reconciled { OverlayInit::Clean } else { OverlayInit::Prime };
            if !graph.validate_workspace_scope() {
                return Err(bsl_search::SearchError::Index(
                    "workspace cache scope changed before search publication".to_owned(),
                ));
            }
            return Ok(Some(WorkspaceSearchInit {
                engine,
                mode: WorkspaceSearchMode::SqliteLocal,
                semantic_failure,
                pending_embed,
                overlay_init,
            }));
        }

        // FTS-only branch (no embedder configured — the common local dev setup). A cold store (no
        // chunks yet) gets a full walk+hash ingest; a warm store with existing chunks skips
        // re-indexing entirely, so it is NOT reconciled against files EDITED while the daemon was
        // down and must prime for those. Either way the index step never removes rows for files
        // DELETED while down, so both sub-branches reconcile the store to disk here (removing gone
        // rows); only the cold, freshly-ingested-and-reconciled sub-branch may then assert Clean.
        let overlay_init = if engine.chunk_count().unwrap_or(0) == 0 {
            tracing::info!(?source_path, "building FTS index from source files");
            match engine.index_directory_fts_fenced(&source_path, |apply| {
                Self::startup_apply_checkpointed(lease, stop, apply)
            }) {
                Ok(bsl_search::FenceOutcome::Applied(indexed)) => {
                    tracing::info!(indexed, "FTS index built")
                }
                Ok(bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released) => {
                    return Ok(None);
                }
                Ok(bsl_search::FenceOutcome::TransientRefusal) => {
                    unreachable!("startup_apply retries transient refusals")
                }
                Err(error) => return Err(error),
            }
            let Some(reconciled) =
                Self::reconcile_boot_store_with_disk_fenced(&mut engine, lease, stop)
            else {
                return Ok(None);
            };
            if reconciled {
                OverlayInit::Clean
            } else {
                OverlayInit::Prime
            }
        } else {
            // Warm store: prime handles the while-down EDITS; the reconcile still removes rows for
            // files DELETED while down (a prime only hides them lazily and never from the store).
            // A root DECLARED while down is neither: it has no rows to refresh and no rows to
            // remove, so the skip is taken per root and only the unindexed ones are ingested.
            match engine.index_unindexed_roots_fts_fenced(|apply| {
                Self::startup_apply_checkpointed(lease, stop, apply)
            }) {
                Ok(bsl_search::FenceOutcome::Applied(indexed)) if indexed > 0 => {
                    tracing::info!(
                        indexed,
                        "indexed a source root declared while the daemon was down"
                    )
                }
                Ok(bsl_search::FenceOutcome::Applied(_)) => {}
                Ok(bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released) => {
                    return Ok(None);
                }
                Ok(bsl_search::FenceOutcome::TransientRefusal) => {
                    unreachable!("startup_apply retries transient refusals")
                }
                Err(error) => return Err(error),
            }
            if Self::reconcile_boot_store_with_disk_fenced(&mut engine, lease, stop).is_none() {
                return Ok(None);
            }
            OverlayInit::Prime
        };

        if !graph.validate_workspace_scope() {
            return Err(bsl_search::SearchError::Index(
                "workspace cache scope changed before search publication".to_owned(),
            ));
        }
        Ok(Some(WorkspaceSearchInit {
            engine,
            mode: WorkspaceSearchMode::SqliteLocal,
            semantic_failure,
            pending_embed: None,
            overlay_init,
        }))
    }

    #[cfg(test)]
    pub(super) fn init_workspace_search_engine_unmanaged(
        workspace_root: &std::path::Path,
        watch: Option<(&WorkspaceChangeHub, super::sync::WatchWaitPolicy)>,
        mode: WorkspaceSearchMode,
        external_baseline: Option<Arc<ExternalBaselineService>>,
        graph: &GraphState,
    ) -> Option<WorkspaceSearchInit> {
        Self::init_workspace_search_engine(
            workspace_root,
            watch,
            mode,
            external_baseline,
            graph,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &super::OwnerStop::default(),
            &super::types::EmbeddingPrefixes::default(),
        )
        .ok()
        .flatten()
    }

    fn init_reference_search_engine(
        progress: &Arc<IndexProgress>,
        external_baseline: Option<Arc<ExternalBaselineService>>,
        embedding_prefixes: &super::types::EmbeddingPrefixes,
    ) -> Result<(SearchEngine, Option<EmbeddingFailure>), ReferenceInitError> {
        let db_path = Self::reference_search_db_path().ok_or_else(|| {
            ("reference cache path is unavailable".to_owned(), "storage_error".to_owned(), None)
        })?;
        Self::init_reference_search_engine_at(
            &db_path,
            progress,
            external_baseline,
            Some(embedding_prefixes),
        )
    }

    fn init_reference_search_engine_at(
        db_path: &Path,
        progress: &Arc<IndexProgress>,
        external_baseline: Option<Arc<ExternalBaselineService>>,
        prefixes: Option<&super::types::EmbeddingPrefixes>,
    ) -> Result<(SearchEngine, Option<EmbeddingFailure>), ReferenceInitError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| (error.to_string(), "storage_error".to_owned(), None))?;
        }

        let mut engine = Self::open_search_engine_with_prefixes(db_path, prefixes)
            .map_err(search_failure)?
            .ok_or_else(|| {
                (
                    "failed to open reference search engine".to_owned(),
                    "storage_error".to_owned(),
                    None,
                )
            })?;
        let requested_token_failure = prefixes.and_then(token_profile_failure);
        let embedding_failure = if external_baseline
            .as_ref()
            .is_some_and(|baseline| matches!(baseline.corpus(), CorpusId::Reference))
        {
            if let Some(external_baseline) = external_baseline.as_ref() {
                let model_id = engine.embedding_storage_identity().map(ToOwned::to_owned);
                let dimension = engine.embedding_dimension();
                match external_baseline
                    .load_reference_snapshot_documents(
                        &crate::baseline::uncancellable(),
                        model_id.as_deref(),
                        dimension,
                    )
                    .map_err(crate::baseline::BaselineCall::into_error)
                {
                    Ok(Some(snapshot)) => {
                        if engine.has_semantic() {
                            let cleared = engine
                                .clear_file_hashes_without_embeddings("platform")
                                .unwrap_or(0);
                            if cleared > 0 {
                                tracing::info!(
                                    cleared,
                                    "cleared hashes for reference cache files without embeddings"
                                );
                            }
                        }
                        engine
                            .remove_file("platform://docs", "platform")
                            .map_err(search_failure)?;
                        Self::index_external_reference_docs(&mut engine, progress, snapshot)
                            .map_err(search_failure)?;
                    }
                    Ok(None) => {
                        return Err((
                            "external reference baseline has no resolved snapshot".to_owned(),
                            "baseline_unavailable".to_owned(),
                            None,
                        ));
                    }
                    Err(error) => {
                        return Err(search_failure(error));
                    }
                }
            }
            tracing::info!(
                "external reference baseline is configured; lexical search uses the shared snapshot and semantic cache is synchronized locally"
            );
            requested_token_failure
        } else {
            Self::index_platform_docs(&mut engine, progress)
                .map_err(search_failure)?
                .or(requested_token_failure)
        };
        Ok((engine, embedding_failure))
    }

    fn index_external_reference_docs(
        engine: &mut SearchEngine,
        progress: &Arc<IndexProgress>,
        snapshot: crate::baseline::BaselineSnapshotDocuments,
    ) -> Result<(), SearchError> {
        let version = snapshot.fingerprint.unwrap_or(snapshot.snapshot_id);

        tracing::info!(
            snapshot = %version,
            documents = snapshot.documents.len(),
            shared_embeddings = snapshot.shared_embeddings.len(),
            "synchronizing external reference snapshot into local semantic cache"
        );

        let indexed_files = engine.sync_indexed_documents_in_collection_with_embeddings(
            "platform",
            &snapshot.documents,
            Some(&snapshot.shared_embeddings),
            Some(progress),
        )?;
        if indexed_files > 0 {
            tracing::info!(indexed_files, "external reference docs cached locally");
        } else {
            tracing::info!("external reference docs cache is up to date");
        }
        Ok(())
    }

    #[cfg(test)]
    fn clear_reference_docs_cache(engine: &mut SearchEngine) {
        match engine.sync_indexed_documents_in_collection(
            "platform",
            &[] as &[bsl_search::IndexedDocument],
            None,
        ) {
            Ok(removed_files) => {
                if removed_files > 0 {
                    tracing::info!(removed_files, "cleared stale reference docs cache files");
                }
            }
            Err(error) => {
                tracing::warn!("failed to clear stale reference docs cache: {error}");
            }
        }
    }

    fn index_platform_docs(
        engine: &mut SearchEngine,
        progress: &Arc<IndexProgress>,
    ) -> Result<Option<EmbeddingFailure>, SearchError> {
        Self::index_platform_docs_from(engine, progress, PlatformDataInner::instance())
    }

    /// Keeps the local `platform://docs` document in step with `platform`: the
    /// served corpus replaces it, and no corpus removes it.
    fn index_platform_docs_from(
        engine: &mut SearchEngine,
        progress: &Arc<IndexProgress>,
        platform: &PlatformDataInner,
    ) -> Result<Option<EmbeddingFailure>, SearchError> {
        if platform.help_origin().is_none() {
            // Only the local help document goes: the same collection may hold an
            // external reference snapshot, which stays.
            if engine.remove_file_if_present("platform://docs", "platform")? {
                tracing::info!(
                    reason = platform.help_missing_reason().unwrap_or("empty corpus"),
                    "no platform help; local platform docs removed from the search index"
                );
            }
            return Ok(None);
        }

        let documents = crate::tools::platform::build_reference_documents_from(platform);

        let fingerprint = crate::reference_documents_fingerprint(&documents);

        tracing::info!(
            types = platform.all_types().len(),
            methods = platform.all_methods().len(),
            global_functions = platform.all_global_functions().len(),
            total_documents = documents.len(),
            "indexing platform reference documentation"
        );

        let outcome = engine.replace_reference_collection_if_stale(
            "platform",
            "platform://docs",
            &fingerprint,
            &documents,
            Some(progress),
        )?;
        if outcome.written {
            tracing::info!(count = documents.len(), "platform docs indexed");
        } else {
            tracing::info!("platform docs unchanged, skipped");
        }
        Ok(outcome.embedding_failure)
    }

    fn reference_search_db_path() -> Option<PathBuf> {
        if let Some(base) = dirs::cache_dir() {
            return Some(base.join("bsl-analyzer").join("reference-search.db"));
        }

        if let Some(home) = env::var_os("HOME") {
            return Some(PathBuf::from(home).join(".cache/bsl-analyzer/reference-search.db"));
        }

        if let Some(profile) = env::var_os("USERPROFILE") {
            return Some(PathBuf::from(profile).join(".bsl-analyzer/reference-search.db"));
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{env_lock, write_common_module_tree, EnvVarGuard};
    use super::{
        DiagnosticsState, EmbedFlight, GraphState, OverlayInit, SemanticRuntimeStatus, SharedState,
        WorkspaceSearchMode, DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        EMBEDDING_PUBLISH_RETRY_BUDGET_ENV, EMBEDDING_PUBLISH_RETRY_BUDGET_WARNINGS,
    };
    use crate::baseline::{
        BaselineBootstrap, BaselineRuntime, ConfiguredBaselineStatus, DeferredBaselineRuntime,
        ExternalBaselineService, RefreshableExternalBaselineSource,
    };
    use crate::change_hub::WorkspaceChangeHub;
    use bsl_search::{
        BaselineRef, CorpusId, Document, ExternalBaselineConfig, IndexProgress, IndexedDocument,
        SearchEngine,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    /// A transient refusal means ANOTHER process holds the lease lock. Waiting that out is
    /// right; waiting it out under the engine mutex is not — every graph publication, every
    /// mark consumption and every `search_code` queues behind a pause whose length is set by a
    /// foreign holder, not by this daemon.
    ///
    /// The order the comment at the call site asks for — engine first, then the lease — is
    /// about one ATTEMPT. The pause between attempts belongs to nobody.
    #[test]
    fn the_engine_is_free_while_a_publication_waits_out_a_foreign_lease_hold() {
        use crate::state::OwnerStop;
        let shared = Arc::new(crate::state::shared_engine(None));
        let owners = OwnerStop::default();
        let free_while_paused = Arc::new(AtomicUsize::new(0));
        let held_while_paused = Arc::new(AtomicUsize::new(0));

        let attempts = AtomicUsize::new(0);
        let probe_engine = Arc::clone(&shared);
        let free = Arc::clone(&free_while_paused);
        let held = Arc::clone(&held_while_paused);
        let published = SharedState::publish_engine_with_retry(
            &shared,
            &owners,
            |_slot| {
                // Two foreign holds, then the lock frees and the publication lands.
                if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                    bsl_search::FenceOutcome::TransientRefusal
                } else {
                    bsl_search::FenceOutcome::Applied(Ok(()))
                }
            },
            std::time::Instant::now,
            |_delay| {
                // What a co-tenant sees during the pause. A request path cannot wait for a
                // background owner, so "free" here has to mean free right now.
                match crate::tools::search::try_acquire_engine(
                    &probe_engine,
                    &tokio_util::sync::CancellationToken::new(),
                ) {
                    Ok(_) => free.fetch_add(1, Ordering::SeqCst),
                    Err(_) => held.fetch_add(1, Ordering::SeqCst),
                };
                false
            },
        );

        assert!(matches!(published, Ok(Some(()))), "the publication should land: {published:?}");
        assert_eq!(
            held_while_paused.load(Ordering::SeqCst),
            0,
            "the engine was held through {} of the pauses between lease attempts",
            held_while_paused.load(Ordering::SeqCst),
        );
        assert_eq!(
            free_while_paused.load(Ordering::SeqCst),
            2,
            "both pauses should have left the engine acquirable",
        );
    }

    #[test]
    fn payload_lifecycle_reference_lexical_publication_keeps_failure_and_recovers() {
        use super::super::test_support::{mock_semantic_config, spawn_mock_embedding_server};
        let _lock = env_lock();
        let server = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference.db");
        let documents = [Document {
            title: "Массив".to_owned(),
            body: "payloadrefmarker".to_owned(),
            kind: "type".to_owned(),
        }];
        let state = super::ReferenceSearchState::new(None);
        let mut limited = mock_semantic_config(&server);
        limited.embedder.max_request_bytes = 1;
        let mut engine = SearchEngine::new(&db_path, limited).unwrap();
        let outcome = engine
            .replace_reference_collection_if_stale(
                "platform",
                "platform://docs",
                "payload-reference",
                &documents,
                Some(&state.progress),
            )
            .unwrap();
        let failure = outcome.embedding_failure.expect("real local embedding refusal");
        assert_eq!(failure.code, bsl_search::EmbeddingFailureCode::EmbeddingInputTooLarge);
        state.finish_initialization(Ok((engine, outcome.embedding_failure)));
        assert_eq!(state.lifecycle(), super::ReferenceSearchLifecycle::Ready);
        assert_eq!(state.semantic_runtime.lock().unwrap().embedding_failure(), Some(failure));
        {
            let guard = state.engine.lock().unwrap();
            let engine = guard.as_ref().unwrap();
            assert_eq!(engine.vector_count(), 0);
            assert_eq!(
                engine.text_search("payloadrefmarker", 10, Some("platform")).unwrap().len(),
                1
            );
        }

        let mut recovered = SearchEngine::new(&db_path, mock_semantic_config(&server)).unwrap();
        let outcome = recovered
            .replace_reference_collection_if_stale(
                "platform",
                "platform://docs",
                "payload-reference",
                &documents,
                Some(&state.progress),
            )
            .unwrap();
        assert!(outcome.written, "the lexical stamp must request a semantic retry");
        assert!(outcome.embedding_failure.is_none());
        state.finish_initialization(Ok((recovered, outcome.embedding_failure)));
        assert_eq!(state.lifecycle(), super::ReferenceSearchLifecycle::Ready);
        assert_eq!(*state.semantic_runtime.lock().unwrap(), SemanticRuntimeStatus::Ready);
        assert_eq!(state.engine.lock().unwrap().as_ref().unwrap().vector_count(), 1);
        state.shutdown();
    }

    #[test]
    fn payload_lifecycle_reference_config_error_reaches_the_runtime_owner() {
        let _lock = env_lock();
        let _enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        let _model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");
        let _limit = EnvVarGuard::set("EMBEDDING_MAX_REQUEST_BYTES", "private-invalid-value");
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference.db");
        let state = super::ReferenceSearchState::new(None);
        let initialization =
            SharedState::init_reference_search_engine_at(&db_path, &state.progress, None, None);
        assert!(!db_path.exists(), "invalid configuration must fail before storage opens");
        state.finish_initialization(initialization);
        assert!(
            matches!(state.lifecycle(), super::ReferenceSearchLifecycle::Failed { ref message, .. }
            if message == "embedding_invalid_config")
        );
        assert_eq!(
            state.semantic_runtime.lock().unwrap().embedding_failure().unwrap().code,
            bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig,
        );
        assert!(state.engine.lock().unwrap().is_none());
        state.shutdown();
    }

    /// The declared width is optional: unset means the request carries no `dimensions`
    /// at all (the model answers in its native width). An explicit one still wins, and
    /// a set-but-unusable value must not silently become "unset": now that the field
    /// is optional, a typo would otherwise choose a different width without a word.
    #[test]
    fn embedding_config_declares_a_width_only_when_asked() {
        let _lock = env_lock();
        let _enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        let _model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");
        let _limit = EnvVarGuard::unset("EMBEDDING_MAX_REQUEST_BYTES");

        let _dim = EnvVarGuard::unset("EMBEDDING_DIM");
        assert_eq!(SharedState::embedding_config().unwrap().unwrap().embedder.dim, None);

        let _dim = EnvVarGuard::set("EMBEDDING_DIM", "7");
        assert_eq!(SharedState::embedding_config().unwrap().unwrap().embedder.dim, Some(7));

        for unusable in ["", "seven", "0", " 7"] {
            let _dim = EnvVarGuard::set("EMBEDDING_DIM", unusable);
            let error = SharedState::embedding_config().err().expect("unusable width");
            assert_eq!(error.to_string(), "embedding_invalid_config");
        }
    }

    #[test]
    fn invalid_token_profile_reference_fallback_keeps_structured_semantic_failure() {
        let _lock = env_lock();
        let _max_bytes = EnvVarGuard::unset("EMBEDDING_MAX_REQUEST_BYTES");
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference.db");
        let prefixes = super::super::types::EmbeddingPrefixes {
            token_profile: Some(super::invalid_token_profile()),
            ..Default::default()
        };
        let state = super::ReferenceSearchState::new(None);
        let initialization = SharedState::init_reference_search_engine_at(
            &db_path,
            &state.progress,
            None,
            Some(&prefixes),
        );
        assert!(db_path.exists(), "invalid requested token policy keeps reference FTS available");
        state.finish_initialization(initialization);
        assert_eq!(state.lifecycle(), super::ReferenceSearchLifecycle::Ready);
        assert_eq!(
            state.semantic_runtime.lock().unwrap().embedding_failure().unwrap().code,
            bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig,
            "reference fallback must retain the token-profile failure instead of reporting Disabled"
        );
        assert!(state.engine.lock().unwrap().as_ref().is_some_and(|engine| !engine.has_semantic()));
        state.shutdown();
    }

    #[test]
    fn payload_configuration_rejects_invalid_enabled_settings_before_opening_storage() {
        let _lock = env_lock();
        let _enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        let _model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");
        let _batch = EnvVarGuard::set("EMBEDDING_BATCH_SIZE", "7");
        let _concurrency = EnvVarGuard::set("EMBEDDING_CONCURRENCY", "3");
        let _limit = EnvVarGuard::unset("EMBEDDING_MAX_REQUEST_BYTES");
        let config = SharedState::embedding_config().unwrap().unwrap();
        assert_eq!(config.embedder.max_request_bytes, 1_048_576);
        assert_eq!(config.execution.batch_size, 7);
        assert_eq!(config.execution.concurrency, 3);

        {
            let _limit = EnvVarGuard::set("EMBEDDING_MAX_REQUEST_BYTES", "4096");
            assert_eq!(
                SharedState::embedding_config().unwrap().unwrap().embedder.max_request_bytes,
                4096
            );
        }
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("unopened.db");
        let lease = crate::workspace_lease::WorkspaceLease::unmanaged();
        for invalid in
            ["0".to_owned(), "private-invalid-value".to_owned(), format!("{}0", usize::MAX)]
        {
            let _limit = EnvVarGuard::set("EMBEDDING_MAX_REQUEST_BYTES", &invalid);
            let error = SharedState::embedding_config().err().expect("invalid configuration");
            assert_eq!(error.to_string(), "embedding_invalid_config");
            assert_eq!(
                error.embedding_failure().unwrap().code,
                bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig
            );
            assert!(SharedState::open_search_engine_fenced(
                &db_path,
                &lease,
                &super::super::OwnerStop::default(),
                &super::super::types::EmbeddingPrefixes::default(),
            )
            .is_err());
            assert!(SharedState::open_workspace_overlay_search_engine_fenced(
                &db_path,
                &lease,
                &super::super::OwnerStop::default(),
                &super::super::types::EmbeddingPrefixes::default(),
            )
            .is_err());
            assert!(SharedState::open_search_engine(&db_path).is_err());
            assert!(!db_path.exists(), "invalid enabled configuration must fail before storage");

            let _model = EnvVarGuard::unset("EMBEDDING_MODEL");
            assert!(SharedState::embedding_config().unwrap().is_none());
        }
    }

    #[test]
    fn token_profile_bootstrap_loads_project_artifact_once_for_workspace_and_reference() {
        let _lock = env_lock();
        let _limit = EnvVarGuard::unset("EMBEDDING_MAX_INPUT_TOKENS");
        let _file = EnvVarGuard::unset("EMBEDDING_TOKENIZER_FILE");
        let _hash = EnvVarGuard::unset("EMBEDDING_TOKENIZER_SHA256");
        let _frozen_limit = EnvVarGuard::unset(crate::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV);
        let _frozen_file = EnvVarGuard::unset(crate::broker::EMBEDDING_TOKENIZER_FILE_ENV);
        let _frozen_hash = EnvVarGuard::unset(crate::broker::EMBEDDING_TOKENIZER_SHA256_ENV);
        let dir = tempdir().unwrap();
        let tokenizer = dir.path().join("tokenizer.json");
        fs::write(
            &tokenizer,
            r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,"model":{"type":"WordLevel","vocab":{"[UNK]":0,"document":1,"query":2},"unk_token":"[UNK]"}}"#,
        )
        .unwrap();
        let config_path = dir.path().join("bsl-analyzer.toml");
        fs::write(
            &config_path,
            "[search.baseline.embedding]\nmaxInputTokens = 8192\ntokenizerFile = \"tokenizer.json\"\ntokenizerSha256 = \"0140e7cf55bacdcb49a072e767dd1bc0114c690d6796d3729a0f2cb41cc27bc5\"\n",
        )
        .unwrap();
        let config = project_model::ProjectConfig::load(dir.path()).unwrap().unwrap();
        let profile = super::token_profile_for_bootstrap(Some(&config), Some(dir.path()))
            .expect("project profile is resolved without launcher prefixes");
        assert_eq!(profile.max_input_tokens, 8192);
        assert_eq!(profile.tokenizer_file, tokenizer.canonicalize().unwrap());
        assert!(profile.token_policy.is_some(), "the tokenizer is loaded once at bootstrap");
        assert!(profile.token_policy.as_ref().unwrap().count("hello").is_ok());

        let wrong_hash = config_path;
        fs::write(
            &wrong_hash,
            "[search.baseline.embedding]\nmaxInputTokens = 8192\ntokenizerFile = \"tokenizer.json\"\ntokenizerSha256 = \"wrong\"\n",
        )
        .unwrap();
        let invalid = project_model::ProjectConfig::load(dir.path()).unwrap().unwrap();
        let profile = super::token_profile_for_bootstrap(Some(&invalid), Some(dir.path()))
            .expect("invalid declared profile remains marked for lexical fallback");
        assert!(profile.token_policy.is_none());
        assert_eq!(profile.max_input_tokens, 0);
    }

    #[test]
    fn invalid_token_profile_keeps_existing_local_fts_search_available() {
        let _lock = env_lock();
        let _max_bytes = EnvVarGuard::unset("EMBEDDING_MAX_REQUEST_BYTES");
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        let mut store = bsl_search::Store::open(&db_path).unwrap();
        let chunks = [bsl_search::Chunk {
            kind: bsl_search::ChunkKind::Procedure,
            name: "ExistingProcedure".to_owned(),
            is_export: false,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 1,
            text: "procedure ExistingProcedure()\n    BoundedFallbackProbe = 1;\nendprocedure"
                .to_owned(),
        }];
        store
            .reindex_file_in_collection(
                "root",
                "Existing.bsl",
                b"fixture-hash",
                "code",
                &chunks,
                None,
                None,
            )
            .unwrap();
        drop(store);

        let prefixes = super::super::types::EmbeddingPrefixes {
            token_profile: Some(super::super::types::EmbeddingTokenProfile {
                max_input_tokens: 0,
                tokenizer_file: dir.path().join("missing-tokenizer.json"),
                tokenizer_sha256: String::new(),
                token_policy: None,
            }),
            ..Default::default()
        };
        let lease = crate::workspace_lease::WorkspaceLease::unmanaged();
        let opened = SharedState::open_search_engine_fenced(
            &db_path,
            &lease,
            &super::super::OwnerStop::default(),
            &prefixes,
        )
        .unwrap()
        .expect("invalid token profile falls back to local FTS");
        assert_eq!(
            opened.semantic_failure,
            Some(bsl_search::EmbeddingFailure::new(
                bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig
            ))
        );
        let engine = opened.engine;
        let hits = engine.text_search("BoundedFallbackProbe", 5, Some("code")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol_name, "ExistingProcedure");

        let runtime = Arc::new(Mutex::new(super::semantic_runtime_status_after_publish(
            &engine,
            &super::WorkspaceSearchMode::SqliteLocal,
            false,
            &prefixes,
            opened.semantic_failure,
        )));
        let status = crate::tools::search::search_status(
            crate::McpProfile::Workspace,
            &super::super::shared_engine(Some(engine)),
            &IndexProgress::new(),
            &runtime,
            super::WorkspaceSearchMode::SqliteLocal,
            super::OverlayWarmupState::Pending,
            None,
            None,
            false,
        )
        .unwrap();
        let body = status.structured_content.unwrap();
        assert_eq!(
            body["semantic_failure"],
            serde_json::json!(bsl_search::EmbeddingFailure::new(
                bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig
            ))
        );
        assert!(status.content[0].as_text().unwrap().text.contains("embedding_invalid_config"));

        let _invalid_max_bytes = EnvVarGuard::set("EMBEDDING_MAX_REQUEST_BYTES", "0");
        let unopened = dir.path().join("invalid-legacy-config.db");
        assert!(SharedState::open_search_engine_with_prefixes(&unopened, Some(&prefixes)).is_err());
        assert!(
            !unopened.exists(),
            "invalid legacy byte limit must still fail before storage opens"
        );
    }

    #[test]
    fn valid_token_profile_layout_refusal_keeps_fts_and_reports_semantic_failure() {
        let _lock = env_lock();
        let _enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        let _model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");
        let _max_bytes = EnvVarGuard::unset("EMBEDDING_MAX_REQUEST_BYTES");
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("claimed-search.db");
        let source = "Procedure ExistingProcedure()\n    LayoutFallbackProbe = 1;\nEndProcedure";
        let chunks = [bsl_search::Chunk {
            kind: bsl_search::ChunkKind::Procedure,
            name: "ExistingProcedure".to_owned(),
            is_export: false,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 3,
            text: source.to_owned(),
        }];
        let foreign_claim = "profile-v1:foreign-token-layout";
        {
            let mut store = bsl_search::Store::open(&db_path).unwrap();
            store
                .reindex_file_in_collection(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Existing.bsl",
                    blake3::hash(source.as_bytes()).as_bytes(),
                    "code",
                    &chunks,
                    None,
                    None,
                )
                .unwrap();
        }
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('token_layout_claim_v1', ?1)",
            [foreign_claim],
        )
        .unwrap();
        drop(conn);

        let tokenizer = dir.path().join("tokenizer.json");
        std::fs::write(
            &tokenizer,
            r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,"model":{"type":"WordLevel","vocab":{"[UNK]":0,"document":1,"query":2},"unk_token":"[UNK]"}}"#,
        )
        .unwrap();
        let tokenizer_sha256 = "0140e7cf55bacdcb49a072e767dd1bc0114c690d6796d3729a0f2cb41cc27bc5";
        let profile = super::super::types::EmbeddingPrefixes {
            token_profile: Some(super::super::types::EmbeddingTokenProfile {
                max_input_tokens: 128,
                tokenizer_file: tokenizer.clone(),
                tokenizer_sha256: tokenizer_sha256.to_owned(),
                token_policy: Some(
                    bsl_search::TokenPolicy::load(&tokenizer, tokenizer_sha256, 128).unwrap(),
                ),
            }),
            ..Default::default()
        };
        let opened = SharedState::open_search_engine_fenced(
            &db_path,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &super::super::OwnerStop::default(),
            &profile,
        )
        .unwrap()
        .expect("layout refusal leaves the lexical engine open");
        assert_eq!(
            opened.semantic_failure,
            Some(bsl_search::EmbeddingFailure::new(
                bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig
            ))
        );
        let hits = opened.engine.text_search("LayoutFallbackProbe", 5, Some("code")).unwrap();
        assert_eq!(hits.len(), 1);

        let runtime = Arc::new(Mutex::new(super::semantic_runtime_status_after_publish(
            &opened.engine,
            &super::WorkspaceSearchMode::SqliteLocal,
            false,
            &profile,
            opened.semantic_failure,
        )));
        let status = crate::tools::search::search_status(
            crate::McpProfile::Workspace,
            &super::super::shared_engine(Some(opened.engine)),
            &IndexProgress::new(),
            &runtime,
            super::WorkspaceSearchMode::SqliteLocal,
            super::OverlayWarmupState::Pending,
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            status.structured_content.unwrap()["semantic_failure"],
            serde_json::json!(bsl_search::EmbeddingFailure::new(
                bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig
            ))
        );
    }

    #[test]
    fn tokenizer_hash_mismatch_is_classified_without_disclosing_local_details() {
        let _lock = env_lock();
        let _enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        let _model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");
        let _limit = EnvVarGuard::unset("EMBEDDING_MAX_REQUEST_BYTES");
        let dir = tempdir().unwrap();
        let path = dir.path().join("sensitive-local-tokenizer.json");
        std::fs::write(&path, "not a tokenizer").unwrap();
        let profile = super::super::types::EmbeddingPrefixes {
            token_profile: Some(super::super::types::EmbeddingTokenProfile {
                max_input_tokens: 8192,
                tokenizer_file: path.clone(),
                tokenizer_sha256: "private-wrong-hash".to_owned(),
                token_policy: None,
            }),
            ..Default::default()
        };

        let error = match SharedState::embedding_config_with_prefixes(Some(&profile)) {
            Err(error) => error,
            Ok(_) => panic!("a tokenizer hash mismatch must reject the embedding profile"),
        };

        assert_eq!(error.to_string(), "embedding_invalid_config");
        assert_eq!(
            error.embedding_failure().unwrap().code,
            bsl_search::EmbeddingFailureCode::EmbeddingInvalidConfig
        );
        assert!(!error.to_string().contains(path.to_str().unwrap()));
        assert!(!error.to_string().contains("private-wrong-hash"));
    }

    #[test]
    fn embedding_publish_retry_budget_requires_positive_representable_seconds() {
        let _lock = env_lock();
        EMBEDDING_PUBLISH_RETRY_BUDGET_WARNINGS.store(0, Ordering::SeqCst);

        let _env = EnvVarGuard::unset(EMBEDDING_PUBLISH_RETRY_BUDGET_ENV);
        assert_eq!(
            SharedState::embedding_publish_retry_budget(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET
        );
        assert_eq!(EMBEDDING_PUBLISH_RETRY_BUDGET_WARNINGS.load(Ordering::SeqCst), 0);
        drop(_env);

        for invalid in ["0".to_owned(), "not-a-number".to_owned(), u64::MAX.to_string()] {
            let warnings_before = EMBEDDING_PUBLISH_RETRY_BUDGET_WARNINGS.load(Ordering::SeqCst);
            let _env = EnvVarGuard::set(EMBEDDING_PUBLISH_RETRY_BUDGET_ENV, &invalid);
            assert_eq!(
                SharedState::embedding_publish_retry_budget(),
                DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET
            );
            assert_eq!(
                EMBEDDING_PUBLISH_RETRY_BUDGET_WARNINGS.load(Ordering::SeqCst),
                warnings_before + 1,
                "one invalid host parse emits exactly one warning"
            );
        }

        let _env = EnvVarGuard::set(EMBEDDING_PUBLISH_RETRY_BUDGET_ENV, "42");
        assert_eq!(
            SharedState::embedding_publish_retry_budget(),
            std::time::Duration::from_secs(42)
        );
        assert_eq!(EMBEDDING_PUBLISH_RETRY_BUDGET_WARNINGS.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn startup_constructor_fence_distinguishes_retry_terminal_and_error() {
        let dir = tempdir().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let held = lease.hold_file_lock_for_test();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker = {
            let lease = lease.clone();
            let calls = Arc::clone(&calls);
            std::thread::spawn(move || {
                let mut apply = || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                };
                SharedState::startup_apply(&lease, &crate::state::OwnerStop::default(), &mut apply)
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "a refused fence does not invoke apply");
        drop(held);
        assert!(matches!(worker.join().unwrap(), bsl_search::FenceOutcome::Applied(Ok(()))));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the prepared operation runs once");

        let old = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let _newer = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let terminal_calls = AtomicUsize::new(0);
        let mut terminal_apply = || {
            terminal_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        assert!(matches!(
            SharedState::startup_apply(
                &old,
                &crate::state::OwnerStop::default(),
                &mut terminal_apply
            ),
            bsl_search::FenceOutcome::Superseded
        ));
        assert!(old.is_superseded());
        assert_eq!(terminal_calls.load(Ordering::SeqCst), 0);

        let released_dir = tempdir().unwrap();
        let released = crate::workspace_lease::WorkspaceLease::claim(released_dir.path());
        released.release();
        assert!(matches!(
            SharedState::startup_apply(&released, &crate::state::OwnerStop::default(), &mut || Ok(
                ()
            )),
            bsl_search::FenceOutcome::Released
        ));

        let error = SharedState::startup_apply(
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &crate::state::OwnerStop::default(),
            &mut || Err::<(), _>(bsl_search::SearchError::Index("expected".to_owned())),
        );
        assert!(matches!(
            error,
            bsl_search::FenceOutcome::Applied(Err(bsl_search::SearchError::Index(message)))
                if message == "expected"
        ));
    }

    /// A pause that ends because the daemon stopped is not a pause to try again after. The
    /// boot's own sleep returns at once from then on, so a loop that ignored the answer would
    /// not merely retry — it would spend the whole ten-minute budget in a tight spin, still
    /// writing into a workspace that is being handed over.
    #[test]
    fn a_startup_retry_leaves_when_the_daemon_stops() {
        let attempts = std::cell::Cell::new(0_u16);
        let outcome = SharedState::startup_retry(
            || {
                attempts.set(attempts.get() + 1);
                bsl_search::FenceOutcome::<Result<(), bsl_search::SearchError>>::TransientRefusal
            },
            std::time::Instant::now,
            // What `OwnerStop::sleep` answers once the stop is raised.
            |_| true,
        );

        assert!(
            matches!(outcome, bsl_search::FenceOutcome::Released),
            "a boot told to go must read as 'nothing was published', got {outcome:?}"
        );
        assert_eq!(attempts.get(), 1, "the boot tried again after it was told to leave");
    }

    #[test]
    fn startup_lease_retry_stops_at_600_seconds() {
        let started = std::time::Instant::now();
        let clock = std::cell::Cell::new(started);
        let attempts = std::cell::Cell::new(0_u16);
        let outcome = SharedState::startup_retry(
            || {
                attempts.set(attempts.get() + 1);
                bsl_search::FenceOutcome::<Result<(), bsl_search::SearchError>>::TransientRefusal
            },
            || clock.get(),
            |delay| {
                clock.set(clock.get().checked_add(delay).unwrap());
                false
            },
        );

        assert!(matches!(
            outcome,
            bsl_search::FenceOutcome::Applied(Err(bsl_search::SearchError::Index(message)))
                if message == "workspace lease startup retry budget exhausted"
        ));
        assert_eq!(clock.get().duration_since(started), std::time::Duration::from_secs(600));
        assert_eq!(attempts.get(), 301);
    }

    /// The configuration mutates process state; the declaration is a retryable transaction.
    /// A refused checkpoint must roll back the second and leave the first alone — otherwise
    /// ordinary contention reports `workspace roots are already initialized` and workspace
    /// search stays offline for the life of the daemon.
    #[test]
    fn a_refused_checkpoint_does_not_poison_the_configured_engine() {
        let dir = tempdir().unwrap();
        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(dir.path(), dir.path(), &[]);
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        lease.fail_next_checkpoint_lock_for_test();

        let declared = SharedState::configure_and_declare_baseline(
            &mut engine,
            roots,
            bsl_search::BaselineHashMode::RawFileBytes,
            false,
            &lease,
            &crate::state::OwnerStop::default(),
        )
        .expect("a refused checkpoint retries the transaction, not the engine configuration");

        assert_eq!(declared, Some(()));
    }

    /// The store open takes several checkpoints of its own (`finish_open_checkpointed`), and
    /// each one releases and re-takes the interprocess lock. A refusal there rolls the open
    /// back and the host runs it again — which only works if the adapter under it can be run
    /// twice. This drives the real production wiring, not a hand-built copy of it.
    #[test]
    fn checkpoint_refusal_retries_the_real_store_open() {
        let dir = tempdir().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        lease.fail_checkpoint_lock_after_for_test(1);

        let opened =
            bsl_search::SearchEngine::fts_only_fenced(&dir.path().join("search.db"), |apply| {
                SharedState::startup_apply_checkpointed(
                    &lease,
                    &crate::state::OwnerStop::default(),
                    apply,
                )
            })
            .expect("a refused checkpoint is retried, not turned into an initialization error");

        assert!(matches!(opened, bsl_search::FenceOutcome::Applied(_)));
    }

    #[test]
    fn checkpoint_refusal_retries_the_startup_transaction() {
        let dir = tempdir().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        lease.fail_next_checkpoint_lock_for_test();
        let mut attempts = 0_u8;

        let result = SharedState::startup_apply_checkpointed_value(
            &lease,
            &crate::state::OwnerStop::default(),
            |checkpoint| {
                attempts += 1;
                if checkpoint().is_break() {
                    return std::ops::ControlFlow::Break(());
                }
                std::ops::ControlFlow::Continue(Ok(attempts))
            },
        )
        .unwrap();

        assert_eq!(result, Some(2));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn startup_metadata_fence_stops_after_takeover() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        let engine = SearchEngine::fts_only(&db_path).unwrap();
        engine
            .store()
            .save_baseline_manifest(&bsl_search::WorkspaceBaselineManifest {
                snapshot_id: "prepared".to_owned(),
                snapshot_fingerprint: None,
                files: Vec::new(),
            })
            .unwrap();

        let old = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let prepared = engine.store().load_baseline_manifest().unwrap();
        assert!(prepared.is_some(), "metadata preparation happens before the fence");
        let _newer = crate::workspace_lease::WorkspaceLease::claim(dir.path());

        let result =
            SharedState::startup_apply_once(&old, &crate::state::OwnerStop::default(), || {
                engine.store().clear_baseline_manifest()
            })
            .unwrap();
        assert!(result.is_none());
        assert!(old.is_superseded());
        assert!(engine.store().load_baseline_manifest().unwrap().is_some());
    }

    #[test]
    fn startup_ingest_fence_does_not_repeat_preparation() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let source = workspace.join("CommonModule.bsl");
        fs::write(&source, "Процедура Первичная()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine
            .initialize_workspace_roots(
                bsl_search::WorkspaceRoots::build(&workspace, &workspace, &[]).0,
            )
            .unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        let held = lease.hold_file_lock_for_test();
        let (prepared_tx, prepared_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut announced = false;
            let result = engine.index_directory_fts_fenced(&workspace, |apply| {
                if !announced {
                    prepared_tx.send(()).unwrap();
                    announced = true;
                }
                SharedState::startup_apply_checkpointed(
                    &lease,
                    &crate::state::OwnerStop::default(),
                    apply,
                )
            });
            (result, engine)
        });

        prepared_rx.recv().unwrap();
        fs::write(&source, "Процедура Измененная()\nКонецПроцедуры").unwrap();
        drop(held);
        let (result, engine) = worker.join().unwrap();
        assert_eq!(result.unwrap(), bsl_search::FenceOutcome::Applied(1));
        assert_eq!(engine.text_search("Первичная", 10, Some("code")).unwrap().len(), 1);
        assert!(engine.text_search("Измененная", 10, Some("code")).unwrap().is_empty());

        let partial_dir = tempdir().unwrap();
        for index in 0..=bsl_search::WORKSPACE_APPLY_BATCH_ROWS {
            fs::write(
                partial_dir.path().join(format!("Module{index:02}.bsl")),
                format!("Процедура Метод{index}()\nКонецПроцедуры"),
            )
            .unwrap();
        }
        let mut partial = SearchEngine::fts_only(&partial_dir.path().join("search.db")).unwrap();
        partial
            .initialize_workspace_roots(
                bsl_search::WorkspaceRoots::build(partial_dir.path(), partial_dir.path(), &[]).0,
            )
            .unwrap();
        let old = crate::workspace_lease::WorkspaceLease::claim(partial_dir.path());
        let mut newer = None;
        let mut admitted = 0;
        let result = partial
            .index_directory_fts_fenced(partial_dir.path(), |apply| {
                if admitted == bsl_search::WORKSPACE_APPLY_BATCH_ROWS {
                    newer = Some(crate::workspace_lease::WorkspaceLease::claim(partial_dir.path()));
                }
                let result = SharedState::startup_apply_checkpointed(
                    &old,
                    &crate::state::OwnerStop::default(),
                    apply,
                );
                if matches!(result, bsl_search::FenceOutcome::Applied(Ok(()))) {
                    admitted += 1;
                }
                result
            })
            .unwrap();
        assert!(matches!(result, bsl_search::FenceOutcome::Superseded));
        assert_eq!(
            admitted,
            bsl_search::WORKSPACE_APPLY_BATCH_ROWS,
            "each independently visible file used its own completed fence"
        );
        assert_eq!(
            partial.file_count().unwrap(),
            bsl_search::WORKSPACE_APPLY_BATCH_ROWS,
            "all 64 completed file transactions survive takeover before file 65"
        );

        let present = std::collections::HashSet::new();
        assert!(matches!(
            partial
                .reconcile_workspace_files_fenced(&present, |apply| {
                    SharedState::startup_apply(&old, &crate::state::OwnerStop::default(), apply)
                })
                .unwrap(),
            bsl_search::FenceOutcome::Superseded
        ));
        assert_eq!(
            partial.file_count().unwrap(),
            bsl_search::WORKSPACE_APPLY_BATCH_ROWS,
            "terminal reconcile removes nothing"
        );
        drop(newer);
    }

    #[test]
    fn superseded_bootstrap_stops_mutating() {
        let prime_dir = tempdir().unwrap();
        fs::write(prime_dir.path().join("Prime.bsl"), "Процедура Подготовленная()\nКонецПроцедуры")
            .unwrap();
        let mut prime = SearchEngine::fts_only(&prime_dir.path().join("search.db")).unwrap();
        prime
            .initialize_workspace_roots(
                bsl_search::WorkspaceRoots::build(prime_dir.path(), prime_dir.path(), &[]).0,
            )
            .unwrap();
        let old = crate::workspace_lease::WorkspaceLease::claim(prime_dir.path());
        let mut newer = None;
        assert!(matches!(
            prime
                .prime_workspace_overlay_fenced(|apply| {
                    newer = Some(crate::workspace_lease::WorkspaceLease::claim(prime_dir.path()));
                    SharedState::startup_apply(&old, &crate::state::OwnerStop::default(), apply)
                })
                .unwrap(),
            bsl_search::FenceOutcome::Superseded
        ));
        assert!(!prime.workspace_overlay_retry_signals().unwrap().initialized);
        drop(newer);

        let overlay_dir = tempdir().unwrap();
        let mut overlay = SearchEngine::fts_only(&overlay_dir.path().join("search.db")).unwrap();
        overlay
            .initialize_workspace_roots(
                bsl_search::WorkspaceRoots::build(overlay_dir.path(), overlay_dir.path(), &[]).0,
            )
            .unwrap();
        let old = crate::workspace_lease::WorkspaceLease::claim(overlay_dir.path());
        let _newer = crate::workspace_lease::WorkspaceLease::claim(overlay_dir.path());
        assert!(SharedState::startup_apply_once(&old, &crate::state::OwnerStop::default(), || {
            overlay.initialize_workspace_overlay_clean()
        })
        .unwrap()
        .is_none());
        assert!(!overlay.workspace_overlay_retry_signals().unwrap().initialized);

        let publish_dir = tempdir().unwrap();
        let engine = SearchEngine::fts_only(&publish_dir.path().join("search.db")).unwrap();
        let old = crate::workspace_lease::WorkspaceLease::claim(publish_dir.path());
        let _newer = crate::workspace_lease::WorkspaceLease::claim(publish_dir.path());
        let slot: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        let mut guard = slot.lock().unwrap();
        assert!(SharedState::startup_apply_once(&old, &crate::state::OwnerStop::default(), || {
            *guard = Some(engine);
            Ok(())
        })
        .unwrap()
        .is_none());
        assert!(guard.is_none(), "terminal refusal publishes neither engine nor status");
        drop(guard);

        let release_dir = tempdir().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim(release_dir.path());
        let held = lease.hold_file_lock_for_test();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker = {
            let lease = lease.clone();
            let calls = Arc::clone(&calls);
            std::thread::spawn(move || {
                SharedState::startup_apply_once(&lease, &crate::state::OwnerStop::default(), || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        lease.release();
        drop(held);
        assert!(worker.join().unwrap().unwrap().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    fn immediate_bootstrap(backend: &'static str, issue: Option<&str>) -> BaselineBootstrap {
        BaselineBootstrap::Immediate(BaselineRuntime {
            configured_baseline: ConfiguredBaselineStatus {
                backend,
                selection: "test".to_owned(),
                issue: issue.map(str::to_owned),
                support: None,
            },
            external_baseline: None,
        })
    }

    /// The walk and the watch must describe the same tree. A source file under the
    /// cache that the walk indexes but the watch never reports is worse than either
    /// alone: it enters the corpus and then freezes at the content of the last full
    /// scan, because no edit to it ever arrives as drift.
    #[test]
    fn a_source_file_under_the_cache_enters_neither_the_walk_nor_the_watch() {
        let workspace = tempdir().unwrap();
        fs::write(
            workspace.path().join("Configuration.xml"),
            "<Configuration><Name>Conf</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            workspace.path(),
            "Живой",
            "Процедура Проц() Экспорт КонецПроцедуры\n",
        );
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(workspace.path());
        let vendored = cache.root().join("vendor");
        write_common_module_tree(&vendored, "Чужой", "Процедура Чуж() Экспорт КонецПроцедуры\n");

        let excluded: Vec<PathBuf> =
            cache.spellings().iter().map(|path| path.to_path_buf()).collect();
        let project = crate::project::at(workspace.path()).unwrap();
        let snapshot =
            crate::graph::input::ProjectSnapshot::from_project_excluding(&project, &excluded);
        let universe = crate::graph::universe::ScannedUniverse::scan_excluding(
            &snapshot.scan_roots,
            &snapshot.excluded,
        );

        let walked: Vec<String> =
            universe.files.iter().map(|(_, path)| path.display().to_string()).collect();
        assert!(
            walked.iter().any(|path| path.contains("Живой")),
            "the workspace module was not walked: {walked:?}"
        );
        assert!(
            !walked.iter().any(|path| path.contains("Чужой")),
            "a module under the cache was walked: {walked:?}"
        );
    }

    /// A cache that contains a scan root would exclude that root from the watch, so the
    /// server would serve a tree it stopped following. Every scan root is checked, not
    /// just the configuration root: an extension root swallowed the same way is the same
    /// silent hole, and a check named after one member of the class leaves the rest open.
    #[test]
    fn a_cache_that_contains_a_scanned_root_is_refused() {
        let workspace = tempdir().unwrap();
        fs::write(
            workspace.path().join("Configuration.xml"),
            "<Configuration><Name>Conf</Name></Configuration>",
        )
        .unwrap();

        let parent = crate::cache::WorkspaceCacheLayout::from_root(
            workspace.path().parent().unwrap().to_path_buf(),
        );
        let refused = SharedState::workspace_with_cache(workspace.path().to_path_buf(), parent);
        let Err(error) = refused else {
            panic!("a cache above the source root must be refused");
        };
        assert!(
            matches!(error, crate::WorkspaceInitError::CacheCoversScanRoot { .. }),
            "unexpected error: {error}"
        );

        // The workspace root is watched too — non-recursively, for config files — and
        // the exclusion is consulted before the config branch, so a cache covering it
        // swallows every config edit just as silently.
        // The sources live OUTSIDE the project directory on purpose: with them inside,
        // a cache covering the project root covers the scan root too, and a check that
        // looks only at scan roots refuses the case anyway — the input would not tell
        // the two implementations apart.
        let elsewhere = tempdir().unwrap();
        let sources = elsewhere.path().join("sources");
        let nested = tempdir().unwrap();
        fs::create_dir_all(&sources).unwrap();
        fs::write(
            sources.join("Configuration.xml"),
            "<Configuration><Name>Conf</Name></Configuration>",
        )
        .unwrap();
        fs::write(
            nested.path().join("bsl-analyzer.toml"),
            format!("[source]\nroot = \"{}\"\n", sources.display()),
        )
        .unwrap();
        let over_project_root =
            crate::cache::WorkspaceCacheLayout::from_root(nested.path().to_path_buf());
        let refused_config =
            SharedState::workspace_with_cache(nested.path().to_path_buf(), over_project_root);
        let Err(error) = refused_config else {
            panic!("a cache covering the workspace root must be refused");
        };
        assert!(
            matches!(error, crate::WorkspaceInitError::CacheCoversScanRoot { .. }),
            "unexpected error: {error}"
        );

        // Positive control: a cache beside the workspace is the ordinary case and must
        // still be accepted, or the assertion above would hold on a build that refuses
        // every cache it is given.
        let beside = tempdir().unwrap();
        let ok = crate::cache::WorkspaceCacheLayout::from_root(beside.path().to_path_buf());
        SharedState::workspace_with_cache(workspace.path().to_path_buf(), ok)
            .expect("a cache outside every source root must be accepted");
    }

    /// A scan root inside a service directory is refused by the same rule, and named as the
    /// service directory it is: the advice for a cache misplacement ("choose another
    /// --cache-dir") would be wrong — the root is what moves.
    #[test]
    fn a_source_root_inside_a_service_directory_is_refused_as_such() {
        let workspace = tempdir().unwrap();
        fs::write(
            workspace.path().join("Configuration.xml"),
            "<Configuration><Name>Conf</Name></Configuration>",
        )
        .unwrap();
        fs::create_dir_all(workspace.path().join("target")).unwrap();
        fs::write(workspace.path().join("target").join("Configuration.xml"), "<Configuration/>")
            .unwrap();
        fs::write(workspace.path().join("bsl-analyzer.toml"), "[source]\nroot = \"target\"\n")
            .unwrap();

        let refused = SharedState::workspace(workspace.path().to_path_buf());
        let Err(error) = refused else {
            panic!("a source root inside `target` must be refused");
        };
        assert!(
            matches!(error, crate::WorkspaceInitError::ScanRootInsideServiceDirectory { .. }),
            "unexpected error: {error}"
        );
    }

    /// A `--cache-dir` that names the very service directory holding the source root is still
    /// refused as the service directory: moving the cache would not free the root.
    #[test]
    fn a_cache_placed_at_the_service_directory_holding_the_root_is_refused_as_service() {
        let workspace = tempdir().unwrap();
        fs::write(
            workspace.path().join("Configuration.xml"),
            "<Configuration><Name>Conf</Name></Configuration>",
        )
        .unwrap();
        fs::create_dir_all(workspace.path().join("target")).unwrap();
        fs::write(workspace.path().join("target").join("Configuration.xml"), "<Configuration/>")
            .unwrap();
        fs::write(workspace.path().join("bsl-analyzer.toml"), "[source]\nroot = \"target\"\n")
            .unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::from_root(workspace.path().join("target"));

        let refused = SharedState::workspace_with_cache(workspace.path().to_path_buf(), cache);
        let Err(error) = refused else {
            panic!("a source root inside `target` must be refused");
        };
        assert!(
            matches!(error, crate::WorkspaceInitError::ScanRootInsideServiceDirectory { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_user_exclusion_covering_the_source_root_is_not_mistaken_for_a_cache_hole() {
        let workspace = tempdir().unwrap();
        fs::write(
            workspace.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\nexclude = [\".\"]\n",
        )
        .unwrap();
        let cache_dir = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::from_root(cache_dir.path().to_path_buf());

        SharedState::workspace_with_cache(workspace.path().to_path_buf(), cache)
            .expect("an intentionally empty user source scope must remain a valid workspace");
    }

    #[test]
    fn workspace_state_uses_external_cache_without_touching_source_tree() {
        use std::time::{Duration, Instant};

        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let _embedding_model = EnvVarGuard::unset("EMBEDDING_MODEL");

        let workspace_parent = tempdir().unwrap();
        let workspace = workspace_parent.path().join("исходники с пробелом");
        fs::create_dir(&workspace).unwrap();
        fs::write(
            workspace.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &workspace,
            "Сервер",
            "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции\n",
        );

        let cache_parent = tempdir().unwrap();
        let cache_root = cache_parent.path().join("внешний кеш");
        let cache = crate::cache::WorkspaceCacheLayout::from_root(cache_root);
        let state = SharedState::workspace_with_cache(workspace.clone(), cache.clone()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(60);
        while state.search_engine().lock().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        while !cache.graph_db_path().exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(cache.search_db_path().exists(), "search DB must use explicit cache");
        assert!(cache.graph_db_path().exists(), "graph DB must use explicit cache");
        assert!(cache.lease_path().exists(), "lease must use explicit cache");
        assert!(!workspace.join(".build").exists(), "source tree must stay untouched");
        state.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_cache_scope_ignores_current_and_sibling_leaf_writes() {
        use std::time::{Duration, Instant};

        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let _embedding_model = EnvVarGuard::unset("EMBEDDING_MODEL");

        let workspace_dir = tempdir().unwrap();
        let workspace = workspace_dir.path();
        let main = workspace.join("src/cf");
        let neighbor = workspace.join(".derived/workspaces/v1-neighbor");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&neighbor).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            workspace.join(".derived/workspaces"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::write(main.join("Configuration.xml"), "<Configuration/>").unwrap();
        fs::write(
            neighbor.join("Configuration.xml"),
            "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
        )
        .unwrap();
        write_common_module_tree(&main, "Main", "Процедура Основная() КонецПроцедуры\n");
        write_common_module_tree(&neighbor, "Neighbor", "Процедура Соседняя() КонецПроцедуры\n");
        let legitimate_build = main.join(".build/Legit.bsl");
        fs::create_dir_all(legitimate_build.parent().unwrap()).unwrap();
        fs::write(&legitimate_build, "Процедура ИзBuild() КонецПроцедуры\n").unwrap();
        fs::write(
            workspace.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\nextensions = [{ name = \"Neighbor\", path = \".derived/workspaces/v1-neighbor\" }]\n",
        )
        .unwrap();

        let project = crate::project::at(workspace).unwrap();
        let base = workspace.join(".derived");
        let cache =
            crate::cache::WorkspaceCacheLayout::for_project(&project, Some(&base), workspace, None)
                .unwrap();
        let state = SharedState::workspace_with_cache(workspace.to_path_buf(), cache.clone())
            .expect("a sibling source tree beside the owned namespace is valid");
        state.graph().ensure_loading();
        let hub = state.change_hub().expect("workspace installs its watcher");

        let deadline = Instant::now() + Duration::from_secs(30);
        while (state.search_engine().lock().unwrap().is_none()
            || state.graph().status_report().state != "ready")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(state.search_engine().lock().unwrap().is_some(), "search startup completed");
        let before_revision = state.graph().status_report().revision.expect("graph is ready");
        let inventory = || {
            state
                .search_engine()
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .store()
                .all_files_in_collection("code")
                .unwrap()
                .into_iter()
                .map(|(key, _)| key.path)
                .collect::<Vec<_>>()
        };
        let before_keys = inventory();
        for source in [".build/Legit.bsl", "CommonModules/Neighbor/Ext/Module.bsl"] {
            assert!(
                before_keys.iter().any(|path| path == source),
                "legitimate source path was not indexed: {source}; inventory={before_keys:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let cursor = hub.subscribe();
        let before_generation = hub.generation();

        let family = cache.root().parent().unwrap();
        let sibling = family.join("0".repeat(64));
        fs::write(cache.root().join("Ignored.bsl"), "Процедура Cache() КонецПроцедуры\n").unwrap();
        fs::write(cache.root().join("writer.lease"), "lease\n").unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("Ignored.bsl"), "Процедура Sibling() КонецПроцедуры\n").unwrap();
        fs::write(sibling.join("writer.lease.lock"), "lock\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(
            hub.generation(),
            before_generation,
            "owned current and sibling leaves are not watched"
        );
        assert!(
            hub.materialize(cursor).entries.is_empty(),
            "cache leaf writes produced watcher events"
        );
        assert_eq!(inventory(), before_keys, "cache writes do not enter search key inventory");
        assert_eq!(
            state.graph().status_report().revision,
            Some(before_revision),
            "cache writes do not publish a graph rebuild"
        );

        // The adjacent extension is a positive control: its source remains watched.
        let adjacent = neighbor.join("New.bsl");
        fs::write(&adjacent, "Процедура СоседняяНовая() КонецПроцедуры\n").unwrap();
        assert!(crate::change_hub::test_support::eventually(Duration::from_secs(5), || {
            hub.generation() > before_generation
        }));
        state.shutdown();
    }

    /// The Postgres branch of the search init returns before it ever reaches the fused cold
    /// build's graph claim, which used to leave the graph idle until the first
    /// `graph`/`symbol_info` call — billing a whole-config build to a mid-session request.
    /// The boot must start it regardless, so this drives the harshest case: an unavailable
    /// baseline, where the search init bails immediately and touches no graph at all.
    #[test]
    fn postgres_boot_starts_the_graph_even_when_the_search_init_bails() {
        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(
            workspace.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &workspace,
            "Сервер",
            "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции\n",
        );

        let graph = GraphState::for_workspace(workspace.clone());
        // Postgres mode with no external baseline: `init_workspace_search_engine` warns and
        // returns `None` without opening a store.
        let baseline = DeferredBaselineRuntime::ready(BaselineRuntime {
            configured_baseline: ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "test".to_owned(),
                issue: Some("baseline unavailable".to_owned()),
                support: None,
            },
            external_baseline: None,
        });

        // Armed before the boot starts, so the wait for it costs nothing and the branch under
        // test is the one that bails on the baseline, not the one that waits on the watch.
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(std::time::Duration::from_secs(5)), "the watch must arm");

        SharedState::spawn_workspace_search_init(
            crate::state::shared_engine(None),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            IndexProgress::new(),
            Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            workspace.clone(),
            hub.clone(),
            crate::change_hub::CursorLease::new(hub),
            baseline,
            WorkspaceSearchMode::PostgresRemoteOverlay,
            graph.clone(),
            EmbedFlight::new(),
            Arc::new(crate::diagnostics_state::ResidentModuleSnapshotSource::new(
                DiagnosticsState::disabled(),
            )),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            None,
            Arc::new(AtomicU64::new(0)),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
            crate::state::OwnerStop::default(),
            Default::default(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
            super::super::types::EmbeddingPrefixes::default(),
        );

        for _ in 0..600 {
            match graph.status() {
                crate::graph::GraphStatus::Ready { .. } => return,
                crate::graph::GraphStatus::Failed(msg) => panic!("graph load failed: {msg}"),
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        panic!("the boot left the graph at {:?}; it must not stay lazy", graph.status());
    }

    fn local_workspace_for_boot(dir: &std::path::Path) -> (PathBuf, DeferredBaselineRuntime) {
        let workspace = dir.to_path_buf();
        fs::write(
            workspace.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &workspace,
            "Сервер",
            "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции\n",
        );
        let baseline = DeferredBaselineRuntime::ready(BaselineRuntime {
            configured_baseline: ConfiguredBaselineStatus {
                backend: "sqlite",
                selection: "test".to_owned(),
                issue: None,
                support: None,
            },
            external_baseline: None,
        });
        (workspace, baseline)
    }

    /// Failure fixtures must seed the same leaf that production startup opens.
    fn resolved_workspace_cache(workspace: &std::path::Path) -> crate::cache::WorkspaceCacheLayout {
        let project = crate::project::at(workspace).expect("fixture project parses");
        let cwd = std::env::current_dir().expect("test process has a current directory");
        let scope =
            crate::cache::expected_scope_from_env().expect("fixture cache scope stamp is valid");
        crate::cache::WorkspaceCacheLayout::for_project(&project, None, &cwd, scope.as_deref())
            .expect("fixture cache namespace resolves")
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_local_boot(
        workspace: PathBuf,
        hub: WorkspaceChangeHub,
        lease: crate::change_hub::CursorLease,
        baseline: DeferredBaselineRuntime,
        engine: super::SharedSearchEngine,
    ) {
        SharedState::spawn_workspace_search_init(
            engine,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            IndexProgress::new(),
            Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            workspace,
            hub,
            lease,
            baseline,
            WorkspaceSearchMode::SqliteLocal,
            GraphState::disabled(),
            EmbedFlight::new(),
            Arc::new(crate::diagnostics_state::ResidentModuleSnapshotSource::new(
                DiagnosticsState::disabled(),
            )),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            None,
            Arc::new(AtomicU64::new(0)),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
            crate::state::OwnerStop::default(),
            Default::default(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
            super::super::types::EmbeddingPrefixes::default(),
        );
    }

    /// Every read this init does is a baseline, and a baseline taken before the watch armed
    /// can be older than the oldest change anyone will ever report — the window this node
    /// exists to close. Closing it is nothing but statement order, which is exactly the kind
    /// of guarantee a later edit erases without noticing, so both reads the init performs
    /// are pinned, each by its own effect.
    ///
    /// The store is pinned by its file: none may exist while the hub is held. The project
    /// load is pinned by its input: the extension is declared only in the instant before
    /// release, so an init that read the project early would register one root and an init
    /// that waited registers two. Checking only the store would leave the project read free
    /// to drift back above the wait — reopening the window on exactly the input whose drift
    /// this node is about.
    #[test]
    fn the_boot_read_waits_for_the_watch_to_arm() {
        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        // The configuration sits in its own subdirectory and the extension beside it: an
        // extension nested inside the configuration root is rejected as an overlap, and the
        // test would then measure the rejection instead of the read order.
        let configuration = workspace.join("src").join("cf");
        let extension = workspace.join("ext");
        for (dir, name) in [(&configuration, "Конфа"), (&extension, "Расширение")] {
            fs::create_dir_all(dir).unwrap();
            fs::write(
                dir.join("Configuration.xml"),
                format!("<Configuration><Name>{name}</Name></Configuration>"),
            )
            .unwrap();
            write_common_module_tree(
                dir,
                "Сервер",
                "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции\n",
            );
        }
        fs::write(workspace.join("bsl-analyzer.toml"), "[source]\nroot = \"src/cf\"\n").unwrap();

        let old_cache = resolved_workspace_cache(&workspace);
        let old_db_path = old_cache.search_db_path();
        let old_leaf = old_cache.root().to_path_buf();
        assert!(!old_leaf.exists(), "the boot has not created the old topology's leaf");
        let (hub, hold) = WorkspaceChangeHub::start_targets_held(vec![
            crate::change_hub::WatchTarget::recursive(workspace.clone()),
        ]);

        let init = {
            let workspace = workspace.clone();
            let hub = hub.clone();
            std::thread::spawn(move || {
                SharedState::init_workspace_search_engine_unmanaged(
                    &workspace,
                    Some((
                        &hub,
                        crate::state::sync::WatchWaitPolicy::new(
                            std::time::Duration::from_millis(5),
                            std::time::Duration::from_secs(30),
                        ),
                    )),
                    WorkspaceSearchMode::SqliteLocal,
                    None,
                    &GraphState::disabled(),
                )
                .map(|init| {
                    init.engine.workspace_roots().map_or(0, |roots| roots.entries().count())
                })
            })
        };

        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !old_db_path.exists(),
            "the boot opened its store before the watch was up: {}",
            old_db_path.display(),
        );

        // Only now is the extension part of the project.
        fs::write(
            workspace.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\nextensions = [{ name = \"e\", path = \"ext\" }]\n",
        )
        .unwrap();
        let new_cache = resolved_workspace_cache(&workspace);
        let new_db_path = new_cache.search_db_path();
        assert_ne!(old_cache.root(), new_cache.root(), "the extension changes the frozen scope");
        assert!(!new_cache.root().exists(), "no storage opens before the held watch is released");
        hold.release();

        let roots = init.join().unwrap().expect("the init runs to completion once the watch is up");
        assert_eq!(
            roots, 2,
            "the project was read after the watch, so the extension declared meanwhile is registered",
        );
        assert!(
            new_db_path.exists(),
            "the init opened the store for the Project read after the watch"
        );
        assert!(!old_leaf.exists(), "the old scope was not opened after the Project changed");
        assert!(!old_db_path.exists());
    }

    /// A cursor is subscribed before the thread that will read it exists, so every way out
    /// that does not end in a running consumer has to release it — and there are more of
    /// those than a list would hold: an init that fails, one that publishes nothing, a spawn
    /// the operating system refuses. A cursor nobody drains holds entries back for the life
    /// of the process. (A watch that never arms is no longer one of them: the consumer runs
    /// in every watch mode.)
    #[test]
    fn a_boot_that_publishes_no_engine_leaves_no_cursor_behind() {
        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");

        let dir = tempdir().unwrap();
        let (workspace, baseline) = local_workspace_for_boot(dir.path());
        // A store that cannot be opened: the init fails and publishes nothing.
        let cache = resolved_workspace_cache(&workspace);
        cache.ensure().unwrap();
        fs::create_dir(cache.search_db_path()).unwrap();
        let hub = WorkspaceChangeHub::start_with_unstartable_thread(vec![
            crate::change_hub::WatchTarget::recursive(workspace.clone()),
        ]);
        let lease = crate::change_hub::CursorLease::new(hub.clone());
        assert_eq!(hub.active_cursor_count(), 1, "the boot holds a cursor from the start");

        spawn_local_boot(
            workspace,
            hub.clone(),
            lease,
            baseline,
            crate::state::shared_engine(None),
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while hub.active_cursor_count() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            hub.active_cursor_count(),
            0,
            "a boot that started no consumer must not leave its cursor subscribed",
        );
    }

    /// Watcher mode is one-way in the store and doubles as "skip the full rescan", so it may
    /// only be asked for by something that is actually feeding the overlay. Asserting the
    /// mode rather than one delivered change is the point: a polling overlay delivers changes
    /// too, through the full scan, so a delivery test would pass either way.
    #[test]
    fn a_boot_with_a_watch_hands_the_cursor_to_a_running_sink() {
        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");

        let dir = tempdir().unwrap();
        let (workspace, baseline) = local_workspace_for_boot(dir.path());
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(std::time::Duration::from_secs(5)), "the watch must arm");
        let lease = crate::change_hub::CursorLease::new(hub.clone());
        let engine: super::SharedSearchEngine = crate::state::shared_engine(None);

        spawn_local_boot(workspace, hub.clone(), lease, baseline, Arc::clone(&engine));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut watching = false;
        while std::time::Instant::now() < deadline {
            let mode = engine
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|engine| engine.workspace_overlay_stats().ok().flatten())
                .map(|stats| stats.watcher_mode);
            if mode == Some(true) {
                watching = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(watching, "an armed boot must end with a sink feeding the overlay");
        assert_eq!(hub.active_cursor_count(), 1, "and the sink owns the cursor it was handed");
    }

    /// A postgres config failure (unconfigured section, credential rejection) must NOT
    /// downgrade the mode to SqliteLocal: that would silently reindex the whole
    /// configuration locally instead of surfacing the configured backend's issue.
    #[test]
    fn workspace_mode_stays_postgres_for_immediate_config_failures() {
        assert!(matches!(
            SharedState::workspace_mode_for(&immediate_bootstrap(
                "postgres",
                Some("credentials rejected"),
            )),
            WorkspaceSearchMode::PostgresRemoteOverlay
        ));
        assert!(matches!(
            SharedState::workspace_mode_for(&immediate_bootstrap("sqlite", None)),
            WorkspaceSearchMode::SqliteLocal
        ));
    }
    #[test]
    fn workspace_external_failure_clears_local_baseline_rows_before_failing_closed() {
        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(
            workspace.join("CommonModule.bsl"),
            "Процедура ЛокальнаяПроцедура()\nКонецПроцедуры",
        )
        .unwrap();
        let cache = resolved_workspace_cache(workspace);
        cache.ensure().unwrap();
        let db_path = cache.search_db_path();
        let mut stale_engine = SearchEngine::fts_only(&db_path).unwrap();
        stale_engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "GhostModule.bsl".to_owned(),
                    symbol_name: "ПризрачнаяПроцедура".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура ПризрачнаяПроцедура()\nКонецПроцедуры".to_owned(),
                    content_hash: "ghost".to_owned(),
                    graph_context: None,
                    source_span: None,
                }],
                None,
            )
            .unwrap();
        assert_eq!(stale_engine.file_count().unwrap(), 1);
        // Seed a persisted manifest too: with the warm-boot cache the init path no
        // longer wipes it up front, so this test must prove the failure branch does.
        stale_engine
            .store()
            .save_baseline_manifest(&bsl_search::WorkspaceBaselineManifest {
                snapshot_id: "stale-snap".to_owned(),
                snapshot_fingerprint: Some("stale-fp".to_owned()),
                files: vec![bsl_search::BaselineManifestFile {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "GhostModule.bsl".to_owned(),
                    file_fingerprint: "ghost".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-ghost".to_owned(),
                }],
            })
            .unwrap();
        assert!(stale_engine.store().load_baseline_manifest().unwrap().is_some());
        drop(stale_engine);

        let external = ExternalBaselineService::for_test(
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
        );

        let init = SharedState::init_workspace_search_engine_unmanaged(
            workspace,
            None,
            crate::state::WorkspaceSearchMode::PostgresRemoteOverlay,
            Some(external),
            &crate::graph::GraphState::disabled(),
        );

        assert!(init.is_none());
        let reopened = SearchEngine::fts_only(&db_path).unwrap();
        assert_eq!(reopened.file_count().unwrap(), 0);
        assert!(reopened.text_search("ПризрачнаяПроцедура", 10, Some("code")).unwrap().is_empty());
        assert!(reopened.store().load_baseline_manifest().unwrap().is_none());
    }
    #[test]
    fn baseline_manifest_matches_snapshot_requires_id_and_fingerprint_agreement() {
        let record =
            |snapshot_id: &str, fingerprint: Option<&str>| bsl_search::BaselineManifestRecord {
                snapshot_id: snapshot_id.to_owned(),
                fingerprint: fingerprint.map(str::to_owned),
                manifest_files: 1,
                fetched_at: "0".to_owned(),
            };
        let snapshot = |id: &str, fingerprint: Option<&str>| {
            let snapshot = bsl_search::Snapshot::new(id, CorpusId::WorkspaceCode);
            match fingerprint {
                Some(fingerprint) => snapshot.with_fingerprint(fingerprint),
                None => snapshot,
            }
        };
        let matches = SharedState::baseline_manifest_matches_snapshot;

        assert!(matches(&record("snap-1", Some("fp-1")), &snapshot("snap-1", Some("fp-1"))));
        assert!(matches(&record("snap-1", None), &snapshot("snap-1", None)));
        assert!(!matches(&record("snap-1", Some("fp-1")), &snapshot("snap-2", Some("fp-1"))));
        assert!(!matches(&record("snap-1", Some("fp-1")), &snapshot("snap-1", Some("fp-2"))));
        assert!(!matches(&record("snap-1", None), &snapshot("snap-1", Some("fp-1"))));
        assert!(!matches(&record("snap-1", Some("fp-1")), &snapshot("snap-1", None)));
    }
    #[test]
    fn workspace_external_failure_with_embeddings_fails_closed_without_hybrid_warmup() {
        let _env_lock = env_lock();
        let _embedding_enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _embedding_url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        // A configured embedder now requires an explicit model (no silent default), so set
        // one here; otherwise the ambient env decides whether the engine is semantic, which
        // is what made this test pass locally but fail in CI.
        let _embedding_model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let cache = resolved_workspace_cache(workspace);
        cache.ensure().unwrap();
        let db_path = cache.search_db_path();
        let mut stale_engine = SearchEngine::fts_only(&db_path).unwrap();
        stale_engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "GhostModule.bsl".to_owned(),
                    symbol_name: "ПризрачнаяПроцедура".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура ПризрачнаяПроцедура()\nКонецПроцедуры".to_owned(),
                    content_hash: "ghost".to_owned(),
                    graph_context: None,
                    source_span: None,
                }],
                None,
            )
            .unwrap();
        drop(stale_engine);

        let external = ExternalBaselineService::for_test(
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
        );

        let init = SharedState::init_workspace_search_engine_unmanaged(
            workspace,
            None,
            crate::state::WorkspaceSearchMode::PostgresRemoteOverlay,
            Some(external),
            &crate::graph::GraphState::disabled(),
        );

        assert!(init.is_none());
        let reopened = SearchEngine::fts_only(&db_path).unwrap();
        assert_eq!(reopened.file_count().unwrap(), 0);
        assert!(reopened.store().load_baseline_manifest().unwrap().is_none());
    }
    #[test]
    fn workspace_standalone_semantic_fallback_publishes_before_embedding() {
        let _env_lock = env_lock();
        // A configured embedder makes the engine semantic, but the URL is unreachable:
        // the point is that init must NOT run the synchronous embed here. It writes the
        // FTS chunks and defers embedding, so init returns promptly with work pending.
        let _embedding_enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _embedding_url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        // A configured embedder now requires an explicit model (no silent default), so set
        // one here; otherwise the ambient env decides whether the engine is semantic, which
        // is what made this test pass locally but fail in CI.
        let _embedding_model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        crate::cache::ensure_workspace_cache_dir(workspace).unwrap();
        fs::write(workspace.join("CommonModule.bsl"), "Процедура СделатьЧтоТо()\nКонецПроцедуры")
            .unwrap();

        // A disabled graph has no workspace root, so the fused path is skipped and the
        // standalone semantic branch runs — the path that previously embedded inline.
        let init = SharedState::init_workspace_search_engine_unmanaged(
            workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("standalone init should produce an engine");

        // FTS chunks are written (lexical search goes live)...
        assert!(init.engine.chunk_count().unwrap() > 0);
        // ...the unreachable embedder was never called, so no vectors exist yet...
        assert_eq!(init.engine.vector_count(), 0);
        // ...and the embedding work is handed to the background pass.
        assert!(init.pending_embed.is_some());
    }
    fn fixture_platform(type_name: &str) -> bsl_platform::PlatformDataInner {
        use bsl_platform::{
            PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest, PlatformHelpSourceKind,
            PlatformSnapshot,
        };
        let mut corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../bsl-platform/tests/fixtures/help/corpus.json"
        ))
        .unwrap();
        for ty in corpus["types"].as_array_mut().unwrap() {
            if ty["english_name"] == "Array" {
                ty["name"] = serde_json::Value::String(type_name.to_owned());
            }
        }
        let snapshot =
            PlatformSnapshot::from_corpus_json(&serde_json::to_vec(&corpus).unwrap()).unwrap();
        bsl_platform::PlatformDataInner::from_help(PlatformHelp::loaded(
            PlatformHelpRequest::ExternalPath(type_name.into()),
            snapshot,
            PlatformHelpOrigin {
                source: PlatformHelpSourceKind::External,
                location: None,
                platform_version: None,
                digest: None,
            },
        ))
    }

    #[test]
    fn a_served_corpus_without_types_is_still_indexed() {
        use bsl_platform::{
            GlobalFunction, PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest,
            PlatformHelpSourceKind, PlatformSnapshot,
        };
        let dir = tempdir().unwrap();
        let mut engine = SearchEngine::fts_only(&dir.path().join("reference-search.db")).unwrap();
        let snapshot = PlatformSnapshot {
            global_functions: vec![GlobalFunction {
                id: 0,
                name: "ФункцияБезТипов".into(),
                english_name: "FunctionWithoutTypes".into(),
                return_type: None,
                parameters: vec![],
                variants: vec![],
                min_version: None,
                context: None,
            }],
            ..PlatformSnapshot::default()
        };
        let platform = bsl_platform::PlatformDataInner::from_help(PlatformHelp::loaded(
            PlatformHelpRequest::ExternalPath("functions.json".into()),
            snapshot,
            PlatformHelpOrigin {
                source: PlatformHelpSourceKind::External,
                location: None,
                platform_version: None,
                digest: None,
            },
        ));
        SharedState::index_platform_docs_from(&mut engine, &IndexProgress::new(), &platform)
            .unwrap();
        assert!(!engine.text_search("ФункцияБезТипов", 10, Some("platform")).unwrap().is_empty());
    }

    #[test]
    fn local_platform_docs_follow_the_served_corpus_and_spare_other_documents() {
        let dir = tempdir().unwrap();
        let mut engine = SearchEngine::fts_only(&dir.path().join("reference-search.db")).unwrap();
        engine
            .index_documents(
                "platform",
                "platform://legacy/external",
                b"external-docs",
                &[Document {
                    title: "ВнешнийСнимокДокумент".to_owned(),
                    body: "Описание ВнешнийСнимокДокумент".to_owned(),
                    kind: "type".to_owned(),
                }],
                None,
            )
            .unwrap();
        let progress = IndexProgress::new();
        let found = |engine: &SearchEngine, text: &str| {
            !engine.text_search(text, 10, Some("platform")).unwrap().is_empty()
        };

        SharedState::index_platform_docs_from(
            &mut engine,
            &progress,
            &fixture_platform("КорпусАльфа"),
        )
        .unwrap();
        assert!(found(&engine, "КорпусАльфа"));

        SharedState::index_platform_docs_from(
            &mut engine,
            &progress,
            &fixture_platform("КорпусБета"),
        )
        .unwrap();
        assert!(!found(&engine, "КорпусАльфа"), "the previous corpus document must go");
        assert!(found(&engine, "КорпусБета"));

        // A configured external snapshot shares the collection; no help must not
        // remove it. Re-add it, since a local corpus replaces the whole collection.
        engine
            .index_documents(
                "platform",
                "platform://legacy/external",
                b"external-docs",
                &[Document {
                    title: "ВнешнийСнимокДокумент".to_owned(),
                    body: "Описание ВнешнийСнимокДокумент".to_owned(),
                    kind: "type".to_owned(),
                }],
                None,
            )
            .unwrap();
        let missing =
            bsl_platform::PlatformDataInner::from_help(bsl_platform::PlatformHelp::missing(
                bsl_platform::PlatformHelpRequest::None,
                "disabled",
            ));
        SharedState::index_platform_docs_from(&mut engine, &progress, &missing).unwrap();
        assert!(!found(&engine, "КорпусБета"), "no corpus, no local platform docs");
        assert!(found(&engine, "ВнешнийСнимокДокумент"), "other documents stay");
        // Idempotent with nothing left to remove.
        SharedState::index_platform_docs_from(&mut engine, &progress, &missing).unwrap();
        assert!(found(&engine, "ВнешнийСнимокДокумент"));
    }

    #[test]
    fn clear_reference_docs_cache_removes_stale_local_and_external_docs() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let mut stale_engine = SearchEngine::fts_only(&db_path).unwrap();
        stale_engine
            .index_documents(
                "platform",
                "platform://docs",
                b"stale-docs",
                &[Document {
                    title: "СтарыйДокумент".to_owned(),
                    body: "Описание СтарыйДокумент".to_owned(),
                    kind: "type".to_owned(),
                }],
                None,
            )
            .unwrap();
        stale_engine
            .index_documents(
                "platform",
                "platform://legacy/external",
                b"stale-external-docs",
                &[Document {
                    title: "СтарыйВнешнийДокумент".to_owned(),
                    body: "Описание СтарыйВнешнийДокумент".to_owned(),
                    kind: "type".to_owned(),
                }],
                None,
            )
            .unwrap();
        assert_eq!(
            stale_engine.text_search("СтарыйДокумент", 10, Some("platform")).unwrap().len(),
            1
        );
        assert_eq!(
            stale_engine.text_search("СтарыйВнешнийДокумент", 10, Some("platform")).unwrap().len(),
            1
        );
        drop(stale_engine);

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        SharedState::clear_reference_docs_cache(&mut engine);

        assert!(engine.text_search("СтарыйДокумент", 10, Some("platform")).unwrap().is_empty());
        assert!(engine
            .text_search("СтарыйВнешнийДокумент", 10, Some("platform"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reference_search_loading_is_single_flight_and_shutdown_joins_worker() {
        let _env = env_lock();
        let dir = tempdir().unwrap();
        let _cache = EnvVarGuard::set("XDG_CACHE_HOME", dir.path().to_str().unwrap());
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let _embedding_model = EnvVarGuard::unset("EMBEDDING_MODEL");
        let state = super::ReferenceSearchState::new(None);
        assert_eq!(state.lifecycle(), super::ReferenceSearchLifecycle::Uninitialized);

        state.ensure_loading();
        let first_worker = state
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(std::thread::JoinHandle::thread)
            .map(std::thread::Thread::id)
            .expect("first ensure starts a worker");
        state.ensure_loading();
        let second_worker = state
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(std::thread::JoinHandle::thread)
            .map(std::thread::Thread::id)
            .expect("worker remains registered");
        assert_eq!(first_worker, second_worker);
        assert!(matches!(state.lifecycle(), super::ReferenceSearchLifecycle::Loading));

        state.shutdown();
        assert!(state.worker.lock().unwrap().is_none());
        assert!(state.engine.lock().unwrap().is_none());
    }

    #[test]
    fn indexing_owner_lifecycles_reference_publication_failure() {
        let _env = env_lock();
        let dir = tempdir().unwrap();
        let _cache = EnvVarGuard::set("XDG_CACHE_HOME", dir.path().to_str().unwrap());
        let _url = EnvVarGuard::unset("EMBEDDING_URL");
        let _model = EnvVarGuard::unset("EMBEDDING_MODEL");
        let state = super::ReferenceSearchState::new(None);
        let engine = state.engine.clone();
        let _ = std::thread::spawn(move || {
            let _held = engine.lock().unwrap();
            panic!("refuse engine publication");
        })
        .join();
        state.ensure_loading();
        // The worker builds the whole reference index before it reaches the lock that
        // refuses it, so the wait is for the worker to END, not for a deadline: the index
        // takes seconds on an idle machine and however long a loaded one gives it. The
        // verdict is written by the guard the worker drops on its way out, so a worker
        // that is gone and a lifecycle still loading cannot coexist once the join returns.
        let worker = state
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("ensure_loading registers its worker");
        // However the worker ends — unwound by the poisoned lock, or returned after a
        // refused publication — it ends without a verdict of its own, and that is the case.
        let _ = worker.join();
        assert!(!state.loading(), "the worker's exit guard writes the verdict before the join");
        assert!(
            matches!(state.lifecycle(), super::ReferenceSearchLifecycle::Failed { reason_code, .. } if reason_code == "worker_gone")
        );
        assert_eq!(state.indexing_snapshot().state, crate::indexing::State::Failed);
        state.shutdown();
    }

    #[test]
    fn reference_search_invalid_project_config_is_terminal() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("bsl-analyzer.toml"), "invalid {{{{ toml").unwrap();

        let state = super::ReferenceSearchState::new(Some(dir.path()));

        assert!(matches!(
            state.lifecycle(),
            super::ReferenceSearchLifecycle::Failed { ref reason_code, .. }
                if reason_code == "project_config_error"
        ));
        assert!(state.engine.lock().unwrap().is_none());
    }

    #[test]
    fn workspace_cache_reference_overlap_disables_optional_owner_without_creating_output() {
        let dir = tempdir().unwrap();
        let reference_cache = dir.path().join("reference-search.db");
        let state = super::ReferenceSearchState::new_with_reference_cache(
            Some(dir.path()),
            Some(&reference_cache),
        );

        assert!(matches!(
            state.lifecycle(),
            super::ReferenceSearchLifecycle::Failed { ref reason_code, ref message }
                if reason_code == "baseline_unavailable"
                    && message.contains("overlaps source root")
        ));
        state.ensure_loading();
        assert!(state.worker.lock().unwrap().is_none(), "disabled optional owner must not start");
        assert!(!reference_cache.exists(), "overlap validation precedes creating the output");
    }

    #[test]
    fn workspace_cache_scope_config_change_stops_owner_transport_but_body_drift_stays_hot() {
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        fs::write(root.join("Module.bsl"), "Процедура До() КонецПроцедуры").unwrap();
        let project = crate::project::at(&root).expect("fixture project parses");
        let base = dir.path().join("cache");
        let cache = crate::cache::WorkspaceCacheLayout::for_project(
            &project,
            Some(&base),
            dir.path(),
            None,
        )
        .expect("cache scope resolves");
        let hub = crate::change_hub::WorkspaceChangeHub::start(vec![root.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let owners = super::super::OwnerStop::default();
        let transport_stop = tokio_util::sync::CancellationToken::new();
        owners.set_scope_transport_stop(transport_stop.clone());
        SharedState::start_scope_guard(
            hub.clone(),
            root.clone(),
            cache,
            owners.clone(),
            transport_stop.clone(),
        );

        let before = hub.generation();
        fs::write(root.join("Module.bsl"), "Процедура После() КонецПроцедуры").unwrap();
        assert!(crate::change_hub::test_support::eventually(Duration::from_secs(5), || {
            hub.generation() > before
        }));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!owners.is_stopped(), "a BSL body edit remains a hot update");
        assert!(!transport_stop.is_cancelled());

        let before_xml = hub.generation();
        fs::write(
            root.join("Configuration.xml"),
            "<Configuration><Name>Updated</Name></Configuration>",
        )
        .unwrap();
        assert!(crate::change_hub::test_support::eventually(Duration::from_secs(5), || {
            hub.generation() > before_xml
        }));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !owners.is_stopped(),
            "ordinary configuration XML data changes do not retire the scope"
        );
        assert!(!transport_stop.is_cancelled());

        let configuration = root.join("src").join("cf");
        fs::create_dir_all(&configuration).unwrap();
        fs::write(configuration.join("Configuration.xml"), "<Configuration/>").unwrap();
        fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \"src/cf\"\n").unwrap();
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(5), || {
                owners.is_stopped() && transport_stop.is_cancelled()
            }),
            "a topology change retires owners and the serving transport"
        );
        hub.shutdown();
        assert!(
            owners.wait_empty(Duration::from_secs(1)),
            "scope guard leaves before shutdown completes"
        );
    }

    #[test]
    fn workspace_cache_scope_supplied_layout_rejects_project_drift_before_cache_creation() {
        let workspace = tempdir().unwrap();
        let cache_parent = tempdir().unwrap();
        let root = workspace.path();
        fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let project = crate::project::at(root).expect("initial Project parses");
        let base = cache_parent.path().join("cache");
        let cache = crate::cache::WorkspaceCacheLayout::for_project(
            &project,
            Some(&base),
            cache_parent.path(),
            None,
        )
        .expect("initial scope resolves");
        let leaf = cache.root().to_path_buf();
        assert!(!leaf.exists());

        fs::write(root.join("bsl-analyzer.toml"), "[source]\nexclude = [\"generated\"]\n").unwrap();
        let error = SharedState::workspace_with_cache(root.to_path_buf(), cache)
            .err()
            .expect("a stale parent layout must fail in the child before writes");
        assert!(error.to_string().contains("cache scope no longer matches"));
        assert!(!leaf.exists(), "old namespace was touched after Project drift");
    }

    #[test]
    fn reference_search_keeps_pending_external_baseline_loading_until_shutdown() {
        let mut state = super::ReferenceSearchState::new(None);
        state.baseline = DeferredBaselineRuntime::pending_for_test();

        state.ensure_loading_with_wait(std::time::Duration::from_millis(10));
        std::thread::sleep(std::time::Duration::from_millis(35));

        assert_eq!(state.lifecycle(), super::ReferenceSearchLifecycle::Loading);
        assert!(state.engine.lock().unwrap().is_none());
        let started = std::time::Instant::now();
        state.shutdown();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(state.worker.lock().unwrap().is_none());
        // The worker is gone, so nothing is loading any more. Saying otherwise keeps a broker
        // backend alive for the life of the process.
        assert!(!state.loading(), "a departed worker left the lifecycle claiming to load");
    }

    /// A mutex is poisoned for good. Reading poison as "still loading" would pin a broker
    /// backend forever, so the lifecycle is read the way every other reader reads it.
    #[test]
    fn a_poisoned_lifecycle_does_not_read_as_forever_loading() {
        let state = super::ReferenceSearchState::new(None);
        let poisoner = state.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lifecycle.lock().unwrap();
            panic!("poison the lifecycle");
        })
        .join();

        // Positive control: without a genuinely poisoned mutex the assertion below holds
        // on any implementation at all.
        assert!(state.lifecycle.is_poisoned(), "the stand poisoned nothing");
        assert!(!state.loading(), "a poisoned lifecycle read as live background work");
    }

    #[test]
    fn reference_search_does_not_fall_back_for_unavailable_postgres_intent() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[search.baseline]\nbackend = \"postgres\"\n",
        )
        .unwrap();
        let state = super::ReferenceSearchState::new(Some(dir.path()));

        state.ensure_loading();
        for _ in 0..100 {
            if matches!(state.lifecycle(), super::ReferenceSearchLifecycle::Failed { .. }) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(matches!(
            state.lifecycle(),
            super::ReferenceSearchLifecycle::Failed { ref reason_code, .. }
                if reason_code == "baseline_unavailable"
        ));
        assert!(state.engine.lock().unwrap().is_none());
        state.shutdown();
    }
    #[test]
    fn extension_only_workspace_does_not_expose_workspace_as_configuration_root() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let extension = ws.join("Расширения").join("Feature");
        fs::create_dir_all(&extension).unwrap();
        fs::write(
            extension.join("Configuration.xml"),
            "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
        )
        .unwrap();
        fs::write(
            ws.join("bsl-analyzer.toml"),
            "[source]\nextensions = [\"Расширения/Feature\"]\n",
        )
        .unwrap();

        let project = crate::project::at(ws).expect("valid extension-only project");
        assert!(project.configuration_path().is_none());
        let (roots, rejected) = crate::project::workspace_roots(&project, &[]);
        assert!(rejected.is_empty());
        assert!(roots.configuration().is_none());

        let state = SharedState::workspace(ws.to_path_buf()).expect("extension-only workspace");
        assert!(
            state.source_root().is_none(),
            "workspace directory must not become a synthetic base configuration"
        );
        state.shutdown();
    }

    /// `metadata form` in a nested layout — config root `<ws>/src/cf`, workspace root one
    /// level up — resolves object form directories relative to the CONFIG root. That root
    /// is `SharedState::source_root()`, which must survive the `MetadataCache` retirement:
    /// `form` reads it directly and is the one metadata action with no substrate backing.
    #[test]
    fn metadata_form_resolves_under_nested_source_root_after_cache_removal() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        let cf = ws.join("src").join("cf");
        fs::create_dir_all(cf.join("Catalogs").join("Товары").join("Forms").join("ФормаСписка"))
            .unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();

        let state = SharedState::workspace(ws.to_path_buf()).expect("valid workspace project");
        let source_root =
            state.source_root().cloned().expect("source_root is set for a workspace profile");
        assert!(
            source_root.ends_with("src/cf") || source_root.ends_with("src\\cf"),
            "source_root points at the nested config root, not the workspace root: {source_root:?}",
        );

        // `metadata form` lists the object's forms relative to that config root.
        let result = crate::tools::metadata::get_form_structure(
            Some(&source_root),
            "Catalog",
            Some("Товары"),
            None,
        );
        state.shutdown();
        let result = result.expect("metadata form must resolve under the nested config root");
        let text = result.content[0].as_text().expect("text content").text.clone();
        assert!(text.contains("ФормаСписка"), "form listing resolves under src/cf: {text}");
    }
    #[test]
    fn workspace_boot_initializes_overlay_so_local_edits_serve_fresh_from_resident() {
        use crate::diagnostics_state::DiagnosticsStatus;
        use std::time::{Duration, Instant};

        let _env_lock = env_lock();
        // No embedder configured -> FTS-only local mode, the branch whose overlay was never
        // initialized before this fix.
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let _embedding_model = EnvVarGuard::unset("EMBEDDING_MODEL");

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(
            workspace.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        // v1 baseline body; the boot ingests it into the store.
        write_common_module_tree(
            &workspace,
            "Сервер",
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n",
        );
        let module = workspace.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");

        let state = SharedState::workspace(workspace.clone()).expect("valid workspace project");

        // Wait for the background init to publish the engine (the overlay is initialized just
        // before publish, so a visible engine already has it online).
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if state.search_engine().lock().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "the search engine never published");
            std::thread::sleep(Duration::from_millis(20));
        }

        // The resident feeds the overlay's shared parse; drive it to Ready.
        state.diagnostics().ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !matches!(state.diagnostics().status(), DiagnosticsStatus::Ready { .. }) {
            assert!(Instant::now() < deadline, "the resident never became ready");
            std::thread::sleep(Duration::from_millis(20));
        }

        // Observe the workspace watcher independently so the edit's delivery is confirmed before we
        // rely on the resident's own cursor (which a point refresh drains via catch_up).
        let hub = state.change_hub().expect("workspace boot owns a change hub").clone();
        assert!(hub.wait_until_watching(Duration::from_secs(10)), "the watcher must arm");
        let mut observer = hub.subscribe();

        // Edit on disk: v2 adds a symbol absent from the v1 baseline, so a hit for it can only come
        // from the overlay serving the working-tree bytes.
        std::thread::sleep(Duration::from_millis(20));
        fs::write(
            &module,
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n\
             Процедура СвежаяПроцедура() Экспорт КонецПроцедуры\n",
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut delivered = false;
        while Instant::now() < deadline {
            let batch = hub.drain(observer);
            observer = batch.cursor;
            if batch.entries.iter().any(|e| e.raw.to_string_lossy().ends_with("Module.bsl")) {
                delivered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(delivered, "the watcher delivered the edit");

        // Nothing is marked or refreshed by hand: the consumer (spawned by
        // `SharedState::workspace`) marks the edited `.bsl` dirty in the overlay, and the backlog
        // owner reads it back — catching the resident up first, so its shared parse matches the
        // new bytes. That is only reachable once the boot brought the overlay online; revert that
        // wiring and the overlay stays uninitialized, the mark lands nowhere, and this never
        // trips. So waiting for a resident-fed entry exercises hub -> consumer -> mark -> owner.
        let deadline = Instant::now() + Duration::from_secs(20);
        let fed = loop {
            let fed = state
                .search_engine()
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_resident_fed_count()
                .unwrap();
            if fed >= 1 {
                break fed;
            }
            assert!(Instant::now() < deadline, "the resident-fed reindex never ran");
            std::thread::sleep(Duration::from_millis(20));
        };
        // One feed per batch, and the owner takes a batch per delivered mark — a single disk
        // write reaches the watcher as one batch or as several, so the count is bounded by
        // delivery, not by the edit. What it has to show is that the reindex came from the
        // resident's shared parse at all, rather than from a second disk read of the same file.
        assert!(fed >= 1, "the edited file was reindexed from the resident's shared parse");

        // Fresh bytes are served: the new symbol is found through the overlay (the lexical path the
        // search tool drives), though it is absent from the v1 store baseline.
        let (hits, _hidden) = {
            let guard = state.search_engine().lock().unwrap();
            let engine = guard.as_ref().unwrap();
            engine.workspace_overlay_lexical_hits("СвежаяПроцедура", 10).unwrap()
        };
        state.shutdown();
        assert!(
            hits.iter().any(|hit| hit.file_path.ends_with("Module.bsl")),
            "the overlay must serve the fresh working-tree bytes for the edited file",
        );
    }
    /// Warm boot of a local FTS-only workspace: the store is reused from a prior run and its
    /// re-index is skipped (chunks already exist), so a file changed WHILE THE DAEMON WAS DOWN is
    /// not in the store and no watcher event ever fires for it. That branch must NOT empty-init the
    /// overlay (which would be false-clean and serve the stale baseline forever) — it must prime,
    /// scanning disk against the store baseline so the while-down edit is served fresh. This asserts
    /// the branch selects [`OverlayInit::Prime`] and that the prime serves the fresh bytes with no
    /// dirty-marking at all.
    #[test]
    fn warm_boot_ftsonly_primes_overlay_for_edits_made_while_down() {
        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let _embedding_model = EnvVarGuard::unset("EMBEDDING_MODEL");

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(
            workspace.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &workspace,
            "Сервер",
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n",
        );
        let module = workspace.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");

        // First (cold) boot: an empty store, so the FTS index is built from v1 disk and the branch
        // reconciles -> Clean. Dropping the init persists the store under the workspace cache dir.
        let cold = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("cold FTS-only init produces an engine");
        assert!(matches!(cold.overlay_init, OverlayInit::Clean), "cold boot reconciles the store");
        assert!(cold.engine.chunk_count().unwrap() > 0, "the store now holds the v1 baseline");
        drop(cold);

        // The daemon is "down"; the file gains a symbol absent from the persisted v1 store.
        fs::write(
            &module,
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n\
             Процедура СвежаяПроцедура() Экспорт КонецПроцедуры\n",
        )
        .unwrap();

        // Second (warm) boot: the persisted store already has chunks, so FTS re-indexing is skipped
        // and the store is NOT reconciled with the while-down edit -> this branch must prime.
        let warm = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("warm FTS-only init produces an engine");
        assert!(
            matches!(warm.overlay_init, OverlayInit::Prime),
            "a warm store that skipped re-indexing must prime, not empty-init",
        );

        // The store baseline is stale (v1), proving the boot did not reconcile it:
        assert!(
            warm.engine
                .text_search("СвежаяПроцедура", 10, Some("code"))
                .unwrap_or_default()
                .is_empty(),
            "sanity: the stale store baseline does not hold the while-down symbol",
        );

        // Apply the boot's chosen initialization exactly as `spawn_workspace_search_init` does. The
        // prime scans disk against the store baseline; NO dirty-marking, NO watcher event.
        warm.engine.prime_workspace_overlay().unwrap();

        let (hits, _hidden) =
            warm.engine.workspace_overlay_lexical_hits("СвежаяПроцедура", 10).unwrap();
        assert!(
            hits.iter().any(|hit| hit.file_path.ends_with("Module.bsl")),
            "the prime must serve the while-down edit that no watcher event covered",
        );
    }
    /// Deleted-while-down through the REAL init path on the STANDALONE (deferred-embedding) branch,
    /// a Clean branch: a semantic engine indexes two modules, the daemon stops, one module is
    /// deleted on disk, and a re-boot re-runs the deferred index — which only re-ingests files that
    /// still EXIST. The boot reconcile is what removes the vanished module's rows so the store ==
    /// working tree, and the branch must still assert Clean (its baseline is now truly clean).
    /// Store-level `file_count` is asserted (not `text_search(code)`, which routes through the
    /// overlay and would hide the deleted file regardless), so reverting the boot reconcile leaves
    /// the ghost row and fails this.
    #[test]
    fn deferred_boot_reconciles_deleted_file_and_stays_clean() {
        let _env_lock = env_lock();
        // A configured embedder selects the semantic deferred branch; the URL is never dialed
        // (deferred indexing writes NULL embeddings), it only flips `has_semantic` true.
        let _embedding_enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _embedding_url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        let _embedding_model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(
            workspace.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &workspace,
            "Постоянный",
            "&НаСервере\nФункция ЖивойСимвол() Экспорт Возврат 1; КонецФункции\n",
        );
        write_common_module_tree(
            &workspace,
            "Улетевший",
            "&НаСервере\nФункция ИсчезнувшийСимвол() Экспорт Возврат 1; КонецФункции\n",
        );

        // Cold boot: the deferred branch indexes both modules -> Clean; drop persists the store.
        let cold = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("cold deferred init produces an engine");
        assert!(cold.engine.has_semantic(), "a configured embedder selects the semantic branch");
        assert!(matches!(cold.overlay_init, OverlayInit::Clean), "the deferred branch is Clean");
        assert_eq!(cold.engine.file_count().unwrap(), 2, "both modules are indexed");
        drop(cold);

        // The daemon is down; the Улетевший module is deleted.
        fs::remove_dir_all(workspace.join("CommonModules").join("Улетевший")).unwrap();
        fs::remove_file(workspace.join("CommonModules").join("Улетевший.xml")).unwrap();

        // Warm re-boot through the same real init path: the deferred re-index only sees present
        // files, so ONLY the boot reconcile can remove the deleted module's rows.
        let warm = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("warm deferred init produces an engine");
        assert!(
            matches!(warm.overlay_init, OverlayInit::Clean),
            "a reconciled deferred boot stays Clean",
        );
        assert_eq!(
            warm.engine.file_count().unwrap(),
            1,
            "the boot reconcile removed the deleted-while-down module's rows",
        );
        let files: Vec<String> = warm
            .engine
            .store()
            .all_files_in_collection("code")
            .unwrap()
            .into_iter()
            .map(|(key, _hash)| key.path)
            .collect();
        assert!(
            files.iter().any(|p| p.contains("Постоянный")),
            "the surviving module is untouched: {files:?}",
        );
        assert!(
            !files.iter().any(|p| p.contains("Улетевший")),
            "the deleted module is gone from the store: {files:?}",
        );
    }

    #[test]
    fn warm_deferred_boot_reuses_frozen_embedding_prefixes_for_pending_pass() {
        let _env_lock = env_lock();
        let _enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _embedding_url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        let _embedding_model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");
        let _embedding_dim = EnvVarGuard::set("EMBEDDING_DIM", "8");
        let _query_prefix = EnvVarGuard::set("EMBEDDING_QUERY_PREFIX", "environment-query");
        let _document_prefix =
            EnvVarGuard::set("EMBEDDING_DOCUMENT_PREFIX", "environment-document");

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(
            workspace.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &workspace,
            "Сервер",
            "&НаСервере\nФункция Тест() Экспорт Возврат 1; КонецФункции\n",
        );
        let frozen_prefixes = super::super::types::EmbeddingPrefixes {
            query: "frozen-query".to_owned(),
            document: "frozen-document".to_owned(),
            token_profile: None,
        };

        let initialize = |prefixes: &super::super::types::EmbeddingPrefixes| {
            SharedState::init_workspace_search_engine(
                &workspace,
                None,
                WorkspaceSearchMode::SqliteLocal,
                None,
                &GraphState::disabled(),
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                &super::super::OwnerStop::default(),
                prefixes,
            )
            .unwrap()
            .unwrap()
        };
        let assert_pending_profile = |init: &super::WorkspaceSearchInit| {
            let pending = init.pending_embed.as_ref().expect("NULL chunks schedule embedding");
            let pending_embedder = bsl_search::Embedder::new(pending.config.embedder.clone());
            let pending_identity = pending_embedder.storage_identity();
            assert_eq!(Some(pending_identity), init.engine.embedding_storage_identity());
            assert_eq!(pending.config.embedder.query_prefix, frozen_prefixes.query);
            assert_eq!(pending.config.embedder.document_prefix, frozen_prefixes.document);
        };

        let cold = initialize(&frozen_prefixes);
        assert_pending_profile(&cold);
        drop(cold);

        let warm = initialize(&frozen_prefixes);
        assert_pending_profile(&warm);
    }

    /// A file that still EXISTS but was gutted to comments-only while the daemon was down yields zero
    /// chunks; the boot indexer must REMOVE its now-stale prior chunks rather than skip it (the
    /// deletion reconcile can't help — the file is not gone). Index a module with a symbol, gut it,
    /// re-index: the prior chunk must leave the store. Reverting the chunkless-removal (bare
    /// `continue`) leaves the stale chunk and fails this.
    #[test]
    fn boot_indexer_removes_stale_chunks_when_file_gutted_to_comments() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module_tree(
            &workspace,
            "Сервер",
            "&НаСервере\nФункция УникальныйСимвол() Экспорт Возврат 1; КонецФункции\n",
        );
        let module = workspace.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        assert_eq!(engine.chunk_count().unwrap(), 1, "the original body is one chunk");

        // Gutted to a comment-only file while down: hash changes, chunking yields nothing.
        fs::write(&module, "// только комментарий, без исполняемого кода\n").unwrap();
        engine.index_directory_fts(&workspace).unwrap();

        assert_eq!(
            engine.chunk_count().unwrap(),
            0,
            "the stale chunk of the now-chunkless file is removed from the store",
        );
    }

    /// Lay out a workspace whose configuration sits in a SUBDIRECTORY and whose extensions sit
    /// beside it, then declare both extensions. The nesting matters: it is the only shape where
    /// a table built on the configuration root and one built on the project root disagree about
    /// every extension's identifier.
    fn workspace_with_two_extensions() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let configuration = workspace.join("src").join("cf");
        fs::create_dir_all(&configuration).unwrap();
        fs::write(
            configuration.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &configuration,
            "Основной",
            "&НаСервере\nФункция СимволКонфигурации() Экспорт Возврат 1; КонецФункции\n",
        );
        for (directory, symbol) in [("ext-a", "СимволПервого"), ("ext-b", "СимволВторого")]
        {
            let extension = workspace.join(directory);
            fs::create_dir_all(&extension).unwrap();
            fs::write(
                extension.join("Configuration.xml"),
                format!("<Configuration><Name>{directory}</Name></Configuration>"),
            )
            .unwrap();
            write_common_module_tree(
                &extension,
                "Расширенный",
                &format!("&НаСервере\nФункция {symbol}() Экспорт Возврат 1; КонецФункции\n"),
            );
        }
        fs::write(
            workspace.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\n\
             extensions = [{ name = \"a\", path = \"ext-a\" }, { name = \"b\", path = \"ext-b\" }]\n",
        )
        .unwrap();
        (dir, workspace)
    }

    /// The production boot must register EVERY declared extension root, and under the identifier
    /// `WorkspaceRoots` promises: relative to the project directory, not to the configuration root
    /// ("root ids are relative to it… not the configuration root, which may sit in a
    /// subdirectory"). The identifier is the stored rows' identity across restarts, so an
    /// absolute one is a silent, persistent mis-keying rather than a cosmetic difference.
    /// Two extensions, because a table built from only the first element of the declared list
    /// satisfies every single-extension check.
    #[test]
    fn boot_registers_every_declared_extension_under_a_workspace_relative_id() {
        let _env_lock = env_lock();
        let (_dir, workspace) = workspace_with_two_extensions();

        let init = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("the local init produces an engine");

        let roots = init.engine.workspace_roots().expect("the boot configures a root table");
        let ids: Vec<&str> = roots.ids().collect();
        assert!(
            ids.contains(&"ext-a") && ids.contains(&"ext-b"),
            "both declared extensions are registered by their workspace-relative ids: {ids:?}",
        );

        let extension_module = workspace
            .join("ext-a")
            .join("CommonModules")
            .join("Расширенный")
            .join("Ext")
            .join("Module.bsl");
        assert!(
            init.engine.mark_workspace_path_dirty(&extension_module).unwrap(),
            "a file of a declared extension resolves to a store key",
        );

        // The table's workspace is the project directory, but the base callers resolve a hit's
        // relative path against is the CONFIGURATION root. Conflating the two sends every
        // relative path one directory level too high, which shows up as unresolvable graph ids
        // and unrecognised root descriptors rather than as an error.
        assert_eq!(
            init.engine.configuration_root(),
            Some(workspace.join("src").join("cf").as_path()),
            "the configuration root stays the base of stored relative paths",
        );
    }

    /// The graph's drift has an owner of its own. With search down for good — its store cannot
    /// even be opened — nothing search-side will ever run, and a body edit still has to
    /// reach the graph.
    #[test]
    fn the_graph_follows_edits_after_search_init_failed() {
        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let cf = workspace.join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        crate::graph::test_support::sample_workspace(&cf);
        let cache = resolved_workspace_cache(&workspace);
        cache.ensure().unwrap();
        fs::create_dir(cache.search_db_path()).unwrap();

        let state = SharedState::workspace(workspace.clone()).expect("valid workspace project");
        wait_until_graph_ready(state.graph());
        assert!(
            crate::change_hub::test_support::eventually(std::time::Duration::from_secs(30), || {
                matches!(
                    *state.semantic_runtime().lock().unwrap(),
                    SemanticRuntimeStatus::Failed(_)
                )
            }),
            "the stand needs a search init that failed"
        );
        let before = wait_until_graph_revision(state.graph());

        fs::write(
            cf.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl"),
            "&НаСервере\nФункция Считать() Экспорт Возврат 7; КонецФункции",
        )
        .unwrap();

        let reloaded =
            crate::change_hub::test_support::eventually(std::time::Duration::from_secs(30), || {
                state.graph().status_report().revision.is_some_and(|revision| revision > before)
            });
        state.shutdown();
        assert!(reloaded, "the edit never reached the graph with search down");
    }

    /// A workspace whose watch never came up is not a workspace nobody watches: the hub polls
    /// it, and both the search consumer and the graph watcher take the poll's records. The
    /// consumer runs although the boot's wait for the watch ended in failure.
    #[test]
    fn a_workspace_without_a_watch_follows_edits_through_the_poll() {
        struct PollOff;
        impl Drop for PollOff {
            fn drop(&mut self) {
                crate::change_hub::POLL_INSTEAD_OF_WATCHING.with(|poll| poll.set(None));
            }
        }
        let _env_lock = env_lock();
        crate::change_hub::POLL_INSTEAD_OF_WATCHING.with(|poll| {
            poll.set(Some(crate::change_hub::PollConfig {
                period: std::time::Duration::from_millis(200),
                verify_bytes: 1 << 20,
            }))
        });
        let _poll_off = PollOff;
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let cf = workspace.join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        crate::graph::test_support::sample_workspace(&cf);

        let state = SharedState::workspace(workspace.clone()).expect("valid workspace project");
        let eventually = |f: &dyn Fn() -> bool| {
            crate::change_hub::test_support::eventually(std::time::Duration::from_secs(30), f)
        };
        assert!(
            eventually(&|| state.change_hub().unwrap().is_polling()),
            "the stand needs a hub that polls"
        );
        wait_until_graph_ready(state.graph());
        let before = wait_until_graph_revision(state.graph());
        assert!(
            eventually(&|| state.search_engine().lock().unwrap().is_some()),
            "the search engine was never published"
        );

        fs::write(
            cf.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl"),
            "&НаСервере\nФункция ОпросНайден() Экспорт КонецФункции",
        )
        .unwrap();
        // Boot and edit publications can invalidate consecutive point batches. Their
        // scheduled retries must fit inside the wait, with the ordinary ceiling left for
        // polling and preparation; thirty seconds alone ends at the first retry's deadline.
        let search_ceiling = super::super::overlay_retry::retry_delay(1)
            + super::super::overlay_retry::retry_delay(2)
            + std::time::Duration::from_secs(30);
        let searched = crate::change_hub::test_support::eventually(search_ceiling, || {
            state.search_engine().lock().unwrap().as_ref().is_some_and(|engine| {
                engine
                    .text_search_read_only("ОпросНайден", 10, Some("code"))
                    .is_ok_and(|hits| !hits.is_empty())
            })
        });
        let reloaded = eventually(&|| {
            state.graph().status_report().revision.is_some_and(|revision| revision > before)
        });
        let graph_state = crate::graph::test_support::graph_state_summary(state.graph());
        let search_state = format!(
            "consumer {:?}; backlog {:?}; polls {}; seq {}",
            *state.search_consumer.lock().unwrap(),
            state.overlay_backlog_state(),
            state.change_hub().unwrap().poll_count(),
            state.change_hub().unwrap().seq(),
        );
        state.shutdown();
        assert!(searched, "search never saw the polled edit: {search_state}; {graph_state}");
        assert!(reloaded, "the graph never saw the polled edit: {graph_state}");
    }

    /// A quiet boot on a healthy hub: both workspace sources say they are watched, and the
    /// graph calls itself fresh after its watcher's first look with no event at all. After the
    /// daemon stops, both say nobody watches, and the graph stops calling itself fresh.
    #[test]
    fn the_search_and_the_graph_say_who_watches_them() {
        use crate::tools::location::DriftWatch;
        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let cf = workspace.join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        crate::graph::test_support::sample_workspace(&cf);
        let state = SharedState::workspace(workspace).expect("valid workspace project");
        let eventually = |f: &dyn Fn() -> bool| {
            crate::change_hub::test_support::eventually(std::time::Duration::from_secs(30), f)
        };
        assert!(eventually(&|| state.search_watch().drift_watch == Some(DriftWatch::Watching)));
        wait_until_graph_ready(state.graph());
        assert!(
            eventually(&|| {
                let report = state.graph().status_report();
                report.drift_watch == Some("watching") && report.stale == Some(false)
            }),
            "a quiet, watched boot never read fresh: {:?}",
            state.graph().status_report().stale
        );

        state.shutdown();
        assert!(eventually(&|| state.search_watch().drift_watch == Some(DriftWatch::Unobserved)));
        assert!(eventually(&|| {
            let report = state.graph().status_report();
            report.drift_watch == Some("unobserved") && report.stale != Some(false)
        }));
    }

    /// A quiet boot on a healthy hub costs nothing past the boot itself: no reconcile is asked
    /// of the hub, no context mark is placed, and the graph is neither reloaded nor rebuilt,
    /// however many coverage ticks follow.
    #[test]
    fn a_quiet_healthy_boot_asks_for_no_reconcile_mark_or_reload() {
        use crate::tools::location::DriftWatch;
        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let cf = workspace.join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        crate::graph::test_support::sample_workspace(&cf);
        let state = SharedState::workspace(workspace).expect("valid workspace project");
        let eventually = |f: &dyn Fn() -> bool| {
            crate::change_hub::test_support::eventually(std::time::Duration::from_secs(30), f)
        };
        assert!(eventually(&|| state.search_watch().drift_watch == Some(DriftWatch::Watching)));
        wait_until_graph_ready(state.graph());
        assert!(eventually(&|| state.graph().status_report().drift_watch == Some("watching")));
        let revision = wait_until_graph_revision(state.graph());

        let hub = state.change_hub().expect("a workspace boot owns a hub").clone();
        for _ in 0..3 {
            assert!(hub.tick_now(std::time::Duration::from_secs(10)), "the hub stopped ticking");
        }
        assert_eq!(hub.rescan_request_count(), 0, "a healthy hub was asked to reconcile");
        assert_eq!(state.graph().owes_forced(), None, "a quiet boot reloaded the project");
        assert_eq!(wait_until_graph_revision(state.graph()), revision, "a quiet boot rebuilt");
        assert!(!state.graph().marks_pending(), "a quiet boot placed marks");
        {
            let guard = state.search_engine().lock().unwrap();
            let engine = guard.as_ref().expect("the local index is published");
            assert_eq!(engine.mark_seq_handle().load(Ordering::SeqCst), 0);
            assert!(engine.context_dirty_paths("code").unwrap().is_empty());
        }
        state.shutdown();
    }

    /// A newer daemon takes the workspace while edits keep arriving: the search consumer and
    /// graph watcher leave on their own, and the edits after takeover change nothing this
    /// daemon owns. Its scope guard remains subscribed while this session can still serve, so
    /// a project-input change can retire the immutable cache scope; ordinary supersession must
    /// not cancel that transport ahead of the daemon's established drain path.
    #[test]
    fn a_superseded_daemon_applies_nothing_more_and_releases_its_cursors() {
        use crate::tools::location::DriftWatch;
        let _env_lock = env_lock();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let cf = workspace.join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        crate::graph::test_support::sample_workspace(&cf);
        let state = SharedState::workspace(workspace.clone()).expect("valid workspace project");
        let eventually = |f: &dyn Fn() -> bool| {
            crate::change_hub::test_support::eventually(std::time::Duration::from_secs(30), f)
        };
        assert!(eventually(&|| state.search_watch().drift_watch == Some(DriftWatch::Watching)));
        wait_until_graph_ready(state.graph());
        assert!(eventually(&|| state.graph().status_report().drift_watch == Some("watching")));
        let hub = state.change_hub().expect("a workspace boot owns a hub").clone();
        // The report leaves the revision out while a background read holds the graph's lock
        // or every handle, so the sample is waited for rather than taken once.
        let sampled = std::cell::Cell::new(None);
        assert!(eventually(&|| {
            sampled.set(state.graph().status_report().revision);
            sampled.get().is_some()
        }));
        let revision = sampled.get();
        let cursors = hub.active_cursor_count();

        let cache = resolved_workspace_cache(&workspace);
        let _newer = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        fs::write(
            cf.join("CommonModules/Сервер/Ext/Module.bsl"),
            "Функция Считать() Экспорт\nВозврат 2;\nКонецФункции",
        )
        .unwrap();
        fs::write(
            cf.join("CommonModules/Клиент/Ext/Module.bsl"),
            "Процедура Главная() Экспорт\nКонецПроцедуры",
        )
        .unwrap();

        assert!(
            eventually(&|| state.search_watch().drift_watch == Some(DriftWatch::Unobserved)
                && state.graph().status_report().drift_watch == Some("unobserved")),
            "an owner stayed on a workspace it no longer owns"
        );
        assert!(cursors >= 2, "control: the consumers were subscribed, got {cursors}");
        {
            let guard = state.search_engine().lock().unwrap();
            let engine = guard.as_ref().expect("the local index is published");
            // Nothing is left that could still apply them: both consumers are gone, and the
            // backlog owner answers marks, of which there are none.
            let stats = engine.workspace_overlay_stats_read_only().unwrap().expect("workspace");
            assert_eq!(
                (stats.overlay_files, stats.pending_dirty_paths),
                (0, 0),
                "an edit was applied"
            );
        }
        assert!(
            eventually(&|| state.graph().released()),
            "the superseded graph kept the file from the owner"
        );
        let report = state.graph().status_report();
        assert_eq!((report.state, report.superseded), ("failed", Some(true)), "it serves nothing");
        let on_disk = crate::graph::test_support::meta_string(&cache.graph_db_path(), "revision");
        assert_eq!(Some(on_disk.parse::<u64>().unwrap()), revision, "the graph was rebuilt");
        assert!(
            !state.scope_transport_stop().is_cancelled(),
            "ordinary supersession leaves scope transport shutdown to the existing drain path"
        );
        assert!(
            eventually(&|| hub.active_cursor_count() == 1),
            "only the live session's scope guard remains after search and graph owners leave; got {}",
            hub.active_cursor_count()
        );
        let extension = workspace.join("extra-extension");
        fs::create_dir_all(&extension).unwrap();
        fs::write(extension.join("Configuration.xml"), "<Configuration/>").unwrap();
        fs::write(
            workspace.join("bsl-analyzer.toml"),
            "[source]\nroot = \"cf\"\nextensions = [{ name = \"extra\", path = \"extra-extension\" }]\n",
        )
        .unwrap();
        assert!(
            eventually(&|| state.scope_transport_stop().is_cancelled()),
            "a changed Project retires the still-live scope guard after ordinary supersession"
        );
        assert!(
            eventually(&|| hub.active_cursor_count() == 0),
            "the terminal scope guard releases its cursor"
        );
        state.shutdown();
    }

    /// A search init that publishes nothing abandons its consumer: `search_code` says nobody
    /// watches the workspace for the index, instead of `starting` for ever.
    #[test]
    fn a_search_init_that_publishes_nothing_leaves_the_index_unobserved() {
        use crate::tools::location::DriftWatch;
        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let cf = workspace.join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        crate::graph::test_support::sample_workspace(&cf);
        let cache = resolved_workspace_cache(&workspace);
        cache.ensure().unwrap();
        fs::create_dir(cache.search_db_path()).unwrap();
        let state = SharedState::workspace(workspace).expect("valid workspace project");
        let abandoned =
            crate::change_hub::test_support::eventually(std::time::Duration::from_secs(30), || {
                state.search_watch().drift_watch == Some(DriftWatch::Unobserved)
            });
        state.shutdown();
        assert!(abandoned, "a consumer nothing will ever start still reads as starting");
    }

    /// A ready graph's nonblocking report can omit its revision while an owner holds a
    /// sampled lock. Keep the sample that ended the wait instead of racing another read.
    fn wait_until_graph_revision(graph: &GraphState) -> u64 {
        let mut revision = None;
        crate::graph::test_support::wait_until_within(
            graph,
            std::time::Duration::from_secs(60),
            "a readable graph revision",
            || {
                revision = graph.status_report().revision;
                revision.is_some()
            },
        );
        revision.expect("the wait captured a graph revision")
    }

    /// Drive a graph to `Ready`, or say which state it got stuck in.
    fn wait_until_graph_ready(graph: &GraphState) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match graph.status() {
                crate::graph::GraphStatus::Ready { .. } => return,
                crate::graph::GraphStatus::Failed(msg) => panic!("graph load failed: {msg}"),
                other => assert!(
                    std::time::Instant::now() < deadline,
                    "the graph never became ready; it stayed {other:?}",
                ),
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn canonical(path: &std::path::Path) -> PathBuf {
        path.canonicalize()
            .unwrap_or_else(|e| panic!("{path:?} must exist to be canonicalized: {e}"))
    }

    /// The boot declares to the change hub EVERY root the project has — the configuration
    /// and each extension — plus the workspace directory itself, non-recursively, so a
    /// config-file edit is delivered even in a nested layout.
    ///
    /// The set is observable on a SECOND boot and only there. Every graph path that builds
    /// re-declares the hub onto its own snapshot's roots, so on a first boot a narrower set
    /// here is repaired while the test is still waiting for the graph; the publish that
    /// reuses a matching cache is the one path that declares nothing, which is why this test
    /// pays for two boots. The same holds for the diagnostics resident, whose publish also
    /// re-declares — hence the assertion that it never started.
    ///
    /// What the hub was ASKED to watch is the subject, not what the watcher took: an
    /// exhausted inotify limit leaves the declaration intact, so this gate cannot turn red
    /// on a machine's resource state.
    #[test]
    fn a_second_boot_over_a_matching_cache_declares_every_source_root_to_the_hub() {
        use crate::diagnostics_state::DiagnosticsStatus;

        let _env_lock = env_lock();
        let (_dir, workspace) = workspace_with_two_extensions();
        let graph_db = resolved_workspace_cache(&workspace).graph_db_path();

        let first = SharedState::workspace(workspace.clone()).expect("valid workspace project");
        wait_until_graph_ready(first.graph());
        first.shutdown();
        let built_at_before = crate::graph::test_support::meta_string(&graph_db, "built_at");

        let second = SharedState::workspace(workspace.clone()).expect("valid workspace project");
        wait_until_graph_ready(second.graph());

        let built_at_now = crate::graph::test_support::meta_string(&graph_db, "built_at");
        let snapshot = second.graph().snapshot().expect("a ready graph snapshots");
        let freshness = second.graph().freshness(&snapshot);
        let resident = second.diagnostics().status();
        let declared =
            second.change_hub().expect("a workspace boot owns a change hub").declared_targets();
        second.shutdown();

        // Warmth, in three parts because the two non-warm branches announce themselves
        // differently. A finished full build writes its meta before publishing `Ready`, so
        // the timestamp catches it. The stale-cache publish writes no meta at all — it goes
        // `Ready` and pre-claims the reload in one lock hold — so at the instant it is
        // observed only the published state tells it apart from a warm republish. Where its
        // catch-up lands then decides whether the declaration is repaired at all: a full
        // rebuild re-declares the roots, a body-only incremental one never does. Neither
        // outcome is safe to observe over, which is why this refuses both.
        assert_eq!(
            built_at_now, built_at_before,
            "the second boot must republish the cached build, not rebuild it",
        );
        assert!(!freshness.stale, "a republished cache is not stale: {:?}", freshness.reload);
        assert_eq!(freshness.reload, "none", "no catch-up reload may be in flight");
        // The resident's publish re-declares the hub too, and moves neither the graph's meta
        // nor its published state — the assertions above cannot see it. `SharedState::workspace`
        // leaves the resident idle on purpose (serve paths call `warm_start` themselves); this
        // says so out loud, so moving that call inside the constructor fails here instead of
        // silently disarming the check below.
        assert!(
            matches!(resident, DiagnosticsStatus::Idle),
            "the resident must not have started; it was {resident:?}",
        );

        let mut declared: Vec<(PathBuf, bool)> =
            declared.iter().map(|t| (canonical(&t.path), t.recursive)).collect();
        declared.sort();
        // Spelled out rather than recomputed from `project.source_roots()`: the derivation is
        // what is on trial, and checking a call against itself would pass whatever it returned.
        let mut expected = vec![
            (canonical(&workspace.join("src").join("cf")), true),
            (canonical(&workspace.join("ext-a")), true),
            (canonical(&workspace.join("ext-b")), true),
            (canonical(&workspace), false),
        ];
        expected.sort();
        assert_eq!(
            declared, expected,
            "the boot declares the configuration, every extension, and the workspace root",
        );
    }

    /// Overlapping roots behave in two DIFFERENT ways, and both must survive the boot: an
    /// extension INSIDE the configuration takes no root of its own (its files stay the
    /// configuration's, and must remain findable), while an extension CONTAINING the
    /// configuration registers normally and is told apart from it by the longest matching
    /// prefix. An implementation that rejects the containing root loses its files entirely —
    /// the configuration's walk never reaches them.
    #[test]
    fn overlapping_roots_keep_every_file_under_exactly_one_owner() {
        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let configuration = workspace.join("src").join("cf");
        fs::create_dir_all(&configuration).unwrap();
        fs::write(
            configuration.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &configuration,
            "Основной",
            "&НаСервере\nФункция СимволКонфигурации() Экспорт Возврат 1; КонецФункции\n",
        );
        // Inside the configuration root.
        let inner = configuration.join("inner-ext");
        fs::create_dir_all(&inner).unwrap();
        fs::write(
            inner.join("Configuration.xml"),
            "<Configuration><Name>Внутреннее</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &inner,
            "Внутренний",
            "&НаСервере\nФункция СимволВнутреннего() Экспорт Возврат 1; КонецФункции\n",
        );
        // Containing the configuration root: the workspace itself.
        fs::write(
            workspace.join("Configuration.xml"),
            "<Configuration><Name>Внешнее</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &workspace,
            "Внешний",
            "&НаСервере\nФункция СимволВнешнего() Экспорт Возврат 1; КонецФункции\n",
        );
        fs::write(
            workspace.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\n\
             extensions = [{ name = \"inner\", path = \"src/cf/inner-ext\" }, \
             { name = \"outer\", path = \".\" }]\n",
        )
        .unwrap();
        let project = crate::project::at(&workspace);
        assert!(project.is_ok(), "the fixture project parses: {:?}", project.err());

        let init = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("the local init produces an engine");
        let roots = init.engine.workspace_roots().expect("the boot configures a root table");

        let owner_of = |path: &std::path::Path| {
            let canonical = path.canonicalize().expect("the fixture file exists");
            roots.root_of(path, &canonical).expect("every fixture file has an owner")
        };

        let inner_module =
            inner.join("CommonModules").join("Внутренний").join("Ext").join("Module.bsl");
        let owner = owner_of(&inner_module);
        assert_eq!(
            owner.root_id,
            bsl_search::CONFIGURATION_ROOT_ID,
            "an extension inside the configuration takes no root of its own",
        );

        let configuration_module =
            configuration.join("CommonModules").join("Основной").join("Ext").join("Module.bsl");
        assert_eq!(
            owner_of(&configuration_module).root_id,
            bsl_search::CONFIGURATION_ROOT_ID,
            "the containing extension does not steal the configuration's own files",
        );

        let outer_module =
            workspace.join("CommonModules").join("Внешний").join("Ext").join("Module.bsl");
        assert_eq!(
            owner_of(&outer_module).root_id,
            ".",
            "a file outside the configuration belongs to the extension that contains it",
        );
    }

    /// A cold boot must index the declared extensions, not just the configuration — otherwise the
    /// extension reaches the index only if someone happens to edit it. And it must key each row by
    /// its own root: a `cfe` extension repeats the configuration's relative paths wholesale, so a
    /// writer that keys everything as the configuration silently overwrites one file with the
    /// other and serves one symbol where two exist.
    #[test]
    fn a_cold_boot_indexes_every_root_and_keeps_same_named_paths_apart() {
        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let configuration = workspace.join("src").join("cf");
        let extension = workspace.join("ext-a");
        for (root, name, symbol) in [
            (&configuration, "Конфа", "СимволКонфигурации"),
            (&extension, "Расш", "СимволРасширения"),
        ] {
            fs::create_dir_all(root).unwrap();
            fs::write(
                root.join("Configuration.xml"),
                format!("<Configuration><Name>{name}</Name></Configuration>"),
            )
            .unwrap();
            // The SAME relative path under both roots: this is the shape a `cfe` extension has.
            write_common_module_tree(
                root,
                "Общий",
                &format!("&НаСервере\nФункция {symbol}() Экспорт Возврат 1; КонецФункции\n"),
            );
        }
        fs::write(
            workspace.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\nextensions = [{ name = \"a\", path = \"ext-a\" }]\n",
        )
        .unwrap();

        let init = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("the local init produces an engine");

        let rows: Vec<(String, String)> = init
            .engine
            .store()
            .all_files_in_collection("code")
            .unwrap()
            .into_iter()
            .map(|(key, _hash)| (key.root_id, key.path))
            .collect();
        let module_rows: Vec<&(String, String)> =
            rows.iter().filter(|(_, path)| path.ends_with("Module.bsl")).collect();
        assert_eq!(
            module_rows.len(),
            2,
            "the same relative path under two roots is two rows: {rows:?}",
        );
        assert!(
            module_rows.iter().any(|(root_id, _)| root_id == "ext-a"),
            "the extension's row is keyed by its own root: {rows:?}",
        );

        for symbol in ["СимволКонфигурации", "СимволРасширения"] {
            let hits = init.engine.text_search(symbol, 10, Some("code")).unwrap();
            assert!(!hits.is_empty(), "a cold boot serves {symbol}: {rows:?}");
        }
    }

    /// A warm FTS store skips re-indexing entirely — that is what makes a restart cheap. But an
    /// extension declared while the daemon was down has NO rows at all, and "skip everything"
    /// would leave it out of the store until someone edits it. The cheap skip must therefore be
    /// per-root, not global: roots that already have rows stay untouched (the whole point), roots
    /// with none get indexed.
    #[test]
    fn a_warm_store_indexes_a_root_declared_while_it_was_down() {
        let _env_lock = env_lock();
        // No embedder: this is the FTS-only branch, the one that skips a warm store.
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let _embedding_model = EnvVarGuard::unset("EMBEDDING_MODEL");

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let configuration = workspace.join("src").join("cf");
        let extension = workspace.join("ext-a");
        for (root, name, symbol) in [
            (&configuration, "Конфа", "СимволКонфигурации"),
            (&extension, "Расш", "СимволРасширения"),
        ] {
            fs::create_dir_all(root).unwrap();
            fs::write(
                root.join("Configuration.xml"),
                format!("<Configuration><Name>{name}</Name></Configuration>"),
            )
            .unwrap();
            write_common_module_tree(
                root,
                "Общий",
                &format!("&НаСервере\nФункция {symbol}() Экспорт Возврат 1; КонецФункции\n"),
            );
        }
        // First boot: the extension is not declared yet, so the store warms up on the
        // configuration alone.
        fs::write(workspace.join("bsl-analyzer.toml"), "[source]\nroot = \"src/cf\"\n").unwrap();
        let cold = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("the first init produces an engine");
        assert!(cold.engine.chunk_count().unwrap() > 0, "the first boot warms the store");
        let configuration_rows = cold.engine.store().all_files_in_collection("code").unwrap();
        drop(cold);

        // The extension is declared while the daemon is down.
        fs::write(
            workspace.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\nextensions = [{ name = \"a\", path = \"ext-a\" }]\n",
        )
        .unwrap();
        let warm = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("the warm init produces an engine");

        let rows: Vec<(String, String)> = warm
            .engine
            .store()
            .all_files_in_collection("code")
            .unwrap()
            .into_iter()
            .map(|(key, _hash)| (key.root_id, key.path))
            .collect();
        assert!(
            rows.iter().any(|(root_id, _)| root_id == "ext-a"),
            "the newly declared root is indexed on the warm boot: {rows:?}",
        );
        // The point of the warm branch is that the already-indexed root is NOT rewritten.
        let warm_configuration: Vec<_> = warm
            .engine
            .store()
            .all_files_in_collection("code")
            .unwrap()
            .into_iter()
            .filter(|(key, _)| key.root_id.is_empty())
            .collect();
        assert_eq!(
            warm_configuration, configuration_rows,
            "the configuration's rows survive the warm boot untouched",
        );
    }

    /// Reaching the store is not the same as reaching the SEMANTIC index: `search_code` serves a
    /// query from its lexical half whenever the semantic half is empty, so "the symbol is found"
    /// stays true with zero vectors for the extension. What must hold is that the extension's
    /// chunks enter the embedding queue — that queue is exactly what the deferred pass drains.
    /// The vectors themselves are written by the background pass over HTTP and are not built here.
    #[test]
    fn a_deferred_boot_queues_the_extension_for_embedding() {
        let _env_lock = env_lock();
        // A configured embedder selects the semantic deferred branch; the URL is never dialed.
        let _embedding_enabled = EnvVarGuard::set("BSL_TEST_EMBEDDING", "1");
        let _embedding_url = EnvVarGuard::set("EMBEDDING_URL", "http://127.0.0.1:9/v1");
        let _embedding_model = EnvVarGuard::set("EMBEDDING_MODEL", "test-model");

        let (_dir, workspace) = workspace_with_two_extensions();
        let init = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("the local init produces an engine");
        assert!(init.engine.has_semantic(), "a configured embedder selects the semantic branch");

        let pending = init.engine.store().load_pending_embedding_documents("code").unwrap();
        let queued_roots: Vec<&str> =
            pending.iter().map(|(_, document)| document.root_id.as_str()).collect();
        for root_id in ["", "ext-a", "ext-b"] {
            assert!(
                queued_roots.contains(&root_id),
                "root {root_id:?} has chunks waiting for a vector: {queued_roots:?}",
            );
        }
        // Positive control on the count: an empty queue would satisfy a "no root is missing"
        // check written as a subset test, so the queue must actually hold chunks.
        assert!(pending.len() >= 3, "one chunk per module at least: {}", pending.len());
    }

    /// The negative control for the test above: with nothing declared, the very same file is NOT
    /// a workspace key. Without it, an implementation that keys every path under the sun would
    /// pass the positive check.
    #[test]
    fn a_file_outside_every_declared_root_has_no_key() {
        let _env_lock = env_lock();
        let (_dir, workspace) = workspace_with_two_extensions();
        fs::write(workspace.join("bsl-analyzer.toml"), "[source]\nroot = \"src/cf\"\n").unwrap();

        let init = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("the local init produces an engine");

        let extension_module = workspace
            .join("ext-a")
            .join("CommonModules")
            .join("Расширенный")
            .join("Ext")
            .join("Module.bsl");
        assert!(
            !init.engine.mark_workspace_path_dirty(&extension_module).unwrap(),
            "an undeclared tree stays outside the index",
        );
    }
}
