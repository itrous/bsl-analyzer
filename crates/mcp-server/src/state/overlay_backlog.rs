//! The owner of the workspace overlay's point backlog: the files the watcher marked dirty and
//! nobody has read back yet.
//!
//! One per published engine, in every mode. It answers the marks through the point refresh
//! (see [`bsl_search::PointCapture`]): a moment under the engine mutex to capture a batch, the
//! reading and chunking under no lock at all, and a bounded commit under the mutex and the
//! ownership fence. Vectors are not its business — the embedding driver
//! ([`super::overlay_retry::OverlayRetry`]) owns those, and after each batch this owner only
//! wakes it, without telling it anything new: a batch is the continuation of a fact the
//! consumer already reported.
//!
//! It never sleeps on someone else's timeout while there is work it can do, never lets a wake
//! step around its own backoff, and never takes the interprocess lock for an empty backlog.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::retry_window::{RetryDecision, RetryOwner, RetryWindow};
use super::{OwnerLive, OwnerStop, SharedSearchEngine, SharedState, WorkspaceSearchApply};
use crate::workspace_lease::WorkspaceLease;

/// How long the owner sleeps with nothing to do before it looks again on its own.
const IDLE_TICK: Duration = Duration::from_secs(30);

/// The first wait after SQLite's writer lock refused a commit, doubled on each such refusal
/// in a row up to the ordinary retry tick. The batch is prepared and the commit waited only
/// its short busy timeout, so it is offered again soon; a batch that settled nothing backs
/// off on the long schedule.
const BUSY_COMMIT_WAIT: Duration = Duration::from_millis(500);

/// A publication that holds the engine longer than this is reported: phase C is bounded by
/// construction, and one that is not has something in it that should not be there.
const SLOW_HOLD: Duration = Duration::from_millis(100);

/// Where the owner is, for `search status` and the completeness reasons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BacklogState {
    /// Answering marks, or idle with none to answer.
    Running,
    /// A refusal holds the next batch off until `until`.
    Backoff { until: Instant },
    /// The retry budget ran out, or a batch failed outright, at `since`. The marks stay; only
    /// a fresh fact from the consumer revives the owner.
    Exhausted { since: Instant },
    /// The owner has left, or has not started because no engine is published yet.
    Stopped { reason: &'static str },
}

#[derive(Default)]
struct SignalState {
    wake: bool,
    fresh_epoch: u64,
    stop: bool,
    /// How many waits ended on a signal instead of on their timeout — the owner's own count
    /// of the wakes it took. A test may not infer it from how often it called `wake`: the
    /// call and the taking are on different threads.
    #[cfg(test)]
    consumed: u64,
}

/// How the consumer and requests reach the owner.
#[derive(Default)]
pub(crate) struct BacklogSignal {
    state: Mutex<SignalState>,
    wake: Condvar,
}

impl BacklogSignal {
    fn lock(&self) -> std::sync::MutexGuard<'_, SignalState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The consumer placed marks: a fresh fact, which may revive an exhausted owner.
    pub(crate) fn fresh(&self) {
        let mut state = self.lock();
        state.fresh_epoch = state.fresh_epoch.wrapping_add(1);
        state.wake = true;
        drop(state);
        self.wake.notify_all();
    }

    /// A request saw the backlog: wake the owner, but tell it nothing new — it may not step
    /// around its backoff or revive an exhausted budget on this.
    pub(crate) fn wake(&self) {
        self.lock().wake = true;
        self.wake.notify_all();
    }

    fn stop(&self) {
        self.lock().stop = true;
        self.wake.notify_all();
    }

    fn is_stopped(&self) -> bool {
        self.lock().stop
    }

    /// Wait until woken, stopped, or `timeout` passes; returns the stop and the fresh epoch.
    /// The wake is taken BEFORE the owner works, so a signal that lands during the work
    /// leaves another pass owed instead of being cleared by it.
    fn wait(&self, timeout: Duration) -> (bool, u64) {
        let mut state = self.lock();
        let deadline = Instant::now() + timeout;
        while !state.wake && !state.stop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { break };
            state =
                self.wake.wait_timeout(state, remaining).unwrap_or_else(PoisonError::into_inner).0;
        }
        #[cfg(test)]
        if state.wake {
            state.consumed += 1;
        }
        state.wake = false;
        (state.stop, state.fresh_epoch)
    }

    #[cfg(test)]
    fn consumed(&self) -> u64 {
        self.lock().consumed
    }
}

/// The handle the rest of the backend holds: the signal, and the state the owner reports.
#[derive(Clone)]
pub(crate) struct OverlayBacklog {
    signal: Arc<BacklogSignal>,
    state: Arc<Mutex<BacklogState>>,
    slow_holds: Arc<AtomicU64>,
    /// Raised while a batch is being worked and while the next one follows at once; lowered
    /// whenever the owner waits. The backend's lifetime reads it, as it reads a pass of the
    /// embedding driver: a backoff sat out is no work that should hold a process.
    active: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    pub(crate) probe: Arc<test_probe::Probe>,
}

impl Default for OverlayBacklog {
    fn default() -> Self {
        Self {
            signal: Arc::default(),
            state: Arc::new(Mutex::new(BacklogState::Stopped { reason: "no published engine" })),
            slow_holds: Arc::default(),
            active: Arc::default(),
            #[cfg(test)]
            probe: Arc::default(),
        }
    }
}

impl OverlayBacklog {
    pub(crate) fn fresh(&self) {
        self.signal.fresh();
    }

    pub(crate) fn wake(&self) {
        self.signal.wake();
    }

    /// Ask the owner to leave; it answers at its next wake, which this is.
    pub(crate) fn stop(&self) {
        self.signal.stop();
    }

    pub(crate) fn state(&self) -> BacklogState {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Whether the owner is working through the backlog right now.
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// How many publications held the engine past their bound.
    pub(crate) fn slow_holds(&self) -> u64 {
        self.slow_holds.load(Ordering::Relaxed)
    }

    /// How many wakes the owner has taken at its own wait.
    #[cfg(test)]
    pub(crate) fn wakes_consumed(&self) -> u64 {
        self.signal.consumed()
    }

    fn set(&self, state: BacklogState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
    }

    /// Start the owner over `engine`. Returns whether its thread started.
    pub(crate) fn start(
        &self,
        engine: SharedSearchEngine,
        lease: WorkspaceLease,
        overlay_retry: Option<Arc<super::overlay_retry::OverlayRetry>>,
        stop: OwnerStop,
    ) -> bool {
        self.start_with(engine, lease, overlay_retry, stop, Pacing::PRODUCTION)
    }

    pub(crate) fn start_with(
        &self,
        engine: SharedSearchEngine,
        lease: WorkspaceLease,
        overlay_retry: Option<Arc<super::overlay_retry::OverlayRetry>>,
        stop: OwnerStop,
        pacing: Pacing,
    ) -> bool {
        self.set(BacklogState::Running);
        let owner = Owner {
            handle: self.clone(),
            engine,
            lease,
            overlay_retry,
            _live: stop.enter(),
            stop,
            pacing,
        };
        let spawned = std::thread::Builder::new()
            .name("bsl-overlay-backlog".to_owned())
            .spawn(move || owner.run());
        if spawned.is_err() {
            self.set(BacklogState::Stopped { reason: "the owner thread could not start" });
        }
        spawned.is_ok()
    }
}

/// The owner's clock: how long it idles, the retry budget it spends on refusals, and how long
/// a publication may hold the engine before it is reported.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pacing {
    pub(crate) idle_tick: Duration,
    pub(crate) retry_budget: Duration,
    pub(crate) slow_hold: Duration,
}

impl Pacing {
    pub(crate) const PRODUCTION: Self = Self {
        idle_tick: IDLE_TICK,
        retry_budget: super::retry_window::DEFAULT_RETRY_BUDGET,
        slow_hold: SLOW_HOLD,
    };
}

struct Owner {
    handle: OverlayBacklog,
    engine: SharedSearchEngine,
    lease: WorkspaceLease,
    overlay_retry: Option<Arc<super::overlay_retry::OverlayRetry>>,
    stop: OwnerStop,
    pacing: Pacing,
    _live: OwnerLive,
}

/// Why a backlog owner left, in the words the status already uses.
///
/// Every exit used to arrive at the single word "stopped", so a workspace taken over by a newer
/// daemon, or one whose lease was handed back, reported the same thing as an ordinary shutdown
/// — and the one explanation a reader needs to tell those apart was the one thrown away. These
/// are the existing strings; no new reason code is introduced and the search answers are
/// unchanged.
fn exit_reason(lease: &WorkspaceLease) -> &'static str {
    if lease.is_superseded() {
        "the workspace was taken over"
    } else if lease.is_released() {
        "the lease went terminal"
    } else {
        "stopped"
    }
}

/// What one round did.
#[derive(Debug, PartialEq, Eq)]
enum Round {
    /// The backlog shrank: marks went for good, or keys were dropped as stale. Go again at once.
    Progress,
    /// Nothing to do.
    Idle,
    /// SQLite's writer lock outlasted the commit's busy timeout; the prepared batch is kept.
    Busy,
    /// The lease lock stayed contended past its wait; the prepared batch is kept.
    Refused,
    /// The batch settled nothing for good: its keys were marked again by a fault. Back off.
    Unsettled,
    /// The state the batch was prepared against was replaced under it — a graph publication
    /// swapping the context provider is the ordinary way — so nothing was applied and every
    /// mark stayed. The state that moved IS the pause, exactly as a contended lease is, so the
    /// first one is re-prepared at once and each further one is paced.
    Invalidated,
    /// A batch failed outright.
    Failed,
    /// The owner was told to leave mid-batch.
    Stopping,
    /// The lease went terminal.
    Terminal,
}

