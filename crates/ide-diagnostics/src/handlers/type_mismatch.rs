use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::TypeId;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
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
    // An overlapping set means some value of this expression is acceptable, so
    // the analyzer cannot state a mismatch; only a disjoint set can.
    // Identical labels (distinct internal types sharing a display name, e.g. a
    // doc-derived nominal type vs the inferred platform one) are the full overlap.
    if labels_overlap(&expected_label, &actual_label) {
        return None;
    }
    let message =
        format!("Несоответствие типов: ожидалось '{expected_label}', получено '{actual_label}'");
    crate::simple_hir_diagnostic(DiagnosticCode::TypeMismatch, message, range, ctx)
}

/// Whether two union display labels (`A | B | C`) share a member.
pub(crate) fn labels_overlap(expected: &str, actual: &str) -> bool {
    let actual_members = actual.split(" | ").map(str::trim);
    expected.split(" | ").map(str::trim).any(|member| actual_members.clone().any(|a| a == member))
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    use super::labels_overlap;

    #[test]
    fn label_overlap_is_by_member_not_by_substring() {
        assert!(labels_overlap("Булево | Строка", "ФорматированнаяСтрока | Строка"));
        assert!(labels_overlap("Число", "Число | Неопределено"));
        assert!(labels_overlap("Строка", "Строка"));
        assert!(!labels_overlap("Число", "Неопределено"));
        assert!(!labels_overlap("Число", "Строка | Неопределено"));
        assert!(!labels_overlap("Строка", "ФорматированнаяСтрока"));
    }

    // The arguments below are variables whose union the analyzer computed from
    // branches: a union the documentation gave is relieved earlier, in hir-ty.

    #[test]
    fn expected_number_actual_number_or_undefined_is_silent() {
        let code = r#"
Процедура Тест(Условие)
    Позиция = Неопределено;
    Если Условие Тогда
        Позиция = 5;
    КонецЕсли;
    Результат = Лев("abc", Позиция);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::TypeMismatch, expect![[""]]);
    }

    #[test]
    fn expected_number_actual_string_is_reported() {
        let code = "Процедура Тест()\n    Результат = Лев(\"abc\", \"x\");\nКонецПроцедуры\n";
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::TypeMismatch,
            expect![[r#"
                TypeMismatch @ 2:28..2:31
                  message: Несоответствие типов: ожидалось 'Число', получено 'Строка'
                  severity: Major"#]],
        );
    }

    #[test]
    fn overlapping_unions_sharing_a_member_are_silent() {
        // `УстановитьБезопасныйРежим(Булево | Строка)` against `Строка | Неопределено`:
        // neither set contains the other, they meet in `Строка`.
        let code = r#"
Процедура Тест(Условие)
    Режим = Неопределено;
    Если Условие Тогда
        Режим = "";
    КонецЕсли;
    УстановитьБезопасныйРежим(Режим);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::TypeMismatch, expect![[""]]);
    }

    #[test]
    fn expected_number_actual_undefined_alone_is_reported() {
        let code =
            "Процедура Тест()\n    Результат = Лев(\"abc\", Неопределено);\nКонецПроцедуры\n";
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::TypeMismatch,
            expect![[r#"
                TypeMismatch @ 2:28..2:40
                  message: Несоответствие типов: ожидалось 'Число', получено 'Неопределено'
                  severity: Major"#]],
        );
    }

    #[test]
    fn expected_number_actual_disjoint_union_is_reported() {
        let code = r#"
Процедура Тест(Условие)
    Позиция = Неопределено;
    Если Условие Тогда
        Позиция = "а";
    КонецЕсли;
    Результат = Лев("abc", Позиция);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::TypeMismatch,
            expect![[r#"
                TypeMismatch @ 7:28..7:35
                  message: Несоответствие типов: ожидалось 'Число', получено 'Строка | Неопределено'
                  severity: Major"#]],
        );
    }
}
