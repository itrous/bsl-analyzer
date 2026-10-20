//! Integration tests of `hir` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod cfg_analysis_contract;
mod cfg_solver_boundaries;
mod cfg_surface_contract;
mod classify_token;
