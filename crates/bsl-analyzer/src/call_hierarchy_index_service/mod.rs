mod reconcile;
mod scheduler;
mod worker;

const BATCH_SIZE: usize = 250;

/// How much catching up a build may do before it gives the generation back.
///
/// An input of the worker rather than a constant inside it: the wall-clock part is a
/// policy about an editor's patience, and a test of what catching up DOES must not
/// depend on how fast the machine it runs on happens to be.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CatchUpBudget {
    pub(crate) passes: usize,
    pub(crate) limit: std::time::Duration,
}

impl CatchUpBudget {
    pub(crate) const PRODUCTION: Self =
        Self { passes: 3, limit: std::time::Duration::from_secs(1) };

    /// Never runs out: for a test of the catch-up logic, where a build that is
    /// superseded for being slow would be judged for the machine, not the code.
    #[cfg(test)]
    pub(crate) const UNBOUNDED: Self = Self { passes: usize::MAX, limit: std::time::Duration::MAX };
}

#[cfg(test)]
use crate::call_hierarchy_index_overlay::CallHierarchyIndexFrozenSnapshot;
#[cfg(test)]
use crate::call_hierarchy_index_state::CallHierarchyIndexState;
#[cfg(test)]
use crate::global_state::Task;
#[cfg(test)]
use reconcile::catch_up_exhausted;
#[cfg(test)]
use std::time::Instant;
#[cfg(test)]
use worker::run_build_within;

#[cfg(test)]
#[path = "../call_hierarchy_index_service_tests.rs"]
mod tests;
