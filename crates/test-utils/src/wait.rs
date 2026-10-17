//! Bounds on the waits a test makes for a state it goes on to assert.
//!
//! A required test must not turn a slow machine into a failure, so in an ordinary run such a
//! wait has no bound any run reaches: it ends when the awaited state arrives, and a state that
//! never arrives is a hang left to the runner that runs the suite. The bounds that tell a slow wait
//! from a hung one are kept, and enforced only under `--cfg timing_contract`.

use std::time::Duration;

/// A bound past any run: a hundred years. Product code that adds a wait's bound to an
/// `Instant` — a readiness wait, a shutdown budget — takes it without overflow, and every
/// waiting primitive the tests use, from channel receives and condition variables to
/// `tokio::time::timeout`, just waits until the value arrives.
pub const NO_BOUND: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

/// The bound a test puts on a wait for a state it is about to assert on: `bound` under
/// `timing_contract`, and [`NO_BOUND`] otherwise.
pub const fn ceiling(bound: Duration) -> Duration {
    if cfg!(timing_contract) {
        bound
    } else {
        NO_BOUND
    }
}

/// How many polls `step` apart a counted wait of nominal length `bound` may make: the count
/// `bound / step` under `timing_contract`, and no bound ([`u64::MAX`]) otherwise.
pub const fn polls(bound: Duration, step: Duration) -> u64 {
    if !cfg!(timing_contract) {
        return u64::MAX;
    }
    let step = step.as_nanos();
    let count = bound.as_nanos() / if step == 0 { 1 } else { step };
    if count > u64::MAX as u128 {
        u64::MAX
    } else {
        count as u64
    }
}

/// Polls `condition` every `step` until it holds and returns `true`; under `timing_contract`
/// gives up after [`polls`]`(bound, step)` and returns what one last look says.
pub fn poll_until(bound: Duration, step: Duration, mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..polls(bound, step) {
        if condition() {
            return true;
        }
        std::thread::sleep(step);
    }
    condition()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_run_bounds_no_wait() {
        if cfg!(timing_contract) {
            assert_eq!(ceiling(Duration::from_secs(3)), Duration::from_secs(3));
            assert_eq!(polls(Duration::from_secs(3), Duration::from_millis(10)), 300);
        } else {
            assert_eq!(ceiling(Duration::from_secs(3)), NO_BOUND);
            assert_eq!(polls(Duration::from_secs(3), Duration::from_millis(10)), u64::MAX);
        }
    }

    #[test]
    fn an_unbounded_receive_still_returns_what_arrives() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(7).unwrap());
        assert_eq!(rx.recv_timeout(ceiling(Duration::from_secs(60))), Ok(7));
    }

    #[test]
    fn an_unbounded_ceiling_can_be_added_to_an_instant() {
        let now = std::time::Instant::now();
        assert!(now.checked_add(NO_BOUND * 3).is_some());
        assert!(now + ceiling(Duration::from_secs(1)) > now);
    }

    #[test]
    fn a_counted_wait_returns_once_its_condition_holds() {
        let mut seen = 0;
        assert!(poll_until(Duration::from_millis(50), Duration::from_millis(1), || {
            seen += 1;
            seen == 3
        }));
        assert_eq!(seen, 3);
    }
}
