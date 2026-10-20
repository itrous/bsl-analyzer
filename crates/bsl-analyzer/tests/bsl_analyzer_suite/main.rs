//! Integration tests of `bsl-analyzer` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod extension_live_metadata_contract;
mod semantic_tokens_directives_and_declarations;
