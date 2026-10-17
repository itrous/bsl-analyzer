use super::acquire::{acquire_engine_within, engine_lock_poisoned_error};
use super::render::format_baseline_ref;
use super::types::AcquireFailure;
use crate::baseline::{
    BaselineStatusProbe, ConfiguredBaselineStatus, ExternalBaselineService, ExternalBaselineState,
};
use crate::state::{OverlayWarmupState, SemanticRuntimeStatus, WorkspaceSearchMode};
use crate::tools::response::structured;
use bsl_search::{IndexProgress, SearchEngine};
use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use serde_json::json;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

/// Poll-back hint (ms) for a not-ready `search_code` response. Index build and overlay
/// warmup advance on a multi-second cadence, so a sub-second retry just spins.
const SEARCH_NOT_READY_RETRY_MS: u64 = 1500;

/// A structured "index not ready yet" envelope for `search_code`, mirroring the `graph`
/// tool's loading envelope so a programmatic poller reads JSON — a machine `status`, a
/// retry hint, and a live `progress.active` flag — instead of parsing prose. The human
/// `detail` (the upstream pending reason, verbatim) is preserved for people.
///
/// `progress.active` is always present so a poller can tell "a build is running" (going)
/// from "no build counting yet" (idle/pre-index); the numeric counters are attached ONLY
/// while `active`, because [`IndexProgress`] is never `reset()` — an inactive object can
/// still hold stale totals from a finished or failed attempt, and reporting those as
/// current progress would mislead. The pretty-JSON text mirror keeps the response readable.
pub(crate) fn search_not_ready(
    detail: &str,
    progress: &IndexProgress,
    action: &str,
) -> CallToolResult {
    let snapshot = progress.snapshot();
    let prog = crate::indexing::legacy_progress(snapshot.as_ref());
    structured(json!({
        "action": action,
        "schema_version": super::types::search_schema_version(action),
        "status": "not_ready",
        "detail": detail,
        "retry_after_ms": SEARCH_NOT_READY_RETRY_MS,
        "progress": prog,
    }))
}

/// Who follows the workspace for the index, and who reads its overlay backlog back — the
/// section a workspace `search status` ends with. The structured status is unchanged: these
/// facts reach a machine through `search_code`'s `freshness` instead.
pub(crate) fn append_workspace_changes(
    mut result: CallToolResult,
    drift_watch: Option<crate::tools::location::DriftWatch>,
    poll_cycle: Option<std::time::Duration>,
    backlog: &crate::state::overlay_backlog::BacklogState,
    slow_holds: u64,
) -> CallToolResult {
    use crate::state::overlay_backlog::BacklogState;
    let mut section = String::from("\nWorkspace changes:\n");
    if let Some(watch) = drift_watch {
        let _ = writeln!(section, "  Watch:    {}", watch.as_str());
    }
    if let Some(cycle) = poll_cycle {
        let _ = writeln!(
            section,
            "  Polling:  an edit that keeps its size and mtime is found within {}s, while the \
             polled files stay readable (plus the walk's own I/O)",
            cycle.as_secs()
        );
    }
    let backlog = match backlog {
        BacklogState::Running => "running".to_owned(),
        BacklogState::Backoff { until } => format!(
            "backing off ({}s left)",
            until.saturating_duration_since(std::time::Instant::now()).as_secs()
        ),
        BacklogState::Exhausted { since } => format!(
            "exhausted {}s ago; the changed files wait for a fresh change",
            since.elapsed().as_secs()
        ),
        BacklogState::Stopped { reason } => format!("stopped ({reason})"),
    };
    let _ = writeln!(section, "  Backlog:  {backlog}");
    if slow_holds > 0 {
        let _ = writeln!(
            section,
            "  Holds:    {slow_holds} batch publication(s) held the engine past their bound"
        );
    }
    if let Some(rmcp::model::ContentBlock::Text(text)) = result.content.first_mut() {
        text.text.push_str(&section);
    }
    result
}

/// The text the reference profile has always answered with while its index builds, kept
/// verbatim as the mirror of [`docs_not_ready`].
pub(super) const DOCS_INDEX_BUILDING_TEXT: &str =
    "Search index is being built, please try again in a moment.";

/// The reference profile's "index still building" answer. The sentence stays the text block;
/// the machine state rides alongside it so a docs consumer reads "retry, this is not an empty
/// result" from a field instead of matching the sentence — the same distinction `search_code`
/// gets from [`search_not_ready`]. No progress counters: this path holds no [`IndexProgress`]
/// handle, and inventing zeros would read as a stalled build.
pub(crate) fn docs_not_ready(action: &str) -> CallToolResult {
    crate::tools::response::structured_with_text(
        DOCS_INDEX_BUILDING_TEXT.to_owned(),
        json!({
            "action": action,
            "schema_version": super::types::search_schema_version(action),
            "status": "not_ready",
            "detail": DOCS_INDEX_BUILDING_TEXT,
            "retry_after_ms": SEARCH_NOT_READY_RETRY_MS,
        }),
    )
}

/// The `not_ready` retry envelope for a query that arrived while the deferred baseline
/// connect is still running. Distinct from the `baseline_unavailable` config errors: the
/// agent should simply retry in a few seconds, not go fix configuration or restart.
pub(crate) fn baseline_warming_not_ready(progress: &IndexProgress) -> CallToolResult {
    search_not_ready(
        "connecting to the shared PostgreSQL baseline (startup warmup)",
        progress,
        "search_code",
    )
}

#[allow(clippy::too_many_arguments, reason = "distinct status inputs, mirrored by _with_cap")]
pub fn search_status(
    profile: crate::McpProfile,
    engine: &crate::state::SharedSearchEngine,
    progress: &Arc<IndexProgress>,
    semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
    workspace_search_mode: WorkspaceSearchMode,
    overlay_warmup: OverlayWarmupState,
    configured_baseline: Option<ConfiguredBaselineStatus>,
    external_baseline: Option<Arc<ExternalBaselineService>>,
    baseline_pending: bool,
    initialization: EmptyIndex,
) -> Result<CallToolResult, McpError> {
    // Status is a polling primitive: agents call it in a loop to decide when search is usable,
    // so it answers within the cap and degrades to the "busy" note instead of waiting out a
    // long engine hold (overlay warmup, a slow embed). The trade-off is real: while something
    // holds the engine for longer than the cap, status reports no counts / overlay stats —
    // that is preferred over a poll that blocks for tens of seconds.
    search_status_with_cap(
        profile,
        engine,
        progress,
        semantic_runtime,
        workspace_search_mode,
        overlay_warmup,
        configured_baseline,
        external_baseline,
        baseline_pending,
        initialization,
        std::time::Duration::from_secs(2),
    )
}

/// What an empty engine slot means, as the profile that OWNS the initialization reports it. The
/// slot itself cannot say: an empty slot is a build in flight, an initialization that failed, or
/// — for a unit test's bare slot, or a profile that does not own this index — neither. Every
/// surface below branches on this one value, so prose and `state` can never disagree about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EmptyIndex {
    /// The initialization is still running: a build is coming, whatever a runtime status already
    /// says (a refused token profile is recorded before the engine is published).
    Initializing,
    /// The initialization ran, published nothing, and its OWNER declares the outcome terminal:
    /// waiting will not produce an index in this process.
    Failed,
    /// Not running and no declared failure: an empty slot nobody is filling for now.
    Idle,
}

/// The profile's own answer about an empty engine slot — one derivation per profile, read from
/// the source that DECLARES the verdict, never inferred from the runtime by the wrong profile:
///
/// - the workspace owns a boot thread (`workspace_search_initializing`) and a runtime whose
///   terminal states are `Failed`/`EmbeddingFailed` and `Stopped` (terminal by its own contract);
/// - the reference owns its load lifecycle, which records refusals the runtime never sees (an
///   unreadable project config, an unavailable baseline, a worker that never spawned) and is the
///   only place a verdict exists before the load finishes.
pub(crate) fn index_initialization(
    state: &crate::state::SharedState,
    profile: crate::McpProfile,
) -> EmptyIndex {
    use crate::state::ReferenceSearchLifecycle;
    match profile {
        crate::McpProfile::Workspace => {
            if state.search_initializing() {
                return EmptyIndex::Initializing;
            }
            let terminal = state
                .semantic_runtime()
                .lock()
                .map(|runtime| {
                    runtime.is_failed() || matches!(*runtime, SemanticRuntimeStatus::Stopped)
                })
                .unwrap_or(false);
            if terminal {
                EmptyIndex::Failed
            } else {
                EmptyIndex::Idle
            }
        }
        crate::McpProfile::Reference => match state.reference_lifecycle() {
            ReferenceSearchLifecycle::Uninitialized | ReferenceSearchLifecycle::Loading => {
                EmptyIndex::Initializing
            }
            ReferenceSearchLifecycle::Failed { .. } => EmptyIndex::Failed,
            ReferenceSearchLifecycle::Ready => EmptyIndex::Idle,
        },
    }
}

