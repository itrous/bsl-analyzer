//! Integration tests of `sdbl-hir` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod clean_room_slice13_diagnostics;
mod comment_invariance;
mod independent_hir_contracts;
mod inline_extension_position;
