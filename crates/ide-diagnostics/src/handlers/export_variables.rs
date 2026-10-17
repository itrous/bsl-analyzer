use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use ide_db::TextRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Design, MetadataTag::Unpredictable],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(_name: &str, range: TextRange, ctx: &AnalysisContext) -> Option<Diagnostic> {
    let code = DiagnosticCode::ExportVariables;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    Some(Diagnostic {
        code,
        message: "Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам".to_string(),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;
    use expect_test::expect;
    #[test]
    fn test_no_export() {
        let code = r#"
Перем МояПеременная;

Процедура Инициализация()
    МояПеременная = 0;
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let export_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::ExportVariables).collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &export_diags));
    }

    #[test]
    fn test_simple_export() {
        let code = r#"Перем МояПеременная Экспорт;"#;
        let diagnostics = check_hir_diagnostic(code);
        let export_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::ExportVariables).collect();
        expect![[r#"
            ExportVariables @ 1:7..1:20
              message: Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам
              severity: Warning"#]].assert_eq(&format_diags(code, &export_diags));
    }

    #[test]
    fn test_inside_procedure() {
        let code = r#"
Процедура Тест()
    Перем ПеременнаяМодуля, ПеременнаяЭкспорт;
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let export_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::ExportVariables).collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &export_diags));
    }

    #[test]
    fn test_bilingual() {
        let code_ru = r#"Перем МояПеременная Экспорт;"#;
        let diagnostics_ru = check_hir_diagnostic(code_ru);
        let export_diags_ru: Vec<_> = diagnostics_ru
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::ExportVariables)
            .collect();
        expect![[r#"
            ExportVariables @ 1:7..1:20
              message: Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам
              severity: Warning"#]].assert_eq(&format_diags(code_ru, &export_diags_ru));

        let code_en = r#"Var MyVariable Export;"#;
        let diagnostics_en = check_hir_diagnostic(code_en);
        let export_diags_en: Vec<_> = diagnostics_en
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::ExportVariables)
            .collect();
        expect![[r#"
            ExportVariables @ 1:5..1:15
              message: Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам
              severity: Warning"#]].assert_eq(&format_diags(code_en, &export_diags_en));
    }

    #[test]
    fn test_multiple_exported_and_private_vars() {
        let code = "Перем Перем1 Экспорт;\nПерем Перем2;\nПерем Перем53 Экспорт;\n\nПроцедура МетодСодержащийПеременную()\n    Перем ПеременнаяМодуля, ПеременнаяЭкспорт;\nКонецПроцедуры";
        let diagnostics = check_hir_diagnostic(code);
        let export_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::ExportVariables).collect();
        expect![[r#"
            ExportVariables @ 1:7..1:13
              message: Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам
              severity: Warning
            ExportVariables @ 3:7..3:14
              message: Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам
              severity: Warning"#]].assert_eq(&format_diags(code, &export_diags));
    }

    #[test]
    fn test_commented_export_not_detected() {
        let code = "// Перем Закомментированная Экспорт;";
        let diagnostics = check_hir_diagnostic(code);
        let export_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::ExportVariables).collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &export_diags));
    }
    /// `Экспорт` exports only the name it follows, so only those names are
    /// global variables: `Б`, `В` and `Е` are not reported.
    #[test]
    fn only_names_followed_by_export_are_reported() {
        let code = "Перем А Экспорт, Б;\nПерем В, Г Экспорт;\nПерем Д Экспорт, Е, Ж Экспорт;\n";
        let diagnostics = check_hir_diagnostic(code);
        let export_diags: Vec<_> =
            diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::ExportVariables).collect();
        expect![[r#"
            ExportVariables @ 1:7..1:8
              message: Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам
              severity: Warning
            ExportVariables @ 2:10..2:11
              message: Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам
              severity: Warning
            ExportVariables @ 3:7..3:8
              message: Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам
              severity: Warning
            ExportVariables @ 3:21..3:22
              message: Не рекомендуется использовать глобальные переменные. Они могут приводить к трудноуловимым ошибкам
              severity: Warning"#]].assert_eq(&format_diags(code, &export_diags));
    }
}
