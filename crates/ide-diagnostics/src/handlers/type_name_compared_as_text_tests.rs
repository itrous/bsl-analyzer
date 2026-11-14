use crate::test_utils::{check_diagnostics_for, range_to_line_col};
use crate::DiagnosticCode;

const MESSAGE: &str = "Сравнение с текстом имени типа: текст зависит от языка интерфейса, сравнивать типы следует через Тип()";

fn text_of(body: &str) -> String {
    format!("Процедура Проба(Значение)\n{body}\nКонецПроцедуры\n")
}

fn run(body: &str) -> Vec<crate::Diagnostic> {
    check_diagnostics_for(&text_of(body), DiagnosticCode::TypeNameComparedAsText)
}

fn run_module(source: &str) -> Vec<crate::Diagnostic> {
    check_diagnostics_for(source, DiagnosticCode::TypeNameComparedAsText)
}

fn anchored_text<'a>(text: &'a str, d: &crate::Diagnostic) -> &'a str {
    let start: usize = d.range.start().into();
    let end: usize = d.range.end().into();
    &text[start..end]
}

// ---- the flagged shape -------------------------------------------------------------------

#[test]
fn string_of_type_of_compared_with_a_literal_is_flagged_on_the_string_call() {
    let body = "    Если Строка(ТипЗнч(Элемент)) = \"Табличное поле\" Тогда\n    КонецЕсли;";
    let d = run(body);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].message, MESSAGE);
    let (start_line, start_col, _, end_col) = range_to_line_col(&text_of(body), d[0].range);
    assert_eq!((start_line, start_col, end_col), (1, 9, 32), "{:?}", d[0]);
}

#[test]
fn both_operand_orders_and_both_operators_are_flagged_on_the_call() {
    for condition in [
        "Строка(ТипЗнч(Элемент)) = \"Табличное поле\"",
        "\"Табличное поле\" = Строка(ТипЗнч(Элемент))",
        "Строка(ТипЗнч(Элемент)) <> \"Табличное поле\"",
        "\"Табличное поле\" <> Строка(ТипЗнч(Элемент))",
    ] {
        let body = format!("    Если {condition} Тогда\n    КонецЕсли;");
        let d = run(&body);
        assert_eq!(d.len(), 1, "{condition}: {d:?}");
        assert_eq!(anchored_text(&text_of(&body), &d[0]), "Строка(ТипЗнч(Элемент))", "{condition}");
    }
}

#[test]
fn type_name_through_a_local_is_flagged_on_the_comparison() {
    let body = "    Имя = Строка(ТипЗнч(Объект));\n    Если Имя = \"Внешняя обработка объект\" Тогда\n    КонецЕсли;";
    let d = run(body);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(
        anchored_text(&text_of(body), &d[0]),
        "Имя = \"Внешняя обработка объект\"",
        "{:?}",
        d[0]
    );
}

