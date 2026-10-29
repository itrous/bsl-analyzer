//! Diagnostics for a query text that is not embedded in a module.
//!
//! The rules are the same ones a query inside a `.bsl` string literal gets — same codes,
//! same severities, same parameters — because they are the same functions. What differs is
//! only where the ranges land and where the metadata comes from: a caller with no file
//! passes a resolver directly instead of letting Salsa derive one from a `FileId`.

use bsl_metadata::QueryMetadataResolver;

use crate::runner::SDBL_DISPATCH;
use crate::sdbl_utils::SdblPositionMapper;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig};

/// Every diagnostic the SDBL rules produce for `query_text`, in the coordinates of
/// `query_text` itself.
///
/// `resolver` is what separates a full check from a parse-only one: without it the
/// metadata-dependent rules ([`METADATA_DEPENDENT_CODES`]) cannot fire at all, while the
/// structural rules are unaffected. A caller that reports how complete its
/// answer is must decide that from whether it passed a resolver, not from whether the result
/// is empty.
pub fn validate_query_text(
    config: &DiagnosticsConfig,
    resolver: Option<&dyn QueryMetadataResolver>,
    query_text: &str,
) -> Vec<Diagnostic> {
    let parse = parser::parse_sdbl(query_text);
    let mut diagnostics = Vec::new();

    if !config.is_disabled(DiagnosticCode::QueryParseError) {
        let code = DiagnosticCode::QueryParseError;
        for (range, err) in syntax::sdbl_query::collect_query_parse_errors(&parse) {
            diagnostics.push(Diagnostic {
                code,
                message: err.format_ru(),
                severity: config.severity(code),
                range,
                tags: config.tags(code),
                fixes: vec![],
            });
        }
    }

    let enabled: Vec<_> =
        SDBL_DISPATCH.iter().filter(|(code, _)| !config.is_disabled(*code)).collect();
    if enabled.is_empty() {
        return diagnostics;
    }

    let package = sdbl_hir::lower_sdbl_to_hir_with_resolver(&parse, resolver);
    let mapper = SdblPositionMapper::Standalone;

    for hir_diag in package.all_diagnostics() {
        for (_, dispatch_fn) in &enabled {
            dispatch_fn(config, hir_diag, &mapper, query_text, &mut diagnostics);
        }
    }

    diagnostics
}

/// The codes that cannot be produced without metadata. A caller degrading to a parse-only
/// answer asserts their absence; a caller claiming a full check expects them to be reachable.
///
/// Membership follows from the lowering, not from intent: each of these needs a resolved table
/// to speak at all. `AmbiguousFieldInQuery` and `QueryNestedFieldsByDot` belong here for the
/// same reason — without a resolver a column's type stays `Unknown`, so neither the ambiguity
/// marker nor a composite reference to dereference is ever reached. `DuplicateAliasInQuery`
/// does not: an alias collision is visible in the query text.
pub const METADATA_DEPENDENT_CODES: &[DiagnosticCode] = &[
    DiagnosticCode::AmbiguousFieldInQuery,
    DiagnosticCode::QueryNestedFieldsByDot,
    DiagnosticCode::QueryToMissingMetadata,
    DiagnosticCode::UnknownFieldInQuery,
];

/// Every code decided by the query text alone, whatever the route. Rules that need the BSL
/// context executing the query are not here: bare text has no such context and never reports
/// them.
///
/// Exported because a consumer comparing the two routes has to know the set, and a hand-copied
/// list is exactly what drifts when a rule is added — as it did the moment this module gained
/// two rules of its own.
pub fn sdbl_query_codes() -> Vec<DiagnosticCode> {
    let mut codes: Vec<_> = SDBL_DISPATCH.iter().map(|(code, _)| *code).collect();
    codes.push(DiagnosticCode::QueryParseError);
    codes
}
