use ide::{Analysis, CompletionItem};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn setup(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);

    let mut db = RootDatabaseImpl::new();
    let source_root_id = SourceRootId(0);

    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(source_root_id, source_root);

    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, source_root_id);
        db.set_file_text(*file_id, &file.content);
    }

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
    let (analysis, file_id, offset) = setup(fixture);
    analysis.completions(file_id, offset, None, ide::Locale::Ru)
}

fn has_label(items: &[CompletionItem], label: &str) -> bool {
    items.iter().any(|i| i.label == label)
}

fn labels(items: &[CompletionItem]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_new_value_table() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    ТЗ = Новый ТаблицаЗначений;
    ТЗ.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(!items.is_empty(), "ТаблицаЗначений members must be offered; got empty");
    for expected in ["Добавить", "Очистить", "Количество", "НайтиСтроки", "Колонки"]
    {
        assert!(
            has_label(&items, expected),
            "ValueTable member `{expected}` must be offered; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_new_structure() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Стр = Новый Структура;
    Стр.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(!items.is_empty(), "Структура members must be offered; got empty");
    for expected in ["Вставить", "Свойство", "Очистить", "Количество", "Удалить"]
    {
        assert!(
            has_label(&items, expected),
            "Structure member `{expected}` must be offered; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_new_value_tree() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    ДЗ = Новый ДеревоЗначений;
    ДЗ.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(!items.is_empty(), "ДеревоЗначений members must be offered; got empty");
    for expected in ["Колонки", "Строки", "Скопировать"] {
        assert!(
            has_label(&items, expected),
            "ValueTree member `{expected}` must be offered; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_new_value_table_with_prefix() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    ТЗ = Новый ТаблицаЗначений;
    ТЗ.Доб$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(!items.is_empty(), "ТЗ.Доб — at least `Добавить` must remain; got empty");
    // Fuzzy member matching: `Добавить` (prefix hit) must be offered and ranked in
    // the best quality tier (sort_text starts with `0`).
    let dobavit = items
        .iter()
        .find(|i| i.label == "Добавить")
        .unwrap_or_else(|| panic!("`Добавить` must be in the filtered set; got: {ls:?}"));
    assert!(
        dobavit.sort_text.as_deref().is_some_and(|s| s.starts_with('0')),
        "prefix hit `Добавить` must be top quality tier; got {:?}",
        dobavit.sort_text
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_local_from_same_module_fn_returning_value_table() {
    let items = complete(
        r#"//- /test.bsl
Функция Получить()
    Возврат Новый ТаблицаЗначений;
КонецФункции

Процедура Тест()
    Х = Получить();
    Х.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "cascade typing must propagate ТаблицаЗначений through `Получить()` to `Х`; got empty"
    );
    for expected in ["Добавить", "Колонки", "Количество"] {
        assert!(
            has_label(&items, expected),
            "cascade-typed local `Х` must surface ValueTable member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_local_from_same_module_fn_returning_structure() {
    let items = complete(
        r#"//- /test.bsl
Функция СоздатьОтбор()
    Возврат Новый Структура;
КонецФункции

Процедура Тест()
    Отбор = СоздатьОтбор();
    Отбор.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "cascade typing must propagate Структура through `СоздатьОтбор()` to `Отбор`; got empty"
    );
    for expected in ["Вставить", "Свойство"] {
        assert!(
            has_label(&items, expected),
            "cascade-typed local `Отбор` must surface Structure member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_two_hop_same_module_cascade_value_table() {
    let items = complete(
        r#"//- /test.bsl
Функция Inner()
    Возврат Новый ТаблицаЗначений;
КонецФункции

Функция Outer()
    Возврат Inner();
КонецФункции

Процедура Тест()
    Х = Outer();
    Х.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "2-hop cascade Inner→Outer must propagate ТаблицаЗначений; got empty"
    );
    for expected in ["Добавить", "Колонки", "Количество"] {
        assert!(
            has_label(&items, expected),
            "2-hop cascade must surface ValueTable member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_two_hop_same_module_cascade_structure() {
    let items = complete(
        r#"//- /test.bsl
Функция СоздатьБазу()
    Возврат Новый Структура;
КонецФункции

Функция Прочитать()
    Возврат СоздатьБазу();
КонецФункции

Процедура Тест()
    Отбор = Прочитать();
    Отбор.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(!items.is_empty(), "2-hop cascade Прочитать→СоздатьБазу must propagate Структура");
    for expected in ["Вставить", "Свойство"] {
        assert!(
            has_label(&items, expected),
            "2-hop cascade must surface Structure member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_qualified_call_cross_module_value_table() {
    let items = complete(
        r#"//- /CommonModules/Util/Ext/Module.bsl
Функция СоздатьТЗ() Экспорт
    Возврат Новый ТаблицаЗначений;
КонецФункции

//- /test.bsl
Процедура Тест()
    Х = Util.СоздатьТЗ();
    Х.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "cross-module qualified call `Util.СоздатьТЗ()` must propagate ТаблицаЗначений; got empty"
    );
    for expected in ["Добавить", "Колонки"] {
        assert!(
            has_label(&items, expected),
            "cross-module cascade must surface ValueTable member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_two_hop_cross_module_cascade_structure() {
    let items = complete(
        r#"//- /CommonModules/Util/Ext/Module.bsl
Функция Inner() Экспорт
    Возврат Новый Структура;
КонецФункции

Функция Outer() Экспорт
    Возврат Inner();
КонецФункции

//- /test.bsl
Процедура Тест()
    Отбор = Util.Outer();
    Отбор.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "2-hop cross-module cascade `Util.Outer()→Util.Inner()` must propagate Структура; got empty"
    );
    for expected in ["Вставить", "Свойство"] {
        assert!(
            has_label(&items, expected),
            "2-hop cross-module cascade must surface Structure member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_method_graph_hop_then_property_hop_value_table_columns() {
    let items = complete(
        r#"//- /test.bsl
Функция Получить()
    Возврат Новый ТаблицаЗначений;
КонецФункции

Процедура Тест()
    Х = Получить();
    Y = Х.Колонки;
    Y.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "method-graph + property hop must reach ValueTableColumnCollection; got empty"
    );
    for expected in ["Добавить", "Найти", "Количество"] {
        assert!(
            has_label(&items, expected),
            "mixed-hop cascade must surface column-collection member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_local_from_same_module_fn_returning_array_callee_below() {
    let items = complete(
        r#"//- /test.bsl
Функция ТестированиеТипов()
    Пар = СписокПараметров();
    Пар.$0
КонецФункции

Функция СписокПараметров()
    Возврат Новый Массив;
КонецФункции
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "cascade typing must propagate Массив through `СписокПараметров()` to `Пар`; got empty"
    );
    for expected in ["Добавить", "Количество", "Найти"] {
        assert!(
            has_label(&items, expected),
            "cascade-typed local `Пар` must surface Array member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_local_from_same_module_fn_value_table_callee_below() {
    let items = complete(
        r#"//- /test.bsl
Функция Тест()
    Х = Получить();
    Х.$0
КонецФункции

Функция Получить()
    Возврат Новый ТаблицаЗначений;
КонецФункции
"#,
    );

    let ls = labels(&items);
    assert!(!items.is_empty(), "ValueTable callee-below cascade must work; got empty: {ls:?}");
    for expected in ["Добавить", "Колонки", "Количество"] {
        assert!(
            has_label(&items, expected),
            "ValueTable callee-below cascade must surface `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_local_from_same_module_fn_array_callee_above() {
    let items = complete(
        r#"//- /test.bsl
Функция СписокПараметров()
    Возврат Новый Массив;
КонецФункции

Функция ТестированиеТипов()
    Пар = СписокПараметров();
    Пар.$0
КонецФункции
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "Array callee-above cascade must propagate Ty::Array; got empty: {ls:?}"
    );
    for expected in ["Добавить", "Количество", "Найти"] {
        assert!(
            has_label(&items, expected),
            "Array callee-above cascade must surface `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_local_from_self_recursive_value_table() {
    let items = complete(
        r#"//- /test.bsl
Функция СписокШагов(Глубина)
    Если Глубина = 0 Тогда
        Возврат Новый ТаблицаЗначений;
    КонецЕсли;
    Возврат СписокШагов(Глубина - 1);
КонецФункции

Процедура Тест()
    ТЗ = СписокШагов(5);
    ТЗ.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "self-recursive callee must still cascade ValueTable to caller; got empty: {ls:?}"
    );
    for expected in ["Добавить", "Колонки", "Количество"] {
        assert!(
            has_label(&items, expected),
            "self-recursion cascade must surface ValueTable member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_local_from_mutual_recursion_structure() {
    let items = complete(
        r#"//- /test.bsl
Функция Четный(Х)
    Если Х = 0 Тогда
        Возврат Новый Структура;
    КонецЕсли;
    Возврат Нечетный(Х - 1);
КонецФункции

Функция Нечетный(Х)
    Если Х = 0 Тогда
        Возврат Новый Структура;
    КонецЕсли;
    Возврат Четный(Х - 1);
КонецФункции

Процедура Тест()
    Стр = Четный(7);
    Стр.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "mutual-recursion cascade must surface Structure members; got empty: {ls:?}"
    );
    for expected in ["Вставить", "Свойство"] {
        assert!(
            has_label(&items, expected),
            "mutual-recursion cascade must surface Structure member `{expected}`; got: {ls:?}"
        );
    }
}

#[test]
fn completion_after_dot_on_local_from_pure_self_recursion_yields_no_items() {
    let items = complete(
        r#"//- /test.bsl
Функция Бесконечная()
    Возврат Бесконечная();
КонецФункции

Процедура Тест()
    Х = Бесконечная();
    Х.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !has_label(&items, "Добавить") && !has_label(&items, "Колонки"),
        "pure self-recursion must NOT spuriously surface ValueTable members; got: {ls:?}"
    );
    assert!(
        !has_label(&items, "Вставить") && !has_label(&items, "Свойство"),
        "pure self-recursion must NOT spuriously surface Structure members; got: {ls:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_structure_returned_from_fn_lists_keys_and_methods() {
    let items = complete(
        r#"//- /test.bsl
Функция ПараметрыСоединения()
    Пар = Новый Структура("Таймаут, Адрес");
    Пар.Вставить("Шифрование");
    Возврат Пар;
КонецФункции

Процедура Тест()
    Стр = ПараметрыСоединения();
    Стр.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    for method in ["Вставить", "Свойство", "Количество", "Удалить", "Очистить"]
    {
        assert!(
            has_label(&items, method),
            "Structure method `{method}` must be offered; got: {ls:?}"
        );
    }
    for key in ["Таймаут", "Адрес", "Шифрование"] {
        assert!(
            has_label(&items, key),
            "Structure key `{key}` must be offered alongside methods; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_same_body_structure_lists_constructor_and_insert_keys() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Стр = Новый Структура("Таймаут, Адрес");
    Стр.Вставить("Шифрование");
    Стр.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    for key in ["Таймаут", "Адрес", "Шифрование"] {
        assert!(
            has_label(&items, key),
            "literal structure key `{key}` must be offered in the same body; got: {ls:?}"
        );
    }
    for method in ["Вставить", "Свойство"] {
        assert!(
            has_label(&items, method),
            "platform Structure method `{method}` must remain alongside keys; got: {ls:?}"
        );
    }
}

/// У динамической формы второй аргумент — массив параметров конструктора, а не
/// строка ключей: разбирать его как `Новый Структура("К1,К2")` нельзя. Тест
/// падает, если имя типа из строкового литерала начнут подставлять в
/// `Expr::New::type_name` — тогда сама строка `"Структура"` уедет в позицию
/// ключей и породит ключ-призрак.
#[test]
fn completion_on_dynamically_named_structure_offers_no_phantom_keys() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Стр = Новый("Структура", "Таймаут, Адрес");
    Стр.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    for key in ["Структура", "Таймаут", "Адрес"] {
        assert!(
            !has_label(&items, key),
            "dynamic `Новый(<Тип>, <Параметры>)` carries no key string, so `{key}` must not be \
             offered as a structure key; got: {ls:?}"
        );
    }
}

#[test]
fn completion_after_dot_on_nested_structure_value_lists_inner_keys() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    С = Новый Структура;
    С.Вставить("Адрес", Новый Структура("Город, Улица"));
    С.Адрес.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    for key in ["Город", "Улица"] {
        assert!(
            has_label(&items, key),
            "nested structure key `{key}` must be offered after `С.Адрес.`; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_non_literal_key_does_not_break_known_keys() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест(ИмяКлюча)
    Стр = Новый Структура("Известный");
    Стр.Вставить(ИмяКлюча, 1);
    Стр.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        has_label(&items, "Известный"),
        "a non-literal `.Вставить(ИмяКлюча, …)` must not drop the known key; got: {ls:?}"
    );
    assert!(has_label(&items, "Вставить"), "platform methods must remain; got: {ls:?}");
}

#[test]
#[ignore = "ValueTable column tracking not implemented — Ty::ValueTable has no \
            payload for declared columns; `Колонки.Добавить(\"Имя\", …)` mutations \
            are not propagated to row receivers in `Для Каждого … Из ТЗ`. \
            Tech-debt pin (same shape as Structure key tracking)."]
fn completion_after_for_each_row_lists_declared_columns() {
    let items = complete(
        r#"//- /test.bsl
Функция СоздатьТаблицу()
    ТЗ = Новый ТаблицаЗначений;
    ТЗ.Колонки.Добавить("Артикул");
    ТЗ.Колонки.Добавить("Цена");
    Возврат ТЗ;
КонецФункции

Процедура Тест()
    ТЗ = СоздатьТаблицу();
    Для Каждого Стр Из ТЗ Цикл
        Стр.$0
    КонецЦикла;
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    for column in ["Артикул", "Цена"] {
        assert!(
            has_label(&items, column),
            "declared column `{column}` must be offered on row receiver; got: {ls:?}"
        );
    }
    for member in ["Владелец", "НомерСтроки"] {
        assert!(
            has_label(&items, member),
            "row platform member `{member}` must remain alongside columns; got: {ls:?}"
        );
    }
}

#[test]
#[ignore = "ValueTable column tracking not implemented — `ТЗ.Колонки.|` should \
            surface declared column names alongside ValueTableColumnCollection \
            methods. Tech-debt pin."]
fn completion_after_dot_on_columns_lists_declared_column_names() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    ТЗ = Новый ТаблицаЗначений;
    ТЗ.Колонки.Добавить("Артикул");
    ТЗ.Колонки.Добавить("Цена");
    ТЗ.Колонки.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    for method in ["Добавить", "Найти", "Количество"] {
        assert!(
            has_label(&items, method),
            "ColumnCollection method `{method}` must be offered; got: {ls:?}"
        );
    }
    for column in ["Артикул", "Цена"] {
        assert!(
            has_label(&items, column),
            "declared column name `{column}` must be offered on .Колонки; got: {ls:?}"
        );
    }
}

#[test]
#[ignore = "Composite payload tracking not implemented — requires Structure \
            key→value-type propagation AND ValueTable column tracking AND \
            cross-receiver iteration. Tech-debt pin (depends on both \
            Structure key and ValueTable column tracking pins)."]
fn completion_for_each_row_from_chained_structure_key_value_table() {
    let items = complete(
        r#"//- /test.bsl
Функция Тест2()
    ТЗ = Новый ТаблицаЗначений;
    ТЗ.Колонки.Добавить("Артикул", Новый ОписаниеТипов("Строка"));
    ТЗ.Колонки.Добавить("Цена", Новый ОписаниеТипов("Число"));
    Возврат Новый Структура("Таб", ТЗ);
КонецФункции

Функция Тест()
    Для Каждого Стр Из Тест2().Таб Цикл
        Стр.$0
    КонецЦикла;
КонецФункции
"#,
    );

    let ls = labels(&items);
    for column in ["Артикул", "Цена"] {
        assert!(
            has_label(&items, column),
            "column `{column}` must propagate Structure-key → ValueTable → row; got: {ls:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_chained_dot_on_value_table_columns() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    ТЗ = Новый ТаблицаЗначений;
    ТЗ.Колонки.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        !items.is_empty(),
        "ТЗ.Колонки.| must surface ValueTableColumnCollection methods; got empty"
    );
    for expected in ["Добавить", "Найти", "Количество"] {
        assert!(
            has_label(&items, expected),
            "ValueTableColumnCollection member `{expected}` must be offered; got: {ls:?}"
        );
    }
}

// ---- Stage 2: interprocedural keys (helpers / child methods) ----

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_keys_from_same_module_helper() {
    let items = complete(
        r#"//- /test.bsl
Процедура ЗаполнитьЛокально(С)
    С.Вставить("Дата");
    С.Вставить("Номер");
КонецПроцедуры

Процедура Тест()
    С = Новый Структура;
    ЗаполнитьЛокально(С);
    С.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    for key in ["Дата", "Номер"] {
        assert!(
            has_label(&items, key),
            "key `{key}` inserted by a same-module helper must surface; got: {ls:?}"
        );
    }
    assert!(has_label(&items, "Вставить"), "platform methods must remain; got: {ls:?}");
}

#[test]
fn completion_keys_from_two_hop_child_helper() {
    let items = complete(
        r#"//- /test.bsl
Процедура Внутренний(П)
    П.Вставить("Товары");
КонецПроцедуры

Процедура Внешний(П)
    Внутренний(П);
КонецПроцедуры

Процедура Тест()
    С = Новый Структура;
    Внешний(С);
    С.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        has_label(&items, "Товары"),
        "key inserted by a child helper (Внешний→Внутренний) must propagate; got: {ls:?}"
    );
}

#[test]
fn completion_byval_param_keys_not_propagated() {
    let items = complete(
        r#"//- /test.bsl
Процедура ЗаполнитьКопию(Знач П)
    П.Вставить("Эфемерный");
КонецПроцедуры

Процедура Тест()
    С = Новый Структура("Реальный");
    ЗаполнитьКопию(С);
    С.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(has_label(&items, "Реальный"), "literal key must remain; got: {ls:?}");
    assert!(
        !has_label(&items, "Эфемерный"),
        "a key inserted into a `Знач` (by-value copy) param must NOT reach the caller; got: {ls:?}"
    );
}

#[test]
fn completion_keys_from_common_module_helper() {
    let items = complete(
        r#"//- /CommonModules/Util/Ext/Module.bsl
Процедура Заполнить(П) Экспорт
    П.Вставить("ИзМодуля");
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    С = Новый Структура;
    Util.Заполнить(С);
    С.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        has_label(&items, "ИзМодуля"),
        "key inserted by a common-module helper `Util.Заполнить` must propagate; got: {ls:?}"
    );
}

#[test]
fn completion_nested_literal_key_from_helper() {
    let items = complete(
        r#"//- /test.bsl
Процедура Заполнить(П)
    П.Вставить("Адрес", Новый Структура("Город, Улица"));
КонецПроцедуры

Процедура Тест()
    С = Новый Структура;
    Заполнить(С);
    С.Адрес.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    for key in ["Город", "Улица"] {
        assert!(
            has_label(&items, key),
            "nested structure key `{key}` inserted by a helper must surface after `С.Адрес.`; got: {ls:?}"
        );
    }
}

#[test]
fn completion_mutual_recursion_helpers_terminate_with_key_union() {
    let items = complete(
        r#"//- /test.bsl
Процедура F(П)
    П.Вставить("A");
    G(П);
КонецПроцедуры

Процедура G(П)
    П.Вставить("B");
    F(П);
КонецПроцедуры

Процедура Тест()
    С = Новый Структура;
    F(С);
    С.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    for key in ["A", "B"] {
        assert!(
            has_label(&items, key),
            "mutual-recursion helpers must terminate and union keys; got: {ls:?}"
        );
    }
}

#[test]
fn completion_forward_before_construction_is_ignored() {
    // The helper call precedes the construction of `С`, so its keys must NOT be folded in (the
    // forwarding fold is source-order aware); the later literal key still shows.
    let items = complete(
        r#"//- /test.bsl
Процедура Заполнить(П)
    П.Вставить("Призрак");
КонецПроцедуры

Процедура Тест()
    Заполнить(С);
    С = Новый Структура;
    С.Вставить("Реальный");
    С.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(has_label(&items, "Реальный"), "literal key after construction must show; got: {ls:?}");
    assert!(
        !has_label(&items, "Призрак"),
        "a helper call BEFORE construction must not contribute keys; got: {ls:?}"
    );
}

#[test]
fn completion_byref_param_reassigned_kills_later_inserts() {
    // `Ранний` is inserted while `П` still aliases the caller's argument, so it reaches `С`. After
    // `П = Новый Структура` the parameter is a fresh local, so `Поздний` must NOT reach the caller.
    let items = complete(
        r#"//- /test.bsl
Процедура Заполнить(П)
    П.Вставить("Ранний");
    П = Новый Структура;
    П.Вставить("Поздний");
КонецПроцедуры

Процедура Тест()
    С = Новый Структура;
    Заполнить(С);
    С.$0
КонецПроцедуры
"#,
    );

    let ls = labels(&items);
    assert!(
        has_label(&items, "Ранний"),
        "a key inserted before the param is reassigned must reach the caller; got: {ls:?}"
    );
    assert!(
        !has_label(&items, "Поздний"),
        "a key inserted after the by-ref param is reassigned must NOT reach the caller; got: {ls:?}"
    );
}

/// Ключи, которые кладёт менеджерный метод, доходят до `С.` — и поздняя запись в
/// имя коллекции их не отменяет: присваивание глобальному свойству локали не
/// создаёт, так что цепочка остаётся менеджерным вызовом.
#[test]
fn manager_keys_survive_a_later_write_to_the_collection_name() {
    let items = complete(
        r#"//- /Catalogs/Товары/Ext/ManagerModule.bsl
Процедура Заполнить(П) Экспорт
    П.Вставить("ИзМенеджера");
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    С = Новый Структура;
    Справочники.Товары.Заполнить(С);
    С.$0
    Справочники = Неопределено;
КонецПроцедуры
"#,
    );
    assert!(has_label(&items, "ИзМенеджера"), "{:?}", labels(&items));
}

/// Контроль: ОБЪЯВЛЕННАЯ локаль с именем коллекции цепочку действительно отменяет,
/// иначе проверка выше зелена сама по себе.
#[test]
fn a_declared_local_named_like_the_collection_does_cancel_the_chain() {
    let items = complete(
        r#"//- /Catalogs/Товары/Ext/ManagerModule.bsl
Процедура Заполнить(П) Экспорт
    П.Вставить("ИзМенеджера");
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Перем Справочники;
    С = Новый Структура;
    Справочники.Товары.Заполнить(С);
    С.$0
КонецПроцедуры
"#,
    );
    assert!(!has_label(&items, "ИзМенеджера"), "{:?}", labels(&items));
}

/// Запись в имя коллекции ДО цепочки её тоже не отменяет: присваивание глобальному
/// свойству локали не создаёт, и порядок этого не меняет. Прежний гейт спрашивал про
/// цели присваиваний и потому расходился с вердиктом инференса.
#[test]
fn manager_keys_survive_a_write_before_the_chain() {
    let items = complete(
        r#"//- /Catalogs/Товары/Ext/ManagerModule.bsl
Процедура Заполнить(П) Экспорт
    П.Вставить("ИзМенеджера");
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    С = Новый Структура;
    Справочники = Неопределено;
    Справочники.Товары.Заполнить(С);
    С.$0
КонецПроцедуры
"#,
    );
    assert!(has_label(&items, "ИзМенеджера"), "{:?}", labels(&items));
}

/// Владельца имени может дать и модуль, а не только тело метода: `Перем Справочники`
/// на уровне модуля делает корень переменной, и цепочка перестаёт быть менеджерным
/// вызовом. Гейт спрашивает тот же предикат, что и инференс, поэтому слои не
/// расходятся.
#[test]
fn a_module_variable_named_like_the_collection_cancels_the_chain() {
    let items = complete(
        r#"//- /Catalogs/Товары/Ext/ManagerModule.bsl
Процедура Заполнить(П) Экспорт
    П.Вставить("ИзМенеджера");
КонецПроцедуры

//- /test.bsl
Перем Справочники;

Процедура Тест()
    С = Новый Структура;
    Справочники.Товары.Заполнить(С);
    С.$0
КонецПроцедуры
"#,
    );
    assert!(!has_label(&items, "ИзМенеджера"), "{:?}", labels(&items));
}

/// Контроль к нему: без объявления та же фикстура ключ приносит.
#[test]
fn without_a_module_variable_the_chain_still_forwards() {
    let items = complete(
        r#"//- /Catalogs/Товары/Ext/ManagerModule.bsl
Процедура Заполнить(П) Экспорт
    П.Вставить("ИзМенеджера");
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    С = Новый Структура;
    Справочники.Товары.Заполнить(С);
    С.$0
КонецПроцедуры
"#,
    );
    assert!(has_label(&items, "ИзМенеджера"), "{:?}", labels(&items));
}
