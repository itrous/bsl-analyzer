use super::retry_window::{RetryDecision, RetryOwner, RetryWindow};
use super::types::{OverlayWarmupState, SemanticRuntimeStatus, SharedSearchEngine};
use super::SharedState;
use bsl_search::lifecycle::{
    Context as LifecycleContext, Outcome as LifecycleOutcome, Reason as LifecycleReason,
    Record as LifecycleRecord,
};
use bsl_search::{IndexProgress, SearchEngine, WorkspaceRootsTransitionOutcome};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Mirror of `state.in_flight` for the one reader that must not wait: the broker's serve
    /// loop asks on every tick, and an async loop has no business blocking on another
    /// thread's critical section, however short. Written under the same lock as the field it
    /// mirrors, so the two cannot disagree.
    in_flight_now: AtomicBool,
}

#[derive(Default)]
struct EmbedFlightState {
    in_flight: bool,
    rerun_pending: bool,
}

impl EmbedFlight {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(EmbedFlightState::default()),
            in_flight_now: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EmbedFlightState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Try to claim the flight. `true` = THIS caller won and must run the pass; `false` = a
    /// pass is already running and a rerun was recorded so it loops again for this caller's
    /// (later-NULLed) chunks.
    /// The ONE writer of the claim, and the reason it and its mirror cannot drift apart: both
    /// go through a `&mut EmbedFlightState`, which exists only while the caller holds the lock.
    /// Written by hand, the mirror is one careless statement away from landing after the
    /// guard is dropped — and an interleaved `claim` would then be blinded by that late
    /// store, leaving a running pass invisible to the broker.
    fn set_in_flight(&self, st: &mut EmbedFlightState, value: bool) {
        st.in_flight = value;
        self.publish_working(st, value);
    }

    /// The ONE writer of the mirror. `working` is what the pass is doing right now; the claim
    /// is the ceiling on it, so a pause that ends after the claim was released — the pass left
    /// while the pause was being lifted — cannot resurrect a mirror for a pass that is gone.
    fn publish_working(&self, st: &mut EmbedFlightState, working: bool) {
        self.in_flight_now.store(working && st.in_flight, Ordering::SeqCst);
    }

    fn claim(&self) -> bool {
        let mut st = self.lock();
        if st.in_flight {
            st.rerun_pending = true;
            false
        } else {
            self.set_in_flight(&mut st, true);
            true
        }
    }

    /// The pass is waiting, not working. The claim stays — no second pass may start — but the
    /// backend stops counting it as live work: a backoff sat out is no reason to hold a whole
    /// process, and this one grows to half an hour.
    fn pause(&self) {
        let mut st = self.lock();
        self.publish_working(&mut st, false);
    }

    /// The pause is over and the pass is working again.
    fn resume(&self) {
        let mut st = self.lock();
        self.publish_working(&mut st, true);
    }

    /// Start of a pass iteration: clear the rerun flag so a request arriving DURING this
    /// iteration triggers another loop rather than being swallowed.
    fn begin_pass(&self) {
        self.lock().rerun_pending = false;
    }

    /// End of a pass iteration. `true` = a rerun was requested (keep the claim, loop again);
    /// `false` = none, so the claim is released under the same lock (no wakeup can be lost).
    #[cfg(test)]
    fn finish_pass(&self) -> bool {
        self.finish_pass_with(|| {})
    }

    fn finish_pass_with(&self, publish: impl FnOnce()) -> bool {
        let mut st = self.lock();
        if st.rerun_pending {
            true
        } else {
            // Publish before releasing the claim so a new owner cannot be overwritten.
            publish();
            self.set_in_flight(&mut st, false);
            false
        }
    }

    /// Force-release the claim on an abnormal exit (panic / embed error). A leftover rerun
    /// request is harmless — the next owner clears it in `begin_pass` and runs anyway.
    fn release(&self) {
        let mut st = self.lock();
        self.set_in_flight(&mut st, false);
    }

