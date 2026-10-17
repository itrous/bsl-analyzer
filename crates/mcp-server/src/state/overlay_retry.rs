//! The overlay retry driver: the ONE owner of every Embed pass over the workspace overlay,
//! startup included.
//!
//! The rule the driver enforces is an ownership invariant, not a schedule: the right to run
//! a pass is the CURRENT workspace-lease ownership until a live foreign owner is observed,
//! writes happen only under ownership, and a disarm is reserved for what cannot change
//! within this process (including terminal supersession). Transient refusal retries within one
//! bounded deadline; an operation failure waits for a new signal, while an incomplete scan or
//! internally superseded overlay plan keeps the obligation alive.

use super::retry_window::{RetryDecision, RetryOwner, RetryWindow};
use super::types::{OverlayWarmupState, SemanticRuntimeStatus, SharedSearchEngine};
use crate::workspace_lease::WorkspaceLease;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
static RUN_PASS_FINISH_HOOK: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);
#[cfg(test)]
static BUDGET_EXHAUSTED_HOOK: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

/// The driver's own tick: how often it re-checks the condition with no kick arriving. A
/// safety net for a signal that went quiet (a notification lost, a backoff that expired
/// with no drift), not the primary wake-up — kicks are.
pub(super) const TICK: Duration = Duration::from_secs(30);

/// Backoff schedule for consecutive not-clean outcomes: exponential from one tick, capped.
/// Pure so the schedule is testable apart from the thread.
pub(crate) fn retry_delay(streak: u32) -> Duration {
    const CAP: Duration = Duration::from_secs(30 * 60);
    if streak == 0 {
        return Duration::ZERO;
    }
    TICK.checked_mul(1u32 << streak.min(6).saturating_sub(1)).map(|d| d.min(CAP)).unwrap_or(CAP)
}

struct RetryState {
    /// Bumped by every [`OverlayRetry::kick_fresh`]. A pass captures it before running;
    /// if it moved while a non-transient pass ran, the pass's outcome predates a fresh fact.
    /// During an active transient obligation it is observability only: signals do not reset
    /// that obligation's deadline or backoff.
    fresh_epoch: u64,
    /// Consecutive passes that did not end clean; indexes into [`retry_delay`].
    streak: u32,
    /// No pass starts before this instant (backoff gate).
    next_allowed: Instant,
    /// The last outcome demands another pass even if the cache signals read clean —
    /// `Failed` before Phase C leaves no trace in the cache, `Superseded` leaves the
    /// fresher state that may itself be incomplete.
    obligation: bool,
    /// An operation error or expired transient budget waits for a new external signal.
    failed: bool,
    /// Terminal: nothing this process can retry into existence (no embedder/root, terminal
    /// engine init failure, or an observed live foreign workspace owner).
    disarmed: bool,
    /// Asked to look at the signals again (see [`OverlayRetry::coalesce`]). Read by an idle
    /// worker before it sleeps: a wake that came while it was deciding is not lost.
    look_again: bool,
}

pub(crate) struct OverlayRetry {
    engine: SharedSearchEngine,
    overlay_warmup: Arc<Mutex<OverlayWarmupState>>,
    semantic_runtime: Arc<Mutex<SemanticRuntimeStatus>>,
    lease: WorkspaceLease,
    publish_retry_budget: Duration,
    state: Mutex<RetryState>,
    wake: Condvar,
    /// The daemon's stop, not one of its own: a driver with a private flag is one more thing
    /// a shutdown has to remember to raise, and one more order in which it can be wrong.
    stop: super::OwnerStop,
    /// Whether a pass is running right now. The backend's lifetime is read off this rather
    /// than off the shared status slot: the status has a second writer (the embed pass), and
    /// its `Ready` at its own finish would otherwise erase this pass from view mid-sync.
    /// Raised and lowered by the pass itself, so no exit can leave it set.
    pass_active: AtomicBool,
    #[cfg(test)]
    passes_started: std::sync::atomic::AtomicUsize,
    /// Completed pass count — observability and the single-flight proof in tests.
    passes: AtomicUsize,
    /// Runs once, the next time the worker is about to idle with nothing due, and is spent.
    #[cfg(test)]
    before_idle_wait: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// Marks a pass as running for as long as it lives.
struct PassActive<'a>(&'a AtomicBool);

impl<'a> PassActive<'a> {
    fn raise(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(flag)
    }
}

impl Drop for PassActive<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Writes the verdict a departed overlay worker can no longer write.
///
/// Once the worker is gone — unwound out of a pass, or returned on stop — nobody will ever
/// settle `OverlaySyncing` again, and `search status` would report a sync that ended long ago.
/// A status that was already settled is left alone: this writes the missing verdict, it does
/// not overwrite an existing one.
struct SyncingVerdict(Arc<OverlayRetry>);

impl Drop for SyncingVerdict {
    fn drop(&mut self) {
        let mut status =
            self.0.semantic_runtime.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(*status, SemanticRuntimeStatus::OverlaySyncing) {
            // A driver that left because the daemon asked it to did not fail, and saying it
            // did puts a breakage on the status line for an ordinary shutdown. The caller's
            // reason code is the same either way; what differs is whether the line tells the
            // truth about why the sync stopped.
            *status = if self.0.stop.is_stopped() {
                SemanticRuntimeStatus::Stopped
            } else {
                SemanticRuntimeStatus::Failed(
                    "workspace overlay sync ended without a verdict".to_owned(),
                )
            };
        }
    }
}

impl OverlayRetry {
    /// Build the driver and start its worker thread. The worker is the ONLY place a pass
    /// runs, so single-flight holds by construction: kicks merely wake it.
    pub(crate) fn spawn(
        engine: SharedSearchEngine,
        stop: super::OwnerStop,
        overlay_warmup: Arc<Mutex<OverlayWarmupState>>,
        semantic_runtime: Arc<Mutex<SemanticRuntimeStatus>>,
        lease: WorkspaceLease,
        publish_retry_budget: Duration,
    ) -> Arc<Self> {
        let retry =
            Self::new(engine, stop, overlay_warmup, semantic_runtime, lease, publish_retry_budget);
        retry.start();
        retry
    }

