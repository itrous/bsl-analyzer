//! Serialises the tests of this binary that read the process-global walk counter.
//!
//! The counter itself lives in `test_utils::walk_probe`. [`WALK_GATE`] is not politeness:
//! a test that walks references WITHOUT holding it makes `await_walk_start` return on
//! somebody else's file, and the gates then measure an interleaving instead of a
//! cancellation. Any test that reaches `ide::find_references_by_name` takes it.

pub(crate) use test_utils::walk_probe::{
    await_walk_start, entered, hold_first_span, hold_walk_start, install, reset,
};

pub(crate) static WALK_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
