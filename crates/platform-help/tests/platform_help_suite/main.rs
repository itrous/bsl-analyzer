//! Integration tests of `platform-help` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod installed_auto;
mod prepare_package;
mod real_hbk_smoke;