    /// The driver with no worker behind it yet. Split out from [`OverlayRetry::spawn`] so a
    /// caller can settle the flags a started worker reads — `stop` above all — before the
    /// worker exists to read them; the worker's first act is a pass, so there is no other
    /// way to observe it starting from an already-final state.
    fn new(
        engine: SharedSearchEngine,
        stop: super::OwnerStop,
        overlay_warmup: Arc<Mutex<OverlayWarmupState>>,
        semantic_runtime: Arc<Mutex<SemanticRuntimeStatus>>,
        lease: WorkspaceLease,
        publish_retry_budget: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            overlay_warmup,
            semantic_runtime,
            lease,
            publish_retry_budget,
            state: Mutex::new(RetryState {
                fresh_epoch: 0,
                streak: 0,
                next_allowed: Instant::now(),
                obligation: false,
                failed: false,
                disarmed: false,
                look_again: false,
            }),
            wake: Condvar::new(),
            stop,
            pass_active: AtomicBool::new(false),
            #[cfg(test)]
            passes_started: std::sync::atomic::AtomicUsize::new(0),
            passes: AtomicUsize::new(0),
            #[cfg(test)]
            before_idle_wait: Mutex::new(None),
        })
    }

    /// Start the worker thread. The worker is the ONLY place a pass runs.
    fn start(self: &Arc<Self>) {
        // The stop releases THIS wait too: the driver parks on its own condvar, and a stop
        // that only raised a flag would leave it asleep until its next tick.
        //
        // The state lock is TAKEN before the notify, and that is the whole point: the worker
        // reads the stop and enters its wait while holding it, so a notification delivered in
        // between would find no waiter and be lost — and one of those waits is unbounded, so
        // losing it is not a late exit but no exit at all.
        let waiter = Arc::downgrade(self);
        self.stop.wakes(move || {
            if let Some(retry) = waiter.upgrade() {
                let _state = retry.state.lock().unwrap_or_else(|poison| poison.into_inner());
                retry.wake.notify_all();
            }
        });
        let worker = Arc::clone(self);
        // Counted before the thread starts, and dropped on every way out: a driver about to
        // run is an owner a shutdown must still see leave.
        let live = self.stop.enter();
        std::thread::Builder::new()
            .name("bsl-search-overlay-retry".to_owned())
            .spawn(move || {
                let _live = live;
                let _verdict = SyncingVerdict(Arc::clone(&worker));
                worker.run()
            })
            .ok();
    }

    /// Fresh drift arrived. It rearms a previously failed worker, but coalesces into an active
    /// transient obligation without resetting its deadline or backoff.
    pub(crate) fn kick_fresh(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.fresh_epoch = state.fresh_epoch.wrapping_add(1);
            if state.failed {
                state.failed = false;
                state.obligation = true;
            }
            state.streak = 0;
            state.next_allowed = Instant::now();
        }
        self.wake.notify_all();
    }

    /// A signal that something MAY be worth another look, from a caller that has not observed
    /// any new fact — a request path is the only one.
    ///
    /// It wakes the worker and nothing else. Resetting the backoff here would let a stream of
    /// searches erase the pacing of an obligation that is already failing, and clearing
    /// `failed` would restart an obligation that expired without any new external work — both
    /// of which the retry contract forbids.
    ///
    /// It cannot be lost: the request to look again is recorded under the state lock, and a
    /// worker reads it under that lock before it sleeps. The worker never holds the lock while
    /// it waits for the engine, so taking it here is brief.
    pub(crate) fn coalesce(&self) {
        // Deliberately NOT `fresh_epoch`: `settle_outcome_at` reads a moved epoch as "a fresh
        // fact landed while the pass ran" and clears the backoff for it. Bumping it here would
        // let a stream of searches erase the pacing of a failing obligation — the very thing
        // this method exists to avoid. Asking for another look is all a request may do.
        if let Ok(mut state) = self.state.lock() {
            state.look_again = true;
        }
        self.wake.notify_all();
    }

    /// Terminal disarm: nothing this process can retry into existence.
    pub(crate) fn disarm(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.disarmed = true;
        }
        self.wake.notify_all();
    }

    /// Settle the stop this driver reads. Production stops every owner through the daemon's
    /// `OwnerStop` itself, before the lease is released; a test that builds its own driver
    /// owns the only clone of that stop and reaches it through here.
    #[cfg(test)]
    pub(crate) fn stop(&self) {
        self.stop.stop();
        self.wake.notify_all();
    }

    /// A driver with no worker thread, so a test reads the obligation state it leaves.
    #[cfg(test)]
    pub(crate) fn unstarted_for_test(engine: SharedSearchEngine) -> Arc<Self> {
        Self::new(
            engine,
            super::OwnerStop::default(),
            Arc::new(Mutex::new(OverlayWarmupState::Pending)),
            Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            WorkspaceLease::unmanaged(),
            Duration::from_secs(600),
        )
    }

    /// Put the driver where an operation error leaves it: waiting for a fresh signal.
    #[cfg(test)]
    pub(crate) fn fail_for_test(&self) {
        self.state.lock().unwrap().failed = true;
    }

    #[cfg(test)]
    pub(crate) fn is_failed(&self) -> bool {
        self.state.lock().unwrap().failed
    }

    /// Completed passes so far — the single-flight proof in tests.
    #[cfg(test)]
    pub(crate) fn pass_count(&self) -> usize {
        self.passes.load(Ordering::SeqCst)
    }

    fn run(self: Arc<Self>) {
        tracing::debug!(
            publish_retry_budget_secs = self.publish_retry_budget.as_secs(),
            "overlay retry worker started"
        );
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        loop {
            if self.stop.is_stopped() {
                return;
            }
            if state.disarmed || state.failed {
                // Bounded even so: the notification above cannot be lost, and a wait that
                // answers only to a notify is one missed wake-up away from lasting for ever.
                let (next, _) = match self.wake.wait_timeout(state, TICK) {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                state = next;
                continue;
            }
            let now = Instant::now();
            if now < state.next_allowed {
                let delay = state.next_allowed - now;
                let (next, _) = match self.wake.wait_timeout(state, delay) {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                state = next;
                continue;
            }
            // The signals are read with the state lock released: they take the engine, and a
            // request's `coalesce` must never wait behind that. What changed meanwhile is
            // re-read below, before any sleep.
            let seen = (state.fresh_epoch, state.obligation, state.failed, state.disarmed);
            state.look_again = false;
            drop(state);
            let due = self.should_run(seen.1);
            state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            if (state.fresh_epoch, state.obligation, state.failed, state.disarmed) != seen {
                continue;
            }
            if !due {
                if self.lease.is_superseded() {
                    self.disarm_superseded(&mut state);
                    continue;
                }
                #[cfg(test)]
                if let Some(hook) = self.before_idle_wait.lock().unwrap().take() {
                    hook();
                }
                if std::mem::take(&mut state.look_again) {
                    continue;
                }
                // The stop, read here and not only on the due branch. `should_run` above takes
                // the engine with this lock RELEASED, and a stop landing in that window has
                // already run its waker: the notify it sent found no waiter, and without this
                // read the worker would go on to sleep out the whole tick after being asked to
                // leave. Read while the lock is held, so a stop arriving from here on cannot
                // slip between the read and the wait.
                if self.stop.is_stopped() {
                    return;
                }
                let (next, _) = match self.wake.wait_timeout(state, TICK) {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                state = next;
                continue;
            }
            if self.stop.is_stopped() {
                return;
            }
            // Run the pass with the state lock RELEASED: a pass embeds for minutes, and
            // kick/disarm/stop must stay responsive meanwhile.
            let epoch_before = state.fresh_epoch;
            drop(state);
            let mut outcome = self.run_pass();
            // A stop seen anywhere inside the pass is the answer, whatever shape the refusal
            // took on the way out: a pause cut short by the stop comes back as a transient
            // refusal, and a publication abandoned at the stop looks like lost ownership.
            // Neither is true, and settling either would write a backoff — or a terminal
            // disarm — into a driver that is leaving.
            if self.stop.is_stopped() {
                outcome = super::WorkspaceSearchApply::Stopping;
            }
            self.passes.fetch_add(1, Ordering::SeqCst);
            state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            self.settle_outcome(&mut state, outcome, epoch_before);
        }
    }

    /// Whether a pass is due: an obligation carried by the driver itself, or any cache
    /// signal. An unpublished engine reads as "due" — the pass classifies it (a transient
    /// `Skipped` before the async init publishes, a backoff-paced retry after).
    fn should_run(&self, obligation: bool) -> bool {
        if self.lease.is_superseded() {
            return false;
        }
        if obligation {
            return true;
        }
        let signals = match self.engine.acquire_for_owner(&self.stop) {
            Ok(guard) => match guard.as_ref() {
                Some(engine) => engine.workspace_overlay_retry_signals().ok(),
                None => None,
            },
            Err(_) => None,
        };
        signals.map(|signals| signals.demands_a_pass()).unwrap_or(true)
    }

    /// Whether an overlay pass is running right now.
    ///
    /// A pass that is merely due — parked on its backoff between attempts — is deliberately
    /// not "running": nothing is being computed, and holding a whole backend process through
    /// a growing backoff is the very unbounded hold this signal exists to avoid.
    pub(super) fn pass_active(&self) -> bool {
        self.pass_active.load(Ordering::SeqCst)
    }

    /// Passes STARTED. Together with [`Self::pass_count`] (passes finished) and
    /// [`Self::pass_active`] it tells a pass parked in its backoff — started, not finished,
    /// not working — from one that has not begun, which reads the same from outside.
    #[cfg(test)]
    pub(super) fn passes_started(&self) -> usize {
        self.passes_started.load(Ordering::SeqCst)
    }

    fn set_runtime_status(&self, status: SemanticRuntimeStatus) {
        if let Ok(mut runtime) = self.semantic_runtime.lock() {
            // The main embedding pass owns its failure until its next attempt. Overlay work
            // keeps its own failure in overlay_warmup and cannot clear the main owner's cause.
            if runtime.embedding_failure().is_none() {
                *runtime = status;
            }
        }
    }

    fn run_pass(&self) -> super::WorkspaceSearchApply<OverlayWarmupState, String> {
        #[cfg(test)]
        self.passes_started.fetch_add(1, Ordering::SeqCst);
        let _active = PassActive::raise(&self.pass_active);
        super::SharedState::set_overlay_warmup_state(
            &self.overlay_warmup,
            OverlayWarmupState::Pending,
        );
        // The syncing status is shown only for a pass that can actually reach the engine;
        // flipping it while the engine is absent would mask a terminal init `Failed`.
        let engine_present = self
            .engine
            .acquire_for_owner(&self.stop)
            .map(|guard| guard.as_ref().is_some())
            .unwrap_or(false);
        if engine_present && self.lease.is_superseded() {
            self.set_runtime_status(SemanticRuntimeStatus::Failed(
                "workspace cache ownership was superseded; reconnect to use the new daemon"
                    .to_owned(),
            ));
        } else if engine_present {
            self.set_runtime_status(SemanticRuntimeStatus::OverlaySyncing);
        }
        let stop = self.stop.clone();
        let mut publish_retry =
            RetryWindow::with_budget(RetryOwner::OverlayEmbedding, self.publish_retry_budget);
        let mut budget_exhausted = false;
        let pass_active = &self.pass_active;
        let mut outcome = super::SharedState::run_overlay_warmup(
            &self.engine,
            &self.stop,
            &self.overlay_warmup,
            &self.lease,
            &|| !stop.is_stopped(),
            &mut || {
                let now = Instant::now();
                let bounded_delay = retry_delay(publish_retry.streak());
                let delay = match publish_retry.refused(now, bounded_delay) {
                    RetryDecision::RetryAfter(delay) => delay,
                    RetryDecision::Stop(_) => {
                        budget_exhausted = true;
                        return false;
                    }
                };
                // Waiting is not working: the pause is lowered out of the pass so the backend
                // is free to go idle, and raised again for the attempt that follows. Held
                // across the pause, this flag keeps a whole process alive for half an hour.
                pass_active.store(false, Ordering::SeqCst);
                let stopped = stop.sleep(delay);
                pass_active.store(true, Ordering::SeqCst);
                if stopped {
                    // Told to go: not a refusal to be retried, and nothing more to publish.
                    return false;
                }
                if publish_retry.expired(Instant::now()) {
                    budget_exhausted = true;
                    false
                } else {
                    true
                }
            },
        );
        if budget_exhausted && matches!(outcome, super::WorkspaceSearchApply::TransientRefusal) {
            #[cfg(test)]
            if let Some(hook) =
                BUDGET_EXHAUSTED_HOOK.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
            {
                hook();
            }
            outcome = super::WorkspaceSearchApply::OperationError(
                "workspace embedding publication retry budget exhausted".to_owned(),
            );
        }
        #[cfg(test)]
        if let Some(hook) = RUN_PASS_FINISH_HOOK.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
        {
            hook();
        }
        if engine_present {
            match &outcome {
                super::WorkspaceSearchApply::Applied(_) => {
                    self.set_runtime_status(SemanticRuntimeStatus::Ready);
                }
                // Nothing to say about a pass that was asked to stop: the status it would
                // write is about work nobody is doing any more.
                super::WorkspaceSearchApply::TransientRefusal
                | super::WorkspaceSearchApply::Stopping => {}
                super::WorkspaceSearchApply::OperationError(error) => {
                    self.set_runtime_status(SemanticRuntimeStatus::Failed(format!(
                        "workspace overlay embedding failed: {error}"
                    )));
                }
                super::WorkspaceSearchApply::Superseded => {
                    self.set_runtime_status(SemanticRuntimeStatus::Failed(
                        "workspace cache ownership was superseded; reconnect to use the new daemon"
                            .to_owned(),
                    ));
                }
                super::WorkspaceSearchApply::Released => {}
            }
        }
        outcome
    }

    fn settle_outcome(
        &self,
        state: &mut RetryState,
        outcome: super::WorkspaceSearchApply<OverlayWarmupState, String>,
        epoch_before: u64,
    ) {
        self.settle_outcome_at(state, outcome, epoch_before, Instant::now());
    }

    fn settle_outcome_at(
        &self,
        state: &mut RetryState,
        outcome: super::WorkspaceSearchApply<OverlayWarmupState, String>,
        epoch_before: u64,
        now: Instant,
    ) {
        if self.lease.is_superseded() {
            self.disarm_superseded(state);
            return;
        }
        match outcome {
            super::WorkspaceSearchApply::Applied(
                OverlayWarmupState::NoLocalDiffs | OverlayWarmupState::Synced { .. },
            ) => {
                state.streak = 0;
                state.obligation = false;
                state.next_allowed = now;
            }
            super::WorkspaceSearchApply::Applied(
                OverlayWarmupState::Incomplete { .. } | OverlayWarmupState::Superseded,
            ) => {
                state.streak = state.streak.saturating_add(1);
                state.obligation = true;
                state.next_allowed = now + retry_delay(state.streak);
            }
            // A stop is answered by leaving, and the worker's loop does that at its next
            // look. The obligation stands untouched: nothing about the work has changed.
            //
            // But `Stopping` also comes from an engine CLOSED before the owners were told to
            // go — the shutdown order does not promise otherwise — and then the loop's own
            // stop check is still false. Left as a bare no-op the driver spun: the outcome
            // moved nothing, so the next turn asked again immediately, and the pass count
            // climbed with no pause until the stop finally arrived. Pacing it costs a live
            // driver nothing, because a real stop leaves before the delay matters.
            super::WorkspaceSearchApply::Stopping => {
                state.next_allowed = now + retry_delay(state.streak.saturating_add(1));
            }
            super::WorkspaceSearchApply::TransientRefusal => {
                state.streak = state.streak.saturating_add(1);
                state.obligation = true;
                state.next_allowed = now + retry_delay(state.streak);
            }
            super::WorkspaceSearchApply::OperationError(error) => {
                state.failed = true;
                state.obligation = false;
                self.set_runtime_status(SemanticRuntimeStatus::Failed(format!(
                    "workspace overlay embedding failed: {error}"
                )));
            }
            super::WorkspaceSearchApply::Superseded
            | super::WorkspaceSearchApply::Released
            | super::WorkspaceSearchApply::Applied(
                OverlayWarmupState::Pending
                | OverlayWarmupState::Skipped(_)
                | OverlayWarmupState::Failed(_)
                | OverlayWarmupState::EmbeddingFailed(_),
            ) => {
                state.disarmed = true;
                state.obligation = false;
            }
        }
        // A fresh kick landed WHILE the pass ran: its notification found no waiter, and the
        // settling above would bury it — under a backoff, or under the failure this very
        // pass ended in. The outcome predates the fresh fact either way, so the fact is
        // honoured here exactly as `kick_fresh` would have honoured it had the worker been
        // waiting: a failure that a later edit already answered is not a failure to wait on,
        // and dropping the fact would leave the driver disarmed until some OTHER file
        // changes.
        if state.fresh_epoch != epoch_before {
            if state.failed {
                state.failed = false;
                state.obligation = true;
            }
            state.streak = 0;
            state.next_allowed = now;
        }
    }

    fn disarm_superseded(&self, state: &mut RetryState) {
        state.disarmed = true;
        state.obligation = false;
        self.set_runtime_status(SemanticRuntimeStatus::Failed(
            "workspace cache ownership was superseded; reconnect to use the new daemon".to_owned(),
        ));
    }
}

#[cfg(test)]
mod every_wait_reads_the_stop {
    /// Every branch of the worker loop reads the stop before it sleeps.
    ///
    /// Counted rather than tested behaviourally, and the reason is worth stating: the harm of
    /// a missing read is a LOST notification — the stop's waker takes the state lock while the
    /// worker holds none (it is off asking the engine whether there is work), notifies an empty
    /// room and is done; the worker then re-takes the lock and sleeps out the whole `TICK` on a
    /// wake-up already delivered. Reproducing that from a test means scheduling the waker to
    /// complete inside a window the test cannot observe the worker entering, and a test that
    /// cannot schedule it passes whether or not the read is there — which is exactly what the
    /// first attempt at one did. What CAN be checked exactly is the property the fix is: no
    /// branch waits without having read the stop since the last one did.
    ///
    /// The needles are assembled at run time: spelled out, they would match this gate's own
    /// source and pass for the wrong reason.
    #[test]
    fn no_branch_of_the_loop_sleeps_without_reading_the_stop() {
        let source = include_str!("overlay_retry.rs");
        // A CRLF checkout (core.autocrlf on Windows, no .gitattributes pinning LF) gives this
        // file "\r\n" endings, and a needle anchored on "\n" would then match nothing — the
        // gate would fail on the line endings rather than on the code, in the very CI step that
        // runs it by name. Normalised first, so the gate is about the source and not the
        // checkout.
        let source = &source.replace("\r\n", "\n");
        let cut = ["\n#[cfg(test)]\n", "mod every_wait_reads_the_stop {"].concat();
        assert_eq!(
            source.matches(&cut).count(),
            1,
            "the production/test cut moved; this gate scans only what it can prove it scanned",
        );
        let production = source.split(&cut).next().unwrap_or(source);
        let start = production.find("fn run(self: Arc<Self>)").expect("the worker loop");
        let loop_body = &production[start..];
        let sleeps = ["self.wake", ".wait_timeout("].concat();
        let reads = ["self.stop", ".is_stopped()"].concat();

        // Only a wait reached AFTER the lock was let go needs its own read. While the lock is
        // held from the top of the loop to the wait, the stop's waker cannot get in front of
        // the waiter — it blocks on that very lock — so the read at the top still covers it.
        // The branch that does let go is the one that asks the engine whether there is work,
        // and it is the one the lost notification belongs to.
        let releases = ["drop(", "state);"].concat();
        let waits = loop_body.match_indices(&sleeps).count();
        assert!(waits >= 3, "the loop has fewer waits than it had; the needle must have moved");
        assert!(loop_body.contains(&releases), "the loop no longer lets the lock go; recheck this");
        for (at, _) in loop_body.match_indices(&sleeps) {
            let from = loop_body[..at].rfind(&sleeps).map_or(0, |prev| prev + sleeps.len());
            let since = &loop_body[from..at];
            let Some(released) = since.rfind(&releases) else { continue };
            assert!(
                since[released..].contains(&reads),
                "the worker lets the state lock go and then sleeps without reading the stop. A \
                 stop delivered in that window runs its waker to completion against nobody: the \
                 notification is lost, and the worker sleeps out the whole tick after being \
                 asked to leave:\n{}",
                &since[released..],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        env_lock, mock_embedding_env, mock_semantic_config, spawn_mock_embedding_server,
    };
    use super::*;
    use bsl_search::SearchEngine;
    use tempfile::tempdir;

    /// The shared hang watchdog's bound under `timing_contract`, in the milliseconds
    /// [`wait_for`] counts.
    const CEILING_MS: u64 = 60_000;

    #[test]
    fn payload_lifecycle_overlay_failure_and_retry_preserve_the_main_owner() {
        let _lock = env_lock();
        let server = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let dir = tempdir().unwrap();
        let path = dir.path().join("Module.bsl");
        std::fs::write(
            &path,
            format!("Процедура Большая()\nЗначение = \"{}\";\nКонецПроцедуры", "x".repeat(8192)),
        )
        .unwrap();
        let mut config = mock_semantic_config(&server);
        config.embedder.max_request_bytes = 2048;
        let mut engine = SearchEngine::new(&dir.path().join("search.db"), config).unwrap();
        engine.set_workspace_root(dir.path());
        let retry = unstarted_driver_over(
            crate::state::shared_engine(Some(engine)),
            WorkspaceLease::unmanaged(),
        );
        let main_failure = bsl_search::EmbeddingFailure::new(
            bsl_search::EmbeddingFailureCode::EmbeddingTransportError,
        );
        *retry.semantic_runtime.lock().unwrap() =
            SemanticRuntimeStatus::EmbeddingFailed(main_failure);
        let failed = retry.run_pass();
        assert!(matches!(failed, super::super::WorkspaceSearchApply::OperationError(_)));
        let overlay_failure = retry
            .overlay_warmup
            .lock()
            .unwrap()
            .embedding_failure()
            .expect("the actual prime failure reaches the overlay owner");
        assert_eq!(overlay_failure.code, bsl_search::EmbeddingFailureCode::EmbeddingInputTooLarge);
        assert_eq!(retry.semantic_runtime.lock().unwrap().embedding_failure(), Some(main_failure));

        retry.settle_outcome_at(&mut state(), failed, 0, Instant::now());
        assert_eq!(retry.semantic_runtime.lock().unwrap().embedding_failure(), Some(main_failure));
        super::super::SharedState::set_semantic_runtime_status(
            &retry.semantic_runtime,
            SemanticRuntimeStatus::Ready,
        );
        assert_eq!(retry.overlay_warmup.lock().unwrap().embedding_failure(), Some(overlay_failure));

        *retry.semantic_runtime.lock().unwrap() =
            SemanticRuntimeStatus::EmbeddingFailed(main_failure);
        std::fs::write(&path, "Процедура Малая()\nКонецПроцедуры").unwrap();
        let guard = retry.engine.lock().unwrap();
        let next = Arc::clone(&retry);
        let attempt = std::thread::spawn(move || next.run_pass());
        let cleared_at_start = wait_for(1000, || {
            matches!(*retry.overlay_warmup.lock().unwrap(), OverlayWarmupState::Pending)
        });
        drop(guard);
        let recovered = attempt.join().unwrap();
        assert!(cleared_at_start, "an admitted attempt clears only its own previous failure");
        assert!(matches!(recovered, super::super::WorkspaceSearchApply::Applied(_)));
        assert!(retry.overlay_warmup.lock().unwrap().embedding_failure().is_none());
        assert_eq!(retry.semantic_runtime.lock().unwrap().embedding_failure(), Some(main_failure));
    }

    /// The schedule itself: no delay after a clean pass, exponential growth from one tick,
    /// a hard cap — apart from the thread, so the shape is provable.
    #[test]
    fn the_retry_delay_grows_exponentially_to_a_cap() {
        assert_eq!(retry_delay(0), Duration::ZERO);
        assert_eq!(retry_delay(1), Duration::from_secs(30));
        assert_eq!(retry_delay(2), Duration::from_secs(60));
        assert_eq!(retry_delay(3), Duration::from_secs(120));
        assert_eq!(retry_delay(6), Duration::from_secs(960));
        assert_eq!(retry_delay(7), Duration::from_secs(960), "the exponent is clamped");
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(960));
    }

    /// A request may wake the driver; it may not tell it that the world changed. Bumping the
    /// fresh epoch would do the latter, because a moved epoch clears the backoff of the pass
    /// that was running.
    #[test]
    fn a_request_coalesce_does_not_erase_the_backoff_of_a_running_pass() {
        let dummy = unstarted_driver_over(
            crate::state::shared_engine(None),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        );
        let mut state = state();
        let epoch_before = state.fresh_epoch;
        dummy.coalesce();
        assert_eq!(
            dummy.state.lock().unwrap().fresh_epoch,
            epoch_before,
            "coalesce presented itself as a new external fact",
        );

        state.fresh_epoch = epoch_before;
        let at = Instant::now();
        dummy.settle_outcome(
            &mut state,
            super::super::WorkspaceSearchApply::TransientRefusal,
            epoch_before,
        );
        assert_eq!(state.streak, 1, "the backoff of a refused pass was cleared");
        assert!(state.next_allowed >= at + retry_delay(1), "the pass may restart immediately");
    }

    #[test]
    fn publish_retry_window_keeps_one_deadline() {
        let start = Instant::now();
        let mut retry =
            RetryWindow::with_budget(RetryOwner::OverlayEmbedding, Duration::from_secs(10));
        let delay = retry_delay(retry.streak());
        assert_eq!(retry.refused(start, delay), RetryDecision::RetryAfter(Duration::ZERO));
        let delay = retry_delay(retry.streak());
        assert_eq!(
            retry.refused(start + Duration::from_secs(9), delay),
            RetryDecision::RetryAfter(Duration::from_secs(1))
        );
        assert!(retry.expired(start + Duration::from_secs(10)));
        assert!(matches!(
            retry.refused(start + Duration::from_secs(10), Duration::ZERO),
            RetryDecision::Stop(_)
        ));

        assert!(
            retry.observe_external_work(Instant::now(), true),
            "a later external pass gets a fresh budget"
        );
    }

    fn state() -> RetryState {
        RetryState {
            fresh_epoch: 0,
            streak: 0,
            next_allowed: Instant::now(),
            obligation: false,
            failed: false,
            disarmed: false,
            look_again: false,
        }
    }

    fn driver_over(engine: SharedSearchEngine, lease: WorkspaceLease) -> Arc<OverlayRetry> {
        let retry = unstarted_driver_over(engine, lease);
        retry.start();
        retry
    }

    /// The same driver with its worker not yet running, so a test can settle `stop` before
    /// the worker exists to read it.
    fn unstarted_driver_over(
        engine: SharedSearchEngine,
        lease: WorkspaceLease,
    ) -> Arc<OverlayRetry> {
        OverlayRetry::new(
            engine,
            super::super::OwnerStop::default(),
            Arc::new(Mutex::new(OverlayWarmupState::Pending)),
            Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            lease,
            super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        )
    }

    /// The signal the backend's lifetime is read from has to be raised by the pass itself and
    /// gone when the pass is. Observed from INSIDE the pass: asserting it from outside after
    /// the fact would hold on a flag nobody ever raised.
    #[test]
    fn a_running_pass_is_visible_as_background_work() {
        struct ResetHook;
        impl Drop for ResetHook {
            fn drop(&mut self) {
                *RUN_PASS_FINISH_HOOK.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
        }
        let _lock = env_lock();
        let _reset = ResetHook;

        let engine: SharedSearchEngine = crate::state::shared_engine(None);
        let retry = unstarted_driver_over(engine, WorkspaceLease::unmanaged());
        assert!(!retry.pass_active(), "a driver that never ran a pass reads as running one");

        let seen = Arc::new(AtomicBool::new(false));
        let seen_in_hook = Arc::clone(&seen);
        let retry_in_hook = Arc::clone(&retry);
        *RUN_PASS_FINISH_HOOK.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(Box::new(move || {
                seen_in_hook.store(retry_in_hook.pass_active(), Ordering::SeqCst);
            }));

        retry.run_pass();

        assert!(seen.load(Ordering::SeqCst), "the pass never marked itself running");
        assert!(!retry.pass_active(), "the pass stayed marked running after it ended");
    }

    /// The worker is the only writer of `OverlaySyncing`. Once it is gone nobody settles
    /// that status again, and the broker reads a pass that is still running — holding the
    /// backend process for the rest of its life.
    #[test]
    fn a_departed_worker_settles_the_syncing_status() {
        let engine: SharedSearchEngine = crate::state::shared_engine(None);
        let retry = unstarted_driver_over(engine, WorkspaceLease::unmanaged());
        *retry.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::OverlaySyncing;

        drop(SyncingVerdict(Arc::clone(&retry)));

        assert!(
            matches!(&*retry.semantic_runtime.lock().unwrap(), SemanticRuntimeStatus::Failed(_)),
            "a departed worker left the status claiming to sync"
        );
    }

    /// The other half of the same contract, and the control on the test above: a verdict
    /// that was already written stands, so the guard cannot be a blanket overwrite that
    /// would pass the check above no matter what the worker had settled.
    #[test]
    fn a_settled_status_survives_the_departing_worker() {
        let engine: SharedSearchEngine = crate::state::shared_engine(None);
        let retry = unstarted_driver_over(engine, WorkspaceLease::unmanaged());
        *retry.semantic_runtime.lock().unwrap() = SemanticRuntimeStatus::Ready;

        drop(SyncingVerdict(Arc::clone(&retry)));

        assert!(matches!(&*retry.semantic_runtime.lock().unwrap(), SemanticRuntimeStatus::Ready));
    }

    /// Polls `check` until it holds; `deadline_ms` bounds the wait only under `timing_contract`.
    fn wait_for(deadline_ms: u64, check: impl FnMut() -> bool) -> bool {
        test_utils::wait::poll_until(
            Duration::from_millis(deadline_ms),
            Duration::from_millis(10),
            check,
        )
    }

    /// A stop that lands while the driver is asking whether it has work leaves it at once,
    /// not thirty seconds later.
    ///
    /// That question takes the ENGINE, and the driver releases the state lock to ask it — a
    /// request's `coalesce` must never queue behind the engine. So the stop's waker finds the
    /// state lock free, takes it, notifies nobody, and is done. The driver then re-takes the
    /// lock, finds nothing to do, and — before this was fixed — went to sleep on a notification
    /// that had already been delivered to an empty room, for the whole `TICK`. The wake was not
    /// lost for want of a notify; it was lost because nothing read the stop on this branch.
    ///
    /// The engine is held here for exactly that window, which is what makes the race a
    /// schedule rather than a hope.
    #[test]
    fn a_stop_while_the_driver_is_asking_the_engine_is_not_slept_off() {
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        engine.enable_workspace_watcher_mode();
        engine.initialize_workspace_overlay_clean().unwrap();
        let shared: SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let retry = unstarted_driver_over(Arc::clone(&shared), WorkspaceLease::unmanaged());
        let stop = retry.stop.clone();

        // The engine is held, so the driver's question blocks inside `should_run` with the
        // state lock released — the window the stop has to be answered in.
        let held = shared.lock().unwrap();
        retry.start();
        assert!(wait_for(3_000, || stop.live() >= 1), "the driver never started");
        // Gives the driver time to block on the held engine. A stop that comes before it gets
        // there is read on the way in and proves nothing, but fails nothing either; the pause
        // that makes the race likely is the timing profile's.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(200));
        }

        // The waker runs to completion here: it takes the free state lock, notifies an empty
        // room and returns. Nothing is waiting to be woken.
        stop.stop();
        drop(held);

        assert!(
            wait_for(5_000, || stop.live() == 0),
            "the driver slept off a stop it was told about while it had the engine; \
             it must leave without waiting out the idle tick",
        );
    }

    /// A batch the backlog owner settled can leave the driver work to do — entries without
    /// vectors. Its wake reaches a driver that is between deciding it has nothing to do and
    /// going to sleep, instead of being lost to it until the next idle tick.
    #[test]
    fn a_wake_between_the_check_and_the_sleep_is_not_lost() {
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        engine.enable_workspace_watcher_mode();
        engine.initialize_workspace_overlay_clean().unwrap();
        let shared: SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let retry = unstarted_driver_over(Arc::clone(&shared), WorkspaceLease::unmanaged());
        {
            let (engine, driver) = (Arc::clone(&shared), Arc::clone(&retry));
            let file = workspace.join("New.bsl");
            *retry.before_idle_wait.lock().unwrap() = Some(Box::new(move || {
                std::fs::write(&file, "Процедура Новая()\nКонецПроцедуры").unwrap();
                {
                    // An entry settled without its vectors: the driver's work, not a mark.
                    let guard = engine.lock().unwrap();
                    let engine = guard.as_ref().unwrap();
                    engine.mark_workspace_path_dirty(&file).unwrap();
                    engine.workspace_overlay_stats().unwrap();
                    assert!(engine.workspace_overlay_retry_signals().unwrap().demands_a_pass());
                }
                std::thread::spawn(move || driver.coalesce());
                // Room for the wake to land before the worker goes to sleep.
                std::thread::sleep(Duration::from_millis(100));
            }));
        }
        retry.start();
        assert!(wait_for(3_000, || retry.pass_count() >= 1), "the wake was lost to the idle tick");
        retry.stop();
    }

    /// The outcome classification: clean resets, incomplete/superseded keeps the obligation,
    /// transient retries, operation error waits for a fresh signal, and terminal disarms.
    #[test]
    fn outcomes_classify_into_retry_reset_and_disarm() {
        let dummy = driver_over(
            crate::state::shared_engine(None),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        );
        dummy.stop();

        let mut s = state();
        s.streak = 3;
        s.obligation = true;
        dummy.settle_outcome(
            &mut s,
            super::super::WorkspaceSearchApply::Applied(OverlayWarmupState::NoLocalDiffs),
            0,
        );
        assert_eq!((s.streak, s.obligation, s.disarmed), (0, false, false));

        let mut s = state();
        dummy.settle_outcome(
            &mut s,
            super::super::WorkspaceSearchApply::Applied(OverlayWarmupState::Incomplete {
                unreadable: 0,
                canonical_fallbacks: 0,
                read_failures: 0,
                persist_failed: true,
            }),
            0,
        );
        assert_eq!((s.streak, s.obligation, s.disarmed), (1, true, false));

        let mut s = state();
        dummy.settle_outcome(
            &mut s,
            super::super::WorkspaceSearchApply::Applied(OverlayWarmupState::Superseded),
            0,
        );
        assert_eq!((s.streak, s.obligation, s.disarmed), (1, true, false));

        let mut s = state();
        dummy.settle_outcome(
            &mut s,
            super::super::WorkspaceSearchApply::OperationError("x".to_owned()),
            0,
        );
        assert_eq!((s.obligation, s.failed, s.disarmed), (false, true, false));

        let mut s = state();
        dummy.settle_outcome(&mut s, super::super::WorkspaceSearchApply::TransientRefusal, 0);
        assert_eq!((s.streak, s.obligation, s.disarmed), (1, true, false), "transient skip");

        let mut s = state();
        dummy.settle_outcome(&mut s, super::super::WorkspaceSearchApply::Released, 0);
        assert!(s.disarmed, "terminal skip disarms");
    }

    /// A fresh kick that lands WHILE a pass runs must not be buried by that pass's stale
    /// outcome: the settle sees the epoch moved and schedules the next check immediately.
    #[test]
    fn a_fresh_kick_during_a_pass_overrides_the_stale_backoff() {
        let dummy = driver_over(
            crate::state::shared_engine(None),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        );
        dummy.stop();
        let mut s = state();
        // The pass captured epoch 4; a kick_fresh bumped it to 5 mid-pass.
        s.fresh_epoch = 5;
        dummy.settle_outcome(
            &mut s,
            super::super::WorkspaceSearchApply::Applied(OverlayWarmupState::Incomplete {
                unreadable: 1,
                canonical_fallbacks: 0,
                read_failures: 0,
                persist_failed: false,
            }),
            4,
        );
        assert_eq!(s.streak, 0, "a fresh fact bypasses the stale incomplete-pass backoff");
        assert!(s.obligation, "the obligation itself stands");
    }

    #[test]
    fn store_and_network_errors_wait_for_a_fresh_signal() {
        let dummy = driver_over(
            crate::state::shared_engine(None),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        );
        dummy.stop();

        for error in ["store write failed", "network request failed"] {
            let mut s = state();
            dummy.settle_outcome_at(
                &mut s,
                super::super::WorkspaceSearchApply::OperationError(error.to_owned()),
                0,
                Instant::now(),
            );
            assert!(s.failed);
            assert!(!s.obligation, "an operation error schedules no self-repeat");

            *dummy.state.lock().unwrap() = s;
            dummy.kick_fresh();
            let rearmed = std::mem::replace(&mut *dummy.state.lock().unwrap(), state());
            assert!(!rearmed.failed);
            assert!(rearmed.obligation, "only the fresh signal rearms {error}");
            assert_eq!(rearmed.streak, 0);
        }
    }

    /// The fresh fact that arrives WHILE a pass runs is not lost when that pass then fails.
    /// Its notification found no waiter, so nothing but the settling can honour it — and a
    /// failure that a later edit has already answered is not a failure to sit out. Dropped,
    /// it leaves the driver disarmed until some OTHER file changes, with the overlay out of
    /// date and the semantic status reading failed.
    #[test]
    fn a_fresh_kick_during_a_failing_pass_rearms_the_driver() {
        let dummy = driver_over(
            crate::state::shared_engine(None),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        );
        dummy.stop();

        let mut s = state();
        // The pass captured epoch 4; a kick_fresh bumped it to 5 while it ran.
        s.fresh_epoch = 5;
        dummy.settle_outcome_at(
            &mut s,
            super::super::WorkspaceSearchApply::OperationError("store write failed".to_owned()),
            4,
            Instant::now(),
        );

        assert!(!s.failed, "the fact that arrived mid-pass was buried under the failure");
        assert!(s.obligation, "the work that fact asked for is owed");
        assert_eq!(s.streak, 0);

        // Control: with no fact arriving mid-pass, the failure stands and waits for one.
        let mut quiet = state();
        quiet.fresh_epoch = 4;
        dummy.settle_outcome_at(
            &mut quiet,
            super::super::WorkspaceSearchApply::OperationError("store write failed".to_owned()),
            4,
            Instant::now(),
        );
        assert!(quiet.failed, "an operation error with no fresh fact still waits for one");
        assert!(!quiet.obligation);
    }

    /// A budget spent is a failure that waits for a fresh fact — but facts that arrived while
    /// the pass was failing are exactly that, and they buy one more attempt between them, not
    /// one each.
    #[test]
    fn transient_budget_fails_once_answers_mid_pass_facts_and_rearms_on_new_drift() {
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        std::fs::write(workspace.join("New.bsl"), "Процедура Новая()\nКонецПроцедуры").unwrap();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine = crate::state::shared_engine(Some(engine));
        let runtime = Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled));
        let (expired_tx, expired_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();

        struct ResetHooks;
        impl Drop for ResetHooks {
            fn drop(&mut self) {
                super::super::embed::FORCE_OVERLAY_PUBLICATION_REFUSALS.store(0, Ordering::SeqCst);
                *BUDGET_EXHAUSTED_HOOK.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
        }
        let _reset = ResetHooks;
        super::super::embed::FORCE_OVERLAY_PUBLICATION_REFUSALS.store(usize::MAX, Ordering::SeqCst);
        // The hook survives a second exhaustion: the kicks below arrive while the first pass
        // is parked in it, and facts that arrive mid-pass are answered by one more pass.
        let expired_tx = Mutex::new(expired_tx);
        let resume_rx = Mutex::new(Some(resume_rx));
        *BUDGET_EXHAUSTED_HOOK.lock().unwrap() = Some(Box::new(move || {
            let _ = expired_tx.lock().unwrap().send(());
            if let Some(resume) = resume_rx.lock().unwrap().take() {
                resume.recv().unwrap();
            }
        }));

        let retry = OverlayRetry::spawn(
            Arc::clone(&engine),
            super::super::OwnerStop::default(),
            Arc::new(Mutex::new(OverlayWarmupState::Pending)),
            Arc::clone(&runtime),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            Duration::from_millis(100),
        );
        expired_rx.recv_timeout(crate::change_hub::test_support::STATE_CEILING).unwrap();
        // Eight facts arrive while that pass is failing. They are answered by ONE more pass —
        // a fact the pass did not see is work nobody else will do — and a storm of them is
        // still one pass, not eight.
        for _ in 0..8 {
            retry.kick_fresh();
        }
        resume_tx.send(()).unwrap();
        assert!(wait_for(5_000, || matches!(
            *runtime.lock().unwrap(),
            SemanticRuntimeStatus::Failed(ref reason) if reason.contains("budget exhausted")
        )));
        assert!(
            wait_for(5_000, || retry.pass_count() >= 2),
            "the facts that arrived mid-pass were never answered"
        );
        let exhausted_passes = retry.pass_count();
        assert_eq!(exhausted_passes, 2, "a storm of mid-pass facts is one pass, not eight");
        // A pass that never comes has no event to wait on; only a quiet window can show it.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(retry.pass_count(), exhausted_passes, "budget failure does not self-repeat");
        }

        super::super::embed::FORCE_OVERLAY_PUBLICATION_REFUSALS.store(1, Ordering::SeqCst);
        retry.kick_fresh();
        assert!(wait_for(5_000, || matches!(
            *runtime.lock().unwrap(),
            SemanticRuntimeStatus::Ready
        )));
        assert!(retry.pass_count() > exhausted_passes, "new drift receives a fresh budget");
        retry.stop();
    }

    /// Concurrent kicks collapse into ONE pass: the worker is the only runner, and after a
    /// clean first pass the condition goes quiet, so no kick storm re-runs it.
    #[test]
    fn concurrent_kicks_collapse_into_one_pass() {
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let retry = driver_over(engine_arc, crate::workspace_lease::WorkspaceLease::unmanaged());
        let kickers: Vec<_> = (0..8)
            .map(|_| {
                let retry = Arc::clone(&retry);
                std::thread::spawn(move || retry.kick_fresh())
            })
            .collect();
        for kicker in kickers {
            kicker.join().unwrap();
        }
        assert!(wait_for(CEILING_MS, || retry.pass_count() >= 1), "the first pass runs");
        // The driver about to sleep with nothing due is the state in which no further pass is
        // scheduled; the wake sends it through that decision once more.
        let (idle_tx, idle_rx) = std::sync::mpsc::channel();
        *retry.before_idle_wait.lock().unwrap() = Some(Box::new(move || {
            let _ = idle_tx.send(());
        }));
        retry.coalesce();
        idle_rx
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the driver never settled with no pass due");
        assert_eq!(retry.pass_count(), 1, "a kick storm still yields one pass");
        retry.stop();
    }

    /// A pass before the engine publishes is a TRANSIENT skip: the obligation survives, and
    /// once the engine appears the next kick completes the startup pass — it was never lost.
    #[test]
    fn a_transient_engine_absence_keeps_the_obligation() {
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let engine_arc: SharedSearchEngine = crate::state::shared_engine(None);
        let retry = driver_over(
            Arc::clone(&engine_arc),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        );
        retry.kick_fresh();
        assert!(wait_for(5_000, || retry.pass_count() >= 1), "the blind pass runs");

        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        *engine_arc.lock().unwrap() = Some(engine);
        retry.kick_fresh();
        assert!(
            wait_for(5_000, || {
                engine_arc
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|engine| {
                        engine
                            .workspace_overlay_retry_signals()
                            .map(|signals| signals.initialized)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            }),
            "the startup pass completes once the engine appears"
        );
        retry.stop();
    }

    /// `stop()` freezes the driver before the lease handover: no pass starts after it.
    ///
    /// The worker is started only once the stop is already in place, because a driver built
    /// by `spawn` opens its own startup pass at once: stopping it afterwards would race that
    /// pass, and the race — not the freeze — would decide the count.
    #[test]
    fn stop_freezes_the_driver() {
        let retry = unstarted_driver_over(
            crate::state::shared_engine(None),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        );
        retry.stop();
        retry.start();
        retry.kick_fresh();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(retry.pass_count(), 0, "a stopped driver runs nothing");
    }

    /// The counterpart that keeps the freeze above from passing vacuously: the very same
    /// driver, started with no stop in place, DOES open a pass. No kick is sent — a kick
    /// would itself demand a pass, and the count could then no longer tell a worker that
    /// opens its startup pass from one that only ever answers kicks.
    #[test]
    fn an_unstopped_driver_opens_its_startup_pass() {
        let retry = unstarted_driver_over(
            crate::state::shared_engine(None),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        );
        retry.start();
        assert!(wait_for(5_000, || retry.pass_count() >= 1), "the startup pass runs");
        retry.stop();
    }

    #[test]
    fn temporary_lock_refusal_remains_retryable() {
        let dir = tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        let (retry, lease) =
            crate::workspace_lease::WorkspaceLease::while_cache_lock_held(&cache, || {
                let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
                let retry = driver_over(crate::state::shared_engine(None), lease.clone());
                retry.kick_fresh();
                assert!(
                    wait_for(CEILING_MS, || retry.pass_count() >= 1),
                    "the workflow owns its admission attempt"
                );
                assert!(!lease.is_superseded(), "lock contention is not terminal");
                (retry, lease)
            });

        retry.kick_fresh();
        assert!(wait_for(CEILING_MS, || retry.pass_count() >= 1), "the unclaimed lease retries");
        assert!(!lease.is_superseded());
        assert!(!retry.state.lock().unwrap().disarmed);
        retry.stop();
    }

    /// A driver parked in a publication backoff leaves on shutdown whatever order the three
    /// middle steps are taken in — stop the owners, close the engine, shut the hub down. The
    /// outcome must not depend on that order: each step releases a different wait, and a
    /// driver whose exit needs them in one particular sequence is one reordering away from
    /// sitting out a half-hour backoff after the daemon has gone.
    ///
    /// Only the clock tells an exit on shutdown from a backoff sat out, so an ordinary run checks
    /// that every order ends every driver, and the timing profile that it ends them at once.
    #[test]
    fn a_paused_driver_leaves_in_every_shutdown_order() {
        use crate::change_hub::WorkspaceChangeHub;

        const STOP_OWNERS: usize = 0;
        const CLOSE_ENGINE: usize = 1;
        const SHUT_THE_HUB: usize = 2;
        const ORDERS: [[usize; 3]; 6] = [
            [STOP_OWNERS, CLOSE_ENGINE, SHUT_THE_HUB],
            [STOP_OWNERS, SHUT_THE_HUB, CLOSE_ENGINE],
            [CLOSE_ENGINE, STOP_OWNERS, SHUT_THE_HUB],
            [CLOSE_ENGINE, SHUT_THE_HUB, STOP_OWNERS],
            [SHUT_THE_HUB, STOP_OWNERS, CLOSE_ENGINE],
            [SHUT_THE_HUB, CLOSE_ENGINE, STOP_OWNERS],
        ];

        struct Daemon {
            stop: super::super::OwnerStop,
            engine: SharedSearchEngine,
            hub: WorkspaceChangeHub,
            retry: Arc<OverlayRetry>,
            hold: (std::thread::JoinHandle<()>, std::sync::mpsc::Sender<()>),
            _dir: tempfile::TempDir,
        }

        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);

        // Everything a daemon needs is built first and the drivers are started only after,
        // so the peer's hold of the lock covers every driver's first attempts. Started as
        // each was built, the drivers would queue behind one another's setup and the earliest
        // would publish before the last had a hold to be refused by.
        let built: Vec<_> = (0..ORDERS.len())
            .map(|_| {
                let dir = tempdir().unwrap();
                let workspace = dir.path();
                std::fs::write(workspace.join("Модуль.bsl"), "Процедура П()\nКонецПроцедуры")
                    .unwrap();
                let cache = crate::cache::WorkspaceCacheLayout::for_workspace(workspace);
                cache.ensure().unwrap();
                let mut engine =
                    SearchEngine::new(&cache.search_db_path(), mock_semantic_config(&mock))
                        .unwrap();
                let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
                engine.set_workspace_roots(roots);
                let engine: SharedSearchEngine = crate::state::shared_engine(Some(engine));
                let stop = super::super::OwnerStop::default();
                let hub = WorkspaceChangeHub::start(vec![workspace.to_path_buf()]);
                let hub_waker = hub.clone();
                stop.wakes(move || hub_waker.interrupt_waiters());
                (dir, cache, engine, stop, hub)
            })
            .collect();

        // The six are parked side by side: each waits out two refusals of two seconds, and
        // one after another that alone would take half a minute.
        let daemons: Vec<Daemon> = built
            .into_iter()
            .map(|(dir, cache, engine, stop, hub)| {
                // Held until the drivers have left, however long the machine takes to get them
                // parked: a hold that ended between the first refusal and the second would let
                // the pass publish and end instead of parking.
                let hold =
                    crate::workspace_lease::WorkspaceLease::hold_cache_lock_until_released(&cache);
                let retry = OverlayRetry::spawn(
                    Arc::clone(&engine),
                    stop.clone(),
                    Arc::new(Mutex::new(OverlayWarmupState::Pending)),
                    Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
                    crate::workspace_lease::WorkspaceLease::claim_cache(&cache),
                    super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
                );
                Daemon { stop, engine, hub, retry, hold, _dir: dir }
            })
            .collect();

        // Parked in the backoff: a pass began, has not ended, and is not working.
        for daemon in &daemons {
            assert!(
                wait_for(CEILING_MS, || daemon.retry.passes_started() >= 1
                    && daemon.retry.pass_count() == 0
                    && !daemon.retry.pass_active()),
                "a driver never reached its backoff (started {}, finished {}, active {})",
                daemon.retry.passes_started(),
                daemon.retry.pass_count(),
                daemon.retry.pass_active(),
            );
        }

        let asked = Instant::now();
        for (daemon, order) in daemons.iter().zip(ORDERS) {
            for step in order {
                match step {
                    STOP_OWNERS => daemon.stop.stop(),
                    CLOSE_ENGINE => daemon.engine.close(),
                    _ => daemon.hub.shutdown(),
                }
            }
        }

        for (daemon, order) in daemons.iter().zip(ORDERS) {
            assert!(
                wait_for(CEILING_MS, || daemon.stop.live() == 0),
                "the driver stayed in its backoff under order {order:?}"
            );
            let owed =
                daemon.engine.lock().unwrap().as_ref().unwrap().workspace_overlay_debt().unwrap();
            assert!(
                !owed.initialized,
                "the driver published an overlay after its daemon stopped, order {order:?}"
            );
        }
        // A driver that sat its backoff out would cost a whole tick; half of one is beyond any
        // slow machine.
        let exits = asked.elapsed();
        if cfg!(timing_contract) {
            assert!(exits < TICK / 2, "the six exits took {exits:?}");
            assert!(exits < Duration::from_secs(2), "the six exits took {exits:?}");
        }
        for daemon in daemons {
            let (holder, release) = daemon.hold;
            release.send(()).expect("the holder still holds the lock");
            holder.join().expect("the holder lets go");
        }
    }

    /// A driver sitting out a publication backoff is waiting, not working: the flag the
    /// backend reads for "a pass is running" is lowered for the pause and raised again for
    /// the attempt after it. Held across the pause, it keeps a whole process alive for the
    /// length of a backoff that grows to half an hour.
    #[test]
    fn a_driver_in_its_retry_pause_is_not_a_running_pass() {
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        std::fs::write(workspace.join("Модуль.bsl"), "Процедура П()\nКонецПроцедуры").unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(workspace);
        cache.ensure().unwrap();
        let mut engine =
            SearchEngine::new(&cache.search_db_path(), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let mine = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);

        // A peer holds the fence's lock for longer than two of its waits: the first refusal
        // backs off by nothing, the second by a whole tick — the pause under test.
        let holder = crate::workspace_lease::WorkspaceLease::hold_cache_lock_for(
            &cache,
            Duration::from_secs(5),
        );
        let retry = OverlayRetry::spawn(
            Arc::clone(&engine_arc),
            super::super::OwnerStop::default(),
            Arc::new(Mutex::new(OverlayWarmupState::Pending)),
            Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            mine,
            super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );

        assert!(
            wait_for(10_000, || retry.pass_active()),
            "no pass ever started, so the pause below would prove nothing"
        );
        assert!(
            wait_for(10_000, || !retry.pass_active()),
            "the paused driver still reads as a running pass"
        );
        assert_eq!(
            retry.pass_count(),
            0,
            "the pass ended instead of pausing; this proves nothing about a pause"
        );
        assert!(
            retry_delay(1) >= Duration::from_secs(5),
            "the pause is short enough that a finished pass would pass for a paused one"
        );

        retry.stop();
        holder.join().unwrap();
    }

    /// Observing a live foreign owner is terminal: releasing that owner and kicking again
    /// cannot start another pass or report the old daemon semantically ready.
    #[test]
    fn observed_supersession_never_resumes() {
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let mine = crate::workspace_lease::WorkspaceLease::claim(workspace);
        let runtime = Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled));
        let retry = OverlayRetry::spawn(
            Arc::clone(&engine_arc),
            super::super::OwnerStop::default(),
            Arc::new(Mutex::new(OverlayWarmupState::Pending)),
            Arc::clone(&runtime),
            mine.clone(),
            super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        assert!(wait_for(5_000, || retry.pass_count() >= 1), "the startup pass runs");

        // A newer daemon takes the workspace; a pending signal appears meanwhile.
        let newer = crate::workspace_lease::WorkspaceLease::claim(workspace);
        std::fs::write(workspace.join("New.bsl"), "Процедура Новая()\nКонецПроцедуры").unwrap();
        engine_arc
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .mark_workspace_path_dirty(workspace.join("New.bsl"))
            .unwrap();
        let before = retry.pass_count();
        // Marks alone are no work of the driver's; a fresh fact after its failure is.
        retry.fail_for_test();
        retry.kick_fresh();
        assert!(wait_for(5_000, || mine.is_superseded()), "the fresh check observes takeover");
        assert_eq!(
            retry.pass_count(),
            before + 1,
            "one typed admission attempt observes the supersession"
        );
        let terminal_passes = retry.pass_count();

        // The newer daemon exits cleanly; the old process remains terminally read-only.
        newer.release();
        retry.kick_fresh();
        // A pass that should never come is an absence. Only the timing profile waits a window
        // out for it; the reading itself holds at any moment.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(300));
        }
        assert_eq!(
            retry.pass_count(),
            terminal_passes,
            "owner release cannot re-arm a terminal driver"
        );
        assert!(matches!(*runtime.lock().unwrap(), SemanticRuntimeStatus::Failed(_)));
        retry.stop();
    }

    #[test]
    fn takeover_during_a_pass_never_reports_ready() {
        struct ResetHook;
        impl Drop for ResetHook {
            fn drop(&mut self) {
                *RUN_PASS_FINISH_HOOK.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
        }
        let _reset = ResetHook;
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(&workspace, &workspace, &[]);
        engine.set_workspace_roots(roots);
        let mine = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        let mine_in_hook = mine.clone();
        let newer = Arc::new(Mutex::new(None));
        let newer_in_hook = Arc::clone(&newer);
        *RUN_PASS_FINISH_HOOK.lock().unwrap() = Some(Box::new(move || {
            *newer_in_hook.lock().unwrap() =
                Some(crate::workspace_lease::WorkspaceLease::claim(&workspace));
            assert!(!mine_in_hook.owns_caches_now());
        }));
        let runtime = Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled));
        let retry = OverlayRetry::spawn(
            crate::state::shared_engine(Some(engine)),
            super::super::OwnerStop::default(),
            Arc::new(Mutex::new(OverlayWarmupState::Pending)),
            Arc::clone(&runtime),
            mine.clone(),
            super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        assert!(wait_for(5_000, || mine.is_superseded()));
        assert!(matches!(*runtime.lock().unwrap(), SemanticRuntimeStatus::Failed(_)));
        assert!(newer.lock().unwrap().is_some());
        retry.stop();
    }

    /// Phase C runs under the REAL fence: with the lease taken over between the phases, the
    /// publish is refused and nothing lands — fingerprint rows would otherwise suppress the
    /// new owner's re-reads after a restart.
    #[test]
    fn a_lost_lease_refuses_the_publish_fence() {
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: SharedSearchEngine = crate::state::shared_engine(Some(engine));
        let overlay_warmup = Arc::new(Mutex::new(OverlayWarmupState::Pending));

        let mine = crate::workspace_lease::WorkspaceLease::claim(workspace);
        let _newer = crate::workspace_lease::WorkspaceLease::claim(workspace);
        let result = super::super::SharedState::run_overlay_warmup(
            &engine_arc,
            &super::super::OwnerStop::default(),
            &overlay_warmup,
            &mine,
            &|| true,
            &mut || false,
        );
        let outcome = overlay_warmup.lock().unwrap().clone();
        assert!(
            matches!(&outcome, OverlayWarmupState::Failed(reason) if reason.contains("ownership")),
            "the fenced publish refuses a superseded daemon, got {outcome:?} ({result:?})"
        );
        let initialized = engine_arc
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_overlay_retry_signals()
            .unwrap()
            .initialized;
        assert!(!initialized, "nothing was published");
    }

    /// The end-to-end catch-up: an incomplete pass (an unreadable subtree) keeps the
    /// obligation, and once the subtree is restored the driver's next pass settles clean —
    /// no restart required.
    #[cfg(unix)]
    #[test]
    fn an_incomplete_pass_is_caught_up_after_the_subtree_returns() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let closed = workspace.join("closed");
        std::fs::create_dir(&closed).unwrap();
        std::fs::write(closed.join("Hidden.bsl"), "Процедура Скрытая()\nКонецПроцедуры").unwrap();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        let engine_arc: SharedSearchEngine = crate::state::shared_engine(Some(engine));

        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&closed).is_ok() {
            // Running as root: permissions cannot hide anything.
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let overlay_warmup = Arc::new(Mutex::new(OverlayWarmupState::Pending));
        let retry = OverlayRetry::spawn(
            Arc::clone(&engine_arc),
            super::super::OwnerStop::default(),
            Arc::clone(&overlay_warmup),
            Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        assert!(wait_for(5_000, || retry.pass_count() >= 1), "the incomplete pass runs");
        assert!(
            matches!(overlay_warmup.lock().unwrap().clone(), OverlayWarmupState::Incomplete { .. }),
            "the first pass is honest about its coverage"
        );

        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        retry.kick_fresh();
        assert!(
            wait_for(10_000, || {
                matches!(overlay_warmup.lock().unwrap().clone(), OverlayWarmupState::Synced { .. })
            }),
            "the returned subtree is caught up without a restart, got {:?}",
            overlay_warmup.lock().unwrap().clone()
        );
        let rescan = engine_arc
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_overlay_needs_full_rescan()
            .unwrap();
        assert!(!rescan, "the clean pass settles the withheld removals");
        retry.stop();
    }

    /// The semantic side has its own durable signal: an entry the ReuseOnly path built
    /// lexical-only (its mark consumed) still drives a pass that attaches the vectors.
    #[test]
    fn a_lexical_only_entry_drives_a_vector_catch_up_pass() {
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(workspace, workspace, &[]);
        engine.set_workspace_roots(roots);
        engine.enable_workspace_watcher_mode();
        let engine_arc: SharedSearchEngine = crate::state::shared_engine(Some(engine));

        let overlay_warmup = Arc::new(Mutex::new(OverlayWarmupState::Pending));
        let retry = OverlayRetry::spawn(
            Arc::clone(&engine_arc),
            super::super::OwnerStop::default(),
            Arc::clone(&overlay_warmup),
            Arc::new(Mutex::new(SemanticRuntimeStatus::Disabled)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
            super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        assert!(wait_for(5_000, || retry.pass_count() >= 1), "the startup pass runs");

        // An edit lands; the interactive ReuseOnly path consumes the mark WITHOUT vectors.
        std::fs::write(workspace.join("New.bsl"), "Процедура Новая()\nКонецПроцедуры").unwrap();
        {
            let guard = engine_arc.lock().unwrap();
            let engine = guard.as_ref().unwrap();
            engine.mark_workspace_path_dirty(workspace.join("New.bsl")).unwrap();
            engine.workspace_overlay_stats().unwrap();
            let signals = engine.workspace_overlay_retry_signals().unwrap();
            assert_eq!(signals.pending_dirty_paths, 0, "the ReuseOnly path consumed the mark");
            assert_eq!(signals.unembedded_entries, 1, "the entry serves lexical-only");
        }
        retry.kick_fresh();
        assert!(
            wait_for(10_000, || {
                engine_arc
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .workspace_overlay_retry_signals()
                    .unwrap()
                    .unembedded_entries
                    == 0
            }),
            "the driver's pass attaches the missing vectors"
        );
        retry.stop();
    }
}