impl Owner {
    fn run(self) {
        let mut window = RetryWindow::with_budget(RetryOwner::Drift, self.pacing.retry_budget);
        let mut streak = 0u32;
        let mut busy = 0u32;
        let mut refusals = 0u32;
        let mut invalidations = 0u32;
        let mut next_allowed = self.now();
        let mut seen_epoch = 0u64;
        let mut held: Option<bsl_search::PreparedPointBatch> = None;
        let mut progressed = false;
        // Read where the owner decides to leave, not where it says so: a stop landing after a
        // terminal lease has already sent the owner away renames an exit it had no part in.
        let left_because;
        loop {
            let now = self.now();
            let backlog = held.is_some() || self.backlog() > 0;
            // An owner out of budget waits for a fresh fact, however much is marked.
            let timeout = if !window.is_open(now) {
                self.pacing.idle_tick
            } else if backlog && progressed {
                Duration::ZERO
            } else if backlog {
                next_allowed.saturating_duration_since(now).max(Duration::from_millis(1))
            } else {
                self.pacing.idle_tick
            };
            #[cfg(test)]
            self.handle.probe.before_wait();
            // Waiting is not working, however long the wait.
            if !timeout.is_zero() {
                self.handle.active.store(false, Ordering::SeqCst);
            }
            let (stopped, epoch) = self.handle.signal.wait(timeout);
            if stopped || self.must_leave() {
                left_because = self.left_because();
                break;
            }
            let now = self.now();
            if epoch != seen_epoch {
                seen_epoch = epoch;
                if window.observe_external_work(now, true) {
                    // A new obligation starts its schedules from the beginning.
                    streak = 0;
                    busy = 0;
                    refusals = 0;
                    invalidations = 0;
                    next_allowed = now;
                    self.handle.set(BacklogState::Running);
                }
            }
            progressed = false;
            if now < next_allowed {
                continue;
            }
            if !window.is_open(now) {
                if !matches!(self.handle.state(), BacklogState::Exhausted { .. }) {
                    self.handle.set(BacklogState::Exhausted { since: now });
                }
                continue;
            }
            self.handle.active.store(true, Ordering::SeqCst);
            #[cfg(test)]
            self.handle.probe.admitted();
            let round = self.round(&mut held);
            match round {
                Round::Progress => {
                    progressed = true;
                    streak = 0;
                    busy = 0;
                    refusals = 0;
                    invalidations = 0;
                    window.complete();
                    self.handle.set(BacklogState::Running);
                    if let Some(retry) = &self.overlay_retry {
                        retry.coalesce();
                    }
                }
                Round::Idle => self.handle.set(BacklogState::Running),
                // Told to leave mid-batch: the prepared work is dropped and the owner goes.
                // Nothing is marked done and nothing is backed off — the marks stay for
                // whoever runs next.
                Round::Stopping => {
                    left_because = self.left_because();
                    break;
                }
                Round::Busy | Round::Refused | Round::Unsettled | Round::Invalidated => {
                    let delay = match round {
                        Round::Busy => {
                            let wait = BUSY_COMMIT_WAIT
                                .checked_mul(1u32 << busy.min(6))
                                .unwrap_or(super::overlay_retry::TICK);
                            busy = busy.saturating_add(1);
                            wait.min(super::overlay_retry::TICK)
                        }
                        // The lease wait itself was the pause: one more try at once, as the
                        // consumer's own drift does, then the ordinary schedule.
                        Round::Refused => {
                            let wait = super::overlay_retry::retry_delay(refusals);
                            refusals = refusals.saturating_add(1);
                            wait
                        }
                        // A batch invalidated by a state that moved under it settled nothing,
                        // so it is never progress and never resets the budget. But the move
                        // itself was the pause: the marks are still there and re-preparing
                        // against the state that now holds is the whole remedy, so the first
                        // one goes again at once, exactly as a refused lease does. A trigger
                        // that keeps moving is what the schedule below is for — and the budget
                        // spent here is what ends it.
                        Round::Invalidated => {
                            let wait = super::overlay_retry::retry_delay(invalidations);
                            invalidations = invalidations.saturating_add(1);
                            wait
                        }
                        _ => {
                            streak = streak.saturating_add(1);
                            super::overlay_retry::retry_delay(streak)
                        }
                    };
                    match window.refused(now, delay) {
                        RetryDecision::RetryAfter(delay) => {
                            next_allowed = now + delay;
                            self.handle.set(BacklogState::Backoff { until: next_allowed });
                        }
                        RetryDecision::Stop(_) => {
                            self.handle.set(BacklogState::Exhausted { since: now });
                        }
                    }
                }
                Round::Failed => {
                    window.operation_error();
                    held = None;
                    self.handle.set(BacklogState::Exhausted { since: now });
                }
                Round::Terminal => {
                    left_because = self.left_because();
                    break;
                }
            }
            self.handle.active.store(progressed, Ordering::SeqCst);
        }
        self.handle.active.store(false, Ordering::SeqCst);
        #[cfg(test)]
        self.handle.probe.before_exit();
        self.handle.set(BacklogState::Stopped { reason: left_because });
    }

    /// Why this owner is leaving, asked at the moment it decides to.
    ///
    /// A normal shutdown stops every owner and then releases the lease without waiting for any
    /// of them, so an owner slow to act on its stop finds the release already made — and still
    /// left because it was stopped. A takeover is somebody else's doing, and says so whenever
    /// it is seen. What this cannot separate is a stop and a release that are BOTH already
    /// visible when the owner looks: nothing records which of them happened first.
    fn left_because(&self) -> &'static str {
        let stopped = self.stop.is_stopped() || self.handle.signal.is_stopped();
        if stopped && !self.lease.is_superseded() {
            "stopped"
        } else {
            exit_reason(&self.lease)
        }
    }

    /// Every instant the owner's schedule is made of. Production always reads the system
    /// clock; a test may hold this one still, so that what the owner does before a deadline
    /// can be observed without outrunning it.
    fn now(&self) -> Instant {
        #[cfg(test)]
        return self.handle.probe.now();
        #[cfg(not(test))]
        Instant::now()
    }

    fn lease_is_terminal(&self) -> bool {
        self.lease.is_superseded() || self.lease.is_released()
    }

    /// Asked between the keys of a preparation, too: the longest stretch an owner spends away
    /// from its wait.
    fn must_leave(&self) -> bool {
        self.stop.is_stopped() || self.handle.signal.is_stopped() || self.lease_is_terminal()
    }

    fn backlog(&self) -> usize {
        let Ok(guard) = self.engine.acquire_for_owner(&self.stop) else { return 0 };
        guard.as_ref().and_then(|engine| engine.workspace_overlay_point_backlog().ok()).unwrap_or(0)
    }

    /// One batch: capture and prepare it if none is held, then publish it.
    fn round(&self, held: &mut Option<bsl_search::PreparedPointBatch>) -> Round {
        if held.is_none() {
            let capture = {
                let guard = match self.engine.acquire_for_owner(&self.stop) {
                    Ok(guard) => guard,
                    Err(crate::tools::search::OwnerLockRefused::Closing) => return Round::Terminal,
                    Err(crate::tools::search::OwnerLockRefused::Poisoned) => return Round::Failed,
                };
                let Some(engine) = guard.as_ref() else { return Round::Idle };
                match engine.capture_point_refresh(bsl_search::POINT_BATCH_KEYS) {
                    Ok(Some(capture)) => capture,
                    Ok(None) => return Round::Idle,
                    Err(error) => {
                        tracing::warn!("overlay backlog capture failed: {error}");
                        return Round::Failed;
                    }
                }
            };
            #[cfg(test)]
            self.handle.probe.captured();
            // A reader per preparation: one kept across batches would go on reading a store
            // file replaced at the same path, and prepare against a baseline that is gone.
            let reader = match bsl_search::Store::open_reader(capture.db_path()) {
                Ok(reader) => reader,
                Err(error) => {
                    tracing::warn!("overlay backlog reader could not open: {error}");
                    return Round::Failed;
                }
            };
            let cancelled = || self.must_leave();
            match capture.prepare(&reader, &cancelled) {
                Ok(batch) => *held = Some(batch),
                Err(error) => {
                    tracing::warn!("overlay backlog preparation failed: {error}");
                    return Round::Failed;
                }
            }
            #[cfg(test)]
            self.handle
                .probe
                .prepared(held.as_ref().map_or(0, bsl_search::PreparedPointBatch::len));
            if self.must_leave() {
                return Round::Terminal;
            }
        }
        let Some(batch) = held.as_mut() else { return Round::Idle };
        #[cfg(test)]
        self.handle.probe.publishing();
        let outcome =
            SharedState::apply_workspace_search(&self.engine, &self.stop, &self.lease, |engine| {
                let _span =
                    tracing::debug_span!("overlay_backlog_publish", keys = batch.len()).entered();
                let held_since = Instant::now();
                let published = engine.publish_point_refresh(batch);
                let held = held_since.elapsed();
                if held > self.pacing.slow_hold {
                    self.handle.slow_holds.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        ?held,
                        keys = batch.len(),
                        "an overlay batch held the engine past its bound"
                    );
                }
                published
            });
        #[cfg(test)]
        self.handle.probe.published();
        match outcome {
            WorkspaceSearchApply::Applied(publish) => {
                if !matches!(publish, bsl_search::PointPublish::Busy) {
                    *held = None;
                }
                Self::pacing_for(&publish)
            }
            WorkspaceSearchApply::TransientRefusal => Round::Refused,
            WorkspaceSearchApply::OperationError(error) => {
                tracing::warn!("overlay backlog publication failed: {error}");
                Round::Failed
            }
            // Told to leave. Not a refusal to back off from and not a failure to report: the
            // owner's next wait returns at once and it goes.
            WorkspaceSearchApply::Stopping => Round::Stopping,
            WorkspaceSearchApply::Superseded | WorkspaceSearchApply::Released => Round::Terminal,
        }
    }
}

