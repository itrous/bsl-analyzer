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
    // An overlapping set means some value of this expression is acceptable, so the analyzer
    // cannot state a mismatch; only a disjoint set can. Two sides rendering identically in
    // the message language make it self-contradictory whatever their members are.
    if expected_label == actual_label || ctx.kernel_types_overlap(expected, actual) {
        return None;
    }
    let message =
        format!("Несоответствие типов: ожидалось '{expected_label}', получено '{actual_label}'");
    crate::simple_hir_diagnostic(DiagnosticCode::TypeMismatch, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

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

    #[test]
    fn a_union_nested_in_a_member_is_not_a_member_of_the_outer_set() {
        // The control's label reads `ПолеФормы:Значение -> Булево | Число`, yet the value
        // passed is the control, not a number.
        let form_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <ChildItems>
        <InputField name="ПолеВвода" id="1">
            <DataPath>Значение</DataPath>
        </InputField>
    </ChildItems>
    <Attributes>
        <Attribute name="Значение" id="2">
            <Type>
                <v8:Type>xs:boolean</v8:Type>
                <v8:Type>xs:decimal</v8:Type>
            </Type>
        </Attribute>
    </Attributes>
</Form>"#;
        let code = "&НаКлиенте\nПроцедура Тест()\n    Р = Лев(\"abc\", Элементы.ПолеВвода);\nКонецПроцедуры\n";
        let messages: Vec<String> = crate::test_utils::check_form_with_form_xml(code, form_xml)
            .into_iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::TypeMismatch)
            .map(|diagnostic| diagnostic.message)
            .collect();
        assert_eq!(
            messages,
            ["Несоответствие типов: ожидалось 'Число', получено 'ПолеФормы:Значение -> Булево | Число'"]
        );
    }

    #[test]
    fn an_overlapping_argument_does_not_hide_a_disjoint_one_after_it() {
        let code = r#"
Процедура Тест(Условие)
    Начало = Неопределено;
    Если Условие Тогда
        Начало = 1;
    КонецЕсли;
    Результат = Сред("abc", Начало, "x");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::TypeMismatch,
            expect![[r#"
                TypeMismatch @ 7:37..7:40
                  message: Несоответствие типов: ожидалось 'Число', получено 'Строка'
                  severity: Major"#]],
        );
    }

    #[test]
    fn a_documented_slot_reports_its_first_incompatible_argument_as_before() {
        let code = r#"
// Параметры:
//   Первый - Число - значение.
//   Второй - Число - значение.
Процедура Принять(Первый, Второй) Экспорт
КонецПроцедуры

Процедура Тест(Условие)
    Начало = Неопределено;
    Если Условие Тогда
        Начало = 1;
    КонецЕсли;
    Принять(Начало, "x");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::TypeMismatchByDocComment,
            expect![[r#"
                TypeMismatchByDocComment @ 13:13..13:19
                  message: Несоответствие типов: ожидалось 'Число', получено 'Число | Неопределено' (тип параметра из описания)
                  severity: Error"#]],
        );
    }
}
