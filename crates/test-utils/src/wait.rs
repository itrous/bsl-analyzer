//! Bounds on the waits a test makes for a state it goes on to assert.
//!
//! A required test must not turn a slow machine into a failure, so an ordinary run judges no
//! wait by a timing: a wait ends when the awaited state arrives. [`ceiling`] keeps the bounds
//! that tell a slow wait from a hung one, enforced only under `--cfg timing_contract`.
//! [`hang_guard`] bounds a wait against a state that never arrives: in an ordinary run it is
//! far past any healthy wait, yet finite, so a hang fails the test with a message instead of
//! stalling the runner.

use std::time::Duration;

/// A bound past any run: a hundred years. Product code that adds a wait's bound to an
/// `Instant` — a readiness wait, a shutdown budget — takes it without overflow, and every
/// waiting primitive the tests use, from channel receives and condition variables to
/// `tokio::time::timeout`, just waits until the value arrives.
pub const NO_BOUND: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

/// A finite bound for a wait whose only other way to end is the awaited state: ten minutes.
pub const HANG_GUARD: Duration = Duration::from_secs(10 * 60);

/// The bound on a wait that would otherwise only end on success: `bound` under
/// `timing_contract`; in an ordinary run the larger of `bound` and [`HANG_GUARD`] —
/// never stricter than the timing profile, never unbounded.
pub const fn hang_guard(bound: Duration) -> Duration {
    if cfg!(timing_contract) || bound.as_nanos() > HANG_GUARD.as_nanos() {
        bound
    } else {
        HANG_GUARD
    }
}

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

/// Polls `condition` every `step` until it holds and returns `true`; gives up at
/// [`hang_guard`]`(bound)` in ordinary runs, or the timing profile's poll count,
/// and takes one last look.
pub fn poll_until(bound: Duration, step: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + hang_guard(bound);
    for _ in 0..polls(bound, step) {
        if condition() {
            return true;
        }
        if cfg!(timing_contract) {
            std::thread::sleep(step);
        } else {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(step.min(remaining));
        }
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
    fn a_hang_guard_is_finite_and_never_stricter_than_the_timing_profile() {
        const MINUTE: Duration = Duration::from_secs(60);
        assert!(HANG_GUARD <= 30 * MINUTE);
        for bound in [Duration::ZERO, Duration::from_secs(1), MINUTE, 10 * MINUTE, 20 * MINUTE] {
            if cfg!(timing_contract) {
                assert_eq!(hang_guard(bound), bound);
            } else {
                assert_eq!(hang_guard(bound), bound.max(HANG_GUARD), "bound {bound:?}");
                if bound <= HANG_GUARD {
                    assert!(hang_guard(bound) <= 30 * MINUTE, "bound {bound:?}");
                }
            }
        }
    }

    #[cfg(timing_contract)]
    #[test]
    #[should_panic(expected = "hang guard: unreachable state never arrived")]
    fn an_unreachable_state_fails_with_a_message() {
        let (_sender, receiver) = std::sync::mpsc::channel::<()>();
        assert!(
            receiver.recv_timeout(hang_guard(Duration::ZERO)).is_ok(),
            "hang guard: unreachable state never arrived"
        );
    }

    #[test]
    fn a_guarded_receive_still_returns_what_arrives() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(7).unwrap());
        assert_eq!(rx.recv_timeout(hang_guard(Duration::from_secs(60))), Ok(7));
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