impl Owner {
    /// How a publication's outcome is paced. Pure, so the pacing can be read — and asserted —
    /// without running an owner.
    ///
    /// Only a round that moved marks is progress. A DISCARD moved none: its own documentation
    /// says the batch was prepared against a state that no longer holds, nothing was applied
    /// and the marks stay. Calling it progress zeroed the retry budget AND set the next wait to
    /// nothing, so a trigger that keeps moving that state — the baseline version, which this
    /// change made move on every write of `files` — turned a bounded class of refusals into an
    /// owner spinning without a pause, holding the engine lock and the lease fence while it
    /// spun. Re-preparing against the moved state is right; doing it unpaced is not, and
    /// genuinely fresh work still resets the schedules through the epoch signal.
    fn pacing_for(publish: &bsl_search::PointPublish) -> Round {
        match publish {
            // Progress is marks gone for good, or keys dropped as stale — their fresher marks
            // are the next batch's. A fault that marked its key again is not: backing off is
            // what bounds a key that keeps failing.
            bsl_search::PointPublish::Applied { cleared, stale, .. } => {
                if *cleared > 0 || *stale > 0 {
                    Round::Progress
                } else {
                    Round::Unsettled
                }
            }
            bsl_search::PointPublish::Discarded => Round::Invalidated,
            bsl_search::PointPublish::Busy => Round::Busy,
        }
    }
}

/// One whole batch — capture, prepare, publish — for a test that wants a point refresh to have
/// happened without running the owner.
#[cfg(test)]
pub(crate) fn refresh_one_batch(engine: &SharedSearchEngine, lease: &WorkspaceLease) {
    let capture = {
        let guard = engine.lock().unwrap();
        guard.as_ref().unwrap().capture_point_refresh(bsl_search::POINT_BATCH_KEYS).unwrap()
    };
    let Some(capture) = capture else { return };
    let reader = bsl_search::Store::open_reader(capture.db_path()).unwrap();
    let mut batch = capture.prepare(&reader, &|| false).unwrap();
    let outcome = SharedState::apply_workspace_search(
        engine,
        &crate::state::OwnerStop::default(),
        lease,
        |engine| engine.publish_point_refresh(&mut batch),
    );
    assert!(matches!(
        outcome,
        WorkspaceSearchApply::Applied(bsl_search::PointPublish::Applied { .. })
    ));
}

#[cfg(test)]
pub(crate) mod test_probe {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// A clock a test can hold still.
    ///
    /// The backoff deadline is an instant the owner computes for itself, so a test that wants
    /// to see what the owner does *before* that deadline otherwise has to outrun it — and
    /// loses that race whenever the host deschedules the observing thread. Holding the clock
    /// still ends the race without touching a single production duration: the waits, the busy
    /// timeout and the delays are all the ones production uses, and the decision the test
    /// checks is still the owner's own.
    pub(crate) struct TestClock {
        base: Instant,
        advanced: AtomicU64,
    }

    impl Default for TestClock {
        fn default() -> Self {
            Self { base: Instant::now(), advanced: AtomicU64::new(0) }
        }
    }

    impl TestClock {
        pub(crate) fn now(&self) -> Instant {
            self.base + Duration::from_nanos(self.advanced.load(Ordering::SeqCst))
        }

        pub(crate) fn advance(&self, by: Duration) {
            self.advanced
                .fetch_add(u64::try_from(by.as_nanos()).unwrap_or(u64::MAX), Ordering::SeqCst);
        }
    }

    /// What the owner did, counted, and a park a test can hold it in.
    #[derive(Default)]
    pub(crate) struct Probe {
        pub(crate) captures: AtomicUsize,
        /// Keys the preparations produced, all of them together.
        pub(crate) prepared_keys: AtomicUsize,
        pub(crate) publishes: AtomicUsize,
        /// Rounds the owner was let through to — what it attempted, counted before the round
        /// can decide there was nothing to do.
        pub(crate) admissions: AtomicUsize,
        /// Unset in every test that does not ask for it, and the owner then reads the system
        /// clock.
        pub(crate) clock: Mutex<Option<std::sync::Arc<TestClock>>>,
        /// Runs after every capture, before its preparation starts.
        pub(crate) after_capture: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
        pub(crate) publish_times: Mutex<Vec<std::time::Instant>>,
        /// Runs after every preparation, called with the slot unlocked so a test can clear it
        /// while a call is parked inside.
        pub(crate) after_prepare: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
        /// Runs after every publication, before the owner tells anyone about it.
        pub(crate) after_publish: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
        /// Runs once, the next time the owner is about to wait, and is then spent.
        pub(crate) before_wait_once: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// Runs once, after the owner has decided to leave and before it says why.
        pub(crate) before_exit_once: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Probe {
        pub(super) fn captured(&self) {
            self.captures.fetch_add(1, Ordering::SeqCst);
            let hook = self.after_capture.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook();
            }
        }

        pub(super) fn prepared(&self, keys: usize) {
            self.prepared_keys.fetch_add(keys, Ordering::SeqCst);
            let hook = self.after_prepare.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook();
            }
        }

