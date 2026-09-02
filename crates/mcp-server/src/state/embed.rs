use super::retry_window::{RetryDecision, RetryOwner, RetryWindow};
use super::types::{OverlayWarmupState, SemanticRuntimeStatus, SharedSearchEngine};
use super::SharedState;
use bsl_search::{IndexProgress, SearchEngine, WorkspaceRootsTransitionOutcome};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// The ONE embed single-flight for the whole workspace. Both the boot pass (fills the initial
/// NULL embeddings after a fused cold build) and the post-context-refresh re-embed kick funnel
/// through it, so an older pass can never install a vector index over a newer one
/// (last-writer-wins). A pass that loses the claim records `rerun_pending`; the winning owner
/// loops while that flag is set, and both the "record a rerun" and the "release the claim"
/// decisions happen under the same mutex — so a rerun request can never be lost between the
/// owner deciding to stop and a late caller signalling more work.
pub(super) struct EmbedFlight {
    state: Mutex<EmbedFlightState>,
}

#[derive(Default)]
struct EmbedFlightState {
    in_flight: bool,
    rerun_pending: bool,
}

impl EmbedFlight {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(EmbedFlightState::default()) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EmbedFlightState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Try to claim the flight. `true` = THIS caller won and must run the pass; `false` = a
    /// pass is already running and a rerun was recorded so it loops again for this caller's
    /// (later-NULLed) chunks.
    fn claim(&self) -> bool {
        let mut st = self.lock();
        if st.in_flight {
            st.rerun_pending = true;
            false
        } else {
            st.in_flight = true;
            true
        }
    }

    /// Start of a pass iteration: clear the rerun flag so a request arriving DURING this
    /// iteration triggers another loop rather than being swallowed.
    fn begin_pass(&self) {
        self.lock().rerun_pending = false;
    }

    /// End of a pass iteration. `true` = a rerun was requested (keep the claim, loop again);
    /// `false` = none, so the claim is released under the same lock (no wakeup can be lost).
    fn finish_pass(&self) -> bool {
        let mut st = self.lock();
        if st.rerun_pending {
            true
        } else {
            st.in_flight = false;
            false
        }
    }

    /// Force-release the claim on an abnormal exit (panic / embed error). A leftover rerun
    /// request is harmless — the next owner clears it in `begin_pass` and runs anyway.
    fn release(&self) {
        self.lock().in_flight = false;
    }

    #[cfg(test)]
    fn is_in_flight(&self) -> bool {
        self.lock().in_flight
    }

    /// Whether a caller that lost the claim recorded a rerun — the observable proof that its
    /// work was absorbed into the running pass rather than dropped or spawned as a second one.
    #[cfg(test)]
    fn rerun_pending(&self) -> bool {
        self.lock().rerun_pending
    }

    #[cfg(test)]
    fn in_flight_for_test() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(EmbedFlightState { in_flight: true, rerun_pending: false }),
        })
    }
}

/// RAII release of the shared embed claim on an abnormal exit (panic / early return) while the
/// owner still holds it, so a crashed pass never strands the flight `in_flight`. A clean exit
/// calls [`Self::disarm`] first (the owner released the claim itself under the flight lock), so
/// this does not stomp a later owner that already re-claimed.
struct EmbedClaimGuard {
    flight: Arc<EmbedFlight>,
    armed: bool,
}

