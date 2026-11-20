pub use base_db;
pub use hir::is_bsl_source;
pub use parking_lot::FairMutex;
pub use syntax::TextRange;
pub use vfs;

pub mod database;
pub mod effective_exports;
pub mod effects;
pub mod features;
pub mod metadata;
mod metadata_symbol;
pub mod provider;
pub mod queries;
pub mod query_resolver;
pub mod root_db;
pub mod salsa_events;
pub mod salsa_provider;
pub mod type_kernel;
pub mod types;
pub(crate) mod vfs_helpers;

pub use database::METHOD_KEYED_QUERY_FAMILIES;
pub use hir::SdblHirEntries;
// Named by `RootDatabase::resolve_common_module_files` / `AnalysisProvider`, so every
// consumer of those must be able to spell it without depending on `hir` itself.
pub use hir::CommonModuleBodies;
pub use types::SymbolKind;

pub use database::{
    EffectiveMetadataMember, EffectiveMetadataMemberValue, GraphConfigCache, RootDatabaseImpl,
    VisibleRoots,
};
pub use root_db::RootDatabase;

pub use provider::AnalysisProvider;
pub use salsa_provider::SalsaProvider;
pub use vfs_helpers::get_file_path;

pub use hir::{all_sdbl_in_file_query, sdbl_hir_for_file_query};
pub use queries::{
    configuration_path_for_file, effective_target, line_index_query, method_cfg_query,
    method_path_terminates_query, module_code_reaching_definitions_query, module_metadata_query,
    reaching_definitions_query, weaving_target,
};

#[cfg(test)]
#[path = "tests/flow_type_regressions.rs"]
mod flow_type_regressions;

#[cfg(test)]
#[path = "tests/platform_call_proof.rs"]
mod platform_call_proof;

pub use effects::{
    method_effect_summary_query, method_security_state_query, module_code_security_state_query,
    module_effect_summaries_query, ModuleEffectSummaries,
};

pub use effective_exports::{
    effective_module_exports_query, EffectiveMethodExport, EffectiveModuleExports,
    EffectiveModuleRole, EffectiveVariableExport, ExportComposition,
};

pub use metadata::build_module_metadata;

mod external_context;
