//! Completion inside the string argument of `Тип("…")`.

use ide::{Analysis, CompletionItem};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup_with_config(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);

    let mut db = RootDatabaseImpl::new();
    let source_root_id = SourceRootId(0);

    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));

    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, source_root_id);
        db.set_file_text(*file_id, &file.content);
    }

    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

    (Analysis::from_database(db), test_file, cursor_offset)
}

fn extract_cursor(fixture_text: &str) -> (String, String, u32) {
    let abs_idx = fixture_text.find("$0").expect("fixture must contain $0 cursor marker");
    let prefix = &fixture_text[..abs_idx];
    let last_header_start = prefix.rfind("//- ").expect("cursor must be inside a //- file");
    let header_end =
        prefix[last_header_start..].find('\n').expect("//- header must end with newline")
            + last_header_start;
    let path_line = &prefix[last_header_start + 4..header_end];
    let file_offset_in_prefix = header_end + 1;
    let cursor_in_file = (abs_idx - file_offset_in_prefix) as u32;
    let cleaned = fixture_text.replacen("$0", "", 1);
    (cleaned, path_line.to_string(), cursor_in_file)
}

fn complete(fixture: &str) -> Vec<CompletionItem> {
    complete_in(fixture, ide::Locale::Ru)
}

fn complete_in(fixture: &str, locale: ide::Locale) -> Vec<CompletionItem> {
    let (analysis, file_id, offset) = setup_with_config(fixture);
    analysis.completions(file_id, offset, None, locale)
}

fn labels(items: &[CompletionItem]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

fn has_label(items: &[CompletionItem], label: &str) -> bool {
    items.iter().any(|i| i.label == label)
}

#[test]
fn type_string_after_ref_prefix_offers_configuration_objects() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("СправочникСсылка.$0") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    let item = items.iter().find(|i| i.label == "Справочник1").unwrap_or_else(|| {
        panic!("catalog name expected after `СправочникСсылка.`, got {:?}", labels(&items))
    });
    assert_eq!(item.insert_text, "Справочник1");
    assert_eq!(item.detail.as_deref(), Some("СправочникСсылка.Справочник1"));
    assert!(
        !has_label(&items, "Документ1"),
        "only names under the typed prefix must be offered, got {:?}",
        labels(&items)
    );
}

#[test]
fn type_string_narrows_by_partial_name_after_prefix() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("ДокументСсылка.Док$0") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    assert!(has_label(&items, "Документ1"), "got {:?}", labels(&items));
    assert!(!has_label(&items, "Справочник1"), "got {:?}", labels(&items));
}

#[test]
fn type_string_without_prefix_offers_platform_and_reference_types() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("$0") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    assert!(has_label(&items, "ТаблицаЗначений"), "got {:?}", labels(&items));
    assert!(has_label(&items, "СправочникСсылка.Справочник1"), "got {:?}", labels(&items));
    assert!(has_label(&items, "ДокументСсылка.Документ1"), "got {:?}", labels(&items));
    assert!(
        items.iter().all(|i| !i.label.contains('<') && !i.label.contains('&')),
        "templated placeholder names must not be offered"
    );
}

#[test]
fn string_outside_type_call_gets_no_type_names() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Сообщить("СправочникСсылка.$0");
КонецПроцедуры
"#,
    );

    assert!(!has_label(&items, "Справочник1"), "got {:?}", labels(&items));
}

#[test]
fn cursor_after_closing_quote_is_outside_type_string() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("СправочникСсылка.Справочник1"$0) Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    assert!(!has_label(&items, "СправочникСсылка.Справочник1"), "got {:?}", labels(&items));
    assert!(!has_label(&items, "Справочник1"), "got {:?}", labels(&items));
}

const PRIMITIVE_TYPES: [&str; 6] = ["Число", "Строка", "Дата", "Булево", "Неопределено", "Null"];

#[test]
fn type_string_without_prefix_offers_primitive_types() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("$0") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    for name in PRIMITIVE_TYPES {
        let count = items.iter().filter(|i| i.label == name).count();
        assert_eq!(count, 1, "`{name}` expected exactly once, got {:?}", labels(&items));
    }
}