impl EmbedClaimGuard {
    fn new(flight: Arc<EmbedFlight>) -> Self {
        Self { flight, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for EmbedClaimGuard {
    fn drop(&mut self) {
        if self.armed {
            self.flight.release();
        }
    }
}

/// RAII restoration of the semantic runtime status for a background embed pass. The pass sets
/// `Indexing` before it starts; this guarantees the status leaves `Indexing` even if the pass
/// panics or returns early without an explicit terminal transition — otherwise a crashed pass
/// would strand the runtime at `Indexing` forever. An explicit [`Self::finish`] on a clean
/// success/failure suppresses the fallback.
struct EmbedStatusGuard {
    runtime: Arc<Mutex<SemanticRuntimeStatus>>,
    finished: bool,
}

impl EmbedStatusGuard {
    fn new(runtime: Arc<Mutex<SemanticRuntimeStatus>>) -> Self {
        Self { runtime, finished: false }
    }

    fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for EmbedStatusGuard {
    fn drop(&mut self) {
        if !self.finished {
            SharedState::set_semantic_runtime_status(
                &self.runtime,
                SemanticRuntimeStatus::Failed("embedding pass ended without completing".to_owned()),
            );
        }
    }
}

/// Test seam: force the embed pass body to panic after its guards are in place, to verify the
/// guards restore the flight claim and the runtime status (never leaving it stuck `Indexing`).
#[cfg(test)]
static FORCE_EMBED_PASS_PANIC: AtomicBool = AtomicBool::new(false);

/// Test seam: a callback invoked once after the first embed iteration installs its index (and
/// before `finish_pass`), so a test can create a NULL chunk mid-flight and signal a rerun,
/// proving the owner loops and embeds it. Receives the store DB path.
#[cfg(test)]
type EmbedPostPassHook = Box<dyn FnMut(&Path) + Send>;
#[cfg(test)]
static EMBED_POST_PASS_HOOK: Mutex<Option<EmbedPostPassHook>> = Mutex::new(None);

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbedFencePoint {
    Apply(usize),
    Swap,
}
#[cfg(test)]
type EmbedFenceHook = Box<dyn FnMut(EmbedFencePoint) + Send>;
#[cfg(test)]
static EMBED_FENCE_HOOK: Mutex<Option<EmbedFenceHook>> = Mutex::new(None);
#[cfg(test)]
static FORCE_EMBED_PREFLIGHT_REFUSALS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static FORCE_EMBED_PUBLICATION_REFUSALS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
pub(super) static FORCE_OVERLAY_PUBLICATION_REFUSALS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// Test observer bracketing the expensive root-plan validation. Thread-local so parallel graph
// tests cannot report their own transitions into this test's deterministic assertion.
#[cfg(test)]
type RootValidationHook = Option<Box<dyn Fn(bool)>>;
#[cfg(test)]
thread_local! {
    static ROOT_VALIDATION_HOOK: std::cell::RefCell<RootValidationHook> =
        const { std::cell::RefCell::new(None) };
}

impl SharedState {
    /// The production publish hook: after a graph publish it re-renders the search chunks
    /// marked context-dirty by an `.xml` drift, then re-embeds them. Extracted so a test can
    /// wire the SAME closure the daemon does rather than calling the refresh by hand. The
    /// hook receives `(drift_pending, build_start_seq)`: `build_start_seq` bounds which marks
    /// the refresh may clear (only drifts this build already reflects), while `drift_pending`
    /// is a fast-path hint to skip a round when a fresher reload is imminent.
    // Each handle is an independent owner used by the long-lived publish closure; grouping them
    // would only move the same lifecycle dependencies behind a bag-of-fields type.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_publish_hook(
        search_engine: SharedSearchEngine,
        cache: crate::cache::WorkspaceCacheLayout,
        semantic_runtime: Arc<Mutex<SemanticRuntimeStatus>>,
        index_progress: Arc<IndexProgress>,
        embed_flight: Arc<EmbedFlight>,
        overlay_retry: Option<Arc<super::overlay_retry::OverlayRetry>>,
        root_drift_epoch: Arc<AtomicU64>,
        lease: crate::workspace_lease::WorkspaceLease,
        publish_retry_budget: std::time::Duration,
    ) -> Arc<
        dyn Fn(crate::graph::GraphPublishSignal) -> crate::graph::GraphPublishOutcome + Send + Sync,
    > {
        Arc::new(move |signal| {
            // The transition must precede the context consume: the latter marks and refreshes
            // rows under the root keyspace that is current when it takes the engine lock.
            let (roots_handled, pending_collection_embeddings, pending_overlay_embeddings) =
                Self::refresh_search_roots_after_graph(
                    &search_engine,
                    &cache,
                    &root_drift_epoch,
                    &lease,
                    &signal,
                );
            let topology_handled = Self::refresh_search_contexts_after_graph_with_cache(
                &search_engine,
                &cache,
                &semantic_runtime,
                &index_progress,
                &embed_flight,
                &lease,
                signal,
                publish_retry_budget,
            );
            // Context refresh may have NULLed more chunks, so kick after both mutations and let
            // the existing single-flights absorb all pending work in one rerun.
            if pending_collection_embeddings {
                Self::kick_context_reembed(
                    &search_engine,
                    &semantic_runtime,
                    &index_progress,
                    &embed_flight,
                    &lease,
                    publish_retry_budget,
                );
            }
            if pending_overlay_embeddings {
                if let Some(retry) = &overlay_retry {
                    retry.kick_fresh();
                }
            }
            crate::graph::GraphPublishOutcome { topology_handled, roots_handled }
        })
    }

    /// Apply the search root table carried by the published graph. Planning walks and reads the
    /// filesystem without the outer engine mutex; only seed capture and guarded apply serialize
    /// with searches and watcher attribution.
    fn refresh_search_roots_after_graph(
        engine: &SharedSearchEngine,
        cache: &crate::cache::WorkspaceCacheLayout,
        root_drift_epoch: &AtomicU64,
        lease: &crate::workspace_lease::WorkspaceLease,
        signal: &crate::graph::GraphPublishSignal,
    ) -> (bool, bool, bool) {
        if !signal.roots_refresh_requested {
            return (true, false, false);
        }
        if signal.drift_pending {
            tracing::debug!(
                "graph drift still pending; deferring search root transition to the next publish"
            );
            return (false, false, false);
        }
        let Some(roots) = signal.workspace_roots.clone() else {
            tracing::debug!(
                "published graph has no validated project root table; keeping search roots"
            );
            return (false, false, false);
        };
        let Some(provider) = Self::published_graph_context_provider(cache, signal.topology) else {
            return (false, false, false);
        };
        let seed = {
            let Ok(mut guard) = engine.lock() else {
                tracing::warn!("search engine lock poisoned while capturing root transition");
                return (false, false, false);
            };
            let Some(engine) = guard.as_mut() else {
                return (false, false, false);
            };
            // Every graph publish asks the hook to check roots. The overwhelmingly common
            // unchanged case must stay O(1), not re-walk and re-chunk the whole workspace.
            // It still installs the new artifact provider: otherwise a later watcher point
            // refresh would keep querying the graph file that was open at daemon boot.
            if engine.workspace_roots() == Some(&roots) {
                None
            } else {
                match engine.workspace_roots_transition_seed(roots) {
                    Ok(seed) => Some(seed),
                    Err(error) => {
                        tracing::warn!("could not capture search root transition: {error}");
                        return (false, false, false);
                    }
                }
            }
        };
        let Some(seed) = seed else {
            return match Self::apply_workspace_search(engine, lease, |engine| {
                engine.replace_published_graph_context_provider(provider)
            }) {
                super::WorkspaceSearchApply::Applied(()) => (true, false, false),
                super::WorkspaceSearchApply::OperationError(error) => {
                    tracing::warn!("could not install published graph context provider: {error}");
                    (false, false, false)
                }
                super::WorkspaceSearchApply::TransientRefusal
                | super::WorkspaceSearchApply::Superseded
                | super::WorkspaceSearchApply::Released => (false, false, false),
            };
        };
        // Fence the complete off-lock preparation, not only its second validation pass. Metadata
        // is intentionally absent from the BSL file identity set, so only the sink epoch can say
        // that an XML/config event landed while plan() was chunking the workspace.
        let validation_epoch = root_drift_epoch.load(Ordering::SeqCst);
        let seed = seed.with_graph_context_provider(provider.clone());
        let plan = match seed.plan() {
            Ok(plan) => plan,
            Err(error) => {
                tracing::warn!("could not plan search root transition: {error}");
                return (false, false, false);
            }
        };
        // The second scan/read bracket is deliberately off the outer engine mutex. Fence it with
        // the search sink's root-relevant drift epoch: a BSL/config/subtree batch processed before
        // the final engine-lock claim supersedes this plan, while one processed afterwards waits
        // on that lock and is attributed through the newly-published roots after apply. Unlike the
        // hub-wide raw event counter, unrelated files do not reject a valid transition.
        #[cfg(test)]
        ROOT_VALIDATION_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook(true);
            }
        });
        let validation = plan.revalidate();
        #[cfg(test)]
        ROOT_VALIDATION_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook(false);
            }
        });
        let validated = match validation {
            Ok(Some(validated)) => validated,
            Ok(None) => {
                tracing::debug!(
                    "search root transition validation was superseded; keeping retry obligation"
                );
                return (false, false, false);
            }
            Err(error) => {
                // No inner retry loop: GraphState retains the root-only obligation and the
                // existing search-sink heartbeat retries it once per bounded wake.
                tracing::warn!("could not validate search root transition: {error}");
                return (false, false, false);
            }
        };
        let mut staged = {
            let Ok(mut guard) = engine.lock() else {
                tracing::warn!("search engine lock poisoned while staging root transition");
                return (false, false, false);
            };
            let Some(engine) = guard.as_mut() else {
                return (false, false, false);
            };
            match engine.stage_validated_workspace_roots_transition(validated) {
                Ok(Some(staged)) => staged,
                Ok(None) => {
                    tracing::debug!(
                        "search root transition was superseded while staging; keeping retry obligation"
                    );
                    return (false, false, false);
                }
                Err(error) => {
                    tracing::warn!("could not stage search root transition: {error}");
                    return (false, false, false);
                }
            }
        };
        let outcome = match Self::apply_workspace_search_checkpointed(
            engine,
            lease,
            |engine, checkpoint| {
                if root_drift_epoch.load(Ordering::SeqCst) != validation_epoch {
                    tracing::debug!(
                    "root-relevant drift was processed across validation; keeping retry obligation"
                );
                    return std::ops::ControlFlow::Continue(Ok(
                        WorkspaceRootsTransitionOutcome::Superseded,
                    ));
                }
                let applied =
                    engine.apply_staged_workspace_roots_transition(&mut staged, checkpoint);
                match applied {
                    std::ops::ControlFlow::Continue(Ok(
                        WorkspaceRootsTransitionOutcome::Unchanged,
                    )) => std::ops::ControlFlow::Continue(
                        engine
                            .replace_published_graph_context_provider(provider)
                            .map(|()| WorkspaceRootsTransitionOutcome::Unchanged),
                    ),
                    std::ops::ControlFlow::Continue(Ok(
                        outcome @ WorkspaceRootsTransitionOutcome::Applied { .. },
                    )) => {
                        // Preserve the already-committed outcome even if the provider lock is
                        // poisoned: its pending embedding signals must still reach their owners.
                        if let Err(error) =
                            engine.replace_published_graph_context_provider(provider)
                        {
                            tracing::warn!(
                            "root transition applied but published graph provider was not installed: {error}"
                        );
                        }
                        std::ops::ControlFlow::Continue(Ok(outcome))
                    }
                    other => other,
                }
            },
        ) {
            super::WorkspaceSearchApply::Applied(outcome) => outcome,
            super::WorkspaceSearchApply::OperationError(error) => {
                tracing::warn!("could not apply search root transition: {error}");
                return (false, false, false);
            }
            super::WorkspaceSearchApply::TransientRefusal
            | super::WorkspaceSearchApply::Superseded
            | super::WorkspaceSearchApply::Released => return (false, false, false),
        };
        match outcome {
            WorkspaceRootsTransitionOutcome::Unchanged => (true, false, false),
            WorkspaceRootsTransitionOutcome::Applied {
                removed,
                rebuilt,
                added,
                pending_collection_embeddings,
                pending_overlay_embeddings,
            } => {
                tracing::info!(
                    removed,
                    rebuilt,
                    added,
                    "search root table transitioned after graph publish"
                );
                (true, pending_collection_embeddings, pending_overlay_embeddings)
            }
            WorkspaceRootsTransitionOutcome::Superseded => {
                tracing::debug!("search root transition was superseded; keeping retry obligation");
                (false, false, false)
            }
        }
    }

    fn published_graph_context_provider(
        cache: &crate::cache::WorkspaceCacheLayout,
        topology: u64,
    ) -> Option<Arc<crate::graph_query::GraphDbContextProvider>> {
        let graph_path = cache.graph_db_path();
        let graph_db = match crate::graph_query::GraphDb::open(&graph_path) {
            Ok(db) => db,
            Err(error) => {
                tracing::debug!("graph unavailable for search root transition: {error}");
                return None;
            }
        };
        match graph_db.freshness_token() {
            Ok((_, fingerprint, _)) if fingerprint.topology == topology => {
                Some(Arc::new(crate::graph_query::GraphDbContextProvider::new(graph_db)))
            }
            _ => {
                tracing::warn!(
                    published_topology = topology,
                    "graph database on disk is not the published build; skipping root transition"
                );
                None
            }
        }
    }
    /// The outcome of one warmup pass, from what its plan proved. A pass whose scan left
    /// something unseen (`unreadable`, `canonical_fallbacks`) or whose reads failed may not
    /// speak for the whole tree: reporting `NoLocalDiffs`/`Synced` then would claim a
    /// completeness nobody verified, so those are reserved for a fully-verified pass.
    fn warmup_outcome(
        plan_empty: bool,
        overlay_files: usize,
        embedded: usize,
        unreadable: usize,
        canonical_fallbacks: usize,
        read_failures: usize,
        persist_failed: bool,
    ) -> OverlayWarmupState {
        if unreadable > 0 || canonical_fallbacks > 0 || read_failures > 0 || persist_failed {
            OverlayWarmupState::Incomplete {
                unreadable,
                canonical_fallbacks,
                read_failures,
                persist_failed,
            }
        } else if plan_empty {
            OverlayWarmupState::NoLocalDiffs
        } else {
            OverlayWarmupState::Synced { overlay_files, embedded }
        }
    }

    pub(super) fn run_overlay_warmup(
        search_engine: &SharedSearchEngine,
        overlay_warmup: &Arc<Mutex<OverlayWarmupState>>,
        lease: &crate::workspace_lease::WorkspaceLease,
        keep_going: &dyn Fn() -> bool,
        retry_transient: &mut dyn FnMut() -> bool,
    ) -> super::WorkspaceSearchApply<OverlayWarmupState, String> {
        let cloned = match search_engine.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(engine) => {
                    let Some(embedder_config) = engine.embedder_config() else {
                        tracing::debug!("overlay warmup: no embedder configured; skipping");
                        Self::set_overlay_warmup_state(
                            overlay_warmup,
                            OverlayWarmupState::Skipped("no embedder configured".to_owned()),
                        );
                        return super::WorkspaceSearchApply::Applied(OverlayWarmupState::Skipped(
                            "no embedder configured".to_owned(),
                        ));
                    };
                    let Some(roots) = engine.workspace_roots().cloned() else {
                        tracing::debug!("overlay warmup: no workspace root; skipping");
                        Self::set_overlay_warmup_state(
                            overlay_warmup,
                            OverlayWarmupState::Skipped("no workspace root".to_owned()),
                        );
                        return super::WorkspaceSearchApply::Applied(OverlayWarmupState::Skipped(
                            "no workspace root".to_owned(),
                        ));
                    };
                    let warm_cache = match engine.workspace_overlay_embedding_cache_snapshot() {
                        Ok(cache) => cache,
                        Err(error) => {
                            tracing::warn!(
                                "overlay warmup: failed to snapshot warm cache: {error}"
                            );
                            Self::set_overlay_warmup_state(
                                overlay_warmup,
                                OverlayWarmupState::Failed(error.to_string()),
                            );
                            return super::WorkspaceSearchApply::OperationError(error.to_string());
                        }
                    };
                    // Captured here, under the same lock as the warm cache and before the lock-free
                    // embed: the publish judges itself against this baseline — marks it may
                    // consume, and the freshness fence point settlements must out-date to
                    // survive it.
                    let dirty_before = match engine.workspace_overlay_publication_baseline() {
                        Ok(dirty) => dirty,
                        Err(error) => {
                            tracing::warn!(
                                "overlay warmup: failed to snapshot dirty paths: {error}"
                            );
                            Self::set_overlay_warmup_state(
                                overlay_warmup,
                                OverlayWarmupState::Failed(error.to_string()),
                            );
                            return super::WorkspaceSearchApply::OperationError(error.to_string());
                        }
                    };
                    Some((
                        engine.db_path().to_path_buf(),
                        embedder_config,
                        roots,
                        warm_cache,
                        engine.graph_context_provider(),
                        dirty_before,
                    ))
                }
                None => None,
            },
            Err(e) => {
                tracing::warn!("overlay warmup: engine lock error: {e}");
                Self::set_overlay_warmup_state(
                    overlay_warmup,
                    OverlayWarmupState::Failed(format!("engine lock error: {e}")),
                );
                return super::WorkspaceSearchApply::OperationError(format!(
                    "engine lock error: {e}"
                ));
            }
        };
        let Some((db_path, embedder_config, roots, warm_cache, graph_provider, dirty_before)) =
            cloned
        else {
            // Engine was published earlier but is gone now (e.g. shutdown raced the warmup).
            Self::set_overlay_warmup_state(
                overlay_warmup,
                OverlayWarmupState::Skipped("engine unavailable".to_owned()),
            );
            return super::WorkspaceSearchApply::OperationError("engine unavailable".to_owned());
        };

        // Lock-free: plan against a reopened standalone store and embed the missing chunks. The
        // engine mutex is NOT held here, so search/status stay responsive during the remote embed.
        // Ownership is re-checked between embed batches (the uncached read — the cached
        // verdict would let a superseded daemon write for up to its TTL after a takeover),
        // and the caller's own stop signal rides along: a shutdown mid-batch must not keep
        // writing the shared table while the lease is being handed over.
        let should_continue = || keep_going();
        let planning_distrusted = dirty_before.retry_distrusted();
        let primed = SearchEngine::prime_workspace_overlay_standalone_retrying(
            &db_path,
            embedder_config,
            &roots,
            warm_cache,
            graph_provider,
            &should_continue,
            |operation| Self::search_fence_outcome(lease.publish_short(&mut (), |_| operation())),
            &planning_distrusted,
            &mut *retry_transient,
        );
        let (plan, new_embeddings) = match primed {
            Ok(bsl_search::FenceOutcome::Applied(result)) => result,
            Ok(bsl_search::FenceOutcome::TransientRefusal) => {
                tracing::warn!("workspace overlay semantic warmup temporarily refused");
                return super::WorkspaceSearchApply::TransientRefusal;
            }
            Ok(bsl_search::FenceOutcome::Superseded) => {
                tracing::warn!("workspace overlay semantic warmup stopped");
                Self::set_overlay_warmup_state(
                    overlay_warmup,
                    OverlayWarmupState::Failed("workspace ownership lost at publish".to_owned()),
                );
                return super::WorkspaceSearchApply::Superseded;
            }
            Ok(bsl_search::FenceOutcome::Released) => {
                tracing::warn!("workspace overlay semantic warmup released");
                return super::WorkspaceSearchApply::Released;
            }
            Err(error) => {
                tracing::warn!("workspace overlay semantic warmup failed: {error}");
                Self::set_overlay_warmup_state(
                    overlay_warmup,
                    OverlayWarmupState::Failed(error.to_string()),
                );
                return super::WorkspaceSearchApply::OperationError(error.to_string());
            }
        };

        // Capture plan stats BEFORE `plan`/`new_embeddings` are consumed by the publish below, so
        // the warmup outcome can report how many local files were embedded (and how many chunks)
        // — and, for an incomplete pass, exactly how much the pass could not vouch for.
        let embedded = new_embeddings.len();
        let scan_unreadable = plan.scan_unreadable();
        let scan_canonical_fallbacks = plan.scan_canonical_fallbacks();

        // The stop/ownership signal is honoured even when the embed set was EMPTY (the
        // in-batch checks never ran): a stopped driver must not publish anything.
        if !keep_going() {
            tracing::warn!("overlay warmup: stopped before publish");
            Self::set_overlay_warmup_state(
                overlay_warmup,
                OverlayWarmupState::Failed("workspace ownership lost at publish".to_owned()),
            );
            return super::WorkspaceSearchApply::Released;
        }
        let mut prepared = match search_engine.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(engine) => match engine.stage_workspace_overlay_publication(
                    plan,
                    new_embeddings,
                    &dirty_before,
                ) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        Self::set_overlay_warmup_state(
                            overlay_warmup,
                            OverlayWarmupState::Failed(error.to_string()),
                        );
                        return super::WorkspaceSearchApply::OperationError(error.to_string());
                    }
                },
                None => {
                    return super::WorkspaceSearchApply::OperationError(
                        "engine unavailable".to_owned(),
                    );
                }
            },
            Err(error) => {
                return super::WorkspaceSearchApply::OperationError(format!(
                    "engine lock error: {error}"
                ));
            }
        };
        let published = loop {
            if !keep_going() {
                return super::WorkspaceSearchApply::Released;
            }
            #[cfg(test)]
            if FORCE_OVERLAY_PUBLICATION_REFUSALS
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                if retry_transient() {
                    continue;
                }
                return super::WorkspaceSearchApply::TransientRefusal;
            }
            match Self::apply_workspace_search_checkpointed(
                search_engine,
                lease,
                |engine, checkpoint| {
                    engine.apply_staged_workspace_overlay_publication(&mut prepared, checkpoint)
                },
            ) {
                super::WorkspaceSearchApply::Applied(published) => break Ok(published),
                super::WorkspaceSearchApply::TransientRefusal if retry_transient() => {}
                super::WorkspaceSearchApply::TransientRefusal => {
                    return super::WorkspaceSearchApply::TransientRefusal
                }
                super::WorkspaceSearchApply::Superseded => {
                    Self::set_overlay_warmup_state(
                        overlay_warmup,
                        OverlayWarmupState::Failed(
                            "workspace ownership lost at publish".to_owned(),
                        ),
                    );
                    return super::WorkspaceSearchApply::Superseded;
                }
                super::WorkspaceSearchApply::Released => {
                    return super::WorkspaceSearchApply::Released;
                }
                super::WorkspaceSearchApply::OperationError(error) => break Err(error),
            }
        };
        match published {
            Ok(bsl_search::PublishOutcome::Applied {
                gate_deferred,
                persist_ok,
                overlay_files: applied_overlay_files,
                deleted_files,
                unread_keys,
            }) => {
                tracing::info!("workspace overlay semantic warmup complete");
                let outcome = Self::warmup_outcome(
                    applied_overlay_files == 0 && deleted_files == 0,
                    applied_overlay_files,
                    embedded,
                    scan_unreadable,
                    scan_canonical_fallbacks,
                    unread_keys + gate_deferred,
                    !persist_ok,
                );
                Self::set_overlay_warmup_state(overlay_warmup, outcome.clone());
                super::WorkspaceSearchApply::Applied(outcome)
            }
            Ok(bsl_search::PublishOutcome::Superseded) => {
                Self::set_overlay_warmup_state(overlay_warmup, OverlayWarmupState::Superseded);
                super::WorkspaceSearchApply::Applied(OverlayWarmupState::Superseded)
            }
            Err(error) => {
                Self::set_overlay_warmup_state(
                    overlay_warmup,
                    OverlayWarmupState::Failed(error.to_string()),
                );
                super::WorkspaceSearchApply::OperationError(error.to_string())
            }
        }
    }

    pub(super) fn set_overlay_warmup_state(
        overlay_warmup: &Arc<Mutex<OverlayWarmupState>>,
        state: OverlayWarmupState,
    ) {
        if let Ok(mut guard) = overlay_warmup.lock() {
            *guard = state;
        }
    }

    pub(super) fn set_semantic_runtime_status(
        semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
        status: SemanticRuntimeStatus,
    ) {
        if let Ok(mut guard) = semantic_runtime.lock() {
            *guard = status;
        }
    }
    /// After the graph publishes a fresh build, re-render the stored graph context of any
    /// search chunk whose owning file was marked context-dirty by an `.xml` drift, so a
    /// metadata edit becomes visible without waiting for the owning `.bsl` to change. This
    /// runs on the graph's background publish thread — never on a query path — because the
    /// freshly published graph is the "caught up" state a re-render must read. `build_start_seq`
    /// (captured when this build STARTED) bounds the marks it may clear: only drifts this
    /// build already reflects, never one stamped after it began, so a mark is never cleared
    /// against a graph that predates its `.xml` change. Opens the just-published graph
    /// database for the render; when the graph is unavailable nothing is cleared and the
    /// marks persist for the next publish. Never touches the resident mutex.
    /// Returns whether the render actually ran to completion. The caller turns that into its
    /// own obligation: a requested topology refresh is re-raised for the next publish, and a
    /// leftover-marks pickup keeps its captured bound. Reporting a skip as done would discharge
    /// an obligation nothing has met, so every path answers for what was DONE — never for what
    /// was asked.
    #[cfg(test)]
    fn refresh_search_contexts_after_graph(
        engine: &SharedSearchEngine,
        workspace_root: &Path,
        semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
        index_progress: &Arc<IndexProgress>,
        embed_flight: &Arc<EmbedFlight>,
        lease: &crate::workspace_lease::WorkspaceLease,
        signal: crate::graph::GraphPublishSignal,
    ) -> bool {
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(workspace_root);
        Self::refresh_search_contexts_after_graph_with_cache(
            engine,
            &cache,
            semantic_runtime,
            index_progress,
            embed_flight,
            lease,
            signal,
            super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::type_complexity,
        reason = "the host passes one retry budget to the existing worker contract; a wrapper would only rename these inputs"
    )]
    fn refresh_search_contexts_after_graph_with_cache(
        engine: &SharedSearchEngine,
        cache: &crate::cache::WorkspaceCacheLayout,
        semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
        index_progress: &Arc<IndexProgress>,
        embed_flight: &Arc<EmbedFlight>,
        lease: &crate::workspace_lease::WorkspaceLease,
        signal: crate::graph::GraphPublishSignal,
        publish_retry_budget: std::time::Duration,
    ) -> bool {
        let crate::graph::GraphPublishSignal {
            drift_pending,
            build_start_seq,
            topology_changed,
            topology,
            ..
        } = signal;
        // Fast-path skip (an optimization, not correctness): a follow-up reload is already
        // catching up, so let ITS publish re-render against the fresher graph. Correctness
        // does not depend on this — the `build_start_seq` bound below already prevents
        // clearing a mark against a graph that predates its drift. Nothing was rendered, so
        // the caller keeps whatever obligation it was discharging.
        if drift_pending {
            tracing::debug!(
                "graph drift still pending; deferring search context refresh to the next publish"
            );
            return false;
        }
        let graph_path = cache.graph_db_path();
        let graph_db = match crate::graph_query::GraphDb::open(&graph_path) {
            Ok(db) => db,
            Err(e) => {
                tracing::debug!("graph unavailable for search context refresh: {e}");
                return false;
            }
        };
        // The file we just opened is not necessarily the build that fired this hook: a
        // daemon of another generation may have renamed ITS graph into the same path
        // meanwhile. Contexts rendered from a foreign topology would be persisted as this
        // workspace's answers, so treat the mismatch like an unavailable graph — the marks
        // stay dirty and a later publish re-renders them from our own build.
        match graph_db.freshness_token() {
            Ok((_, fingerprint, _)) if fingerprint.topology == topology => {}
            _ => {
                tracing::warn!(
                    published_topology = topology,
                    "graph database on disk is not the published build; skipping context refresh"
                );
                return false;
            }
        }
        let provider = crate::graph_query::GraphDbContextProvider::new(graph_db);
        let refreshed = match engine.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(engine) => {
                    let mut apply = |operation: &mut dyn FnMut(
                        &mut dyn FnMut() -> std::ops::ControlFlow<()>,
                    )
                        -> std::ops::ControlFlow<
                        (),
                        Result<(), bsl_search::SearchError>,
                    >| {
                        Self::search_fence_outcome(lease.publish_checkpointed(operation))
                    };
                    engine.refresh_dirty_contexts_fenced(
                        &provider,
                        build_start_seq,
                        topology_changed,
                        &mut apply,
                    )
                }
                None => Err(bsl_search::SearchError::Index(
                    "workspace search engine is not published".to_owned(),
                )),
            },
            Err(error) => Err(bsl_search::SearchError::Index(format!(
                "workspace search engine lock poisoned: {error}"
            ))),
        };
        let (stats, outcome) = match refreshed {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!("could not refresh search graph contexts: {error}");
                return false;
            }
        };
        if stats.paths_marked > 0 {
            tracing::info!(
                count = stats.paths_marked,
                "topology changed; re-rendering every document's graph context"
            );
        }
        if stats.paths_cleared > 0 {
            tracing::info!(
                paths = stats.paths_cleared,
                chunks = stats.chunks_updated,
                cleared_embeddings = stats.cleared_embeddings,
                "search graph context refreshed after graph publish"
            );
        }
        let topology_handled = matches!(outcome, bsl_search::FenceOutcome::Applied(()));
        // Re-rendered chunks had their live embedding NULLed; without a re-embed they serve
        // the OLD vector in-process and vanish from semantic results after a restart until
        // the boot pass. Kick the same background embed machinery workspace init uses.
        if stats.cleared_embeddings > 0
            && !matches!(
                outcome,
                bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released
            )
        {
            Self::kick_context_reembed(
                engine,
                semantic_runtime,
                index_progress,
                embed_flight,
                lease,
                publish_retry_budget,
            );
        }
        topology_handled
    }

    /// After a context refresh NULLed live embeddings, re-embed the pending chunks through the
    /// shared embed single-flight — the same pass workspace boot uses, so the two never race an
    /// index swap. When no embedder is configured the kick returns without claiming (lexical
    /// results, already fresh from the refresh, are the whole story).
    fn kick_context_reembed(
        engine: &SharedSearchEngine,
        semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
        index_progress: &Arc<IndexProgress>,
        embed_flight: &Arc<EmbedFlight>,
        lease: &crate::workspace_lease::WorkspaceLease,
        publish_retry_budget: std::time::Duration,
    ) {
        // A no-embedder engine has nothing to re-embed; resolve the DB path only if semantic
        // is live so we never claim the flight for a pass that would do nothing.
        let db_path = engine.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|engine| engine.has_semantic().then(|| engine.db_path().to_path_buf()))
        });
        let Some(db_path) = db_path else { return };
        let Some(config) = Self::embedding_config() else { return };

        Self::spawn_embed_pass(
            Arc::clone(engine),
            Arc::clone(semantic_runtime),
            Arc::clone(index_progress),
            Arc::clone(embed_flight),
            lease.clone(),
            db_path,
            config,
            publish_retry_budget,
        );
    }

    /// The ONE background embed entry for the workspace: both boot (initial NULL embeddings)
    /// and the post-refresh kick funnel through here so they share a single claim. The caller
    /// that wins the claim runs the pass in a loop, re-running while a rerun was requested — so
    /// a caller that lost the claim (its later-NULLed chunks absorbed) is guaranteed a later
    /// iteration sees them.
    ///
    /// INVARIANT: because `embed_pending_chunks_standalone` re-selects NULL chunks from the
    /// store on every iteration and the `set_vector_index` swap happens per iteration, the LAST
    /// iteration installs an index reflecting the latest store state — an older caller can never
    /// install a stale index over a newer one.
    #[allow(
        clippy::too_many_arguments,
        reason = "the host passes one retry budget to the existing worker contract; a wrapper would only rename these inputs"
    )]
    pub(super) fn spawn_embed_pass(
        engine: SharedSearchEngine,
        semantic_runtime: Arc<Mutex<SemanticRuntimeStatus>>,
        index_progress: Arc<IndexProgress>,
        embed_flight: Arc<EmbedFlight>,
        lease: crate::workspace_lease::WorkspaceLease,
        db_path: PathBuf,
        config: bsl_search::SearchConfig,
        publish_retry_budget: std::time::Duration,
    ) {
        // The one search write a superseded daemon must NOT make. A chunk's embedding is stored
        // as a bare blob against its id, with no record of the model that produced it, and the
        // embedding configuration is one of the axes that forks a daemon generation in the first
        // place — so two generations filling the same NULL rows can leave vectors from the older
        // daemon's model in the newer one's index, silently at equal dimensions and unfixably at
        // unequal ones (a non-NULL row is never re-embedded). Chunks and FTS text stay ungated:
        // both generations derive them from the same files, so duplicating them costs work, not
        // correctness.
        let _ = lease.owns_caches();
        if lease.is_superseded() || lease.is_released() {
            tracing::debug!(
                "another daemon generation owns this workspace's derived caches; \
                 skipping the embedding pass"
            );
            return;
        }
        if !embed_flight.claim() {
            // A pass is already running; it will loop again and absorb these NULL chunks.
            return;
        }

        Self::set_semantic_runtime_status(&semantic_runtime, SemanticRuntimeStatus::Indexing);
        // Clone the handles the thread owns; the originals stay behind for the spawn-error path.
        let engine = Arc::clone(&engine);
        let runtime = Arc::clone(&semantic_runtime);
        let flight = Arc::clone(&embed_flight);
        // Checked between batches, not just before the pass: this runs for hours on a large
        // configuration, and a generation that takes the workspace over meanwhile must not keep
        // finding this daemon's vectors — from a possibly different model — arriving in its
        // index. Uncached, because a batch is seconds and the cached verdict's two-second
        // "yes" is most of one.
        let worker_lease = lease.clone();
        let keep_running = {
            let lease = lease.clone();
            move || !lease.is_superseded() && !lease.is_released()
        };
        let spawned =
            std::thread::Builder::new().name("bsl-search-embed".to_owned()).spawn(move || {
                // Restore the flight claim on any abnormal exit; a clean release calls
                // `disarm()` first so this never stomps a later owner that already re-claimed.
                let mut claim_guard = EmbedClaimGuard::new(Arc::clone(&flight));
                // Restore the runtime status on any abnormal exit so it never sticks `Indexing`.
                let mut status_guard = EmbedStatusGuard::new(Arc::clone(&runtime));
                #[cfg(test)]
                let mut apply_count = 0usize;
                let mut publish_retry =
                    RetryWindow::with_budget(RetryOwner::OverlayEmbedding, publish_retry_budget);
                tracing::info!(
                    publish_retry_budget_secs = publish_retry_budget.as_secs(),
                    "background embedding pass started"
                );
                loop {
                    if publish_retry.expired(Instant::now()) {
                        Self::set_semantic_runtime_status(
                            &runtime,
                            SemanticRuntimeStatus::Failed(
                                "embedding publication retry budget exhausted".to_owned(),
                            ),
                        );
                        status_guard.finish();
                        return;
                    }
                    flight.begin_pass();
                    #[cfg(test)]
                    if FORCE_EMBED_PASS_PANIC.load(Ordering::SeqCst) {
                        panic!("forced embedding pass panic");
                    }
                    let mut retry_refusal = false;
                    match SearchEngine::embed_pending_chunks_fenced_retrying(
                        &db_path,
                        &config,
                        Some(&index_progress),
                        Some(&keep_running),
                        |operation| {
                            #[cfg(test)]
                            {
                                apply_count += 1;
                                if let Some(hook) = EMBED_FENCE_HOOK
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .as_mut()
                                {
                                    hook(EmbedFencePoint::Apply(apply_count));
                                }
                                let forced = if apply_count == 1 {
                                    &FORCE_EMBED_PREFLIGHT_REFUSALS
                                } else {
                                    &FORCE_EMBED_PUBLICATION_REFUSALS
                                };
                                if forced
                                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                                        remaining.checked_sub(1)
                                    })
                                    .is_ok()
                                {
                                    return bsl_search::FenceOutcome::TransientRefusal;
                                }
                            }
                            Self::search_fence_outcome(
                                worker_lease.publish_short(&mut (), |_| operation()),
                            )
                        },
                        || {
                            let now = Instant::now();
                            let bounded_delay =
                                super::overlay_retry::retry_delay(publish_retry.streak());
                            let delay = match publish_retry.refused(now, bounded_delay) {
                                RetryDecision::RetryAfter(delay) => delay,
                                RetryDecision::Stop(_) => return false,
                            };
                            std::thread::sleep(delay);
                            !publish_retry.expired(Instant::now())
                        },
                    ) {
                        Ok(bsl_search::FenceOutcome::Applied(index)) => {
                            #[cfg(test)]
                            if let Some(hook) = EMBED_FENCE_HOOK
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .as_mut()
                            {
                                hook(EmbedFencePoint::Swap);
                            }
                            let mut prepared_index = Some(index);
                            let swapped = match engine.lock() {
                                Ok(mut guard) => match guard.as_mut() {
                                    Some(engine) => worker_lease.publish_short(
                                        &mut prepared_index,
                                        |prepared| {
                                            engine.set_vector_index(
                                                prepared.take().expect("prepared index exists"),
                                            );
                                            Ok::<_, std::convert::Infallible>(())
                                        },
                                    ),
                                    None => {
                                        tracing::warn!("embedding pass: engine unavailable");
                                        Self::set_semantic_runtime_status(
                                            &runtime,
                                            SemanticRuntimeStatus::Failed(
                                                "embedding engine unavailable".to_owned(),
                                            ),
                                        );
                                        status_guard.finish();
                                        return;
                                    }
                                },
                                Err(e) => {
                                    tracing::warn!("embedding pass: engine lock error: {e}");
                                    Self::set_semantic_runtime_status(
                                        &runtime,
                                        SemanticRuntimeStatus::Failed(format!(
                                            "embedding engine lock error: {e}"
                                        )),
                                    );
                                    status_guard.finish();
                                    return;
                                }
                            };
                            match swapped {
                                crate::workspace_lease::LeaseOperationOutcome::Applied(()) => {
                                    #[cfg(test)]
                                    {
                                        let mut hook = EMBED_POST_PASS_HOOK
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner());
                                        if let Some(h) = hook.as_mut() {
                                            h(&db_path);
                                        }
                                    }
                                }
                                crate::workspace_lease::LeaseOperationOutcome::TransientRefusal => {
                                    retry_refusal = true;
                                }
                                crate::workspace_lease::LeaseOperationOutcome::Superseded
                                | crate::workspace_lease::LeaseOperationOutcome::Released => {
                                    Self::set_semantic_runtime_status(
                                        &runtime,
                                        SemanticRuntimeStatus::Failed(
                                            "embedding stopped after workspace ownership was superseded"
                                                .to_owned(),
                                        ),
                                    );
                                    status_guard.finish();
                                    return;
                                }
                                crate::workspace_lease::LeaseOperationOutcome::OperationError(
                                    error,
                                ) => {
                                    Self::set_semantic_runtime_status(
                                        &runtime,
                                        SemanticRuntimeStatus::Failed(format!(
                                            "embedding publication failed: {error:?}"
                                        )),
                                    );
                                    status_guard.finish();
                                    return;
                                }
                            }
                        }
                        Ok(bsl_search::FenceOutcome::TransientRefusal) => {
                            retry_refusal = true;
                        }
                        Ok(
                            bsl_search::FenceOutcome::Superseded
                            | bsl_search::FenceOutcome::Released,
                        ) => {
                            Self::set_semantic_runtime_status(
                                &runtime,
                                SemanticRuntimeStatus::Failed(
                                    "embedding stopped after workspace ownership was superseded"
                                        .to_owned(),
                                ),
                            );
                            status_guard.finish();
                            return;
                        }
                        Err(e) => {
                            tracing::warn!("background embedding pass failed: {e}");
                            Self::set_semantic_runtime_status(
                                &runtime,
                                SemanticRuntimeStatus::Failed(format!(
                                    "background embedding failed: {e}"
                                )),
                            );
                            status_guard.finish();
                            return;
                        }
                    }
                    let retry_wait = if retry_refusal {
                        let delay = super::overlay_retry::retry_delay(publish_retry.streak());
                        match publish_retry.refused(Instant::now(), delay) {
                            RetryDecision::RetryAfter(delay) => {
                                let _ = flight.claim();
                                Some(delay)
                            }
                            RetryDecision::Stop(_) => {
                                Self::set_semantic_runtime_status(
                                    &runtime,
                                    SemanticRuntimeStatus::Failed(
                                        "embedding publication retry budget exhausted".to_owned(),
                                    ),
                                );
                                status_guard.finish();
                                return;
                            }
                        }
                    } else {
                        publish_retry.complete();
                        None
                    };
                    if !flight.finish_pass() {
                        // No rerun requested → the claim was released under the flight lock.
                        claim_guard.disarm();
                        Self::set_semantic_runtime_status(&runtime, SemanticRuntimeStatus::Ready);
                        status_guard.finish();
                        tracing::info!("background embedding pass complete; semantic index live");
                        return;
                    }
                    // A rerun was requested during the pass; loop again for its NULL chunks.
                    if let Some(delay) = retry_wait {
                        std::thread::sleep(delay);
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("failed to spawn embedding thread: {e}");
            embed_flight.release();
            Self::set_semantic_runtime_status(
                &semantic_runtime,
                SemanticRuntimeStatus::Failed(format!("could not spawn embedding thread: {e}")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET;
    use super::super::test_support::{
        mock_embedding_env, mock_semantic_config, spawn_mock_embedding_server, write_common_module,
        ENV_LOCK,
    };
    use super::SharedState;
    use bsl_search::SearchEngine;
    use std::fs;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tempfile::tempdir;

    struct ResetEmbeddingRefusals;

    impl Drop for ResetEmbeddingRefusals {
        fn drop(&mut self) {
            super::FORCE_EMBED_PREFLIGHT_REFUSALS.store(0, Ordering::SeqCst);
            super::FORCE_EMBED_PUBLICATION_REFUSALS.store(0, Ordering::SeqCst);
        }
    }

    fn seed_pending_embedding(path: &std::path::Path) {
        use bsl_search::{Chunk, ChunkKind, Store};

        Store::open(path)
            .unwrap()
            .reindex_file_with_context(
                bsl_search::CONFIGURATION_ROOT_ID,
                "A.bsl",
                b"h",
                &[Chunk {
                    kind: ChunkKind::Procedure,
                    name: "Альфа".to_owned(),
                    is_export: true,
                    annotations: Vec::new(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура Альфа()\nКонецПроцедуры".to_owned(),
                }],
                None,
                Some(&[None]),
            )
            .unwrap();
    }

    fn wait_for_embed_flight(flight: &super::EmbedFlight) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while flight.is_in_flight() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!flight.is_in_flight(), "embedding pass did not finish");
    }

    fn spawn_counting_embedding_server() -> (String, Arc<AtomicUsize>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut chunk = [0; 2048];
                let mut header_end = None;
                let mut content_len = 0;
                while let Ok(read) = stream.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if header_end.is_none() {
                        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                            header_end = Some(end + 4);
                            let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                            content_len = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse().ok())
                                .unwrap_or(0);
                        }
                    }
                    if header_end.is_some_and(|end| request.len() >= end + content_len) {
                        break;
                    }
                }
                observed.fetch_add(1, Ordering::SeqCst);
                let inputs = header_end
                    .and_then(|end| {
                        serde_json::from_slice::<serde_json::Value>(&request[end..]).ok()
                    })
                    .and_then(|value| value.get("input")?.as_array().map(Vec::len))
                    .unwrap_or(1);
                let data: Vec<_> = (0..inputs)
                    .map(|index| serde_json::json!({"index": index, "embedding": [1.0, 0.0, 0.0]}))
                    .collect();
                let body = serde_json::json!({"data": data}).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), calls)
    }

    fn start_test_embed(
        cache: &crate::cache::WorkspaceCacheLayout,
        server: &str,
        budget: Duration,
    ) -> (
        super::SharedSearchEngine,
        Arc<Mutex<crate::state::SemanticRuntimeStatus>>,
        Arc<super::EmbedFlight>,
    ) {
        cache.ensure().unwrap();
        let db_path = cache.search_db_path();
        seed_pending_embedding(&db_path);
        let engine = Arc::new(Mutex::new(Some(
            SearchEngine::new(&db_path, mock_semantic_config(server)).unwrap(),
        )));
        let runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let flight = super::EmbedFlight::new();
        SharedState::spawn_embed_pass(
            Arc::clone(&engine),
            Arc::clone(&runtime),
            bsl_search::IndexProgress::new(),
            Arc::clone(&flight),
            crate::workspace_lease::WorkspaceLease::claim_cache(cache),
            db_path,
            mock_semantic_config(server),
            budget,
        );
        (engine, runtime, flight)
    }

    /// The extension topology recorded in the graph database a test just built — what a real
    /// publish would put in its signal, and what the refresh checks the file against.
    fn built_graph_topology(workspace: &std::path::Path) -> u64 {
        crate::graph_query::GraphDb::open(&crate::cache::graph_db_path(workspace))
            .expect("graph database built by the test")
            .freshness_token()
            .expect("graph database carries its freshness token")
            .1
            .topology
    }

    /// The publish hook the leftover-pickup tests drive: the real context refresh over a shared
    /// engine, reporting back the outcome the graph uses to decide whether the obligation was
    /// discharged. Every completed fire appends the bound it ran with to `fire_bounds`, so a test
    /// waits for the fire it needs instead of for a wall clock.
    fn leftover_test_hook(
        engine_arc: &super::SharedSearchEngine,
        workspace: &std::path::Path,
        fire_bounds: &Arc<Mutex<Vec<i64>>>,
    ) -> Arc<
        dyn Fn(crate::graph::GraphPublishSignal) -> crate::graph::GraphPublishOutcome + Send + Sync,
    > {
        let engine_arc = Arc::clone(engine_arc);
        let workspace = workspace.to_path_buf();
        let fire_bounds = Arc::clone(fire_bounds);
        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        Arc::new(move |signal: crate::graph::GraphPublishSignal| {
            let bound = signal.build_start_seq;
            let handled = SharedState::refresh_search_contexts_after_graph(
                &engine_arc,
                &workspace,
                &semantic_runtime,
                &index_progress,
                &embed_flight,
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                signal,
            );
            fire_bounds.lock().unwrap().push(bound);
            crate::graph::GraphPublishOutcome { topology_handled: handled, roots_handled: true }
        })
    }

    fn write_root_layout(workspace: &std::path::Path, include_extension: bool) {
        let configuration = workspace.join("cf");
        fs::create_dir_all(&configuration).unwrap();
        fs::write(configuration.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(&configuration, "Основа", "Процедура Основа() Экспорт\nКонецПроцедуры");
        let extension = workspace.join("ext/live");
        fs::create_dir_all(&extension).unwrap();
        fs::write(extension.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(
            &extension,
            "Расширение",
            "Процедура Расширение() Экспорт\nКонецПроцедуры",
        );
        let config = if include_extension {
            "[source]\nroot = \"cf\"\nextensions = [{ name = \"live\", path = \"ext/live\" }]\n"
        } else {
            "[source]\nroot = \"cf\"\nextensions = []\n"
        };
        fs::write(workspace.join("bsl-analyzer.toml"), config).unwrap();
    }

    struct BootGraphProvider;

    impl bsl_search::GraphContextProvider for BootGraphProvider {
        fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
            Some("boot graph".to_owned())
        }
    }

    fn wait_for_root_count(engine: &super::SharedSearchEngine, expected: usize) {
        for _ in 0..400 {
            let count = engine.lock().ok().and_then(|guard| {
                guard
                    .as_ref()
                    .and_then(SearchEngine::workspace_roots)
                    .map(|roots| roots.entries().count())
            });
            if count == Some(expected) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("search root table did not reach {expected} entries");
    }

    /// The production publish hook, not a hand-called transition, installs and removes an
    /// extension from the root table carried by the graph's exact project snapshot.
    #[test]
    fn production_publish_hook_transitions_live_search_roots() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_root_layout(&workspace, true);

        let db_path = crate::cache::search_db_path(&workspace);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let (boot_roots, _) =
            bsl_search::WorkspaceRoots::build(&workspace, &workspace.join("cf"), &[]);
        engine.initialize_workspace_roots(boot_roots).unwrap();
        let boot_provider: Arc<dyn bsl_search::GraphContextProvider> = Arc::new(BootGraphProvider);
        engine.set_graph_context_provider(Arc::clone(&boot_provider));
        let engine: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Disabled));
        let progress = bsl_search::IndexProgress::new();
        let flight = super::EmbedFlight::new();
        let hook = SharedState::build_publish_hook(
            Arc::clone(&engine),
            crate::cache::WorkspaceCacheLayout::for_workspace(&workspace),
            Arc::clone(&semantic_runtime),
            Arc::clone(&progress),
            Arc::clone(&flight),
            None,
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        let graph =
            crate::graph::GraphState::for_workspace(workspace.clone()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_for_root_count(&engine, 2);
        let guard = engine.lock().unwrap();
        let published_engine = guard.as_ref().unwrap();
        assert!(published_engine.workspace_roots().unwrap().contains_id("ext/live"));
        let published_provider = published_engine.graph_context_provider().unwrap();
        assert!(
            !Arc::ptr_eq(&boot_provider, &published_provider),
            "the root transition must install the provider of the published graph artifact"
        );
        drop(guard);

        write_root_layout(&workspace, false);
        graph.nudge_project_reload();
        wait_for_root_count(&engine, 1);
        assert!(!engine
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_roots()
            .unwrap()
            .contains_id("ext/live"));
    }

    /// The root transition reads the graph the daemon actually published. With the cache
    /// moved out of the source tree there is no `<workspace>/.build` to fall back to, so a
    /// provider keyed on the workspace instead of the layout would never find the artifact
    /// and the root table would stay frozen at its boot contents.
    #[test]
    fn publish_hook_transitions_roots_when_the_cache_lives_outside_the_workspace() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_root_layout(&workspace, true);
        let external = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::from_root(external.path().to_path_buf());
        cache.ensure().unwrap();

        let mut engine = SearchEngine::fts_only(&cache.search_db_path()).unwrap();
        let (boot_roots, _) =
            bsl_search::WorkspaceRoots::build(&workspace, &workspace.join("cf"), &[]);
        engine.initialize_workspace_roots(boot_roots).unwrap();
        engine.set_graph_context_provider(Arc::new(BootGraphProvider));
        let engine: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let hook = SharedState::build_publish_hook(
            Arc::clone(&engine),
            cache.clone(),
            Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Disabled)),
            bsl_search::IndexProgress::new(),
            super::EmbedFlight::new(),
            None,
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        let graph = crate::graph::GraphState::for_workspace_with_cache(workspace.clone(), cache)
            .with_publish_hook(hook);
        graph.ensure_loading();

        wait_for_root_count(&engine, 2);
        assert!(engine
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_roots()
            .unwrap()
            .contains_id("ext/live"));
        assert!(!workspace.join(".build").exists(), "the source tree stays untouched");
    }

    /// Both edges bracket the complete second SourceSet scan plus file reads. `try_lock`
    /// succeeding there proves production never performs that filesystem validation while
    /// holding the outer engine mutex that serializes `search_code`.
    #[test]
    fn production_root_validation_runs_off_the_engine_mutex() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_root_layout(&workspace, true);
        let db_path = crate::cache::search_db_path(&workspace);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let mut search = SearchEngine::fts_only(&db_path).unwrap();
        let (boot_roots, _) =
            bsl_search::WorkspaceRoots::build(&workspace, &workspace.join("cf"), &[]);
        search.initialize_workspace_roots(boot_roots).unwrap();
        let engine: super::SharedSearchEngine = Arc::new(Mutex::new(Some(search)));
        let observed = Arc::new(AtomicUsize::new(0));
        let observed_in_hook = Arc::clone(&observed);
        let checked_engine = Arc::clone(&engine);
        super::ROOT_VALIDATION_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_| {
                assert!(
                    checked_engine.try_lock().is_ok(),
                    "filesystem validation ran while the outer search-engine mutex was held"
                );
                observed_in_hook.fetch_add(1, Ordering::SeqCst);
            }));
        });

        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        graph.ensure_loading();
        for _ in 0..400 {
            if matches!(graph.status(), crate::graph::GraphStatus::Ready { .. }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(graph.status(), crate::graph::GraphStatus::Ready { .. }));
        let signal = crate::graph::GraphPublishSignal {
            drift_pending: false,
            build_start_seq: 0,
            topology_changed: false,
            topology: built_graph_topology(&workspace),
            roots_refresh_requested: true,
            workspace_roots: crate::project::at(&workspace)
                .ok()
                .map(|project| crate::project::workspace_roots(&project, &[]).0),
        };
        let outcome = SharedState::refresh_search_roots_after_graph(
            &engine,
            &crate::cache::WorkspaceCacheLayout::for_workspace(&workspace),
            &AtomicU64::new(0),
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &signal,
        );
        super::ROOT_VALIDATION_HOOK.with(|hook| *hook.borrow_mut() = None);

        assert!(outcome.0, "the validated transition must apply");
        assert_eq!(observed.load(Ordering::SeqCst), 2, "both validation edges were observed");
    }

    /// An event delivered after filesystem validation but before the final engine-lock claim
    /// supersedes the plan. Without the hub fence the old table can consume and drop an event in
    /// a newly-added root, after which stale planned bytes would be published with no retry debt.
    #[test]
    fn event_across_root_validation_keeps_the_old_table_and_retry_obligation() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_root_layout(&workspace, true);
        let db_path = crate::cache::search_db_path(&workspace);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let mut search = SearchEngine::fts_only(&db_path).unwrap();
        let (boot_roots, _) =
            bsl_search::WorkspaceRoots::build(&workspace, &workspace.join("cf"), &[]);
        search.initialize_workspace_roots(boot_roots).unwrap();
        let engine: super::SharedSearchEngine = Arc::new(Mutex::new(Some(search)));

        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        graph.ensure_loading();
        for _ in 0..400 {
            if matches!(graph.status(), crate::graph::GraphStatus::Ready { .. }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(graph.status(), crate::graph::GraphStatus::Ready { .. }));
        let signal = crate::graph::GraphPublishSignal {
            drift_pending: false,
            build_start_seq: 0,
            topology_changed: false,
            topology: built_graph_topology(&workspace),
            roots_refresh_requested: true,
            workspace_roots: crate::project::at(&workspace)
                .ok()
                .map(|project| crate::project::workspace_roots(&project, &[]).0),
        };

        let root_drift_epoch = Arc::new(AtomicU64::new(0));
        let hook_epoch = Arc::clone(&root_drift_epoch);
        super::ROOT_VALIDATION_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |started| {
                if !started {
                    // Exactly what the search sink does before processing a root-relevant batch.
                    hook_epoch.fetch_add(1, Ordering::SeqCst);
                }
            }));
        });
        let outcome = SharedState::refresh_search_roots_after_graph(
            &engine,
            &crate::cache::WorkspaceCacheLayout::for_workspace(&workspace),
            root_drift_epoch.as_ref(),
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &signal,
        );
        super::ROOT_VALIDATION_HOOK.with(|hook| *hook.borrow_mut() = None);

        assert_eq!(
            root_drift_epoch.load(Ordering::SeqCst),
            1,
            "the seam must actually cross the validation fence"
        );
        assert!(!outcome.0, "the root-only retry obligation must remain armed");
        let guard = engine.lock().unwrap();
        let roots = guard.as_ref().unwrap().workspace_roots().unwrap();
        assert_eq!(roots.entries().count(), 1, "the stale plan must not publish");
    }

    /// Field-by-field transfer into the outcome: the enum-variant check alone cannot tell
    /// correctly-carried numbers from zeros, and `NoLocalDiffs`/`Synced` stay reserved for a
    /// fully-verified pass.
    #[test]
    fn warmup_outcome_carries_the_exact_numbers() {
        use crate::state::OverlayWarmupState;
        match SharedState::warmup_outcome(true, 0, 0, 2, 1, 2, false) {
            OverlayWarmupState::Incomplete {
                unreadable,
                canonical_fallbacks,
                read_failures,
                ..
            } => {
                assert_eq!((unreadable, canonical_fallbacks, read_failures), (2, 1, 2))
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
        assert!(matches!(
            SharedState::warmup_outcome(true, 0, 0, 0, 0, 1, false),
            OverlayWarmupState::Incomplete { read_failures: 1, .. }
        ));
        assert!(matches!(
            SharedState::warmup_outcome(true, 0, 0, 0, 0, 0, false),
            OverlayWarmupState::NoLocalDiffs
        ));
        assert!(matches!(
            SharedState::warmup_outcome(false, 2, 5, 0, 0, 0, false),
            OverlayWarmupState::Synced { overlay_files: 2, embedded: 5 }
        ));
    }

    /// A stopped driver must not publish even when the embed set was EMPTY: the in-batch
    /// stop checks never ran, so the pre-publish check is the only thing standing between a
    /// shutdown and a post-handover publication.
    #[test]
    fn a_stopped_empty_embed_pass_does_not_publish() {
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let overlay_warmup = Arc::new(Mutex::new(crate::state::OverlayWarmupState::Pending));

        SharedState::run_overlay_warmup(
            &engine_arc,
            &overlay_warmup,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &|| false,
            &mut || false,
        );
        assert!(
            !engine_arc
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_retry_signals()
                .unwrap()
                .initialized,
            "a stopped pass publishes nothing"
        );
    }

    /// A stop that lands AFTER the pre-publish check still wins: the post-lock re-check is
    /// the only guard once the pre-check has passed, and an unmanaged lease's fence cannot
    /// stand in for it.
    #[test]
    fn a_stop_after_the_precheck_still_blocks_the_publication() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let overlay_warmup = Arc::new(Mutex::new(crate::state::OverlayWarmupState::Pending));

        // The first check (pre-publish) passes; the stop lands before the post-lock one.
        let calls = AtomicUsize::new(0);
        SharedState::run_overlay_warmup(
            &engine_arc,
            &overlay_warmup,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &|| calls.fetch_add(1, Ordering::SeqCst) == 0,
            &mut || false,
        );
        assert!(
            !engine_arc
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_retry_signals()
                .unwrap()
                .initialized,
            "the post-lock re-check must stop the publication"
        );
    }

    /// A warmup pass that could not SEE or READ everything must report `Incomplete` with the
    /// pass's own numbers — never `NoLocalDiffs`: an empty plan from an incomplete pass proves
    /// nothing about the working tree, and the numbers must travel from the plan, not be zeros.
    #[cfg(unix)]
    #[test]
    fn an_incomplete_warmup_pass_reports_incomplete_not_no_diffs() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let closed = workspace.join("closed");
        std::fs::create_dir(&closed).unwrap();
        std::fs::write(closed.join("Hidden.bsl"), "Процедура Скрытая()\nКонецПроцедуры").unwrap();
        let broken = workspace.join("Broken.bsl");
        std::fs::write(&broken, "Процедура Ломкая()\nКонецПроцедуры").unwrap();

        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let overlay_warmup = Arc::new(Mutex::new(crate::state::OverlayWarmupState::Pending));

        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&closed).is_ok() {
            // Running as root: permissions cannot hide anything.
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        SharedState::run_overlay_warmup(
            &engine_arc,
            &overlay_warmup,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &|| true,
            &mut || false,
        );
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o644)).unwrap();

        let outcome = overlay_warmup.lock().unwrap().clone();
        match outcome {
            crate::state::OverlayWarmupState::Incomplete {
                unreadable,
                canonical_fallbacks,
                read_failures,
                ..
            } => assert_eq!(
                (unreadable, canonical_fallbacks, read_failures),
                (1, 0, 1),
                "the outcome carries the pass's own numbers"
            ),
            other => panic!("an incomplete pass must say so, got {other:?}"),
        }
    }

    /// A CLEAN scan with an unread seen file is still not `NoLocalDiffs`: the file is proven
    /// present with unknown contents, so the outcome is `Incomplete` and the key stays dirty
    /// for the retry.
    #[cfg(unix)]
    #[test]
    fn an_unread_file_on_a_clean_scan_reports_incomplete_and_stays_dirty() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let broken = workspace.join("Broken.bsl");
        std::fs::write(&broken, "Процедура Ломкая()\nКонецПроцедуры").unwrap();

        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let overlay_warmup = Arc::new(Mutex::new(crate::state::OverlayWarmupState::Pending));

        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&broken).is_ok() {
            std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        SharedState::run_overlay_warmup(
            &engine_arc,
            &overlay_warmup,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &|| true,
            &mut || false,
        );
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o644)).unwrap();

        let outcome = overlay_warmup.lock().unwrap().clone();
        match outcome {
            crate::state::OverlayWarmupState::Incomplete {
                unreadable,
                canonical_fallbacks,
                read_failures,
                ..
            } => assert_eq!((unreadable, canonical_fallbacks, read_failures), (0, 0, 1)),
            other => panic!("an unread file must not read as no-diffs, got {other:?}"),
        }
        let dirty = engine_arc
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_overlay_dirty_paths_snapshot()
            .unwrap();
        assert!(
            dirty.keys().any(|key| key.path == "Broken.bsl"),
            "the unread key stays dirty for the retry: {dirty:?}"
        );
    }

    /// The re-embed kick: after a context refresh NULLs a chunk's embedding, the kick's
    /// background pass re-embeds it and swaps the fresh vector into the LIVE engine, so the
    /// re-contexted chunk answers semantic queries in-process (not only after a restart).
    /// Disable the spawn in `kick_context_reembed` → the live index stays empty and this fails.
    #[test]
    fn context_reembed_kick_fills_nulled_chunks_into_the_live_index() {
        use bsl_search::{Chunk, ChunkKind, Store};
        use std::time::{Duration, Instant};

        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        // A chunk with NO embedding (pending): the kick must fill it.
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Owned.bsl",
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Считать()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                    Some(&[Some("контекст".to_owned())]),
                )
                .unwrap();
        }
        let mut engine = SearchEngine::new(&db_path, mock_semantic_config(&mock)).unwrap();
        engine.set_workspace_root(dir.path());
        assert!(engine.has_semantic());
        // No vector is live yet: the query for the mock vector finds nothing.
        assert!(
            engine.search_with_embedding(&[1.0, 0.0, 0.0], 5, Some("code")).unwrap().is_empty(),
            "no vector is live before the kick",
        );
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();

        SharedState::kick_context_reembed(
            &engine_arc,
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );

        // Poll until the background pass swaps the fresh vector into the live engine.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut live = false;
        while Instant::now() < deadline {
            let hits = {
                let guard = engine_arc.lock().unwrap();
                guard
                    .as_ref()
                    .unwrap()
                    .search_with_embedding(&[1.0, 0.0, 0.0], 5, Some("code"))
                    .unwrap()
            };
            if hits.iter().any(|h| h.symbol_name == "Считать") {
                live = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(live, "the re-embed kick made the NULLed chunk answer with its new live vector");
    }

    /// Single-flight: a kick arriving while a pass is already claimed is absorbed — it spawns
    /// no second pass (the in-flight background count does not rise). Disable the
    /// `compare_exchange` claim guard → the second kick proceeds and the count rises.
    #[test]
    fn context_reembed_kick_is_single_flight() {
        use bsl_search::{Chunk, ChunkKind, Store};

        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        // A chunk that is already embedded (no pending), so a proceeding pass returns fast.
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Owned.bsl",
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Считать()\nКонецПроцедуры".to_owned(),
                    }],
                    Some(&[vec![1.0, 0.0, 0.0]]),
                    Some(&[Some("контекст".to_owned())]),
                )
                .unwrap();
        }
        let mut engine = SearchEngine::new(&db_path, mock_semantic_config(&mock)).unwrap();
        engine.set_workspace_root(dir.path());
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        // A pass is already in flight: the kick must be absorbed, spawning nothing.
        let embed_flight = super::EmbedFlight::in_flight_for_test();

        SharedState::kick_context_reembed(
            &engine_arc,
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        assert!(embed_flight.is_in_flight(), "the existing claim is untouched");
        assert!(
            embed_flight.rerun_pending(),
            "a kick while a pass is claimed is absorbed as a rerun, not spawned as a second pass",
        );
    }

    /// End-to-end lifecycle net through PRODUCTION wiring, using real components (real store,
    /// real graph build, real hub types, the real publish hook built by `build_publish_hook`)
    /// and faking only the embedder: an `.xml` drift → `apply_search_drift` marks the owned
    /// module + nudges the graph → the graph builds and its REAL publish fires the hook → the
    /// hook re-renders the stale context from the just-published graph, NULLs the embedding, and
    /// the shared embed pass re-embeds it into the live index. The refresh runs off the graph's
    /// own publish, not a hand-call, so the whole chain is exercised.
    #[test]
    fn xml_drift_lifecycle_refreshes_context_and_reembeds_into_live_index() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};
        use std::time::{Duration, Instant};

        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        // A real CommonModule so its method resolves to a graph id and the graph renders context.
        write_common_module(&workspace, "Сервер", "Функция Считать() Экспорт КонецФункции");
        let module_rel = "CommonModules/Сервер/Ext/Module.bsl";

        // The search chunk starts with a STALE stored context and a live embedding, so the
        // refresh detects a change, rewrites it, and NULLs the embedding.
        let db_path = workspace.join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    module_rel,
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Function,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Функция Считать() Экспорт КонецФункции".to_owned(),
                    }],
                    Some(&[vec![0.0, 1.0, 0.0]]),
                    Some(&[Some("СТАРЫЙ контекст".to_owned())]),
                )
                .unwrap();
        }
        let mut engine = SearchEngine::new(&db_path, mock_semantic_config(&mock)).unwrap();
        engine.set_workspace_root(&workspace);
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        // Wire the SAME publish hook the daemon builds, so the graph's real publish — not a
        // hand-call — drives the context refresh and re-embed.
        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        let hook = SharedState::build_publish_hook(
            Arc::clone(&engine_arc),
            crate::cache::WorkspaceCacheLayout::for_workspace(&workspace),
            Arc::clone(&semantic_runtime),
            Arc::clone(&index_progress),
            Arc::clone(&embed_flight),
            None,
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        let graph =
            crate::graph::GraphState::for_workspace(workspace.clone()).with_publish_hook(hook);
        // Wire the mark-seq source as the daemon does at boot, so the nudged build captures a
        // bound that covers the mark this drift stamps. An unwired build captures bound 0 and
        // clears nothing.
        graph.set_mark_seq_source(engine_arc.lock().unwrap().as_ref().unwrap().mark_seq_handle());

        // The xml drift marks the owned module context-dirty and nudges the graph; the nudged
        // build publishes and fires the hook automatically.
        let xml = workspace.join("CommonModules/Сервер.xml");
        let entry = ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };
        SharedState::apply_search_drift(&engine_arc, &[entry], false, &graph);
        {
            let guard = engine_arc.lock().unwrap();
            let dirty = guard.as_ref().unwrap().context_dirty_paths("code").unwrap();
            assert!(
                dirty.contains(&bsl_search::FileKey::configuration(module_rel)),
                "the owned module is marked context-dirty: {dirty:?}"
            );
        }
        assert_ne!(graph.status(), crate::graph::GraphStatus::Idle, "the graph nudge fired");

        // The stored context is re-rendered from the real graph (no longer the stale string),
        // and the re-embed kick swaps the fresh vector into the live index.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut refreshed = false;
        while Instant::now() < deadline {
            let (ctx, hits) = {
                let guard = engine_arc.lock().unwrap();
                let engine = guard.as_ref().unwrap();
                let docs = engine.load_indexed_documents(Some("code")).unwrap();
                let ctx = docs
                    .iter()
                    .find(|d| d.symbol_name == "Считать")
                    .and_then(|d| d.graph_context.clone());
                let hits = engine.search_with_embedding(&[1.0, 0.0, 0.0], 5, Some("code")).unwrap();
                (ctx, hits)
            };
            let ctx_fresh =
                ctx.as_deref().is_some_and(|c| c != "СТАРЫЙ контекст" && c.contains("Signature"));
            if ctx_fresh && hits.iter().any(|h| h.symbol_name == "Считать") {
                refreshed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            refreshed,
            "the xml drift re-rendered the module's graph context and re-embedded it into the live index",
        );
    }

    /// While a graph drift is still being caught up (a follow-up reload is pending), the context
    /// refresh must DEFER: consuming the marks against the pre-drift publish would clear them
    /// against stale facts. Reverting the `drift_pending` guard makes the deferred call consume
    /// the mark and the survival assertion fails.
    #[test]
    fn context_refresh_defers_marks_while_graph_drift_is_pending() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module(&workspace, "Сервер", "Функция Считать() Экспорт КонецФункции");
        let module_rel = "CommonModules/Сервер/Ext/Module.bsl";

        // A chunk with a stale stored context and NO live embedding (so consumption needs no
        // embedder — the mark, not the vector, is under test).
        let db_path = workspace.join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    module_rel,
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Function,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Функция Считать() Экспорт КонецФункции".to_owned(),
                    }],
                    None,
                    Some(&[Some("СТАРЫЙ контекст".to_owned())]),
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(&workspace);
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        // Mark the owned module context-dirty (disabled graph → the nudge is a no-op here).
        let xml = workspace.join("CommonModules/Сервер.xml");
        let entry = ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };
        SharedState::apply_search_drift(
            &engine_arc,
            &[entry],
            false,
            &crate::graph::GraphState::disabled(),
        );
        {
            let g = engine_arc.lock().unwrap();
            assert!(
                g.as_ref()
                    .unwrap()
                    .context_dirty_paths("code")
                    .unwrap()
                    .contains(&bsl_search::FileKey::configuration(module_rel)),
                "the owned module is marked context-dirty",
            );
        }

        // Build a real graph the refresh can read.
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        graph.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(graph.status(), crate::graph::GraphStatus::Ready { .. }) {
            if Instant::now() > deadline {
                panic!("graph did not build: {:?}", graph.status());
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();

        // drift_pending = true → defer: the mark SURVIVES for the follow-up reload's publish.
        // An unbounded seq (i64::MAX) isolates the drift_pending skip from the seq bound.
        SharedState::refresh_search_contexts_after_graph(
            &engine_arc,
            &workspace,
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            crate::graph::GraphPublishSignal {
                drift_pending: true,
                build_start_seq: i64::MAX,
                topology_changed: false,
                topology: built_graph_topology(&workspace),
                roots_refresh_requested: false,
                workspace_roots: None,
            },
        );
        {
            let g = engine_arc.lock().unwrap();
            assert!(
                g.as_ref()
                    .unwrap()
                    .context_dirty_paths("code")
                    .unwrap()
                    .contains(&bsl_search::FileKey::configuration(module_rel)),
                "a pending drift defers the refresh; the mark survives",
            );
        }

        // drift_pending = false → consume: the mark is cleared against the fresh graph.
        SharedState::refresh_search_contexts_after_graph(
            &engine_arc,
            &workspace,
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            crate::graph::GraphPublishSignal {
                drift_pending: false,
                build_start_seq: i64::MAX,
                topology_changed: false,
                topology: built_graph_topology(&workspace),
                roots_refresh_requested: false,
                workspace_roots: None,
            },
        );
        {
            let g = engine_arc.lock().unwrap();
            assert!(
                !g.as_ref()
                    .unwrap()
                    .context_dirty_paths("code")
                    .unwrap()
                    .contains(&bsl_search::FileKey::configuration(module_rel)),
                "with no pending drift the mark is consumed",
            );
        }
    }

    /// A graph whose mark-seq source is NOT yet wired (the boot window before
    /// `set_mark_seq_source`) captures the unwired default bound (`0`), so its publish's
    /// consume clears NOTHING — never a mark stamped before the source existed. Reverting the
    /// unwired default from `0` back to `i64::MAX` makes the publish consume the mark and the
    /// survival assertion fails.
    #[test]
    fn an_unwired_graph_publish_cannot_clear_context_marks() {
        use bsl_search::{Chunk, ChunkKind, SearchEngine, Store};
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module(&workspace, "Сервер", "Функция Считать() Экспорт КонецФункции");
        let module_rel = "CommonModules/Сервер/Ext/Module.bsl";

        let db_path = workspace.join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    module_rel,
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Function,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Функция Считать() Экспорт КонецФункции".to_owned(),
                    }],
                    None,
                    Some(&[Some("СТАРЫЙ контекст".to_owned())]),
                )
                .unwrap();
            // A mark left pending before any wired bound exists (seq 1).
            store
                .mark_context_dirty("code", bsl_search::CONFIGURATION_ROOT_ID, module_rel)
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(&workspace);
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();

        // The real refresh, wrapped so the test can wait until the publish actually fired the
        // hook (with bound 0 the consume has no observable side effect to poll on otherwise).
        let fired = Arc::new(AtomicUsize::new(0));
        let hook = {
            let engine_arc = Arc::clone(&engine_arc);
            let workspace = workspace.clone();
            let semantic_runtime = Arc::clone(&semantic_runtime);
            let index_progress = Arc::clone(&index_progress);
            let embed_flight = Arc::clone(&embed_flight);
            let fired = Arc::clone(&fired);
            Arc::new(move |signal: crate::graph::GraphPublishSignal| {
                let handled = SharedState::refresh_search_contexts_after_graph(
                    &engine_arc,
                    &workspace,
                    &semantic_runtime,
                    &index_progress,
                    &embed_flight,
                    &crate::workspace_lease::WorkspaceLease::unmanaged(),
                    signal,
                );
                fired.fetch_add(1, Ordering::SeqCst);
                crate::graph::GraphPublishOutcome { topology_handled: handled, roots_handled: true }
            })
                as Arc<
                    dyn Fn(crate::graph::GraphPublishSignal) -> crate::graph::GraphPublishOutcome
                        + Send
                        + Sync,
                >
        };

        // The graph is never wired to a mark-seq source: its build captures the unwired default.
        let graph =
            crate::graph::GraphState::for_workspace(workspace.clone()).with_publish_hook(hook);
        graph.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while fired.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(fired.load(Ordering::SeqCst) >= 1, "the build published and fired the hook");

        let guard = engine_arc.lock().unwrap();
        assert!(
            guard
                .as_ref()
                .unwrap()
                .context_dirty_paths("code")
                .unwrap()
                .contains(&bsl_search::FileKey::configuration(module_rel)),
            "an unwired build's publish (bound 0) clears no marks; the mark survives",
        );
    }

    /// Marks a PRIOR daemon run left in `context_dirty` survive the boot build's unwired publish
    /// (as the test above shows), then are consumed by the explicit leftover pickup once the
    /// mark-seq source is wired: a wired-bound consume against the already-fresh boot graph.
    /// Removing the `consume_leftover_marks` call leaves the mark stranded and the final
    /// assertion fails.
    #[test]
    fn leftover_marks_are_consumed_after_boot_wiring() {
        use bsl_search::{Chunk, ChunkKind, SearchEngine, Store};
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module(&workspace, "Сервер", "Функция Считать() Экспорт КонецФункции");
        let module_rel = "CommonModules/Сервер/Ext/Module.bsl";

        let db_path = workspace.join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    module_rel,
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Function,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Функция Считать() Экспорт КонецФункции".to_owned(),
                    }],
                    None,
                    Some(&[Some("СТАРЫЙ контекст".to_owned())]),
                )
                .unwrap();
            store
                .mark_context_dirty("code", bsl_search::CONFIGURATION_ROOT_ID, module_rel)
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(&workspace);
        engine.enable_workspace_watcher_mode();
        let mark_seq = engine.mark_seq_handle();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        let fired = Arc::new(AtomicUsize::new(0));
        let hook = {
            let engine_arc = Arc::clone(&engine_arc);
            let workspace = workspace.clone();
            let semantic_runtime = Arc::clone(&semantic_runtime);
            let index_progress = Arc::clone(&index_progress);
            let embed_flight = Arc::clone(&embed_flight);
            let fired = Arc::clone(&fired);
            Arc::new(move |signal: crate::graph::GraphPublishSignal| {
                let handled = SharedState::refresh_search_contexts_after_graph(
                    &engine_arc,
                    &workspace,
                    &semantic_runtime,
                    &index_progress,
                    &embed_flight,
                    &crate::workspace_lease::WorkspaceLease::unmanaged(),
                    signal,
                );
                fired.fetch_add(1, Ordering::SeqCst);
                crate::graph::GraphPublishOutcome { topology_handled: handled, roots_handled: true }
            })
                as Arc<
                    dyn Fn(crate::graph::GraphPublishSignal) -> crate::graph::GraphPublishOutcome
                        + Send
                        + Sync,
                >
        };

        // Boot: the graph builds and publishes while UNWIRED, so the leftover mark survives.
        let graph =
            crate::graph::GraphState::for_workspace(workspace.clone()).with_publish_hook(hook);
        graph.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while fired.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(fired.load(Ordering::SeqCst) >= 1, "the boot build published and fired the hook");
        assert!(
            engine_arc
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .context_dirty_paths("code")
                .unwrap()
                .contains(&bsl_search::FileKey::configuration(module_rel)),
            "the leftover mark survives the unwired boot publish",
        );

        // Boot wiring, then the explicit pickup: a consume bounded by the seq captured at
        // observation time clears the leftover mark synchronously (the graph is already `Ready`).
        let leftover_bound = mark_seq.load(Ordering::SeqCst);
        graph.set_mark_seq_source(mark_seq);
        graph.consume_leftover_marks(leftover_bound);

        assert!(
            !engine_arc
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .context_dirty_paths("code")
                .unwrap()
                .contains(&bsl_search::FileKey::configuration(module_rel)),
            "the leftover pickup consumed the mark with the wired bound",
        );
    }

    /// The leftover pickup must clear ONLY marks that existed when its bound was captured. A
    /// drift the running search sink stamps AFTER the capture (a higher mark seq) must survive
    /// the pickup — its own nudge→publish will resolve it against a graph that reflects it.
    /// Reverting the direct (`Ready`) fire path to a LIVE `current_mark_seq()` read makes the
    /// pickup clear the newer mark too, and the survival assertion fails.
    #[test]
    fn a_newer_mark_survives_the_leftover_pickups_captured_bound() {
        use bsl_search::{Chunk, ChunkKind, SearchEngine, Store};
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module(&workspace, "Сервер", "Функция Считать() Экспорт КонецФункции");
        let leftover_rel = "CommonModules/Сервер/Ext/Module.bsl";
        // A path the search sink will freshly mark AFTER the bound is captured; never indexed,
        // it only needs to resolve to a workspace `.bsl` to receive a higher-seq mark.
        let newer_rel = "CommonModules/Клиент/Ext/Module.bsl";

        let db_path = workspace.join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    leftover_rel,
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Function,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Функция Считать() Экспорт КонецФункции".to_owned(),
                    }],
                    None,
                    Some(&[Some("СТАРЫЙ контекст".to_owned())]),
                )
                .unwrap();
            // The leftover mark a prior run left pending (seq 1).
            store
                .mark_context_dirty("code", bsl_search::CONFIGURATION_ROOT_ID, leftover_rel)
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(&workspace);
        engine.enable_workspace_watcher_mode();
        let mark_seq = engine.mark_seq_handle();
        // The bound captured at observation time: the high-water at seq 1 (the leftover only).
        let leftover_bound = mark_seq.load(Ordering::SeqCst);
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        let fired = Arc::new(AtomicUsize::new(0));
        let hook = {
            let engine_arc = Arc::clone(&engine_arc);
            let workspace = workspace.clone();
            let semantic_runtime = Arc::clone(&semantic_runtime);
            let index_progress = Arc::clone(&index_progress);
            let embed_flight = Arc::clone(&embed_flight);
            let fired = Arc::clone(&fired);
            Arc::new(move |signal: crate::graph::GraphPublishSignal| {
                let handled = SharedState::refresh_search_contexts_after_graph(
                    &engine_arc,
                    &workspace,
                    &semantic_runtime,
                    &index_progress,
                    &embed_flight,
                    &crate::workspace_lease::WorkspaceLease::unmanaged(),
                    signal,
                );
                fired.fetch_add(1, Ordering::SeqCst);
                crate::graph::GraphPublishOutcome { topology_handled: handled, roots_handled: true }
            })
                as Arc<
                    dyn Fn(crate::graph::GraphPublishSignal) -> crate::graph::GraphPublishOutcome
                        + Send
                        + Sync,
                >
        };

        // Boot: build+publish while UNWIRED, so the leftover mark survives, then wire the source.
        let graph =
            crate::graph::GraphState::for_workspace(workspace.clone()).with_publish_hook(hook);
        graph.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while fired.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(fired.load(Ordering::SeqCst) >= 1, "the boot build published and fired the hook");
        graph.set_mark_seq_source(mark_seq);

        // The search sink stamps a NEW drift (seq 2) after the bound was captured — as it would
        // between publishing the engine and reaching its own nudge→publish.
        {
            let guard = engine_arc.lock().unwrap();
            let engine = guard.as_ref().unwrap();
            assert!(
                engine.mark_workspace_path_context_dirty(workspace.join(newer_rel)).unwrap(),
                "the newer path resolves to a workspace .bsl and receives a higher-seq mark",
            );
        }

        // The explicit pickup fires on the already-`Ready` graph with the CAPTURED bound.
        graph.consume_leftover_marks(leftover_bound);

        let guard = engine_arc.lock().unwrap();
        let dirty = guard.as_ref().unwrap().context_dirty_paths("code").unwrap();
        assert!(
            !dirty.contains(&bsl_search::FileKey::configuration(leftover_rel)),
            "the leftover mark is consumed by the pickup"
        );
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration(newer_rel)),
            "the newer mark (stamped after the captured bound) survives the pickup",
        );
    }

    /// The deferred (`Loading`) pickup path: arming while the graph is not yet `Ready` stores the
    /// captured bound, and the build's own publish re-fires the consume with THAT stored bound. A
    /// newer mark stamped after the capture must still survive. Reverting the deferred fire in
    /// `notify_published` to a live `current_mark_seq()` read (which on this unwired graph is `0`)
    /// makes the deferred consume clear nothing, so the leftover-consumed assertion fails.
    ///
    /// One publish fires the hook TWICE, and the marks may only be read once both are done: the
    /// build's own fire carries the unwired `0` bound and clears nothing, and the leftover consume
    /// that follows is the one that clears. Waiting for a fire COUNT of one would read the store
    /// while the publish is still between the two, which is a state no caller ever observes. The
    /// bounds each fire ran with are recorded and asserted, so the wait cannot be satisfied by two
    /// fires of the wrong kind.
    #[test]
    fn a_newer_mark_survives_the_deferred_leftover_pickup() {
        use bsl_search::{Chunk, ChunkKind, SearchEngine, Store};
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module(&workspace, "Сервер", "Функция Считать() Экспорт КонецФункции");
        let leftover_rel = "CommonModules/Сервер/Ext/Module.bsl";
        let newer_rel = "CommonModules/Клиент/Ext/Module.bsl";

        let db_path = workspace.join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    leftover_rel,
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Function,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Функция Считать() Экспорт КонецФункции".to_owned(),
                    }],
                    None,
                    Some(&[Some("СТАРЫЙ контекст".to_owned())]),
                )
                .unwrap();
            store
                .mark_context_dirty("code", bsl_search::CONFIGURATION_ROOT_ID, leftover_rel)
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(&workspace);
        engine.enable_workspace_watcher_mode();
        // Capture the bound (seq 1) before stamping the newer mark.
        let leftover_bound = engine.mark_seq_handle().load(Ordering::SeqCst);
        // The newer drift (seq 2), stamped before the engine is shared.
        assert!(
            engine.mark_workspace_path_context_dirty(workspace.join(newer_rel)).unwrap(),
            "the newer path resolves to a workspace .bsl and receives a higher-seq mark",
        );
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        // The bound each completed hook fire ran with, in order — the wait condition and the
        // identity of the two fires in one.
        let fire_bounds: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let hook = {
            let engine_arc = Arc::clone(&engine_arc);
            let workspace = workspace.clone();
            let semantic_runtime = Arc::clone(&semantic_runtime);
            let index_progress = Arc::clone(&index_progress);
            let embed_flight = Arc::clone(&embed_flight);
            let fire_bounds = Arc::clone(&fire_bounds);
            Arc::new(move |signal: crate::graph::GraphPublishSignal| {
                let bound = signal.build_start_seq;
                let handled = SharedState::refresh_search_contexts_after_graph(
                    &engine_arc,
                    &workspace,
                    &semantic_runtime,
                    &index_progress,
                    &embed_flight,
                    &crate::workspace_lease::WorkspaceLease::unmanaged(),
                    signal,
                );
                fire_bounds.lock().unwrap().push(bound);
                crate::graph::GraphPublishOutcome { topology_handled: handled, roots_handled: true }
            })
                as Arc<
                    dyn Fn(crate::graph::GraphPublishSignal) -> crate::graph::GraphPublishOutcome
                        + Send
                        + Sync,
                >
        };

        // The graph is `Idle` (never wired): arming the pickup here stores the bound but cannot
        // fire, so the build's own publish runs the deferred consume with the stored bound.
        let graph =
            crate::graph::GraphState::for_workspace(workspace.clone()).with_publish_hook(hook);
        graph.consume_leftover_marks(leftover_bound);
        graph.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while fire_bounds.lock().unwrap().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            *fire_bounds.lock().unwrap(),
            vec![0, leftover_bound],
            "the publish fired the build's own unwired consume, then the leftover one with the \
             stored bound",
        );

        let guard = engine_arc.lock().unwrap();
        let dirty = guard.as_ref().unwrap().context_dirty_paths("code").unwrap();
        assert!(
            !dirty.contains(&bsl_search::FileKey::configuration(leftover_rel)),
            "the deferred pickup consumed the leftover mark with the stored bound",
        );
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration(newer_rel)),
            "the newer mark (stamped after the captured bound) survives the deferred pickup",
        );
    }

    /// A context refresh that skipped its work must report itself unhandled. Its caller uses
    /// the answer to decide whether an obligation was discharged, and the leftover-marks pickup
    /// asks with no topology refresh requested — so an answer derived from what was REQUESTED
    /// rather than from what was DONE tells that caller its work is finished when nothing ran.
    /// Deriving the early returns from `topology_changed` again makes the skip below report
    /// success and the first assertion fails.
    #[test]
    fn a_context_refresh_that_could_not_run_reports_itself_unhandled() {
        use bsl_search::{Chunk, ChunkKind, SearchEngine, Store};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module(&workspace, "Сервер", "Функция Считать() Экспорт КонецФункции");
        let module_rel = "CommonModules/Сервер/Ext/Module.bsl";

        let db_path = workspace.join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    module_rel,
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Function,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Функция Считать() Экспорт КонецФункции".to_owned(),
                    }],
                    None,
                    Some(&[Some("СТАРЫЙ контекст".to_owned())]),
                )
                .unwrap();
            store
                .mark_context_dirty("code", bsl_search::CONFIGURATION_ROOT_ID, module_rel)
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(&workspace);
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        let refresh = |topology: u64| {
            SharedState::refresh_search_contexts_after_graph(
                &engine_arc,
                &workspace,
                &semantic_runtime,
                &index_progress,
                &embed_flight,
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                crate::graph::GraphPublishSignal {
                    drift_pending: false,
                    build_start_seq: i64::MAX,
                    topology_changed: false,
                    topology,
                    roots_refresh_requested: false,
                    workspace_roots: None,
                },
            )
        };

        // No graph has been built, so the database the render reads is not on disk and the
        // refresh can only skip.
        assert!(
            !crate::cache::graph_db_path(&workspace).exists(),
            "the graph database is absent, so the refresh has nothing to render from",
        );
        assert!(!refresh(0), "a refresh that could not open the graph reports itself unhandled");

        // The control: the SAME call over a graph that is there does run and reports handled,
        // so the assertion above is about the skip and not about a call that can never say yes.
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        graph.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(graph.status(), crate::graph::GraphStatus::Ready { .. }) {
            if Instant::now() > deadline {
                panic!("graph did not build: {:?}", graph.status());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(refresh(built_graph_topology(&workspace)), "a refresh that ran reports handled");
    }

    /// A leftover pickup that could not run must KEEP its obligation. The pickup discharges it
    /// with a `swap`, so a skip that reports success drops it: the marks stay in the persisted
    /// table with nothing left to clear them, and on a quiet workspace no later build comes to
    /// pick them up — those files serve a stale graph context until the daemon restarts.
    /// Deriving the refresh's early returns from `topology_changed` again makes the skip report
    /// success and the obligation vanishes.
    #[test]
    fn a_leftover_pickup_that_could_not_run_keeps_its_obligation() {
        use bsl_search::{Chunk, ChunkKind, SearchEngine, Store};
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module(&workspace, "Сервер", "Функция Считать() Экспорт КонецФункции");
        let module_rel = "CommonModules/Сервер/Ext/Module.bsl";

        let db_path = workspace.join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    module_rel,
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Function,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Функция Считать() Экспорт КонецФункции".to_owned(),
                    }],
                    None,
                    Some(&[Some("СТАРЫЙ контекст".to_owned())]),
                )
                .unwrap();
            store
                .mark_context_dirty("code", bsl_search::CONFIGURATION_ROOT_ID, module_rel)
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(&workspace);
        engine.enable_workspace_watcher_mode();
        let leftover_bound = engine.mark_seq_handle().load(Ordering::SeqCst);
        assert!(leftover_bound != 0, "the seeded mark gives the pickup a non-empty bound");
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let fire_bounds: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let graph = crate::graph::GraphState::for_workspace(workspace.clone())
            .with_publish_hook(leftover_test_hook(&engine_arc, &workspace, &fire_bounds));
        graph.ensure_loading();
        // The boot build's publish PASS, not its status: the pass ends with the same
        // `leftover_bound.swap(0)` … `fetch_max` the assertion below reads, so a wait that
        // stops at `Ready` lets the background tail steal the bound this test arms.
        crate::graph::test_support::wait_publish_pass_within(&graph, Duration::from_secs(120), 1);

        // Take the rendered-from database away, so the pickup below can only skip. The graph
        // stays `Ready`, so the pickup does fire — it just cannot do anything.
        let graph_db = crate::cache::graph_db_path(&workspace);
        fs::rename(&graph_db, graph_db.with_extension("db.taken")).unwrap();

        graph.consume_leftover_marks(leftover_bound);
        assert!(
            graph.leftover_consume_pending(),
            "a pickup that could not run leaves the obligation armed for the next publish",
        );
    }

    /// The obligation a skipped pickup kept is what actually clears the marks later: the next
    /// publish re-runs the consume with the STORED bound. The graph here is never wired to the
    /// mark-seq source, so its own publish captures the unwired `0` bound and clears nothing —
    /// the kept obligation is the only thing that can clear the leftover mark, and the assertion
    /// cannot pass through the ordinary path by accident.
    #[test]
    fn a_kept_leftover_obligation_is_discharged_by_the_next_publish() {
        use bsl_search::{Chunk, ChunkKind, SearchEngine, Store};
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module(&workspace, "Сервер", "Функция Считать() Экспорт КонецФункции");
        let module_rel = "CommonModules/Сервер/Ext/Module.bsl";

        let db_path = workspace.join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    module_rel,
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Function,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Функция Считать() Экспорт КонецФункции".to_owned(),
                    }],
                    None,
                    Some(&[Some("СТАРЫЙ контекст".to_owned())]),
                )
                .unwrap();
            store
                .mark_context_dirty("code", bsl_search::CONFIGURATION_ROOT_ID, module_rel)
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(&workspace);
        engine.enable_workspace_watcher_mode();
        let leftover_bound = engine.mark_seq_handle().load(Ordering::SeqCst);
        assert!(leftover_bound != 0, "the seeded mark gives the pickup a non-empty bound");
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let fire_bounds: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let graph = crate::graph::GraphState::for_workspace(workspace.clone())
            .with_publish_hook(leftover_test_hook(&engine_arc, &workspace, &fire_bounds));
        graph.ensure_loading();
        // The boot build's publish PASS, not its status: the pass ends with the same
        // `leftover_bound.swap(0)` … `fetch_max` the assertion below reads, so a wait that
        // stops at `Ready` lets the background tail steal the bound this test arms.
        crate::graph::test_support::wait_publish_pass_within(&graph, Duration::from_secs(120), 1);

        let graph_db = crate::cache::graph_db_path(&workspace);
        let taken = graph_db.with_extension("db.taken");
        fs::rename(&graph_db, &taken).unwrap();
        graph.consume_leftover_marks(leftover_bound);
        fs::rename(&taken, &graph_db).unwrap();
        assert!(
            graph.leftover_consume_pending(),
            "the skipped pickup kept an obligation to discharge"
        );
        // The skipped pickup fired the hook too, with this very bound. Only fires AFTER this
        // point can be the publish's, so the wait below must not count what already happened.
        let fires_before_publish = fire_bounds.lock().unwrap().len();

        // A `Ready` graph claims a reload only for a drift it can see on disk. Without one the
        // nudge is a no-op and this test would wait on a publish that never comes — passing or
        // failing on how the machine was loaded rather than on the obligation.
        write_common_module(&workspace, "Клиент", "Функция Прочесть() Экспорт КонецФункции");
        assert!(
            matches!(graph.nudge_rebuild(), crate::graph::NudgeOutcome::ReloadClaimed),
            "the drift claims the reload whose publish discharges the obligation",
        );

        // Wait for the leftover fire ITSELF: its stored bound tells it apart from the build's
        // own fire, which runs the unwired `0` and clears nothing.
        let deadline = Instant::now() + Duration::from_secs(120);
        let discharged = |bounds: &Arc<Mutex<Vec<i64>>>| {
            bounds.lock().unwrap()[fires_before_publish..].contains(&leftover_bound)
        };
        while !discharged(&fire_bounds) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            discharged(&fire_bounds),
            "the publish re-ran the consume with the stored bound; fires so far: {:?}",
            fire_bounds.lock().unwrap(),
        );
        assert!(
            !engine_arc
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .context_dirty_paths("code")
                .unwrap()
                .contains(&bsl_search::FileKey::configuration(module_rel)),
            "the discharged obligation cleared the leftover mark",
        );
    }

    /// The shared embed single-flight: exactly one owner runs; a caller that loses the claim
    /// records a rerun that makes the owner loop again. Reverting the loop (ignoring the rerun in
    /// `finish_pass`) makes the first `finish_pass` return false and the assertion fails.
    #[test]
    fn embed_flight_is_single_flight_with_a_rerun_loop() {
        let flight = super::EmbedFlight::new();
        assert!(flight.claim(), "the first caller wins the claim");
        flight.begin_pass();
        assert!(!flight.claim(), "a concurrent caller loses and records a rerun");
        assert!(flight.finish_pass(), "a rerun requested during the pass loops the owner again");
        flight.begin_pass();
        assert!(!flight.finish_pass(), "no rerun requested → the claim is released");
        assert!(flight.claim(), "the released flight can be claimed again");
    }

    /// A NULL chunk created AFTER the pass has read the store still gets embedded, because the
    /// owner loops on the recorded rerun and the final `set_vector_index` reflects the latest
    /// An embedding pass over a large configuration runs for hours, so checking the right to
    /// write only before it starts is not enough: a generation that takes the workspace over
    /// meanwhile must not keep finding this daemon's vectors — from a possibly different model,
    /// stored as unlabelled blobs — arriving in its index. The pass asks between batches and
    /// stops, writing neither the remaining vectors nor the persisted sidecar. Drop the
    /// `should_continue` check in `run_embedding_pass` and the chunk is embedded anyway.
    #[test]
    fn an_embedding_pass_stops_between_batches_when_the_right_to_write_is_withdrawn() {
        use bsl_search::{Chunk, ChunkKind, SearchConfig, Store};

        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        let seed = |db: &std::path::Path| {
            let mut store = Store::open(db).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "A.bsl",
                    b"ha",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Альфа".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Альфа()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                    Some(&[Some("ctx".to_owned())]),
                )
                .unwrap();
        };
        let embedded_count = |db: &std::path::Path, config: &SearchConfig| {
            let dim = config.embedder.dim.unwrap_or(1024);
            Store::open(db).unwrap().load_all_embeddings_with_generation(dim).unwrap().1.len()
        };
        seed(&db_path);
        let config = mock_semantic_config(&mock);

        SearchEngine::embed_pending_chunks_standalone(&db_path, &config, None, Some(&|| false))
            .expect("a stopped pass is not an error");
        assert_eq!(
            embedded_count(&db_path, &config),
            0,
            "a pass that may no longer write persists no vector",
        );

        // The control: the same pass with the right to write does embed it, so the assertion
        // above is about the withdrawal and not about an inert fixture.
        SearchEngine::embed_pending_chunks_standalone(&db_path, &config, None, Some(&|| true))
            .expect("the pass runs");
        assert_eq!(embedded_count(&db_path, &config), 1, "with the right to write it embeds");
    }

    /// store state. Reverting the rerun loop leaves the mid-flight chunk unembedded and it never
    /// answers the query.
    #[test]
    fn embed_pass_rerun_loop_embeds_a_chunk_nulled_mid_flight() {
        use bsl_search::{Chunk, ChunkKind, Store};
        use std::time::{Duration, Instant};

        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        let chunk = |name: &str| Chunk {
            kind: ChunkKind::Procedure,
            name: name.to_owned(),
            is_export: true,
            annotations: vec![],
            line_start: 0,
            line_end: 1,
            text: format!("Процедура {name}()\nКонецПроцедуры"),
        };
        // Chunk A is NULL at the start; chunk B is added mid-flight by the post-pass hook.
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "A.bsl",
                    b"ha",
                    &[chunk("Альфа")],
                    None,
                    Some(&[Some("ctx".to_owned())]),
                )
                .unwrap();
        }
        let mut engine = SearchEngine::new(&db_path, mock_semantic_config(&mock)).unwrap();
        engine.set_workspace_root(dir.path());
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let embed_flight = super::EmbedFlight::new();

        // A one-shot hook fired after the first iteration installs its index: it creates a NULL
        // chunk B and contends for the claim, recording a rerun so the owner loops for B.
        struct ResetHook;
        impl Drop for ResetHook {
            fn drop(&mut self) {
                *super::EMBED_POST_PASS_HOOK.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
        }
        let _reset = ResetHook;
        {
            let flight_for_hook = Arc::clone(&embed_flight);
            let mut fired = false;
            *super::EMBED_POST_PASS_HOOK.lock().unwrap() =
                Some(Box::new(move |db: &std::path::Path| {
                    if fired {
                        return;
                    }
                    fired = true;
                    let mut store = Store::open(db).unwrap();
                    store
                        .reindex_file_with_context(
                            bsl_search::CONFIGURATION_ROOT_ID,
                            "B.bsl",
                            b"hb",
                            &[Chunk {
                                kind: ChunkKind::Procedure,
                                name: "Бета".to_owned(),
                                is_export: true,
                                annotations: vec![],
                                line_start: 0,
                                line_end: 1,
                                text: "Процедура Бета()\nКонецПроцедуры".to_owned(),
                            }],
                            None,
                            Some(&[Some("ctx".to_owned())]),
                        )
                        .unwrap();
                    flight_for_hook.claim();
                }));
        }

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let index_progress = bsl_search::IndexProgress::new();
        SharedState::spawn_embed_pass(
            Arc::clone(&engine_arc),
            semantic_runtime,
            index_progress,
            Arc::clone(&embed_flight),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            db_path.clone(),
            mock_semantic_config(&mock),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );

        // Both A and B must answer the query: A from iteration 1, B from the rerun iteration.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut both = false;
        while Instant::now() < deadline {
            let hits = {
                let guard = engine_arc.lock().unwrap();
                guard
                    .as_ref()
                    .unwrap()
                    .search_with_embedding(&[1.0, 0.0, 0.0], 5, Some("code"))
                    .unwrap()
            };
            let has_a = hits.iter().any(|h| h.symbol_name == "Альфа");
            let has_b = hits.iter().any(|h| h.symbol_name == "Бета");
            if has_a && has_b {
                both = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(both, "the rerun loop embedded the chunk created after the pass started");
    }

    #[test]
    fn failed_typed_preflight_makes_zero_network_calls() {
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _reset = ResetEmbeddingRefusals;
        let (server, calls) = spawn_counting_embedding_server();
        let dir = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        super::FORCE_EMBED_PREFLIGHT_REFUSALS.store(1, Ordering::SeqCst);

        let (_, runtime, flight) = start_test_embed(&cache, &server, Duration::ZERO);
        wait_for_embed_flight(&flight);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            &*runtime.lock().unwrap(),
            crate::state::SemanticRuntimeStatus::Failed(message)
                if message.contains("retry budget exhausted")
        ));
    }

    #[test]
    fn prepared_vectors_survive_transient_publish_without_second_call() {
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _reset = ResetEmbeddingRefusals;
        let (server, calls) = spawn_counting_embedding_server();
        let dir = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        super::FORCE_EMBED_PUBLICATION_REFUSALS.store(1, Ordering::SeqCst);

        let (engine, runtime, flight) = start_test_embed(&cache, &server, Duration::from_secs(1));
        wait_for_embed_flight(&flight);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(*runtime.lock().unwrap(), crate::state::SemanticRuntimeStatus::Ready));
        assert_eq!(engine.lock().unwrap().as_ref().unwrap().vector_count(), 1);
        assert!(bsl_search::Store::open_existing(&cache.search_db_path())
            .unwrap()
            .load_pending_embedding_documents("code")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn publication_deadline_moves_runtime_to_failed() {
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _reset = ResetEmbeddingRefusals;
        let (server, _) = spawn_counting_embedding_server();
        let dir = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        super::FORCE_EMBED_PREFLIGHT_REFUSALS.store(1, Ordering::SeqCst);

        let (_, runtime, flight) = start_test_embed(&cache, &server, Duration::ZERO);
        wait_for_embed_flight(&flight);

        assert!(matches!(
            &*runtime.lock().unwrap(),
            crate::state::SemanticRuntimeStatus::Failed(message)
                if message.contains("retry budget exhausted")
        ));
    }

    #[test]
    fn embedding_fence_distinguishes_retry_from_supersession() {
        use bsl_search::{Chunk, ChunkKind, Store};
        use std::time::Instant;

        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        struct ResetHook;
        impl Drop for ResetHook {
            fn drop(&mut self) {
                *super::EMBED_FENCE_HOOK.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
        }
        let _reset = ResetHook;

        let seed = |path: &std::path::Path| {
            let mut store = Store::open(path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "A.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Альфа".to_owned(),
                        is_export: true,
                        annotations: Vec::new(),
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Альфа()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                    Some(&[None]),
                )
                .unwrap();
        };
        let sidecar = |path: &std::path::Path| {
            let mut value = path.as_os_str().to_os_string();
            value.push(".usearch.json");
            std::path::PathBuf::from(value)
        };

        for point in [
            super::EmbedFencePoint::Apply(1),
            super::EmbedFencePoint::Apply(2),
            super::EmbedFencePoint::Swap,
        ] {
            let dir = tempdir().unwrap();
            let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
            cache.ensure().unwrap();
            let db_path = cache.search_db_path();
            seed(&db_path);
            let engine = SearchEngine::new(&db_path, mock_semantic_config(&mock)).unwrap();
            let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
            let old = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
            let newer = Arc::new(Mutex::new(None));
            let newer_hook = Arc::clone(&newer);
            let cache_hook = cache.clone();
            *super::EMBED_FENCE_HOOK.lock().unwrap() = Some(Box::new(move |seen| {
                if seen == point && newer_hook.lock().unwrap().is_none() {
                    *newer_hook.lock().unwrap() =
                        Some(crate::workspace_lease::WorkspaceLease::claim_cache(&cache_hook));
                }
            }));
            let runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
            let flight = super::EmbedFlight::new();
            SharedState::spawn_embed_pass(
                Arc::clone(&engine_arc),
                Arc::clone(&runtime),
                bsl_search::IndexProgress::new(),
                Arc::clone(&flight),
                old.clone(),
                db_path.clone(),
                mock_semantic_config(&mock),
                DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            while (!old.is_superseded() || flight.is_in_flight()) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(old.is_superseded(), "takeover was observed at the requested fence");
            assert!(!flight.is_in_flight());
            assert!(matches!(
                *runtime.lock().unwrap(),
                crate::state::SemanticRuntimeStatus::Failed(_)
            ));
            let pending = Store::open_existing(&db_path)
                .unwrap()
                .load_pending_embedding_documents("code")
                .unwrap()
                .len();
            assert_eq!(
                pending,
                usize::from(point != super::EmbedFencePoint::Swap),
                "pending rows at {point:?}"
            );
            assert!(
                !sidecar(&db_path).exists() || point == super::EmbedFencePoint::Swap,
                "only takeover after persist may leave the admitted sidecar"
            );
            assert_eq!(engine_arc.lock().unwrap().as_ref().unwrap().vector_count(), 0);
            newer.lock().unwrap().take().unwrap().release();
        }
        *super::EMBED_FENCE_HOOK.lock().unwrap() = None;

        let dir = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        cache.ensure().unwrap();
        let db_path = cache.search_db_path();
        seed(&db_path);
        let engine = Arc::new(Mutex::new(Some(
            SearchEngine::new(&db_path, mock_semantic_config(&mock)).unwrap(),
        )));
        let runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let flight = super::EmbedFlight::new();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let holder = crate::workspace_lease::WorkspaceLease::hold_cache_lock_for(
            &cache,
            Duration::from_secs(3),
        );
        SharedState::spawn_embed_pass(
            Arc::clone(&engine),
            Arc::clone(&runtime),
            bsl_search::IndexProgress::new(),
            Arc::clone(&flight),
            lease.clone(),
            db_path.clone(),
            mock_semantic_config(&mock),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        while !matches!(*runtime.lock().unwrap(), crate::state::SemanticRuntimeStatus::Ready)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!lease.is_superseded());
        let final_runtime = runtime.lock().unwrap().clone();
        assert!(
            matches!(final_runtime, crate::state::SemanticRuntimeStatus::Ready),
            "transient refusal must retry to Ready, got {final_runtime:?}"
        );
        assert_eq!(engine.lock().unwrap().as_ref().unwrap().vector_count(), 1);
        holder.join().unwrap();
    }

    /// A panicking embed pass leaves the runtime `Failed`, never stuck `Indexing`, and releases
    /// the shared flight claim (RAII guards fire on unwind). Reverting the status guard leaves the
    /// runtime stuck `Indexing`.
    #[test]
    fn embed_pass_panic_leaves_status_failed_and_releases_flight() {
        use bsl_search::{Chunk, ChunkKind, Store};
        use std::time::{Duration, Instant};

        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        struct ResetPanic;
        impl Drop for ResetPanic {
            fn drop(&mut self) {
                super::FORCE_EMBED_PASS_PANIC.store(false, Ordering::SeqCst);
            }
        }
        super::FORCE_EMBED_PASS_PANIC.store(true, Ordering::SeqCst);
        let _reset = ResetPanic;

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file_with_context(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Owned.bsl",
                    b"h1",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Считать".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Считать()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                    Some(&[Some("ctx".to_owned())]),
                )
                .unwrap();
        }
        let mut engine = SearchEngine::new(&db_path, mock_semantic_config(&mock)).unwrap();
        engine.set_workspace_root(dir.path());
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        SharedState::kick_context_reembed(
            &engine_arc,
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut failed = false;
        while Instant::now() < deadline {
            let status = semantic_runtime.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if matches!(status, crate::state::SemanticRuntimeStatus::Failed(_)) {
                failed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(failed, "a panicking embed pass ends Failed, not stuck Indexing");
        // Give the guards a beat to run on unwind, then assert the claim was released.
        let deadline = Instant::now() + Duration::from_secs(5);
        while embed_flight.is_in_flight() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!embed_flight.is_in_flight(), "the flight claim is released after the panic");
    }
}
