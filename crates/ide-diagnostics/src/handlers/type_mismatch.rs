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

/// Shortens a union for a diagnostic message. `members` are the union's own members, each
/// already rendered whole: a union nested inside a member prints with the same separator and
/// is not another member. Callers must compare full labels BEFORE shortening: two different
/// unions can share their first members.
pub(crate) fn shorten_union_members(members: &[String]) -> String {
    if members.len() <= MESSAGE_UNION_MEMBERS {
        return members.join(" | ");
    }
    let kept = members[..MESSAGE_UNION_MEMBERS].join(" | ");
    format!("{kept} | … и ещё {}", members.len() - MESSAGE_UNION_MEMBERS)
}

/// The label to print. A long union keeps the first [`MESSAGE_UNION_MEMBERS`] members; every
/// other type, including one whose own label contains ` | `, is printed in full.
fn message_type_label(ctx: &AnalysisContext, id: TypeId, full_label: String) -> String {
    match ctx.kernel_union_member_labels(id, ctx.locale()) {
        Some(members) if members.len() > MESSAGE_UNION_MEMBERS => shorten_union_members(&members),
        _ => full_label,
    }
}

/// Shortened labels for a message. When shortening would make the two sides read the same,
/// the full labels are kept: the tail was the only difference, and a message that says the
/// two sides are identical is not actionable. Callers have already dropped a pair whose full
/// labels are equal.
pub(crate) fn labels_for_message(
    ctx: &AnalysisContext,
    expected: TypeId,
    actual: TypeId,
    expected_full: String,
    actual_full: String,
) -> (String, String) {
    let expected_short = message_type_label(ctx, expected, expected_full.clone());
    let actual_short = message_type_label(ctx, actual, actual_full.clone());
    if expected_short == actual_short {
        (expected_full, actual_full)
    } else {
        (expected_short, actual_short)
    }
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
        labels_for_message(ctx, expected, actual, expected_label, actual_label);
    let message =
        format!("Несоответствие типов: ожидалось '{expected_label}', получено '{actual_label}'");
    crate::simple_hir_diagnostic(DiagnosticCode::TypeMismatch, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use super::shorten_union_members;
    use crate::test_utils::{
        built_in_facts_are_served, check_config_files, check_diagnostics_snapshot_for, format_diags,
    };
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn members(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|label| (*label).to_owned()).collect()
    }

    #[test]
    fn shorten_leaves_up_to_three_members_alone() {
        assert_eq!(shorten_union_members(&members(&["А"])), "А");
        assert_eq!(shorten_union_members(&members(&["А", "Б", "В"])), "А | Б | В");
        assert_eq!(shorten_union_members(&[]), "");
    }

    #[test]
    fn shorten_cuts_the_fourth_member_onwards() {
        assert_eq!(shorten_union_members(&members(&["А", "Б", "В", "Г"])), "А | Б | В | … и ещё 1");
    }

    #[test]
    fn shorten_bounds_a_sixty_member_union() {
        let labels: Vec<String> = (0..60).map(|i| format!("ОбъектМетаданных: Вид{i}")).collect();
        let short = shorten_union_members(&labels);
        assert!(short.ends_with(" | … и ещё 57"), "{short}");
        assert_eq!(short.split(" | ").count(), 4);
        assert!(short.chars().count() < 120, "{} chars", short.chars().count());
        assert!(labels.join(" | ").chars().count() > 1000);
    }

    #[test]
    fn shorten_keeps_a_member_that_prints_a_separator_intact() {
        let labels = members(&[
            "ОбъектМетаданных: HTTPСервис",
            "Строка",
            "ПолеФормы:Значение -> Булево | Число",
            "Дата",
        ]);
        assert_eq!(
            shorten_union_members(&labels),
            "ОбъектМетаданных: HTTPСервис | Строка | ПолеФормы:Значение -> Булево | Число | … и ещё 1"
        );
    }

    #[test]
    fn distinct_unions_sharing_three_members_differ_in_full_but_not_shortened() {
        // The guard in `from_hir` must compare the full labels: shortened they are
        // equal, and comparing after shortening would drop a real finding.
        let expected = members(&["А", "Б", "В", "Г"]);
        let actual = members(&["А", "Б", "В", "Д"]);
        assert_ne!(expected, actual);
        assert_eq!(shorten_union_members(&expected), shorten_union_members(&actual));
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
    fn a_four_member_union_message_keeps_three_members() {
        // Each `Если` without `Иначе` leaves the previous value reachable, and a literal
        // assignment is a member narrowing can join. The message must name three of them
        // and count the fourth, not print the whole union.
        let code = r#"
Процедура Тест(У1, У2, У3)
    Значение = "а";
    Если У1 Тогда
        Значение = Истина;
    КонецЕсли;
    Если У2 Тогда
        Значение = Неопределено;
    КонецЕсли;
    Если У3 Тогда
        Значение = '20240101';
    КонецЕсли;
    Результат = Лев("abc", Значение);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::TypeMismatch,
            expect![[r#"
                TypeMismatch @ 13:28..13:36
                  message: Несоответствие типов: ожидалось 'Число', получено 'Булево | ДатаВремя | Строка | … и ещё 1'
                  severity: Major"#]],
        );
    }

    #[test]
    fn a_form_control_whose_value_is_a_long_union_is_not_cut() {
        // The control is one type. Its value prints four members with the same separator a
        // union uses; splitting that label would hide the control and invent a count.
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
                <v8:Type>xs:string</v8:Type>
                <v8:Type>xs:dateTime</v8:Type>
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
            ["Несоответствие типов: ожидалось 'Число', получено 'ПолеФормы:Значение -> Булево | ДатаВремя | Число | Строка'"]
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
    fn shortened_doc_union_sides_stay_distinguishable() {
        // The first three members match, so shortening would print the same text on both
        // sides. The finding stays, and the full labels keep the tail that tells them apart.
        let code = r#"
// Параметры:
//   П1 - Число, Строка, Булево, ТаблицаЗначений - значение.
Процедура Принять(П1) Экспорт
КонецПроцедуры

Процедура Тест(У1, У2, У3)
    Значение = Неопределено;
    Если У1 Тогда
        Значение = Истина;
    КонецЕсли;
    Если У2 Тогда
        Значение = 1;
    КонецЕсли;
    Если У3 Тогда
        Значение = "а";
    КонецЕсли;
    Принять(Значение);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::TypeMismatchByDocComment,
            expect![[r#"
                TypeMismatchByDocComment @ 18:13..18:21
                  message: Несоответствие типов: ожидалось 'Булево | Число | Строка | ТаблицаЗначений', получено 'Булево | Число | Строка | Неопределено' (тип параметра из описания)
                  severity: Error"#]],
        );
    }

    const BOOLEAN_SLOT_MODULE: &str = "CommonModules/Проба/Ext/Module.bsl";

    const BOOLEAN_SLOT_CONFIGURATION_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Configuration uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Тест</Name></Properties>
        <ChildObjects>
            <Catalog>Товары</Catalog>
            <CommonModule>Проба</CommonModule>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#;

    const BOOLEAN_SLOT_CATALOG_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Catalog uuid="00000000-0000-0000-0000-000000000002">
        <Properties>
            <Name>Товары</Name>
            <Hierarchical>true</Hierarchical>
            <CodeLength>9</CodeLength>
            <DescriptionLength>100</DescriptionLength>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

    const BOOLEAN_SLOT_MODULE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <CommonModule uuid="00000000-0000-0000-0000-000000000008">
        <Properties>
            <Name>Проба</Name>
            <Server>true</Server>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

    fn boolean_slot_mismatches(body: &str) -> (String, Vec<crate::Diagnostic>) {
        let source = format!("Процедура Тест()\n{body}КонецПроцедуры\n");
        let files = [
            ("Configuration.xml", BOOLEAN_SLOT_CONFIGURATION_XML),
            ("Catalogs/Товары.xml", BOOLEAN_SLOT_CATALOG_XML),
            ("CommonModules/Проба.xml", BOOLEAN_SLOT_MODULE_XML),
            (BOOLEAN_SLOT_MODULE, source.as_str()),
        ];
        let found = check_config_files(&files, BOOLEAN_SLOT_MODULE)
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::TypeMismatch)
            .collect();
        (source, found)
    }

    #[test]
    fn a_number_in_a_boolean_platform_parameter_of_a_manager_method_is_silent() {
        if !built_in_facts_are_served() {
            return;
        }
        let (source, found) = boolean_slot_mismatches(
            "    Первый = Справочники.Товары.НайтиПоКоду(\"001\", 5);\n    \
             Второй = Справочники.Товары.НайтиПоКоду(\"001\", 0);\n",
        );
        expect![[""]].assert_eq(&format_diags(&source, &found));
    }

    #[test]
    fn a_number_in_a_boolean_parameter_of_a_global_function_is_silent() {
        if !built_in_facts_are_served() {
            return;
        }
        let (source, found) = boolean_slot_mismatches(
            "    Первый = СтрРазделить(\"a,b\", \",\", 5);\n    \
             Второй = СтрРазделить(\"a,b\", \",\", 0);\n",
        );
        expect![[""]].assert_eq(&format_diags(&source, &found));
    }

    #[test]
    fn a_date_and_an_array_in_a_boolean_platform_parameter_are_still_reported() {
        if !built_in_facts_are_served() {
            return;
        }
        let (source, found) = boolean_slot_mismatches(
            "    Первый = Справочники.Товары.НайтиПоКоду(\"001\", ТекущаяДата());\n    \
             Второй = Справочники.Товары.НайтиПоКоду(\"001\", Новый Массив);\n",
        );
        expect![[r#"
            TypeMismatch @ 2:52..2:65
              message: Несоответствие типов: ожидалось 'Булево', получено 'ДатаВремя'
              severity: Major
            TypeMismatch @ 3:52..3:64
              message: Несоответствие типов: ожидалось 'Булево', получено 'Массив'
              severity: Major"#]]
        .assert_eq(&format_diags(&source, &found));
    }

    #[test]
    fn a_number_in_a_boolean_documented_parameter_is_silent_and_a_date_is_still_reported() {
        // The language itself turns a number into a Boolean where one is expected (a
        // condition, a logical operator, a Boolean attribute), so a configuration
        // procedure documented as taking Булево accepts it as a platform method does.
        let code = r#"
// Параметры:
//   Флаг - Булево - флаг.
//   Признак - Булево, Неопределено - признак.
Процедура Принять(Флаг, Признак = Неопределено) Экспорт
КонецПроцедуры

Процедура Тест()
    Принять(5);
    Принять(0, 1);
    Принять(ТекущаяДата());
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::TypeMismatchByDocComment,
            expect![[r#"
                TypeMismatchByDocComment @ 11:13..11:26
                  message: Несоответствие типов: ожидалось 'Булево', получено 'ДатаВремя' (тип параметра из описания)
                  severity: Error"#]],
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
