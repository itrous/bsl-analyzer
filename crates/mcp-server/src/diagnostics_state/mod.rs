//! Lazily-built resident analysis database for per-file diagnostics (workspace
//! profile).
//!
//! Unlike the call `graph`, diagnostics are content-dependent and cannot be folded
//! into a static on-disk store: computing them needs a live Salsa database with the
//! target file and its whole resolution closure resident, exactly like the LSP
//! server. So the first `diagnostics file` call builds a resident
//! [`RootDatabaseImpl`] over every workspace `.bsl` text (the proven ~2.8 GB LSP
//! footprint, NOT the graph's fold) and serves per-file diagnostics from it through
//! Salsa's lazy lowering + LRU cache. `catalog`/`schema` never trigger this.
//!
//! Concurrency is a single [`Mutex`], not an `RwLock`: a Salsa `RootDatabaseImpl` is
//! `Send` but `!Sync`, so a `&db` cannot be shared across threads — reads must run on
//! the thread that holds the handle. Each per-file query therefore runs on the calling
//! (blocking) thread WHILE holding the mutex, so reads serialise but a drift reload's
//! `set_file_text` can never alias an in-flight query — a read here never unwinds as
//! `salsa::Cancelled::PendingWrite`. It does unwind as `Cancelled::Local`, on purpose:
//! that is how a cancelled request stops, and [`session`] owns that path.
//! Per-file diagnostics are LRU-cached and fast, so serialising them is cheap. Cloning
//! the db handle inside the lock shares the memo/LRU cache (`RootDatabaseImpl::clone`
//! clones the Salsa `Storage`) and the clone never leaves the calling thread.
//!
//! Freshness is pull-on-request, mirroring the graph: each read cheaply re-checks the
//! workspace on disk (throttled). A changed `.bsl` body is re-keyed with `set_file_text`,
//! a created/deleted `.bsl` is (un)registered into the live source root, and any `.xml`
//! add/remove/edit point-refreshes the metadata substrate — all in place under the
//! resident mutex, preserving every unrelated memo. Only an analyzer config-file change
//! or a removed directory subtree falls back to a full off-thread rebuild. An idle
//! sweeper drops the resident db after a quiet period so a standalone `mcp serve`
//! reclaims the memory after a burst.

mod drift;
mod lifecycle;
#[cfg(test)]
mod project_config_tests;
mod resident;
mod session;
mod snapshot;
#[cfg(test)]
pub(crate) mod test_support;
mod types;
mod workspace_sweep;

pub(crate) use lifecycle::{lock_recover, DiagnosticsState};
pub(crate) use resident::{
    computation_config, deferred_scope, diagnostic_survives_authors, DiagnosticsResident,
};
pub(crate) use session::{resident_call, CallOutcome, ResidentSession};
pub(crate) use snapshot::ResidentModuleSnapshotSource;
#[allow(
    unused_imports,
    reason = "the stable diagnostics facade preserves crate::diagnostics_state value paths"
)]
pub(crate) use types::{DiagnosticsStatus, Freshness, ResidentOutcome, StatusReport, WatchReport};
#[allow(
    unused_imports,
    reason = "the stable diagnostics facade preserves crate::diagnostics_state workspace sweep paths"
)]
pub(crate) use workspace_sweep::{CodeAggregate, SweepOptions, WorkspaceSweep};
