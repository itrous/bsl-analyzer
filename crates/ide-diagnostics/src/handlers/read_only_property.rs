use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::{Name, TypeId};

// An Error, not a smell: assigning to a read-only property of a platform type throws at
// run time ("Поле объекта недоступно для записи") on every execution, measured identically
// on 8.3.17.1549 and 8.3.27.2214. A CodeSmell collapses to a warning in
// `calculate_severity()`, which hides a guaranteed crash. Where the platform help marks a
// writable property read-only, the remedy is the overlay correction (see
// crates/bsl-platform/data/OVERLAYS.md), not a lower grade for the whole rule.
pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 2,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    receiver_ty: TypeId,
    field_name: &Name,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let message = format!(
        "Свойство '{}' типа '{}' доступно только для чтения",
        field_name.as_str(),
        ctx.kernel_type_display(receiver_ty, ctx.locale())
    );
    crate::simple_hir_diagnostic(DiagnosticCode::ReadOnlyPropertyAssignment, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_assignment_is_shown_as_an_error() {
        // Error + Major resolves to Severity::Major, which the LSP maps to ERROR.
        assert_eq!(METADATA.effective_severity(), crate::Severity::Major);
    }
}
