//! Integration tests of `ide-diagnostics` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod ambiguous_field_in_query;
mod bilingual_parity;
mod common_attributes_in_query;
mod data_exchange_and_unused_method_fixes;
mod diagnostic_quick_fixes;
mod handlers_bound_from_another_module;
mod inline_extension_directives;
mod name_resolution_correctness;
mod platform_fact_characterization;
mod platform_membership_characterization;
mod platform_ui_fact_characterization;
mod preproc_symbol_docs;
mod query_keyword_alias;
mod query_nested_in_select_list;
mod standalone_query_parity;
mod unresolved_metadata_path;
