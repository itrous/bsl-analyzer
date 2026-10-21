use super::types::AcquireFailure;
use crate::state::SharedSearchEngine;
use bsl_search::SearchEngine;
use rmcp::ErrorData as McpError;
use std::collections::VecDeque;
use std::sync::{Condvar, LockResult, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// The search engine behind a strict first-come queue.
///
/// `std::sync::Mutex` promises no order, and a background owner that re-locks in a tight loop
/// can keep winning it while a request waits: the wait is then not the sum of the holds ahead
/// of it, but unbounded. So every acquisition — blocking, trying, or a request's cancellable
/// wait — takes a ticket first, and only the ticket at the head of the queue touches the raw
/// mutex. Order is then the order tickets were taken; how long a waiter waits is still the sum
/// of the holds ahead of it, which is why the long ones are bounded where they are taken.
///
/// The raw mutex is private to this type, so no path can reach it without the queue.
pub(crate) struct AdmittedEngine {
    admission: Mutex<Admission>,
    turn: Condvar,
    engine: Mutex<Option<SearchEngine>>,
    /// The daemon is shutting down: background owners stop waiting for the engine.
    closing: std::sync::atomic::AtomicBool,
    /// Run between the last look at the stop and the attempt on the lock, so a test can put
    /// the race that window is there for — a stop raised in it — where it can be observed.
    /// A window one instruction wide is no less real for being narrow.
    #[cfg(test)]
    between_the_look_and_the_lock: Mutex<Option<Box<dyn Fn() + Send>>>,
}

/// Why a background owner did not get the engine.
#[derive(Debug)]
pub(crate) enum OwnerLockRefused {
    /// The daemon is shutting down; the owner is to leave, not to wait out a hold.
    Closing,
    /// A prior holder panicked.
    Poisoned,
}

/// What a background owner waits on besides the queue: its own stop. Passed in rather than read
/// from a flag of the engine's own, so one call answers both "the engine is closing" and "this
/// daemon is stopping" — and the order the two are raised in stops mattering.
pub(crate) trait OwnerWait {
    fn stopped(&self) -> bool;
}

impl std::fmt::Display for OwnerLockRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Closing => "the daemon is shutting down",
            Self::Poisoned => "a prior holder of the engine panicked",
        })
    }
}

#[derive(Default)]
struct Admission {
    next: u64,
    queue: VecDeque<u64>,
    /// Threads inside the raw `lock()` right now, for a test to prove only the head is there.
    #[cfg(test)]
    raw_waiters: usize,
    #[cfg(test)]
    most_raw_waiters: usize,
    #[cfg(test)]
    served: Vec<u64>,
}

/// A place in the queue, given up on every way out.
struct Ticket<'a> {
    engine: &'a AdmittedEngine,
    number: u64,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        let mut admission = self.engine.admission();
        admission.queue.retain(|number| *number != self.number);
        drop(admission);
        self.engine.turn.notify_all();
    }
}

impl AdmittedEngine {
    pub(crate) fn new(engine: Option<SearchEngine>) -> Self {
        Self {
            admission: Mutex::new(Admission::default()),
            turn: Condvar::new(),
            engine: Mutex::new(engine),
            closing: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            between_the_look_and_the_lock: Mutex::new(None),
        }
    }

