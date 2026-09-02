//! Background-built semantic call graph for the workspace MCP profile.
//!
//! The whole-config call graph is built into an on-disk SQLite store (the
//! in-memory graph does not fit in RAM on large configs) and served read-only from
//! there. The build runs off-thread in RAM-bounded batches: tools observe
//! [`GraphStatus`] and degrade gracefully while it indexes.
//!
//! Freshness is **pull-on-request**: each `graph` call cheaply checks whether the
//! workspace drifted on disk since the snapshot it served and, on drift, kicks an
//! async reload while still serving the current (stale) snapshot. The agent-facing
//! freshness token is a monotonic *generation*, recorded in the built file's `meta`
//! so a served response's revision always describes the exact build it serves.

mod build;
pub(crate) mod input;
pub(crate) mod mdo_files;
pub(crate) mod scan;
mod snapshot;
#[cfg(test)]
pub(crate) use snapshot::{BackgroundSnapshotFailure, SNAPSHOT_POOL_CAP};
mod state;
#[cfg(test)]
pub(crate) mod test_support;
mod types;
pub(crate) mod universe;

#[allow(
    unused_imports,
    reason = "the stable graph facade preserves crate::graph helper paths while leaf consumers import directly"
)]
pub(crate) use build::{read_stored_fingerprints, read_stored_sig_hashes};
#[allow(
    unused_imports,
    reason = "the stable graph facade preserves crate::graph helper paths while leaf consumers import directly"
)]
pub(crate) use input::{
    build_source_root, db_for_files, db_for_files_lazy, ProjectSnapshot, GRAPH_SOURCE_ROOT,
};
#[allow(
    unused_imports,
    reason = "the stable graph facade preserves crate::graph scan paths while leaf consumers import directly"
)]
pub(crate) use scan::{classify_changes, file_fingerprint, FileStat, WorkspaceDiff};
#[allow(
    unused_imports,
    reason = "the stable graph facade preserves crate::graph snapshot paths while implementation stays private"
)]
pub(crate) use snapshot::{BackgroundSnapshotError, GraphSnapshot, PooledGraphDb};
pub(crate) use state::GraphState;
#[allow(
    unused_imports,
    reason = "the stable graph facade preserves crate::graph lifecycle paths"
)]
pub(crate) use types::{
    Freshness, FusedStartup, GraphPublishOutcome, GraphPublishSignal, GraphStatus,
    GraphStatusReport, NudgeOutcome, SUPERSEDED_GRAPH_ERROR,
};