    /// Whether a pass owns the flight right now. This is the backend's liveness signal for
    /// embedding: the claim is taken before the pass starts and released on every way out,
    /// including a panic, by [`EmbedClaimGuard`].
    pub(super) fn is_in_flight(&self) -> bool {
        self.in_flight_now.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(super) fn claim_for_test(&self) -> bool {
        self.claim()
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
            in_flight_now: AtomicBool::new(true),
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
    record: LifecycleRecord,
    finished: bool,
}

impl EmbedStatusGuard {
    fn new(runtime: Arc<Mutex<SemanticRuntimeStatus>>, record: LifecycleRecord) -> Self {
        Self { runtime, record, finished: false }
    }

    fn finish(&mut self, outcome: LifecycleOutcome) {
        self.record.outcome = outcome;
        self.record.emit(false);
        self.finished = true;
    }
}

impl Drop for EmbedStatusGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.record.outcome = LifecycleOutcome::Interrupted;
            self.record.emit(false);
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
    /// hook receives `(drift_pending, mark_bound)`: `mark_bound` bounds which marks
    /// the refresh may clear (only drifts this build already reflects), while `drift_pending`
    /// is a fast-path hint to skip a round when a fresher reload is imminent.
    // Each handle is an independent owner used by the long-lived publish closure; grouping them
    // would only move the same lifecycle dependencies behind a bag-of-fields type.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_publish_hook(
        search_engine: SharedSearchEngine,
        stop: super::OwnerStop,
        graph_store: crate::graph::GraphStore,
        owed_context_marks: crate::graph::OwedContextMarks,
        semantic_runtime: Arc<Mutex<SemanticRuntimeStatus>>,
        index_progress: Arc<IndexProgress>,
        embed_flight: Arc<EmbedFlight>,
        overlay_retry: Option<Arc<super::overlay_retry::OverlayRetry>>,
        root_drift_epoch: Arc<AtomicU64>,
        embedding_prefixes: super::types::EmbeddingPrefixes,
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
                    &stop,
                    &graph_store,
                    Some(&owed_context_marks),
                    &root_drift_epoch,
                    &lease,
                    &signal,
                );
            let topology_handled = Self::refresh_search_contexts_after_graph_with_store(
                &search_engine,
                &stop,
                &graph_store,
                &semantic_runtime,
                &index_progress,
                &embed_flight,
                &lease,
                signal,
                publish_retry_budget,
                Some(&embedding_prefixes),
            );
            // Context refresh may have NULLed more chunks, so kick after both mutations and let
            // the existing single-flights absorb all pending work in one rerun.
            if pending_collection_embeddings {
                Self::kick_context_reembed(
                    &search_engine,
                    &stop,
                    &semantic_runtime,
                    &index_progress,
                    &embed_flight,
                    &lease,
                    publish_retry_budget,
                    Some(&embedding_prefixes),
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
        stop: &super::OwnerStop,
        store: &crate::graph::GraphStore,
        owed: Option<&crate::graph::OwedContextMarks>,
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
        let Some(provider) = Self::published_graph_context_provider(
            store,
            owed,
            signal.revision,
            signal.fingerprint,
            Some(&roots),
        ) else {
            return (false, false, false);
        };
        let seed = {
            let guard = match engine.acquire_for_owner(stop) {
                Ok(guard) => Some(guard),
                Err(crate::tools::search::OwnerLockRefused::Closing) => None,
                Err(_) => {
                    tracing::warn!("search engine lock poisoned while capturing root transition");
                    None
                }
            };
            let Some(mut guard) = guard else {
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
            return match Self::apply_workspace_search(engine, stop, lease, |engine| {
                engine.replace_published_graph_context_provider(provider)
            }) {
                super::WorkspaceSearchApply::Applied(()) => (true, false, false),
                super::WorkspaceSearchApply::OperationError(error) => {
                    tracing::warn!("could not install published graph context provider: {error}");
                    (false, false, false)
                }
                super::WorkspaceSearchApply::TransientRefusal
                | super::WorkspaceSearchApply::Stopping
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
            let guard = match engine.acquire_for_owner(stop) {
                Ok(guard) => Some(guard),
                Err(crate::tools::search::OwnerLockRefused::Closing) => None,
                Err(_) => {
                    tracing::warn!("search engine lock poisoned while staging root transition");
                    None
                }
            };
            let Some(mut guard) = guard else {
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
            stop,
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
            | super::WorkspaceSearchApply::Stopping
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
        store: &crate::graph::GraphStore,
        owed: Option<&crate::graph::OwedContextMarks>,
        revision: u64,
        expected_fingerprint: crate::graph_db::GraphFp,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> Option<Arc<crate::graph_query::GraphDbContextProvider>> {
        match Self::read_published_generation(store, revision, expected_fingerprint) {
            Ok(()) => Some(Arc::new(crate::graph_query::GraphDbContextProvider::new(
                store.clone(),
                revision,
                roots,
                owed.cloned(),
            ))),
            Err(error) => {
                tracing::warn!(
                    published_revision = revision,
                    published_topology = expected_fingerprint.topology,
                    "published graph generation is not readable ({error}); skipping root transition"
                );
                None
            }
        }
    }

    /// Whether the store serves exactly the generation a publish signal announced. A graph
    /// another daemon generation renamed into the shared path meanwhile is not this
    /// workspace's publication: contexts rendered from it would carry a foreign topology.
    fn read_published_generation(
        store: &crate::graph::GraphStore,
        revision: u64,
        expected_fingerprint: crate::graph_db::GraphFp,
    ) -> Result<(), String> {
        let read = store.read(Some(revision), crate::graph::BACKGROUND_READ_WAIT, |snapshot| {
            snapshot.graph.freshness_token()
        });
        match read {
            Ok(Ok((actual_revision, fingerprint, _)))
                if actual_revision == revision && fingerprint == expected_fingerprint =>
            {
                Ok(())
            }
            Ok(Ok(_)) => Err("the served database is another generation".to_owned()),
            Ok(Err(error)) => Err(error.to_string()),
            Err(error) => Err(error.to_string()),
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
        stop: &super::OwnerStop,
        overlay_warmup: &Arc<Mutex<OverlayWarmupState>>,
        lease: &crate::workspace_lease::WorkspaceLease,
        keep_going: &dyn Fn() -> bool,
        retry_transient: &mut dyn FnMut() -> bool,
    ) -> super::WorkspaceSearchApply<OverlayWarmupState, String> {
        let cloned = match search_engine.acquire_for_owner(stop) {
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
                                OverlayWarmupState::from_search_error(&error),
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
                                OverlayWarmupState::from_search_error(&error),
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
            // Leaving is not failing: a stop refuses the admission the same way a poisoned
            // lock does, and writing `Failed` here would latch the driver's own failure flag
            // over a clean shutdown.
            Err(crate::tools::search::OwnerLockRefused::Closing) => {
                return super::WorkspaceSearchApply::Stopping;
            }
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
                    OverlayWarmupState::from_search_error(&error),
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
            // Which of the two it was decides what the caller does with it. A stop is
            // answered by leaving, with nothing written and no obligation touched; only a
            // lease that is really gone is the terminal "ownership lost" that disarms the
            // driver for good.
            if stop.is_stopped() {
                return super::WorkspaceSearchApply::Stopping;
            }
            tracing::warn!("overlay warmup: stopped before publish");
            Self::set_overlay_warmup_state(
                overlay_warmup,
                OverlayWarmupState::Failed("workspace ownership lost at publish".to_owned()),
            );
            return super::WorkspaceSearchApply::Released;
        }
        let mut prepared = match search_engine.acquire_for_owner(stop) {
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
                            OverlayWarmupState::from_search_error(&error),
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
            Err(crate::tools::search::OwnerLockRefused::Closing) => {
                return super::WorkspaceSearchApply::Stopping;
            }
            Err(error) => {
                return super::WorkspaceSearchApply::OperationError(format!(
                    "engine lock error: {error}"
                ));
            }
        };
        let published = loop {
            if !keep_going() {
                if stop.is_stopped() {
                    return super::WorkspaceSearchApply::Stopping;
                }
                return super::WorkspaceSearchApply::Released;
            }
            #[cfg(test)]
            #[allow(deprecated, reason = "test fault injection retains Rust 1.91 compatibility")]
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
                stop,
                lease,
                |engine, checkpoint| {
                    engine.apply_staged_workspace_overlay_publication(&mut prepared, checkpoint)
                },
            ) {
                super::WorkspaceSearchApply::Applied(published) => break Ok(published),
                // Told to leave: not a refusal, so no pause and no retry. Nothing of this pass
                // is lost that a later one cannot redo.
                super::WorkspaceSearchApply::Stopping => {
                    return super::WorkspaceSearchApply::Stopping
                }
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
                    OverlayWarmupState::from_search_error(&error),
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
    /// freshly published graph is the "caught up" state a re-render must read. `mark_bound`
    /// (captured when this build STARTED) bounds the marks it may clear: only drifts this
    /// build already reflects, never one stamped after it began, so a mark is never cleared
    /// against a graph that predates its `.xml` change. Opens the just-published graph
    /// database for the render; when the graph is unavailable nothing is cleared and the
    /// marks persist for the next publish. Never touches the resident mutex.
    /// Returns whether the render actually ran to completion. The caller turns that into its
    /// own obligation: a requested topology refresh is re-raised for the next publish, and the
    /// marks offered stay placed for the next consume. Reporting a skip as done would discharge
    /// an obligation nothing has met, so every path answers for what was DONE — never for what
    /// was asked.
    // The test-side spelling of the call the hook makes, argument for argument; naming a
    // struct for it would only rename the hook's own inputs.
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    fn refresh_search_contexts_after_graph(
        engine: &SharedSearchEngine,
        stop: &super::OwnerStop,
        workspace_root: &Path,
        semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
        index_progress: &Arc<IndexProgress>,
        embed_flight: &Arc<EmbedFlight>,
        lease: &crate::workspace_lease::WorkspaceLease,
        signal: crate::graph::GraphPublishSignal,
    ) -> bool {
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(workspace_root);
        let Ok(store) =
            crate::graph::GraphStore::serving_file_for_test(&cache.graph_db_path(), None)
        else {
            return false;
        };
        Self::refresh_search_contexts_after_graph_with_store(
            engine,
            stop,
            &store,
            semantic_runtime,
            index_progress,
            embed_flight,
            lease,
            signal,
            super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
            None,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::type_complexity,
        reason = "the host passes one retry budget to the existing worker contract; a wrapper would only rename these inputs"
    )]
    fn refresh_search_contexts_after_graph_with_store(
        engine: &SharedSearchEngine,
        stop: &super::OwnerStop,
        store: &crate::graph::GraphStore,
        semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
        index_progress: &Arc<IndexProgress>,
        embed_flight: &Arc<EmbedFlight>,
        lease: &crate::workspace_lease::WorkspaceLease,
        signal: crate::graph::GraphPublishSignal,
        publish_retry_budget: std::time::Duration,
        embedding_prefixes: Option<&super::types::EmbeddingPrefixes>,
    ) -> bool {
        let crate::graph::GraphPublishSignal {
            drift_pending,
            mark_bound,
            topology_changed,
            topology,
            revision,
            fingerprint,
            workspace_roots,
            ..
        } = signal;
        // Fast-path skip (an optimization, not correctness): a follow-up reload is already
        // catching up, so let ITS publish re-render against the fresher graph. Correctness
        // does not depend on this — the `mark_bound` bound below already prevents
        // clearing a mark against a graph that predates its drift. Nothing was rendered, so
        // the caller keeps whatever obligation it was discharging.
        if drift_pending {
            tracing::debug!(
                "graph drift still pending; deferring search context refresh to the next publish"
            );
            return false;
        }
        // The generation served now is not necessarily the build that fired this hook: a
        // newer publication may have been installed meanwhile. Contexts rendered from another
        // topology would be persisted as this build's answers, so treat the mismatch like an
        // unavailable graph — the marks stay dirty and a later publish re-renders them.
        if let Err(error) = Self::read_published_generation(store, revision, fingerprint) {
            tracing::warn!(
                published_revision = revision,
                published_topology = topology,
                "published graph generation is not readable ({error}); skipping context refresh"
            );
            return false;
        }
        let refreshed = match engine.acquire_for_owner(stop) {
            Ok(guard) => match guard.as_ref() {
                Some(engine) => {
                    // A stale cached graph is published without its root table while the
                    // catch-up runs. Its keys are portable and its topology was checked against
                    // the live project, so the engine's roots resolve them; with no roots at all
                    // the graph's source text is unreadable and the marks stay owed.
                    let Some(roots) = workspace_roots.as_ref().or(engine.workspace_roots()) else {
                        tracing::debug!(
                            "no workspace roots to read graph sources; deferring search context refresh"
                        );
                        return false;
                    };
                    let provider = crate::graph_query::GraphDbContextProvider::new(
                        store.clone(),
                        revision,
                        Some(roots),
                        None,
                    );
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
                        mark_bound,
                        topology_changed,
                        &mut apply,
                    )
                }
                None => Err(bsl_search::SearchError::Index(
                    "workspace search engine is not published".to_owned(),
                )),
            },
            // Either way the hook reports the obligation unhandled and it stays owed; only
            // the wording differs, and calling a stop a poisoning would send a reader after
            // a failure that never happened.
            Err(crate::tools::search::OwnerLockRefused::Closing) => {
                Err(bsl_search::SearchError::Index("the daemon is shutting down".to_owned()))
            }
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
                stop,
                semantic_runtime,
                index_progress,
                embed_flight,
                lease,
                publish_retry_budget,
                embedding_prefixes,
            );
        }
        topology_handled
    }

    /// After a context refresh NULLed live embeddings, re-embed the pending chunks through the
    /// shared embed single-flight — the same pass workspace boot uses, so the two never race an
    /// index swap. When no embedder is configured the kick returns without claiming (lexical
    /// results, already fresh from the refresh, are the whole story).
    // Reuse the existing owner/single-flight controls with the same frozen input profile.
    #[allow(clippy::too_many_arguments)]
    fn kick_context_reembed(
        engine: &SharedSearchEngine,
        stop: &super::OwnerStop,
        semantic_runtime: &Arc<Mutex<SemanticRuntimeStatus>>,
        index_progress: &Arc<IndexProgress>,
        embed_flight: &Arc<EmbedFlight>,
        lease: &crate::workspace_lease::WorkspaceLease,
        publish_retry_budget: std::time::Duration,
        embedding_prefixes: Option<&super::types::EmbeddingPrefixes>,
    ) {
        // A no-embedder engine has nothing to re-embed; resolve the DB path only if semantic
        // is live so we never claim the flight for a pass that would do nothing.
        let db_path = engine.acquire_for_owner(stop).ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|engine| engine.has_semantic().then(|| engine.db_path().to_path_buf()))
        });
        let Some(db_path) = db_path else { return };
        let config = match Self::embedding_config_with_prefixes(embedding_prefixes) {
            Ok(Some(config)) => config,
            Ok(None) => return,
            Err(error) => {
                Self::set_semantic_runtime_status(
                    semantic_runtime,
                    SemanticRuntimeStatus::from_search_error(&error),
                );
                return;
            }
        };

        Self::spawn_embed_pass(
            Arc::clone(engine),
            stop.clone(),
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
        stop: super::OwnerStop,
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
        // Orchestration itself writes no vectors; child embedding_pass records own
        // committed counts, so this envelope never duplicates their totals.
        let mut record =
            LifecycleRecord::new(&db_path, "embedding_orchestration", LifecycleReason::Embedding);
        let _ = lease.owns_caches();
        if lease.is_superseded() || lease.is_released() {
            record.outcome = LifecycleOutcome::Skipped;
            record.emit(false);
            tracing::debug!(
                "another daemon generation owns this workspace's derived caches; \
                 skipping the embedding pass"
            );
            return;
        }
        if !embed_flight.claim() {
            record.outcome = LifecycleOutcome::Skipped;
            record.emit(false);
            // A pass is already running; it will loop again and absorb these NULL chunks.
            return;
        }

        let progress_pass = index_progress.begin_pass();
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
            // The daemon's stop belongs in the same predicate as the lease: after it, an
            // owner takes no new resources, and a batch is a network call plus a fenced
            // write. What has already been paid for finishes; nothing new begins.
            let stop = stop.clone();
            move || !stop.is_stopped() && !lease.is_superseded() && !lease.is_released()
        };
        // Counted before the thread starts, and dropped on every way out: a pass about to run
        // is an owner a shutdown must still see leave. Without it `owners.live()` could read
        // zero while this pass was mid-batch, and "every owner has gone" would be a count of
        // the owners that had bothered to register.
        record.emit(false);
        let worker_record = record.clone();
        let lifecycle_context = LifecycleContext::current();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let live = stop.enter();
        let spawned =
            std::thread::Builder::new().name("bsl-search-embed".to_owned()).spawn(move || {
                let _live = live;
                let _dispatch = tracing::dispatcher::set_default(&dispatch);
                let run = || {
                // Restore the flight claim on any abnormal exit; a clean release calls
                // `disarm()` first so this never stomps a later owner that already re-claimed.
                let mut claim_guard = EmbedClaimGuard::new(Arc::clone(&flight));
                let mut progress_pass = progress_pass;
                // Restore the runtime status on any abnormal exit so it never sticks `Indexing`.
                let mut status_guard = EmbedStatusGuard::new(Arc::clone(&runtime), worker_record);
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
                            status_guard.finish(LifecycleOutcome::Failed);
                        return;
                    }
                    flight.begin_pass();
                    #[cfg(test)]
                    if FORCE_EMBED_PASS_PANIC.load(Ordering::SeqCst) {
                        panic!("forced embedding pass panic");
                    }
                    let publication_epoch = engine.acquire_for_owner(&stop).ok().and_then(|guard| guard.as_ref().map(|engine| engine.semantic_index_qualification().epoch));
                    let mut retry_refusal = false;
                    match SearchEngine::embed_pending_chunks_fenced_retrying_owned(
                        &db_path,
                        &config,
                        Some(&progress_pass.token()),
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
                                #[allow(deprecated, reason = "test fault injection retains Rust 1.91 compatibility")]
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
                            // Waiting is not working: the claim stays (nobody else may start a
                            // pass), but the backend is free to go idle while the pause runs.
                            // BOTH signals have to say so — `background_work_active` reads the
                            // flight AND the index progress, and the progress flag is raised
                            // for the whole enclosing pass, so lowering only the flight left
                            // the process pinned for a backoff of up to half an hour.
                            flight.pause();
                            let _paused = index_progress.pause_pass();
                            let stopped = stop.sleep(delay);
                            flight.resume();
                            !stopped && !publish_retry.expired(Instant::now())
                        },
                    ) {
                        Ok(bsl_search::FenceOutcome::Applied(index)) => {
                            let mut prepared_index = Some(index);
                            // Retry only the swap: keep the built index and any pending rerun.
                            loop {
                                #[cfg(test)]
                                if let Some(hook) = EMBED_FENCE_HOOK
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .as_mut()
                                {
                                    hook(EmbedFencePoint::Swap);
                                }
                                let swapped = match engine.acquire_for_owner(&stop) {
                                    Ok(mut guard) => match guard.as_mut() {
                                        Some(engine) => worker_lease.publish_short(
                                            &mut prepared_index,
                                            |prepared| {
                                                engine.set_vector_index(
                                                    prepared.take().expect("prepared index exists"),
                                                );
                                                if progress_pass.token().is_running() {
                                                    if let Some(epoch) = publication_epoch { engine.observe_semantic_publication(epoch); }
                                                }
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
                                            status_guard.finish(LifecycleOutcome::Failed);
                                            return;
                                        }
                                    },
                                    // Told to go with the index already built: the pass leaves
                                    // it unpublished, and says so as a SHUTDOWN rather than a
                                    // failed publication. Saying nothing at all is what left
                                    // `Indexing` standing after the daemon had stopped — a
                                    // status that reads "come back in a moment" when nothing
                                    // is coming.
                                    Err(crate::tools::search::OwnerLockRefused::Closing) => {
                                        Self::set_semantic_runtime_status(
                                            &runtime,
                                            SemanticRuntimeStatus::Stopped,
                                        );
                                        progress_pass.finish(if stop.is_stopped() { bsl_search::IndexPassState::Cancelled } else if worker_lease.is_superseded() { bsl_search::IndexPassState::Superseded } else { bsl_search::IndexPassState::Cancelled });
                                        status_guard.finish(LifecycleOutcome::Interrupted);
                                        return;
                                    }
                                    Err(e) => {
                                        tracing::warn!("embedding pass: engine lock error: {e}");
                                        Self::set_semantic_runtime_status(
                                            &runtime,
                                            SemanticRuntimeStatus::Failed(format!(
                                                "embedding engine lock error: {e}"
                                            )),
                                        );
                                        status_guard.finish(LifecycleOutcome::Failed);
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
                                        break;
                                    }
                                    crate::workspace_lease::LeaseOperationOutcome::TransientRefusal => {
                                        let delay = super::overlay_retry::retry_delay(publish_retry.streak());
                                        if let RetryDecision::RetryAfter(delay) =
                                            publish_retry.refused(Instant::now(), delay)
                                        {
                                            flight.pause();
                                            let _paused = index_progress.pause_pass();
                                            let stopped = stop.sleep(delay);
                                            flight.resume();
                                            if !stopped && !publish_retry.expired(Instant::now()) {
                                                continue;
                                            }
                                        }
                                        retry_refusal = true;
                                        break;
                                    }
                                    crate::workspace_lease::LeaseOperationOutcome::Superseded
                                    | crate::workspace_lease::LeaseOperationOutcome::Released
                                        if stop.is_stopped() =>
                                    {
                                        // The pass's own `keep_running` reads the stop, so a
                                        // shutdown reaches here as a released fence. Reporting
                                        // it as a superseded ownership names a takeover that
                                        // never happened.
                                        Self::set_semantic_runtime_status(
                                            &runtime,
                                            SemanticRuntimeStatus::Stopped,
                                        );
                                        progress_pass.finish(if stop.is_stopped() { bsl_search::IndexPassState::Cancelled } else if worker_lease.is_superseded() { bsl_search::IndexPassState::Superseded } else { bsl_search::IndexPassState::Cancelled });
                                        status_guard.finish(LifecycleOutcome::Interrupted);
                                        return;
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
                                        progress_pass.finish(if stop.is_stopped() { bsl_search::IndexPassState::Cancelled } else if worker_lease.is_superseded() { bsl_search::IndexPassState::Superseded } else { bsl_search::IndexPassState::Cancelled });
                                        status_guard.finish(LifecycleOutcome::Interrupted);
                                        return;
                                    }
                                    crate::workspace_lease::LeaseOperationOutcome::OperationError(
                                        error,
                                    ) => {
                                        // The wrapper has no Display of its own, and its
                                        // Debug reaches `search status` verbatim — the
                                        // variant name and its braces where a message
                                        // belongs. The error inside is what a reader needs.
                                        let error = match error {
                                            crate::workspace_lease::LeaseOperationError::Lease(
                                                error,
                                            ) => error.to_string(),
                                            crate::workspace_lease::LeaseOperationError::Operation(
                                                error,
                                            ) => error.to_string(),
                                        };
                                        Self::set_semantic_runtime_status(
                                            &runtime,
                                            SemanticRuntimeStatus::Failed(format!(
                                                "embedding publication failed: {error}"
                                            )),
                                        );
                                        status_guard.finish(LifecycleOutcome::Failed);
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(bsl_search::FenceOutcome::TransientRefusal) => {
                            // Counted HERE, where the refusal happened. Without this the only
                            // call on this path was the one that deliberately does not count
                            // (`refused_again`, for a refusal already counted), so the streak
                            // never moved and every pause it asked for was `retry_delay(0)` —
                            // zero. The whole pass, embedder calls included, repeated back to
                            // back until the budget ran out, where every other branch backs
                            // off thirty seconds, then a minute, and so on.
                            let delay = super::overlay_retry::retry_delay(publish_retry.streak());
                            let _ = publish_retry.refused(Instant::now(), delay);
                            retry_refusal = true;
                        }
                        Ok(
                            bsl_search::FenceOutcome::Superseded
                            | bsl_search::FenceOutcome::Released,
                        ) if stop.is_stopped() => {
                            // Same fence, same reason as the publication arm above: the pass's
                            // own `keep_running` reads the stop, so a shutdown arrives here as
                            // a released fence rather than as a takeover.
                            Self::set_semantic_runtime_status(
                                &runtime,
                                SemanticRuntimeStatus::Stopped,
                            );
                            progress_pass.finish(if stop.is_stopped() { bsl_search::IndexPassState::Cancelled } else if worker_lease.is_superseded() { bsl_search::IndexPassState::Superseded } else { bsl_search::IndexPassState::Cancelled });
                                        status_guard.finish(LifecycleOutcome::Interrupted);
                            return;
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
                                progress_pass.finish(if stop.is_stopped() { bsl_search::IndexPassState::Cancelled } else if worker_lease.is_superseded() { bsl_search::IndexPassState::Superseded } else { bsl_search::IndexPassState::Cancelled });
                                        status_guard.finish(LifecycleOutcome::Interrupted);
                            return;
                        }
                        Err(e) => {
                            tracing::warn!("background embedding pass failed: {e}");
                            Self::set_semantic_runtime_status(
                                &runtime,
                                SemanticRuntimeStatus::from_search_error(&e),
                            );
                                status_guard.finish(LifecycleOutcome::Failed);
                            return;
                        }
                    }
                    let retry_wait = if retry_refusal {
                        // The refusal was counted where it happened. Counting it again here
                        // would double every step of the backoff against the schedule every
                        // other retry owner follows.
                        let delay = super::overlay_retry::retry_delay(publish_retry.streak());
                        match publish_retry.refused_again(Instant::now(), delay) {
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
                                    status_guard.finish(LifecycleOutcome::Failed);
                                return;
                            }
                        }
                    } else {
                        publish_retry.complete();
                        None
                    };
                    if !flight.finish_pass_with(|| {
                        progress_pass.finish(bsl_search::IndexPassState::Ready);
                        Self::set_semantic_runtime_status(&runtime, SemanticRuntimeStatus::Ready);
                    }) {
                        // No rerun requested → the claim was released under the flight lock.
                        claim_guard.disarm();
                            status_guard.finish(LifecycleOutcome::Completed);
                        tracing::info!("background embedding pass complete; semantic index live");
                        return;
                    }
                    progress_pass.finish(bsl_search::IndexPassState::Waiting);
                    progress_pass = index_progress.begin_pass();
                    // A rerun was requested during the pass; loop again for its NULL chunks.
                    if let Some(delay) = retry_wait {
                        flight.pause();
                        let _paused = index_progress.pause_pass();
                        let stopped = stop.sleep(delay);
                        flight.resume();
                        if stopped {
                            claim_guard.disarm();
                            flight.release();
                            Self::set_semantic_runtime_status(
                                &runtime,
                                SemanticRuntimeStatus::Stopped,
                            );
                            progress_pass.finish(if stop.is_stopped() { bsl_search::IndexPassState::Cancelled } else if worker_lease.is_superseded() { bsl_search::IndexPassState::Superseded } else { bsl_search::IndexPassState::Cancelled });
                                        status_guard.finish(LifecycleOutcome::Interrupted);
                            return;
                        }
                    }
                }
                };
                match lifecycle_context {
                    Some(context) => context.in_scope(run),
                    None => run(),
                }
            });
        if let Err(e) = spawned {
            record.outcome = LifecycleOutcome::Failed;
            record.emit(false);
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
mod publication_retry_accounting {
    /// One refusal moves the backoff one step. The publication counts a refusal where it
    /// happens and asks for the pause afterwards, so the same refusal reaches the window
    /// twice — and the second call must be the form that does not count it again. The
    /// schedule itself is proved by `state::retry_window::tests`; what a unit test cannot
    /// see from there is which form this file calls, so it is counted here.
    ///
    /// The needles are assembled at run time: spelled out, they would match this gate's own
    /// source and pass for the wrong reason.
    #[test]
    fn one_refusal_of_a_publication_reaches_the_window_once() {
        let source = include_str!("embed.rs");
        // A CRLF checkout (core.autocrlf on Windows, no .gitattributes pinning LF) gives this
        // file "\r\n" endings, and a needle anchored on "\n" would then match nothing — the
        // gate would fail on the line endings rather than on the code, in the very CI step that
        // runs it by name. Normalised first, so the gate is about the source and not the
        // checkout.
        let source = &source.replace("\r\n", "\n");
        let cut = ["\n#[cfg(test)]\n", "mod tests {"].concat();
        assert_eq!(
            source.matches(&cut).count(),
            1,
            "the production/test cut moved; this gate scans only what it can prove it scanned"
        );
        let production = source.split(&cut).next().unwrap_or(source);
        let counted = ["publish_retry", ".refused("].concat();
        let again = ["publish_retry", ".refused_again("].concat();
        assert_eq!(
            production.matches(&counted).count(),
            3,
            "a refusal is counted where it happens: once per place that observes one — the \
             publication's own refusal, the fence's, and the swap's"
        );
        assert_eq!(
            production.matches(&again).count(),
            1,
            "the pause for a refusal already counted is asked for with `refused_again`; \
             counting it again doubles every step of the backoff"
        );
    }
}

#[cfg(test)]
mod embed_exit_status {
    /// Every way out of the pass writes a terminal runtime status before it silences the
    /// guard.
    ///
    /// `EmbedStatusGuard` exists to make sure `Indexing` never outlives the pass: on any exit
    /// that did not write a status of its own, its drop writes `Failed`. `finish()` silences
    /// that fallback, so a `finish()` with no terminal write ahead of it leaves `Indexing`
    /// standing — a status that says "come back in a moment" after the daemon has stopped, and
    /// nothing is coming. The stop exits were exactly that: deliberately quiet, because a
    /// shutdown is not a failed publication, and quiet is what left the lie behind.
    ///
    /// Counted rather than reviewed, because the next exit added will be added by someone who
    /// has not read this. The needles are assembled at run time: spelled out, they would match
    /// this gate's own source and pass for the wrong reason.
    #[test]
    fn every_exit_of_the_pass_says_how_it_ended() {
        let source = include_str!("embed.rs");
        // A CRLF checkout (core.autocrlf on Windows, no .gitattributes pinning LF) gives this
        // file "\r\n" endings, and a needle anchored on "\n" would then match nothing — the
        // gate would fail on the line endings rather than on the code, in the very CI step that
        // runs it by name. Normalised first, so the gate is about the source and not the
        // checkout.
        let source = &source.replace("\r\n", "\n");
        let cut = ["\n#[cfg(test)]\n", "mod tests {"].concat();
        assert_eq!(
            source.matches(&cut).count(),
            1,
            "the production/test cut moved; this gate scans only what it can prove it scanned",
        );
        let production = source.split(&cut).next().unwrap_or(source);
        let finish = ["status_guard", ".finish("].concat();
        let writes = ["set_semantic_runtime_status", "("].concat();
        let terminal = ["SemanticRuntimeStatus::", "Ready"].concat();
        let failed = ["SemanticRuntimeStatus::", "Failed"].concat();
        let stopped = ["SemanticRuntimeStatus::", "Stopped"].concat();
        let typed_failure = ["SemanticRuntimeStatus::", "from_search_error"].concat();

        let exits = production.match_indices(&finish).count();
        assert!(exits > 0, "the pass has no exits at all; the needle must have moved");
        for (at, _) in production.match_indices(&finish) {
            // The window between this exit and the previous one: the status it wrote, if any.
            let from = production[..at].rfind(&finish).map_or(0, |prev| prev + finish.len());
            let window = &production[from..at];
            assert!(
                window.contains(&writes)
                    && (window.contains(&terminal)
                        || window.contains(&failed)
                        || window.contains(&typed_failure)
                        || window.contains(&stopped)),
                "an exit of the embedding pass silences the guard without saying how it ended, \
                 which leaves the runtime reading `Indexing` for a pass that is over:\n{window}",
            );
        }
        assert!(
            production.contains(&stopped),
            "a stop is a terminal outcome of its own and must be said as one",
        );
    }
}

#[cfg(test)]
mod flight_mirror_ownership {
    use super::EmbedFlight;
    use bsl_search::IndexProgress;
    #[test]
    fn indexing_pass_publication_precedes_claim_release() {
        let flight = EmbedFlight::new();
        let progress = IndexProgress::new();
        assert!(flight.claim());
        flight.begin_pass();
        let mut pass = progress.begin_pass();
        pass.token().phase(bsl_search::IndexPhase::Persisting);
        assert!(!flight.finish_pass_with(|| {
            assert!(flight.is_in_flight());
            assert!(progress.is_active());
            pass.finish(bsl_search::IndexPassState::Ready);
        }));
        assert!(!flight.is_in_flight());
        assert_eq!(progress.snapshot().unwrap().state, bsl_search::IndexPassState::Ready);
        assert!(flight.claim());
        flight.begin_pass();
        assert!(!flight.claim());
        assert!(flight.finish_pass_with(|| panic!("rerun cannot publish ready")));
        flight.release();
    }

    /// The mirror exists so the broker can read the claim without waiting, and it is only
    /// trustworthy while it is written under the same lock as the field. `publish_working` is
    /// the one place that can do that safely, so a second hand-written store is a defect by
    /// construction — this counts them rather than trusting review to notice the next one.
    ///
    /// The needle is assembled at run time: spelled out, it would match this gate's own
    /// source and pass for the wrong reason.
    #[test]
    fn only_one_writer_publishes_the_claim_mirror() {
        let source = include_str!("embed.rs");
        // A CRLF checkout (core.autocrlf on Windows, no .gitattributes pinning LF) gives this
        // file "\r\n" endings, and a needle anchored on "\n" would then match nothing — the
        // gate would fail on the line endings rather than on the code, in the very CI step that
        // runs it by name. Normalised first, so the gate is about the source and not the
        // checkout.
        let source = &source.replace("\r\n", "\n");
        let cut = ["\n#[cfg(test)]\n", "mod tests {"].concat();
        assert_eq!(
            source.matches(&cut).count(),
            1,
            "the production/test cut moved; this gate scans only what it can prove it scanned"
        );
        let production = source.split(&cut).next().unwrap_or(source);
        let stores = ["in_flight_now", ".store("].concat();
        assert_eq!(
            production.matches(&stores).count(),
            1,
            "publish the claim mirror through publish_working, which holds the lock while it does"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET;
    use super::super::test_support::{
        env_lock, mock_embedding_env, mock_semantic_config, spawn_mock_embedding_server,
        write_common_module,
    };
    use super::SharedState;
    use bsl_search::SearchEngine;
    use std::fs;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn vector_lifecycle_embedding_orchestration_preserves_context_and_terminal_states() {
        use bsl_search::lifecycle::{Batch, Outcome, Reason};
        use tracing_subscriber::prelude::*;
        struct Capture(Arc<Mutex<Vec<serde_json::Value>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct Visitor<'a>(&'a mut Vec<serde_json::Value>);
                impl tracing::field::Visit for Visitor<'_> {
                    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {
                    }
                    fn record_str(&mut self, field: &tracing::field::Field, text: &str) {
                        if field.name() == "record" {
                            self.0.push(serde_json::from_str(text).unwrap());
                        }
                    }
                }
                if event.metadata().target() == bsl_search::lifecycle::TARGET {
                    event.record(&mut Visitor(&mut self.0.lock().unwrap()));
                }
            }
        }
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let records = Arc::new(Mutex::new(Vec::new()));
        test_utils::with_subscriber(
            tracing_subscriber::registry().with(Capture(records.clone())),
            || {
                let directory = tempdir().unwrap();
                let path = directory.path().join("unused.db");
                let flight = super::EmbedFlight::in_flight_for_test();
                SharedState::spawn_embed_pass(
                    crate::state::shared_engine(None),
                    crate::state::OwnerStop::default(),
                    Arc::new(Mutex::new(super::SemanticRuntimeStatus::Ready)),
                    bsl_search::IndexProgress::new(),
                    flight,
                    crate::workspace_lease::WorkspaceLease::unmanaged(),
                    path.clone(),
                    mock_semantic_config(&mock),
                    Duration::ZERO,
                );
                let released = crate::workspace_lease::WorkspaceLease::unmanaged();
                released.release();
                SharedState::spawn_embed_pass(
                    crate::state::shared_engine(None),
                    crate::state::OwnerStop::default(),
                    Arc::new(Mutex::new(super::SemanticRuntimeStatus::Ready)),
                    bsl_search::IndexProgress::new(),
                    super::EmbedFlight::new(),
                    released,
                    path.clone(),
                    mock_semantic_config(&mock),
                    Duration::ZERO,
                );
                for fail in [false, true] {
                    let parent = Batch::new(&path, Reason::Embedding);
                    let cache = crate::cache::WorkspaceCacheLayout::for_workspace(
                        &directory.path().join(if fail { "fail" } else { "ok" }),
                    );
                    let _reset = ResetEmbeddingRefusals;
                    if fail {
                        super::FORCE_EMBED_PREFLIGHT_REFUSALS.store(1, Ordering::SeqCst);
                    }
                    parent.context().in_scope(|| {
                        let (_, _, flight) = start_test_embed(
                            &cache,
                            &mock,
                            if fail { Duration::ZERO } else { Duration::from_secs(2) },
                        );
                        wait_for_embed_flight(&flight);
                    });
                    parent.finish(Outcome::Completed);
                }
                // The existing status guard also emits an honest interruption on unwind.
                let record = super::LifecycleRecord::new(
                    &path,
                    "embedding_orchestration",
                    Reason::Embedding,
                );
                drop(super::EmbedStatusGuard::new(
                    Arc::new(Mutex::new(super::SemanticRuntimeStatus::Indexing)),
                    record,
                ));
            },
        );
        let values = records.lock().unwrap();
        let orchestration: Vec<_> =
            values.iter().filter(|v| v["kind"] == "embedding_orchestration").collect();
        for outcome in ["skipped", "completed", "failed", "interrupted"] {
            assert!(
                orchestration.iter().any(|v| v["outcome"] == outcome),
                "missing {outcome}: {orchestration:?}"
            );
        }
        assert_eq!(orchestration.iter().filter(|v| v["outcome"] == "skipped").count(), 2);
        assert!(orchestration
            .iter()
            .filter(|v| v["outcome"] == "started")
            .all(|v| !v["parent_operation_id"].is_null()));
        assert!(
            orchestration
                .iter()
                .all(|v| v["counts"]["sqlite_vectors_removed"] == 0
                    && v["committed_totals"].is_null())
        );
        assert!(values
            .iter()
            .any(|v| v["kind"] == "embedding_pass" && !v["parent_operation_id"].is_null()));
    }

    struct ResetEmbeddingRefusals;

    impl Drop for ResetEmbeddingRefusals {
        fn drop(&mut self) {
            super::FORCE_EMBED_PREFLIGHT_REFUSALS.store(0, Ordering::SeqCst);
            super::FORCE_EMBED_PUBLICATION_REFUSALS.store(0, Ordering::SeqCst);
            *super::EMBED_FENCE_HOOK.lock().unwrap_or_else(|p| p.into_inner()) = None;
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
        let engine = crate::state::shared_engine(Some(
            SearchEngine::new(&db_path, mock_semantic_config(server)).unwrap(),
        ));
        let runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let flight = super::EmbedFlight::new();
        SharedState::spawn_embed_pass(
            Arc::clone(&engine),
            crate::state::OwnerStop::default(),
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

    /// The graph freshness token recorded in the database a test just built — what a real
    /// publish would put in its signal, and what the refresh checks the file against.
    fn built_graph_token(workspace: &std::path::Path) -> (u64, crate::graph_db::GraphFp) {
        let (revision, fingerprint, _) =
            crate::graph_query::GraphDb::open(&crate::cache::graph_db_path(workspace))
                .expect("graph database built by the test")
                .freshness_token()
                .expect("graph database carries its freshness token");
        (revision, fingerprint)
    }

    fn built_graph_topology(workspace: &std::path::Path) -> u64 {
        built_graph_token(workspace).1.topology
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
            let bound = signal.mark_bound;
            let handled = SharedState::refresh_search_contexts_after_graph(
                &engine_arc,
                &crate::state::OwnerStop::default(),
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
        let engine: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Disabled));
        let progress = bsl_search::IndexProgress::new();
        let flight = super::EmbedFlight::new();
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        let hook = SharedState::build_publish_hook(
            Arc::clone(&engine),
            crate::state::OwnerStop::default(),
            graph.store().clone(),
            graph.owed_context_marks(),
            Arc::clone(&semantic_runtime),
            Arc::clone(&progress),
            Arc::clone(&flight),
            None,
            Arc::new(AtomicU64::new(0)),
            super::super::types::EmbeddingPrefixes::default(),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        let graph = graph.with_publish_hook(hook);
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
        let engine: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let graph = crate::graph::GraphState::for_workspace_with_cache(workspace.clone(), cache);
        let hook = SharedState::build_publish_hook(
            Arc::clone(&engine),
            crate::state::OwnerStop::default(),
            graph.store().clone(),
            graph.owed_context_marks(),
            Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Disabled)),
            bsl_search::IndexProgress::new(),
            super::EmbedFlight::new(),
            None,
            Arc::new(AtomicU64::new(0)),
            super::super::types::EmbeddingPrefixes::default(),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        let graph = graph.with_publish_hook(hook);
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
        let engine: super::SharedSearchEngine = crate::state::shared_engine(Some(search));
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
            mark_bound: 0,
            topology_changed: false,
            topology: built_graph_topology(&workspace),
            revision: built_graph_token(&workspace).0,
            fingerprint: built_graph_token(&workspace).1,
            roots_refresh_requested: true,
            workspace_roots: crate::project::at(&workspace)
                .ok()
                .map(|project| crate::project::workspace_roots(&project, &[]).0),
        };
        let outcome = SharedState::refresh_search_roots_after_graph(
            &engine,
            &crate::state::OwnerStop::default(),
            &crate::graph::GraphStore::serving_file_for_test(
                &crate::cache::graph_db_path(&workspace),
                None,
            )
            .unwrap(),
            None,
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
        let engine: super::SharedSearchEngine = crate::state::shared_engine(Some(search));

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
            mark_bound: 0,
            topology_changed: false,
            topology: built_graph_topology(&workspace),
            revision: built_graph_token(&workspace).0,
            fingerprint: built_graph_token(&workspace).1,
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
            &crate::state::OwnerStop::default(),
            &crate::graph::GraphStore::serving_file_for_test(
                &crate::cache::graph_db_path(&workspace),
                None,
            )
            .unwrap(),
            None,
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
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let overlay_warmup = Arc::new(Mutex::new(crate::state::OverlayWarmupState::Pending));

        SharedState::run_overlay_warmup(
            &engine_arc,
            &crate::state::OwnerStop::default(),
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
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let overlay_warmup = Arc::new(Mutex::new(crate::state::OverlayWarmupState::Pending));

        // The first check (pre-publish) passes; the stop lands before the post-lock one.
        let calls = AtomicUsize::new(0);
        SharedState::run_overlay_warmup(
            &engine_arc,
            &crate::state::OwnerStop::default(),
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
        let _lock = env_lock();
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
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
            &crate::state::OwnerStop::default(),
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
        let _lock = env_lock();
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let overlay_warmup = Arc::new(Mutex::new(crate::state::OverlayWarmupState::Pending));

        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&broken).is_ok() {
            std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        SharedState::run_overlay_warmup(
            &engine_arc,
            &crate::state::OwnerStop::default(),
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

        let _lock = env_lock();
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();

        SharedState::kick_context_reembed(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
            None,
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

        let _lock = env_lock();
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        // A pass is already in flight: the kick must be absorbed, spawning nothing.
        let embed_flight = super::EmbedFlight::in_flight_for_test();

        SharedState::kick_context_reembed(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
            None,
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

        let _lock = env_lock();
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        // Wire the SAME publish hook the daemon builds, so the graph's real publish — not a
        // hand-call — drives the context refresh and re-embed.
        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        let hook = SharedState::build_publish_hook(
            Arc::clone(&engine_arc),
            crate::state::OwnerStop::default(),
            graph.store().clone(),
            graph.owed_context_marks(),
            Arc::clone(&semantic_runtime),
            Arc::clone(&index_progress),
            Arc::clone(&embed_flight),
            None,
            Arc::new(AtomicU64::new(0)),
            super::super::types::EmbeddingPrefixes::default(),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        let graph = graph.with_publish_hook(hook);

        // The xml drift marks the owned module context-dirty and nudges the graph; the nudged
        // build publishes and fires the hook automatically.
        let xml = workspace.join("CommonModules/Сервер.xml");
        let entry = ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[entry],
            false,
            &graph,
        );
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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
            &crate::state::OwnerStop::default(),
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
            &crate::state::OwnerStop::default(),
            &workspace,
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            crate::graph::GraphPublishSignal {
                drift_pending: true,
                mark_bound: i64::MAX,
                topology_changed: false,
                topology: built_graph_topology(&workspace),
                revision: built_graph_token(&workspace).0,
                fingerprint: built_graph_token(&workspace).1,
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
            &crate::state::OwnerStop::default(),
            &workspace,
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            crate::graph::GraphPublishSignal {
                drift_pending: false,
                mark_bound: i64::MAX,
                topology_changed: false,
                topology: built_graph_topology(&workspace),
                revision: built_graph_token(&workspace).0,
                fingerprint: built_graph_token(&workspace).1,
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
            // A publication that carries no root table (a stale cached graph adopted while its
            // catch-up runs) still renders from source text: the engine's own roots resolve the
            // graph's portable keys.
            let contexts: Vec<_> = g
                .as_ref()
                .unwrap()
                .load_indexed_documents(Some("code"))
                .unwrap()
                .into_iter()
                .filter_map(|document| document.graph_context)
                .collect();
            assert!(
                contexts.iter().any(|context| context.contains("Signature: Функция Считать")),
                "the re-rendered context quotes the method signature: {contexts:?}",
            );
        }
    }

    /// A publication consumes only the marks it was handed, with the fact that caused them. A
    /// mark nobody handed to the graph — stamped straight into the store — gets bound `0` and
    /// survives the publish; an unbounded consume would clear it.
    #[test]
    fn a_publication_clears_no_mark_nobody_handed_it() {
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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
                    &crate::state::OwnerStop::default(),
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

        // The mark was never handed to the graph, so no publication has a bound that covers it.
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
            "a publication with nothing handed to it (bound 0) clears no marks; the mark survives",
        );
    }

    /// Marks a PRIOR daemon run left in `context_dirty` survive the boot build's publish — no
    /// consumer had handed them to the graph yet — and are consumed by the explicit leftover
    /// pickup against the already-fresh boot graph. Removing the `consume_leftover_marks` call
    /// leaves the mark stranded and the final assertion fails.
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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
                    &crate::state::OwnerStop::default(),
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

        // Boot: the graph builds and publishes before anyone handed it the leftover mark.
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
            "the leftover mark survives the boot publish nobody handed it to",
        );

        // The explicit pickup: a consume bounded by the seq captured at observation time
        // clears the leftover mark synchronously (the graph is already `Ready`).
        let leftover_bound = mark_seq.load(Ordering::SeqCst);
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
    /// A consume bounded by a LIVE counter read instead of the captured bound would clear the
    /// newer mark too, and the survival assertion fails.
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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
                    &crate::state::OwnerStop::default(),
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

        // Boot: build+publish before the leftover mark is handed over, so it survives.
        let graph =
            crate::graph::GraphState::for_workspace(workspace.clone()).with_publish_hook(hook);
        graph.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while fired.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(fired.load(Ordering::SeqCst) >= 1, "the boot build published and fired the hook");

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

    /// The deferred (`Loading`) pickup path: leftovers handed over while the graph is not yet
    /// `Ready` wait in the ledger, and the build's own publish consumes them with the captured
    /// bound — the one fire it makes. A newer mark stamped after the capture must still
    /// survive: a consume bounded by a live counter read would clear it too.
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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
                let bound = signal.mark_bound;
                let handled = SharedState::refresh_search_contexts_after_graph(
                    &engine_arc,
                    &crate::state::OwnerStop::default(),
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

        // The graph is `Idle`: the leftovers cannot be consumed yet, so they wait for the
        // build's own publication, which observes every fact there is.
        let graph =
            crate::graph::GraphState::for_workspace(workspace.clone()).with_publish_hook(hook);
        graph.consume_leftover_marks(leftover_bound);
        graph.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while fire_bounds.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            *fire_bounds.lock().unwrap(),
            vec![leftover_bound],
            "the publish consumed the leftovers with the captured bound, once",
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        let refresh = |topology: u64| {
            let (revision, fingerprint) = if crate::cache::graph_db_path(&workspace).exists() {
                built_graph_token(&workspace)
            } else {
                (0, crate::graph_db::GraphFp { files: 0, topology })
            };
            SharedState::refresh_search_contexts_after_graph(
                &engine_arc,
                &crate::state::OwnerStop::default(),
                &workspace,
                &semantic_runtime,
                &index_progress,
                &embed_flight,
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                crate::graph::GraphPublishSignal {
                    drift_pending: false,
                    mark_bound: i64::MAX,
                    topology_changed: false,
                    topology,
                    revision,
                    fingerprint,
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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
            graph.marks_pending(),
            "a pickup that could not run leaves the obligation armed for the next publish",
        );
    }

    /// The marks a skipped pickup kept are what the next publication consumes, with the bound
    /// they were handed over with; nothing else could clear the leftover mark, so the assertion
    /// cannot pass through another path by accident.
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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
        assert!(graph.marks_pending(), "the skipped pickup kept an obligation to discharge");
        // The skipped pickup fired the hook too, with this very bound. Only fires AFTER this
        // point can be the publish's, so the wait below must not count what already happened.
        let fires_before_publish = fire_bounds.lock().unwrap().len();

        // A `Ready` graph claims a reload only for a drift it can see on disk. Without one the
        // nudge is a no-op and this test would wait on a publish that never comes — passing or
        // failing on how the machine was loaded rather than on the obligation.
        write_common_module(&workspace, "Клиент", "Функция Прочесть() Экспорт КонецФункции");
        graph.nudge_rebuild();
        assert!(
            graph.drift_pending(),
            "the drift claims the reload whose publish discharges the obligation",
        );

        // Wait for the fire that carries the leftover bound: the handed-over marks tell it
        // apart from a fire with nothing to consume.
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

        let _lock = env_lock();
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

        let _lock = env_lock();
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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
            crate::state::OwnerStop::default(),
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
    fn payload_lifecycle_workspace_failure_is_retained_until_an_admitted_retry() {
        let _lock = env_lock();
        let (server, calls) = spawn_counting_embedding_server();
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        seed_pending_embedding(&db_path);
        let engine = crate::state::shared_engine(Some(
            SearchEngine::new(&db_path, mock_semantic_config(&server)).unwrap(),
        ));
        let runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Ready));
        let flight = super::EmbedFlight::new();
        let lease = crate::workspace_lease::WorkspaceLease::unmanaged();
        let start = |config| {
            SharedState::spawn_embed_pass(
                Arc::clone(&engine),
                crate::state::OwnerStop::default(),
                Arc::clone(&runtime),
                bsl_search::IndexProgress::new(),
                Arc::clone(&flight),
                lease.clone(),
                db_path.clone(),
                config,
                DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
            )
        };

        let mut too_small = mock_semantic_config(&server);
        too_small.embedder.max_request_bytes = 1;
        start(too_small);
        wait_for_embed_flight(&flight);
        let failure = runtime.lock().unwrap().embedding_failure().expect("typed pass failure");
        assert_eq!(failure.code, bsl_search::EmbeddingFailureCode::EmbeddingInputTooLarge);
        assert_eq!(failure.max_request_bytes, Some(1));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(engine.lock().unwrap().as_ref().unwrap().vector_count(), 0);

        assert!(flight.claim());
        start(mock_semantic_config(&server));
        assert_eq!(runtime.lock().unwrap().embedding_failure(), Some(failure));
        flight.release();

        // Block only the final installation, so the admitted retry's reset can be observed
        // independently of how quickly the loopback request finishes.
        let guard = engine.lock().unwrap();
        start(mock_semantic_config(&server));
        assert_eq!(*runtime.lock().unwrap(), crate::state::SemanticRuntimeStatus::Indexing);
        drop(guard);
        wait_for_embed_flight(&flight);
        assert_eq!(*runtime.lock().unwrap(), crate::state::SemanticRuntimeStatus::Ready);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(engine.lock().unwrap().as_ref().unwrap().vector_count(), 1);
    }

    #[test]
    fn failed_typed_preflight_makes_zero_network_calls() {
        let _lock = env_lock();
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
        let _lock = env_lock();
        let _reset = ResetEmbeddingRefusals;
        let (server, calls) = spawn_counting_embedding_server();
        let dir = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        super::FORCE_EMBED_PUBLICATION_REFUSALS.store(1, Ordering::SeqCst);

        let (engine, runtime, flight) = start_test_embed(&cache, &server, Duration::from_secs(1));
        wait_for_embed_flight(&flight);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let status = runtime.lock().unwrap().clone();
        assert!(
            matches!(status, crate::state::SemanticRuntimeStatus::Ready),
            "the pass must end ready after the one transient refusal, got {status:?}",
        );
        assert_eq!(engine.lock().unwrap().as_ref().unwrap().vector_count(), 1);
        assert!(bsl_search::Store::open_existing(&cache.search_db_path())
            .unwrap()
            .load_pending_embedding_documents("code")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn prepared_index_survives_transient_swap_without_rebuild() {
        use super::EmbedFencePoint;

        let _lock = env_lock();
        for request_rerun in [false, true] {
            let _reset = ResetEmbeddingRefusals;
            let (server, calls) = spawn_counting_embedding_server();
            let dir = tempdir().unwrap();
            let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
            cache.ensure().unwrap();
            let db_path = cache.search_db_path();
            seed_pending_embedding(&db_path);
            let engine = crate::state::shared_engine(Some(
                SearchEngine::new(&db_path, mock_semantic_config(&server)).unwrap(),
            ));
            let runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
            let flight = super::EmbedFlight::new();
            let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
            let events = Arc::new(Mutex::new(Vec::new()));
            let observed = Arc::clone(&events);
            let hook_lease = lease.clone();
            let hook_flight = Arc::clone(&flight);
            let mut held = None;
            let mut swaps = 0;
            *super::EMBED_FENCE_HOOK.lock().unwrap() = Some(Box::new(move |point| {
                observed.lock().unwrap().push(point);
                // Let a regressed rebuild finish so the event assertion diagnoses it directly.
                if matches!(point, EmbedFencePoint::Apply(_)) {
                    drop(held.take());
                }
                if point == EmbedFencePoint::Swap {
                    swaps += 1;
                    if swaps == 1 {
                        held = Some(hook_lease.hold_file_lock_for_test());
                        if request_rerun {
                            assert!(!hook_flight.claim());
                        }
                    } else {
                        drop(held.take());
                    }
                }
            }));
            SharedState::spawn_embed_pass(
                Arc::clone(&engine),
                crate::state::OwnerStop::default(),
                Arc::clone(&runtime),
                bsl_search::IndexProgress::new(),
                Arc::clone(&flight),
                lease.clone(),
                db_path,
                mock_semantic_config(&server),
                Duration::from_secs(10),
            );
            wait_for_embed_flight(&flight);

            assert!(matches!(*runtime.lock().unwrap(), crate::state::SemanticRuntimeStatus::Ready));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(engine.lock().unwrap().as_ref().unwrap().vector_count(), 1);
            let events = events.lock().unwrap();
            let swaps: Vec<_> = events
                .iter()
                .enumerate()
                .filter_map(|(i, point)| (*point == EmbedFencePoint::Swap).then_some(i))
                .collect();
            assert_eq!(
                swaps.len(),
                2 + usize::from(request_rerun),
                "fresh work must survive retry"
            );
            assert_eq!(
                swaps[1],
                swaps[0] + 1,
                "retry must not re-enter embedding or sidecar publication"
            );
            lease.release();
        }
    }

    #[test]
    fn publication_deadline_moves_runtime_to_failed() {
        let _lock = env_lock();
        let _reset = ResetEmbeddingRefusals;
        let (server, _) = spawn_counting_embedding_server();
        let dir = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        // The PUBLICATION, which is what this test is named for. Forcing the preflight instead
        // exercised the admission check and left the publish path — the one the deadline is
        // supposed to bound — completely uncovered.
        super::FORCE_EMBED_PUBLICATION_REFUSALS.store(u64::MAX, Ordering::SeqCst);

        let (_, runtime, flight) = start_test_embed(&cache, &server, Duration::ZERO);
        wait_for_embed_flight(&flight);

        assert!(matches!(
            &*runtime.lock().unwrap(),
            crate::state::SemanticRuntimeStatus::Failed(message)
                if message.contains("retry budget exhausted")
        ));
    }

    /// A pass sitting out a publication backoff is waiting, not working. The claim stays —
    /// no second pass may start — but the backend is free to go idle meanwhile: this backoff
    /// grows to half an hour, and counted as work it pins the whole process for it.
    #[test]
    fn an_embed_pass_in_its_retry_pause_is_not_live_work() {
        use crate::change_hub::test_support::eventually;

        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        let dir = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        cache.ensure().unwrap();
        let db_path = cache.search_db_path();
        seed_pending_embedding(&db_path);
        let engine = crate::state::shared_engine(Some(
            SearchEngine::new(&db_path, mock_semantic_config(&mock)).unwrap(),
        ));
        let runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let flight = super::EmbedFlight::new();
        let stop = crate::state::OwnerStop::default();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);

        // A peer holds the lock the publication fence needs for longer than two of its waits,
        // so two attempts are refused: the first backs off by nothing, the second by a whole
        // tick — the pause this test is about.
        let holder = crate::workspace_lease::WorkspaceLease::hold_cache_lock_for(
            &cache,
            Duration::from_secs(5),
        );
        SharedState::spawn_embed_pass(
            Arc::clone(&engine),
            stop.clone(),
            Arc::clone(&runtime),
            bsl_search::IndexProgress::new(),
            Arc::clone(&flight),
            lease,
            db_path,
            mock_semantic_config(&mock),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );

        assert!(
            eventually(Duration::from_secs(10), || flight.is_in_flight()),
            "no pass ever claimed the flight, so the pause below would prove nothing"
        );
        assert!(
            eventually(Duration::from_secs(10), || !flight.is_in_flight()),
            "the paused pass is still counted as live work"
        );
        assert!(
            !flight.claim_for_test(),
            "the pause gave up the claim; a second pass could start beside the first"
        );
        assert!(
            crate::state::overlay_retry::retry_delay(1) >= Duration::from_secs(5),
            "the pause is short enough that a finished pass would pass for a paused one"
        );

        stop.stop();
        holder.join().unwrap();
    }

    #[test]
    fn embedding_fence_distinguishes_retry_from_supersession() {
        use bsl_search::{Chunk, ChunkKind, Store};
        use std::time::Instant;

        let _lock = env_lock();
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
            let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
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
                crate::state::OwnerStop::default(),
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
        let engine = crate::state::shared_engine(Some(
            SearchEngine::new(&db_path, mock_semantic_config(&mock)).unwrap(),
        ));
        let runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let flight = super::EmbedFlight::new();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let holder = crate::workspace_lease::WorkspaceLease::hold_cache_lock_for(
            &cache,
            Duration::from_secs(3),
        );
        SharedState::spawn_embed_pass(
            Arc::clone(&engine),
            crate::state::OwnerStop::default(),
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

        let _lock = env_lock();
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
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let semantic_runtime = Arc::new(Mutex::new(crate::state::SemanticRuntimeStatus::Indexing));
        let index_progress = bsl_search::IndexProgress::new();
        let embed_flight = super::EmbedFlight::new();
        SharedState::kick_context_reembed(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &semantic_runtime,
            &index_progress,
            &embed_flight,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
            None,
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
