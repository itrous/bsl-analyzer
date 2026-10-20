//! Integration tests of `lexer` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod case_fold_matches_lexer;
mod sdbl_golden_corpus;
mod sdbl_slice1_addendum_braces;
mod sdbl_slice1_core;
mod sdbl_slice2_addendum_clause_keywords;
mod sdbl_slice2_keywords;
mod sdbl_slice3a_types;
mod sdbl_slice3b_metadata_objects;
mod sdbl_slice4_functions;
mod sdbl_slice5_virtual_tables;
