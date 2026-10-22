use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::{ImplicitDatabaseReadKind, LocalRange};

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

/// Projects a typed implicit database read into its diagnostic.
pub fn from_hir(
    kind: &ImplicitDatabaseReadKind,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let message = match kind {
        ImplicitDatabaseReadKind::RefAttribute { field } => format!(
            "Возможное чтение из базы в цикле: обращение к реквизиту «{}» через точку от ссылки загружает объект. Получите реквизит в запросе до цикла",
            field.as_str()
        ),
        ImplicitDatabaseReadKind::RefPresentation => {
            "Возможное чтение из базы в цикле: Строка() от ссылки получает её представление. Получите представление в запросе (ПРЕДСТАВЛЕНИЕ) до цикла".to_string()
        }
        ImplicitDatabaseReadKind::ManagerFind { method } => format!(
            "Возможное чтение из базы в цикле: {}() менеджера выполняет поиск в базе. Найдите значения одним запросом до цикла",
            method.as_str()
        ),
    };
    crate::simple_hir_diagnostic(DiagnosticCode::ImplicitDatabaseReadInCycle, message, range, ctx)
}
