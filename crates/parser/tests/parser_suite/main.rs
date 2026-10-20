//! Integration tests of `parser` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod error_ranges;
mod fixture_licence;
mod grammar_attestation;
mod inline_extension_directives;
mod integration_tests;
mod line_sensitivity;
mod line_tokens;
mod parse_stability;
mod recovery_boundaries;
mod reparse;
mod sdbl_independent_corpus;
mod sdbl_keyword_alias;
mod sdbl_parser_tests;
mod sdbl_recovery_and_allowances;
mod sdbl_slice10a_backbone;
mod sdbl_slice10b_predicates;
mod sdbl_slice11_clauses;
mod sdbl_slice6_package;
mod sdbl_slice7_addendum_limitations;
mod sdbl_slice7_fields;
mod sdbl_slice8_addendum_virtual_table_args;
mod sdbl_slice8_sources;
mod sdbl_slice9_joins;
mod torn_preprocessor_if;