    /// Put something in the window between the last look at the stop and the attempt on the
    /// lock. One-shot, per engine.
    #[cfg(test)]
    pub(crate) fn race_the_lock_with(&self, hook: impl Fn() + Send + 'static) {
        *self.between_the_look_and_the_lock.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Box::new(hook));
    }

    /// Stop every background owner's wait for the engine: from now on
    /// [`Self::acquire_for_owner`] refuses, in the queue and at its head alike.
    pub(crate) fn close(&self) {
        self.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        self.turn.notify_all();
    }

    fn is_closing(&self) -> bool {
        self.closing.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Take the engine in queue order for a background owner, which a shutdown must be able
    /// to call off: the wait asks every [`ACQUIRE_POLL`] whether the daemon is closing, and
    /// gives its place up if so. At the head of the queue it polls the lock rather than
    /// parking in it — a parked lock is woken by nothing but its holder.
    pub(crate) fn acquire_for_owner(
        &self,
        stop: &dyn OwnerWait,
    ) -> Result<MutexGuard<'_, Option<SearchEngine>>, OwnerLockRefused> {
        let leave = || self.is_closing() || stop.stopped();
        let ticket = self.ticket();
        if !self.wait_turn(&ticket, leave) {
            return Err(OwnerLockRefused::Closing);
        }
        loop {
            if leave() {
                return Err(OwnerLockRefused::Closing);
            }
            #[cfg(test)]
            if let Some(hook) = self
                .between_the_look_and_the_lock
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                hook();
            }
            match self.engine.try_lock() {
                Ok(guard) => {
                    // Asked again with the lock in hand: the last look was before the try,
                    // and a stop raised in between would otherwise be answered by taking the
                    // engine and writing through it — one more publication after the daemon
                    // said goodbye. Giving the guard back here is what makes `close()` mean
                    // "refuses from now on" even when the engine is free.
                    if leave() {
                        drop(guard);
                        return Err(OwnerLockRefused::Closing);
                    }
                    #[cfg(test)]
                    self.note_served(&ticket);
                    drop(ticket);
                    return Ok(guard);
                }
                Err(TryLockError::Poisoned(_)) => return Err(OwnerLockRefused::Poisoned),
                Err(TryLockError::WouldBlock) => std::thread::sleep(ACQUIRE_POLL),
            }
        }
    }

    fn admission(&self) -> MutexGuard<'_, Admission> {
        self.admission.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ticket(&self) -> Ticket<'_> {
        let mut admission = self.admission();
        let number = admission.next;
        admission.next += 1;
        admission.queue.push_back(number);
        Ticket { engine: self, number }
    }

    /// Wait until `ticket` heads the queue, or until `stop` says to give up.
    fn wait_turn(&self, ticket: &Ticket<'_>, mut stop: impl FnMut() -> bool) -> bool {
        let mut admission = self.admission();
        loop {
            if admission.queue.front() == Some(&ticket.number) {
                return true;
            }
            if stop() {
                return false;
            }
            admission = self
                .turn
                .wait_timeout(admission, ACQUIRE_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    #[cfg(test)]
    fn note_served(&self, ticket: &Ticket<'_>) {
        self.admission().served.push(ticket.number);
    }

    /// Take the engine in queue order, blocking, with nothing able to call the wait off.
    ///
    /// The daemon's shutdown is the ONE caller: it takes the engine to drop it, and by then
    /// every owner has been told to leave, so the only hold it can wait on is a request's.
    /// Background owners take [`Self::acquire_for_owner`] instead — a wait no stop can reach is
    /// how a shutdown turns into a thirty-second one.
    pub(crate) fn take_for_shutdown(&self) -> LockResult<MutexGuard<'_, Option<SearchEngine>>> {
        self.lock_blocking()
    }

    /// Take the engine in queue order, blocking. Test-only: production has exactly two ways in
    /// — a request with its cancellation, and an owner with its stop.
    #[cfg(test)]
    pub(crate) fn lock(&self) -> LockResult<MutexGuard<'_, Option<SearchEngine>>> {
        self.lock_blocking()
    }

    fn lock_blocking(&self) -> LockResult<MutexGuard<'_, Option<SearchEngine>>> {
        let ticket = self.ticket();
        self.wait_turn(&ticket, || false);
        #[cfg(test)]
        {
            let mut admission = self.admission();
            admission.raw_waiters += 1;
            admission.most_raw_waiters = admission.most_raw_waiters.max(admission.raw_waiters);
        }
        let guard = self.engine.lock();
        #[cfg(test)]
        {
            self.admission().raw_waiters -= 1;
            self.note_served(&ticket);
        }
        // Leaving the queue only now: the next head may start waiting on the raw mutex, and
        // it is the only one that does.
        drop(ticket);
        guard
    }

    /// Observe the engine without waiting or overtaking an admitted caller.
    pub(crate) fn try_snapshot(&self) -> Option<MutexGuard<'_, Option<SearchEngine>>> {
        let admission = self.admission.try_lock().ok()?;
        if !admission.queue.is_empty() {
            return None;
        }
        self.engine.try_lock().ok()
    }

    /// Take the engine only if nobody is queued for it and nobody holds it.
    #[cfg(test)]
    pub(crate) fn try_lock(
        &self,
    ) -> Result<
        MutexGuard<'_, Option<SearchEngine>>,
        TryLockError<MutexGuard<'_, Option<SearchEngine>>>,
    > {
        let ticket = self.ticket();
        if self.admission().queue.front() != Some(&ticket.number) {
            return Err(TryLockError::WouldBlock);
        }
        let taken = self.engine.try_lock();
        #[cfg(test)]
        if taken.is_ok() {
            self.note_served(&ticket);
        }
        taken
    }

    /// How many acquisitions hold a ticket right now.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.admission().queue.len()
    }

    /// How many threads are inside the raw lock right now and not yet through it.
    #[cfg(test)]
    pub(crate) fn raw_waiting(&self) -> usize {
        self.admission().raw_waiters
    }

    /// Everything that took the engine in the order it took it, and the most threads that
    /// were ever inside the raw lock at once.
    #[cfg(test)]
    pub(crate) fn admission_record(&self) -> (Vec<u64>, usize) {
        let admission = self.admission();
        (admission.served.clone(), admission.most_raw_waiters)
    }
}

