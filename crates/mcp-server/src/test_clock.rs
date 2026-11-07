use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A clock a test can hold still.
///
/// A deadline is an instant the owner computes for itself, so a test that wants to see what the
/// owner does *before* that deadline otherwise has to outrun it — and loses that race whenever
/// the host deschedules the observing thread. Holding the clock still ends the race without
/// touching a single production duration: the waits, the busy timeout and the delays are all
/// the ones production uses, and the decision the test checks is still the owner's own.
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
        self.advanced.fetch_add(u64::try_from(by.as_nanos()).unwrap_or(u64::MAX), Ordering::SeqCst);
    }
}
