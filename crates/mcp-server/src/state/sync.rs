use super::retry_window::{RetryDecision, RetryOwner, RetryWindow};
use super::{SharedSearchEngine, SharedState};
use crate::change_hub::WorkspaceChangeHub;
use crate::graph::GraphState;
use bsl_search::SearchEngine;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct SearchDriftPlan {
    dirty_paths: Vec<PathBuf>,
    removed_paths: Vec<PathBuf>,
    removed_subtrees: Vec<PathBuf>,
    context_paths: Vec<PathBuf>,
    mark_all_context: bool,
    rewalk_paths: Vec<PathBuf>,
    reconcile_present: Option<std::collections::HashSet<PathBuf>>,
    dirty_keys: Vec<bsl_search::FileKey>,
    removed_keys: Vec<bsl_search::FileKey>,
    context_keys: Vec<bsl_search::FileKey>,
    full_rescan: bool,
    roots_epoch: u64,
    preparation_error: Option<String>,
    /// The preparation was refused because the daemon is leaving. Kept apart from
    /// `preparation_error`: a stop is not a failure of this pass and must not be reported,
    /// slept on, or retried as one.
    preparation_stopping: bool,
    snapshot_outcome: Option<SnapshotPreparationOutcome>,
    snapshot_paths: Vec<PathBuf>,
    nudge_rebuild: bool,
    nudge_project_reload: bool,
    /// The identity of the loss that asked for the reload above, when one did. Shared with
    /// every other consumer of the same batch, so one loss reaching the graph through two
    /// cursors is one event.
    loss_token: Option<u64>,
    dirty_cursor: usize,
    removed_cursor: usize,
    context_cursor: usize,
    /// The highest seq the applied slices stamped context marks with; handed to the graph
    /// with the batch's fact once the plan is done.
    mark_high: Option<i64>,
    /// How far the cursors had moved when the backlog owner was last told about this plan's
    /// marks. Not a flag: a plan can reach the telling more than once, and each time it has
    /// marked more paths than the owner has heard about.
    backlog_told_through: (usize, usize),
}

/// The declared roots of the workspace, and the subtrees a walk of them must skip.
type RootsAndHoles = (Vec<PathBuf>, Vec<PathBuf>, project_model::ExcludedPaths);

enum SnapshotPreparationOutcome {
    OperationError(String),
    TransientRefusal,
    Superseded,
    Released,
}

enum ReferencingFilesOutcome {
    Applied(std::collections::HashSet<PathBuf>),
    OperationError(String),
    TransientRefusal,
    Superseded,
    Released,
}

#[derive(Default)]
struct RescanDebt {
    streak: u32,
    next_allowed: Option<std::time::Instant>,
}

impl RescanDebt {
    fn required(&self) -> bool {
        self.next_allowed.is_some()
    }

    fn record_failure(&mut self, now: std::time::Instant) {
        let delay = super::overlay_retry::retry_delay(self.streak);
        self.streak = self.streak.saturating_add(1);
        self.next_allowed = Some(now + delay);
    }

    fn waiting(&self, now: std::time::Instant) -> bool {
        #[cfg(test)]
        if FORCE_RESCAN_DEBT_DUE.load(Ordering::SeqCst) {
            return false;
        }
        // A debt this consumer really earned, held in its wait for as long as a stand needs it:
        // the branch under test is the wait, and a backoff that ran out in the middle of a
        // stand would hand its batch to the ordinary path instead.
        #[cfg(test)]
        if FORCE_RESCAN_DEBT_WAITING.load(Ordering::SeqCst) && self.required() {
            return true;
        }
        self.next_allowed.is_some_and(|next| now < next)
    }

