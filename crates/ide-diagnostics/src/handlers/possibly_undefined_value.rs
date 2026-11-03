use crate::define_metadata;
use crate::handlers::type_mismatch::labels_overlap;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::TypeId;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Info,
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

/// A weak hint for the one overlap case `TypeMismatch` stays silent on: the
/// expected type is met on some branch, and the other branch leaves the value
/// `Неопределено`, which the expected type does not accept.
pub fn from_hir(
    expected: TypeId,
    actual: TypeId,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    // Decided from the types, not from the display labels: those are
    // locale-dependent.
    if !ctx.kernel_type_includes_undefined(actual) || ctx.kernel_type_includes_undefined(expected) {
        return None;
    }
    let locale = ctx.locale();
    let expected_label = ctx.kernel_type_display(expected, locale);
    let actual_label = ctx.kernel_type_display(actual, locale);
    // Disjoint sets are `TypeMismatch`'s verdict, not a hint.
    if !labels_overlap(&expected_label, &actual_label) {
        return None;
    }
    let message = format!(
        "Значение может быть 'Неопределено': ожидалось '{expected_label}', получено '{actual_label}'"
    );
    crate::simple_hir_diagnostic(DiagnosticCode::PossiblyUndefinedValue, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    const NUMBER_OR_UNDEFINED: &str = r#"
Процедура Тест(Условие)
    Позиция = Неопределено;
    Если Условие Тогда
        Позиция = 5;
    КонецЕсли;
    Результат = Лев("abc", Позиция);
КонецПроцедуры
"#;

    #[test]
    fn expected_number_actual_number_or_undefined_is_a_hint() {
        check_diagnostics_snapshot_for(
            NUMBER_OR_UNDEFINED,
            DiagnosticCode::PossiblyUndefinedValue,
            expect![[r#"
                PossiblyUndefinedValue @ 7:28..7:35
                  message: Значение может быть 'Неопределено': ожидалось 'Число', получено 'Число | Неопределено'
                  severity: Hint"#]],
        );
    }

    #[test]
    fn the_same_code_reports_no_type_mismatch() {
        check_diagnostics_snapshot_for(
            NUMBER_OR_UNDEFINED,
            DiagnosticCode::TypeMismatch,
            expect![[""]],
        );
    }

    #[test]
    fn expected_accepting_undefined_is_silent() {
        // `Массив.Добавить` takes an arbitrary value, `Неопределено` included.
        let code = r#"
Процедура Тест(Условие)
    Позиция = Неопределено;
    Если Условие Тогда
        Позиция = 5;
    КонецЕсли;
    Массив = Новый Массив;
    Массив.Добавить(Позиция);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::PossiblyUndefinedValue, expect![[""]]);
        check_diagnostics_snapshot_for(code, DiagnosticCode::TypeMismatch, expect![[""]]);
    }

    #[test]
    fn disjoint_mismatch_is_not_a_hint() {
        let code = "Процедура Тест()\n    Результат = Лев(\"abc\", \"x\");\nКонецПроцедуры\n";
        check_diagnostics_snapshot_for(code, DiagnosticCode::PossiblyUndefinedValue, expect![[""]]);
    }

    #[test]
    fn overlap_without_undefined_is_silent() {
        // Meets in `Строка`; `Неопределено` is on neither side.
        let code = r#"
Процедура Тест(Условие)
    Режим = "";
    Если Условие Тогда
        Режим = 1;
    КонецЕсли;
    УстановитьБезопасныйРежим(Режим);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::PossiblyUndefinedValue, expect![[""]]);
        check_diagnostics_snapshot_for(code, DiagnosticCode::TypeMismatch, expect![[""]]);
    }
}
