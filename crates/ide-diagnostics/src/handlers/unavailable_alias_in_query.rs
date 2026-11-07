use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig};
use sdbl_hir;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious, MetadataTag::Sql],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::UnavailableAlias { alias, range } = diag {
        let code = DiagnosticCode::UnavailableAliasInQuery;
        diagnostics.push(Diagnostic {
            code,
            message: format!(
                "Псевдоним \"{alias}\" внешнего запроса недоступен внутри производной таблицы",
            ),
            severity: config.severity(code),
            range: mapper.map_range(*range, query_text),
            tags: config.tags(code),
            fixes: vec![],
        });
    }
}

pub fn check(ctx: &crate::DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::UnavailableAliasInQuery,
        dispatch,
    )
}