    fn wait_for(&self, now: std::time::Instant, idle: Duration) -> Duration {
        #[cfg(test)]
        if FORCE_RESCAN_DEBT_DUE.load(Ordering::SeqCst) && self.required() {
            return Duration::ZERO;
        }
        self.next_allowed.map_or(idle, |next| next.saturating_duration_since(now).min(idle))
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

impl SearchDriftPlan {
    fn complete(&self) -> bool {
        self.dirty_cursor == self.dirty_keys.len()
            && self.removed_cursor == self.removed_keys.len()
            && self.context_cursor == self.context_keys.len()
    }
}

/// Test seam: force a reconcile walk (the overflow rescan and the boot store reconcile) to count as
/// errored, so a test can assert the reconcile is skipped (a partial walk must never be treated as
/// authoritative and delete healthy files) — and, at boot, that a Clean init downgrades to a prime.
#[cfg(test)]
pub(super) static FORCE_REWALK_WALK_ERROR: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FORCE_DRIFT_APPLY_ERROR_ENGINE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
static FORCE_RESCAN_DEBT_DUE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FORCE_RESCAN_DEBT_WAITING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Batches the rescan-debt wait has condensed into a demand on the graph. Read by a stand that
/// must prove its reconcile went through THAT branch and not the ordinary apply.
#[cfg(test)]
pub(super) static RESCAN_DEBT_WAIT_BRANCH: AtomicU64 = AtomicU64::new(0);

/// Runs between the in-memory capture of the carriers and the read of the stored ones, with
/// the engine lock already given back.
#[cfg(test)]
type CarrierReadHook = (std::thread::ThreadId, Box<dyn Fn() + Send>);
#[cfg(test)]
static CARRIER_READ_HOOK: std::sync::Mutex<Option<CarrierReadHook>> = std::sync::Mutex::new(None);

/// One slice of waiting for the watch to arm, and the whole budget for it.
///
/// The slice is only how often the wait comes up for air; the budget is what actually
/// bounds it. Ten minutes is far more than the initial walk of a large configuration
/// takes and still finite — a boot that waited forever would never publish an engine at
/// all, and one that gave up after a single slice abandons the workspace whose walk
/// merely outlasted a minute.
const WATCH_READY_SLICE: Duration = Duration::from_secs(60);
const WATCH_READY_BUDGET: Duration = Duration::from_secs(600);

/// How long the boot waits for the watch, in one slice and in total. A parameter
/// rather than the two constants read directly, so a test can drive the slow-start path
/// without standing through a production-sized slice.
#[derive(Debug, Clone, Copy)]
pub(super) struct WatchWaitPolicy {
    slice: Duration,
    budget: Duration,
}

impl WatchWaitPolicy {
    pub(super) const PRODUCTION: Self =
        Self { slice: WATCH_READY_SLICE, budget: WATCH_READY_BUDGET };

    #[cfg(test)]
    pub(super) fn new(slice: Duration, budget: Duration) -> Self {
        Self { slice, budget }
    }
}

/// Park until the hub delivers a batch that arrived AFTER this call, and return once it has.
///
/// The dormant half of the sink's watcher-mode admission: the budget for enabling watcher mode
/// has run out, and only fresh external work opens the next one. "Fresh" is measured from the
/// hub's generation as it stands here, not from whatever generation the sink last happened to
/// observe — a sink that never waited still holds generation 0, and every batch delivered while
/// it was retrying would answer as work that arrived after the budget ended, handing out a
/// second full deadline nobody asked for.
///
/// The batch itself cannot answer instead: the cursor has not moved, so the same unacknowledged
/// entries materialise every round and would read as fresh work for ever.
///
/// Returns `false` when the owner must leave instead of waiting on: the daemon is stopping, or
/// the lease went terminal — a dormant owner that only checked for work would outlive both.
fn wait_for_fresh_batch(
    hub: &WorkspaceChangeHub,
    cursor: crate::change_hub::SinkCursor,
    generation: &mut u64,
    stop: &super::OwnerStop,
    lease: &crate::workspace_lease::WorkspaceLease,
) -> bool {
    *generation = hub.generation();
    loop {
        if owner_must_leave(hub, stop, lease) {
            return false;
        }
        let observed = *generation;
        *generation =
            hub.wait_for_change_or(*generation, Duration::from_secs(30), || stop.is_stopped());
        if *generation == observed {
            continue;
        }
        let batch = hub.materialize(cursor);
        if !batch.entries.is_empty() || batch.rescan_required {
            return true;
        }
    }
}

/// Marks this plan has applied that the backlog owner has not been told about, if any.
///
/// The answer is the cursor pair to record once the telling is made. A plan reaches this
/// question more than once — a refusal retried, then the rest of its slices applied — and each
/// time it has marked more paths than the owner has heard about. A flag latched on the first
/// telling leaves the rest of the marks to wait out the owner's idle tick, with the index
/// serving the old text of every file in them; an owner that already spent its budget does not
/// come back at all without a fresh fact.
fn backlog_owes_a_telling(plan: &SearchDriftPlan) -> Option<(usize, usize)> {
    let marked = (plan.dirty_cursor, plan.removed_cursor);
    let anything_marked = plan.dirty_cursor > 0 || plan.removed_cursor > 0;
    (anything_marked && marked != plan.backlog_told_through).then_some(marked)
}

/// The daemon asked its owners to stop, or this generation no longer owns the workspace.
fn owner_must_leave(
    hub: &WorkspaceChangeHub,
    stop: &super::OwnerStop,
    lease: &crate::workspace_lease::WorkspaceLease,
) -> bool {
    stop.is_stopped() || hub.is_closing() || lease.is_superseded() || lease.is_released()
}

/// Whether a hub can actually feed its consumers: a watch that is up, or its own poll while
/// the watch is refused. The sink asks this before flipping the one-way watcher mode — a hub
/// whose thread never started has neither, and enabling the mode there would leave the overlay
/// following a feed that is not there (github#186, github#187). A hub that arms later flips the
/// mode where the feed proves itself: on the first applied drift batch.
fn hub_is_feeding(hub: &WorkspaceChangeHub) -> bool {
    hub.is_watching() || hub.is_polling()
}

impl SharedState {
    /// Wait for the watch to arm; `false` means it will not, or not within the budget.
    ///
    /// Three readiness answers, two decisions. `Failed` is permanent, so waiting out the
    /// budget over it would only delay a boot that has to happen either way. `NotYet` says
    /// nothing has gone wrong yet — a long initial walk looks exactly like this — so the
    /// wait resumes until the budget is spent.
    pub(super) fn await_watch(
        hub: &WorkspaceChangeHub,
        stop: &super::OwnerStop,
        policy: WatchWaitPolicy,
    ) -> bool {
        let deadline = std::time::Instant::now() + policy.budget;
        loop {
            // The slice never outlives the budget. Asked for a whole slice at the very end
            // of one, the hub answers a slice past the deadline the caller was promised —
            // and with a production slice that overshoot is a minute, long enough to be
            // mistaken for a hub that is still arming.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match hub.watch_readiness_or(policy.slice.min(remaining), || stop.is_stopped()) {
                crate::change_hub::WatchReadiness::Armed => return true,
                crate::change_hub::WatchReadiness::Failed => {
                    // A stop answers the same question — this watch will not arm for this
                    // daemon — but it is not a hub that could not be set up, and saying so
                    // would put a warning in every clean shutdown.
                    if !stop.is_stopped() {
                        tracing::warn!(
                            "workspace change hub could not watch; the search overlay follows the \
                             hub's feed when one is running and stays on full scans otherwise"
                        );
                    }
                    return false;
                }
                crate::change_hub::WatchReadiness::NotYet => {
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            budget_secs = policy.budget.as_secs(),
                            "workspace change hub did not arm within the budget; the search overlay \
                             proceeds on the boot baseline and follows the hub once it arms or \
                             falls back to its own poll"
                        );
                        return false;
                    }
                }
            }
        }
    }

    /// Drive the search overlay from the change hub. Search is one sink among
    /// several: it drains its own cursor and applies the shared drift classification
    /// (stateless policy) — `.bsl` bodies marked dirty, deleted `.bsl` removed from the
    /// store, `.xml` metadata resolved to the affected documents' context. The raw
    /// (non-canonical) path is used so the strip against the configured source root
    /// still matches when that root has symlinks.
    ///
    /// Started by the boot after the engine is published, in every watch mode: armed,
    /// polling or still arming. Existing any earlier would mean draining events into an
    /// engine that is not there: every apply below no-ops on `None` and the batch is gone
    /// for good. The cursor is
    /// older than the boot's own read of disk, so the stream begins strictly before the
    /// baseline it corrects and nothing falls between the two.
    ///
    /// Returns whether the thread started. Until it does, the cursor belongs to the caller.
    // Every input is a handle the sink thread owns for its whole life; a one-use context
    // struct would rename them without grouping anything that belongs together.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_search_sink(
        hub: WorkspaceChangeHub,
        cursor: crate::change_hub::SinkCursor,
        engine: SharedSearchEngine,
        graph: GraphState,
        overlay_retry: Option<Arc<super::overlay_retry::OverlayRetry>>,
        root_drift_epoch: Arc<AtomicU64>,
        lease: crate::workspace_lease::WorkspaceLease,
        stop: super::OwnerStop,
        backlog: super::overlay_backlog::OverlayBacklog,
        phase: Arc<std::sync::Mutex<super::ConsumerPhase>>,
    ) -> bool {
        let live = stop.enter();
        *phase.lock().unwrap_or_else(|p| p.into_inner()) = super::ConsumerPhase::Attaching;
        // A thread that never starts drops this unrun, and the consumer reads as abandoned.
        let abandon = super::AbandonIfStill(Arc::clone(&phase), super::ConsumerPhase::Attaching);
        std::thread::Builder::new()
            .name("bsl-search-overlay-watch".to_owned())
            .spawn(move || {
                let _live = live;
                let _abandon = abandon;
                // Stopped on every way out of this thread.
                struct StoppedOnExit(Arc<std::sync::Mutex<super::ConsumerPhase>>);
                impl Drop for StoppedOnExit {
                    fn drop(&mut self) {
                        *self.0.lock().unwrap_or_else(|p| p.into_inner()) =
                            super::ConsumerPhase::Stopped;
                    }
                }
                let _stopped = StoppedOnExit(Arc::clone(&phase));
                let mut cursor = cursor;
                let mut generation = 0u64;
                let mut enable_retry = RetryWindow::new(RetryOwner::ChangeHub);
                let mut enable_rescan_debt = false;
                // Watcher mode is one-way in the store and doubles as "skip the full
                // rescan", so the admission asks for it only over a live feed
                // ([`hub_is_feeding`]): a hub that is watching or polling is a feeder that
                // exists.
                loop {
                    match Self::apply_workspace_search(&engine, &stop, &lease, |engine| {
                        // The mode is one-way and doubles as "skip the full rescan", so it is
                        // asked for only where a feed can really exist (see
                        // [`hub_is_feeding`]). A hub that arms later enables the mode where the
                        // feed proves itself — on the first applied batch.
                        if hub_is_feeding(&hub) {
                            engine.enable_workspace_watcher_mode();
                        }
                        Ok(())
                    }) {
                        super::WorkspaceSearchApply::Applied(()) => break,
                        // Told to leave before it could take the engine: go, without sleeping
                        // a retry delay first.
                        super::WorkspaceSearchApply::Stopping => {
                            hub.unsubscribe(cursor);
                            return;
                        }
                        super::WorkspaceSearchApply::TransientRefusal => {
                            let proceed = match enable_retry
                                .refused(std::time::Instant::now(), Duration::from_secs(2))
                            {
                                RetryDecision::RetryAfter(delay) => !stop.sleep(delay),
                                RetryDecision::Stop(_) => {
                                    let fresh = wait_for_fresh_batch(
                                        &hub,
                                        cursor,
                                        &mut generation,
                                        &stop,
                                        &lease,
                                    );
                                    enable_retry
                                        .observe_external_work(std::time::Instant::now(), true);
                                    fresh
                                }
                            };
                            if !proceed {
                                hub.unsubscribe(cursor);
                                return;
                            }
                        }
                        super::WorkspaceSearchApply::Superseded
                        | super::WorkspaceSearchApply::Released => {
                            enable_retry.terminal();
                            hub.unsubscribe(cursor);
                            return;
                        }
                        super::WorkspaceSearchApply::OperationError(error) => {
                            enable_retry.operation_error();
                            enable_rescan_debt = true;
                            tracing::warn!(
                                "could not enable workspace watcher mode; sink stays dormant until fresh work: {error}"
                            );
                            if !wait_for_fresh_batch(&hub, cursor, &mut generation, &stop, &lease)
                            {
                                hub.unsubscribe(cursor);
                                return;
                            }
                            enable_retry = RetryWindow::new(RetryOwner::ChangeHub);
                        }
                    }
                }
                // Only from here does what the cursor reads reach the index.
                *phase.lock().unwrap_or_else(|p| p.into_inner()) = super::ConsumerPhase::Attached;
                tracing::info!("search overlay sink subscribed to workspace change hub");

                let mut pending = None;
                let mut drift_retry = None;
                // The first look takes what the cursor already holds: a fresh batch that ended
                // a dormancy moved `generation` past itself, and waiting on it would sit out a
                // whole idle timeout with that batch unapplied.
                let mut look_now = true;
                let mut rescan_debt = RescanDebt::default();
                if enable_rescan_debt {
                    rescan_debt.record_failure(std::time::Instant::now());
                }
                loop {
                    if owner_must_leave(&hub, &stop, &lease) {
                        hub.unsubscribe(cursor);
                        return;
                    }
                    if pending.is_none() {
                        if !std::mem::take(&mut look_now) {
                            generation = hub.wait_for_change_or(
                                generation,
                                rescan_debt
                                    .wait_for(std::time::Instant::now(), Duration::from_secs(30)),
                                || stop.is_stopped(),
                            );
                        }
                        if owner_must_leave(&hub, &stop, &lease) {
                            hub.unsubscribe(cursor);
                            return;
                        }
                        let batch = hub.materialize(cursor);
                        if rescan_debt.waiting(std::time::Instant::now()) {
                            let fresh = !batch.entries.is_empty() || batch.rescan_required;
                            if fresh {
                                #[cfg(test)]
                                RESCAN_DEBT_WAIT_BRANCH.fetch_add(1, Ordering::SeqCst);
                                if Self::root_transition_relevant_drift(
                                    &batch.entries,
                                    true,
                                    &graph,
                                ) {
                                    root_drift_epoch.fetch_add(1, Ordering::SeqCst);
                                }
                                // Under the fact this batch carries: a debt recorded against
                                // a fact is discharged by the publication that observed it.
                                // A reconcile is told by its own identity, exactly as the
                                // ordinary path tells it: this branch acts on the loss — it
                                // asks for the project to be re-read and acknowledges the
                                // batch — and a loss acted on under a bare fact number reaches
                                // the graph again through the other cursor as news, buying a
                                // second reload for the one event.
                                match batch.loss_token() {
                                    token @ Some(_) => graph.record_loss(token, batch.fact_seq()),
                                    None => graph.record_forced(batch.fact_seq()),
                                }
                                hub.acknowledge(&batch);
                                cursor = batch.cursor;
                                if let Some(retry) = &overlay_retry {
                                    retry.kick_fresh();
                                }
                            }
                            continue;
                        }
                        let fresh = !batch.entries.is_empty() || batch.rescan_required;
                        let rescan_required = batch.rescan_required || rescan_debt.required();
                        // Carried from the batch, where the loss's identity actually is: the
                        // planner classifies entries and never sees it.
                        let loss_token = batch.loss_token();
                        let root_relevant = Self::root_transition_relevant_drift(
                            &batch.entries,
                            rescan_required,
                            &graph,
                        );
                        let mut plan = Self::prepare_search_drift(
                            &engine,
                            &stop,
                            &batch.entries,
                            rescan_required,
                            &graph,
                        );
                        plan.loss_token = loss_token;
                        pending = Some((batch, plan, fresh, root_relevant));
                        drift_retry = Some(RetryWindow::new(RetryOwner::Drift));
                    }
                    let (batch, plan, fresh, root_relevant) = pending.as_mut().unwrap();
                    if *root_relevant {
                        root_drift_epoch.fetch_add(1, Ordering::SeqCst);
                        *root_relevant = false;
                    }
                    let applied = Self::apply_prepared_search_drift(&engine, &stop, &lease, plan, &graph);
                    if matches!(&applied, super::WorkspaceSearchApply::Applied(false)) {
                        continue;
                    }
                    // Leaving, and that is decided BEFORE anything is ordered: every effect
                    // below reaches an owner that would go and do it — the graph starts a
                    // build, the backlog wakes and takes the engine — and a sink that is
                    // going may not order work, whether it goes because the daemon stopped or
                    // because the workspace is no longer this generation's. The batch stays
                    // unacknowledged, so whoever feeds the index next reads it again.
                    if matches!(
                        &applied,
                        super::WorkspaceSearchApply::Stopping
                            | super::WorkspaceSearchApply::Superseded
                            | super::WorkspaceSearchApply::Released
                    ) {
                        if let Some(retry) = drift_retry.as_mut() {
                            retry.terminal();
                        }
                        hub.unsubscribe(cursor);
                        return;
                    }
                    // Outside the engine lock, and before the nudge: the graph consumes the marks
                    // against whichever publication observed this batch's fact, and the order in
                    // which that publication, these marks and the nudge arrive decides nothing.
                    if let Some(mark_high) = plan.mark_high.take() {
                        graph.marks_placed(mark_high, batch.fact_seq());
                    }
                    // Nudge after the mark attempt so a successful build captures its seq
                    // bound. Independent of the apply outcome — an error still gets a graph
                    // catch-up — but told ONCE per plan, like the marks above: the fact is
                    // the graph's debt from the moment it is recorded, and re-recording it on
                    // every retry of the same plan re-opens a debt the graph may already have
                    // answered, buying a walk of the whole tree per refusal.
                    if std::mem::take(&mut plan.nudge_rebuild) {
                        graph.record_change(batch.fact_seq());
                    }
                    if std::mem::take(&mut plan.nudge_project_reload) {
                        match plan.loss_token.take() {
                            // A declared loss carries its own identity: the hub's sequence
                            // stands still while the detail goes, and the same loss reaching
                            // the graph through this cursor and the watcher's is one event.
                            token @ Some(_) => graph.record_loss(token, batch.fact_seq()),
                            None => graph.record_forced(batch.fact_seq()),
                        }
                    }
                    // The batch only MARKED the changed paths; reading them back is the
                    // backlog owner's, told here as a fresh fact — once per set of marks it
                    // has not been told about. A plan that reached here more than once (a
                    // refusal retried, then the rest of its slices applied) marked more paths
                    // each time, and a flag latched on the first telling would leave the rest
                    // to wait out the backlog's idle tick with the index serving stale text.
                    if let Some(marked_through) = backlog_owes_a_telling(plan) {
                        plan.backlog_told_through = marked_through;
                        backlog.fresh();
                    }
                    match applied {
                        super::WorkspaceSearchApply::Applied(true) => {
                            if let Some(retry) = drift_retry.as_mut() {
                                retry.complete();
                            }
                            if plan.full_rescan {
                                rescan_debt.clear();
                            }
                        }
                        super::WorkspaceSearchApply::Stopping
                        | super::WorkspaceSearchApply::Superseded
                        | super::WorkspaceSearchApply::Released => {
                            unreachable!("every way of leaving goes above, before work is ordered")
                        }
                        super::WorkspaceSearchApply::TransientRefusal => {
                            let retry = drift_retry.as_mut().expect("pending drift owns a budget");
                            let delay = super::overlay_retry::retry_delay(retry.streak());
                            if let RetryDecision::RetryAfter(delay) =
                                retry.refused(std::time::Instant::now(), delay)
                            {
                                if stop.sleep(delay) {
                                    hub.unsubscribe(cursor);
                                    return;
                                }
                                continue;
                            }
                            tracing::warn!(
                                "search drift lease retry budget exhausted; advancing the hub cursor"
                            );
                            rescan_debt.record_failure(std::time::Instant::now());
                        }
                        super::WorkspaceSearchApply::OperationError(error) => {
                            if let Some(retry) = drift_retry.as_mut() {
                                retry.operation_error();
                            }
                            tracing::warn!(
                                "search drift apply failed; advancing the hub cursor: {error}"
                            );
                            rescan_debt.record_failure(std::time::Instant::now());
                        }
                        super::WorkspaceSearchApply::Applied(false) => unreachable!(),
                    }
                    hub.acknowledge(batch);
                    cursor = batch.cursor;
                    // Root-transition retry is independent of new file events. This loop
                    // already owns the bounded wake, so no second timer/thread is needed.
                    graph.flush_hook_obligations();
                    // Only GENUINE drift kicks the retry driver (and resets its backoff):
                    // this loop also wakes on the bare 30-second timeout with an empty
                    // batch, and an unconditional kick would zero the backoff each tick.
                    if *fresh {
                        if let Some(retry) = &overlay_retry {
                            retry.kick_fresh();
                        }
                    }
                    pending = None;
                    drift_retry = None;
                }
            })
            .is_ok()
    }

    /// Whether a drained batch can invalidate a root-transition filesystem snapshot. Source and
    /// metadata files, analyzer config, subtree loss and detail-losing rescans are relevant.
    /// `MaybeRemoved` is conservative because a vanished path cannot be stat-ed to distinguish a
    /// file from a directory (including directories whose names contain a dot).
    fn root_transition_relevant_drift(
        entries: &[crate::change_hub::ChangeEntry],
        rescan_required: bool,
        graph: &GraphState,
    ) -> bool {
        rescan_required
            || entries.iter().any(|entry| {
                matches!(
                    entry.kind,
                    crate::change_hub::ChangeKind::MaybeRemoved
                        | crate::change_hub::ChangeKind::SubtreeRemoved
                ) || project_model::file_role(&entry.canonical) != project_model::FileRole::Ignored
                    || project_model::file_role(&entry.raw) != project_model::FileRole::Ignored
                    || graph.is_workspace_config_path(&entry.canonical)
                    || graph.is_workspace_config_path(&entry.raw)
            })
    }

    /// Apply one drained batch to the search overlay. Extracted from the sink loop so it
    /// is unit-testable without driving the thread. On overflow (exact paths lost) it
    /// re-walks the whole tree; otherwise it classifies (stateless policy) and applies
    /// each bucket: `.bsl` bodies dirty, deleted `.bsl` removed, `.xml` → affected context.
    #[cfg(test)]
    pub(super) fn apply_search_drift(
        engine: &SharedSearchEngine,
        stop: &super::OwnerStop,
        entries: &[crate::change_hub::ChangeEntry],
        rescan_required: bool,
        graph: &GraphState,
    ) {
        let mut plan = Self::prepare_search_drift(engine, stop, entries, rescan_required, graph);
        let result = loop {
            let result = Self::apply_prepared_search_drift(
                engine,
                stop,
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                &mut plan,
                graph,
            );
            if !matches!(&result, super::WorkspaceSearchApply::Applied(false)) {
                break result;
            }
        };
        if let Some(mark_high) = plan.mark_high.take() {
            graph.marks_placed(mark_high, entries.iter().map(|entry| entry.seq).max().unwrap_or(0));
        }
        let fact = entries.iter().map(|entry| entry.seq).max().unwrap_or(0);
        if plan.nudge_rebuild {
            graph.record_change(fact);
        }
        if plan.nudge_project_reload {
            graph.record_forced(fact);
        }
        debug_assert!(matches!(result, super::WorkspaceSearchApply::Applied(true)));
    }

    fn prepare_search_drift(
        engine: &SharedSearchEngine,
        stop: &super::OwnerStop,
        entries: &[crate::change_hub::ChangeEntry],
        rescan_required: bool,
        graph: &GraphState,
    ) -> SearchDriftPlan {
        if !graph.validate_workspace_scope() {
            return SearchDriftPlan { preparation_stopping: true, ..SearchDriftPlan::default() };
        }
        let class =
            crate::drift_classify::classify_drift(entries, &std::collections::HashSet::new(), None);
        let mut plan = SearchDriftPlan::default();
        plan.removed_paths.extend(class.bsl_removed.iter().map(|path| path.raw.clone()));
        plan.removed_subtrees.extend(
            entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.kind,
                        crate::change_hub::ChangeKind::SubtreeRemoved
                            | crate::change_hub::ChangeKind::MaybeRemoved
                    )
                })
                .map(|entry| entry.raw.clone()),
        );

        if rescan_required {
            tracing::warn!(
                "workspace change hub overflowed; re-marking all workspace .bsl paths dirty for the search overlay"
            );
            plan.mark_all_context = true;
            plan.full_rescan = true;
            plan.nudge_rebuild = true;
            plan.nudge_project_reload = true;
            Self::prepare_search_rewalk(engine, stop, &mut plan);
            Self::materialize_search_drift(engine, stop, &mut plan);
            return plan;
        }

        plan.dirty_paths.extend(class.bsl_modified.iter().map(|path| path.raw.clone()));
        // The graph holds module bodies, so a `.bsl` edit is drift for it too. This is not a
        // blind rebuild: `nudge_rebuild` reaches `claim_reload_slot`, which compares the disk
        // fingerprint itself and schedules nothing when it still matches. The request path no
        // longer walks disk, so without this nothing would ever notice a body-only change.
        plan.nudge_rebuild |= !class.bsl_modified.is_empty()
            || !class.bsl_added.is_empty()
            || !class.bsl_removed.is_empty()
            || class.structural_rescan;
        if !class.xml_paths.is_empty() {
            let roots = {
                // The root table decides which `.xml` is a root descriptor, so a plan made
                // without it is not a smaller plan but a different one. Refused, the pass has
                // prepared nothing, and the reason is recorded rather than left as an empty
                // plan the caller would acknowledge.
                match engine.acquire_for_owner(stop) {
                    Ok(guard) => {
                        guard.as_ref().and_then(|engine| engine.workspace_roots().cloned())
                    }
                    Err(crate::tools::search::OwnerLockRefused::Closing) => {
                        plan.preparation_stopping = true;
                        return plan;
                    }
                    Err(crate::tools::search::OwnerLockRefused::Poisoned) => {
                        plan.preparation_error = Some("search engine lock poisoned".to_owned());
                        return plan;
                    }
                }
            };
            let mut mark_whole = false;
            for path in &class.xml_paths {
                if is_root_descriptor(roots.as_ref(), &path.raw) {
                    mark_whole = true;
                } else if let Some(subtree) = owned_module_subtree(&path.raw) {
                    plan.context_paths.extend(walk_bsl_files(&subtree));
                }
            }
            let snapshot_paths: Vec<_> =
                class.xml_paths.iter().map(|path| path.raw.clone()).collect();
            match Self::resolve_referencing_module_files(graph, &snapshot_paths, roots.as_ref()) {
                ReferencingFilesOutcome::Applied(paths) => plan.context_paths.extend(paths),
                ReferencingFilesOutcome::OperationError(error) => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::OperationError(error));
                    plan.full_rescan = true;
                }
                ReferencingFilesOutcome::TransientRefusal => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::TransientRefusal);
                    plan.snapshot_paths = snapshot_paths;
                }
                ReferencingFilesOutcome::Superseded => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::Superseded)
                }
                ReferencingFilesOutcome::Released => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::Released)
                }
            }
            plan.mark_all_context |= mark_whole;
            // `|=`, never `=`: a batch carrying both kinds would otherwise lose the `.bsl`
            // arming whenever the xml half resolved to neither a root nor a reader.
            plan.nudge_rebuild |= mark_whole || !plan.context_paths.is_empty();
        }
        if entries.iter().any(|entry| {
            graph.is_workspace_config_path(&entry.canonical)
                || graph.is_workspace_config_path(&entry.raw)
        }) {
            plan.mark_all_context = true;
            plan.nudge_project_reload = true;
        }
        if class.structural_rescan {
            Self::prepare_search_rewalk(engine, stop, &mut plan);
        }
        Self::materialize_search_drift(engine, stop, &mut plan);
        plan
    }

    /// Turn the plan's paths into store keys. Under the engine lock only what lives in memory
    /// is read — the root table, the epoch, the overlay's own keys; every stored key, and every
    /// stat, is read afterwards off the lock. A whole-collection mark or a removal used to load
    /// every stored key under the mutex, and every request waited for it.
    fn materialize_search_drift(
        engine: &SharedSearchEngine,
        stop: &super::OwnerStop,
        plan: &mut SearchDriftPlan,
    ) {
        let needs_carriers = plan.mark_all_context
            || !plan.removed_subtrees.is_empty()
            || plan.reconcile_present.is_some();
        let (roots, carriers, db_path) = {
            let guard = match engine.acquire_for_owner(stop) {
                Ok(guard) => guard,
                Err(crate::tools::search::OwnerLockRefused::Closing) => {
                    plan.preparation_stopping = true;
                    return;
                }
                Err(crate::tools::search::OwnerLockRefused::Poisoned) => {
                    plan.preparation_error = Some("search engine lock poisoned".to_owned());
                    return;
                }
            };
            let Some(engine) = guard.as_ref() else { return };
            plan.roots_epoch = engine.workspace_roots_epoch();
            let Some(roots) = engine.workspace_roots().cloned() else { return };
            let carriers = if needs_carriers { Some(engine.capture_carriers()) } else { None };
            (roots, carriers, engine.store().db_path().to_path_buf())
        };
        #[cfg(test)]
        if needs_carriers {
            let hook = CARRIER_READ_HOOK.lock().unwrap_or_else(|p| p.into_inner()).take();
            match hook {
                Some((thread, hook)) if thread == std::thread::current().id() => hook(),
                other => *CARRIER_READ_HOOK.lock().unwrap_or_else(|p| p.into_inner()) = other,
            }
        }
        let key_of = |path: &PathBuf| bsl_search::workspace_file_key_in(&roots, path);
        plan.dirty_keys
            .extend(plan.dirty_paths.iter().chain(&plan.rewalk_paths).filter_map(key_of));
        plan.context_keys.extend(plan.context_paths.iter().filter_map(key_of));

        let prepared = (|| -> Result<(), bsl_search::SearchError> {
            // The removed PATHS need the journal and nothing else: no carrier snapshot is
            // built for a batch that only deletes files, so the hot path stays what it was
            // (github#192).
            if !plan.removed_paths.is_empty() {
                let reader = bsl_search::Store::open_reader(&db_path)?;
                plan.removed_keys.extend(bsl_search::removed_path_keys(
                    &roots,
                    &reader,
                    &plan.removed_paths,
                )?);
            }
            let Some(capture) = carriers else { return Ok(()) };
            let capture = capture?;
            let reader = bsl_search::Store::open_reader(capture.db_path())?;
            let snapshot = capture.complete(&reader)?;
            if plan.mark_all_context {
                plan.context_keys.extend(snapshot.known_keys());
                plan.mark_all_context = false;
            }
            // The subtrees go through the journal too: a spelling names the key its file was
            // indexed under, which re-attribution cannot reconstruct once the links are gone
            // (github#192).
            plan.removed_keys.extend(snapshot.removal_keys(
                &reader,
                &[],
                &plan.removed_subtrees,
            )?);
            if let Some(present) = &plan.reconcile_present {
                let present: std::collections::HashSet<_> =
                    present.iter().filter_map(key_of).collect();
                plan.removed_keys.extend(snapshot.known_keys().difference(&present).cloned());
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            plan.preparation_error = Some(error.to_string());
        }
        plan.removed_keys.sort_unstable();
        plan.removed_keys.dedup();
        plan.dirty_keys.sort_unstable();
        plan.dirty_keys.dedup();
        plan.context_keys.sort_unstable();
        plan.context_keys.dedup();
    }

    fn prepare_search_rewalk(
        engine: &SharedSearchEngine,
        stop: &super::OwnerStop,
        plan: &mut SearchDriftPlan,
    ) {
        // A walk needs the root table, and the table is behind the same admission the stop
        // refuses. Refused, the pass has prepared nothing: reporting that as an empty plan
        // would let the caller acknowledge a batch it never applied.
        let (declared, excluded, user_excluded) =
            match Self::registered_roots_and_exclusions(engine, stop) {
                Ok(Some(pair)) => pair,
                Ok(None) => return,
                Err(crate::tools::search::OwnerLockRefused::Closing) => {
                    plan.preparation_stopping = true;
                    return;
                }
                Err(crate::tools::search::OwnerLockRefused::Poisoned) => {
                    plan.preparation_error = Some("search engine lock poisoned".to_owned());
                    return;
                }
            };
        let set = project_model::SourceSet::scan_in_scope(&declared, &excluded, &user_excluded);
        let present: std::collections::HashSet<_> = set
            .files
            .iter()
            .filter(|file| file.role == project_model::FileRole::Source)
            .map(|file| file.walked.clone())
            .collect();
        plan.rewalk_paths.extend(present.iter().cloned());
        let incomplete = !set.clean();
        #[cfg(test)]
        let incomplete =
            incomplete || FORCE_REWALK_WALK_ERROR.load(std::sync::atomic::Ordering::SeqCst);
        if incomplete {
            tracing::warn!(
                unreadable = set.unreadable,
                canonical_fallbacks = set.canonical_fallbacks,
                "search rescan walk incomplete; skipping reconcile to avoid deleting healthy files"
            );
        } else {
            plan.reconcile_present = Some(present);
        }
    }

    fn apply_prepared_search_drift(
        shared: &SharedSearchEngine,
        stop: &super::OwnerStop,
        lease: &crate::workspace_lease::WorkspaceLease,
        plan: &mut SearchDriftPlan,
        graph: &GraphState,
    ) -> super::WorkspaceSearchApply<bool, bsl_search::SearchError> {
        if !graph.validate_workspace_scope() {
            return super::WorkspaceSearchApply::Stopping;
        }
        if matches!(plan.snapshot_outcome, Some(SnapshotPreparationOutcome::TransientRefusal)) {
            let roots = match shared.acquire_for_owner(stop) {
                Ok(guard) => guard.as_ref().and_then(|engine| engine.workspace_roots().cloned()),
                Err(crate::tools::search::OwnerLockRefused::Closing) => {
                    return super::WorkspaceSearchApply::Stopping;
                }
                Err(crate::tools::search::OwnerLockRefused::Poisoned) => {
                    return super::WorkspaceSearchApply::OperationError(
                        bsl_search::SearchError::Index("search engine lock poisoned".to_owned()),
                    );
                }
            };
            match Self::resolve_referencing_module_files(
                graph,
                &plan.snapshot_paths,
                roots.as_ref(),
            ) {
                ReferencingFilesOutcome::Applied(paths) => {
                    plan.context_paths.extend(paths);
                    // The readers only became known now, so the arming decision that was made
                    // against an empty set has to be made again.
                    plan.nudge_rebuild |= !plan.context_paths.is_empty();
                    plan.snapshot_outcome = None;
                    plan.snapshot_paths.clear();
                    Self::materialize_search_drift(shared, stop, plan);
                }
                ReferencingFilesOutcome::OperationError(error) => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::OperationError(error));
                }
                ReferencingFilesOutcome::TransientRefusal => {}
                ReferencingFilesOutcome::Superseded => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::Superseded)
                }
                ReferencingFilesOutcome::Released => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::Released)
                }
            }
        }
        // Checked after the re-materialisation above, which is itself a preparation step and
        // can be the one that is refused.
        if plan.preparation_stopping {
            return super::WorkspaceSearchApply::Stopping;
        }
        if let Some(outcome) = plan.snapshot_outcome.as_ref() {
            return match outcome {
                SnapshotPreparationOutcome::OperationError(error) => {
                    super::WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(
                        error.clone(),
                    ))
                }
                SnapshotPreparationOutcome::TransientRefusal => {
                    super::WorkspaceSearchApply::TransientRefusal
                }
                SnapshotPreparationOutcome::Superseded => super::WorkspaceSearchApply::Superseded,
                SnapshotPreparationOutcome::Released => super::WorkspaceSearchApply::Released,
            };
        }
        #[cfg(test)]
        let force_apply_error =
            FORCE_DRIFT_APPLY_ERROR_ENGINE.load(Ordering::SeqCst) == Arc::as_ptr(shared) as usize;
        // A pass with nothing to apply must not touch the lease. The fence takes the
        // lock file, re-reads the record and restamps it through `checkpoint`, and all
        // three land inside the workspace when the cache sits under it — so an empty
        // pass would publish the very event that woke it and wake itself again.
        //
        // The fence is also where a superseded generation learns it lost the caches, so
        // this moves that discovery from "every wake" to "every wake that has work" — the
        // right place for it: a pass with nothing to write has nothing to publish over a
        // new owner, and it goes straight back to sleep.
        if plan.preparation_error.is_none() && plan.complete() {
            if !graph.validate_workspace_scope() {
                return super::WorkspaceSearchApply::Stopping;
            }
            // Still epoch-checked, because "empty" is itself a verdict of the roots the
            // plan was prepared against: a file under a root registered since then maps
            // to no key yet, so the plan comes out empty and must be replanned, not
            // reported as nothing to do. Reading the epoch takes the engine lock, which
            // this pass takes anyway — what the early return avoids is the LEASE fence.
            // Anything other than a confirmed match falls through to the full path, whose
            // answer for a missing or poisoned engine is already the right one.
            let current = shared.acquire_for_owner(stop).ok().and_then(|guard| {
                guard.as_ref().map(bsl_search::SearchEngine::workspace_roots_epoch)
            });
            if current == Some(plan.roots_epoch) {
                return super::WorkspaceSearchApply::Applied(true);
            }
        }
        let dirty_start = plan.dirty_cursor;
        let dirty_end =
            (dirty_start + bsl_search::WORKSPACE_APPLY_BATCH_ROWS).min(plan.dirty_keys.len());
        let mut remaining = bsl_search::WORKSPACE_APPLY_BATCH_ROWS - (dirty_end - dirty_start);
        let removed_start = plan.removed_cursor;
        let removed_end = (removed_start + remaining).min(plan.removed_keys.len());
        remaining -= removed_end - removed_start;
        let context_start = plan.context_cursor;
        let context_end = (context_start + remaining).min(plan.context_keys.len());

        // The final Project check happens before taking the shared engine or lease
        // fence: a prepared batch may be delayed while the immutable scope changes.
        if !graph.validate_workspace_scope() {
            return super::WorkspaceSearchApply::Stopping;
        }

        let outcome = Self::apply_workspace_search(shared, stop, lease, |engine| {
            if engine.workspace_roots_epoch() != plan.roots_epoch {
                return Err(bsl_search::SearchError::Index(
                    "workspace roots changed after search drift preparation".to_owned(),
                ));
            }
            if let Some(error) = &plan.preparation_error {
                return Err(bsl_search::SearchError::Index(error.clone()));
            }
            #[cfg(test)]
            if force_apply_error {
                return Err(bsl_search::SearchError::Index(
                    "forced drift apply failure".to_owned(),
                ));
            }
            let mut checkpoint = || std::ops::ControlFlow::Continue(());
            match engine.apply_prepared_workspace_drift_batch(
                &plan.dirty_keys[dirty_start..dirty_end],
                &plan.removed_keys[removed_start..removed_end],
                &plan.context_keys[context_start..context_end],
                &mut checkpoint,
            ) {
                std::ops::ControlFlow::Continue(result) => result,
                std::ops::ControlFlow::Break(()) => {
                    unreachable!("short publication checkpoint always continues")
                }
            }
        });
        match outcome {
            super::WorkspaceSearchApply::Applied(mark) => {
                plan.mark_high = plan.mark_high.max(mark);
                plan.dirty_cursor = dirty_end;
                plan.removed_cursor = removed_end;
                plan.context_cursor = context_end;
                super::WorkspaceSearchApply::Applied(plan.complete())
            }
            super::WorkspaceSearchApply::TransientRefusal => {
                super::WorkspaceSearchApply::TransientRefusal
            }
            super::WorkspaceSearchApply::Stopping => super::WorkspaceSearchApply::Stopping,
            super::WorkspaceSearchApply::Superseded => super::WorkspaceSearchApply::Superseded,
            super::WorkspaceSearchApply::Released => super::WorkspaceSearchApply::Released,
            super::WorkspaceSearchApply::OperationError(error) => {
                super::WorkspaceSearchApply::OperationError(error)
            }
        }
    }

    /// Reverse-look-up the workspace modules that READ any changed MDO, returning the graph's
    /// own absolute spelling of each. A metadata change alters the `graph_context` of every
    /// module that reads the object — not just its owned modules — and the persisted graph is
    /// the only record of who reads what.
    ///
    /// Absolute, because attributing a path to its root is one procedure and it lives on the
    /// root table: a caller that relativised the path itself would be a second one, and being
    /// able to strip only ONE root's prefix is exactly how the modules of every other root
    /// used to fall out of the result.
    ///
    /// The graph keeps these paths as strings, so under a root whose name holds bytes no
    /// `str` can carry they come back rendered: such a path attributes to nothing, or — with
    /// roots nested inside one another — to an ancestor, under whose key the mark normally
    /// finds no row. Either way the module itself waits for a wider mark, and that is the
    /// deliberate answer: a rendering fits several roots at once, and a key guessed from it
    /// would name a file that did not change.
    ///
    /// Queries the CURRENTLY PUBLISHED graph via [`GraphState::read_blocking`], which gates on a
    /// published build and opens the read-only db off the graph's inner lock. Pre-drift edges
    /// are exactly right here: the set of referencing modules is defined by OTHER modules'
    /// bodies, which this `.xml` edit did not touch — the follow-up rebuild only re-renders the
    /// contexts marked here, it never changes who references the object. No published graph yet
    /// (or an `.xml` that maps to no MDO node — a form/command/config-root descriptor) → an
    /// empty set, so referencing marks are simply skipped and the owned marks + nudge still fire;
    /// a later publish consumes whatever marks then exist. Degrades, never blocks or errors.
    ///
    /// Off-lock throughout: opens the graph db once and runs one index-backed inbound-edge
    /// query per resolved MDO node id, so a batch of N `.xml` edits does at most N indexed
    /// queries, never a table scan.
    fn resolve_referencing_module_files(
        graph: &GraphState,
        xml_paths: &[PathBuf],
        fallback_roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> ReferencingFilesOutcome {
        use crate::workspace_lease::{LeaseOperationError, LeaseOperationOutcome};

        let mut files = std::collections::HashSet::new();
        let mdo_ids: Vec<String> =
            xml_paths.iter().filter_map(|path| xml_to_mdo_id(path)).collect();
        if mdo_ids.is_empty() {
            return ReferencingFilesOutcome::Applied(files);
        }
        let referencing = graph.read_blocking(|snapshot| {
            let Some(roots) = snapshot.workspace_roots().or(fallback_roots) else {
                return ReferencingFilesOutcome::TransientRefusal;
            };
            for mdo_id in mdo_ids {
                match snapshot.graph.referencing_files(&mdo_id, Some(roots)) {
                    Ok(found) => files.extend(found.into_iter().map(PathBuf::from)),
                    Err(error) => {
                        return ReferencingFilesOutcome::OperationError(format!(
                            "referencing-files lookup failed for {mdo_id}: {error}"
                        ));
                    }
                }
            }
            ReferencingFilesOutcome::Applied(files)
        });
        match referencing {
            LeaseOperationOutcome::Applied(Some(outcome)) => outcome,
            LeaseOperationOutcome::Applied(None) => {
                ReferencingFilesOutcome::Applied(std::collections::HashSet::new())
            }
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                crate::graph::BackgroundSnapshotError::Changed,
            )) => ReferencingFilesOutcome::OperationError(
                "background graph snapshot changed during preparation".to_owned(),
            ),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error)) => {
                ReferencingFilesOutcome::OperationError(format!(
                    "background graph snapshot lease failed: {error}"
                ))
            }
            LeaseOperationOutcome::TransientRefusal => ReferencingFilesOutcome::TransientRefusal,
            LeaseOperationOutcome::Superseded => ReferencingFilesOutcome::Superseded,
            LeaseOperationOutcome::Released => ReferencingFilesOutcome::Released,
        }
    }

    /// Re-mark every workspace `.bsl` dirty for the search overlay, then reconcile the
    /// store against what is actually on disk. Used when the change hub overflowed or a
    /// subtree was removed and the exact changed paths are no longer known, so the overlay
    /// must reconsider the whole tree. Marking alone only covers files that STILL exist; a
    /// file deleted during the lost window would keep its FTS rows and vectors forever, so
    /// the reconcile diffs the walked (present) set against the stored set and removes the
    /// gone paths. The walk covers EVERY registered root, through the shared source-set walk,
    /// and runs OUTSIDE the engine lock; the reconcile takes the lock only for its bounded
    /// O(stored) store writes.
    #[cfg(test)]
    fn rewalk_workspace_bsl_dirty(engine: &SharedSearchEngine, stop: &super::OwnerStop) {
        let Some(declared) = Self::registered_roots(engine, stop) else { return };
        let set = project_model::SourceSet::scan(&declared);
        let mut present: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for file in &set.files {
            if file.role != project_model::FileRole::Source {
                continue;
            }
            present.insert(file.walked.clone());
            Self::mark_search_path_dirty(engine, stop, &file.walked);
        }
        let incomplete = !set.clean();
        #[cfg(test)]
        let incomplete =
            incomplete || FORCE_REWALK_WALK_ERROR.load(std::sync::atomic::Ordering::SeqCst);
        // An incomplete scan is NOT authoritative: `present` is missing healthy files, so
        // reconciling against it would delete them from the store. Marking the found files dirty
        // already happened above regardless.
        if incomplete {
            tracing::warn!(
                unreadable = set.unreadable,
                canonical_fallbacks = set.canonical_fallbacks,
                "search rescan walk incomplete; skipping reconcile to avoid deleting healthy files"
            );
            return;
        }
        if let Ok(mut guard) = engine.acquire_for_owner(stop) {
            if let Some(engine) = guard.as_mut() {
                match engine.reconcile_workspace_files(&present) {
                    Ok(removed) if removed > 0 => {
                        tracing::info!(
                            removed,
                            "search rescan reconciled deleted files out of the index"
                        )
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("search rescan reconcile failed: {e}"),
                }
            }
        }
    }

    /// The declared spelling of every root the engine indexes, read under a brief lock so the
    /// walk itself runs with none held. Reading the table rather than a path captured at startup
    /// is what keeps the walk and the store's keys speaking of the same universe: a walk narrower
    /// than the table makes the reconcile below delete the roots it never visited.
    /// Test-side wrapper: production always states its exclusions, so the form that
    /// narrows by nothing is not reachable there by construction.
    #[cfg(test)]
    fn registered_roots(
        engine: &SharedSearchEngine,
        stop: &super::OwnerStop,
    ) -> Option<Vec<PathBuf>> {
        Self::registered_roots_and_exclusions(engine, stop)
            .ok()
            .flatten()
            .map(|(roots, _, _)| roots)
    }

    /// The registered roots together with the subtrees a walk of them must skip.
    ///
    /// Returned as a pair, and read under the one lock: a caller that fetched the roots
    /// and the holes separately could pair a fresh root set with a stale hole, and the
    /// walk would then read the cache of a workspace it no longer serves.
    fn registered_roots_and_exclusions(
        engine: &SharedSearchEngine,
        stop: &super::OwnerStop,
    ) -> Result<Option<RootsAndHoles>, crate::tools::search::OwnerLockRefused> {
        let guard = engine.acquire_for_owner(stop)?;
        Ok((|| {
            let engine = guard.as_ref()?;
            let roots = engine.workspace_roots()?;
            Some((
                roots.entries().map(|(_, declared)| declared.to_path_buf()).collect(),
                roots.excluded().to_vec(),
                roots.user_excluded().clone(),
            ))
        })())
    }

    /// Reconcile the just-indexed workspace store against on-disk truth at BOOT, on the still-owned
    /// engine (no shared lock held), BEFORE the overlay-init decision is applied. A boot index step
    /// (`index_directory_deferred` / `index_directory_fts`, or a fused parse ingest) only re-ingests
    /// files that EXIST now — it never removes rows for a `.bsl` DELETED while the daemon was down.
    /// So a store row for a vanished file survives, and an [`OverlayInit::Clean`] — which asserts the
    /// store already equals the working tree — would serve that ghost forever. This walks the source
    /// tree (error-aware) and, on a CLEAN walk, calls [`SearchEngine::reconcile_workspace_files`] to
    /// remove every stored-but-gone path (overlay dirty + baseline hiding + incremental vector eviction —
    /// the same removal path the overflow rescan ships).
    ///
    /// Returns whether the store was PROVEN reconciled: `false` on any walk error OR a reconcile
    /// failure. A partial walk's `present` set is short, so trusting it would delete healthy rows —
    /// hence the S1 gate (skip reconcile on any walk error) is kept verbatim. And because a failed
    /// walk could not prove reconciliation, the caller must NOT stay Clean: it downgrades to a prime,
    /// whose own scan lazily hides files it finds missing. A prime's scan may itself be incomplete
    /// after a walk error, but a prime never ASSERTS a clean store the way `Clean` does — it only
    /// serves what it can see and hides the rest — so it is the strictly safer degraded default,
    /// matching the pre-existing behavior for a store that could not be reconciled.
    pub(super) fn reconcile_boot_store_with_disk_fenced(
        engine: &mut SearchEngine,
        lease: &crate::workspace_lease::WorkspaceLease,
        stop: &super::OwnerStop,
    ) -> Option<bool> {
        let Some(roots) = engine.workspace_roots() else { return Some(false) };
        let declared: Vec<PathBuf> =
            roots.entries().map(|(_, declared)| declared.to_path_buf()).collect();
        let excluded = roots.excluded().to_vec();
        let set =
            project_model::SourceSet::scan_in_scope(&declared, &excluded, roots.user_excluded());
        let present: std::collections::HashSet<PathBuf> = set
            .files
            .iter()
            .filter(|file| file.role == project_model::FileRole::Source)
            .map(|file| file.walked.clone())
            .collect();
        let incomplete = !set.clean();
        #[cfg(test)]
        let incomplete =
            incomplete || FORCE_REWALK_WALK_ERROR.load(std::sync::atomic::Ordering::SeqCst);
        if incomplete {
            tracing::warn!(
                unreadable = set.unreadable,
                canonical_fallbacks = set.canonical_fallbacks,
                "search boot reconcile walk incomplete; priming the overlay instead of clean-init"
            );
            return Some(false);
        }
        match engine.reconcile_workspace_files_fenced(&present, |apply| {
            Self::startup_apply(lease, stop, apply)
        }) {
            Ok(bsl_search::FenceOutcome::Applied(removed)) => {
                if removed > 0 {
                    tracing::info!(
                        removed,
                        "search boot reconciled deleted files out of the store"
                    );
                }
                Some(true)
            }
            Ok(bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released) => None,
            Ok(bsl_search::FenceOutcome::TransientRefusal) => {
                unreachable!("startup_apply retries transient refusals")
            }
            Err(e) => {
                tracing::warn!("search boot reconcile failed; priming the overlay instead: {e}");
                Some(false)
            }
        }
    }
    /// Mark one path dirty in the search overlay if it is a `.bsl` file. Filtering
    /// on the consumer side keeps the hub itself extension-agnostic.
    #[cfg(test)]
    fn mark_search_path_dirty(engine: &SharedSearchEngine, stop: &super::OwnerStop, path: &Path) {
        if !project_model::is_bsl_source_path(path) {
            return;
        }
        if let Ok(guard) = engine.acquire_for_owner(stop) {
            if let Some(engine) = guard.as_ref() {
                if let Err(e) = engine.mark_workspace_path_dirty(path) {
                    tracing::warn!(path = ?path, "failed to mark workspace file dirty: {e}");
                }
            }
        }
    }
}

