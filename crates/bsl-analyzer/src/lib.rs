pub mod tracing {
    pub mod config;
    pub mod hprof;
    pub mod json;
    pub use config::Config;
}

pub mod analysis_host;
pub mod analysis_scope;
pub mod bench;
pub mod call_hierarchy_index_overlay;
mod call_hierarchy_index_service;
pub mod call_hierarchy_index_state;
pub mod diagnostics_baseline;
pub mod diagnostics_state;
pub mod features_state;
pub mod frozen_context;
pub mod global_state;
pub mod handlers;
pub mod help_bootstrap;
pub mod locale;
pub mod lsp;
pub mod mcp_install;
pub mod mem_docs;
pub mod mem_report;
pub mod panic_watch;
pub mod reporters;
pub mod server;
pub mod smoke;
pub mod task_pool;
#[cfg(test)]
pub(crate) mod test_uri;
pub mod workspace;

pub use server::main_loop;
