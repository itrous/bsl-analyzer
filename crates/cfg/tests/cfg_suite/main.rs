//! Integration tests of `cfg` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod cfg_break_continue;
mod cfg_goto_label;
mod cfg_loops;
mod cfg_preproc;
mod cfg_try_except;
