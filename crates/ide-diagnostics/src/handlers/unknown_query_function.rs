use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
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
    if let sdbl_hir::SdblDiagnostic::UnknownFunction { function_name, range } = diag {
        let code = DiagnosticCode::UnknownQueryFunction;
        diagnostics.push(Diagnostic {
            code,
            message: format!("Функция \"{}\" не найдена в языке запросов", function_name),
            severity: config.severity(code),
            range: mapper.map_range(*range, query_text),
            tags: config.tags(code),
            fixes: vec![],
        });
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::UnknownQueryFunction,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn typo_in_function_name_is_reported_at_the_name() {
        let code = "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ ПОДСТРКА(Т.Имя, 1, 2) ИЗ Справочник.Товары КАК Т\";\nКонецПроцедуры";
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnknownQueryFunction,
            expect![[r#"
                UnknownQueryFunction @ 3:29..3:37
                  message: Функция "ПОДСТРКА" не найдена в языке запросов
                  severity: Major"#]],
        );
    }

    #[test]
    fn real_function_without_a_dedicated_kind_is_silent() {
        let code = "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ СокрЛП(Т.Имя) КАК Имя ИЗ Справочник.Товары КАК Т\";\nКонецПроцедуры";
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnknownQueryFunction,
            expect![[r#""#]],
        );
    }
}
