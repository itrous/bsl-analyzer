//! Metadata of `LocalCallWithQueryInCycle`: a call in a loop body that reaches a query execution
//! through local methods of the same module (any chain length). The detection lives in
//! `create_query_in_cycle`, which reports the direct case under `CreateQueryInCycle`.
//!
//! It is graded below the direct case: reachability is a candidate, not a proven per-iteration
//! query (the callee may not reach the execution on every path) - the same grade as
//! `ImplicitDatabaseReadInCycle`.

use crate::define_metadata;
use crate::metadata::*;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Performance],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};
