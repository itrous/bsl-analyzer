//! Integration tests of `mcp-server` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod contract;
mod doc_examples;
mod one_path_rule;
mod one_resolution_rule;
mod partitioned_baseline_reload;
mod tool_gate;