        pub(super) fn published(&self) {
            let hook = self.after_publish.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook();
            }
        }

        pub(super) fn before_wait(&self) {
            if let Some(hook) = self.before_wait_once.lock().unwrap().take() {
                hook();
            }
        }

        pub(super) fn before_exit(&self) {
            if let Some(hook) = self.before_exit_once.lock().unwrap().take() {
                hook();
            }
        }

        pub(super) fn admitted(&self) {
            self.admissions.fetch_add(1, Ordering::SeqCst);
        }

        pub(super) fn now(&self) -> Instant {
            match self.clock.lock().unwrap().as_ref() {
                Some(clock) => clock.now(),
                None => Instant::now(),
            }
        }

        pub(super) fn publishing(&self) {
            self.publishes.fetch_add(1, Ordering::SeqCst);
            self.publish_times.lock().unwrap().push(std::time::Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {

    /// A terminal lease and an ordinary stop are different events, and the status is where a
    /// reader finds out which happened. Every exit arriving at one word threw that away.
    #[test]
    fn terminal_lease_exit_preserves_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        assert_eq!(super::exit_reason(&lease), "stopped", "an ordinary stop keeps its own word");

        // Taken over by a newer daemon.
        let newer = crate::workspace_lease::WorkspaceLease::claim(root);
        let mut deadline = crate::change_hub::test_support::Patience::new(
            crate::change_hub::test_support::STATE_CEILING,
            std::time::Duration::from_millis(20),
        );
        while !lease.is_superseded() {
            assert!(deadline.allows(), "the takeover was never observed");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            super::exit_reason(&lease),
            "the workspace was taken over",
            "a supersession must not be reported as an ordinary stop",
        );

        // And a lease handed back is its own outcome too.
        newer.release();
        assert_eq!(super::exit_reason(&newer), "the lease went terminal");
    }

    /// A discard applies nothing and leaves every mark where it was. Pacing it as progress
    /// zeroed the retry budget and set the next wait to nothing at the same time: an owner with
    /// a trigger that keeps moving under it then spins, holding the engine lock and the lease
    /// fence for as long as the trigger fires. One such trigger is the baseline version, which
    /// moves on every write of `files`.
    ///
    /// It is not `Unsettled` either: a key that marked itself again by a fault has to wait for
    /// the fault to pass, while a batch whose ground moved has nothing to wait for — the state
    /// it must be prepared against is already here. So it has a pacing of its own, and the
    /// schedule that pacing follows is asserted where the owner runs, not here.
    #[test]
    fn a_discarded_batch_is_paced_because_it_settled_nothing() {
        use bsl_search::PointPublish;
        assert_eq!(
            super::Owner::pacing_for(&PointPublish::Discarded),
            super::Round::Invalidated,
            "a discard applied nothing, so it is not progress and must not reset the budget",
        );
        assert_eq!(
            super::Owner::pacing_for(&PointPublish::Applied {
                settled: 3,
                cleared: 0,
                stale: 0,
                remaining: 3,
            }),
            super::Round::Unsettled,
            "a round whose keys all marked themselves again settled nothing either",
        );
        assert_eq!(
            super::Owner::pacing_for(&PointPublish::Applied {
                settled: 3,
                cleared: 2,
                stale: 0,
                remaining: 1,
            }),
            super::Round::Progress,
            "marks gone for good are the progress the fast path is for",
        );
    }
    use super::*;
    use crate::change_hub::test_support::eventually;
    use bsl_search::SearchEngine;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    const FAST: Pacing = Pacing {
        idle_tick: Duration::from_millis(20),
        retry_budget: super::super::retry_window::DEFAULT_RETRY_BUDGET,
        slow_hold: SLOW_HOLD,
    };

    /// [`engine_over`] for a test that holds SQLite's writer itself: the engine's connection
    /// gives up on it after 50 ms instead of the operational 30 s, so the steps the test takes
    /// with the writer held answer at once rather than after the whole budget. The owner's
    /// first point commit puts the 30 s back; every step these tests take on the connection
    /// with the writer held comes before it.
    fn engine_over_held_writer(workspace: &Path, files: usize) -> SharedSearchEngine {
        let shared = engine_over(workspace, files);
        shared
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .store()
            .set_busy_timeout_for_test(Duration::from_millis(50))
            .unwrap();
        shared
    }

    fn engine_over(workspace: &Path, files: usize) -> SharedSearchEngine {
        for index in 0..files {
            fs::write(
                workspace.join(format!("M{index}.bsl")),
                format!("Процедура Старая{index}() Экспорт\nКонецПроцедуры\n"),
            )
            .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace);
        engine.index_directory_fts(workspace).unwrap();
        engine.enable_workspace_watcher_mode();
        engine.initialize_workspace_overlay_clean().unwrap();
        crate::state::shared_engine(Some(engine))
    }

    fn edit(shared: &SharedSearchEngine, path: &PathBuf, text: &str) {
        fs::write(path, text).unwrap();
        let guard = shared.lock().unwrap();
        assert!(guard.as_ref().unwrap().mark_workspace_path_dirty(path).unwrap());
    }

    fn backlog(shared: &SharedSearchEngine) -> usize {
        shared.lock().unwrap().as_ref().unwrap().workspace_overlay_point_backlog().unwrap()
    }

    fn found(shared: &SharedSearchEngine, symbol: &str) -> bool {
        let guard = shared.lock().unwrap();
        !guard.as_ref().unwrap().text_search_read_only(symbol, 10, Some("code")).unwrap().is_empty()
    }

    fn start(
        shared: &SharedSearchEngine,
        lease: &WorkspaceLease,
        retry: Option<Arc<super::super::overlay_retry::OverlayRetry>>,
        pacing: Pacing,
    ) -> (OverlayBacklog, OwnerStop) {
        let handle = OverlayBacklog::default();
        let stop = OwnerStop::default();
        assert!(handle.start_with(Arc::clone(shared), lease.clone(), retry, stop.clone(), pacing));
        (handle, stop)
    }

    fn finish(handle: &OverlayBacklog, stop: &OwnerStop) {
        stop.stop();
        handle.stop();
        assert!(
            crate::change_hub::test_support::eventually_within(100, || stop.live() == 0),
            "the owner stayed"
        );
    }

    /// An owner that is stopped however the test leaves. A test that asserts in the middle of
    /// a backoff would otherwise strand a thread on the temporary directory it is about to
    /// remove, and the failure would be followed by a worse one.
    struct RunningOwner {
        handle: OverlayBacklog,
        stop: OwnerStop,
    }

    impl RunningOwner {
        fn start(
            handle: &OverlayBacklog,
            shared: &SharedSearchEngine,
            lease: &WorkspaceLease,
            pacing: Pacing,
        ) -> Self {
            let stop = OwnerStop::default();
            assert!(handle.start_with(
                Arc::clone(shared),
                lease.clone(),
                None,
                stop.clone(),
                pacing
            ));
            Self { handle: handle.clone(), stop }
        }

        fn shut_down(&self) -> bool {
            self.stop.stop();
            self.handle.stop();
            eventually(|| self.stop.live() == 0)
        }

        fn finish(self) {
            assert!(self.shut_down(), "the owner stayed");
        }
    }

    impl Drop for RunningOwner {
        fn drop(&mut self) {
            // Nothing is asserted here: on the path this matters for the test is already
            // failing, and a second panic would replace what it was saying.
            let _ = self.shut_down();
        }
    }

    /// What one publication left behind, taken at that publication and handed over whole.
    #[derive(Clone, Copy, Debug)]
    struct FirstPublication {
        backlog: usize,
        hit: bool,
    }

    /// The pause an owner will really wait out, told apart from the one it also publishes for a
    /// retry it makes at once: `RetryDecision::RetryAfter(Duration::ZERO)` still names an
    /// instant, and that instant is `now`. Read against the clock the owner itself reads.
    fn future_pause(state: &BacklogState, now: Instant) -> Option<Instant> {
        match state {
            BacklogState::Backoff { until } if *until > now => Some(*until),
            _ => None,
        }
    }

    /// The premise of the control below is a reading taken inside a callback, and a reader has
    /// to be told when it exists. A counter bumped on the way IN cannot say that: it is true
    /// before the reading is written, and no ordering on it can publish a store that has not
    /// happened yet. This parks the callback exactly in that gap and looks:
    ///
    /// * the counter already says a publication happened — a reader waiting on it would go;
    /// * the reading is not there yet, so that reader would have seen nothing at all.
    ///
    /// The control publishes both fields as ONE value, and waits for that value, so the same
    /// barrier exposes nothing half-written. Deterministic: the callback is held at the gap
    /// until this thread has finished looking.
    #[test]
    fn the_first_publication_is_read_only_once_the_whole_reading_exists() {
        struct NoContext;
        impl bsl_search::GraphContextProvider for NoContext {
            fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
                None
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let handle = OverlayBacklog::default();
        let clock = Arc::new(test_probe::TestClock::default());
        *handle.probe.clock.lock().unwrap() = Some(Arc::clone(&clock));

        let engine_to_swap = Arc::clone(&shared);
        let swapped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let swapping = Arc::clone(&swapped);
        *handle.probe.after_prepare.lock().unwrap() = Some(Arc::new(move || {
            if swapping.fetch_add(1, Ordering::SeqCst) > 0 {
                return;
            }
            engine_to_swap
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .replace_published_graph_context_provider(Arc::new(NoContext))
                .unwrap();
        }));

        let publishes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_publication: Arc<Mutex<Option<FirstPublication>>> = Arc::new(Mutex::new(None));
        let (at_the_gap, reached) = std::sync::mpsc::channel();
        let (go_on, allowed) = std::sync::mpsc::channel::<()>();
        // The hook is shared, so its ends of the handshake are held behind a lock.
        let at_the_gap = Mutex::new(at_the_gap);
        let allowed = Mutex::new(allowed);
        let counted = Arc::clone(&publishes);
        let recorded = Arc::clone(&first_publication);
        let engine_to_read = Arc::clone(&shared);
        *handle.probe.after_publish.lock().unwrap() = Some(Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                return;
            }
            // Exactly the gap: counted, not yet read.
            at_the_gap.lock().unwrap_or_else(PoisonError::into_inner).send(()).unwrap();
            let _ = allowed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv_timeout(crate::change_hub::test_support::STATE_CEILING);
            let reading = FirstPublication {
                backlog: backlog(&engine_to_read),
                hit: found(&engine_to_read, "Новая"),
            };
            *recorded.lock().unwrap_or_else(PoisonError::into_inner) = Some(reading);
        }));

        let owner = RunningOwner::start(&handle, &shared, &WorkspaceLease::unmanaged(), FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Новая() Экспорт\nКонецПроцедуры\n");
        handle.fresh();

        reached
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the owner never published anything to park");
        assert_eq!(
            publishes.load(Ordering::SeqCst),
            1,
            "the counter a reader would wait on is not yet true at the gap",
        );
        assert!(
            first_publication.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "a reader woken by that counter would have read a reading that exists",
        );
        go_on.send(()).unwrap();

        assert!(
            eventually(|| first_publication
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some()),
            "the reading never arrived after the callback was let go",
        );
        let first = first_publication
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .expect("the wait above puts it there");
        assert_eq!(first.backlog, 1, "the invalidated publication settled a mark after all");
        assert!(!first.hit, "the edit was already searchable before that publication");
        owner.finish();
    }

    /// A graph publication swaps the overlay's context provider, and that raises the cache's
    /// wholesale fence: a point batch prepared before the swap can no longer publish, so it is
    /// discarded and every mark it carried stays exactly where it was. What the owner has to do
    /// with that is prepare again against the state that now holds — the marks are still there,
    /// and nothing has to pass before the next attempt can succeed. Pacing one such
    /// invalidation like a fault leaves the edit unpublished for a backoff it has no reason to
    /// wait out.
    ///
    /// The owner's clock is held still for the whole test, so "without waiting out a backoff"
    /// is read exactly and not raced for: no production delay is shortened, no deadline can
    /// pass under the observer, and the decision asserted is the owner's own. This says what
    /// the owner's SCHEDULE does; it is not a latency measurement and does not stand in for
    /// the polling stand's own deadline.
    #[test]
    fn a_single_invalidation_is_re_prepared_without_waiting_out_a_backoff() {
        struct NoContext;
        impl bsl_search::GraphContextProvider for NoContext {
            fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
                None
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let handle = OverlayBacklog::default();
        let clock = Arc::new(test_probe::TestClock::default());
        *handle.probe.clock.lock().unwrap() = Some(Arc::clone(&clock));

        // ONE invalidation, landing between a preparation and its publication, made the way a
        // graph publication makes it: through the engine's own API, with the slot unlocked.
        // Two counters, because they answer different questions: which preparation this is, and
        // whether the replacement actually went through. The second is raised AFTER the call
        // returns, so it counts a finished effect rather than an entry into the hook.
        let preparations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let providers_replaced = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entering = Arc::clone(&preparations);
        let replaced = Arc::clone(&providers_replaced);
        let engine_to_swap = Arc::clone(&shared);
        *handle.probe.after_prepare.lock().unwrap() = Some(Arc::new(move || {
            if entering.fetch_add(1, Ordering::SeqCst) > 0 {
                return;
            }
            engine_to_swap
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .replace_published_graph_context_provider(Arc::new(NoContext))
                .unwrap();
            replaced.fetch_add(1, Ordering::SeqCst);
        }));

        // What the first publication settled, read AT that publication, so the answer cannot be
        // overtaken by the very retry this test is about — and handed over as ONE value. Two
        // fields behind a counter would let a reader woken by the counter see neither of them:
        // an atomic ordering publishes what was written BEFORE it, never what comes after.
        // Which callback is the first is a separate question, and stays a counter.
        let publishes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_publication: Arc<Mutex<Option<FirstPublication>>> = Arc::new(Mutex::new(None));
        let counted = Arc::clone(&publishes);
        let recorded = Arc::clone(&first_publication);
        let engine_to_read = Arc::clone(&shared);
        *handle.probe.after_publish.lock().unwrap() = Some(Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                return;
            }
            let reading = FirstPublication {
                backlog: backlog(&engine_to_read),
                hit: found(&engine_to_read, "Новая"),
            };
            *recorded.lock().unwrap_or_else(PoisonError::into_inner) = Some(reading);
        }));

        let owner = RunningOwner::start(&handle, &shared, &WorkspaceLease::unmanaged(), FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Новая() Экспорт\nКонецПроцедуры\n");
        handle.fresh();

        // Premises, each on its own: the reading of the first publication is here in full, it
        // was the invalidated one, and the edit was not already in the index before it. The
        // wait is on the reading itself, not on a counter that moves before it exists.
        assert!(
            eventually(|| first_publication
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some()),
            "premise: the owner never published anything to invalidate; it is {:?}",
            handle.state(),
        );
        let first = first_publication
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .expect("the wait above puts it there");
        assert_eq!(
            providers_replaced.load(Ordering::SeqCst),
            1,
            "premise: the provider replacement never completed, so nothing was invalidated",
        );
        assert_eq!(
            first.backlog, 1,
            "premise: the first publication was expected to settle nothing and keep the mark",
        );
        assert!(
            !first.hit,
            "premise: the edit was already searchable before the invalidated publication",
        );

        // The claim, with the clock exactly where it was left.
        assert!(
            eventually(|| found(&shared, "Новая") && backlog(&shared) == 0),
            "one invalidation left the edit unpublished while the owner waited: it is {:?}",
            handle.state(),
        );
        owner.finish();
    }

    /// The other side of the same decision: a trigger that keeps moving under the owner must
    /// not turn that prompt second attempt into a loop. After it, the owner pauses; a wake from
    /// a request neither shortens the pause nor hands back the budget the invalidations spent;
    /// and a stop still takes the owner out of the pause at once.
    #[test]
    fn a_trigger_that_keeps_invalidating_is_paced_and_spends_its_budget() {
        struct NoContext;
        impl bsl_search::GraphContextProvider for NoContext {
            fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
                None
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let handle = OverlayBacklog::default();
        let clock = Arc::new(test_probe::TestClock::default());
        *handle.probe.clock.lock().unwrap() = Some(Arc::clone(&clock));

        // Every preparation is invalidated before it can publish.
        let engine_to_swap = Arc::clone(&shared);
        *handle.probe.after_prepare.lock().unwrap() = Some(Arc::new(move || {
            engine_to_swap
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .replace_published_graph_context_provider(Arc::new(NoContext))
                .unwrap();
        }));

        let owner = RunningOwner::start(&handle, &shared, &WorkspaceLease::unmanaged(), FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Новая() Экспорт\nКонецПроцедуры\n");
        handle.fresh();

        // The owner publishes a `Backoff` for the retry it makes AT ONCE as well — `RetryAfter`
        // of nothing still names an instant — so "it paused" cannot be read from the variant.
        // On a clock held still, a pause the owner will really wait out is one whose instant is
        // still ahead; the zero-delay one is the negative control just below.
        let started = clock.now();
        let mut window = None;
        assert!(
            eventually(|| {
                window = future_pause(&handle.state(), clock.now());
                window.is_some()
            }),
            "an invalidation that keeps repeating never paused the owner: it is {:?}",
            handle.state(),
        );
        let until = window.expect("the wait above sets it");
        let now = clock.now();
        // The pause is the ordinary schedule's FIRST step, to the instant. A clock held still
        // makes that exact: the owner read this same instant when it computed the deadline, so
        // any other duration — a millisecond, a second, a step out of order — is a different
        // number here and not merely a different speed.
        assert_eq!(
            until,
            started + super::super::overlay_retry::retry_delay(1),
            "the pause after the second invalidation is not the schedule's first step",
        );
        assert!(
            future_pause(&BacklogState::Backoff { until: now }, now).is_none(),
            "the pause the owner sets for a retry it makes at once was read as a pause",
        );
        assert!(
            future_pause(&BacklogState::Backoff { until: now + Duration::from_millis(1) }, now)
                .is_some(),
            "a pause still ahead of the owner was not read as one",
        );
        // And the round that set it is over: the owner is parked at its wait, so the counters
        // below describe finished work and not a round still running.
        assert!(
            eventually(|| !handle.is_active()),
            "the owner never reached the wait its pause is for: it is {:?}",
            handle.state(),
        );
        let attempts = handle.probe.admissions.load(Ordering::SeqCst);
        let published = handle.probe.publishes.load(Ordering::SeqCst);
        assert_eq!(
            published, 2,
            "before its first pause the owner published {published} times: the first retry was \
             not made at once, or a repeating invalidation was retried more than once unpaced",
        );

        // Five wakes the owner is known to have taken, one handshake at a time.
        for wake in 1..=5 {
            let taken = handle.wakes_consumed();
            handle.wake();
            assert!(
                eventually(|| handle.wakes_consumed() > taken),
                "wake {wake} never reached the owner",
            );
        }
        // The deadline cannot pass under a stalled observer, so this reading is the same
        // whenever it is taken; only the timing profile waits a window out before it.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(300));
        }
        assert_eq!(
            handle.probe.admissions.load(Ordering::SeqCst),
            attempts,
            "a wake took the owner into a round with its pause still to run",
        );
        assert_eq!(
            handle.probe.publishes.load(Ordering::SeqCst),
            published,
            "the owner published again through its own pause",
        );
        assert_eq!(
            future_pause(&handle.state(), clock.now()),
            Some(until),
            "a wake moved the pause the owner was waiting out: it is {:?}",
            handle.state(),
        );

        // The transitions of that schedule, walked on the clock this test owns.
        //
        // A step BEFORE the deadline is still the pause: the owner takes the wake and goes back
        // to waiting, with nothing attempted.
        clock.advance(until.saturating_duration_since(clock.now()) - Duration::from_millis(1));
        let taken = handle.wakes_consumed();
        handle.wake();
        assert!(
            eventually(|| handle.wakes_consumed() > taken),
            "the wake just before the deadline never reached the owner",
        );
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(300));
        }
        assert_eq!(
            handle.probe.admissions.load(Ordering::SeqCst),
            attempts,
            "a millisecond before its deadline the owner was already attempting again",
        );
        assert_eq!(
            future_pause(&handle.state(), clock.now()),
            Some(until),
            "the pause moved while it was still running: it is {:?}",
            handle.state(),
        );

        // The deadline itself is what lets the next attempt through, and nothing else did.
        clock.advance(Duration::from_millis(1));
        handle.wake();
        assert!(
            eventually(|| handle.probe.admissions.load(Ordering::SeqCst) > attempts
                && handle.probe.publishes.load(Ordering::SeqCst) > published),
            "the deadline passed and the owner neither attempted nor published: it is {:?}",
            handle.state(),
        );

        // And the step GROWS: the invalidation after it is paced by the schedule's next step,
        // not by the one just served.
        let second_step = until + super::super::overlay_retry::retry_delay(2);
        assert!(
            eventually(|| future_pause(&handle.state(), clock.now()) == Some(second_step)),
            "the pause after the third invalidation is not the schedule's next step: it is {:?} \
             against an expected {second_step:?}",
            handle.state(),
        );

        // And the budget those invalidations spent is not handed back: past the window, the
        // owner says so instead of going on.
        clock.advance(FAST.retry_budget + Duration::from_secs(1));
        handle.wake();
        assert!(
            eventually(|| matches!(handle.state(), BacklogState::Exhausted { .. })),
            "the owner never spent the retry budget its invalidations were charged to: it is {:?}",
            handle.state(),
        );

        // A stop still takes the owner out of where it is.
        owner.finish();
    }

    #[test]
    fn the_owner_reads_back_a_mark_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Новая() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        assert!(eventually(|| found(&shared, "Новая") && backlog(&shared) == 0));
        finish(&handle, &stop);
    }

    #[test]
    fn a_terminal_lease_stops_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let lease = WorkspaceLease::claim(dir.path());
        let (handle, stop) = start(&shared, &lease, None, FAST);
        lease.release();
        handle.wake();
        assert!(eventually(|| stop.live() == 0), "the owner outlived its lease");
        assert!(matches!(handle.state(), BacklogState::Stopped { .. }));
    }

    /// An owner started over a claimed lease, woken by the stop the way the daemon wires it,
    /// and held at its first wait until the returned sender lets it go on.
    fn owner_held_at_its_wait(
        dir: &Path,
    ) -> (OverlayBacklog, OwnerStop, WorkspaceLease, std::sync::mpsc::Sender<()>) {
        let shared = engine_over(dir, 1);
        let lease = WorkspaceLease::claim(dir);
        let handle = OverlayBacklog::default();
        let stop = OwnerStop::default();
        let waker = handle.clone();
        stop.wakes(move || waker.stop());
        let (parked_tx, parked_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        *handle.probe.before_wait_once.lock().unwrap() = Some(Box::new(move || {
            parked_tx.send(()).unwrap();
            let _ = go_rx.recv_timeout(crate::change_hub::test_support::STATE_CEILING);
        }));
        assert!(handle.start_with(shared, lease.clone(), None, stop.clone(), FAST));
        parked_rx
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the owner never reached its wait");
        (handle, stop, lease, go_tx)
    }

    /// A normal shutdown asks every owner to leave and then hands the lease back without
    /// waiting for anybody. An owner slow to act on the stop finds the lease already released —
    /// and still left because it was stopped, which is what its status has to say.
    #[test]
    fn an_owner_stopped_before_the_lease_is_handed_back_reports_an_ordinary_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, stop, lease, go) = owner_held_at_its_wait(dir.path());
        stop.stop();
        lease.release();
        go.send(()).unwrap();
        assert!(eventually(|| stop.live() == 0), "the owner stayed");
        assert_eq!(
            handle.state(),
            BacklogState::Stopped { reason: "stopped" },
            "a stop that came before the release was reported as the lease going terminal",
        );
    }

    /// The owner left because its lease went terminal, and a stop lands after that decision was
    /// taken. The reason belongs to what made the owner leave, not to whatever is true by the
    /// time it says so.
    #[test]
    fn a_stop_landing_after_the_exit_decision_does_not_rename_it() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, stop, lease, go) = owner_held_at_its_wait(dir.path());
        let (at_exit, reached) = std::sync::mpsc::channel();
        let (leave, allowed) = std::sync::mpsc::channel::<()>();
        *handle.probe.before_exit_once.lock().unwrap() = Some(Box::new(move || {
            at_exit.send(()).unwrap();
            let _ = allowed.recv_timeout(crate::change_hub::test_support::STATE_CEILING);
        }));
        // The cause: the lease is handed back, with no stop anywhere.
        lease.release();
        go.send(()).unwrap();
        reached
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the owner never reached its exit");
        stop.stop();
        leave.send(()).unwrap();
        assert!(eventually(|| stop.live() == 0), "the owner stayed");
        assert_eq!(
            handle.state(),
            BacklogState::Stopped { reason: "the lease went terminal" },
            "a stop that landed after the owner had decided to leave renamed its reason",
        );
    }

    /// Countercontrols: a lease handed back with no stop, and a workspace taken over before the
    /// owner was stopped, keep their own words.
    #[test]
    fn an_owner_leaving_for_its_lease_keeps_the_lease_reason() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, stop, lease, go) = owner_held_at_its_wait(dir.path());
        lease.release();
        go.send(()).unwrap();
        handle.wake();
        assert!(eventually(|| stop.live() == 0), "the owner stayed");
        assert_eq!(handle.state(), BacklogState::Stopped { reason: "the lease went terminal" });

        let dir = tempfile::tempdir().unwrap();
        let (handle, stop, lease, go) = owner_held_at_its_wait(dir.path());
        let newer = WorkspaceLease::claim(dir.path());
        assert!(eventually(|| lease.is_superseded()), "the stand needs the takeover observed",);
        stop.stop();
        go.send(()).unwrap();
        assert!(eventually(|| stop.live() == 0), "the owner stayed");
        assert_eq!(
            handle.state(),
            BacklogState::Stopped { reason: "the workspace was taken over" }
        );
        newer.release();
    }

    /// A branch switch marks every file at once. While batches make progress the owner goes
    /// straight on to the next: no idle tick, no pause, between them.
    #[test]
    fn a_large_backlog_drains_without_pauses_between_batches() {
        let dir = tempfile::tempdir().unwrap();
        let files = 3 * bsl_search::POINT_BATCH_KEYS + 5;
        let shared = engine_over(dir.path(), files);
        // An idle tick no ordinary run reaches: an owner that pauses on one between batches
        // never drains. Under `timing_contract` the tick is half a minute and the clock says
        // how long the drain and its widest gap took.
        let idle_tick = test_utils::wait::ceiling(Duration::from_secs(30));
        let pacing = Pacing { idle_tick, ..FAST };
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, pacing);
        for index in 0..files {
            edit(
                &shared,
                &dir.path().join(format!("M{index}.bsl")),
                &format!("Процедура Новая{index}() Экспорт\nКонецПроцедуры\n"),
            );
        }
        let started = Instant::now();
        handle.fresh();
        assert!(eventually(|| backlog(&shared) == 0));
        let drained = started.elapsed();
        let times = handle.probe.publish_times.lock().unwrap().clone();
        assert!(times.len() >= 4, "{} batches for {files} marks", times.len());
        let widest = times.windows(2).map(|pair| pair[1] - pair[0]).max().unwrap();
        if cfg!(timing_contract) {
            assert!(drained < pacing.idle_tick, "the backlog waited on an idle tick");
            // A pause between batches is an idle tick spent; half of one is slower than any
            // machine and faster than the tick.
            assert!(widest < pacing.idle_tick / 2, "a pause of {widest:?} between batches");
            assert!(widest < Duration::from_secs(1), "a pause of {widest:?} between batches");
        }
        finish(&handle, &stop);
    }

    /// A publication that holds the engine past its bound is counted for `search status`; one
    /// inside it is not.
    #[test]
    fn a_publication_past_its_bound_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 2);
        for (slow_hold, counted) in [(Duration::ZERO, true), (Duration::from_secs(30), false)] {
            let (handle, stop) =
                start(&shared, &WorkspaceLease::unmanaged(), None, Pacing { slow_hold, ..FAST });
            edit(
                &shared,
                &dir.path().join("M0.bsl"),
                &format!("Процедура Новая{counted}() Экспорт\nКонецПроцедуры\n"),
            );
            handle.fresh();
            assert!(eventually(|| backlog(&shared) == 0));
            let publishes = handle.probe.publishes.load(Ordering::SeqCst) as u64;
            assert!(publishes > 0);
            assert_eq!(handle.slow_holds(), if counted { publishes } else { 0 });
            finish(&handle, &stop);
        }
    }

    /// A backlog being worked through keeps the backend alive, as a pass of the embedding
    /// driver does; an owner with nothing left to do does not.
    #[test]
    fn a_backlog_in_progress_counts_as_background_work() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 3);
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, FAST);
        let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let (seen, watched) = (Arc::clone(&seen), handle.clone());
            *handle.probe.after_prepare.lock().unwrap() = Some(Arc::new(move || {
                seen.fetch_or(watched.is_active(), Ordering::SeqCst);
            }));
        }
        assert!(!handle.is_active(), "an owner with nothing marked counts as working");
        for index in 0..3 {
            edit(
                &shared,
                &dir.path().join(format!("M{index}.bsl")),
                "Процедура Новая() Экспорт\nКонецПроцедуры\n",
            );
        }
        handle.fresh();
        assert!(eventually(|| backlog(&shared) == 0));
        assert!(seen.load(Ordering::SeqCst), "a batch in progress did not count as work");
        assert!(
            crate::change_hub::test_support::eventually_within(200, || !handle.is_active()),
            "the owner stayed busy"
        );
        finish(&handle, &stop);
    }

    /// The flag drops as soon as the owner waits, however long the wait: a drained backlog does
    /// not hold the process for an idle tick after the work is done.
    #[test]
    fn a_drained_backlog_stops_counting_as_work_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let pacing = Pacing { idle_tick: Duration::from_secs(30), ..FAST };
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, pacing);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Новая() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        assert!(eventually(|| backlog(&shared) == 0));
        assert!(
            crate::change_hub::test_support::eventually_within(200, || !handle.is_active()),
            "an owner waiting out its idle tick still counts as working"
        );
        finish(&handle, &stop);
    }

    /// A stop that arrives while a batch is being prepared is answered between keys: the
    /// owner reads no further file, publishes nothing, and leaves.
    #[test]
    fn a_stop_during_preparation_is_answered_between_keys() {
        let dir = tempfile::tempdir().unwrap();
        let files = 20;
        let shared = engine_over(dir.path(), files);
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, FAST);
        {
            let (asked, stopping) = (handle.clone(), stop.clone());
            *handle.probe.after_capture.lock().unwrap() = Some(Arc::new(move || {
                stopping.stop();
                asked.stop();
            }));
        }
        for index in 0..files {
            edit(
                &shared,
                &dir.path().join(format!("M{index}.bsl")),
                &format!("Процедура Новая{index}() Экспорт\nКонецПроцедуры\n"),
            );
        }
        handle.fresh();
        assert!(eventually(|| stop.live() == 0), "the owner stayed");
        assert_eq!(handle.probe.captures.load(Ordering::SeqCst), 1);
        let prepared = handle.probe.prepared_keys.load(Ordering::SeqCst);
        assert!(prepared <= 1, "{prepared} keys were prepared after the stop");
        assert_eq!(handle.probe.publishes.load(Ordering::SeqCst), 0, "a stopped owner published");
    }

    /// With nothing marked, the owner answers every wake without touching the interprocess
    /// lock: no capture, no publication — however many idle ticks and request wakes pass.
    #[test]
    fn an_empty_backlog_never_takes_the_interprocess_lock() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let lease = WorkspaceLease::claim(dir.path());
        let (handle, stop) = start(&shared, &lease, None, FAST);
        let until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < until {
            handle.wake();
            std::thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(handle.probe.captures.load(Ordering::SeqCst), 0);
        assert_eq!(handle.probe.publishes.load(Ordering::SeqCst), 0);
        finish(&handle, &stop);
    }

    /// A fact arriving after the owner last looked at the backlog and before it waits is not
    /// lost: the wake is taken before the work, so the owner does not sleep out its idle tick
    /// over a mark it was told about.
    #[test]
    fn a_signal_before_the_wait_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 2);
        let pacing = Pacing { idle_tick: Duration::from_secs(30), ..FAST };
        let handle = OverlayBacklog::default();
        let stop = OwnerStop::default();
        let (marker_shared, path) = (Arc::clone(&shared), dir.path().join("M1.bsl"));
        let marker_handle = handle.clone();
        // Armed before the owner starts: its first wait finds the backlog empty, and the mark
        // plus the signal land in exactly the gap between that finding and the wait.
        *handle.probe.before_wait_once.lock().unwrap() = Some(Box::new(move || {
            edit(&marker_shared, &path, "Процедура Поздняя() Экспорт\nКонецПроцедуры\n");
            marker_handle.fresh();
        }));
        assert!(handle.start_with(
            Arc::clone(&shared),
            WorkspaceLease::unmanaged(),
            None,
            stop.clone(),
            pacing
        ));
        assert!(
            eventually(|| found(&shared, "Поздняя")),
            "the owner slept over the mark it was told about"
        );
        finish(&handle, &stop);
    }

    /// A refusal holds the next batch off. Wakes from requests during the backoff take no
    /// batch: they tell the owner nothing new — and when the backoff runs out, the owner's own
    /// retry does take one.
    ///
    /// Both halves are read at the owner itself: the wakes it took at its wait, and the rounds
    /// it was let through to. Neither is inferred from how often this thread called `wake`,
    /// and neither is timed against the deadline: the owner's clock is held still for as long
    /// as the reading takes, so an observer the host deschedules loses nothing but its own
    /// speed. Everything else is the real thing — the owner thread, the engine, and a commit
    /// a real writer lock refuses.
    #[test]
    fn wakes_do_not_step_around_the_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over_held_writer(dir.path(), 1);
        let holder = rusqlite::Connection::open(dir.path().join("search.db")).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let handle = OverlayBacklog::default();
        let clock = Arc::new(test_probe::TestClock::default());
        *handle.probe.clock.lock().unwrap() = Some(Arc::clone(&clock));
        let owner = RunningOwner::start(&handle, &shared, &WorkspaceLease::unmanaged(), FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Занятая() Экспорт\nКонецПроцедуры\n");
        handle.fresh();

        // The refusal, and the deadline the owner published for itself — the same field
        // `search status` reports.
        let mut window = None;
        assert!(
            eventually(|| match handle.state() {
                BacklogState::Backoff { until } => {
                    window = Some(until);
                    true
                }
                _ => false,
            }),
            "the refused commit did not back the owner off: {:?}",
            handle.state(),
        );
        let until = window.expect("the wait above sets it");
        let attempts = handle.probe.admissions.load(Ordering::SeqCst);

        // Five wakes the owner is known to have taken: each one is waited for at the signal,
        // one handshake at a time, because a call to `wake` is not a wake consumed.
        for wake in 1..=5 {
            let taken = handle.wakes_consumed();
            handle.wake();
            assert!(
                eventually(|| handle.wakes_consumed() > taken),
                "wake {wake} never reached the owner",
            );
        }
        // The deschedule this replaces, on purpose: the deadline cannot pass under a stalled
        // observer, so the reading below is the same whenever it is taken; only the timing
        // profile waits a window out before it.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(300));
        }
        assert_eq!(
            handle.probe.admissions.load(Ordering::SeqCst),
            attempts,
            "a wake took the owner into a round with its backoff still to run",
        );
        assert!(
            matches!(handle.state(), BacklogState::Backoff { until: still } if still == until),
            "the owner left the backoff it was waiting out: {:?}",
            handle.state(),
        );

        // And the other half: the deadline passing is what lets the next attempt through.
        // Only the clock moves — no wait is shortened and no delay is raised.
        clock.advance(until.saturating_duration_since(clock.now()) + Duration::from_millis(1));
        handle.wake();
        assert!(
            eventually(|| handle.probe.admissions.load(Ordering::SeqCst) > attempts),
            "the owner never retried after its backoff ran out",
        );
        holder.execute_batch("ROLLBACK").unwrap();
        owner.finish();
    }

    /// The proof above holds the owner's clock still to read its decisions. This one holds
    /// nothing still: on the system clock, over the same refused commit, the owner backs off,
    /// comes back to the batch it kept, and settles it once the writer lets go.
    #[test]
    fn a_refused_commit_is_backed_off_and_returned_to_on_the_system_clock() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over_held_writer(dir.path(), 1);
        let holder = rusqlite::Connection::open(dir.path().join("search.db")).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let handle = OverlayBacklog::default();
        let owner = RunningOwner::start(&handle, &shared, &WorkspaceLease::unmanaged(), FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Занятая() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        assert!(
            eventually(|| matches!(handle.state(), BacklogState::Backoff { .. })),
            "the refused commit did not back the owner off: {:?}",
            handle.state(),
        );
        let attempts = handle.probe.admissions.load(Ordering::SeqCst);
        assert!(
            eventually(|| handle.probe.admissions.load(Ordering::SeqCst) > attempts),
            "the owner never came back to the batch it was holding",
        );
        assert_eq!(backlog(&shared), 1, "the refused batch lost the mark it never settled");
        holder.execute_batch("ROLLBACK").unwrap();
        assert!(
            eventually(|| backlog(&shared) == 0),
            "the owner never settled the batch once the writer let go",
        );
        owner.finish();
    }

    /// An owner whose budget ran out stays out: request wakes do not revive it, and its marks
    /// stay. A fresh fact does.
    #[test]
    fn an_exhausted_owner_is_revived_by_a_fresh_fact_only() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over_held_writer(dir.path(), 1);
        let holder = rusqlite::Connection::open(dir.path().join("search.db")).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let pacing = Pacing { retry_budget: Duration::from_millis(1), ..FAST };
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, pacing);
        edit(
            &shared,
            &dir.path().join("M0.bsl"),
            "Процедура Вернулась() Экспорт\nКонецПроцедуры\n",
        );
        handle.fresh();
        assert!(eventually(|| matches!(handle.state(), BacklogState::Exhausted { .. })));
        holder.execute_batch("ROLLBACK").unwrap();
        let attempts = handle.probe.publishes.load(Ordering::SeqCst);
        // Each wake is waited for at the owner's own wait, so the reading below follows ten
        // rounds the owner has actually turned, not ten intervals it may not have reached.
        for _ in 0..10 {
            let taken = handle.wakes_consumed();
            handle.wake();
            assert!(
                eventually(|| handle.wakes_consumed() > taken),
                "a wake never reached the owner"
            );
        }
        assert_eq!(handle.probe.publishes.load(Ordering::SeqCst), attempts, "a wake revived it");
        assert!(matches!(handle.state(), BacklogState::Exhausted { .. }));
        assert_eq!(backlog(&shared), 1, "the marks went with the budget");

        handle.fresh();
        assert!(eventually(|| found(&shared, "Вернулась")));
        finish(&handle, &stop);
    }

    /// A batch that fails outright is not retried on a schedule of its own: the owner waits for
    /// a fresh fact, however often it is woken.
    #[test]
    fn a_failing_batch_waits_for_a_fresh_fact() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        rusqlite::Connection::open(dir.path().join("search.db"))
            .unwrap()
            .execute_batch("DELETE FROM meta WHERE key = 'baseline_version';")
            .unwrap();
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Отказ() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        assert!(eventually(|| matches!(handle.state(), BacklogState::Exhausted { .. })));
        let rounds = handle.probe.captures.load(Ordering::SeqCst);
        for _ in 0..20 {
            handle.wake();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(handle.probe.captures.load(Ordering::SeqCst), rounds, "the failure was retried");
        finish(&handle, &stop);
    }

    /// A key whose file cannot be read is settled back into its mark every time. That is not
    /// progress: the owner backs off instead of re-reading it in a loop, so the passes over it
    /// are counted by the schedule, not by the speed of the disk. The owner's clock is held
    /// still, so the backoff never runs out under a slow observer and a second read can only
    /// come from an owner that skipped its schedule.
    #[test]
    fn a_key_that_keeps_failing_is_retried_on_the_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let handle = OverlayBacklog::default();
        *handle.probe.clock.lock().unwrap() = Some(Arc::new(test_probe::TestClock::default()));
        let stop = OwnerStop::default();
        assert!(handle.start_with(
            Arc::clone(&shared),
            WorkspaceLease::unmanaged(),
            None,
            stop.clone(),
            FAST
        ));
        let path = dir.path().join("M0.bsl");
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(shared.lock().unwrap().as_ref().unwrap().mark_workspace_path_dirty(&path).unwrap());
        handle.fresh();
        assert!(eventually(|| matches!(handle.state(), BacklogState::Backoff { .. })));
        // An owner that skipped its schedule re-reads at a pace of its own, so only a quiet
        // window shows it; the window is the timing profile's.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(300));
        }
        assert_eq!(handle.probe.captures.load(Ordering::SeqCst), 1, "the failing key was re-read");
        assert_eq!(backlog(&shared), 1, "the failing key lost its mark");
        finish(&handle, &stop);
    }

    /// Phase B reads files with the engine lock given back: a request arriving while the owner
    /// is inside it takes the lock at its first attempt.
    #[test]
    fn a_request_takes_the_engine_while_a_batch_is_being_prepared() {
        struct ParkedSource {
            entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl bsl_search::ModuleSnapshotSource for ParkedSource {
            fn text_and_parse(&self, _: &str) -> bsl_search::SnapshotFetch {
                if let Some(entered) = self.entered.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    let _ = self
                        .release
                        .lock()
                        .unwrap()
                        .recv_timeout(crate::change_hub::test_support::STATE_CEILING);
                }
                bsl_search::SnapshotFetch::Unavailable
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel();
        shared.lock().unwrap().as_mut().unwrap().set_module_snapshot_source(Arc::new(
            ParkedSource { entered: Mutex::new(Some(entered_tx)), release: Mutex::new(release) },
        ));
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Б() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        entered
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the owner reached phase B");

        // The request runs off this thread and is answered while phase B is still parked, so
        // a request that waited for the owner never answers before the release below.
        let (answer_tx, answer) = std::sync::mpsc::channel();
        let request = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let started = Instant::now();
                let acquired = crate::tools::search::try_acquire_engine(
                    &shared,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .is_ok();
                let _ = answer_tx.send((acquired, started.elapsed()));
            })
        };
        let answered = answer.recv_timeout(crate::change_hub::test_support::STATE_CEILING);
        release_tx.send(()).unwrap();
        // Answered before the join: a thread still stuck past the watchdog is left behind
        // rather than waited on without a bound.
        let (acquired, waited) = answered.expect("the request waited behind phase B");
        request.join().unwrap();
        assert!(acquired);
        if cfg!(timing_contract) {
            assert!(
                waited < crate::tools::search::ACQUIRE_POLL,
                "waited {waited:?} behind phase B"
            );
        }
        finish(&handle, &stop);
    }

    /// Another process holding the workspace writer lock refuses the commit, not the reading:
    /// the batch is prepared, its publication is refused and held for the retry, and a stop
    /// during that backoff is answered at once.
    #[test]
    fn a_held_writer_lock_refuses_the_commit_and_a_stop_is_answered_during_the_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let lease = WorkspaceLease::claim(dir.path());
        let held = lease.hold_file_lock_for_test();
        let (handle, stop) = start(&shared, &lease, None, FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Держат() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        assert!(eventually(|| matches!(handle.state(), BacklogState::Backoff { .. })));
        assert_eq!(handle.probe.captures.load(Ordering::SeqCst), 1, "the reading did not run");
        assert!(handle.probe.publishes.load(Ordering::SeqCst) >= 1);
        assert!(!found(&shared, "Держат"), "a refused commit published");
        let asked = Instant::now();
        stop.stop();
        handle.stop();
        assert!(crate::change_hub::test_support::eventually(|| stop.live() == 0));
        if cfg!(timing_contract) {
            assert!(asked.elapsed() < Duration::from_secs(1));
        }
        drop(held);
    }

    /// A commit refused by SQLite's writer lock keeps its prepared batch, and the batch is
    /// offered again after a short wait, not after the backoff a batch that settled nothing
    /// gets: the lock is someone else's and usually brief, and the changed files are ready.
    #[test]
    fn a_busy_commit_is_offered_again_soon() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over_held_writer(dir.path(), 1);
        let held = rusqlite::Connection::open(dir.path().join("search.db")).unwrap();
        held.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Скоро() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        assert!(eventually(|| matches!(handle.state(), BacklogState::Backoff { .. })));
        let BacklogState::Backoff { until } = handle.state() else { unreachable!() };
        let wait = until.saturating_duration_since(Instant::now());
        assert!(wait <= Duration::from_secs(1), "a busy commit waits {wait:?}");
        held.execute_batch("ROLLBACK").unwrap();
        assert!(eventually(|| found(&shared, "Скоро")));
        assert_eq!(handle.probe.captures.load(Ordering::SeqCst), 1, "the batch was read twice");
        finish(&handle, &stop);
    }

    /// A fresh fact that revives an exhausted owner starts a new obligation, and its schedules
    /// start from the beginning: the first busy commit after it waits the short first step,
    /// not where the spent obligation's doubling left off.
    #[test]
    fn a_revived_owner_starts_its_busy_schedule_over() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over_held_writer(dir.path(), 1);
        let holder = rusqlite::Connection::open(dir.path().join("search.db")).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        // Long enough for the doubling to climb before the budget runs out: a delay is capped
        // by what is left of the budget, so a tiny budget would hide any schedule.
        let pacing = Pacing { retry_budget: Duration::from_secs(2), ..FAST };
        let (handle, stop) = start(&shared, &WorkspaceLease::unmanaged(), None, pacing);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Занята() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        assert!(eventually(|| matches!(handle.state(), BacklogState::Exhausted { .. })));
        handle.fresh();
        assert!(eventually(|| matches!(handle.state(), BacklogState::Backoff { .. })));
        let BacklogState::Backoff { until } = handle.state() else { unreachable!() };
        let wait = until.saturating_duration_since(Instant::now());
        assert!(wait <= Duration::from_millis(600), "the revived owner waits {wait:?}");
        holder.execute_batch("ROLLBACK").unwrap();
        finish(&handle, &stop);
    }

    /// The owner's batches continue a fact the consumer already reported, so the embedding
    /// driver is woken but not revived: a driver waiting for a fresh signal keeps waiting.
    #[test]
    fn batches_wake_the_embedding_driver_without_reviving_it() {
        let dir = tempfile::tempdir().unwrap();
        let shared = engine_over(dir.path(), 1);
        let retry =
            super::super::overlay_retry::OverlayRetry::unstarted_for_test(Arc::clone(&shared));
        retry.fail_for_test();
        let (handle, stop) =
            start(&shared, &WorkspaceLease::unmanaged(), Some(Arc::clone(&retry)), FAST);
        edit(&shared, &dir.path().join("M0.bsl"), "Процедура Порция() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        assert!(eventually(|| found(&shared, "Порция")));
        assert!(retry.is_failed(), "a batch revived the embedding driver");
        finish(&handle, &stop);
    }

    /// The point path and the embedding driver's full plan work over the same keys. Neither
    /// rolls the other back: a batch prepared from bytes a later full plan already replaced is
    /// dropped as stale instead of publishing the older read over it, and the entries the point
    /// path leaves lexical-only are given their vectors by the driver's next pass.
    #[test]
    fn point_batches_and_full_plans_do_not_roll_each_other_back() {
        use super::super::test_support::{
            env_lock, mock_embedding_env, mock_semantic_config, spawn_mock_embedding_server,
        };
        let _lock = env_lock();
        let mock = spawn_mock_embedding_server(vec![1.0, 0.0, 0.0]);
        let _env = mock_embedding_env(&mock);
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("A.bsl"), "Процедура Первая() Экспорт\nКонецПроцедуры\n").unwrap();
        let mut engine =
            SearchEngine::new(&workspace.join("search.db"), mock_semantic_config(&mock)).unwrap();
        engine.set_workspace_roots(bsl_search::WorkspaceRoots::build(workspace, workspace, &[]).0);
        engine.enable_workspace_watcher_mode();
        let shared = crate::state::shared_engine(Some(engine));
        let retry = super::super::overlay_retry::OverlayRetry::spawn(
            Arc::clone(&shared),
            crate::state::OwnerStop::default(),
            Arc::new(Mutex::new(super::super::OverlayWarmupState::Pending)),
            Arc::new(Mutex::new(super::super::SemanticRuntimeStatus::Disabled)),
            WorkspaceLease::unmanaged(),
            super::super::bootstrap::DEFAULT_EMBEDDING_PUBLISH_RETRY_BUDGET,
        );
        assert!(eventually(|| retry.pass_count() >= 1));

        // The owner is held right after it prepared A's second version.
        let (parked_tx, parked) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel::<()>();
        let release = Mutex::new(release);
        let handle = OverlayBacklog::default();
        let parked_tx = Mutex::new(parked_tx);
        *handle.probe.after_prepare.lock().unwrap() = Some(Arc::new(move || {
            let _ = parked_tx.lock().unwrap().send(());
            let _ = release
                .lock()
                .unwrap()
                .recv_timeout(crate::change_hub::test_support::STATE_CEILING);
        }));
        let stop = OwnerStop::default();
        assert!(handle.start_with(
            Arc::clone(&shared),
            WorkspaceLease::unmanaged(),
            Some(Arc::clone(&retry)),
            stop.clone(),
            FAST
        ));
        let a = workspace.join("A.bsl");
        edit(&shared, &a, "Процедура Вторая() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        parked
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the owner prepared a batch");

        // Meanwhile A changes again and the driver's full pass publishes the newest bytes. Marks
        // alone are no work of the driver's; a fresh fact after its failure is.
        edit(&shared, &a, "Процедура Третья() Экспорт\nКонецПроцедуры\n");
        let passes = retry.pass_count();
        retry.fail_for_test();
        retry.kick_fresh();
        assert!(eventually(|| retry.pass_count() > passes));
        assert!(found(&shared, "Третья"), "the full pass did not publish the newest bytes");

        // Released, the held batch meets the state the full pass left. Sampled right after the
        // owner's publication, before it wakes the driver — whose next pass would repair a
        // rollback and hide it.
        let (published_tx, published) = std::sync::mpsc::channel();
        let (resume_tx, resume) = std::sync::mpsc::channel::<()>();
        let (published_tx, resume) = (Mutex::new(published_tx), Mutex::new(resume));
        *handle.probe.after_publish.lock().unwrap() = Some(Arc::new(move || {
            let _ = published_tx.lock().unwrap().send(());
            let _ =
                resume.lock().unwrap().recv_timeout(crate::change_hub::test_support::STATE_CEILING);
        }));
        *handle.probe.after_prepare.lock().unwrap() = None;
        release_tx.send(()).unwrap();
        published
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the held batch was published");
        let older = found(&shared, "Вторая");
        let newest = found(&shared, "Третья");
        *handle.probe.after_publish.lock().unwrap() = None;
        resume_tx.send(()).unwrap();
        assert!(!older, "the older read was published over the full pass");
        assert!(newest, "the stale batch rolled the full pass back");
        assert!(eventually(|| backlog(&shared) == 0));

        // A point settlement is lexical; the driver's next pass gives it its vectors.
        let b = workspace.join("B.bsl");
        edit(&shared, &b, "Процедура Четвёртая() Экспорт\nКонецПроцедуры\n");
        handle.fresh();
        retry.kick_fresh();
        assert!(
            eventually(|| {
                let guard = shared.lock().unwrap();
                let engine = guard.as_ref().unwrap();
                let signals = engine.workspace_overlay_retry_signals().unwrap();
                signals.pending_dirty_paths == 0 && signals.unembedded_entries == 0
            }),
            "the point entry never got its vectors"
        );
        assert!(found(&shared, "Четвёртая") && found(&shared, "Третья"));
        retry.stop();
        finish(&handle, &stop);
    }
}
