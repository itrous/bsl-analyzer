use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::TypeId;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Info,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: false,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious, MetadataTag::Unpredictable],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    expected: TypeId,
    actual: TypeId,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let locale = ctx.locale();
    let expected_label = ctx.kernel_type_display(expected, locale);
    let actual_label = ctx.kernel_type_display(actual, locale);
    // Distinct internal types can share a display name (e.g. a doc-derived
    // nominal type vs the inferred platform type). A message whose two sides
    // render identically is self-contradictory and not actionable — drop it.
    if expected_label == actual_label {
        return None;
    }
    let expected_label = super::type_mismatch::message_type_label(ctx, expected, expected_label);
    let actual_label = super::type_mismatch::message_type_label(ctx, actual, actual_label);
    let message = format!(
        "Несоответствие типов: ожидалось '{expected_label}', получено '{actual_label}' (тип параметра из описания)"
    );
    crate::simple_hir_diagnostic(DiagnosticCode::TypeMismatchByDocComment, message, range, ctx)
}