/// A poisoned engine lock means a prior operation panicked mid-search; the engine state may be
/// inconsistent and retrying is futile, so this is a hard internal error rather than the
/// "warming up / try again" advice a transient state would warrant.
pub(super) fn engine_lock_poisoned_error() -> McpError {
    McpError::internal_error(
        "search engine lock is poisoned (a prior operation panicked); restart the MCP server"
            .to_owned(),
        None,
    )
}

/// How often a waiting request looks at the lock and at its cancellation token. Bounds the
/// latency of a cancellation observed while waiting; the wait itself ends the moment the
/// lock frees.
pub(crate) const ACQUIRE_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Acquire the engine guard on behalf of ONE request, *blocking* (queueing) on contention
/// instead of bailing out, and giving up the moment that request is cancelled.
///
/// The engine owns a `!Sync` rusqlite connection, so every search must serialize on this lock
/// — that serialization is mandatory, not a coarseness to widen away (see
/// [`crate::state::SharedSearchEngine`]). What this MUST NOT do is surface ordinary contention
/// as a failure: an overlay prime, or a peer search inside its (now tightly bounded) embedding
/// round-trip, holds the lock for seconds, and a short `try_lock` budget turned that into a
/// misleading "overlay warming up" for every other `search_code` in a concurrent batch. So we
/// wait for the lock and return real results once it frees. Polling (rather than parking) keeps
/// the brief sleeps on the `spawn_blocking` thread without pulling in a timed-lock dependency,
/// and it is also what lets the wait observe the request's cancellation between polls: a
/// parked `lock()` cannot be woken by anything but the holder.
///
/// This is the only way a request path takes the engine lock. Background writers have no
/// request to be cancelled by and keep their plain `lock()`.
pub(crate) fn try_acquire_engine<'a>(
    engine: &'a SharedSearchEngine,
    cancel: &CancellationToken,
) -> Result<MutexGuard<'a, Option<SearchEngine>>, AcquireFailure> {
    // Bounds a pathological hang (a deadlock bug, a never-returning holder) without ever
    // tripping on the ordinary multi-second holds — an overlay prime or a slow embed. The query
    // embed runs off the lock and is itself capped (see `Embedder::INTERACTIVE_TIMEOUT`), so
    // this cap only ever fires on a real stall, never on a routine concurrent search.
    const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
    acquire_engine_within(engine, cancel, MAX_WAIT, ACQUIRE_POLL)
}