#[test]
fn text_preserving_wrappers_around_the_name_are_flagged_on_the_call() {
    let d = run("    Если НРег(Строка(ТипЗнч(Объект))) = \"документ\" Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
    let d =
        run("    Если ВРег(СокрЛП(Строка(ТипЗнч(Объект)))) = \"ДОКУМЕНТ\" Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn wrapper_around_a_local_is_flagged_on_the_wrapper_call() {
    let body = "    Имя = Строка(ТипЗнч(Объект));\n    Если СокрЛП(Имя) = \"Внешняя обработка объект\" Тогда\n    КонецЕсли;";
    let d = run(body);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(anchored_text(&text_of(body), &d[0]), "СокрЛП(Имя)", "{:?}", d[0]);
}

#[test]
fn english_spellings_of_the_comparison_are_flagged_the_same_way() {
    let d = run("    If String(TypeOf(Object)) = \"Document\" Then\n    EndIf;");
    assert_eq!(d.len(), 1, "{d:?}");
    let d = run("    If Upper(TrimAll(String(TypeOf(Object)))) = \"DOCUMENT\" Then\n    EndIf;");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn each_comparison_is_reported_once() {
    let d = run(
        "    Если Строка(ТипЗнч(А)) = \"X\" Или Строка(ТипЗнч(Б)) = \"Y\" Тогда\n    КонецЕсли;",
    );
    assert_eq!(d.len(), 2, "{d:?}");
    assert_ne!(d[0].range, d[1].range);
}

// ---- not flagged -------------------------------------------------------------------------

#[test]
fn a_parameter_compared_with_a_literal_is_not_flagged() {
    let d = run("    Если Значение = \"Табличное поле\" Тогда\n    КонецЕсли;");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_module_variable_compared_with_a_literal_is_not_flagged() {
    let d = run_module(
        "Перем Имя;\n\nПроцедура Проба()\n    Если Имя = \"Табличное поле\" Тогда\n    КонецЕсли;\nКонецПроцедуры\n",
    );
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_plain_literal_compared_with_a_literal_is_not_flagged() {
    let d = run("    Если \"Табличное поле\" = \"Табличное поле\" Тогда\n    КонецЕсли;");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_type_name_text_compared_with_a_non_literal_is_not_flagged() {
    let d = run("    Если Строка(ТипЗнч(А)) = Строка(ТипЗнч(Б)) Тогда\n    КонецЕсли;");
    assert!(d.is_empty(), "{d:?}");
    let d = run("    Если Строка(ТипЗнч(А)) = Значение Тогда\n    КонецЕсли;");
    assert!(d.is_empty(), "{d:?}");
    let d = run("    Если Строка(ТипЗнч(А)) = Фраза Тогда\n    КонецЕсли;");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_write_through_a_call_proven_by_reference_is_not_flagged() {
    let body = "    Имя = Строка(ТипЗнч(Объект));\n    ВвестиСтроку(Имя);\n    Если Имя = \"Документ\" Тогда\n    КонецЕсли;";
    let d = run(body);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_value_a_call_may_have_rewritten_without_attestation_is_not_flagged() {
    // Measured over five real configurations: reporting this shape gives 11582 findings against
    // 10 without it, and the extra ones are plain string comparisons (`Если ИмяЭлемента =
    // "КодБанка"`), because the walk answers this reason before it looks at the value's origin.
    let body = "    Имя = Строка(ТипЗнч(Объект));\n    НеизвестнаяПроцедура(Имя);\n    Если Имя = \"Документ\" Тогда\n    КонецЕсли;";
    let d = run(body);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_local_assigned_a_type_name_and_something_else_is_not_flagged() {
    let d = run("    Имя = Строка(ТипЗнч(Объект));\n    Имя = \"Документ\";\n    Если Имя = \"Документ\" Тогда\n    КонецЕсли;");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn exception_text_with_the_type_name_is_not_flagged() {
    let d = run("    ВызватьИсключение \"тип: \" + Строка(ТипЗнч(Значение));");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn type_name_that_is_only_concatenated_or_stored_is_not_flagged() {
    let d = run("    Ключ = Строка(ТипЗнч(Значение)) + \":\";");
    assert!(d.is_empty(), "{d:?}");
    let d = run("    Имя = Строка(ТипЗнч(Значение));\n    Сообщить(Имя);\n    Возврат Имя;");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn plain_string_comparison_without_a_type_name_is_not_flagged() {
    let d = run("    Если Строка(Значение) = \"Табличное поле\" Тогда\n    КонецЕсли;");
    assert!(d.is_empty(), "{d:?}");
    let d = run("    Если ТипЗнч(Значение) = Тип(\"Строка\") Тогда\n    КонецЕсли;");
    assert!(d.is_empty(), "{d:?}");
}

// ---- shapes of the earlier name-matching rule that are deliberately NOT caught yet -------
//
// Each needs a function identified by its NAME (or a literal that arrives indirectly), which
// this rule may not do. Identifying them needs new curated platform data - a separate step.
// The cases stay here, ignored, so that the knowledge of what is not caught is not lost.

#[test]
#[ignore = "not caught yet: Найти(<имя типа>, \"литерал\") compared with a number needs the function identified by name; needs curated platform data, a separate step"]
fn find_in_the_type_name_against_zero_is_flagged() {
    let d = run("    Если Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Внешняя обработка\") > 0 Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
    let d = run("    Если Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Отчет\") = 0 Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
#[ignore = "not caught yet: Найти(<имя типа>, \"литерал\") compared with any number, on either side, needs the function identified by name; needs curated platform data, a separate step"]
fn find_against_any_number_literal_is_flagged() {
    let d = run("    Если Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Внешняя обработка объект:\") = 1 Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
    let d = run("    Если 0 < Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Отчет\") Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
    let d = run("    Если Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Отчет\") >= 1 Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
#[ignore = "not caught yet: СтрНачинаетсяС / СтрЗаканчиваетсяНа with a literal are predicates that need the function identified by name; needs curated platform data, a separate step"]
fn starts_with_and_ends_with_a_literal_are_flagged() {
    let d =
        run("    Если СтрНачинаетсяС(Строка(ТипЗнч(Объект)), \"Документ\") Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
    let d = run(
        "    Если СтрЗаканчиваетсяНа(Строка(ТипЗнч(Объект)), \"Ссылка\") Тогда\n    КонецЕсли;",
    );
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
#[ignore = "not caught yet: the English spellings Find / StrStartsWith need the function identified by name; needs curated platform data, a separate step"]
fn english_find_and_starts_with_are_flagged() {
    let d = run("    If Find(String(TypeOf(Object)), \"Report\") > 0 Then\n    EndIf;");
    assert_eq!(d.len(), 1, "{d:?}");
    let d = run("    If StrStartsWith(String(TypeOf(Object)), \"Doc\") Then\n    EndIf;");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
#[ignore = "not caught yet: a localized literal НСтр(\"...\") is recognised only by the name of the function that builds it; needs curated platform data, a separate step"]
fn type_name_compared_with_a_localized_literal_is_flagged() {
    let d = run("    Если Строка(ТипЗнч(Объект)) = НСтр(\"ru='Документ'\") Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
#[ignore = "not caught yet: a literal arriving through a variable is outside the rule, which accepts only a string literal written in place; needs a value-origin fact for literals, a separate step"]
fn type_name_compared_with_a_variable_assigned_only_a_literal_is_flagged() {
    let body = "    Фраза = \"Внешняя обработка объект\";\n    Имя = Строка(ТипЗнч(Объект));\n    Если Лев(Имя, СтрДлина(Фраза)) = Фраза Тогда\n    КонецЕсли;";
    let d = run(body);
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
#[ignore = "not caught yet: Лев / Left keep only a prefix of the text, so the platform data does not mark their result as following the argument's text; whether a prefix still counts is a separate decision and separate curated data"]
fn a_prefix_of_the_type_name_is_flagged() {
    let d = run("    Если Лев(Строка(ТипЗнч(Объект)), 8) = \"Документ\" Тогда\n    КонецЕсли;");
    assert_eq!(d.len(), 1, "{d:?}");
    let body = "    Имя = Строка(ТипЗнч(Объект));\n    Если Лев(Имя, 24) = \"Внешняя обработка объект\" Тогда\n    КонецЕсли;";
    let d = run(body);
    assert_eq!(d.len(), 1, "{d:?}");
    let d = run("    If Left(String(TypeOf(Object)), 8) = \"Document\" Then\n    EndIf;");
    assert_eq!(d.len(), 1, "{d:?}");
}

// ---- the control: the rule file spells no function and no type ----------------------------

/// The rule decides from the value-origin fact, never from the spelling of a name. This reads
/// the rule's own source and fails when a name list is brought back:
///
/// * a line that is not a comment and not the `MESSAGE` constant may contain no Cyrillic letter
///   (a Russian spelling of a function or a type);
/// * such a line may contain no double quote either, except the empty `lsp_severity_override`
///   of the metadata block (a Latin spelling would get past the first check) - so the file
///   holds no string literal other than the message.
///
/// The rule's tests live in this separate file precisely so that the whole rule file can be
/// scanned, with nothing exempt but its comments and its message.
#[test]
fn the_rule_file_spells_no_function_or_type_name() {
    let source = include_str!("type_name_compared_as_text.rs");
    let mut scanned = 0usize;
    for (number, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("const MESSAGE: &str = \"") {
            continue;
        }
        scanned += 1;
        let at = number + 1;
        assert!(
            !line.chars().any(|c| ('\u{0400}'..='\u{052F}').contains(&c)),
            "line {at} of the rule file has a Cyrillic letter outside the message and comments: {line}"
        );
        if line.contains('"') {
            assert_eq!(
                trimmed, "lsp_severity_override: \"\",",
                "line {at} of the rule file has a string literal other than the message: {line}"
            );
        }
    }
    assert!(scanned > 30, "the scan saw only {scanned} lines: the control reads the wrong file");
    assert!(
        source.lines().any(|l| l.starts_with("const MESSAGE: &str = \"")),
        "the control no longer finds the message constant"
    );
}
