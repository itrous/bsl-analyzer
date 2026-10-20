//! Holds the test that keeps retired third-party test material out of `ide-diagnostics`
//! (`tests/retired_material.rs`).
//!
//! The scan hashes every tracked file of that crate over hundreds of window lengths: CPU work an
//! unoptimised build makes ten times slower. A crate of its own lets the dev profile optimise
//! this one test (`[profile.dev.package.retired-material-gate]` in the workspace manifest)
//! without touching the build of any other.
