//! Integration tests of `syntax` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod comment_runs;
mod inline_extension_literal;
mod leading_comments;
mod pre_region_dir;
mod sdbl_subquery_in_select;
