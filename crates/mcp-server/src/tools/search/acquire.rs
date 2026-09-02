use super::types::AcquireFailure;
use bsl_search::SearchEngine;
use rmcp::ErrorData as McpError;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

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
    engine: &'a Arc<Mutex<Option<SearchEngine>>>,
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
    engine: &'a Arc<Mutex<Option<SearchEngine>>>,
    cancel: &CancellationToken,
    max_wait: std::time::Duration,
    poll: std::time::Duration,
) -> Result<MutexGuard<'a, Option<SearchEngine>>, AcquireFailure> {
    let start = std::time::Instant::now();
    loop {
        // Before the lock, not only between polls: a request cancelled before it ever
        // waited must not take a free lock for work nobody will read.
        if cancel.is_cancelled() {
            return Err(AcquireFailure::Cancelled);
        }
        match engine.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(AcquireFailure::Poisoned),
            Err(std::sync::TryLockError::WouldBlock) => {
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
    use super::{acquire_engine_within, try_acquire_engine, AcquireFailure};
    use bsl_search::SearchEngine;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn try_acquire_engine_queues_until_the_lock_frees() {
        let engine: Arc<Mutex<Option<SearchEngine>>> = Arc::new(Mutex::new(None));
        assert!(try_acquire_engine(&engine, &CancellationToken::new()).is_ok());

        const HOLD: Duration = Duration::from_millis(300);
        let held = engine.lock().unwrap();
        let gate = Arc::new(Barrier::new(2));
        let entered = Arc::new(AtomicBool::new(false));
        let probe = {
            let engine = Arc::clone(&engine);
            let gate = Arc::clone(&gate);
            let entered = Arc::clone(&entered);
            std::thread::spawn(move || {
                gate.wait();
                entered.store(true, Ordering::SeqCst);
                let started = Instant::now();
                let acquired = try_acquire_engine(&engine, &CancellationToken::new()).is_ok();
                (acquired, started.elapsed())
            })
        };
        gate.wait();
        std::thread::sleep(HOLD);
        assert!(entered.load(Ordering::SeqCst), "probe must reach the acquire under contention");
        drop(held);
        let (acquired, waited) = probe.join().unwrap();

        assert!(acquired, "acquire must succeed once the lock frees");
        assert!(waited >= HOLD / 2, "acquire returned too fast to have queued: {waited:?}");
    }

    #[test]
    fn acquire_engine_times_out_when_the_lock_stays_held() {
        let engine: Arc<Mutex<Option<SearchEngine>>> = Arc::new(Mutex::new(None));
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
        let engine: Arc<Mutex<Option<SearchEngine>>> = Arc::new(Mutex::new(None));
        let poisoner = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || {
                let _held = engine.lock().unwrap();
                panic!("poison the engine lock");
            })
        };
        assert!(poisoner.join().is_err());
        let started = Instant::now();
        let outcome = acquire_engine_within(
            &engine,
            &CancellationToken::new(),
            Duration::from_secs(30),
            Duration::from_millis(10),
        );

        assert!(matches!(outcome, Err(AcquireFailure::Poisoned)));
        assert!(started.elapsed() < Duration::from_secs(1), "poison must not block on the cap");
    }

    /// A request cancelled while queued on a held lock stops waiting at once: it is the
    /// cancellation, not the holder, that ends the wait. The 30-second cap in production
    /// is for a wedged holder, and a caller that has already gone must not pay it.
    #[test]
    fn acquire_engine_stops_waiting_when_the_request_is_cancelled() {
        let engine: Arc<Mutex<Option<SearchEngine>>> = Arc::new(Mutex::new(None));
        let held = engine.lock().unwrap();
        let cancel = CancellationToken::new();
        let canceller = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                cancel.cancel();
            })
        };
        let started = Instant::now();
        let outcome = acquire_engine_within(
            &engine,
            &cancel,
            Duration::from_secs(2),
            Duration::from_millis(10),
        );
        let waited = started.elapsed();
        drop(held);
        canceller.join().unwrap();

        assert!(
            matches!(outcome, Err(AcquireFailure::Cancelled)),
            "a cancelled wait acquires nothing"
        );
        assert!(
            waited < Duration::from_millis(500),
            "the wait must end at the cancellation, not at the cap: {waited:?}"
        );
    }

    /// A token already cancelled before the wait begins is answered without a single poll,
    /// and without touching a free lock: nothing of a cancelled request runs.
    #[test]
    fn a_pre_cancelled_request_never_takes_a_free_lock() {
        let engine: Arc<Mutex<Option<SearchEngine>>> = Arc::new(Mutex::new(None));
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