/// The owned-module subtree of a metadata descriptor `.xml`: `<Dir>/<Name>/` beside a
/// `<Dir>/<Name>.xml`, when that directory exists. Every `.bsl` under it (object /
/// manager / recordset / form / command modules, or a common-module / service body) is
/// owned by the object the descriptor defines — so the path convention covers ordinary
/// MDOs (which carry no substrate back-link) and common-modules/services alike, with no
/// resident lookup and no resident/engine lock coupling.
fn owned_module_subtree(xml: &Path) -> Option<PathBuf> {
    let stem = xml.file_stem()?;
    let subtree = xml.parent()?.join(stem);
    subtree.is_dir().then_some(subtree)
}

/// Every `.bsl` file under `dir`.
fn walk_bsl_files(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .filter(|p| project_model::is_bsl_source_path(p))
        .collect()
}

/// Map a metadata descriptor `.xml` at `<KindPlural>/<Name>.xml` to its graph MDO node id
/// `mdo/<EnglishType>/<Name>` (the id the fused build encodes, verified against
/// `ide::GraphRowEncoder`). `None` when the parent directory is not a known metadata-kind
/// plural — a form/command descriptor, an `Ext/…` file, or a configuration-root descriptor —
/// since those carry no `mdo/` node and thus no inbound read edges to reverse-look-up. The
/// `<KindPlural>` → [`bsl_metadata::MdoType`] mapping reuses the canonical
/// [`bsl_metadata::MdoType::from_plural`] table rather than duplicating a directory map.
fn xml_to_mdo_id(xml: &Path) -> Option<String> {
    let name = xml.file_stem()?.to_str()?;
    let kind_dir = xml.parent()?.file_name()?.to_str()?;
    let mdo_type = bsl_metadata::MdoType::from_plural(kind_dir)?;
    Some(format!("mdo/{}/{name}", mdo_type.english_name()))
}