/// The status body, parameterized over the engine-acquire cap so tests can drive the timeout
/// (busy) branch without a multi-second sleep. Production goes through [`search_status`].
// Each argument is a distinct status input (engine, progress, runtime status, mode, warmup
// outcome, baselines, the empty slot's own verdict) plus the test-only acquire cap; bundling
// them into a context struct would only rename the same fields, so the one-over-limit arity is
// accepted here.
#[allow(clippy::too_many_arguments)]
pub(super) fn search_status_with_cap(
    profile: crate::McpProfile,
    engine: &crate::state::SharedSearchEngine,
    progress: &Arc<IndexProgress>,
    semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
    workspace_search_mode: WorkspaceSearchMode,
    overlay_warmup: OverlayWarmupState,
    configured_baseline: Option<ConfiguredBaselineStatus>,
    external_baseline: Option<Arc<ExternalBaselineService>>,
    baseline_pending: bool,
    initialization: EmptyIndex,
    engine_acquire_cap: std::time::Duration,
) -> Result<CallToolResult, McpError> {
    let mut out = String::new();

    let semantic_runtime = semantic_runtime
        .lock()
        .map_err(|e| McpError::internal_error(format!("semantic runtime lock error: {e}"), None))?
        .clone();
    let semantic_failure =
        overlay_warmup.embedding_failure().or_else(|| semantic_runtime.embedding_failure());
    // One non-blocking probe feeds every baseline-derived line below (summary wording, source
    // labels, the External baseline section). Status makes NO network round-trips of its own:
    // the probe serves the last completed background probe and re-kicks one when stale.
    let baseline_probe = external_baseline.as_ref().map(|service| service.probe_status_cached());
    // Cap the wait so status never hangs to the MCP client timeout while the overlay warmup or a
    // peer search holds the engine. On a genuine stall we still report the baseline + runtime
    // sections (which need no engine lock) and note the local index as busy.
    // A status probe answers whoever asks, with no request of its own to be cancelled by.
    let guard = match acquire_engine_within(
        engine,
        &tokio_util::sync::CancellationToken::new(),
        engine_acquire_cap,
        std::time::Duration::from_millis(25),
    ) {
        Ok(g) => Some(g),
        Err(AcquireFailure::Poisoned) => return Err(engine_lock_poisoned_error()),
        Err(AcquireFailure::TimedOut | AcquireFailure::Cancelled) => None,
    };
    // Measure how long the engine lock is held across the status build so a future stall is
    // diagnosable from `BSL_LOG=debug` alone (the release binary cannot be stack-traced).
    let guard_held_start = std::time::Instant::now();
    let engine_busy = guard.is_none();
    // Drive the summary's lexical-availability claim off the real engine state: "ready" only when
    // the engine is published and not held, so status never tells the agent the local index is
    // live while it is still building or a long operation holds the lock.
    let engine_state = if engine_busy {
        SummaryEngineState::Busy
    } else if guard.as_ref().is_some_and(|g| g.as_ref().is_some()) {
        SummaryEngineState::Ready
    } else {
        SummaryEngineState::Building
    };
    // An empty engine slot is not one state, and the two readings call for opposite advice.
    // `index_empty` names the slot; `index_terminal` narrows it to the case where NOTHING will
    // ever fill it — and the verdict is the profile's own ([`index_initialization`]), never
    // inferred here: a failed-looking runtime can sit right next to a healthy, in-flight build
    // (a token-profile refusal is recorded before the engine is published), while a terminal
    // one can be declared where the runtime says nothing at all (a refused reference config).
    let index_empty = guard.as_ref().is_some_and(|g| g.is_none());
    let index_terminal = index_empty && initialization == EmptyIndex::Failed;

    // Prepend a plain-language summary an agent can act on directly: the detailed field list
    // below is precise but hard to interpret, and a bare `Ready` + empty overlay is ambiguous
    // between "no local diffs" and "warmup failed". Synthesized from the runtime status, the
    // workspace mode, the overlay warmup outcome, the engine readiness, and whether a published
    // baseline is present.
    write_summary_block(
        &mut out,
        &semantic_runtime,
        &workspace_search_mode,
        &overlay_warmup,
        configured_baseline.as_ref(),
        external_baseline.as_ref(),
        baseline_probe.as_ref(),
        engine_state,
        index_terminal,
    );

    // Which roots are indexed at all. Without it an empty search over an extension reads
    // exactly like an extension that was never registered — and the two call for opposite
    // actions. The profile is passed in rather than inferred from the table: on a cold start
    // neither profile has one, so "no table" cannot tell them apart, and a reference index
    // (which has no source roots by construction) would report a permanent fault.
    if matches!(profile, crate::McpProfile::Workspace) {
        // Read from the live engine table. A graph publication transitions this table together
        // with every root-keyed carrier before it becomes visible here.
        let _ = writeln!(out, "Source roots (current search index):");
        match guard
            .as_ref()
            .and_then(|guard| guard.as_ref())
            .and_then(SearchEngine::workspace_roots)
        {
            Some(roots) => {
                for (id, declared) in roots.entries() {
                    let name = if id.is_empty() { "(configuration)" } else { id };
                    let _ = writeln!(out, "  {name}  {}", declared.display());
                }
            }
            // Two different facts, and telling a reader "none" for either would be a lie about
            // what this workspace indexes.
            None if engine_busy => {
                let _ = writeln!(
                    out,
                    "  not read (a long operation holds the index; the roots are unchanged)"
                );
            }
            // An empty engine slot is not one state. The index may still be building, or its
            // initialization may have failed — and on that path nothing will publish a table
            // later, so telling the reader to wait would be advice to wait forever. The verdict
            // comes from the profile that owns the initialization, so a build in flight (which a
            // failed-looking runtime status can accompany) stays on the waiting side of it.
            None if index_terminal => {
                let _ = writeln!(
                    out,
                    "  unavailable (search index initialization failed; see the runtime status \
                     above — waiting will not publish them)"
                );
            }
            None => {
                let _ = writeln!(out, "  not published yet (the index is still building)");
            }
        }
        let _ = writeln!(out);
    }

    // Pending is only reachable on the postgres path (a Connect plan exists for no other
    // backend), so the placeholder backend below is accurate by construction.
    if baseline_pending && configured_baseline.is_none() {
        let _ = writeln!(out, "Configured baseline:");
        let _ = writeln!(out, "  Backend:  postgres");
        let _ = writeln!(
            out,
            "  Status:   connecting to the shared baseline (startup warmup) — retry shortly"
        );
        let _ = writeln!(out);
    }

    if let Some(configured_baseline) = configured_baseline.as_ref() {
        let _ = writeln!(out, "Configured baseline:");
        let _ = writeln!(out, "  Backend:  {}", configured_baseline.backend);
        let _ = writeln!(out, "  Select:   {}", configured_baseline.selection);
        let _ = writeln!(
            out,
            "  Status:   {}",
            configured_baseline.issue.as_deref().unwrap_or("ready")
        );
        if let Some(support) = configured_baseline.support.as_ref() {
            let _ = writeln!(out, "  Support:  {}", support.state.as_str());
            let _ = writeln!(out, "  Reason:   {}", support.reason);
            let _ = writeln!(
                out,
                "  Policy:   stale after {}d, expire after {}d",
                support.stale_after_days, support.expire_after_days
            );
            if matches!(support.state, project_model::SearchBaselineSupportState::Expired) {
                let _ = writeln!(out, "  Action:   update the branch from develop and restart MCP");
            }
        }
        let _ = writeln!(out);
    }

    if let Some(engine) = guard.as_ref().and_then(|g| g.as_ref()) {
        let counts_start = std::time::Instant::now();
        let files = engine.file_count().unwrap_or(0);
        let chunks = engine.chunk_count().unwrap_or(0);
        let vectors = engine.vector_count();
        let semantic = engine.has_semantic();
        tracing::debug!(
            elapsed_ms = counts_start.elapsed().as_millis() as u64,
            "search.status: engine counts (file/chunk/vector/semantic)"
        );

        let embed_code_start = std::time::Instant::now();
        let code_vectors = engine.embedding_count_by_collection("code").unwrap_or(0);
        tracing::debug!(
            elapsed_ms = embed_code_start.elapsed().as_millis() as u64,
            "search.status: embedding_count_by_collection code"
        );
        let embed_platform_start = std::time::Instant::now();
        let platform_vectors = engine.embedding_count_by_collection("platform").unwrap_or(0);
        tracing::debug!(
            elapsed_ms = embed_platform_start.elapsed().as_millis() as u64,
            "search.status: embedding_count_by_collection platform"
        );

        let search_state = match &semantic_runtime {
            SemanticRuntimeStatus::Failed(_) | SemanticRuntimeStatus::EmbeddingFailed(_) => {
                "ready (semantic runtime failed)"
            }
            // Honest about the window the watcher/overlay sync briefly holds the engine lock:
            // a concurrent search_code now queues behind that hold (it blocks on the engine
            // mutex rather than failing) and returns real results once the sync frees the lock,
            // so the agent should expect a brief wait, not a "warming up" error.
            SemanticRuntimeStatus::OverlaySyncing => {
                "ready — overlay syncing (a concurrent search_code briefly queues behind the sync, then returns results)"
            }
            SemanticRuntimeStatus::Indexing => {
                "ready (lexical) — semantic index building in background"
            }
            SemanticRuntimeStatus::Stopped => {
                "ready (lexical) — semantic indexing stopped with the daemon"
            }
            _ => "ready",
        };
        let _ = writeln!(out, "Search index: {search_state}");
        let _ = writeln!(out, "  Files:    {files}");
        let _ = writeln!(out, "  Chunks:   {chunks}");
        let _ = writeln!(
            out,
            "  Vectors:  {vectors} (code: {code_vectors}, platform: {platform_vectors})"
        );
        let semantic_status = match &semantic_runtime {
            SemanticRuntimeStatus::Disabled => {
                if semantic {
                    "available".to_owned()
                } else {
                    "not configured (set EMBEDDING_URL)".to_owned()
                }
            }
            SemanticRuntimeStatus::OverlaySyncing => match workspace_search_mode {
                WorkspaceSearchMode::PostgresRemoteOverlay => {
                    "syncing local overlay embeddings against remote baseline".to_owned()
                }
                WorkspaceSearchMode::SqliteLocal => "syncing local semantic index".to_owned(),
            },
            SemanticRuntimeStatus::Indexing => {
                "building local semantic index in background".to_owned()
            }
            SemanticRuntimeStatus::Ready => match workspace_search_mode {
                WorkspaceSearchMode::PostgresRemoteOverlay => {
                    if semantic {
                        "available (remote baseline semantic + local overlay only)".to_owned()
                    } else {
                        "not configured (set EMBEDDING_URL)".to_owned()
                    }
                }
                WorkspaceSearchMode::SqliteLocal => {
                    if semantic {
                        "available (local sqlite + local overlay)".to_owned()
                    } else {
                        "not configured (set EMBEDDING_URL)".to_owned()
                    }
                }
            },
            SemanticRuntimeStatus::Failed(_) => "failed (inspect status)".to_owned(),
            SemanticRuntimeStatus::Stopped => "stopped with the daemon".to_owned(),
            SemanticRuntimeStatus::EmbeddingFailed(failure) => format!("failed ({failure})"),
        };
        let _ = writeln!(out, "  Semantic: {semantic_status}");
        let _ = writeln!(out, "  FTS:      {}", if chunks > 0 { "available" } else { "empty" });
        let _ = writeln!(out, "  Collections: code, platform");
        let overlay_stats_start = std::time::Instant::now();
        let workspace_overlay = engine
            .workspace_overlay_stats_read_only()
            .map_err(|e| McpError::internal_error(format!("overlay status error: {e}"), None))?;
        tracing::debug!(
            elapsed_ms = overlay_stats_start.elapsed().as_millis() as u64,
            "search.status: workspace_overlay_stats"
        );
        if let Some(source) = external_baseline.as_ref() {
            match source.corpus() {
                bsl_search::CorpusId::WorkspaceCode => {
                    // The display label only needs to know whether a baseline snapshot resolved
                    // on the last probe — worth neither a PG round-trip nor a document load here.
                    let code_lexical_source = match baseline_probe.as_ref() {
                        Some(BaselineStatusProbe::Cached(cached))
                            if matches!(
                                cached.status.state,
                                ExternalBaselineState::Ready { .. }
                            ) =>
                        {
                            "external baseline + local overlay"
                        }
                        Some(BaselineStatusProbe::Pending) => {
                            "external baseline (status probe pending) + local overlay"
                        }
                        _ => "local sqlite + local overlay",
                    };
                    let _ = writeln!(out, "  Code lexical source: {code_lexical_source}");
                    let code_semantic_source = match (
                        &semantic_runtime,
                        workspace_search_mode.clone(),
                    ) {
                        (SemanticRuntimeStatus::Disabled, _) => {
                            "not configured (set EMBEDDING_URL)".to_owned()
                        }
                        (SemanticRuntimeStatus::Indexing, _) => {
                            "local sqlite semantic index building in background".to_owned()
                        }
                        (
                            SemanticRuntimeStatus::OverlaySyncing,
                            WorkspaceSearchMode::PostgresRemoteOverlay,
                        ) => "remote baseline semantic + local overlay sync in progress".to_owned(),
                        (
                            SemanticRuntimeStatus::OverlaySyncing,
                            WorkspaceSearchMode::SqliteLocal,
                        ) => "local sqlite + local overlay sync in progress".to_owned(),
                        (
                            SemanticRuntimeStatus::Ready,
                            WorkspaceSearchMode::PostgresRemoteOverlay,
                        ) => {
                            if baseline_probe_unreachable(baseline_probe.as_ref()) {
                                "shared baseline not currently reachable (see the External baseline section); local overlay only".to_owned()
                            } else if matches!(
                                baseline_probe.as_ref(),
                                Some(BaselineStatusProbe::Pending)
                            ) {
                                "remote baseline semantic (status probe pending) + local overlay only".to_owned()
                            } else {
                                "remote baseline semantic + local overlay only".to_owned()
                            }
                        }
                        (SemanticRuntimeStatus::Ready, WorkspaceSearchMode::SqliteLocal) => {
                            if semantic {
                                "local sqlite + local overlay".to_owned()
                            } else {
                                "not configured (set EMBEDDING_URL)".to_owned()
                            }
                        }
                        (SemanticRuntimeStatus::Failed(_), _) => "failed".to_owned(),
                        (SemanticRuntimeStatus::Stopped, _) => {
                            "local sqlite semantic indexing stopped with the daemon".to_owned()
                        }
                        (SemanticRuntimeStatus::EmbeddingFailed(failure), _) => {
                            format!("failed ({failure})")
                        }
                    };
                    let _ = writeln!(out, "  Code semantic source: {code_semantic_source}");
                }
                bsl_search::CorpusId::Reference => {
                    // Same probe-derived label as the workspace arm: no PG round-trip and no
                    // reference-corpus document load just to word a display line.
                    let docs_lexical_source = match baseline_probe.as_ref() {
                        Some(BaselineStatusProbe::Cached(cached))
                            if matches!(
                                cached.status.state,
                                ExternalBaselineState::Ready { .. }
                            ) =>
                        {
                            "external baseline"
                        }
                        Some(BaselineStatusProbe::Pending) => {
                            "external baseline (status probe pending)"
                        }
                        _ => "local sqlite",
                    };
                    let _ = writeln!(out, "  Docs lexical source: {docs_lexical_source}");
                    let docs_semantic_source = if semantic {
                        "local semantic cache of external baseline"
                    } else {
                        "not configured (set EMBEDDING_URL)"
                    };
                    let _ = writeln!(out, "  Docs semantic source: {docs_semantic_source}");
                }
                bsl_search::CorpusId::Custom(_) => {}
            }
        } else if workspace_overlay.is_some() {
            let _ = writeln!(out, "  Code lexical source: local sqlite + local overlay");
            let code_semantic_source = if semantic {
                "local sqlite + local overlay"
            } else {
                "not configured (set EMBEDDING_URL)"
            };
            let _ = writeln!(out, "  Code semantic source: {code_semantic_source}");
        } else {
            let _ = writeln!(out, "  Docs lexical source: local sqlite");
            let docs_semantic_source =
                if semantic { "local sqlite" } else { "not configured (set EMBEDDING_URL)" };
            let _ = writeln!(out, "  Docs semantic source: {docs_semantic_source}");
        }

        if let Some(overlay) = workspace_overlay {
            // Counts only — the view itself is every indexed document in memory, and the
            // two numbers below are all this report takes from it.
            let resolve_local_start = std::time::Instant::now();
            let local_view = engine.resolve_workspace_code_view_counts().map_err(|e| {
                McpError::internal_error(format!("resolved workspace view error: {e}"), None)
            })?;
            tracing::debug!(
                elapsed_ms = resolve_local_start.elapsed().as_millis() as u64,
                "search.status: resolve_workspace_code_view_counts (local store)"
            );
            if let Some(view) = local_view {
                let _ = writeln!(out);
                let _ = writeln!(out, "Resolved workspace view: ready");
                let _ = writeln!(out, "  Baseline: {}", format_baseline_ref(view.baseline()));
                let _ = writeln!(out, "  Files:    {}", view.files());
                let _ = writeln!(out, "  Chunks:   {}", view.chunks());
            }

            let _ = writeln!(out);
            let _ = writeln!(out, "Workspace overlay: enabled");
            let _ = writeln!(out, "  Files:    {}", overlay.overlay_files);
            let _ = writeln!(out, "  Deleted:  {}", overlay.deleted_files);
            let _ = writeln!(out, "  Hidden:   {}", overlay.hidden_paths);
            let _ = writeln!(out, "  Chunks:   {}", overlay.lexical_chunks);
            let _ = writeln!(out, "  Semantic: {}", overlay.semantic_chunks);
            let _ = writeln!(out, "  Cached embeddings: {}", overlay.cached_embeddings);
            let _ = writeln!(
                out,
                "  Watcher mode: {}",
                if overlay.watcher_mode {
                    "enabled"
                } else {
                    // False until a feed proves itself, so a hub that armed late over a quiet
                    // tree reads as "not yet proven", never as "no hub at all".
                    "off (no change feed has proved itself to the overlay yet; the overlay is as \
                     of its last publication)"
                }
            );
            let _ = writeln!(out, "  Pending dirty paths: {}", overlay.pending_dirty_paths);
        }
    } else if engine_busy {
        let _ = writeln!(out, "Local index: busy (overlay syncing)");
    } else if index_terminal {
        let _ = writeln!(
            out,
            "Search index: unavailable (initialization failed; waiting will not publish it)"
        );
    } else {
        let _ = writeln!(out, "Search index: building (background initialization in progress)");
    }

    // Everything below is engine-free; release the lock now so a concurrent search never
    // queues behind status rendering. `index_empty`/`index_terminal` were captured above,
    // while the guard was held.
    let embedding_profile = guard
        .as_ref()
        .and_then(|guard| guard.as_ref())
        .and_then(SearchEngine::embedding_profile)
        .map(|(wire_model, storage_identity, dimension)| {
            (wire_model.to_owned(), storage_identity.to_owned(), dimension)
        });
    if !engine_busy {
        tracing::debug!(
            elapsed_ms = guard_held_start.elapsed().as_millis() as u64,
            "search.status: total time holding engine guard"
        );
    }
    drop(guard);

    if let Some(external_baseline) = external_baseline {
        let _ = writeln!(out);
        let _ = writeln!(out, "External baseline: configured");
        match baseline_probe.as_ref() {
            None | Some(BaselineStatusProbe::Pending) => {
                let _ = writeln!(out, "  Backend:  postgres");
                let _ = writeln!(out, "  Schema:   {}", external_baseline.schema_for_status());
                let _ = writeln!(out, "  Select:   {}", external_baseline.selection());
                let _ = writeln!(
                    out,
                    "  Status:   probing the shared baseline in the background — retry shortly"
                );
            }
            Some(BaselineStatusProbe::Cached(cached)) => {
                let status = &cached.status;
                let _ = writeln!(out, "  Backend:  {}", status.backend);
                let _ = writeln!(out, "  Schema:   {}", status.schema);
                let _ = writeln!(out, "  Select:   {}", status.selection);
                if let Some(resolved) = status.resolved.as_deref() {
                    let _ = writeln!(out, "  Resolved: {}", resolved);
                }
                let _ = writeln!(
                    out,
                    "  Probed:   {}s ago (served from cache; re-probed in background when stale)",
                    cached.age().as_secs()
                );
                match &status.state {
                    ExternalBaselineState::Ready { snapshot_id, fingerprint, documents, files } => {
                        let _ = writeln!(out, "  Status:   ready");
                        let _ = writeln!(out, "  Snapshot: {}", snapshot_id);
                        let _ = writeln!(out, "  Files:    {}", files);
                        let _ = writeln!(out, "  Chunks:   {}", documents);
                        if let Some(fingerprint) = fingerprint.as_deref() {
                            let _ = writeln!(
                                out,
                                "  Fingerprint: {}",
                                shorten_fingerprint(fingerprint)
                            );
                        }
                        match external_baseline.corpus() {
                            bsl_search::CorpusId::WorkspaceCode => {
                                // "Ready" already implies the snapshot resolved during the
                                // probe; searches re-resolve fresh on their own path, so the
                                // line reports probe-time truth without another round-trip.
                                let _ = writeln!(out, "  Resolved view: ready (as of last probe)");
                            }
                            bsl_search::CorpusId::Reference => {
                                if let Some(local_fingerprint) =
                                    external_baseline.local_reference_fingerprint()
                                {
                                    let freshness = match fingerprint.as_deref() {
                                        Some(shared) if shared == local_fingerprint => "up to date",
                                        Some(_) => "stale",
                                        None => "unknown",
                                    };
                                    let _ = writeln!(out, "  Freshness: {}", freshness);
                                    let _ = writeln!(
                                        out,
                                        "  Local fingerprint: {}",
                                        shorten_fingerprint(&local_fingerprint)
                                    );
                                }
                                let _ = writeln!(out, "  Resolved view: ready (as of last probe)");
                            }
                            bsl_search::CorpusId::Custom(_) => {}
                        }
                    }
                    ExternalBaselineState::Missing => {
                        let _ = writeln!(out, "  Status:   not found");
                    }
                    ExternalBaselineState::Error(error) => {
                        let _ = writeln!(out, "  Status:   error");
                        let _ = writeln!(out, "  Error:    {}", error);
                    }
                }
            }
        }
    }

    if index_empty && !progress.is_active() {
        if index_terminal {
            // "Pending" here would be the same lie the header just stopped telling: nothing is
            // pending, and no pass will produce the counters this line exists to announce.
            let _ = writeln!(
                out,
                "\nIndexing failed: initialization failed, so no pass will run and there are no \
                 counters to report"
            );
        } else {
            let _ = writeln!(out, "\nIndexing pending: initializing (no live counters yet)");
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Note: code snippets are secret-redacted (credential-like values shown as ***); \
         treat snippet text as sanitized, not byte-exact."
    );

    let state = match engine_state {
        SummaryEngineState::Busy => "busy",
        // The same discriminator the prose uses, so a poller and a reader never read different
        // facts off one answer: a build in flight is `loading` even when the runtime already
        // carries a failure, and only the profile's own terminal verdict says `failed`.
        SummaryEngineState::Building if index_terminal => "failed",
        SummaryEngineState::Building => "loading",
        SummaryEngineState::Ready => "ready",
    };
    let mut body = json!({
        "action": "status",
        "schema_version": super::types::search_schema_version("status"),
        "profile": profile.as_str(),
        "state": state,
    });
    if let Some((wire_model, storage_identity, dimension)) = embedding_profile {
        body["embedding_profile"] = json!({
            "wire_model": wire_model,
            "storage_identity": storage_identity,
            "dimension": dimension,
        });
    }
    if let Some(failure) = semantic_failure {
        body["semantic_failure"] = json!(failure);
    }
    Ok(crate::tools::response::structured_with_text(out, body))
}

/// Write the plain-language Summary block an LLM agent reads first. It states, in three to four
/// short lines: what the working tree of lexical search reflects, where semantic ([S]) results
/// come from, and the overlay warmup outcome (so "no local diffs" is never confused with a failed
/// warmup). All ASCII so the output stays portable across terminals.
/// Engine readiness for the summary's lexical-availability line, derived from the (capped) engine
/// acquire: `Ready` only when the engine is published and the lock was obtained, so status never
/// claims the local index is live while it is still building or a long operation holds the lock.
#[derive(Clone, Copy)]
enum SummaryEngineState {
    Ready,
    Busy,
    Building,
}

/// The summary's claim about baseline-served semantic search must not outrun the probe:
/// before the first background probe lands the availability is unconfirmed, and a probe
/// that ended in Missing/Error means the shared baseline is not currently reachable.
fn baseline_semantic_summary(
    baseline_selection: &str,
    baseline_probe: Option<&BaselineStatusProbe>,
) -> String {
    match baseline_probe {
        Some(BaselineStatusProbe::Pending) => format!(
            "{baseline_selection} baseline configured; first background status probe still running — retry shortly."
        ),
        Some(BaselineStatusProbe::Cached(cached))
            if !matches!(cached.status.state, ExternalBaselineState::Ready { .. }) =>
        {
            "shared baseline is not currently reachable (see the External baseline section)."
                .to_owned()
        }
        _ => format!("served from the {baseline_selection} baseline (published index)."),
    }
}

/// True when the last completed probe says the shared baseline is Missing or errored —
/// the one case where summary lines must stop claiming baseline-backed availability.
/// A pending probe is merely unconfirmed, not unreachable.
fn baseline_probe_unreachable(baseline_probe: Option<&BaselineStatusProbe>) -> bool {
    matches!(
        baseline_probe,
        Some(BaselineStatusProbe::Cached(cached))
            if !matches!(cached.status.state, ExternalBaselineState::Ready { .. })
    )
}

#[allow(clippy::too_many_arguments, reason = "distinct status inputs, mirrors search_status")]
fn write_summary_block(
    out: &mut String,
    semantic_runtime: &SemanticRuntimeStatus,
    workspace_search_mode: &WorkspaceSearchMode,
    overlay_warmup: &OverlayWarmupState,
    configured_baseline: Option<&ConfiguredBaselineStatus>,
    external_baseline: Option<&Arc<ExternalBaselineService>>,
    baseline_probe: Option<&BaselineStatusProbe>,
    engine_state: SummaryEngineState,
    index_terminal: bool,
) {
    // Reference profile = an external docs baseline (no local workspace code overlay). Its wording
    // differs: it indexes reference docs, not a working tree, and has no local overlay to (re)build.
    let is_reference = external_baseline
        .is_some_and(|source| matches!(source.corpus(), bsl_search::CorpusId::Reference));
    let has_baseline = external_baseline.is_some();
    let is_overlay_mode =
        matches!(workspace_search_mode, WorkspaceSearchMode::PostgresRemoteOverlay);

    let _ = writeln!(out, "Summary:");

    let lexical_line = match engine_state {
        SummaryEngineState::Busy => {
            "temporarily unavailable - a background operation holds the index; retry shortly."
        }
        // A terminal initialization failure is told apart from a build in flight here too: the
        // summary is the first thing the agent reads, and "still building" beside a runtime
        // status that says the initialization failed sends it to wait for nothing.
        SummaryEngineState::Building if index_terminal => {
            "unavailable - search index initialization failed; waiting will not publish it."
        }
        SummaryEngineState::Building => "index still building; not ready yet.",
        SummaryEngineState::Ready if is_reference => {
            "reference docs index (platform documentation)."
        }
        SummaryEngineState::Ready => {
            "reflects the current working tree (baseline committed code + live local edits via the file watcher)."
        }
    };
    let _ = writeln!(out, "  Lexical search: {lexical_line}");

    let baseline_selection =
        configured_baseline.map(|b| b.selection.as_str()).unwrap_or("configured");
    let semantic_line = match (semantic_runtime, workspace_search_mode) {
        (SemanticRuntimeStatus::Disabled, _) => "not configured (set EMBEDDING_URL).".to_owned(),
        (SemanticRuntimeStatus::Failed(_), _) => {
            if baseline_probe_unreachable(baseline_probe) {
                "semantic runtime reported a failure and the shared baseline is not currently reachable (see below).".to_owned()
            } else {
                "baseline available; semantic runtime reported a failure (see below).".to_owned()
            }
        }
        (SemanticRuntimeStatus::Stopped, _) => {
            "semantic indexing stopped with the daemon; nothing further will be embedded."
                .to_owned()
        }
        (SemanticRuntimeStatus::EmbeddingFailed(failure), _) => {
            format!("embedding failed ({failure}).")
        }
        (SemanticRuntimeStatus::OverlaySyncing, _) => {
            if baseline_probe_unreachable(baseline_probe) {
                "local overlay still syncing; the shared baseline is not currently reachable (see the External baseline section).".to_owned()
            } else {
                "baseline available; local overlay still syncing.".to_owned()
            }
        }
        (SemanticRuntimeStatus::Indexing, _) => {
            "local semantic index building in background.".to_owned()
        }
        (SemanticRuntimeStatus::Ready, WorkspaceSearchMode::PostgresRemoteOverlay) => {
            baseline_semantic_summary(baseline_selection, baseline_probe)
        }
        (SemanticRuntimeStatus::Ready, WorkspaceSearchMode::SqliteLocal) => {
            if has_baseline {
                baseline_semantic_summary(baseline_selection, baseline_probe)
            } else {
                "local semantic index.".to_owned()
            }
        }
    };
    let _ = writeln!(out, "  Semantic ([S]) search: {semantic_line}");

    // The local overlay (and its startup-rebuild note) only exist in PostgresRemoteOverlay mode.
    // For SqliteLocal / reference profiles there is no remote baseline to overlay, so `Pending`
    // there is the permanent initial value, not an in-progress sync - omit the line entirely.
    if is_overlay_mode {
        // `OverlaySyncing` lives in the runtime status, not the warmup outcome; surface it here so
        // the line is never stale-`Pending` while the runtime says the sync is in flight.
        let overlay_line = if matches!(semantic_runtime, SemanticRuntimeStatus::OverlaySyncing) {
            "building (indexing local diffs against the baseline)...".to_owned()
        } else {
            match overlay_warmup {
                // `Pending` is the permanent initial value on a failed initialization, not an
                // in-progress sync: nothing is indexing local diffs when nothing published a
                // local index. Wording it as "building" told the reader to wait for a pass that
                // is not running.
                OverlayWarmupState::Pending if index_terminal => {
                    "not built (search index initialization failed; no overlay pass is running, \
                     and waiting will not rebuild it)."
                        .to_owned()
                }
                OverlayWarmupState::Pending => {
                    "building (indexing local diffs against the baseline)...".to_owned()
                }
                OverlayWarmupState::NoLocalDiffs => {
                    "none needed - working tree matches the baseline, so [S] comes entirely from the baseline.".to_owned()
                }
                OverlayWarmupState::Synced { overlay_files, embedded } => format!(
                    "{overlay_files} locally-changed file(s) indexed ({embedded} chunks); their [S] reflects local edits."
                ),
                OverlayWarmupState::Incomplete { unreadable, canonical_fallbacks, read_failures, persist_failed } => {
                    let store_note =
                        if *persist_failed { ", fingerprint persist FAILED (stale rows on disk)" } else { "" };
                    format!(
                        "built from an INCOMPLETE pass ({unreadable} unreadable subtree(s), {canonical_fallbacks} unresolved spelling(s), {read_failures} unread file(s){store_note}); what was seen is serving, local edits keep applying incrementally, and stale entries may linger until a clean rescan."
                    )
                }
                OverlayWarmupState::Superseded => {
                    "superseded by a concurrent full publication; a fresh pass follows.".to_owned()
                }
                OverlayWarmupState::Failed(reason) => format!(
                    "not built (warmup failed: {reason}); [S] still served by the baseline. The retry driver repeats the pass automatically."
                ),
                OverlayWarmupState::EmbeddingFailed(failure) => format!(
                    "not built (warmup failed: {failure}); search_code uses lexical fallback. The retry driver repeats the pass after new workspace changes."
                ),
                OverlayWarmupState::Skipped(reason) => format!("disabled ({reason})."),
            }
        };
        let _ = writeln!(out, "  Local overlay semantic: {overlay_line}");
        let _ = writeln!(
            out,
            "  Note: local-only edits are searchable lexically immediately; their semantic index is (re)built at MCP startup, and a retry driver catches up incomplete or failed passes automatically (with backoff)."
        );
    }

    let _ = writeln!(out);
}

fn shorten_fingerprint(fingerprint: &str) -> &str {
    fingerprint.get(..12).unwrap_or(fingerprint)
}

#[cfg(test)]
mod workspace_changes_tests {
    use super::append_workspace_changes;
    use crate::state::overlay_backlog::BacklogState;
    use crate::tools::location::DriftWatch;
    use rmcp::model::{CallToolResult, ContentBlock};
    use std::time::{Duration, Instant};

    fn status() -> CallToolResult {
        let mut result = CallToolResult::success(vec![ContentBlock::text("Search status: ready")]);
        result.structured_content = Some(serde_json::json!({"action": "status"}));
        result
    }

    fn text(result: &CallToolResult) -> String {
        result.content[0].as_text().expect("text").text.clone()
    }

    /// Every state the overlay's backlog owner can be in reads differently in `search status`,
    /// next to who watches the workspace and how long a poll can miss an edit; the structured
    /// status is not touched.
    #[test]
    fn each_backlog_state_reads_differently_in_the_status() {
        let states = [
            BacklogState::Running,
            BacklogState::Backoff { until: Instant::now() + Duration::from_secs(90) },
            BacklogState::Exhausted { since: Instant::now() },
            BacklogState::Stopped { reason: "the lease went terminal" },
        ];
        let lines: Vec<String> = states
            .iter()
            .map(|state| {
                let result = append_workspace_changes(status(), None, None, state, 0);
                assert_eq!(result.structured_content, status().structured_content);
                let text = text(&result);
                text.lines()
                    .find(|line| line.contains("Backlog:"))
                    .expect("a backlog line")
                    .to_owned()
            })
            .collect();
        for (i, line) in lines.iter().enumerate() {
            for other in &lines[i + 1..] {
                assert_ne!(line, other);
            }
        }
        assert!(lines[1].contains("backing off") && lines[2].contains("exhausted"));
        assert!(lines[3].contains("the lease went terminal"));

        let polled = append_workspace_changes(
            status(),
            Some(DriftWatch::Polling),
            Some(Duration::from_secs(60)),
            &BacklogState::Running,
            2,
        );
        let polled = text(&polled);
        assert!(polled.starts_with("Search status: ready\n"), "{polled}");
        assert!(polled.contains("Watch:    polling") && polled.contains("within 60s"), "{polled}");
        assert!(polled.contains("Holds:    2 batch"), "{polled}");
        assert!(!text(&append_workspace_changes(status(), None, None, &BacklogState::Running, 0))
            .contains("Holds:"));
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::unreachable_workspace_service;
    use super::{
        baseline_warming_not_ready, index_initialization, search_status, search_status_with_cap,
        EmptyIndex,
    };
    use crate::baseline::{
        ConfiguredBaselineStatus, ExternalBaselineState, ExternalBaselineStatus,
    };
    use crate::state::{OverlayWarmupState, SemanticRuntimeStatus, WorkspaceSearchMode};
    use bsl_search::{Document, IndexProgress, SearchEngine};
    use rmcp::model::ErrorCode;
    use std::fs;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    #[test]
    fn payload_mcp_contract_status_preserves_lexical_state_and_selects_the_current_owner() {
        use bsl_search::{EmbeddingFailure, EmbeddingFailureCode};
        let dir = tempdir().unwrap();
        let engine = crate::state::shared_engine(Some(
            SearchEngine::fts_only(&dir.path().join("search.db")).unwrap(),
        ));
        let main_failure = EmbeddingFailure::new(EmbeddingFailureCode::EmbeddingTimeout);
        let overlay_failure = EmbeddingFailure {
            code: EmbeddingFailureCode::EmbeddingInputTooLarge,
            request_bytes: Some(256),
            max_request_bytes: Some(128),
        };
        let runtime = Arc::new(Mutex::new(SemanticRuntimeStatus::EmbeddingFailed(main_failure)));
        // The empty-slot verdict follows the runtime this profile owns, as production derives it:
        // the rendering below is what the test is about, and it must be driven the same way.
        let run = |profile, overlay| {
            let initialization = {
                let runtime = runtime.lock().unwrap();
                if runtime.is_failed() || matches!(*runtime, SemanticRuntimeStatus::Stopped) {
                    EmptyIndex::Failed
                } else {
                    EmptyIndex::Idle
                }
            };
            search_status_with_cap(
                profile,
                &engine,
                &IndexProgress::new(),
                &runtime,
                WorkspaceSearchMode::SqliteLocal,
                overlay,
                None,
                None,
                false,
                initialization,
                Duration::ZERO,
            )
            .unwrap()
        };

        for profile in [crate::McpProfile::Workspace, crate::McpProfile::Reference] {
            let result = run(profile, OverlayWarmupState::Pending);
            let body = result.structured_content.unwrap();
            assert_eq!(body["schema_version"], "4");
            assert_eq!(body["state"], "ready");
            assert_eq!(body["semantic_failure"], serde_json::json!(main_failure));
            assert!(result.content[0].as_text().unwrap().text.contains("embedding_timeout"));
        }

        let result =
            run(crate::McpProfile::Workspace, OverlayWarmupState::EmbeddingFailed(overlay_failure));
        assert_eq!(
            result.structured_content.unwrap()["semantic_failure"],
            serde_json::json!(overlay_failure)
        );
        let guard = engine.lock().unwrap();
        let body =
            run(crate::McpProfile::Workspace, OverlayWarmupState::EmbeddingFailed(overlay_failure))
                .structured_content
                .unwrap();
        assert_eq!(body["state"], "busy");
        assert_eq!(body["semantic_failure"], serde_json::json!(overlay_failure));
        drop(guard);

        *engine.lock().unwrap() = None;
        let body = run(crate::McpProfile::Workspace, OverlayWarmupState::Pending)
            .structured_content
            .unwrap();
        assert_eq!(body["state"], "failed");
        assert_eq!(body["semantic_failure"], serde_json::json!(main_failure));
        *runtime.lock().unwrap() = SemanticRuntimeStatus::Indexing;
        let body =
            run(crate::McpProfile::Workspace, OverlayWarmupState::EmbeddingFailed(overlay_failure))
                .structured_content
                .unwrap();
        assert_eq!(body["state"], "loading");
        assert_eq!(body["semantic_failure"], serde_json::json!(overlay_failure));
        *runtime.lock().unwrap() = SemanticRuntimeStatus::Ready;
        let body = run(crate::McpProfile::Reference, OverlayWarmupState::Pending)
            .structured_content
            .unwrap();
        assert_eq!(body["state"], "loading");
        assert!(body.get("semantic_failure").is_none());
    }

    #[test]
    fn baseline_warming_not_ready_preserves_structured_envelope() {
        let progress = IndexProgress::new();
        let result = baseline_warming_not_ready(&progress);
        let body = result.structured_content.as_ref().expect("structured not-ready envelope");
        let text = result.content[0].as_text().expect("text mirror").text.as_str();
        let mirror: serde_json::Value = serde_json::from_str(text).expect("valid JSON text mirror");

        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["detail"], "connecting to the shared PostgreSQL baseline (startup warmup)");
        assert_eq!(body["retry_after_ms"], 1500);
        assert_eq!(&mirror, body);
    }

    #[test]
    fn status_reports_semantic_runtime_lock_poison() {
        let runtime = Arc::new(Mutex::new(SemanticRuntimeStatus::Ready));
        let poisoner = {
            let runtime = Arc::clone(&runtime);
            std::thread::spawn(move || {
                let _guard = runtime.lock().unwrap();
                panic!("poison the semantic runtime lock");
            })
        };
        assert!(poisoner.join().is_err());

        let result = search_status(
            crate::McpProfile::Workspace,
            &crate::state::shared_engine(None),
            &Arc::new(IndexProgress::default()),
            &runtime,
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            EmptyIndex::Idle,
        )
        .unwrap_err();

        assert_eq!(result.code, ErrorCode::INTERNAL_ERROR);
        assert!(result.message.contains("semantic runtime lock error"));
    }

    #[test]
    fn search_status_does_not_consume_overlay_dirty_state() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("Module.bsl");
        fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();
        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_workspace_root(dir.path().to_path_buf());
        engine.initialize_workspace_overlay_clean().unwrap();
        assert!(engine.mark_workspace_path_dirty(&file).unwrap());
        assert_eq!(engine.workspace_overlay_retry_signals().unwrap().pending_dirty_paths, 1);
        let shared = crate::state::shared_engine(Some(engine));

        search_status(
            crate::McpProfile::Workspace,
            &shared,
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            EmptyIndex::Idle,
        )
        .unwrap();

        assert_eq!(
            shared
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_retry_signals()
                .unwrap()
                .pending_dirty_paths,
            1,
            "status is a pure snapshot even when a refresh could consume the mark"
        );
    }

    /// An empty search over an extension and an extension that was never registered read
    /// identically in the hits — so the composition of the index has to be askable somewhere.
    /// The declared spelling comes along because "registered" is only actionable if the reader
    /// can see WHICH directory was registered under that name.
    #[test]
    fn status_names_every_registered_root() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = dir.path().join("outside-ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let mut engine = SearchEngine::fts_only(&workspace.join("bsl-search.db")).unwrap();
        let (roots, rejected) = bsl_search::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        );
        assert!(rejected.is_empty(), "the extension is outside the configuration, so it registers");
        engine.set_workspace_roots(roots);

        let result = search_status(
            crate::McpProfile::Workspace,
            &crate::state::shared_engine(Some(engine)),
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            EmptyIndex::Idle,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("expected text content").text.as_str();

        assert!(text.contains("Source roots (current search index):"), "{text}");
        assert!(text.contains("(configuration)"), "the configuration is named, not blank: {text}");
        assert!(
            text.contains(&extension.display().to_string()),
            "and the extension is named by the directory it was declared as: {text}",
        );
    }

    /// Three states, three answers. "There is no table" is not one fact but two — the index has
    /// not published one yet, or something holds it and it could not be read — and neither is
    /// "this workspace indexes nothing". The reference profile has no source roots at all by
    /// construction, so for it the honest report is silence rather than a permanent fault.
    #[test]
    fn status_tells_an_unread_root_table_from_a_missing_one() {
        let engine: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        let building = search_status(
            crate::McpProfile::Workspace,
            &engine,
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            EmptyIndex::Idle,
        )
        .unwrap();
        let building = building.content[0].as_text().expect("text").text.clone();
        assert!(building.contains("not published yet"), "{building}");

        let reference = search_status(
            crate::McpProfile::Reference,
            &engine,
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            EmptyIndex::Idle,
        )
        .unwrap();
        let reference = reference.content[0].as_text().expect("text").text.clone();
        assert!(
            !reference.contains("Source roots"),
            "a reference index has no source roots to report: {reference}",
        );

        // Hold the engine past the cap so the read genuinely times out, exactly as an overlay
        // prime would. The roots are unchanged all the while — saying "none" here would be a
        // statement about the workspace, when the only true statement is about the read.
        let held: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        // A barrier, not a sleep: the branch under test is the ONLY one that tells "the index
        // is held" from "there is no index", so it must not depend on the holder winning a
        // scheduling race against a fixed window.
        // The lock is let go only once the read has answered, so the read cannot outlast it.
        let taken = Arc::new(std::sync::Barrier::new(2));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = {
            let held = Arc::clone(&held);
            let taken = Arc::clone(&taken);
            std::thread::spawn(move || {
                let guard = held.lock().unwrap();
                taken.wait();
                let _ = release_rx.recv();
                drop(guard);
            })
        };
        taken.wait();
        let busy = search_status_with_cap(
            crate::McpProfile::Workspace,
            &held,
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            EmptyIndex::Idle,
            Duration::from_millis(40),
        )
        .unwrap();
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let busy = busy.content[0].as_text().expect("text").text.clone();
        assert!(busy.contains("not read"), "{busy}");
        assert!(!busy.contains("not published yet"), "a held index is not an unbuilt one: {busy}",);

        // A terminal initialization failure leaves the slot empty for the rest of the process.
        // "Still building" there is not merely imprecise — it tells the reader to wait for
        // something that will never happen. The verdict is the profile's own: for the workspace
        // this runtime, with the boot's thread gone, is exactly what declares it.
        let failed = search_status(
            crate::McpProfile::Workspace,
            &engine,
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Failed(
                "workspace search engine initialization failed".to_owned(),
            ))),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            EmptyIndex::Failed,
        )
        .unwrap();
        let failed = failed.content[0].as_text().expect("text").text.clone();
        assert!(failed.contains("initialization failed"), "{failed}");
        assert!(
            !failed.contains("not published yet"),
            "a failed init is not a build in progress: {failed}",
        );
        // And neither is the terminal read allowed to keep ANY "building" wording: the root
        // table is not the only surface that reported a build in flight.
        assert!(!failed.contains("building") && !failed.contains("Building"), "{failed}");
        assert!(failed.contains("Search index: unavailable"), "{failed}");
        assert!(failed.contains("Indexing failed"), "{failed}");
        assert!(
            !failed.contains("Indexing pending: initializing"),
            "no pass is pending on a terminal failure: {failed}",
        );
    }

    /// The terminal reading of an empty slot: initialization failed and the boot's own init
    /// thread has gone, so nothing will ever publish the index in this process. Every surface
    /// that would otherwise say a build is in flight must say so instead — for both profiles
    /// (the reference profile has no source-roots section to carry the refusal) and both
    /// workspace modes (the overlay line is only rendered in the Postgres one) — while the
    /// structured state keeps naming the failure.
    #[test]
    fn a_terminal_initialization_failure_is_never_worded_as_a_build() {
        let engine: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        for profile in [crate::McpProfile::Workspace, crate::McpProfile::Reference] {
            for mode in
                [WorkspaceSearchMode::SqliteLocal, WorkspaceSearchMode::PostgresRemoteOverlay]
            {
                let result = search_status(
                    profile,
                    &engine,
                    &Arc::new(IndexProgress::default()),
                    &Arc::new(Mutex::new(SemanticRuntimeStatus::Failed(
                        "workspace search engine initialization failed".to_owned(),
                    ))),
                    mode.clone(),
                    // What a failed initialization leaves the overlay warmup at.
                    OverlayWarmupState::Pending,
                    None,
                    None,
                    false,
                    // What the profile's own source declares here: the boot thread is gone and
                    // the runtime it owns reports the failure, so nothing will publish the slot.
                    EmptyIndex::Failed,
                )
                .unwrap();
                let body = result.structured_content.clone().unwrap();
                assert_eq!(body["state"], "failed", "{profile:?}/{mode:?}");
                let text = result.content[0].as_text().expect("text").text.clone();
                assert!(
                    !text.contains("building") && !text.contains("Building"),
                    "{profile:?}/{mode:?} still words a terminal failure as a build: {text}",
                );
                assert!(
                    text.contains("initialization failed"),
                    "{profile:?}/{mode:?} does not name the failure: {text}",
                );
                assert!(
                    text.contains("unavailable"),
                    "{profile:?}/{mode:?} does not say the index is unavailable: {text}",
                );
            }
        }
    }

    /// The other side of the same line: the boot's initialization is STILL RUNNING, and the
    /// runtime hands out a failure it recorded before the engine is published (a refused token
    /// profile does exactly that). The slot is empty and looks terminal — but a build is coming,
    /// so every "building"/"pending" line has to stay.
    #[test]
    fn a_failure_during_initialization_is_still_a_build() {
        use bsl_search::{EmbeddingFailure, EmbeddingFailureCode};
        let failure = EmbeddingFailure::new(EmbeddingFailureCode::EmbeddingTimeout);
        let engine: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        for profile in [crate::McpProfile::Workspace, crate::McpProfile::Reference] {
            for mode in
                [WorkspaceSearchMode::SqliteLocal, WorkspaceSearchMode::PostgresRemoteOverlay]
            {
                let result = search_status(
                    profile,
                    &engine,
                    &Arc::new(IndexProgress::default()),
                    &Arc::new(Mutex::new(SemanticRuntimeStatus::EmbeddingFailed(failure))),
                    mode.clone(),
                    OverlayWarmupState::Pending,
                    None,
                    None,
                    false,
                    // The boot is still building: nothing here is terminal.
                    EmptyIndex::Initializing,
                )
                .unwrap();
                let text = result.content[0].as_text().expect("text").text.clone();
                // The structure must agree with the prose: a build in flight is `loading`, never
                // `failed`, however terminal the runtime status already looks.
                assert_eq!(
                    result.structured_content.clone().unwrap()["state"],
                    "loading",
                    "{profile:?}/{mode:?}",
                );
                assert!(
                    text.contains("building"),
                    "{profile:?}/{mode:?} stopped reporting the build in flight: {text}",
                );
                assert!(
                    text.contains("Indexing pending: initializing"),
                    "{profile:?}/{mode:?} stopped reporting the pending pass: {text}",
                );
                assert!(
                    !text.contains("unavailable"),
                    "{profile:?}/{mode:?} calls a build in flight unavailable: {text}",
                );
                assert!(
                    !text.contains("waiting will not publish"),
                    "{profile:?}/{mode:?} tells the reader to stop waiting during the build: {text}",
                );
            }
        }
    }

    /// A reference index whose initialization was refused outright (an unreadable project config)
    /// leaves the engine slot empty for the life of the process, with an `Disabled` runtime that
    /// says nothing about the refusal. Only the reference's own lifecycle knows, so the verdict is
    /// read from there — and the status must not word a slot nothing will fill as a build.
    #[test]
    fn a_reference_initialization_refusal_is_terminal_in_the_status() {
        let dir = tempdir().unwrap();
        // A malformed project config is refused at construction, before any runtime status exists.
        fs::write(dir.path().join("bsl-analyzer.toml"), "this is not = toml [").unwrap();
        let state = crate::state::SharedState::reference(Some(dir.path().to_path_buf()));

        let initialization = index_initialization(&state, crate::McpProfile::Reference);
        assert_eq!(
            initialization,
            EmptyIndex::Failed,
            "the reference lifecycle is the only place this refusal is recorded",
        );
        assert!(
            !state.semantic_runtime().lock().unwrap().is_failed(),
            "the runtime this profile owns carries no failure at all",
        );

        for mode in [WorkspaceSearchMode::SqliteLocal, WorkspaceSearchMode::PostgresRemoteOverlay] {
            let result = search_status(
                crate::McpProfile::Reference,
                state.search_engine(),
                &Arc::new(IndexProgress::default()),
                &state.semantic_runtime(),
                mode.clone(),
                OverlayWarmupState::Pending,
                None,
                None,
                false,
                initialization,
            )
            .unwrap();
            let body = result.structured_content.clone().unwrap();
            assert_eq!(body["state"], "failed", "{mode:?}");
            let text = result.content[0].as_text().expect("text").text.clone();
            assert!(
                text.contains("unavailable") && text.contains("initialization failed"),
                "{mode:?} does not name the refusal: {text}",
            );
            assert!(
                !text.contains("building") && !text.contains("Building"),
                "{mode:?} words a refused initialization as a build: {text}",
            );
        }
    }

    /// The workspace's own terminal source. A runtime that stopped with the daemon is terminal by
    /// its own contract, so an empty slot it left behind will never be filled — while the runtime
    /// states that merely describe work in progress leave the slot idle, and a failed runtime is
    /// terminal only once the boot's thread has gone.
    #[test]
    fn the_workspaces_initialization_verdict_comes_from_its_own_runtime() {
        let state = crate::state::SharedState::shared();
        let verdict = |runtime: SemanticRuntimeStatus| {
            *state.semantic_runtime().lock().unwrap() = runtime;
            index_initialization(&state, crate::McpProfile::Workspace)
        };
        assert_eq!(
            verdict(SemanticRuntimeStatus::Stopped),
            EmptyIndex::Failed,
            "a runtime stopped with the daemon will not fill the slot",
        );
        assert_eq!(verdict(SemanticRuntimeStatus::Indexing), EmptyIndex::Idle);
        assert_eq!(verdict(SemanticRuntimeStatus::Disabled), EmptyIndex::Idle);
        assert_eq!(
            verdict(SemanticRuntimeStatus::Failed("engine refused".to_owned())),
            EmptyIndex::Failed,
        );

        // And the rendering follows that verdict: the stopped runtime's terminal wording, with no
        // "building" anywhere, for both profiles' shared surfaces.
        *state.semantic_runtime().lock().unwrap() = SemanticRuntimeStatus::Stopped;
        let result = search_status(
            crate::McpProfile::Workspace,
            state.search_engine(),
            &Arc::new(IndexProgress::default()),
            &state.semantic_runtime(),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            EmptyIndex::Failed,
        )
        .unwrap();
        assert_eq!(result.structured_content.clone().unwrap()["state"], "failed");
        let text = result.content[0].as_text().expect("text").text.clone();
        assert!(text.contains("initialization failed"), "{text}");
        assert!(!text.contains("building"), "{text}");
    }

    /// The boot's own window, on a workspace-shaped state: the engine slot is empty, the runtime
    /// already carries a failure (a refused token profile is recorded before the engine is
    /// published) — and the boot's thread is STILL RUNNING, so nothing here is terminal. The
    /// verdict must come from that flag, not from the runtime.
    ///
    /// The window is real but not constructible: the flag is raised by the boot's constructor and
    /// lowered by the init thread's own guard as it leaves, so a test that built a state and read
    /// the flag would be racing that thread (and the constructor always starts it). The test
    /// therefore stands in the window through the field's test seam — with the derivation itself
    /// pinned below, so the flag cannot be dropped from the verdict unnoticed.
    #[test]
    fn a_building_workspace_with_a_failed_runtime_is_not_terminal() {
        let state = crate::state::SharedState::shared();
        *state.semantic_runtime().lock().unwrap() =
            SemanticRuntimeStatus::Failed("engine refused".to_owned());
        assert!(!state.search_initializing(), "no boot thread runs in this fixture");

        // Control: with no boot thread, the same empty slot and the same runtime ARE terminal.
        assert_eq!(
            index_initialization(&state, crate::McpProfile::Workspace),
            EmptyIndex::Failed,
            "a gone boot thread plus a failed runtime is what terminal means here",
        );

        state.hold_search_initialization_for_test();
        let initialization = index_initialization(&state, crate::McpProfile::Workspace);
        assert_eq!(
            initialization,
            EmptyIndex::Initializing,
            "the boot's thread is what makes a failed-looking empty slot a build in flight",
        );

        let result = search_status(
            crate::McpProfile::Workspace,
            state.search_engine(),
            &Arc::new(IndexProgress::default()),
            &state.semantic_runtime(),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            initialization,
        )
        .unwrap();
        assert_eq!(
            result.structured_content.clone().unwrap()["state"],
            "loading",
            "a build in flight is not a terminal state, however the runtime reads",
        );
        let text = result.content[0].as_text().expect("text").text.clone();
        assert!(
            text.contains("building") && text.contains("not published yet"),
            "the window still reads as a build: {text}",
        );
        assert!(
            !text.contains("unavailable") && !text.contains("waiting will not publish"),
            "and never as a terminal failure: {text}",
        );
    }

    #[test]
    fn search_status_shows_workspace_overlay_section() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура СтараяПроцедура()\nКонецПроцедуры").unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        fs::write(&file, "Процедура НоваяПроцедура()\nКонецПроцедуры").unwrap();

        let result = search_status(
            crate::McpProfile::Workspace,
            &crate::state::shared_engine(Some(engine)),
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            Some(ConfiguredBaselineStatus {
                backend: "sqlite",
                selection: "local workspace index".to_owned(),
                issue: None,
                support: None,
            }),
            None,
            false,
            EmptyIndex::Idle,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("expected text content").text.as_str();
        assert!(text.contains("Code lexical source: local sqlite + local overlay"));
        assert!(text.contains("Resolved workspace view: ready"));
        assert!(text.contains("Baseline: snapshot local-workspace-baseline"));
        assert!(text.contains("Workspace overlay: enabled"));
        assert!(
            text.contains("Watcher mode: off (no change feed has proved itself"),
            "no feed ever attached to this engine: {}",
            text,
        );
        assert!(text.contains("Files:    1"));
        assert!(text.contains("Chunks:   1"));
    }

    #[test]
    fn search_status_shows_external_baseline_probe_errors() {
        let source = unreachable_workspace_service();
        source.seed_status_cache_for_test(
            ExternalBaselineStatus {
                backend: "postgres",
                schema: "erp".to_owned(),
                selection: "branch main".to_owned(),
                resolved: None,
                state: ExternalBaselineState::Error("connection refused".to_owned()),
                semantic_details: None,
            },
            Duration::from_secs(1),
        );

        let result = search_status(
            crate::McpProfile::Workspace,
            &crate::state::shared_engine(None),
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::PostgresRemoteOverlay,
            OverlayWarmupState::Pending,
            Some(ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "branch main".to_owned(),
                issue: None,
                support: None,
            }),
            Some(source),
            false,
            EmptyIndex::Idle,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("expected text content").text.as_str();
        assert!(text.contains("Configured baseline:"));
        assert!(text.contains("Select:   branch main"));
        assert!(text.contains("External baseline: configured"));
        assert!(text.contains("Backend:  postgres"));
        assert!(text.contains("Status:   error"));
        assert!(text.contains("Error:    connection refused"));
        assert!(
            text.contains("Probed:   1s ago"),
            "cached render must state the probe age: {text}"
        );
    }

    #[test]
    fn search_status_reports_pending_probe_without_blocking() {
        let source = unreachable_workspace_service();
        let started = Instant::now();
        let result = search_status(
            crate::McpProfile::Workspace,
            &crate::state::shared_engine(None),
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Ready)),
            WorkspaceSearchMode::PostgresRemoteOverlay,
            OverlayWarmupState::Pending,
            Some(ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "branch main".to_owned(),
                issue: None,
                support: None,
            }),
            Some(source),
            false,
            EmptyIndex::Idle,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("expected text content").text.as_str();
        // The answer must not wait for the probe it started: a text check alone would pass a
        // status that waited, and a slow one whose text is unchanged.
        if cfg!(timing_contract) {
            assert!(started.elapsed() < Duration::from_millis(500), "status blocked on the probe");
        }
        assert!(text.contains("probing the shared baseline in the background — retry shortly"));
        assert!(text.contains("first background status probe still running"));
        assert!(!text.contains("(published index)"));
    }

    #[test]
    fn search_status_reports_warming_while_baseline_connect_is_pending() {
        let result = search_status(
            crate::McpProfile::Workspace,
            &crate::state::shared_engine(None),
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::PostgresRemoteOverlay,
            OverlayWarmupState::Pending,
            None,
            None,
            true,
            EmptyIndex::Idle,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("expected text content").text.as_str();
        assert!(text.contains("Configured baseline:"), "{text}");
        assert!(text.contains("Backend:  postgres"), "{text}");
        assert!(text.contains("connecting to the shared baseline"), "{text}");
    }

    #[test]
    fn search_status_shows_reference_docs_source() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("reference-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine
            .index_documents(
                "platform",
                "platform://docs",
                b"v1",
                &[Document {
                    title: "Массив / Array".to_owned(),
                    body: "Тип: Массив / Array".to_owned(),
                    kind: "type".to_owned(),
                }],
                None,
            )
            .unwrap();
        let result = search_status(
            crate::McpProfile::Workspace,
            &crate::state::shared_engine(Some(engine)),
            &Arc::new(IndexProgress::default()),
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            Some(ConfiguredBaselineStatus {
                backend: "sqlite",
                selection: "local reference index".to_owned(),
                issue: None,
                support: None,
            }),
            None,
            false,
            EmptyIndex::Idle,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("expected text content").text.as_str();
        assert!(text.contains("Docs lexical source: local sqlite"));
        assert!(text.contains("Docs semantic source: not configured (set EMBEDDING_URL)"));
    }

    #[test]
    fn search_status_reports_overlay_sync_for_postgres_mode() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let file = workspace.join("CommonModule.bsl");
        fs::write(&file, "Процедура СтараяПроцедура()\nКонецПроцедуры").unwrap();
        let db_path = workspace.join("bsl-search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.index_directory_fts(workspace).unwrap();
        engine.set_workspace_root(workspace);
        let progress = Arc::new(IndexProgress::default());
        let _pass = progress.begin_pass();
        let token = _pass.token();
        token.set_totals(0, 200, 20);
        token.advance(50, 5);
        let result = search_status(
            crate::McpProfile::Workspace,
            &crate::state::shared_engine(Some(engine)),
            &progress,
            &Arc::new(Mutex::new(SemanticRuntimeStatus::OverlaySyncing)),
            WorkspaceSearchMode::PostgresRemoteOverlay,
            OverlayWarmupState::Pending,
            Some(ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "branch develop".to_owned(),
                issue: None,
                support: None,
            }),
            None,
            false,
            EmptyIndex::Idle,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("expected text content").text.as_str();
        assert!(text.contains("Search index: ready"));
        assert!(text.contains("overlay syncing") && text.contains("queues behind the sync"));
        assert!(text.contains("Semantic: syncing local overlay embeddings against remote baseline"));
        assert!(!text.contains("Indexing in progress: 25%"), "boundary owns counter rendering");
    }

    #[test]
    fn search_status_summary_block_is_self_explanatory() {
        let postgres_baseline = || {
            Some(ConfiguredBaselineStatus {
                backend: "postgres",
                selection: "branch develop".to_owned(),
                issue: None,
                support: None,
            })
        };
        let run = |warmup: OverlayWarmupState| {
            search_status(
                crate::McpProfile::Workspace,
                &crate::state::shared_engine(None),
                &Arc::new(IndexProgress::default()),
                &Arc::new(Mutex::new(SemanticRuntimeStatus::Ready)),
                WorkspaceSearchMode::PostgresRemoteOverlay,
                warmup,
                postgres_baseline(),
                None,
                false,
                EmptyIndex::Idle,
            )
            .unwrap()
            .content[0]
                .as_text()
                .expect("text content")
                .text
                .clone()
        };
        let no_diffs = run(OverlayWarmupState::NoLocalDiffs);
        assert!(no_diffs.starts_with("Summary:"));
        assert!(no_diffs.contains("served from the branch develop baseline (published index)."));
        assert!(no_diffs.contains("working tree matches the baseline"));
        let failed = run(OverlayWarmupState::Failed("embedder timeout: global".to_owned()));
        assert!(failed.contains("warmup failed: embedder timeout: global"));
        assert!(
            !failed.contains("Restart MCP to retry"),
            "Failed is retried automatically now; a restart demand would be a lie: {failed}"
        );
        let synced = run(OverlayWarmupState::Synced { overlay_files: 2, embedded: 5 });
        assert!(synced.contains("2 locally-changed file(s) indexed (5 chunks)"));

        // An incomplete pass names its numbers and does not borrow Failed's restart advice; the
        // unconditional Note line itself must mention the catch-up — asserting on THAT line,
        // because neither the compiler nor the branch test above would notice the Note
        // regressing to its old wording.
        let incomplete = run(OverlayWarmupState::Incomplete {
            unreadable: 3,
            canonical_fallbacks: 1,
            read_failures: 2,
            persist_failed: false,
        });
        assert!(
            incomplete.contains(
                "INCOMPLETE pass (3 unreadable subtree(s), 1 unresolved spelling(s), 2 unread file(s))"
            ),
            "{incomplete}"
        );
        assert!(!incomplete.contains("Restart MCP to retry"), "not Failed's advice");
        let note = incomplete
            .lines()
            .find(|line| line.trim_start().starts_with("Note:"))
            .expect("the Note line is unconditional in overlay mode");
        assert!(
            note.contains("a retry driver catches up incomplete or failed passes automatically"),
            "the Note itself names the automatic catch-up: {note}"
        );
    }

    #[test]
    fn search_status_emits_progress_signal_while_building() {
        let engine: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        let progress = Arc::new(IndexProgress::default());
        let result = search_status(
            crate::McpProfile::Workspace,
            &engine,
            &progress,
            &Arc::new(Mutex::new(SemanticRuntimeStatus::Ready)),
            WorkspaceSearchMode::SqliteLocal,
            OverlayWarmupState::Pending,
            None,
            None,
            false,
            EmptyIndex::Idle,
        )
        .unwrap();
        let text = result.content[0].as_text().expect("text").text.as_str();
        assert!(text.contains("building"));
        assert!(text.contains("Indexing pending: initializing"));
        assert!(!text.contains("Indexing in progress"));
        // The cold-build control for the terminal wording: an empty slot with a healthy runtime
        // and no initialization flag must NOT read as a failed initialization.
        assert!(!text.contains("initialization failed"), "{text}");
        assert!(!text.contains("waiting will not publish"), "{text}");
    }

    #[test]
    fn search_status_returns_promptly_with_busy_note_when_engine_lock_is_held() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bsl-search.db");
        let engine = crate::state::shared_engine(Some(SearchEngine::fts_only(&db_path).unwrap()));
        // The holder lets the lock go only after the status has answered, so a status that
        // waited for the lock never answers and the shared watchdog below runs out.
        let gate = Arc::new(Barrier::new(2));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = {
            let engine = Arc::clone(&engine);
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                let held = engine.lock().unwrap();
                gate.wait();
                let _ = release_rx.recv();
                drop(held);
            })
        };
        gate.wait();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        let caller = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || {
                let started = Instant::now();
                let status = search_status_with_cap(
                    crate::McpProfile::Workspace,
                    &engine,
                    &Arc::new(IndexProgress::default()),
                    &Arc::new(Mutex::new(SemanticRuntimeStatus::OverlaySyncing)),
                    WorkspaceSearchMode::PostgresRemoteOverlay,
                    OverlayWarmupState::Pending,
                    Some(ConfiguredBaselineStatus {
                        backend: "postgres",
                        selection: "branch main".to_owned(),
                        issue: None,
                        support: None,
                    }),
                    None,
                    false,
                    EmptyIndex::Idle,
                    Duration::from_millis(40),
                )
                .unwrap();
                let elapsed = started.elapsed();
                let text = status.content[0].as_text().expect("text content").text.clone();
                let _ = answer_tx.send((text, elapsed));
            })
        };
        let (text, elapsed) = answer_rx
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the status waited for the held engine lock");
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        caller.join().unwrap();
        if cfg!(timing_contract) {
            assert!(elapsed < Duration::from_secs(2));
        }
        let text = text.as_str();
        assert!(text.contains("Configured baseline:"));
        assert!(text.contains("Local index: busy (overlay syncing)"));
        assert!(text.contains("Lexical search: temporarily unavailable"));
        assert!(!text.contains("Lexical search: reflects the current working tree"));
    }
}
