use std::time::{Duration, Instant};

pub(crate) const DEFAULT_RETRY_BUDGET: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryOwner {
    Startup,
    ChangeHub,
    Drift,
    OverlayEmbedding,
    Graph,
}

impl RetryOwner {
    fn may_rearm_on_fresh_work(self) -> bool {
        !matches!(self, Self::Startup)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryStop {
    Exhausted,
    OperationError,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    RetryAfter(Duration),
    Stop(RetryStop),
}

pub(crate) struct RetryWindow {
    owner: RetryOwner,
    budget: Duration,
    deadline: Option<Instant>,
    streak: u32,
    stopped: Option<RetryStop>,
}

impl RetryWindow {
    pub(crate) fn new(owner: RetryOwner) -> Self {
        Self::with_budget(owner, DEFAULT_RETRY_BUDGET)
    }

    pub(crate) fn with_budget(owner: RetryOwner, budget: Duration) -> Self {
        Self { owner, budget, deadline: None, streak: 0, stopped: None }
    }

    pub(crate) fn streak(&self) -> u32 {
        self.streak
    }

    pub(crate) fn refused(&mut self, now: Instant, delay: Duration) -> RetryDecision {
        if let Some(reason) = self.stopped {
            return RetryDecision::Stop(reason);
        }

        let deadline = match self.deadline {
            Some(deadline) => deadline,
            None => match now.checked_add(self.budget) {
                Some(deadline) => {
                    self.deadline = Some(deadline);
                    deadline
                }
                None => return self.stop(RetryStop::Exhausted),
            },
        };
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return self.stop(RetryStop::Exhausted);
        };
        if remaining.is_zero() {
            return self.stop(RetryStop::Exhausted);
        }

        self.streak = self.streak.saturating_add(1);
        RetryDecision::RetryAfter(delay.min(remaining))
    }

    pub(crate) fn expired(&mut self, now: Instant) -> bool {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.stop(RetryStop::Exhausted);
            true
        } else {
            false
        }
    }

    pub(crate) fn complete(&mut self) {
        self.deadline = None;
        self.streak = 0;
        self.stopped = None;
    }

    /// Returns true only when fresh external work starts a new obligation.
    pub(crate) fn observe_external_work(&mut self, genuinely_fresh: bool) -> bool {
        if genuinely_fresh
            && matches!(self.stopped, Some(RetryStop::Exhausted | RetryStop::OperationError))
            && self.owner.may_rearm_on_fresh_work()
        {
            self.deadline = None;
            self.streak = 0;
            self.stopped = None;
            return true;
        }
        false
    }

    pub(crate) fn operation_error(&mut self) -> RetryDecision {
        self.stop(RetryStop::OperationError)
    }

    pub(crate) fn terminal(&mut self) -> RetryDecision {
        self.stop(RetryStop::Terminal)
    }

    fn stop(&mut self, reason: RetryStop) -> RetryDecision {
        self.stopped = Some(reason);
        RetryDecision::Stop(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_OWNERS: [RetryOwner; 5] = [
        RetryOwner::Startup,
        RetryOwner::ChangeHub,
        RetryOwner::Drift,
        RetryOwner::OverlayEmbedding,
        RetryOwner::Graph,
    ];
    const BACKGROUND_OWNERS: [RetryOwner; 4] =
        [RetryOwner::ChangeHub, RetryOwner::Drift, RetryOwner::OverlayEmbedding, RetryOwner::Graph];

    #[test]
    fn all_retry_owners_preserve_deadline_and_streak_when_coalescing() {
        let start = Instant::now();
        for owner in ALL_OWNERS {
            let mut window = RetryWindow::new(owner);
            assert_eq!(
                window.refused(start, Duration::from_secs(2)),
                RetryDecision::RetryAfter(Duration::from_secs(2))
            );
            let deadline = window.deadline;

            assert!(!window.observe_external_work(true), "{owner:?}");
            assert_eq!(window.deadline, deadline, "{owner:?}");
            assert_eq!(window.streak(), 1, "{owner:?}");
        }
    }

    #[test]
    fn eligible_background_owners_rearm_only_on_fresh_external_work() {
        let start = Instant::now();
        for owner in BACKGROUND_OWNERS {
            let mut window = RetryWindow::new(owner);
            assert!(matches!(window.refused(start, Duration::ZERO), RetryDecision::RetryAfter(_)));
            assert_eq!(
                window.refused(start + DEFAULT_RETRY_BUDGET, Duration::ZERO),
                RetryDecision::Stop(RetryStop::Exhausted)
            );

            assert!(!window.observe_external_work(false), "{owner:?}");
            assert!(window.observe_external_work(true), "{owner:?}");
            assert!(!window.observe_external_work(true), "{owner:?}");
            assert_eq!(window.streak(), 0, "{owner:?}");
            assert_eq!(window.deadline, None, "{owner:?}");
        }

        let mut startup = RetryWindow::new(RetryOwner::Startup);
        assert!(matches!(startup.refused(start, Duration::ZERO), RetryDecision::RetryAfter(_)));
        assert_eq!(
            startup.refused(start + DEFAULT_RETRY_BUDGET, Duration::ZERO),
            RetryDecision::Stop(RetryStop::Exhausted)
        );
        assert!(!startup.observe_external_work(true));
    }

    #[test]
    fn all_retry_owners_stop_on_operation_error() {
        let now = Instant::now();
        for owner in ALL_OWNERS {
            let mut window = RetryWindow::new(owner);
            assert_eq!(
                window.operation_error(),
                RetryDecision::Stop(RetryStop::OperationError),
                "{owner:?}"
            );
            assert_eq!(
                window.refused(now, Duration::ZERO),
                RetryDecision::Stop(RetryStop::OperationError),
                "{owner:?}"
            );
            assert_eq!(window.observe_external_work(true), owner.may_rearm_on_fresh_work());

            let mut terminal = RetryWindow::new(owner);
            assert_eq!(terminal.terminal(), RetryDecision::Stop(RetryStop::Terminal));
            assert_eq!(
                terminal.refused(now, Duration::ZERO),
                RetryDecision::Stop(RetryStop::Terminal),
                "{owner:?}"
            );
        }
    }
}
