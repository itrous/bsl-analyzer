//! Integration tests of `bsl-platform` that share no process state, built and run as one
//! target: each file a module, so the crate links one test binary instead of one per file.

mod capability_registry;
mod deprecation_matches_platform_data;
mod deprecation_platform_entries;
mod deprecation_registry;
mod help_corpus_equivalence;
mod metadata_object_family_parameters;
mod registry_surface_matches_platform_data;
mod security_registry;
mod type_member_surface;