/// Whether `xml` is the root descriptor of a source tree — `Configuration.xml`,
/// `ConfigDumpInfo.xml`, a plugin's own root descriptor. Such a change can shift ANY
/// module's context, so it is answered conservatively with a whole-collection mark rather
/// than a resolvable owned subtree.
///
/// Three independent signs, because no one of them covers the class:
///
/// - the path attributes to the TOP LEVEL of a registered root. Ranked by both spellings,
///   so an aliased delivery answers the same as a canonical one; and it is the only sign
///   that recognises a root which is not a 1C dump at all and therefore carries no
///   `Configuration.xml`;
/// - the descriptor's own directory CONTAINS a `Configuration.xml` — the same disk probe
///   by which the project model tells an extension from an ordinary directory. The root
///   table deliberately omits the roots it rejected (one inside the configuration, one
///   whose identifier was taken), and a tree nobody declared is not in it either, so a
///   question asked of the table alone leaves their descriptors unrecognised;
/// - the file's own name is `Configuration.xml`. What is left when the descriptor itself
///   is what vanished: the neighbour the sign above looks for is the file now gone.
fn is_root_descriptor(roots: Option<&bsl_search::WorkspaceRoots>, xml: &Path) -> bool {
    let at_root_of_a_registered_root = roots
        .and_then(|roots| roots.key_of_path(xml))
        .is_some_and(|key| Path::new(&key.path).components().count() == 1);
    if at_root_of_a_registered_root {
        return true;
    }
    let beside_a_configuration_xml = xml.parent().is_some_and(|dir| {
        bsl_conventions::find_child_ci(
            dir,
            bsl_conventions::ConventionalName::ConfigurationXml.canonical(),
        )
        .is_some_and(|found| found.is_file())
    });
    beside_a_configuration_xml
        || xml.file_name().and_then(|n| n.to_str()).and_then(bsl_conventions::conventional_of)
            == Some(bsl_conventions::ConventionalName::ConfigurationXml)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        env_lock, write_common_module, write_common_module_tree, EnvVarGuard,
    };
    use super::{
        backlog_owes_a_telling, SearchDriftPlan, SharedState, SnapshotPreparationOutcome,
        FORCE_REWALK_WALK_ERROR,
    };
    use crate::state::types::OverlayInit;
    use bsl_search::{IndexedDocument, SearchEngine};
    use std::fs;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    /// A stop is decided before anything is ordered. Every effect the sink performs after an
    /// apply reaches an owner that would then go and do the work — the graph starts a build,
    /// the backlog owner wakes and takes the engine — and between `owners.stop()` and the
    /// lease's release that work would run on a workspace already being handed over. Counted
    /// rather than acted out: the ordering lives inside the sink thread, and a test that
    /// drove it would have to win the same race the defect needs.
    #[test]
    fn the_sink_decides_to_leave_before_it_orders_any_work() {
        // Every way of leaving, not only the stop: a superseded or released generation must
        // not wake the backlog or start a graph build on a workspace it no longer owns.
        let source = include_str!("sync.rs");
        let sink = source.split_once("fn spawn_search_sink(").expect("the sink is still there").1;
        let applied = sink
            .find("let applied = Self::apply_prepared_search_drift")
            .expect("the sink still applies a prepared plan");
        let ordered = sink[applied..]
            .find("graph.marks_placed(")
            .expect("the sink still hands its marks to the graph");
        let before_ordering = &sink[applied..applied + ordered];
        for leaving in ["Stopping", "Superseded", "Released"] {
            assert!(
                before_ordering.contains(&format!("WorkspaceSearchApply::{leaving}")),
                "the sink orders work before it decides whether it is leaving ({leaving})"
            );
        }
    }

    /// A plan applied in slices tells the backlog owner about the marks of every slice that
    /// lands, not only the first. Between two of its slices a refusal can send the sink into
    /// a backoff of half a minute, and the owner drains what it was told and goes idle; the
    /// keys marked afterwards would then wait out its idle tick — or, if its budget was
    /// already spent, never be read back at all.
    #[test]
    fn every_slice_of_marks_is_told_to_the_backlog_owner() {
        let mut plan = SearchDriftPlan::default();

        assert_eq!(backlog_owes_a_telling(&plan), None, "an empty plan marked nothing");

        plan.dirty_cursor = 64;
        assert_eq!(
            backlog_owes_a_telling(&plan),
            Some((64, 0)),
            "the first slice's marks are owed to the owner"
        );
        plan.backlog_told_through = (64, 0);
        assert_eq!(
            backlog_owes_a_telling(&plan),
            None,
            "the same marks are not told twice — one fact, told once"
        );

        // The refusal is retried and the rest of the plan lands.
        plan.dirty_cursor = 130;
        plan.removed_cursor = 7;
        assert_eq!(
            backlog_owes_a_telling(&plan),
            Some((130, 7)),
            "the marks of the later slices are owed too"
        );
    }

    /// A preparation refused because the daemon is leaving says so. It used to report the
    /// stop as "search engine lock poisoned", which the caller then turned into an operation
    /// error: a clean shutdown looked like a broken engine, and the batch it never applied
    /// was acknowledged all the same.
    #[test]
    fn a_preparation_refused_by_a_stop_is_stopping_not_a_poisoned_lock() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let module = workspace.join("Module.bsl");
        fs::write(&module, "Процедура П() КонецПроцедуры\n").unwrap();
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let graph = crate::graph::GraphState::disabled();
        let entries = [ChangeEntry {
            canonical: module.clone(),
            raw: module,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        }];

        // Both shapes of a preparation, because they take the engine in different places: a
        // plain batch materialises the plan against the root table, and a rescan walks the
        // registered roots first. Either one refused by the stop must answer the same.
        for rescan in [false, true] {
            let quiet = crate::state::OwnerStop::default();
            // Control: with nothing stopped the same batch prepares and applies.
            let mut planned =
                SharedState::prepare_search_drift(&shared, &quiet, &entries, rescan, &graph);
            let applied = SharedState::apply_prepared_search_drift(
                &shared,
                &quiet,
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                &mut planned,
                &graph,
            );
            assert!(
                matches!(applied, crate::state::WorkspaceSearchApply::Applied(_)),
                "the control batch (rescan {rescan}) answered {applied:?}, so the refusal \
                 below would prove nothing"
            );

            let stop = crate::state::OwnerStop::default();
            stop.stop();

            let mut plan =
                SharedState::prepare_search_drift(&shared, &stop, &entries, rescan, &graph);
            // The plan itself, because that is where the two answers differ: the outcome
            // below is `Stopping` either way — the apply's own admission refuses a stopped
            // owner before it looks at the plan — while a preparation that recorded the stop
            // as a broken engine carries that error into every later reading of the plan.
            assert!(
                plan.preparation_stopping,
                "the refused preparation (rescan {rescan}) did not record a stop"
            );
            assert!(
                plan.preparation_error.is_none(),
                "the stop was recorded as an engine failure (rescan {rescan}): {:?}",
                plan.preparation_error
            );
            let outcome = SharedState::apply_prepared_search_drift(
                &shared,
                &stop,
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                &mut plan,
                &graph,
            );
            assert!(
                matches!(outcome, crate::state::WorkspaceSearchApply::Stopping),
                "a stop during preparation (rescan {rescan}) came back as {outcome:?}"
            );
        }
    }

    /// A whole-collection mark needs every stored key, and reading them all takes as long as
    /// the store is large. It happens with the engine lock given back: a request arriving
    /// meanwhile takes the lock at its first attempt.
    #[test]
    fn reading_every_stored_key_does_not_hold_the_engine_lock() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(workspace.join("Module.bsl"), "Процедура П() КонецПроцедуры\n").unwrap();
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel::<()>();
        let release = Mutex::new(release);
        let preparer = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                *super::CARRIER_READ_HOOK.lock().unwrap() = Some((
                    std::thread::current().id(),
                    Box::new(move || {
                        entered_tx.send(()).unwrap();
                        release
                            .lock()
                            .unwrap()
                            .recv_timeout(std::time::Duration::from_secs(30))
                            .unwrap();
                    }),
                ));
                let graph = crate::graph::GraphState::disabled();
                SharedState::prepare_search_drift(
                    &shared,
                    &crate::state::OwnerStop::default(),
                    &[],
                    true,
                    &graph,
                )
            })
        };
        entered.recv_timeout(std::time::Duration::from_secs(30)).unwrap();

        let started = std::time::Instant::now();
        let acquired = crate::tools::search::try_acquire_engine(
            &shared,
            &tokio_util::sync::CancellationToken::new(),
        )
        .is_ok();
        let waited = started.elapsed();
        release_tx.send(()).unwrap();
        let plan = preparer.join().unwrap();

        assert!(acquired);
        assert!(
            waited < crate::tools::search::ACQUIRE_POLL,
            "the request waited {waited:?} behind the stored-key read"
        );
        assert!(!plan.context_keys.is_empty(), "the mark still covers every stored key");
    }

    #[test]
    fn continuous_events_do_not_reset_rescan_debt_backoff() {
        let start = std::time::Instant::now();
        let mut debt = super::RescanDebt::default();
        debt.record_failure(start);
        assert!(!debt.waiting(start), "the first retry is immediate");

        debt.record_failure(start);
        let next = debt.next_allowed;
        for offset in 1..30 {
            assert!(debt.waiting(start + std::time::Duration::from_secs(offset)));
            assert_eq!(debt.next_allowed, next, "fresh wakeups do not move the deadline");
        }
        assert!(!debt.waiting(start + std::time::Duration::from_secs(30)));

        debt.clear();
        assert!(!debt.required(), "a converged full rescan retires the one debt slot");
    }

    /// The idle pass is the hot one: with the cache under the watched tree, every
    /// lock take and every lease restamp it performs is an event that wakes it again.
    /// The observable is the lock file, not the lease record — the record carries a
    /// whole-second stamp, so a rewrite inside the same second leaves it byte-identical
    /// and an assertion on its contents would hold over a pass that did write.
    #[test]
    fn an_empty_drift_plan_never_takes_the_lease_fence() {
        let dir = tempdir().unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(
            SearchEngine::fts_only(&dir.path().join("search.db")).unwrap(),
        ));
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let lock = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path()).lease_lock_path();
        std::fs::remove_file(&lock).unwrap();

        let mut empty = SearchDriftPlan::default();
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &crate::state::OwnerStop::default(),
                &lease,
                &mut empty,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::Applied(true)
        ));
        assert!(!lock.exists(), "an empty plan took the lease fence");

        // Positive control: a plan with work must still take it, or the assertion
        // above would hold on an apply that does nothing at all.
        let mut work = SearchDriftPlan {
            dirty_keys: vec![bsl_search::FileKey::configuration("src/a.bsl")],
            ..Default::default()
        };
        SharedState::apply_prepared_search_drift(
            &shared,
            &crate::state::OwnerStop::default(),
            &lease,
            &mut work,
            &crate::graph::GraphState::disabled(),
        );
        assert!(lock.exists(), "a plan with work skipped the lease fence");
    }

    #[test]
    fn workspace_cache_scope_search_drift_refuses_prepared_batch_before_engine_or_lease() {
        let workspace = tempdir().unwrap();
        let cache_parent = tempdir().unwrap();
        fs::write(workspace.path().join("Configuration.xml"), "<Configuration/>").unwrap();
        fs::write(workspace.path().join("Module.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let project = crate::project::at(workspace.path()).unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_project(
            &project,
            Some(&cache_parent.path().join("cache")),
            cache_parent.path(),
            None,
        )
        .unwrap();
        cache.ensure().unwrap();
        let mut engine = SearchEngine::fts_only(&cache.search_db_path()).unwrap();
        engine.set_workspace_root(workspace.path().to_path_buf());
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let owners = crate::state::OwnerStop::default();
        let transport = tokio_util::sync::CancellationToken::new();
        let graph = crate::graph::GraphState::for_workspace_with_cache(
            workspace.path().to_path_buf(),
            cache.clone(),
        )
        .with_owner_stop(owners.clone())
        .with_scope_transport_stop(transport.clone());
        let mut plan = SearchDriftPlan {
            dirty_keys: vec![bsl_search::FileKey::configuration("Module.bsl")],
            ..Default::default()
        };
        fs::write(
            workspace.path().join("bsl-analyzer.toml"),
            "[source]\nexclude = [\"generated\"]\n",
        )
        .unwrap();
        fs::remove_file(cache.lease_lock_path()).unwrap();

        assert!(matches!(
            SharedState::apply_prepared_search_drift(&shared, &owners, &lease, &mut plan, &graph,),
            crate::state::WorkspaceSearchApply::Stopping
        ));
        assert!(
            !cache.lease_lock_path().exists(),
            "scope drift was rejected before the lease fence"
        );
        assert!(owners.is_stopped());
        assert!(transport.is_cancelled());
        assert!(
            bsl_search::Store::open_existing(&cache.search_db_path())
                .unwrap()
                .all_files()
                .unwrap()
                .is_empty(),
            "the prepared batch was not published into the old search store"
        );
    }

    #[test]
    fn superseded_daemon_cannot_mutate_shared_search() {
        struct Provider;
        impl bsl_search::GraphContextProvider for Provider {
            fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
                Some("graph".to_owned())
            }
        }

        #[derive(Clone, Copy, Debug)]
        enum Family {
            Watcher,
            Dirty,
            Context,
            Delete,
            Subtree,
            Reconcile,
            Point,
            Roots,
            Provider,
            ContextRefresh,
        }

        for family in [
            Family::Watcher,
            Family::Dirty,
            Family::Context,
            Family::Delete,
            Family::Subtree,
            Family::Reconcile,
            Family::Point,
            Family::Roots,
            Family::Provider,
            Family::ContextRefresh,
        ] {
            let dir = tempdir().unwrap();
            let source = dir.path().join("Module.bsl");
            fs::write(&source, "Процедура П()\nКонецПроцедуры").unwrap();
            let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
            engine.set_workspace_root(dir.path().to_path_buf());
            engine.index_directory_fts(dir.path()).unwrap();
            let mut point_batch = matches!(family, Family::Point).then(|| {
                engine.initialize_workspace_overlay_clean().unwrap();
                engine.mark_workspace_path_dirty(&source).unwrap();
                let capture = engine.capture_point_refresh(64).unwrap().unwrap();
                let reader = bsl_search::Store::open_reader(capture.db_path()).unwrap();
                capture.prepare(&reader, &|| false).unwrap()
            });
            let shared: super::super::SharedSearchEngine =
                crate::state::shared_engine(Some(engine));
            let old = crate::workspace_lease::WorkspaceLease::claim(dir.path());
            let _newer = crate::workspace_lease::WorkspaceLease::claim(dir.path());
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let calls_in_apply = Arc::clone(&calls);
            let empty = std::collections::HashSet::new();
            let prefixes = vec![dir.path().to_path_buf()];

            let outcome = SharedState::apply_workspace_search(
                &shared,
                &crate::state::OwnerStop::default(),
                &old,
                |engine| {
                    calls_in_apply.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    match family {
                        Family::Watcher => engine.enable_workspace_watcher_mode(),
                        Family::Dirty => {
                            engine.mark_workspace_path_dirty(&source).unwrap();
                        }
                        Family::Context => {
                            engine.mark_workspace_context_dirty().unwrap();
                        }
                        Family::Delete => {
                            engine.remove_workspace_path(&source).unwrap();
                        }
                        Family::Subtree => {
                            engine.remove_vanished_under(&prefixes).unwrap();
                        }
                        Family::Reconcile => {
                            engine.reconcile_workspace_files(&empty).unwrap();
                        }
                        Family::Point => {
                            engine.publish_point_refresh(point_batch.as_mut().unwrap()).unwrap();
                        }
                        Family::Roots => {
                            engine
                                .initialize_workspace_roots(
                                    bsl_search::WorkspaceRoots::build(dir.path(), dir.path(), &[])
                                        .0,
                                )
                                .unwrap();
                        }
                        Family::Provider => {
                            engine
                                .replace_published_graph_context_provider(Arc::new(Provider))
                                .unwrap();
                        }
                        Family::ContextRefresh => {
                            engine.refresh_dirty_contexts(&Provider, i64::MAX).unwrap();
                        }
                    }
                    Ok::<_, bsl_search::SearchError>(())
                },
            );
            assert!(
                matches!(outcome, crate::state::WorkspaceSearchApply::Superseded),
                "{family:?}"
            );
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "{family:?}");
            let guard = shared.lock().unwrap();
            let engine = guard.as_ref().unwrap();
            assert_eq!(engine.file_count().unwrap(), 1, "{family:?}");
            let marks = usize::from(matches!(family, Family::Point));
            assert_eq!(engine.workspace_overlay_dirty_paths_snapshot().unwrap().len(), marks);
            assert!(engine.context_dirty_paths("code").unwrap().is_empty());
        }

        let dir = tempdir().unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(
            SearchEngine::fts_only(&dir.path().join("search.db")).unwrap(),
        ));
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let held = lease.hold_file_lock_for_test();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        assert!(matches!(
            SharedState::apply_workspace_search(
                &shared,
                &crate::state::OwnerStop::default(),
                &lease,
                |_| {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            ),
            crate::state::WorkspaceSearchApply::TransientRefusal
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(held);
        assert!(matches!(
            SharedState::apply_workspace_search(
                &shared,
                &crate::state::OwnerStop::default(),
                &lease,
                |_| {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            ),
            crate::state::WorkspaceSearchApply::Applied(())
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let old = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let _newer = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let mut plan = SearchDriftPlan {
            // Non-empty on purpose: an idle plan never reaches the fence, and this
            // asserts what the FENCE does when the lease has been taken over.
            dirty_keys: vec![bsl_search::FileKey::configuration("src/a.bsl")],
            ..Default::default()
        };
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &crate::state::OwnerStop::default(),
                &old,
                &mut plan,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::Superseded
        ));

        let retry_lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let _force_lock = env_lock();
        super::FORCE_DRIFT_APPLY_ERROR_ENGINE
            .store(Arc::as_ptr(&shared) as usize, std::sync::atomic::Ordering::SeqCst);
        let mut failing_plan = SearchDriftPlan {
            dirty_keys: vec![bsl_search::FileKey::configuration("src/a.bsl")],
            ..Default::default()
        };
        let failed = SharedState::apply_prepared_search_drift(
            &shared,
            &crate::state::OwnerStop::default(),
            &retry_lease,
            &mut failing_plan,
            &crate::graph::GraphState::disabled(),
        );
        super::FORCE_DRIFT_APPLY_ERROR_ENGINE.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(failed, crate::state::WorkspaceSearchApply::OperationError(_)));
    }

    #[test]
    fn drift_apply_keeps_its_cursor_on_refusal_and_advances_in_bounded_slices() {
        let dir = tempdir().unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(
            SearchEngine::fts_only(&dir.path().join("search.db")).unwrap(),
        ));
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let mut plan = SearchDriftPlan {
            dirty_keys: (0..=bsl_search::WORKSPACE_APPLY_BATCH_ROWS)
                .map(|index| bsl_search::FileKey::configuration(format!("P{index}.bsl")))
                .collect(),
            ..Default::default()
        };

        let held = lease.hold_file_lock_for_test();
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &crate::state::OwnerStop::default(),
                &lease,
                &mut plan,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::TransientRefusal
        ));
        assert_eq!(plan.dirty_cursor, 0, "a refused fence consumes none of the plan");
        drop(held);

        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &crate::state::OwnerStop::default(),
                &lease,
                &mut plan,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::Applied(false)
        ));
        assert_eq!(plan.dirty_cursor, bsl_search::WORKSPACE_APPLY_BATCH_ROWS);
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &crate::state::OwnerStop::default(),
                &lease,
                &mut plan,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::Applied(true)
        ));
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_dirty_paths_snapshot()
                .unwrap()
                .len(),
            bsl_search::WORKSPACE_APPLY_BATCH_ROWS + 1
        );
    }

    /// The wiring, not the helper. A drain that is written but never called from the sink
    /// leaves every unit test above green and the defect exactly where it was: a change hub
    /// event marks the path, and a read-only search keeps answering with the old text.
    /// The admission budget is a deadline: once it runs out, only work delivered AFTER that
    /// may open the next one. The sink reaches this point having never waited on the hub, so
    /// its generation is still 0 and every batch the hub took while the sink was retrying is
    /// "newer" than that — the one thing that must not count.
    #[test]
    fn a_dormant_sink_waits_for_a_batch_newer_than_the_budget_it_spent() {
        use crate::change_hub::{test_support::eventually, WorkspaceChangeHub};
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let cursor = hub.subscribe();

        // Delivered while the budget was still running, and never acknowledged. One write can
        // reach the watcher as several events, so wait for the hub to go quiet: the test is
        // about a generation the sink was already told about, not about a write still landing.
        fs::write(workspace.join("Early.bsl"), "Процедура Ранняя() КонецПроцедуры\n").unwrap();
        let mut last = hub.generation();
        assert!(eventually(Duration::from_secs(20), || {
            std::thread::sleep(Duration::from_millis(200));
            let now = hub.generation();
            let quiet = now > 0 && now == last;
            last = now;
            quiet
        }));

        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                // What the sink holds when the enable retries never had to wait for anything.
                let mut generation = 0;
                super::wait_for_fresh_batch(
                    &hub,
                    cursor,
                    &mut generation,
                    &crate::state::OwnerStop::default(),
                    &crate::workspace_lease::WorkspaceLease::unmanaged(),
                );
                let _ = tx.send(());
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "the batch from inside the spent budget opened another one"
        );

        fs::write(workspace.join("Late.bsl"), "Процедура Поздняя() КонецПроцедуры\n").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(20)).is_ok(),
            "work delivered after the budget ran out must open the next one"
        );
        waiter.join().unwrap();
        hub.shutdown();
    }

    #[test]
    fn the_sink_drains_the_overlay_after_a_real_hub_batch() {
        use crate::change_hub::{test_support::eventually, WorkspaceChangeHub};
        use std::time::Duration;

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let module = workspace.join("Module.bsl");
        fs::write(&module, "Процедура Предыдущая() Экспорт КонецПроцедуры\n").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        engine.enable_workspace_watcher_mode();
        engine.prime_workspace_overlay().unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let graph = crate::graph::GraphState::disabled();
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let cursor = hub.subscribe();
        let lease = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        let owners = crate::state::OwnerStop::default();
        let backlog = crate::state::overlay_backlog::OverlayBacklog::default();
        assert!(backlog.start(Arc::clone(&shared), lease.clone(), None, owners.clone()));
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&shared),
            graph,
            None,
            Arc::new(AtomicU64::new(0)),
            lease.clone(),
            owners.clone(),
            backlog.clone(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
        ));

        fs::write(&module, "Процедура Новая() Экспорт КонецПроцедуры\n").unwrap();

        let seen = eventually(Duration::from_secs(30), || {
            let Ok(guard) = shared.lock() else { return false };
            let Some(engine) = guard.as_ref() else { return false };
            engine
                .text_search_read_only("Новая", 10, Some("code"))
                .map(|hits| !hits.is_empty())
                .unwrap_or(false)
        });

        lease.release();
        owners.stop();
        backlog.stop();
        assert!(seen, "the sink marked the path dirty and nobody read it back");
    }

    /// #187, замер: a hub that cannot watch answers through its own POLLING fallback, and those
    /// records are the feed — the sink consumes them, marks paths and the backlog applies them.
    /// So SqliteLocal without an event-capable watch is kept fresh by the hub's poll, not by a
    /// background cold scan; there is no silent staleness for a daemon that has a hub.
    #[test]
    fn a_polling_hub_still_keeps_the_search_overlay_fresh() {
        use crate::change_hub::{
            test_support::eventually, PollConfig, WatchTarget, WorkspaceChangeHub,
        };
        use std::time::Duration;

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let module = workspace.join("Module.bsl");
        fs::write(&module, "Процедура Предыдущая() Экспорт КонецПроцедуры\n").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        engine.enable_workspace_watcher_mode();
        engine.initialize_workspace_overlay_clean().unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let graph = crate::graph::GraphState::disabled();
        // The hub refuses every watch, so it never arms: its fallback is the poll below, and
        // the records it makes reach the accumulator like any event batch.
        crate::change_hub::POLL_INSTEAD_OF_WATCHING.with(|poll| {
            poll.set(Some(PollConfig {
                period: Duration::from_millis(50),
                verify_bytes: 1024 * 1024,
            }))
        });
        let hub =
            WorkspaceChangeHub::start_targets(vec![WatchTarget::recursive(workspace.clone())]);
        crate::change_hub::POLL_INSTEAD_OF_WATCHING.with(|poll| poll.set(None));
        assert!(
            !hub.wait_until_watching(Duration::from_secs(5)),
            "the fixture stands on a hub that cannot watch",
        );
        assert!(
            hub.is_polling(),
            "and the hub's fallback is the poll this test measures — not some other feed",
        );
        let lease = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        let owners = crate::state::OwnerStop::default();
        let backlog = crate::state::overlay_backlog::OverlayBacklog::default();
        assert!(backlog.start(Arc::clone(&shared), lease.clone(), None, owners.clone()));
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            hub.subscribe(),
            Arc::clone(&shared),
            graph,
            None,
            Arc::new(AtomicU64::new(0)),
            lease.clone(),
            owners.clone(),
            backlog.clone(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
        ));

        fs::write(&module, "Процедура Новая() Экспорт КонецПроцедуры\n").unwrap();

        let seen = eventually(Duration::from_secs(30), || {
            let Ok(guard) = shared.lock() else { return false };
            let Some(engine) = guard.as_ref() else { return false };
            engine
                .text_search_read_only("Новая", 10, Some("code"))
                .map(|hits| !hits.is_empty())
                .unwrap_or(false)
        });

        lease.release();
        owners.stop();
        backlog.stop();
        assert!(seen, "the polling hub's records never reached the search overlay");
    }

    /// The gate the sink asks before flipping the one-way watcher mode: only a hub that can
    /// actually feed qualifies — a watch that is up or the poll that replaces it. A hub whose
    /// thread never started has neither, and that is exactly the state the mode must not be
    /// flipped in (github#186, github#187).
    #[test]
    fn the_sink_enables_watcher_mode_only_over_a_live_feed() {
        use crate::change_hub::{PollConfig, WatchTarget, WorkspaceChangeHub};
        use std::time::Duration;

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(workspace.join("Module.bsl"), "Процедура П() КонецПроцедуры\n").unwrap();

        // No thread at all: neither a watch nor a poll exists to feed anything.
        let unstartable =
            WorkspaceChangeHub::start_with_unstartable_thread(vec![WatchTarget::recursive(
                workspace.clone(),
            )]);
        assert!(!super::hub_is_feeding(&unstartable), "a hub with no thread feeds nothing");

        // A running watcher feeds.
        let watching = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(watching.wait_until_watching(Duration::from_secs(5)));
        assert!(super::hub_is_feeding(&watching));
        watching.shutdown();

        // A hub that cannot watch polls instead, and that poll is a feed too.
        crate::change_hub::POLL_INSTEAD_OF_WATCHING.with(|poll| {
            poll.set(Some(PollConfig {
                period: Duration::from_millis(50),
                verify_bytes: 1024 * 1024,
            }))
        });
        let polling = WorkspaceChangeHub::start(vec![workspace.clone()]);
        crate::change_hub::POLL_INSTEAD_OF_WATCHING.with(|poll| poll.set(None));
        assert!(!polling.wait_until_watching(Duration::from_secs(5)));
        assert!(polling.is_polling(), "the fallback is the poll this gate accepts");
        assert!(super::hub_is_feeding(&polling));
        polling.shutdown();
    }

    /// A consumer that cannot yet apply what it reads — its first fenced step is refused while
    /// someone else holds the lease lock — says so: it is not watching for the index until
    /// facts reach it. Once the fence lets it through, it is.
    #[test]
    fn a_consumer_blocked_on_its_first_fence_is_not_yet_watching() {
        use crate::change_hub::{test_support::eventually, WorkspaceChangeHub};
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(workspace.join("Module.bsl"), "Процедура П() КонецПроцедуры\n").unwrap();
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let lease = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        let held = lease.hold_file_lock_for_test();
        let owners = crate::state::OwnerStop::default();
        let phase = Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending));
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            hub.subscribe(),
            Arc::clone(&shared),
            crate::graph::GraphState::disabled(),
            None,
            Arc::new(AtomicU64::new(0)),
            lease.clone(),
            owners.clone(),
            crate::state::overlay_backlog::OverlayBacklog::default(),
            Arc::clone(&phase),
        ));
        // While the lock is held the first fenced step cannot pass, whatever the timing.
        assert!(eventually(Duration::from_secs(5), || {
            *phase.lock().unwrap() != crate::state::ConsumerPhase::Pending
        }));
        assert_eq!(
            *phase.lock().unwrap(),
            crate::state::ConsumerPhase::Attaching,
            "a consumer whose facts reach nothing called itself attached"
        );
        drop(held);
        assert!(eventually(Duration::from_secs(10), || {
            *phase.lock().unwrap() == crate::state::ConsumerPhase::Attached
        }));
        owners.stop();
        hub.interrupt_waiters();
        assert!(eventually(Duration::from_secs(5), || {
            *phase.lock().unwrap() == crate::state::ConsumerPhase::Stopped
        }));
    }

    /// A fresh edit is the one delivery of a fact to the embedding driver: the consumer kicks
    /// it, which is what lifts a driver waiting after a failure. The backlog owner only ever
    /// wakes it, so without this kick an edit after a failed pass would never be embedded.
    #[test]
    fn a_fresh_edit_revives_a_failed_embedding_driver() {
        use crate::change_hub::{test_support::eventually, WorkspaceChangeHub};
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let module = workspace.join("Module.bsl");
        fs::write(&module, "Процедура Была() Экспорт КонецПроцедуры\n").unwrap();
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        engine.enable_workspace_watcher_mode();
        engine.initialize_workspace_overlay_clean().unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let retry =
            super::super::overlay_retry::OverlayRetry::unstarted_for_test(Arc::clone(&shared));
        retry.fail_for_test();
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let cursor = hub.subscribe();
        let owners = crate::state::OwnerStop::default();
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&shared),
            crate::graph::GraphState::disabled(),
            Some(Arc::clone(&retry)),
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            owners.clone(),
            crate::state::overlay_backlog::OverlayBacklog::default(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
        ));

        fs::write(&module, "Процедура Стала() Экспорт КонецПроцедуры\n").unwrap();
        let revived = eventually(Duration::from_secs(20), || !retry.is_failed());
        owners.stop();
        hub.interrupt_waiters();
        assert!(revived, "the fresh edit never reached the failed embedding driver");
    }

    /// A module body is the most common edit in a BSL workspace, and the graph holds those
    /// bodies. Without this the sink arms a rebuild only for `.xml`, and — since the request
    /// path stopped walking disk — nothing schedules a catch-up at all.
    #[test]
    fn a_bsl_only_drift_arms_the_graph_rebuild() {
        let dir = tempdir().unwrap();
        let configuration = dir.path().join("cf");
        fs::create_dir_all(&configuration).unwrap();
        let file = configuration.join("Module.bsl");
        fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine
            .initialize_workspace_roots(
                bsl_search::WorkspaceRoots::build(dir.path(), &configuration, &[]).0,
            )
            .unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let shared: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let entry = crate::change_hub::ChangeEntry {
            canonical: file.clone(),
            raw: file,
            kind: crate::change_hub::ChangeKind::MaybeChanged,
            seq: 1,
        };
        let graph = crate::graph::GraphState::disabled();

        let plan = SharedState::prepare_search_drift(
            &shared,
            &crate::state::OwnerStop::default(),
            std::slice::from_ref(&entry),
            false,
            &graph,
        );

        assert!(plan.nudge_rebuild, "a .bsl edit leaves the graph without a catch-up");
    }

    /// The `.xml` branch used to ASSIGN the flag, so a batch carrying both kinds lost the
    /// `.bsl` arming whenever the xml half resolved to nothing.
    #[test]
    fn a_mixed_bsl_and_xml_drift_keeps_the_graph_rebuild_armed() {
        let dir = tempdir().unwrap();
        let configuration = dir.path().join("cf");
        fs::create_dir_all(&configuration).unwrap();
        let module = configuration.join("Module.bsl");
        fs::write(&module, "Процедура П()\nКонецПроцедуры").unwrap();
        // A descriptor with no module subtree beside it and no readers in the graph: the xml
        // half contributes neither `mark_whole` nor a context path.
        let descriptor = configuration.join("Форма.xml");
        fs::write(&descriptor, "<Form/>").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine
            .initialize_workspace_roots(
                bsl_search::WorkspaceRoots::build(dir.path(), &configuration, &[]).0,
            )
            .unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let shared: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let entry = |path: std::path::PathBuf| crate::change_hub::ChangeEntry {
            canonical: path.clone(),
            raw: path,
            kind: crate::change_hub::ChangeKind::MaybeChanged,
            seq: 1,
        };
        let graph = crate::graph::GraphState::disabled();

        let plan = SharedState::prepare_search_drift(
            &shared,
            &crate::state::OwnerStop::default(),
            &[entry(module), entry(descriptor)],
            false,
            &graph,
        );

        assert!(plan.nudge_rebuild, "the xml branch overwrote the .bsl arming");
    }

    /// The rescan-debt wait is a real consumer of a reconcile: it takes the batch, tells the
    /// graph the project must be re-read, and acknowledges. Told as a bare fact, the loss loses
    /// the identity that makes it ONE event, and the watcher's delivery of the same window buys
    /// the graph a second project reload for it.
    ///
    /// Native: the search sink, its rescan debt earned by real durable apply failures, the hub,
    /// the graph's own builds answering the demands, and the watcher thread that inherits the
    /// open window. What the duplicate would buy is read off the ledger — which no build can
    /// change — and off the builds the graph then starts.
    #[test]
    fn a_reconcile_taken_while_the_search_rescan_waits_keeps_its_identity() {
        use crate::change_hub::{test_support::eventually, WorkspaceChangeHub};
        use std::time::Duration;

        struct ResetForcedError;
        impl Drop for ResetForcedError {
            fn drop(&mut self) {
                super::FORCE_DRIFT_APPLY_ERROR_ENGINE.store(0, std::sync::atomic::Ordering::SeqCst);
                super::FORCE_RESCAN_DEBT_WAITING.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _env_lock = env_lock();
        let _reset = ResetForcedError;

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        crate::graph::test_support::sample_workspace(&workspace);
        // The index lives outside the watched tree: its own writes are changes like any other,
        // and a stand that waits for the workspace to go quiet would be waiting on itself.
        let db = tempdir().unwrap();
        let mut engine = SearchEngine::fts_only(&db.path().join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        // The graph's derived database lives outside the watched tree too, for the same reason:
        // its writes are events, and a build answering them would keep making more.
        let cache = tempdir().unwrap();
        let graph = crate::graph::GraphState::for_workspace_with_cache(
            workspace.clone(),
            crate::cache::WorkspaceCacheLayout::from_root(cache.path().to_path_buf()),
        )
        .with_change_hub(hub.clone());
        let cursor = hub.subscribe();
        // Keeps the window open after the search has settled its own debt, so the watcher that
        // starts later joins it and is handed the same loss — the hub's own contract.
        let holder = hub.subscribe();
        let lease = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        let sink_stop = crate::state::OwnerStop::default();
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&shared),
            graph.clone(),
            None,
            Arc::new(AtomicU64::new(0)),
            lease.clone(),
            sink_stop.clone(),
            crate::state::overlay_backlog::OverlayBacklog::default(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
        ));
        graph.ensure_loading();
        assert!(
            eventually(Duration::from_secs(60), || matches!(
                graph.status(),
                crate::graph::GraphStatus::Ready { .. }
            )),
            "the graph never finished its first build",
        );

        let branch = || super::RESCAN_DEBT_WAIT_BRANCH.load(std::sync::atomic::Ordering::SeqCst);
        let settle = |name: &str, text: &str| {
            let before = hub.seq();
            fs::write(workspace.join(name), text).unwrap();
            assert!(
                eventually(Duration::from_secs(30), || hub.seq() > before),
                "the hub never saw {name}",
            );
            assert!(
                eventually(Duration::from_secs(30), || {
                    let batch = hub.materialize(cursor);
                    batch.entries.is_empty() && !batch.rescan_required
                }),
                "the search consumer never finished with {name}",
            );
        };
        let answered = |what: &str| {
            let done = eventually(Duration::from_secs(60), || graph.owes_forced().is_none());
            if !done {
                eprintln!(
                    "DIAG {what}: status={:?} forced={:?} change={:?} failed={} marks={} in_flight={} branch={}",
                    graph.status(),
                    graph.owes_forced(),
                    graph.owes_change(),
                    graph.owes_failed(),
                    graph.owes_marks(),
                    graph.build_in_flight(),
                    super::RESCAN_DEBT_WAIT_BRANCH.load(std::sync::atomic::Ordering::SeqCst),
                );
            }
            assert!(done, "{what}: the graph never answered the demand");
        };

        // Two durable apply failures, so the consumer earns a rescan debt of its own, and that
        // debt is then held in its wait for the rest of the stand.
        super::FORCE_DRIFT_APPLY_ERROR_ENGINE
            .store(Arc::as_ptr(&shared) as usize, std::sync::atomic::Ordering::SeqCst);
        for (index, name) in ["Один.bsl", "Два.bsl"].iter().enumerate() {
            settle(name, &format!("Процедура П{index}() КонецПроцедуры\n"));
        }
        super::FORCE_RESCAN_DEBT_WAITING.store(true, std::sync::atomic::Ordering::SeqCst);

        // An ordinary fact taken by the wait is still a demand on the graph: the wait may not
        // swallow what it cannot apply itself. Measured by the work the graph then does, from a
        // graph that has gone quiet — a demand answered before this thread looked was still made.
        answered("the failures that earned the wait");
        assert!(
            eventually(Duration::from_secs(60), || !graph.build_in_flight()),
            "the stand needs the graph quiet before the fact it must act on",
        );
        let before_plain = branch();
        // Everything the hub already holds stands below this line, so the fact the wait is about
        // to condense stands above it — and only a forced build that observed THAT fact can
        // carry the graph's answer past it. A build running for anything else cannot.
        let below_the_fact = hub.seq();
        settle("Шесть.bsl", "Процедура П6() КонецПроцедуры\n");
        assert!(branch() > before_plain, "the stand needs the rescan debt waiting");
        let acted_on_it =
            eventually(Duration::from_secs(60), || graph.answered_forced() > below_the_fact);
        if !acted_on_it {
            eprintln!(
                "DIAG plain: status={:?} forced={:?} answered_forced={} below={} change={:?} failed={} marks={} in_flight={} branch={} seq={}",
                graph.status(),
                graph.owes_forced(),
                graph.answered_forced(),
                below_the_fact,
                graph.owes_change(),
                graph.owes_failed(),
                graph.owes_marks(),
                graph.build_in_flight(),
                branch(),
                hub.seq(),
            );
        }
        assert!(acted_on_it, "a fact taken by the wait told the graph nothing");
        answered("the fact the wait condensed");

        // The reconcile, taken by that same wait: the consumer's own cursor stops owing it.
        let acted_before = graph.acted_losses();
        let before_reconcile = branch();
        hub.deliver_backend_error_for_test();
        assert!(
            eventually(Duration::from_secs(60), || branch() > before_reconcile
                && !hub.drain_peek(cursor)),
            "the wait never took the reconcile",
        );
        // The window's identity, read from the cursor that still owes it — the same number the
        // watcher must be handed, and the one the ledger must already have.
        let owed = hub.materialize(holder);
        let window = owed.loss_token().expect("the stand needs the window still open");
        let acted_after_search = graph.acted_losses();
        answered("the reconcile the wait took");

        let watcher_stop = crate::state::OwnerStop::default();
        assert!(crate::graph::watcher::start(&graph, &hub, None, watcher_stop.clone()));
        // The watcher's OWN cursor, not a count of cursors: the graph keeps one of its own for
        // fingerprint comparisons, and search and this stand hold two more.
        let watcher_cursor = {
            let mut found = None;
            assert!(
                eventually(Duration::from_secs(30), || {
                    found = graph.watch_state().1;
                    found.is_some()
                }),
                "the watcher never took a cursor of its own",
            );
            found.expect("the watcher's cursor")
        };
        // Evidence of inheritance while there is still something to read: until the watcher
        // drains, a batch of its cursor names the window itself. Once it has drained, there is
        // nothing to peek at and this observation is simply absent — what says the same thing
        // then is the completed delivery below, which names the token.
        let inherited = hub.materialize(watcher_cursor);
        let peeked_before_the_drain =
            inherited.rescan_required.then(|| inherited.loss_token()).flatten();
        let carried = hub.drain(holder);
        // Waited on the DELIVERY of that exact token, which is listed only once the ledger has
        // it: a wait that ended earlier would read the ledger before the delivery reached it.
        assert!(
            eventually(Duration::from_secs(30), || graph
                .quiet_loss_deliveries()
                .contains(&Some(window))),
            "the watcher never delivered the window it inherited",
        );
        let acted_after_watcher = graph.acted_losses();

        // A genuinely new window is news, and it is THAT window that must be accepted: a build
        // running for something else, or the loss already acted on, proves nothing here.
        hub.deliver_backend_error_for_test();
        let fresh = hub.materialize(holder).loss_token().expect("a second window is a loss");
        let new_window_accepted =
            eventually(Duration::from_secs(60), || graph.acted_losses().contains(&fresh));

        watcher_stop.stop();
        sink_stop.stop();
        hub.shutdown();
        lease.release();

        assert!(owed.rescan_required, "the stand needs the window still open for the watcher");
        assert_eq!(carried.loss_token(), Some(window), "the stand needs one window, one identity");
        assert!(acted_before.is_empty(), "the stand starts with no loss acted on");
        assert_eq!(
            acted_after_search,
            vec![window],
            "the wait acted on the reconcile without recording its identity",
        );
        if let Some(peeked) = peeked_before_the_drain {
            assert_eq!(
                peeked, window,
                "the watcher was handed a different loss than the window it joined",
            );
        }
        assert_eq!(
            acted_after_watcher,
            vec![window],
            "the same window, delivered again through the watcher, was acted on twice",
        );
        assert_ne!(fresh, window, "the stand needs a second window of its own");
        assert!(new_window_accepted, "a new window after the reconcile was never acted on");
    }

    #[test]
    fn durable_drift_error_advances_cursor_and_coalesces_debt() {
        use crate::change_hub::{test_support::eventually, WorkspaceChangeHub};
        use std::time::Duration;

        struct ResetForcedError;
        impl Drop for ResetForcedError {
            fn drop(&mut self) {
                super::FORCE_DRIFT_APPLY_ERROR_ENGINE.store(0, std::sync::atomic::Ordering::SeqCst);
                super::FORCE_RESCAN_DEBT_DUE.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let _env_lock = env_lock();
        let _reset = ResetForcedError;
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let a = workspace.join("A.bsl");
        fs::write(&a, "Procedure Old()\nEndProcedure").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        let shared: super::super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let cursor = hub.subscribe();
        let lease = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&shared),
            graph.clone(),
            None,
            Arc::new(AtomicU64::new(0)),
            lease.clone(),
            crate::state::OwnerStop::default(),
            crate::state::overlay_backlog::OverlayBacklog::default(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
        ));

        // An independent cursor, read by nobody else, so the wait below can tell "the hub
        // delivered THIS path" from "some event went past". `events_seen` cannot: it counts
        // every raw event, including the ones the scope filter drops (the search db writes
        // into this very tree), and it is bumped before the path is recorded — so it rises
        // for changes that are not the one just written and are not drainable yet.
        let probe = hub.subscribe();
        // One change on disk, waited out end to end: the hub records it, then the sink is done
        // with it. The change is passed in rather than assumed to be a write, because a removal
        // has to be waited out for the same reason — see the floor below.
        let settle = |path: &std::path::Path, change: &dyn Fn()| {
            // The accumulator keeps ONE record per canonical path and stamps a fresh `seq` on
            // every event folded into it, so "an entry for this path" is not "this change was
            // delivered": a removal and the re-write that follows collapse into one record
            // under one path. Everything the hub already holds is therefore drained first and
            // its highest `seq` kept as the floor — only a record above it can be this
            // change's. The floor is only exact because every change gets its own `settle`: an
            // unwaited one would still be in flight here and land above the floor it should
            // have set.
            let floor = hub.drain(probe).entries.iter().map(|entry| entry.seq).max().unwrap_or(0);
            change();
            // Canonicalised through the PARENT: a removal leaves no file to resolve, and both
            // kinds of change must be matched against the spelling the watcher reports.
            let target = path
                .parent()
                .expect("a file in a directory")
                .canonicalize()
                .expect("the directory the change happened in")
                .join(path.file_name().expect("a file name"));
            let mut delivered = false;
            let mut seen: Vec<(std::path::PathBuf, u64)> = Vec::new();
            let arrived = eventually(Duration::from_secs(15), || {
                let batch = hub.drain(probe);
                seen.extend(batch.entries.iter().map(|entry| (entry.canonical.clone(), entry.seq)));
                delivered =
                    delivered || seen.iter().any(|(path, seq)| *path == target && *seq > floor);
                delivered
            });
            assert!(arrived, "the hub delivered {target:?} above seq {floor}; it saw {seen:?}");
            // Only now does the delivery's absence from the sink view mean the sink is DONE with
            // it: it acknowledges after nudging, so the entry stands in its batch until then.
            // The view as a whole cannot be the signal: the sink's own lease restamps and graph
            // cache writes land inside this tree, and while the forced due debt keeps it
            // rescanning with no backoff every pass feeds the next one, so a view with nothing
            // in it is a gap the loop may never leave open.
            assert!(
                eventually(Duration::from_secs(15), || {
                    hub.materialize(cursor).entries.iter().all(|entry| entry.canonical != target)
                }),
                "the sink never acknowledged {target:?}"
            );
        };
        let write_and_wait =
            |path: &std::path::Path, text: &str| settle(path, &|| fs::write(path, text).unwrap());

        super::FORCE_DRIFT_APPLY_ERROR_ENGINE
            .store(Arc::as_ptr(&shared) as usize, std::sync::atomic::Ordering::SeqCst);
        write_and_wait(&workspace.join("Configuration.xml"), "<Configuration/>");
        assert_ne!(
            graph.status(),
            crate::graph::GraphStatus::Idle,
            "the graph nudge survives a search OperationError"
        );
        write_and_wait(&workspace.join("B.bsl"), "Procedure B()\nEndProcedure");
        // Waited out like any other change: leaving it in flight would put its record above
        // the floor the next call takes, and that call would read it as its own delivery.
        settle(&a, &|| fs::remove_file(&a).unwrap());
        write_and_wait(&a, "Procedure New()\nEndProcedure");
        assert!(
            shared
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_dirty_paths_snapshot()
                .unwrap()
                .is_empty(),
            "durable errors advance without pretending their writes applied"
        );

        super::FORCE_RESCAN_DEBT_DUE.store(true, std::sync::atomic::Ordering::SeqCst);
        write_and_wait(&workspace.join("C.bsl"), "Procedure C()\nEndProcedure");
        assert!(
            shared
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_dirty_paths_snapshot()
                .unwrap()
                .is_empty(),
            "an OperationError during the due full rescan keeps one debt slot without partial apply"
        );
        super::FORCE_DRIFT_APPLY_ERROR_ENGINE.store(0, std::sync::atomic::Ordering::SeqCst);
        write_and_wait(&workspace.join("C.bsl"), "Procedure C2()\nEndProcedure");
        assert!(
            eventually(Duration::from_secs(15), || {
                let recovered = shared
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .workspace_overlay_dirty_paths_snapshot()
                    .unwrap();
                ["A.bsl", "B.bsl", "C.bsl"]
                    .into_iter()
                    .all(|path| recovered.contains_key(&bsl_search::FileKey::configuration(path)))
            }),
            "the one recovery rescan converges to the final disk state"
        );

        lease.release();
        hub.shutdown();
        shared.lock().unwrap().as_ref().unwrap().initialize_workspace_overlay_clean().unwrap();
        let next_hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(next_hub.wait_until_watching(Duration::from_secs(5)));
        let next_cursor = next_hub.subscribe();
        let next_lease = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        assert!(SharedState::spawn_search_sink(
            next_hub.clone(),
            next_cursor,
            Arc::clone(&shared),
            graph,
            None,
            Arc::new(AtomicU64::new(0)),
            next_lease.clone(),
            crate::state::OwnerStop::default(),
            crate::state::overlay_backlog::OverlayBacklog::default(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
        ));
        // No backlog owner runs here, so the marks the consumer hands over stay where it put
        // them: what the rescan slot is judged by is exactly the set of marks.
        let marked = || -> std::collections::HashSet<bsl_search::FileKey> {
            shared
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_dirty_paths_snapshot()
                .unwrap()
                .into_keys()
                .collect()
        };
        let before = next_hub.events_seen();
        fs::write(workspace.join("D.bsl"), "Procedure D()\nEndProcedure").unwrap();
        assert!(eventually(Duration::from_secs(5), || next_hub.events_seen() > before));
        assert!(
            eventually(Duration::from_secs(15), || {
                marked().contains(&bsl_search::FileKey::configuration("D.bsl"))
            }),
            "the change that follows the recovery reaches the overlay"
        );
        assert_eq!(
            marked(),
            [bsl_search::FileKey::configuration("D.bsl")].into_iter().collect(),
            "multiple failures coalesce into one rescan slot; the next change stays incremental"
        );

        next_lease.release();
        next_hub.shutdown();
    }

    #[test]
    fn root_transition_epoch_ignores_unrelated_files_and_tracks_keyspace_drift() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let graph = crate::graph::GraphState::for_workspace(root.clone());
        let entry = |path: std::path::PathBuf, kind| crate::change_hub::ChangeEntry {
            canonical: path.clone(),
            raw: path,
            kind,
            seq: 1,
        };

        assert!(!SharedState::root_transition_relevant_drift(
            &[entry(root.join("notes.txt"), crate::change_hub::ChangeKind::MaybeChanged)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("notes.txt"), crate::change_hub::ChangeKind::MaybeRemoved)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("Module.bsl"), crate::change_hub::ChangeKind::MaybeChanged)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("Configuration.xml"), crate::change_hub::ChangeKind::MaybeChanged,)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("Sub.v1"), crate::change_hub::ChangeKind::MaybeRemoved)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("bsl-analyzer.toml"), crate::change_hub::ChangeKind::MaybeChanged,)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("gone"), crate::change_hub::ChangeKind::SubtreeRemoved)],
            false,
            &graph,
        ));
    }

    #[test]
    fn drift_keys_are_replanned_after_workspace_roots_change() {
        let dir = tempdir().unwrap();
        let configuration = dir.path().join("cf");
        let extension = dir.path().join("ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let file = extension.join("Module.bsl");
        fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine
            .initialize_workspace_roots(
                bsl_search::WorkspaceRoots::build(dir.path(), &configuration, &[]).0,
            )
            .unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let shared: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let entry = crate::change_hub::ChangeEntry {
            canonical: file.clone(),
            raw: file.clone(),
            kind: crate::change_hub::ChangeKind::MaybeChanged,
            seq: 1,
        };
        let graph = crate::graph::GraphState::disabled();
        let mut plan = SharedState::prepare_search_drift(
            &shared,
            &crate::state::OwnerStop::default(),
            std::slice::from_ref(&entry),
            false,
            &graph,
        );

        let expected_key = {
            let mut guard = shared.lock().unwrap();
            let engine = guard.as_mut().unwrap();
            engine.set_workspace_roots(
                bsl_search::WorkspaceRoots::build(
                    dir.path(),
                    &configuration,
                    std::slice::from_ref(&extension),
                )
                .0,
            );
            engine.workspace_file_key(&file).unwrap()
        };

        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &crate::state::OwnerStop::default(),
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                &mut plan,
                &graph,
            ),
            crate::state::WorkspaceSearchApply::OperationError(_)
        ));
        let mut replanned = SharedState::prepare_search_drift(
            &shared,
            &crate::state::OwnerStop::default(),
            std::slice::from_ref(&entry),
            false,
            &graph,
        );
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &crate::state::OwnerStop::default(),
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                &mut replanned,
                &graph,
            ),
            crate::state::WorkspaceSearchApply::Applied(true)
        ));
        assert!(shared
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_overlay_dirty_paths_snapshot()
            .unwrap()
            .contains_key(&expected_key));
    }

    /// Entering the event stream costs the overlay nothing. The window a reconcile used to
    /// pay for is not a window any more: the baseline is taken after the watch is up and the
    /// cursor is older than both, so there is nothing for a rescan to recover — and a rescan
    /// is not cheap. It re-walks every root, canonicalizes and stats every file, and marks
    /// them all dirty, which a later refresh pays for by reading each one off disk in full.
    #[test]
    fn a_boot_entering_event_mode_does_not_rescan() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        // Written BEFORE the watch arms, so no event ever reports it: whatever ends up in the
        // dirty set got there from a rescan and from nothing else.
        fs::write(workspace.join("Module.bsl"), "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let (hub, hold) =
            WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(workspace.clone())]);
        let cursor = hub.subscribe();
        hold.release();
        assert!(hub.wait_until_watching(Duration::from_secs(5)), "the watch must arm");
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&engine_arc),
            crate::graph::GraphState::disabled(),
            None,
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            crate::state::OwnerStop::default(),
            crate::state::overlay_backlog::OverlayBacklog::default(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
        ));
        std::thread::sleep(Duration::from_millis(500));

        let snapshot = {
            let guard = engine_arc.lock().unwrap();
            guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
        };
        assert!(
            snapshot.is_empty(),
            "a start that observed nothing must cost the overlay nothing: {snapshot:?}",
        );
    }

    /// The cursor is subscribed before the boot reads disk and long before there is an engine
    /// to feed, so changes landing in between are not lost — they wait in the accumulator
    /// until a sink exists to drain them. The sink applies everything to a published engine
    /// or it does not exist: a drain into an absent engine no-ops path by path and the batch
    /// is gone for good.
    #[test]
    fn an_event_before_the_engine_exists_still_reaches_the_overlay() {
        use crate::change_hub::WorkspaceChangeHub;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)), "the watch must arm");
        let cursor = hub.subscribe();

        // Happens while the boot would still be reading disk: no engine yet, and the only
        // record of it is the cursor's own backlog.
        fs::write(workspace.join("Module.bsl"), "Процедура П()\nКонецПроцедуры").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while hub.events_seen() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(hub.events_seen() > 0, "the hub observed the write");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&engine_arc),
            crate::graph::GraphState::disabled(),
            None,
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            crate::state::OwnerStop::default(),
            crate::state::overlay_backlog::OverlayBacklog::default(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
        ));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut delivered = false;
        while Instant::now() < deadline {
            let snapshot = {
                let guard = engine_arc.lock().unwrap();
                guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
            };
            if snapshot.keys().any(|key| key.path.ends_with("Module.bsl")) {
                delivered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(delivered, "a change that predates the engine is still the overlay's to apply");
    }

    /// A workspace whose initial walk outlasts one slice of patience is an ordinary large
    /// configuration, not a failure — and a boot that gave up on it would leave the search
    /// overlay on its boot baseline until the hub arms or falls back to its own poll
    /// (github#187).
    #[test]
    fn the_boot_keeps_waiting_while_the_hub_is_still_starting() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        let releaser = {
            let hold = hold.shared();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                hold.release();
            })
        };
        let started = Instant::now();
        let armed = SharedState::await_watch(
            &hub,
            &crate::state::OwnerStop::default(),
            super::WatchWaitPolicy::new(Duration::from_millis(5), Duration::from_secs(30)),
        );
        let waited = started.elapsed();
        releaser.join().unwrap();

        assert!(armed, "a hub that is merely slow to start must not be given up on");
        assert!(
            waited >= Duration::from_millis(100),
            "the wait has to have crossed several expired slices to prove it resumed: {waited:?}"
        );
    }

    /// The budget is what bounds the wait — not a count of slices, which is what a naive
    /// "try twice" implementation would bound it by and which no test of mere termination
    /// can tell apart. Measured with a slice far shorter than the budget, so an
    /// implementation stopping after any small number of slices returns far too early.
    #[test]
    fn the_boot_gives_up_on_the_budget_and_not_before() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        let started = Instant::now();
        let armed = SharedState::await_watch(
            &hub,
            &crate::state::OwnerStop::default(),
            super::WatchWaitPolicy::new(Duration::from_millis(5), Duration::from_millis(400)),
        );
        let waited = started.elapsed();
        hold.release();

        assert!(!armed, "a hub that never arms must not hold the thread for ever");
        assert!(
            waited >= Duration::from_millis(400),
            "giving up before the budget abandons a workspace that was merely slow: {waited:?}"
        );
        assert!(waited < Duration::from_secs(10), "and it must give up: {waited:?}");
    }

    /// The budget is a ceiling on the whole wait, so the last slice is cut to whatever is
    /// left of it. Asked for a full slice at the very end of one, the hub answers a slice
    /// past the deadline the caller was promised — a minute, at the production slice, which
    /// reads exactly like a hub that is still arming.
    #[test]
    fn the_wait_never_overshoots_its_budget_by_a_slice() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let (hub, _hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        let started = Instant::now();
        let armed = SharedState::await_watch(
            &hub,
            &crate::state::OwnerStop::default(),
            super::WatchWaitPolicy::new(Duration::from_millis(400), Duration::from_millis(20)),
        );
        let waited = started.elapsed();

        assert!(!armed, "the hub is held short of arming");
        assert!(
            waited < Duration::from_millis(200),
            "a budget of 20ms must not be spent as a 400ms slice: {waited:?}"
        );
    }

    /// A permanent failure is answered at once. Waiting out a ten-minute budget over a hub
    /// that has already said it will never arm only delays a boot that has to happen anyway.
    #[test]
    fn the_boot_does_not_wait_out_a_permanent_failure() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start_with_unstartable_thread(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        let started = Instant::now();
        assert!(!SharedState::await_watch(
            &hub,
            &crate::state::OwnerStop::default(),
            super::WatchWaitPolicy::new(Duration::from_millis(50), Duration::from_secs(600))
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a permanent failure is not something to spend a budget on"
        );
    }

    #[test]
    fn search_sink_marks_only_bsl_paths_dirty() {
        use crate::change_hub::WorkspaceChangeHub;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)), "the watch must arm");
        // A second cursor observes the raw accumulator independently of the sink.
        let observer = hub.subscribe();

        // Subscribed by the caller, as the boot does: the sink is handed a cursor that
        // already covers everything from here on.
        let cursor = hub.subscribe();
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&engine_arc),
            crate::graph::GraphState::disabled(),
            None,
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            crate::state::OwnerStop::default(),
            crate::state::overlay_backlog::OverlayBacklog::default(),
            Arc::new(Mutex::new(crate::state::ConsumerPhase::Pending)),
        ));

        let bsl = workspace.join("Module.bsl");
        std::fs::write(&bsl, "Процедура П()\nКонецПроцедуры").unwrap();
        let xml = workspace.join("Configuration.xml");
        std::fs::write(&xml, "<Configuration/>").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut dirty_has_bsl = false;
        while Instant::now() < deadline {
            let snapshot = {
                let guard = engine_arc.lock().unwrap();
                guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
            };
            if snapshot.keys().any(|key| key.path.ends_with("Module.bsl")) {
                dirty_has_bsl = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(dirty_has_bsl, "the .bsl change is marked dirty for the search overlay");

        let snapshot = {
            let guard = engine_arc.lock().unwrap();
            guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
        };
        assert!(
            !snapshot.keys().any(|key| key.path.ends_with("Configuration.xml")),
            "search ignores non-.bsl paths",
        );
        let watcher_mode = {
            let guard = engine_arc.lock().unwrap();
            guard.as_ref().unwrap().workspace_overlay_stats().unwrap().unwrap().watcher_mode
        };
        assert!(watcher_mode, "a running sink is what puts the overlay into watcher mode");

        // The hub itself accepted the .xml change; only the consumer filtered it.
        // The event is asynchronous, so poll the observer cursor until it lands.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observer = observer;
        let mut saw_xml = false;
        while Instant::now() < deadline {
            let batch = hub.drain(observer);
            observer = batch.cursor;
            if batch.entries.iter().any(|e| e.raw.ends_with("Configuration.xml")) {
                saw_xml = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(saw_xml, "the accumulator carries the .xml change for other consumers");
    }

    /// On a hub overflow the exact changed paths are lost, so the sink re-walks the
    /// workspace and marks every `.bsl` dirty (and nothing else), restoring the
    /// old unbounded watcher's guarantee that no `.bsl` change is dropped.
    #[test]
    fn search_sink_rewalks_all_bsl_on_overflow() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        // Watcher mode makes `mark_workspace_path_dirty` record into the dirty set.
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        // A nested tree of `.bsl` plus a non-`.bsl` file that must NOT be marked.
        let nested = workspace.join("CommonModules").join("Модуль");
        fs::create_dir_all(&nested).unwrap();
        let a = workspace.join("A.bsl");
        let b = nested.join("B.bsl");
        fs::write(&a, "Процедура П()\nКонецПроцедуры").unwrap();
        fs::write(&b, "Процедура П()\nКонецПроцедуры").unwrap();
        fs::write(workspace.join("Configuration.xml"), "<Configuration/>").unwrap();

        SharedState::rewalk_workspace_bsl_dirty(&engine_arc, &crate::state::OwnerStop::default());

        let snapshot = {
            let guard = engine_arc.lock().unwrap();
            guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
        };
        assert!(snapshot.keys().any(|key| key.path.ends_with("A.bsl")), "top-level .bsl re-marked");
        assert!(snapshot.keys().any(|key| key.path.ends_with("B.bsl")), "nested .bsl re-marked");
        assert!(
            !snapshot.keys().any(|key| key.path.ends_with("Configuration.xml")),
            "non-.bsl paths are left alone",
        );
    }

    #[test]
    fn an_exclusion_only_root_table_update_narrows_the_resident_search_rewalk() {
        // Reads the process-global `FORCE_REWALK_WALK_ERROR` seam; serialize against the
        // tests that toggle it.
        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let hidden = configuration.join("generated");
        fs::create_dir_all(&hidden).unwrap();
        let visible_file = configuration.join("Visible.bsl");
        let hidden_file = hidden.join("Hidden.bsl");
        fs::write(&visible_file, "Процедура Видимая()\nКонецПроцедуры").unwrap();
        fs::write(&hidden_file, "Процедура Скрытая()\nКонецПроцедуры").unwrap();

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(&workspace, &configuration, &[]);
        engine.set_workspace_roots(roots.clone());
        engine.index_directory_fts(&configuration).unwrap();
        assert_eq!(engine.text_search("Видимая", 10, Some("code")).unwrap().len(), 1);
        assert_eq!(engine.text_search("Скрытая", 10, Some("code")).unwrap().len(), 1);
        let shared: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let stop = crate::state::OwnerStop::default();

        let mut before = SearchDriftPlan::default();
        SharedState::prepare_search_rewalk(&shared, &stop, &mut before);
        assert!(before.rewalk_paths.contains(&hidden_file), "fixture never entered the corpus");

        {
            let mut guard = shared.lock().unwrap();
            guard.as_mut().unwrap().set_workspace_roots(
                roots
                    .clone()
                    .with_user_excluded(&project_model::ExcludedPaths::new([hidden.clone()])),
            );
        }
        let mut after = SearchDriftPlan::default();
        SharedState::prepare_search_rewalk(&shared, &stop, &mut after);

        assert!(after.rewalk_paths.contains(&visible_file));
        assert!(!after.rewalk_paths.contains(&hidden_file));
        let present = after.reconcile_present.expect("a complete rewalk is authoritative");
        assert!(present.contains(&visible_file));
        assert!(
            !present.contains(&hidden_file),
            "the resident reconcile would keep a source excluded by the new root table"
        );

        SharedState::apply_search_drift(
            &shared,
            &stop,
            &[],
            true,
            &crate::graph::GraphState::disabled(),
        );
        {
            let guard = shared.lock().unwrap();
            let engine = guard.as_ref().unwrap();
            assert_eq!(engine.text_search("Видимая", 10, Some("code")).unwrap().len(), 1);
            assert!(
                engine.text_search("Скрытая", 10, Some("code")).unwrap().is_empty(),
                "the applied resident reconcile kept an excluded search result"
            );
            assert!(
                engine
                    .load_indexed_documents(Some("code"))
                    .unwrap()
                    .iter()
                    .all(|document| !document.path.ends_with("Hidden.bsl")),
                "the applied resident reconcile left the excluded row in the published index"
            );
        }

        {
            let mut guard = shared.lock().unwrap();
            guard.as_mut().unwrap().set_workspace_roots(roots);
        }
        SharedState::apply_search_drift(
            &shared,
            &stop,
            &[],
            true,
            &crate::graph::GraphState::disabled(),
        );
        let guard = shared.lock().unwrap();
        assert_eq!(
            guard.as_ref().unwrap().text_search("Скрытая", 10, Some("code")).unwrap().len(),
            1
        );
    }

    /// The rescan walk feeds `reconcile_workspace_files`, which deletes every stored key it
    /// does not find on disk. So a walk narrower than the engine's root table is not merely
    /// incomplete — it is destructive: the first hub overflow would wipe every extension's rows
    /// while the files sit untouched on disk. The walk must therefore cover the SAME roots the
    /// table knows, and both halves are checked: the extension's file gets marked (the walk
    /// reached it) and its row survives (the reconcile did not disown it).
    #[test]
    fn an_overflow_rescan_covers_every_registered_root() {
        let dir = tempdir().unwrap();
        // The extension lives OUTSIDE the workspace directory: a walk that quietly used the
        // workspace instead of the root table would still cover an extension nested inside it,
        // and the check would pass while covering nothing it claims to.
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = dir.path().join("outside-ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        fs::write(configuration.join("A.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(extension.join("B.bsl"), "Процедура Вторая()\nКонецПроцедуры").unwrap();

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let (roots, _rejected) = bsl_search::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        );
        // A root outside the workspace is identified by its absolute spelling, so the expected
        // key is read from the table rather than spelled out here.
        let extension_key = roots
            .root_of(&extension.join("B.bsl"), &extension.join("B.bsl").canonicalize().unwrap())
            .expect("the extension's file has an owner");
        engine.set_workspace_roots(roots);
        engine.enable_workspace_watcher_mode();
        // Seed both rows directly: the boot indexers cannot write an extension's row yet, and
        // this test is about the WALK, not about who wrote the row.
        engine.store().upsert_file("", "A.bsl", b"hash-a", "code").unwrap();
        engine
            .store()
            .upsert_file(&extension_key.root_id, &extension_key.path, b"hash-b", "code")
            .unwrap();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        SharedState::rewalk_workspace_bsl_dirty(&engine_arc, &crate::state::OwnerStop::default());

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let snapshot = engine.workspace_overlay_dirty_paths_snapshot().unwrap();
        assert!(
            snapshot.keys().any(|key| *key == extension_key),
            "the rescan walk reaches the extension's file: {snapshot:?}",
        );
        let stored: Vec<String> = engine
            .store()
            .all_files_in_collection("code")
            .unwrap()
            .into_iter()
            .map(|(key, _hash)| format!("{}:{}", key.root_id, key.path))
            .collect();
        assert!(
            stored
                .iter()
                .any(|row| *row == format!("{}:{}", extension_key.root_id, extension_key.path)),
            "the reconcile keeps the extension's row: {stored:?}",
        );
        assert!(stored.iter().any(|row| row == ":A.bsl"), "and the configuration's: {stored:?}");
    }

    /// The walk reads the engine's root table at each call rather than a set captured when the
    /// sink started. A captured copy would keep walking yesterday's roots for the daemon's whole
    /// life, and — because the reconcile deletes stored keys the walk did not find — would erase
    /// any root added to the table afterwards.
    #[test]
    fn the_rescan_walk_follows_the_table_rather_than_a_captured_root() {
        let dir = tempdir().unwrap();
        // The extension lives OUTSIDE the workspace directory: a walk that quietly used the
        // workspace instead of the root table would still cover an extension nested inside it,
        // and the check would pass while covering nothing it claims to.
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = dir.path().join("outside-ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        fs::write(configuration.join("A.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(extension.join("B.bsl"), "Процедура Вторая()\nКонецПроцедуры").unwrap();

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let (configuration_only, _) =
            bsl_search::WorkspaceRoots::build(&workspace, &configuration, &[]);
        engine.set_workspace_roots(configuration_only);
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        SharedState::rewalk_workspace_bsl_dirty(&engine_arc, &crate::state::OwnerStop::default());
        {
            let guard = engine_arc.lock().unwrap();
            let snapshot =
                guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap();
            assert!(
                !snapshot.keys().any(|key| key.path.ends_with("B.bsl")),
                "the undeclared tree is outside the walk while the table says so",
            );
        }

        {
            let mut guard = engine_arc.lock().unwrap();
            let engine = guard.as_mut().unwrap();
            let (both, _) = bsl_search::WorkspaceRoots::build(
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );
            engine.set_workspace_roots(both);
            engine.enable_workspace_watcher_mode();
        }
        SharedState::rewalk_workspace_bsl_dirty(&engine_arc, &crate::state::OwnerStop::default());

        let guard = engine_arc.lock().unwrap();
        let snapshot = guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap();
        assert!(
            snapshot.keys().any(|key| key.path.ends_with("B.bsl")),
            "the next walk covers the root the table gained: {snapshot:?}",
        );
    }

    /// A root `.xml` descriptor can shift any module's graph context, so it marks the whole
    /// collection. "Root" here means the CONFIGURATION's root — the base every stored relative
    /// path is spelled against — and it is not the project directory: a configuration commonly
    /// sits in a subdirectory of it. Comparing against the project directory instead leaves the
    /// descriptor unrecognised and silently serves the stale context.
    #[test]
    fn a_root_xml_of_a_nested_configuration_marks_the_whole_collection() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let configuration = workspace.join("src").join("cf");
        fs::create_dir_all(&configuration).unwrap();
        let module = configuration.join("CommonModules").join("Общий").join("Ext");
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("Module.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(&workspace, &configuration, &[]);
        engine.set_workspace_roots(roots);
        engine.index_directory_fts(&configuration).unwrap();
        assert!(engine.file_count().unwrap() > 0, "the fixture indexes a document");
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let descriptor = configuration.join("Configuration.xml");
        fs::write(&descriptor, "<Configuration/>").unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: descriptor.clone(),
                raw: descriptor.clone(),
                kind: ChangeKind::MaybeChanged,
                seq: 1,
            }],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let marked = guard.as_ref().unwrap().store().context_dirty_paths("code").unwrap();
        assert!(
            !marked.is_empty(),
            "the configuration's root descriptor marks every document's context",
        );
    }

    /// A deleted `.bsl` is removed from the workspace store so it stops appearing in
    /// results — closing the pre-existing gap where a deleted file lingered in FTS.
    #[test]
    fn search_sink_removes_deleted_bsl_from_results() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "Removed.bsl".to_owned(),
                    symbol_name: "УдаляемаяПроцедура".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура УдаляемаяПроцедура()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                    source_span: None,
                }],
                None,
            )
            .unwrap();
        assert_eq!(engine.file_count().unwrap(), 1);
        assert!(
            !engine.text_search("УдаляемаяПроцедура", 10, Some("code")).unwrap().is_empty(),
            "the indexed file is initially found",
        );
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        // The file is gone from disk: classification re-stats it (stats are truth) → removed.
        let removed = workspace.join("Removed.bsl");
        let entry = ChangeEntry {
            canonical: removed.clone(),
            raw: removed,
            kind: ChangeKind::MaybeRemoved,
            seq: 1,
        };
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[entry],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert_eq!(engine.file_count().unwrap(), 0, "the deleted file is dropped from the store");
        assert!(
            engine.text_search("УдаляемаяПроцедура", 10, Some("code")).unwrap().is_empty(),
            "the deleted file no longer appears in FTS results",
        );
    }

    /// An `.xml` metadata edit marks only the owned modules (the sibling `<Dir>/<Name>/`
    /// subtree) context-dirty via the store side table; unrelated modules are untouched
    /// and nothing is marked dirty — proving the resolver walks the owned subtree only,
    /// never the whole workspace.
    #[test]
    fn search_sink_xml_marks_only_owned_modules_context_dirty() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        // An MDO descriptor with an owned module, plus an unrelated object elsewhere.
        let owned = workspace.join("Catalogs/Товары/Ext/ObjectModule.bsl");
        fs::create_dir_all(owned.parent().unwrap()).unwrap();
        fs::write(&owned, "Процедура П()\nКонецПроцедуры").unwrap();
        let unrelated = workspace.join("Catalogs/Другой/Ext/ObjectModule.bsl");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, "Процедура П()\nКонецПроцедуры").unwrap();
        let xml = workspace.join("Catalogs/Товары.xml");
        fs::write(&xml, "<MetaDataObject/>").unwrap();

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration(
                "Catalogs/Товары/Ext/ObjectModule.bsl"
            )),
            "the owned module is marked context-dirty: {dirty:?}",
        );
        assert!(
            !dirty.contains(&bsl_search::FileKey::configuration(
                "Catalogs/Другой/Ext/ObjectModule.bsl"
            )),
            "an unrelated object's module is left untouched: {dirty:?}",
        );
        assert_eq!(dirty.len(), 1, "only the owned subtree is marked, not the whole tree");
        // The xml path is metadata context, not a body edit: nothing is marked dirty and
        // no whole-workspace walk ran.
        let snapshot = engine.workspace_overlay_dirty_paths_snapshot().unwrap();
        assert!(snapshot.is_empty(), "an xml edit marks no body dirty and triggers no walk");
    }

    /// An analyzer-config edit (`dependsOn` and friends) can re-shape the extension
    /// topology with not a single `.xml` touched — the graph context of EVERY indexed
    /// document may be stale, so the sink must mark the whole collection dirty.
    /// Revert-proof: drop the config-file branch in `apply_search_drift` and nothing
    /// is marked (the classifier ignores non-`.bsl`/`.xml` paths).
    #[test]
    fn search_sink_config_edit_marks_whole_collection_context_dirty() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "CommonModules/А/Ext/Module.bsl".to_owned(),
                    symbol_name: "П".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура П()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                    source_span: None,
                }],
                None,
            )
            .unwrap();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());

        let nested_toml = workspace.join("nested/bsl-analyzer.toml");
        fs::create_dir_all(nested_toml.parent().unwrap()).unwrap();
        fs::write(&nested_toml, "[source]\nroot = \".\"\n").unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: nested_toml.clone(),
                raw: nested_toml,
                kind: ChangeKind::MaybeChanged,
                seq: 1,
            }],
            false,
            &graph,
        );
        assert!(
            engine_arc
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .context_dirty_paths("code")
                .unwrap()
                .is_empty(),
            "a nested namesake config must not reshape the workspace root table",
        );

        let toml = workspace.join("bsl-analyzer.toml");
        fs::write(&toml, "[source]\nroot = \".\"\n").unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: toml.clone(),
                raw: toml,
                kind: ChangeKind::MaybeChanged,
                seq: 2,
            }],
            false,
            &graph,
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("CommonModules/А/Ext/Module.bsl")),
            "a root config edit must mark every indexed document context-dirty: {dirty:?}",
        );
    }

    /// A hub rescan (overflow / re-arm) destroyed per-path detail — a config edit
    /// may be among the lost events, so the sink must conservatively mark the whole
    /// collection context-dirty, not only re-mark `.bsl` bodies.
    #[test]
    fn search_sink_rescan_marks_whole_collection_context_dirty() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        // The module exists on disk: the rescan's `.bsl` rewalk prunes store rows
        // whose file is gone, and a pruned row cannot carry a context mark.
        let on_disk = workspace.join("CommonModules/Б/Ext/Module.bsl");
        fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        fs::write(&on_disk, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "CommonModules/Б/Ext/Module.bsl".to_owned(),
                    symbol_name: "П".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура П()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                    source_span: None,
                }],
                None,
            )
            .unwrap();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("CommonModules/Б/Ext/Module.bsl")),
            "a rescan must conservatively mark every indexed document context-dirty: {dirty:?}",
        );
    }

    /// A metadata `.xml` edit marks BOTH the object's owned modules (path convention) AND the
    /// REFERENCING modules — those whose `graph_context` embeds a read of the object — resolved
    /// through the persisted graph's inbound read edges. A module that references nothing about
    /// the object is left untouched.
    ///
    /// Revert-proof: drop the `resolve_referencing_module_files` call in
    /// `mark_xml_affected_context_dirty` and the referencing module `Б` is no longer marked —
    /// the referencing assertion fails.
    #[test]
    fn search_sink_xml_marks_owned_and_referencing_modules_context_dirty() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        fs::write(workspace.join("Configuration.xml"), "<Configuration/>").unwrap();

        // Catalog Х with an OWNED object module (A), resolved by path convention.
        let xml = workspace.join("Catalogs/Х.xml");
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(
            &xml,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Х</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#,
        )
        .unwrap();
        let owned_a = workspace.join("Catalogs/Х/Ext/ObjectModule.bsl");
        fs::create_dir_all(owned_a.parent().unwrap()).unwrap();
        fs::write(&owned_a, "Процедура П() Экспорт\nКонецПроцедуры").unwrap();

        // Referencing common module Б reads the catalog (manager access + query) → inbound
        // read edges into `mdo/Catalog/Х`. Non-referencing module В reads nothing about it.
        write_common_module(
            &workspace,
            "Б",
            "&НаСервере\nПроцедура ЧитаетХ() Экспорт\nСправочники.Х.СоздатьЭлемент();\nЗапрос = \"ВЫБРАТЬ Код ИЗ Справочник.Х\";\nКонецПроцедуры",
        );
        write_common_module(
            &workspace,
            "В",
            "&НаСервере\nПроцедура НичегоНеЧитает() Экспорт\nВозврат;\nКонецПроцедуры",
        );

        // Build + publish the graph so the reverse lookup has real inbound edges to read.
        let out = crate::cache::graph_db_path(&workspace);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        let sync_project = crate::graph::ProjectSnapshot::load(&workspace);
        let sync_universe = crate::graph::universe::ScannedUniverse::scan(&sync_project.scan_roots);
        let summary = crate::graph_db::build_graph_database(
            &sync_project,
            &sync_universe,
            &out,
            stdx::batch::BatchBudget::files(100),
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_owned(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph builds");
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        // A cached graph can publish before its root table. The search table must resolve
        // its stored keys in this case, rather than losing every referencing module.
        graph.adopt_prebuilt(1, crate::graph_db::GraphFp::default(), summary.modules, None);

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("Catalogs/Х/Ext/ObjectModule.bsl")),
            "the owned module is marked context-dirty: {dirty:?}",
        );
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("CommonModules/Б/Ext/Module.bsl")),
            "the referencing module (reads the catalog) is marked context-dirty: {dirty:?}",
        );
        assert!(
            !dirty.contains(&bsl_search::FileKey::configuration("CommonModules/В/Ext/Module.bsl")),
            "a module that references nothing about the catalog is left untouched: {dirty:?}",
        );
    }

    #[test]
    fn background_snapshot_failures_require_rescan_instead_of_empty_xml_success() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(workspace.join("Configuration.xml"), "<Configuration/>").unwrap();
        let xml = workspace.join("Catalogs/Х.xml");
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(&xml, "<MetaDataObject/>").unwrap();
        write_common_module(
            &workspace,
            "Б",
            "&НаСервере\nПроцедура ЧитаетХ() Экспорт\nСправочники.Х.СоздатьЭлемент();\nКонецПроцедуры",
        );

        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(&workspace);
        cache.ensure().unwrap();
        let project = crate::graph::ProjectSnapshot::load(&workspace);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        let summary = crate::graph_db::build_graph_database(
            &project,
            &universe,
            &cache.graph_db_path(),
            stdx::batch::BatchBudget::files(100),
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_owned(),
                publication_id: "test-1".to_owned(),
            },
        )
        .unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = crate::graph::GraphState::for_workspace_with_cache(workspace.clone(), cache)
            .with_lease(lease.clone());
        graph.adopt_prebuilt(1, crate::graph_db::GraphFp::default(), summary.modules, None);

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "CommonModules/Б/Ext/Module.bsl".to_owned(),
                    symbol_name: "ЧитаетХ".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 2,
                    text: "Процедура ЧитаетХ()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                    source_span: None,
                }],
                None,
            )
            .unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let engine: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let entry = crate::change_hub::ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: crate::change_hub::ChangeKind::MaybeChanged,
            seq: 1,
        };
        let occupied: Vec<_> = (0..crate::graph::SNAPSHOT_POOL_CAP)
            .map(|_| graph.snapshot().expect("published descriptor"))
            .collect();

        {
            graph.set_background_snapshot_failure_for_test(Some(
                crate::graph::BackgroundSnapshotFailure::Changed,
            ));
            let mut plan = SharedState::prepare_search_drift(
                &engine,
                &crate::state::OwnerStop::default(),
                std::slice::from_ref(&entry),
                false,
                &graph,
            );
            assert!(plan.full_rescan, "snapshot failure must request recovery rescan");
            assert!(matches!(
                plan.snapshot_outcome,
                Some(SnapshotPreparationOutcome::OperationError(_))
            ));
            assert!(matches!(
                SharedState::apply_prepared_search_drift(
                    &engine,
                    &crate::state::OwnerStop::default(),
                    &lease,
                    &mut plan,
                    &graph
                ),
                crate::state::WorkspaceSearchApply::OperationError(_)
            ));
        }

        // The third way the background snapshot can fail to answer, and the one the failure
        // injector cannot produce: the pool is empty and the fallback cannot take the lease
        // because a peer holds it. A refused lease is not an empty answer — it must leave the
        // same recovery debt as a broken one, or an `.xml` edit silently resolves to "no
        // referencing modules" and the modules that read the changed object keep stale context.
        graph.set_background_snapshot_failure_for_test(None);
        {
            let _held = lease.hold_file_lock_for_test();
            let contended = SharedState::prepare_search_drift(
                &engine,
                &crate::state::OwnerStop::default(),
                std::slice::from_ref(&entry),
                false,
                &graph,
            );
            assert!(matches!(
                contended.snapshot_outcome,
                Some(SnapshotPreparationOutcome::TransientRefusal)
            ));
        }

        drop(occupied);
        let mut recovery = SharedState::prepare_search_drift(
            &engine,
            &crate::state::OwnerStop::default(),
            std::slice::from_ref(&entry),
            true,
            &graph,
        );
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &engine,
                &crate::state::OwnerStop::default(),
                &lease,
                &mut recovery,
                &graph
            ),
            crate::state::WorkspaceSearchApply::Applied(true)
        ));
        assert!(engine
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .context_dirty_paths("code")
            .unwrap()
            .contains(&bsl_search::FileKey::configuration("CommonModules/Б/Ext/Module.bsl")));
        lease.release();
    }

    /// An `.xml` edit BEFORE any graph is published degrades: owned modules are still marked
    /// (path convention needs no graph) and referencing resolution is silently skipped — no
    /// error, no panic. The reverse lookup only rides a published graph.
    #[test]
    fn search_sink_xml_referencing_degrades_without_published_graph() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        fs::write(workspace.join("Configuration.xml"), "<Configuration/>").unwrap();
        let xml = workspace.join("Catalogs/Х.xml");
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(&xml, "<MetaDataObject/>").unwrap();
        let owned_a = workspace.join("Catalogs/Х/Ext/ObjectModule.bsl");
        fs::create_dir_all(owned_a.parent().unwrap()).unwrap();
        fs::write(&owned_a, "Процедура П() Экспорт\nКонецПроцедуры").unwrap();
        // A would-be referencing module exists on disk but there is NO published graph, so it
        // is not discoverable and must not be marked.
        write_common_module(
            &workspace,
            "Б",
            "&НаСервере\nПроцедура ЧитаетХ() Экспорт\nСправочники.Х.СоздатьЭлемент();\nКонецПроцедуры",
        );

        // A workspace graph that has never been built → `snapshot()` returns None.
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

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

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("Catalogs/Х/Ext/ObjectModule.bsl")),
            "the owned module is still marked without a published graph: {dirty:?}",
        );
        assert!(
            !dirty.contains(&bsl_search::FileKey::configuration("CommonModules/Б/Ext/Module.bsl")),
            "referencing resolution is skipped with no published graph: {dirty:?}",
        );
    }

    /// ANY `.xml` directly at the workspace root (not only `Configuration.xml`), with no
    /// owned-module subtree, conservatively marks the whole collection context-dirty — a
    /// root descriptor change can shift any module's context.
    #[test]
    fn search_sink_root_xml_marks_whole_collection_context_dirty() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        let doc = |path: &str, sym: &str| IndexedDocument {
            collection: "code".to_owned(),
            root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
            path: path.to_owned(),
            symbol_name: sym.to_owned(),
            kind: "procedure".to_owned(),
            line_start: 0,
            line_end: 1,
            text: format!("Процедура {sym}()\nКонецПроцедуры"),
            content_hash: "h".to_owned(),
            graph_context: None,
            source_span: None,
        };
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[doc("A.bsl", "Ааа"), doc("B.bsl", "Ббб")],
                None,
            )
            .unwrap();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        // A root `.xml` NOT named Configuration.xml, with no sibling `<stem>/` subtree.
        let xml = workspace.join("SomePlugin.xml");
        fs::write(&xml, "<Root/>").unwrap();
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

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert_eq!(dirty.len(), 2, "a root .xml marks every indexed file: {dirty:?}");
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("A.bsl"))
                && dirty.contains(&bsl_search::FileKey::configuration("B.bsl"))
        );
    }

    /// Root descriptors of the roots other than the configuration's, and of the trees the
    /// root table deliberately does not hold. A change to any of them can shift the graph
    /// context of any module, and the modules of every root have been in the index since
    /// the table gained them — so each must reach the whole collection.
    mod root_descriptors {
        use super::*;
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::FileKey;
        use std::path::{Path, PathBuf};

        /// Two documents, one per root, so a whole-collection mark is distinguishable from a
        /// mark that reached one root only.
        fn doc(root_id: &str, path: &str) -> IndexedDocument {
            IndexedDocument {
                collection: "code".to_owned(),
                root_id: root_id.to_owned(),
                path: path.to_owned(),
                symbol_name: format!("Символ{path}"),
                kind: "procedure".to_owned(),
                line_start: 0,
                line_end: 1,
                text: "Процедура П()\nКонецПроцедуры".to_owned(),
                content_hash: "h".to_owned(),
                graph_context: None,
                source_span: None,
            }
        }

        fn engine_over(
            db_path: &Path,
            workspace: &Path,
            configuration: &Path,
            extensions: &[PathBuf],
        ) -> (super::super::SharedSearchEngine, Vec<String>) {
            let (roots, _) =
                bsl_search::WorkspaceRoots::build(workspace, configuration, extensions);
            let ids: Vec<String> = roots.ids().map(str::to_owned).collect();
            let mut engine = SearchEngine::fts_only(db_path).unwrap();
            engine.set_workspace_roots(roots);
            let docs: Vec<IndexedDocument> =
                ids.iter().map(|id| doc(id, "CommonModules/М/Ext/Module.bsl")).collect();
            engine.sync_indexed_documents_in_collection("code", &docs, None).unwrap();
            (crate::state::shared_engine(Some(engine)), ids)
        }

        fn drift(engine: &super::super::SharedSearchEngine, paths: &[&Path]) {
            let entries: Vec<ChangeEntry> = paths
                .iter()
                .enumerate()
                .map(|(i, path)| ChangeEntry {
                    canonical: path.to_path_buf(),
                    raw: path.to_path_buf(),
                    kind: ChangeKind::MaybeChanged,
                    seq: i as u64 + 1,
                })
                .collect();
            SharedState::apply_search_drift(
                engine,
                &crate::state::OwnerStop::default(),
                &entries,
                false,
                &crate::graph::GraphState::disabled(),
            );
        }

        fn marks(engine: &super::super::SharedSearchEngine) -> std::collections::HashSet<FileKey> {
            let guard = engine.lock().unwrap();
            guard.as_ref().unwrap().context_dirty_paths("code").unwrap()
        }

        /// A dump root: the descriptor that makes the project model call a directory an
        /// extension at all.
        fn dump_root(at: &Path) -> PathBuf {
            std::fs::create_dir_all(at).unwrap();
            std::fs::write(at.join("Configuration.xml"), "<Configuration/>").unwrap();
            at.to_path_buf()
        }

        /// The root descriptor of a REGISTERED extension root. Recognising only the
        /// configuration's root left this one marking nothing at all.
        #[test]
        fn a_root_descriptor_of_an_extension_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            // Outside the workspace on purpose: an extension inside it is covered by the
            // configuration walk by accident, and the fixture would prove nothing.
            let extension = dump_root(&dir.path().join("cfe"));
            let (engine, ids) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );

            let descriptor = extension.join("ConfigDumpInfo.xml");
            std::fs::write(&descriptor, "<ConfigDumpInfo/>").unwrap();
            drift(&engine, &[&descriptor]);

            let dirty = marks(&engine);
            assert_eq!(dirty.len(), ids.len(), "every root's documents are marked: {dirty:?}");
        }

        /// The same descriptor one level down is an ordinary metadata file: it owns at most
        /// its own subtree and must not reach the collection. Without this the root branch
        /// could be "always true" and the tests above would still pass.
        #[test]
        fn a_descriptor_below_a_root_marks_nothing() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let extension = dump_root(&dir.path().join("cfe"));
            let (engine, _) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );

            let mut below = Vec::new();
            for root in [&configuration, &extension] {
                let deep = root.join("Catalogs");
                std::fs::create_dir_all(&deep).unwrap();
                let descriptor = deep.join("ConfigDumpInfo.xml");
                std::fs::write(&descriptor, "<ConfigDumpInfo/>").unwrap();
                below.push(descriptor);
            }
            drift(&engine, &below.iter().map(PathBuf::as_path).collect::<Vec<_>>());

            assert!(marks(&engine).is_empty(), "a descriptor below a root marks nothing");
        }

        /// An extension canonically inside the configuration is REJECTED from the table
        /// (its files carry the configuration's key), so no registered root sits at its
        /// directory — the class the cut names as part of the rule, not an exception.
        #[test]
        fn a_configuration_xml_of_a_rejected_extension_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let nested = dump_root(&configuration.join("nested"));
            let (engine, _) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&nested),
            );

            drift(&engine, &[&nested.join("Configuration.xml")]);

            assert!(!marks(&engine).is_empty(), "the rejected root's descriptor marks the tree");
        }

        /// The same rejected root, a descriptor NOT named `Configuration.xml`. The class is
        /// every root-level descriptor, and a rule keyed on one file name would leave the
        /// rest of it — `ConfigDumpInfo.xml`, a third-party dump's own descriptor — unmarked.
        #[test]
        fn a_root_descriptor_beside_a_configuration_xml_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let nested = dump_root(&configuration.join("nested"));
            let (engine, _) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&nested),
            );

            let descriptor = nested.join("ConfigDumpInfo.xml");
            std::fs::write(&descriptor, "<ConfigDumpInfo/>").unwrap();
            drift(&engine, &[&descriptor]);

            assert!(!marks(&engine).is_empty(), "a descriptor beside a Configuration.xml marks");
        }

        /// The descriptor itself is what vanished, so the tree it stood in can no longer be
        /// recognised by its neighbour — and a removal is exactly the change that shifts
        /// every context.
        #[test]
        fn a_removed_configuration_xml_of_a_rejected_extension_still_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let nested = dump_root(&configuration.join("nested"));
            let (engine, _) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&nested),
            );

            let descriptor = nested.join("Configuration.xml");
            std::fs::remove_file(&descriptor).unwrap();
            drift(&engine, &[&descriptor]);

            assert!(!marks(&engine).is_empty(), "a removed root descriptor still marks the tree");
        }

        /// A root-level descriptor that also has a namesake subtree beside it. The two
        /// answers are not alternatives: the owned subtree is a subset of the collection the
        /// root descriptor reaches, and deciding by whichever branch runs first would leave
        /// the rest of the tree stale.
        #[test]
        fn a_root_descriptor_with_a_namesake_subtree_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let extension = dump_root(&dir.path().join("cfe"));
            let (engine, ids) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );

            let owned = extension.join("ConfigDumpInfo/Ext");
            std::fs::create_dir_all(&owned).unwrap();
            std::fs::write(owned.join("Module.bsl"), "Процедура П()\nКонецПроцедуры").unwrap();
            let descriptor = extension.join("ConfigDumpInfo.xml");
            std::fs::write(&descriptor, "<ConfigDumpInfo/>").unwrap();
            drift(&engine, &[&descriptor]);

            let dirty = marks(&engine);
            for id in &ids {
                assert!(
                    dirty.contains(&FileKey::new(id, "CommonModules/М/Ext/Module.bsl")),
                    "root {id:?} is marked despite the namesake subtree: {dirty:?}",
                );
            }
        }

        /// The event arrives spelled through an alias while the root is declared by its real
        /// path. Attribution ranks roots by the canonical spelling, so both spellings answer
        /// the same; comparing the delivered spelling alone would recognise neither.
        #[cfg(unix)]
        #[test]
        fn a_root_descriptor_reached_through_an_alias_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            // No `Configuration.xml` in the extension root: with a neighbour present the
            // structural probe would answer through the alias too, and the canonical
            // attribution this pins would go untested.
            let extension = dir.path().join("plain-root");
            std::fs::create_dir_all(&extension).unwrap();
            let alias = workspace.join("link");
            std::os::unix::fs::symlink(&extension, &alias).unwrap();
            let (engine, ids) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );

            let descriptor = extension.join("SomePlugin.xml");
            std::fs::write(&descriptor, "<Root/>").unwrap();
            drift(&engine, &[&alias.join("SomePlugin.xml")]);

            let dirty = marks(&engine);
            assert_eq!(dirty.len(), ids.len(), "the aliased spelling is attributed: {dirty:?}");
        }
    }
    /// Marks that land on a module of an EXTENSION root. Its rows are keyed by that root,
    /// so a mark spelled against the configuration would name a different file — or no
    /// file at all.
    mod extension_marks {
        use super::*;
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::FileKey;
        use std::path::{Path, PathBuf};

        const MODULE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000002">
        <Properties><Name>{}</Name><Server>true</Server></Properties>
    </CommonModule>
</MetaDataObject>"#;

        fn write(path: &Path, text: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn common_module(root: &Path, name: &str, body: &str) {
            write(&root.join(format!("CommonModules/{name}.xml")), &MODULE_XML.replace("{}", name));
            write(&root.join(format!("CommonModules/{name}/Ext/Module.bsl")), body);
        }

        /// A configuration and an extension side by side, both declared to the project model.
        /// The extension lies OUTSIDE the configuration root: one inside it is rejected from
        /// the root table and its files carry the configuration's key, which would make the
        /// fixture prove nothing.
        fn two_root_workspace(workspace: &Path) -> (PathBuf, PathBuf) {
            let configuration = workspace.join("cf");
            let extension = workspace.join("cfe");
            write(&configuration.join("Configuration.xml"), "<Configuration/>");
            write(&extension.join("Configuration.xml"), "<Configuration/>");
            fs::write(
                workspace.join("bsl-analyzer.toml"),
                "[source]\nroot = \"cf\"\nextensions = [{ name = \"a\", path = \"cfe\" }]\n",
            )
            .unwrap();
            (configuration, extension)
        }

        fn engine_over(
            db_path: &Path,
            workspace: &Path,
            configuration: &Path,
            extension: &Path,
        ) -> super::super::SharedSearchEngine {
            let (roots, rejected) = bsl_search::WorkspaceRoots::build(
                workspace,
                configuration,
                std::slice::from_ref(&extension.to_path_buf()),
            );
            assert!(rejected.is_empty(), "the extension is a root of its own: {rejected:?}");
            let mut engine = SearchEngine::fts_only(db_path).unwrap();
            engine.set_workspace_roots(roots);
            // Rows keyed by the engine's own attribution: the mark is then compared against a
            // key nobody spelled by hand.
            engine.index_unindexed_roots_fts().unwrap();
            engine.enable_workspace_watcher_mode();
            crate::state::shared_engine(Some(engine))
        }

        fn drift(
            engine: &super::super::SharedSearchEngine,
            xml: &Path,
            graph: &crate::graph::GraphState,
        ) {
            SharedState::apply_search_drift(
                engine,
                &crate::state::OwnerStop::default(),
                &[ChangeEntry {
                    canonical: xml.to_path_buf(),
                    raw: xml.to_path_buf(),
                    kind: ChangeKind::MaybeChanged,
                    seq: 1,
                }],
                false,
                graph,
            );
        }

        /// A module of an extension that READS a configuration object. Its context embeds
        /// that object's metadata, so a change to the object's descriptor makes it stale —
        /// and the mark has to carry the extension's root, the one its row carries.
        #[test]
        fn a_referencing_module_of_an_extension_is_marked_under_its_own_root() {
            let dir = tempdir().unwrap();
            let marks = referencing_marks_in(dir.path(), &dir.path().join("ws"));
            let reader = FileKey::new("cfe", "CommonModules/Б/Ext/Module.bsl");
            assert!(
                marks.dirty.contains(&reader),
                "the extension's reader is marked under its own root: {marks:?}",
            );
            assert!(
                !marks.dirty.contains(&FileKey::new("cfe", "CommonModules/В/Ext/Module.bsl")),
                "a module that reads nothing about the object is untouched: {marks:?}",
            );
            assert!(
                marks.rows.contains(&reader),
                "the mark names the key the row lives under: {marks:?}",
            );
            for key in &marks.dirty {
                assert!(
                    marks.rows.contains(key),
                    "no mark names a root the table does not hold: {key:?} vs {marks:?}",
                );
            }
        }

        /// Under a root directory holding bytes no `str` can carry, the graph — which keeps
        /// its file paths as strings — hands back a rendering, and a rendering belongs to no
        /// root. The reader keeps its stale context until a whole-collection mark, and that
        /// is the deliberate answer: the alternative is a key guessed from a rendering that
        /// several different roots fit, and the seam that key would travel is the one
        /// removals resolve through.
        ///
        /// Only a filesystem that accepts a name outside UTF-8 can stage the root this is
        /// about, and APFS refuses to create one at all (`EILSEQ`), so macOS is out: the
        /// stand is a real workspace under a real directory, and there is no half of it
        /// that survives without that directory. The mark's positive side —
        /// [`a_referencing_module_of_an_extension_is_marked_under_its_own_root`] — runs
        /// everywhere.
        #[cfg(all(unix, not(target_os = "macos")))]
        #[test]
        fn a_referencing_module_under_an_unrepresentable_root_is_left_to_a_wider_mark() {
            use std::os::unix::ffi::OsStringExt;
            let dir = tempdir().unwrap();
            let workspace = dir.path().join(std::ffi::OsString::from_vec(b"ws\xff".to_vec()));
            let marks = referencing_marks_in(dir.path(), &workspace);
            assert!(
                marks.dirty.is_empty(),
                "a rendering marks nothing rather than the wrong thing: {marks:?}",
            );
            assert!(!marks.rows.is_empty(), "the files themselves are indexed as always");
        }

        #[derive(Debug)]
        struct Marks {
            dirty: std::collections::HashSet<FileKey>,
            rows: Vec<FileKey>,
        }

        fn referencing_marks_in(dir: &Path, workspace: &Path) -> Marks {
            let (configuration, extension) = two_root_workspace(workspace);

            let xml = configuration.join("Catalogs/Х.xml");
            write(
                &xml,
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Х</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#,
            );
            common_module(
                &extension,
                "Б",
                "&НаСервере\nПроцедура ЧитаетХ() Экспорт\nСправочники.Х.СоздатьЭлемент();\nКонецПроцедуры",
            );
            common_module(
                &extension,
                "В",
                "&НаСервере\nПроцедура НичегоНеЧитает() Экспорт\nВозврат;\nКонецПроцедуры",
            );

            let out = crate::cache::graph_db_path(workspace);
            fs::create_dir_all(out.parent().unwrap()).unwrap();
            let project = crate::graph::ProjectSnapshot::load(workspace);
            let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
            let summary = crate::graph_db::build_graph_database(
                &project,
                &universe,
                &out,
                stdx::batch::BatchBudget::files(100),
                &crate::graph_db::GraphMeta {
                    revision: 1,
                    fingerprint: crate::graph_db::GraphFp::default(),
                    files: 0,
                    built_at: "t".to_owned(),
                    publication_id: "test-1".to_owned(),
                },
            )
            .expect("graph builds");
            let graph = crate::graph::GraphState::for_workspace(workspace.to_path_buf());
            graph.adopt_prebuilt(
                1,
                crate::graph_db::GraphFp::default(),
                summary.modules,
                project.search_roots.clone(),
            );

            let engine = engine_over(&dir.join("search.db"), workspace, &configuration, &extension);
            drift(&engine, &xml, &graph);

            let guard = engine.lock().unwrap();
            let engine = guard.as_ref().unwrap();
            Marks {
                dirty: engine.context_dirty_paths("code").unwrap(),
                rows: engine
                    .store()
                    .all_files_in_collection("code")
                    .unwrap()
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect(),
            }
        }

        /// The owned modules of an extension's own object: resolved by path convention, so no
        /// graph is needed — but the key still has to be the extension's.
        #[test]
        fn an_owned_module_of_an_extension_is_marked_under_its_own_root() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let (configuration, extension) = two_root_workspace(&workspace);
            let owned = extension.join("Catalogs/Т/Ext/ObjectModule.bsl");
            write(&owned, "Процедура П()\nКонецПроцедуры");
            let xml = extension.join("Catalogs/Т.xml");
            write(&xml, "<MetaDataObject/>");

            let engine =
                engine_over(&dir.path().join("search.db"), &workspace, &configuration, &extension);
            drift(&engine, &xml, &crate::graph::GraphState::disabled());

            let guard = engine.lock().unwrap();
            let dirty = guard.as_ref().unwrap().context_dirty_paths("code").unwrap();
            assert!(
                dirty.contains(&FileKey::new("cfe", "Catalogs/Т/Ext/ObjectModule.bsl")),
                "the extension's owned module is marked under its own root: {dirty:?}",
            );
        }
    }

    /// An `.xml` drift whose owned module is marked context-dirty must NUDGE the graph to
    /// catch up — otherwise a search-only user (who never triggers a `graph` tool freshness
    /// check) leaves the marks unresolved forever. Asserting the graph left `Idle` with NO
    /// graph tool call. Disable the `graph.nudge_rebuild()` call → the graph stays `Idle` and
    /// this fails.
    #[test]
    fn search_sink_xml_drift_nudges_graph_to_catch_up() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        // An MDO descriptor with an owned module so the xml resolves to a real dirty mark.
        let owned = workspace.join("Catalogs/Товары/Ext/ObjectModule.bsl");
        fs::create_dir_all(owned.parent().unwrap()).unwrap();
        fs::write(&owned, "Процедура П()\nКонецПроцедуры").unwrap();
        let xml = workspace.join("Catalogs/Товары.xml");
        fs::write(&xml, "<MetaDataObject/>").unwrap();

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        assert_eq!(graph.status(), crate::graph::GraphStatus::Idle, "graph starts idle");

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

        assert_ne!(
            graph.status(),
            crate::graph::GraphStatus::Idle,
            "the xml drift nudged the graph to catch up without any graph tool call",
        );
    }
    /// A batch that demands a full reconcile still carries the exact paths it knows about,
    /// and the deletions among them are the one thing the re-walk cannot recover: an
    /// incomplete walk skips the reconcile precisely so it does not evict healthy files,
    /// leaving a deleted file in the index. Applying the delivered removals costs nothing
    /// and is exact — including a vanished directory, whose descendants no drain can name.
    #[test]
    fn a_rescan_batch_removes_the_deletions_it_delivered_even_when_the_walk_is_incomplete() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        // Toggles the process-global `FORCE_REWALK_WALK_ERROR` seam; serialize against the
        // other tests that read it.
        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            let mut index = |path: &str, name: &str| {
                store
                    .reindex_file(
                        bsl_search::CONFIGURATION_ROOT_ID,
                        path,
                        b"h",
                        &[Chunk {
                            kind: ChunkKind::Procedure,
                            name: name.to_owned(),
                            is_export: true,
                            annotations: vec![],
                            line_start: 0,
                            line_end: 1,
                            text: format!("Процедура {name}()\nКонецПроцедуры"),
                        }],
                        None,
                    )
                    .unwrap();
            };
            index("Gone.bsl", "Ушедшая");
            index("Dropped/One.bsl", "ПерваяИзПоддерева");
            index("Dropped/Two.bsl", "ВтораяИзПоддерева");
            index("Kept.bsl", "Оставшаяся");
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        assert_eq!(engine.file_count().unwrap(), 4, "all four files are indexed");
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let gone = workspace.join("Gone.bsl");
        let dropped = workspace.join("Dropped");
        let entries = [
            ChangeEntry {
                canonical: gone.clone(),
                raw: gone,
                kind: ChangeKind::MaybeRemoved,
                seq: 1,
            },
            ChangeEntry {
                canonical: dropped.clone(),
                raw: dropped,
                kind: ChangeKind::SubtreeRemoved,
                seq: 2,
            },
        ];
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &entries,
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        for token in ["Ушедшая", "ПерваяИзПоддерева", "ВтораяИзПоддерева"]
        {
            assert!(
                engine.text_search(token, 10, Some("code")).unwrap().is_empty(),
                "{token} was delivered as deleted and must not answer searches",
            );
        }
        assert!(
            !engine.text_search("Оставшаяся", 10, Some("code")).unwrap().is_empty(),
            "a file nobody reported deleted is untouched",
        );
    }

    /// An event says what was true when it fired, not what is true when it is consumed: a
    /// directory removed and restored (a checkout, an editor's atomic replace) arrives as a
    /// removal for a subtree that exists again. The classifier re-stats every path for this
    /// reason, and a subtree removal must too — it deletes far more at once, and the walk
    /// that would restore it is skipped exactly when it is incomplete.
    #[test]
    fn a_subtree_removal_for_a_directory_that_came_back_deletes_nothing() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        fs::create_dir_all(workspace.join("Restored")).unwrap();
        fs::write(workspace.join("Restored/Alive.bsl"), "Процедура Живущая()\nКонецПроцедуры")
            .unwrap();
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Restored/Alive.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Живущая".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Живущая()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        // The walk cannot vouch for anything, so nothing would restore a wrong deletion.
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let restored = workspace.join("Restored");
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: restored.clone(),
                raw: restored,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            !engine.text_search("Живущая", 10, Some("code")).unwrap().is_empty(),
            "a directory that is on disk when the removal is applied keeps its files",
        );
    }

    /// A vanished directory has to leave the index whether or not the batch also demanded a
    /// full reconcile. The ordinary branch answers a subtree removal with a re-walk, and
    /// that re-walk refuses to reconcile when it is incomplete — deliberately, so it cannot
    /// evict healthy files. Then nothing removes the descendants: the classifier calls a
    /// subtree removal structural and skips it, and no event ever names them.
    #[test]
    fn a_vanished_directory_leaves_the_index_when_the_ordinary_branch_cannot_walk() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        fs::write(workspace.join("Kept.bsl"), "Процедура Уцелевшая()\nКонецПроцедуры").unwrap();
        {
            let mut store = Store::open(&db_path).unwrap();
            let mut index = |path: &str, name: &str| {
                store
                    .reindex_file(
                        bsl_search::CONFIGURATION_ROOT_ID,
                        path,
                        b"h",
                        &[Chunk {
                            kind: ChunkKind::Procedure,
                            name: name.to_owned(),
                            is_export: true,
                            annotations: vec![],
                            line_start: 0,
                            line_end: 1,
                            text: format!("Процедура {name}()\nКонецПроцедуры"),
                        }],
                        None,
                    )
                    .unwrap();
            };
            index("Dropped/One.bsl", "ПерваяУшедшая");
            index("Kept.bsl", "Уцелевшая");
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        assert_eq!(engine.file_count().unwrap(), 2, "both files are indexed");
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        // The re-walk this branch performs cannot vouch for anything, so its reconcile —
        // the only other thing that would remove the descendants — is skipped.
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let dropped = workspace.join("Dropped");
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: dropped.clone(),
                raw: dropped,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("ПерваяУшедшая", 10, Some("code")).unwrap().is_empty(),
            "the descendants of a vanished directory stop answering searches",
        );
        assert!(
            !engine.text_search("Уцелевшая", 10, Some("code")).unwrap().is_empty(),
            "and nothing else is touched",
        );
    }

    /// The hub names a vanished path a subtree only when it has no extension — it cannot ask
    /// a path that is gone what it used to be. A directory with a dot in its name therefore
    /// arrives as an ordinary removal, and the classifier drops it for being neither `.bsl`
    /// nor `.xml`. Its files would then have nobody to remove them.
    #[test]
    fn a_vanished_directory_with_a_dotted_name_still_loses_its_files() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Dropped.v1/One.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "ИзВерсии".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура ИзВерсии()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        // The hub calls this `MaybeRemoved`, because `Dropped.v1` looks like it has an
        // extension — the one thing it can tell about a path that no longer exists.
        let dropped = workspace.join("Dropped.v1");
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: dropped.clone(),
                raw: dropped,
                kind: ChangeKind::MaybeRemoved,
                seq: 1,
            }],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("ИзВерсии", 10, Some("code")).unwrap().is_empty(),
            "a dotted directory name does not save its files from a deletion",
        );
    }

    /// A directory whose name ends in `.bsl` looks exactly like a file to everything that
    /// can only read the name — the hub, and any filter written in terms of extensions.
    /// Deciding per KEY sidesteps the question: a real file simply has nothing under it.
    #[test]
    fn a_vanished_directory_named_like_a_module_still_loses_its_files() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Модули.bsl/One.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "ИзПапкиМодули".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура ИзПапкиМодули()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let dropped = workspace.join("Модули.bsl");
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: dropped.clone(),
                raw: dropped,
                kind: ChangeKind::MaybeRemoved,
                seq: 1,
            }],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("ИзПапкиМодули", 10, Some("code")).unwrap().is_empty(),
            "a module-looking directory name does not save its files",
        );
    }

    /// A directory that is back proves only that the NAME is taken again, not that the
    /// files under it survived: a checkout can restore it with a different set entirely.
    /// Judging the whole subtree by the directory keeps the ones that are truly gone.
    #[test]
    fn a_directory_that_came_back_with_other_files_loses_the_ones_that_did_not() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        let restored = workspace.join("Restored");
        fs::create_dir_all(&restored).unwrap();
        fs::write(restored.join("Stays.bsl"), "Процедура Оставшаяся()\nКонецПроцедуры").unwrap();
        {
            let mut store = Store::open(&db_path).unwrap();
            let mut index = |path: &str, name: &str| {
                store
                    .reindex_file(
                        bsl_search::CONFIGURATION_ROOT_ID,
                        path,
                        b"h",
                        &[Chunk {
                            kind: ChunkKind::Procedure,
                            name: name.to_owned(),
                            is_export: true,
                            annotations: vec![],
                            line_start: 0,
                            line_end: 1,
                            text: format!("Процедура {name}()\nКонецПроцедуры"),
                        }],
                        None,
                    )
                    .unwrap();
            };
            index("Restored/Stays.bsl", "Оставшаяся");
            index("Restored/Gone.bsl", "Пропавшая");
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: restored.clone(),
                raw: restored,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("Пропавшая", 10, Some("code")).unwrap().is_empty(),
            "the file that did not come back is gone from the index",
        );
        assert!(
            !engine.text_search("Оставшаяся", 10, Some("code")).unwrap().is_empty(),
            "the one that did is untouched",
        );
    }

    /// A name taken by something that is not a directory is not the subtree coming back: a
    /// file holds no files, so its descendants are gone as surely as if nothing were there.
    #[test]
    fn a_subtree_replaced_by_a_file_is_still_removed() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Replaced/One.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "ПодЗамену".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура ПодЗамену()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        // The directory is gone and a plain file now carries its name.
        let replaced = workspace.join("Replaced");
        fs::write(&replaced, "не каталог").unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: replaced.clone(),
                raw: replaced,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("ПодЗамену", 10, Some("code")).unwrap().is_empty(),
            "a name taken by a file cannot hold the subtree's files",
        );
    }

    /// The hub decides a path vanished by following links, so a subtree reached through a
    /// link whose target is deleted is gone as far as it is concerned. Asking about the link
    /// itself would answer "still there" and silently drop the removal it delivered.
    #[cfg(unix)]
    #[test]
    fn a_subtree_reached_through_a_dangling_link_is_removed() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let outside = tempdir().unwrap();
        let target = outside.path().join("real");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("A.bsl"), "Процедура Внешняя()\nКонецПроцедуры").unwrap();
        let link = workspace.join("Linked");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Linked/A.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Внешняя".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Внешняя()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        // The target goes; the link stays behind, pointing at nothing.
        fs::remove_dir_all(&target).unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: link.clone(),
                raw: link,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("Внешняя", 10, Some("code")).unwrap().is_empty(),
            "a subtree whose target is deleted stops answering searches",
        );
    }

    /// "Could not check" is not "is gone". A subtree whose parent is momentarily unreadable
    /// answers `PermissionDenied`, and treating that as proof of deletion clears rows,
    /// overlay entries and vectors for files that are on disk — while the walk that would
    /// restore them is skipped for exactly the same reason.
    #[cfg(unix)]
    #[test]
    fn a_subtree_removal_that_cannot_be_verified_deletes_nothing() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};
        use std::os::unix::fs::PermissionsExt;

        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        let blocked = workspace.join("Blocked");
        fs::create_dir_all(blocked.join("Gone")).unwrap();
        fs::write(blocked.join("Gone/A.bsl"), "Процедура Недоступная()\nКонецПроцедуры").unwrap();
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Blocked/Gone/A.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Недоступная".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Недоступная()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&blocked).is_ok() {
            // Running as root: permissions cannot make the parent unreadable.
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let gone = blocked.join("Gone");
        SharedState::apply_search_drift(
            &engine_arc,
            &crate::state::OwnerStop::default(),
            &[ChangeEntry {
                canonical: gone.clone(),
                raw: gone,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            !engine.text_search("Недоступная", 10, Some("code")).unwrap().is_empty(),
            "a subtree that could not be checked keeps its files",
        );
    }

    /// A partial rescan walk (an error mid-walk) must NOT reconcile: `present` is missing healthy
    /// files, so deleting stored files against it would evict live data. Only a clean walk
    /// reconciles. Reverting the walk-error guard deletes the stored file on the errored walk.
    #[test]
    fn rescan_walk_error_skips_reconcile_and_keeps_stored_files() {
        use bsl_search::{Chunk, ChunkKind, Store};

        // This test toggles the process-global `FORCE_REWALK_WALK_ERROR` seam; serialize against the
        // boot-reconcile tests (which read it) so its forced error can't leak into their walk.
        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Gone.bsl",
                    b"ha",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "П".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура П()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        assert_eq!(engine.file_count().unwrap(), 1, "the stored file is present");
        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }

        // Errored walk: reconcile is skipped, so the stored (disk-absent) file SURVIVES.
        {
            FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
            let _reset = ResetWalkErr;
            SharedState::rewalk_workspace_bsl_dirty(
                &engine_arc,
                &crate::state::OwnerStop::default(),
            );
            assert_eq!(
                engine_arc.lock().unwrap().as_ref().unwrap().file_count().unwrap(),
                1,
                "a partial walk must not reconcile healthy files out of the store",
            );
        }

        // Clean walk: the stored-but-absent file is reconciled out.
        SharedState::rewalk_workspace_bsl_dirty(&engine_arc, &crate::state::OwnerStop::default());
        assert_eq!(
            engine_arc.lock().unwrap().as_ref().unwrap().file_count().unwrap(),
            0,
            "a clean walk reconciles the deleted file out",
        );
    }
    /// The overlay keys dirty paths relative to the ENGINE root (the nested config source root),
    /// while the resident is indexed under the OUTER workspace root. A point refresh's phase B
    /// must resolve each dirty rel to an absolute path against the engine root before asking the
    /// resident, so a nested config (every real workspace) actually gets a resident-fed reindex.
    /// Reverting the absolute-join (passing the rel verbatim) leaves the resident-fed count at 0.
    #[test]
    fn a_point_refresh_feeds_a_nested_config_from_the_resident() {
        use crate::diagnostics_state::{
            DiagnosticsState, DiagnosticsStatus, ResidentModuleSnapshotSource,
        };
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let outer = dir.path().to_path_buf();
        let cf = outer.join("src").join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(
            cf.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &cf,
            "Сервер",
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n",
        );
        let module = cf.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");

        // Overlay engine rooted at the NESTED config root, so `source_path != outer`.
        let mut engine = SearchEngine::fts_only(&outer.join("search.db")).unwrap();
        engine.set_workspace_root(cf.clone());
        engine.enable_workspace_watcher_mode();
        engine.prime_workspace_overlay().unwrap();

        // The file grows on disk so the reindex genuinely rebuilds it (fingerprint differs).
        fs::write(
            &module,
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n\
             Процедура Ещё() Экспорт КонецПроцедуры\n",
        )
        .unwrap();

        // The resident is built against the OUTER root AFTER the edit, so it holds the new bytes.
        let diagnostics = DiagnosticsState::for_workspace(outer.clone());
        diagnostics.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(diagnostics.status(), DiagnosticsStatus::Ready { .. }) {
            assert!(Instant::now() < deadline, "the resident did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }

        let source: Arc<dyn bsl_search::ModuleSnapshotSource> =
            Arc::new(ResidentModuleSnapshotSource::new(diagnostics.clone()));
        engine.set_module_snapshot_source(source);
        assert!(
            engine.mark_workspace_path_dirty(&module).unwrap(),
            "the nested module marks dirty"
        );

        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        crate::state::overlay_backlog::refresh_one_batch(
            &engine_arc,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
        );

        let fed = engine_arc
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_overlay_resident_fed_count()
            .unwrap();
        assert_eq!(
            fed, 1,
            "a nested-config dirty path must be served from the resident's shared parse",
        );
    }

    /// Search and diagnostics drain independent hub cursors, so a just-edited file leaves the
    /// resident BEHIND disk. The point refresh must catch the resident up on pending drift FIRST, so the snapshot text matches disk and the reindex is resident-fed rather than
    /// falling back to a disk read. Reverting the `catch_up` call leaves the resident stale, the
    /// byte-compare misses, and the resident-fed count stays 0.
    #[test]
    fn a_point_refresh_catches_up_a_stale_resident_before_reading() {
        use crate::change_hub::WorkspaceChangeHub;
        use crate::diagnostics_state::{
            DiagnosticsState, DiagnosticsStatus, ResidentModuleSnapshotSource,
        };
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(
            root.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &root,
            "Сервер",
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n",
        );
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");

        let hub = WorkspaceChangeHub::start(vec![root.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)), "the hub must arm");
        let mut observer = hub.subscribe();

        let mut engine = SearchEngine::fts_only(&root.join("search.db")).unwrap();
        engine.set_workspace_root(root.clone());
        engine.enable_workspace_watcher_mode();
        engine.prime_workspace_overlay().unwrap();

        // Resident built at v1, wired to the SAME hub, but it never polls drift on its own.
        let diagnostics =
            DiagnosticsState::for_workspace(root.clone()).with_change_hub(hub.clone());
        diagnostics.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(diagnostics.status(), DiagnosticsStatus::Ready { .. }) {
            assert!(Instant::now() < deadline, "the resident did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }

        let source: Arc<dyn bsl_search::ModuleSnapshotSource> =
            Arc::new(ResidentModuleSnapshotSource::new(diagnostics.clone()));
        engine.set_module_snapshot_source(source);

        // Edit on disk (v2, longer): the resident's recorded revision is now stale.
        std::thread::sleep(Duration::from_millis(10));
        fs::write(
            &module,
            "&НаСервере\nФункция Ч() Экспорт Возврат 2; КонецФункции\n\
             Процедура Ещё() Экспорт КонецПроцедуры\n",
        )
        .unwrap();
        assert!(engine.mark_workspace_path_dirty(&module).unwrap());

        // Wait until the hub delivered the edit, so the diagnostics cursor drains it in `catch_up`.
        let deadline = Instant::now() + Duration::from_secs(5);
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
        assert!(delivered, "the hub delivered the edit");

        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        crate::state::overlay_backlog::refresh_one_batch(
            &engine_arc,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
        );

        let fed = engine_arc
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_overlay_resident_fed_count()
            .unwrap();
        assert_eq!(
            fed, 1,
            "catch_up must reconcile the stale resident so the snapshot matches disk (fed reindex)",
        );
    }

    /// One point batch takes at most its key budget: marking N + k paths dirty serves exactly N
    /// from the shared parse in one batch, and the remaining k stay dirty for the next.
    #[test]
    fn a_point_refresh_takes_at_most_one_batch_of_keys() {
        use bsl_search::{ModuleSnapshot, ModuleSnapshotSource, SnapshotFetch};

        struct DiskFakeSource;
        impl ModuleSnapshotSource for DiskFakeSource {
            fn text_and_parse(&self, path: &str) -> SnapshotFetch {
                match std::fs::read_to_string(path) {
                    Ok(text) => {
                        let root = parser::parse(&text).syntax_node();
                        SnapshotFetch::Fetched(ModuleSnapshot { text: text.into(), root })
                    }
                    Err(_) => SnapshotFetch::Unavailable,
                }
            }
        }

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        engine.prime_workspace_overlay().unwrap();
        engine.set_module_snapshot_source(Arc::new(DiskFakeSource));

        let extra = 3usize;
        let total = bsl_search::POINT_BATCH_KEYS + extra;
        for i in 0..total {
            let rel = format!("Module{i}.bsl");
            fs::write(workspace.join(&rel), format!("Процедура П{i}()\nКонецПроцедуры\n")).unwrap();
            assert!(engine.mark_workspace_path_dirty(workspace.join(&rel)).unwrap());
        }

        let engine_arc: super::SharedSearchEngine = crate::state::shared_engine(Some(engine));
        crate::state::overlay_backlog::refresh_one_batch(
            &engine_arc,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert_eq!(
            engine.workspace_overlay_resident_fed_count().unwrap(),
            bsl_search::POINT_BATCH_KEYS,
            "exactly one batch is served from the shared parse",
        );
        assert_eq!(
            engine.workspace_overlay_dirty_paths().unwrap().len(),
            extra,
            "paths beyond the batch stay dirty for the next one",
        );
    }
    /// Unit proof of the shared boot reconcile that every Clean branch funnels through
    /// ([`SharedState::reconcile_boot_store_with_disk`]): a store row for a file DELETED while the
    /// daemon was down is reconciled out, while a present file is kept, and the helper reports the
    /// store PROVEN reconciled. The fused / standalone-deferred / FTS-cold Clean branches all call
    /// this exact helper after their index step, so proving it here proves the deletion is removed on
    /// each — without standing up a full graph build for the fused path. Store-level `file_count` is
    /// asserted so the removal is real, not overlay-hidden.
    #[test]
    fn boot_reconcile_removes_deleted_file_keeps_present() {
        // The boot reconcile reads the process-global `FORCE_REWALK_WALK_ERROR` seam; serialize
        // against the walk-error tests that toggle it so a concurrent set can't force a false error.
        let _env_lock = env_lock();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module_tree(
            &workspace,
            "Улетевший",
            "&НаСервере\nФункция ИсчезнувшийСимвол() Экспорт Возврат 1; КонецФункции\n",
        );
        write_common_module_tree(
            &workspace,
            "Постоянный",
            "&НаСервере\nФункция ЖивойСимвол() Экспорт Возврат 1; КонецФункции\n",
        );

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        assert_eq!(engine.file_count().unwrap(), 2, "both modules are indexed");

        // The Улетевший module vanishes while the daemon is down.
        fs::remove_dir_all(workspace.join("CommonModules").join("Улетевший")).unwrap();
        fs::remove_file(workspace.join("CommonModules").join("Улетевший.xml")).unwrap();

        let reconciled = SharedState::reconcile_boot_store_with_disk_fenced(
            &mut engine,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &crate::state::OwnerStop::default(),
        )
        .expect("the unmanaged reconcile fence cannot refuse");
        assert!(reconciled, "a clean walk proves the store reconciled");
        assert_eq!(
            engine.file_count().unwrap(),
            1,
            "the deleted file's rows are reconciled out of the store",
        );
        let files: Vec<String> = engine
            .store()
            .all_files_in_collection("code")
            .unwrap()
            .into_iter()
            .map(|(key, _hash)| key.path)
            .collect();
        assert!(
            files.iter().any(|p| p.contains("Постоянный")) && files.len() == 1,
            "only the present module survives: {files:?}",
        );
    }
    /// A walk error at boot cannot prove the store was reconciled, so a Clean branch must DOWNGRADE
    /// to a prime rather than assert a false clean. Force the reconcile walk to error and drive a
    /// cold FTS-only boot (otherwise Clean) through the real init path: it must select Prime.
    /// Reverting the downgrade (staying Clean on a failed walk) fails this.
    #[test]
    fn boot_walk_error_downgrades_clean_to_prime() {
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
        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let init = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("cold FTS-only init produces an engine");
        assert!(
            matches!(init.overlay_init, OverlayInit::Prime),
            "a boot whose reconcile walk errored must prime, not assert a false clean",
        );
    }
}