#[test]
fn type_string_partial_primitive_name_offers_it() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("Чи$0") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    let item = items
        .iter()
        .find(|i| i.label == "Число")
        .unwrap_or_else(|| panic!("`Число` expected, got {:?}", labels(&items)));
    assert_eq!(item.insert_text, "Число");
}

#[test]
fn primitive_types_are_not_offered_after_ref_prefix() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("СправочникСсылка.$0") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    assert!(has_label(&items, "Справочник1"), "got {:?}", labels(&items));
    for name in PRIMITIVE_TYPES {
        assert!(
            !has_label(&items, name),
            "`{name}` after a dotted prefix, got {:?}",
            labels(&items)
        );
    }
}

#[test]
fn string_argument_of_method_on_variable_named_type_gets_no_type_names() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Тип = Новый Соответствие;
    Тип.Вставить("СправочникСсылка.$0", 1);
КонецПроцедуры
"#,
    );

    assert!(!has_label(&items, "Справочник1"), "got {:?}", labels(&items));
}

#[test]
fn string_argument_of_method_on_type_call_result_gets_no_type_names() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Тип("Строка").Метод("ДокументСсылка.$0");
КонецПроцедуры
"#,
    );

    assert!(!has_label(&items, "Документ1"), "got {:?}", labels(&items));
}

#[test]
fn english_type_call_offers_type_names() {
    let items = complete(
        r#"//- /test.bsl
Procedure Check(Value)
    If TypeOf(Value) = Type("ДокументСсылка.$0") Then
        Return;
    EndIf;
EndProcedure
"#,
    );

    assert!(has_label(&items, "Документ1"), "got {:?}", labels(&items));
}

#[test]
fn english_ref_prefix_offers_configuration_objects_in_english_locale() {
    let items = complete_in(
        r#"//- /test.bsl
Procedure Check(Value)
    If TypeOf(Value) = Type("DocumentRef.$0") Then
        Return;
    EndIf;
EndProcedure
"#,
        ide::Locale::En,
    );

    let item = items.iter().find(|i| i.label == "Документ1").unwrap_or_else(|| {
        panic!("got {:?}", labels(&items));
    });
    assert_eq!(item.detail.as_deref(), Some("DocumentRef.Документ1"));
}

#[test]
fn english_ref_prefix_is_recognised_in_russian_locale() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("DocumentRef.$0") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    assert!(has_label(&items, "Документ1"), "got {:?}", labels(&items));
}

#[test]
fn type_string_without_prefix_offers_english_names_in_english_locale() {
    let items = complete_in(
        r#"//- /test.bsl
Procedure Check(Value)
    If TypeOf(Value) = Type("$0") Then
        Return;
    EndIf;
EndProcedure
"#,
        ide::Locale::En,
    );

    assert!(has_label(&items, "DocumentRef.Документ1"), "got {:?}", labels(&items));
    assert!(has_label(&items, "Number"), "got {:?}", labels(&items));
    assert!(!has_label(&items, "ДокументСсылка.Документ1"), "got {:?}", labels(&items));
}

#[test]
fn string_concatenated_inside_type_call_gets_no_type_names() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("СправочникСсылка.$0" + "x") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    assert!(!has_label(&items, "Справочник1"), "got {:?}", labels(&items));
}

#[test]
fn index_key_inside_type_call_gets_no_type_names() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Словарь = Новый Соответствие;
    Если ТипЗнч(Значение) = Тип(Словарь["ДокументСсылка.$0"]) Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    assert!(!has_label(&items, "Документ1"), "got {:?}", labels(&items));
}

#[test]
fn continuation_line_of_multiline_type_string_offers_nothing() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("СправочникСсылка.
    |Спр$0") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    assert!(
        !items.iter().any(|i| i.insert_text.contains("СправочникСсылка.")),
        "got {:?}",
        labels(&items)
    );
}

#[test]
fn first_line_of_multiline_type_string_keeps_completion() {
    let items = complete(
        r#"//- /test.bsl
Процедура ОбработатьЗначение(Значение)
    Если ТипЗнч(Значение) = Тип("СправочникСсылка.$0
    |") Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры
"#,
    );

    assert!(has_label(&items, "Справочник1"), "got {:?}", labels(&items));
}