/// The acquire loop, parameterized over the wait budget so tests can exercise the timeout path
/// without a 30-second sleep. Production callers go through [`try_acquire_engine`].
pub(super) fn acquire_engine_within<'a>(
    engine: &'a SharedSearchEngine,
    cancel: &CancellationToken,
    max_wait: Duration,
    poll: Duration,
) -> Result<MutexGuard<'a, Option<SearchEngine>>, AcquireFailure> {
    // Before the queue, not only while in it: a request cancelled before it ever waited must
    // not take a free lock for work nobody will read.
    if cancel.is_cancelled() {
        return Err(AcquireFailure::Cancelled);
    }
    let start = Instant::now();
    let ticket = engine.ticket();
    let mut failure = None;
    let admitted = engine.wait_turn(&ticket, || {
        if cancel.is_cancelled() {
            failure = Some(AcquireFailure::Cancelled);
        } else if start.elapsed() >= max_wait {
            failure = Some(AcquireFailure::TimedOut);
        }
        failure.is_some()
    });
    if !admitted {
        return Err(failure.unwrap_or(AcquireFailure::TimedOut));
    }
    // At the head, the wait is for the current holder alone; polled rather than parked so a
    // cancellation still ends it within one poll.
    loop {
        if cancel.is_cancelled() {
            return Err(AcquireFailure::Cancelled);
        }
        match engine.engine.try_lock() {
            Ok(guard) => {
                // The same re-check the owner's acquire makes: a request cancelled while the
                // lock was being taken must not hold it — the whole point of the cancellable
                // acquire is that a client who left waits for nobody.
                if cancel.is_cancelled() {
                    drop(guard);
                    return Err(AcquireFailure::Cancelled);
                }
                #[cfg(test)]
                engine.note_served(&ticket);
                return Ok(guard);
            }
            Err(TryLockError::Poisoned(_)) => return Err(AcquireFailure::Poisoned),
            Err(TryLockError::WouldBlock) => {
                if start.elapsed() >= max_wait {
                    return Err(AcquireFailure::TimedOut);
                }
                std::thread::sleep(poll);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn telemetry_snapshot_never_waits_or_overtakes() {
        let engine = super::AdmittedEngine::new(None);
        let admission = engine.admission.lock().unwrap();
        assert!(engine.try_snapshot().is_none());
        drop(admission);
        let ticket = engine.ticket();
        assert!(engine.try_snapshot().is_none());
        drop(ticket);
        let held = engine.engine.lock().unwrap();
        assert!(engine.try_snapshot().is_none());
        drop(held);
        assert!(engine.try_snapshot().is_some());
    }

    use super::{acquire_engine_within, try_acquire_engine, AcquireFailure};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    /// A free engine is not an exception to the refusal. The queue and the head's poll close
    /// the case "somebody else is holding it"; this is the other one — the lock is there for
    /// the taking, and the stop (or the cancellation) arrived while it was being taken. An
    /// owner that takes it anyway writes into the workspace after the daemon said goodbye,
    /// and a cancelled request holds the engine nobody is waiting on any more.
    #[test]
    fn a_free_engine_is_still_refused_once_the_stop_or_the_cancel_is_in() {
        let engine = crate::state::shared_engine(None);

        let stop = crate::state::OwnerStop::default();
        assert!(engine.acquire_for_owner(&stop).is_ok(), "control: a free engine is taken");

        // The race itself: the owner looked, saw no stop, and the stop lands before it takes
        // the lock. Without the second look it would hold the engine and publish through it.
        let racing = stop.clone();
        engine.race_the_lock_with(move || racing.stop());
        assert!(
            matches!(engine.acquire_for_owner(&stop), Err(super::OwnerLockRefused::Closing)),
            "a stop raised while the lock was being taken did not refuse it"
        );
        assert!(
            matches!(engine.acquire_for_owner(&stop), Err(super::OwnerLockRefused::Closing)),
            "and it stays refused afterwards"
        );

        let cancel = CancellationToken::new();
        assert!(try_acquire_engine(&engine, &cancel).is_ok(), "control: a live request is served");
        cancel.cancel();
        assert!(
            matches!(try_acquire_engine(&engine, &cancel), Err(AcquireFailure::Cancelled)),
            "a cancelled request took a free engine"
        );

        // And the engine itself is free afterwards: a refusal that kept the guard would be a
        // deadlock dressed as a refusal.
        assert!(engine.lock().is_ok());
    }

    /// Every acquisition goes through the queue: acquisitions happen in the order tickets
    /// were taken, and only the head of the queue ever waits inside the raw mutex. A queue
    /// consulted only BEFORE the raw lock lets every waiter pile up in it, where the mutex
    /// hands the lock out in whatever order it likes.
    #[test]
    fn acquisitions_are_served_in_ticket_order_with_one_raw_waiter() {
        let engine = crate::state::shared_engine(None);
        let held = engine.lock().unwrap();
        let mut waiters = Vec::new();
        for index in 0..6 {
            let waiter = Arc::clone(&engine);
            waiters.push(std::thread::spawn(move || {
                // Blocking waiters first: those are the ones that reach the raw lock, and a
                // request's poll behind them joins the same queue.
                if index < 5 {
                    drop(waiter.lock().unwrap());
                } else {
                    let acquired = try_acquire_engine(&waiter, &CancellationToken::new());
                    drop(acquired.ok().expect("acquired"));
                }
            }));
            // Each is waiting — queued, or inside the raw lock — before the next one starts.
            let waiting = index + 1;
            assert!(crate::change_hub::test_support::eventually(|| {
                engine.queued() + engine.raw_waiting() >= waiting
            }));
        }
        drop(held);
        for waiter in waiters {
            crate::test_wait::join(waiter, "queued acquisition finishes").unwrap();
        }
        let (served, most_raw_waiters) = engine.admission_record();
        let mut ordered = served.clone();
        ordered.sort_unstable();
        assert_eq!(served, ordered, "the engine was handed out out of ticket order");
        assert!(most_raw_waiters <= 1, "{most_raw_waiters} threads waited inside the raw lock");
    }

    /// A request that gives up while queued leaves the queue: the ones behind it are served.
    #[test]
    fn a_cancelled_waiter_gives_its_place_up() {
        let engine = crate::state::shared_engine(None);
        let held = engine.lock().unwrap();
        let cancel = CancellationToken::new();
        let quitter = {
            let engine = Arc::clone(&engine);
            let cancel = cancel.clone();
            std::thread::spawn(move || try_acquire_engine(&engine, &cancel).is_err())
        };
        assert!(crate::change_hub::test_support::eventually(|| { engine.queued() == 1 }));
        let follower = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || drop(engine.lock().unwrap()))
        };
        assert!(crate::change_hub::test_support::eventually(|| { engine.queued() == 2 }));
        cancel.cancel();
        assert!(
            crate::test_wait::join(quitter, "cancelled acquisition leaves the queue").unwrap(),
            "the cancelled request acquired the engine"
        );
        drop(held);
        crate::test_wait::join(follower, "following acquisition completes").unwrap();
        assert_eq!(engine.queued(), 0);
    }

    #[test]
    fn try_acquire_engine_queues_until_the_lock_frees() {
        let engine: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        assert!(try_acquire_engine(&engine, &CancellationToken::new()).is_ok());

        const HOLD: Duration = Duration::from_millis(300);
        let held = engine.lock().unwrap();
        let entered = Arc::new(AtomicBool::new(false));
        let probe = {
            let engine = Arc::clone(&engine);
            let entered = Arc::clone(&entered);
            std::thread::spawn(move || {
                entered.store(true, Ordering::SeqCst);
                let started = Instant::now();
                let acquired = try_acquire_engine(&engine, &CancellationToken::new()).is_ok();
                (acquired, started.elapsed())
            })
        };
        assert!(
            crate::change_hub::test_support::eventually(|| engine.queued() == 1),
            "probe must queue on the held lock"
        );
        assert!(entered.load(Ordering::SeqCst), "probe must reach the acquire under contention");
        assert!(!probe.is_finished(), "acquire returned while the lock was still held");
        if cfg!(timing_contract) {
            std::thread::sleep(HOLD);
        }
        drop(held);
        let (acquired, waited) =
            crate::test_wait::join(probe, "queued acquisition completes").unwrap();

        assert!(acquired, "acquire must succeed once the lock frees");
        if cfg!(timing_contract) {
            assert!(waited >= HOLD / 2, "acquire returned too fast to have queued: {waited:?}");
        }
    }

    #[test]
    fn acquire_engine_times_out_when_the_lock_stays_held() {
        let engine: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        let held = engine.lock().unwrap();
        let cap = Duration::from_millis(150);
        let started = Instant::now();
        let outcome = acquire_engine_within(
            &engine,
            &CancellationToken::new(),
            cap,
            Duration::from_millis(10),
        );
        let waited = started.elapsed();
        drop(held);

        assert!(matches!(outcome, Err(AcquireFailure::TimedOut)));
        assert!(waited >= cap, "must wait out the cap before giving up: {waited:?}");
    }

    #[test]
    fn acquire_engine_reports_poison_immediately() {
        let engine: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        let poisoner = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || {
                let _held = engine.lock().unwrap();
                panic!("poison the engine lock");
            })
        };
        assert!(crate::test_wait::join(poisoner, "poisoning thread panics").is_err());
        // The thirty-second timing bound is a finite ten-minute hang guard in ordinary runs;
        // a poisoned lock must answer immediately in both profiles.
        let cap = test_utils::wait::hang_guard(Duration::from_secs(30));
        let started = Instant::now();
        let outcome = acquire_engine_within(
            &engine,
            &CancellationToken::new(),
            cap,
            Duration::from_millis(10),
        );
        let waited = started.elapsed();

        assert!(matches!(outcome, Err(AcquireFailure::Poisoned)));
        if cfg!(timing_contract) {
            // Half the cap: an acquire that waited the cap out is caught, a slow machine is not.
            assert!(waited < cap / 2, "poison must not block on the cap: {waited:?}");
            assert!(
                waited < Duration::from_secs(1),
                "poison must not block on the cap: {waited:?}"
            );
        }
    }

    /// A request cancelled while queued on a held lock stops waiting at once: it is the
    /// cancellation, not the holder, that ends the wait. The 30-second cap in production
    /// is for a wedged holder, and a caller that has already gone must not pay it.
    #[test]
    fn acquire_engine_stops_waiting_when_the_request_is_cancelled() {
        let engine: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        let held = engine.lock().unwrap();
        let cancel = CancellationToken::new();
        // The pause that has the request queued before it is cancelled is the timing profile's;
        // a cancellation that lands first must end the wait just the same. The cap is one no
        // ordinary run waits until cancellation, with the finite guard only as a backstop.
        let canceller = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                if cfg!(timing_contract) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                cancel.cancel();
            })
        };
        let started = Instant::now();
        let outcome = acquire_engine_within(
            &engine,
            &cancel,
            test_utils::wait::hang_guard(Duration::from_secs(2)),
            Duration::from_millis(10),
        );
        let waited = started.elapsed();
        drop(held);
        crate::test_wait::join(canceller, "cancellation thread exits").unwrap();

        assert!(
            matches!(outcome, Err(AcquireFailure::Cancelled)),
            "a cancelled wait acquires nothing"
        );
        if cfg!(timing_contract) {
            assert!(
                waited < Duration::from_millis(500),
                "the wait must end at the cancellation, not at the cap: {waited:?}"
            );
        }
    }

    /// A token already cancelled before the wait begins is answered without a single poll,
    /// and without touching a free lock: nothing of a cancelled request runs.
    #[test]
    fn a_pre_cancelled_request_never_takes_a_free_lock() {
        let engine: crate::state::SharedSearchEngine = crate::state::shared_engine(None);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = acquire_engine_within(
            &engine,
            &cancel,
            Duration::from_secs(2),
            Duration::from_millis(10),
        );
        assert!(matches!(outcome, Err(AcquireFailure::Cancelled)));
    }
}
