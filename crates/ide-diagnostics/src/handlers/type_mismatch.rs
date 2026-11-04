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

/// Union members kept in a message; the rest is summarised as a count.
const MESSAGE_UNION_MEMBERS: usize = 3;

/// Cuts a union label down for a diagnostic message. An expected type that is a union of all
/// metadata-object kinds rendered as a 4744-character line, unreadable in a hover. Callers must
/// compare labels BEFORE shortening: two different unions can share their first members.
pub(crate) fn shorten_type_union(label: &str) -> String {
    // `bsl-types` display joins union members with exactly this separator.
    const SEPARATOR: &str = " | ";
    let total = label.split(SEPARATOR).count();
    if total <= MESSAGE_UNION_MEMBERS {
        return label.to_owned();
    }
    let kept: Vec<&str> = label.split(SEPARATOR).take(MESSAGE_UNION_MEMBERS).collect();
    format!("{} | … и ещё {}", kept.join(SEPARATOR), total - MESSAGE_UNION_MEMBERS)
}

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
    let (expected_label, actual_label) =
        (shorten_type_union(&expected_label), shorten_type_union(&actual_label));
    let message =
        format!("Несоответствие типов: ожидалось '{expected_label}', получено '{actual_label}'");
    crate::simple_hir_diagnostic(DiagnosticCode::TypeMismatch, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use super::shorten_type_union;
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn shorten_leaves_a_label_without_separator_alone() {
        assert_eq!(shorten_type_union("Число"), "Число");
        assert_eq!(shorten_type_union(""), "");
    }

    #[test]
    fn shorten_leaves_up_to_three_members_alone() {
        assert_eq!(shorten_type_union("А"), "А");
        assert_eq!(shorten_type_union("А | Б | В"), "А | Б | В");
    }

    #[test]
    fn shorten_cuts_the_fourth_member_onwards() {
        assert_eq!(shorten_type_union("А | Б | В | Г"), "А | Б | В | … и ещё 1");
    }

    #[test]
    fn shorten_bounds_a_sixty_member_union() {
        let members: Vec<String> = (0..60).map(|i| format!("ОбъектМетаданных: Вид{i}")).collect();
        let full = members.join(" | ");
        let short = shorten_type_union(&full);
        assert!(short.ends_with(" | … и ещё 57"), "{short}");
        assert_eq!(short.split(" | ").count(), 4);
        assert!(short.chars().count() < 120, "{} chars", short.chars().count());
        assert!(full.chars().count() > 1000);
    }

    #[test]
    fn shorten_keeps_a_member_with_spaces_intact() {
        assert_eq!(
            shorten_type_union("ОбъектМетаданных: HTTPСервис | Строка | Число | Булево"),
            "ОбъектМетаданных: HTTPСервис | Строка | Число | … и ещё 1"
        );
        assert_eq!(
            shorten_type_union("ОбъектМетаданных: HTTPСервис"),
            "ОбъектМетаданных: HTTPСервис"
        );
    }

    #[test]
    fn distinct_unions_sharing_three_members_differ_in_full_but_not_shortened() {
        // The guard in `from_hir` must compare these full labels: shortened they are
        // equal, and comparing after shortening would drop a real finding.
        let expected = "А | Б | В | Г";
        let actual = "А | Б | В | Д";
        assert_ne!(expected, actual);
        assert_eq!(shorten_type_union(expected), shorten_type_union(actual));
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
