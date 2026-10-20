//! Integration tests of `hir-def` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod inline_extension_merge;
mod module_members;
