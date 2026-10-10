//! Member completion and hover on a structure whose fields are declared in the doc-comment.

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
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
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
    let cursor_in_file = (abs_idx - (header_end + 1)) as u32;
    (fixture_text.replacen("$0", "", 1), path_line.to_string(), cursor_in_file)
}

fn complete(fixture: &str) -> Vec<CompletionItem> {
    let (analysis, file_id, offset) = setup(fixture);
    analysis.completions(file_id, offset, None, ide::Locale::Ru)
}

fn labels(items: &[CompletionItem]) -> Vec<String> {
    items.iter().map(|item| item.label.clone()).collect()
}

const MODULE: &str = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   Структура:
//    * Имя - Строка - имя профиля.
//    * Количество - Число - количество попыток.
Функция Свойства() Экспорт
	Возврат Новый Структура;
КонецФункции
"#;

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn documented_structure_fields_complete_after_dot() {
    let items = complete(&format!(
        "{MODULE}\n\
//- /test.bsl\n\
Процедура Тест()\n\
	С = ПервыйОбщийМодуль.Свойства();\n\
	С.$0\n\
КонецПроцедуры\n"
    ));
    let labels = labels(&items);
    assert!(labels.iter().any(|l| l == "Имя"), "documented field Имя is missing: {labels:?}");
    assert!(
        labels.iter().any(|l| l == "Количество"),
        "documented field Количество is missing: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "Вставить"),
        "platform Структура methods must still be offered: {labels:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn undocumented_structure_offers_only_platform_methods() {
    let items = complete(
        "\n//- /test.bsl\n\
Процедура Тест()\n\
	С = Новый Структура;\n\
	С.$0\n\
КонецПроцедуры\n",
    );
    let labels = labels(&items);
    assert!(labels.iter().any(|l| l == "Вставить"), "platform methods missing: {labels:?}");
    assert!(!labels.iter().any(|l| l == "Имя"), "no field may appear from nowhere: {labels:?}");
}

#[test]
fn documented_structure_parameter_completes_inside_its_own_body() {
    // The materialised signature serves callers; inside the method the parameter is a binding with
    // no reaching write, so without the seed this offers nothing.
    let items = complete(
        "\n//- /test.bsl\n\
// Параметры:\n\
//   Параметры - Структура:\n\
//    * Адрес - Строка - адрес сервера.\n\
Процедура Обработать(Параметры) Экспорт\n\
\tПараметры.$0\n\
КонецПроцедуры\n",
    );
    let labels = labels(&items);
    assert!(labels.iter().any(|l| l == "Адрес"), "documented parameter field missing: {labels:?}");
}

#[test]
fn second_level_of_documented_nesting_is_offered() {
    // Each additional star documents one more structure level.
    let items = complete(
        "\n//- /test.bsl\n\
// Параметры:\n\
//   Параметры - Структура:\n\
//    * Адрес - Структура:\n\
//    ** Город - Строка - город.\n\
Процедура Обработать(Параметры) Экспорт\n\
\tПараметры.Адрес.$0\n\
КонецПроцедуры\n",
    );
    let labels = labels(&items);
    assert!(labels.iter().any(|l| l == "Город"), "second level is missing: {labels:?}");
}

#[test]
fn documented_structure_beside_undefined_completes_inside_its_own_body() {
    // `Неопределено, Структура:` is how an optional structure parameter is written in practice, and
    // the documented structure then sits in a union arm rather than alone.
    let items = complete(
        "\n//- /test.bsl\n\
// Параметры:\n\
//   Параметры - Неопределено, Структура:\n\
//    * Адрес - Строка - адрес сервера.\n\
Процедура Обработать(Параметры) Экспорт\n\
\tПараметры.$0\n\
КонецПроцедуры\n",
    );
    let labels = labels(&items);
    assert!(labels.iter().any(|l| l == "Адрес"), "documented field missing: {labels:?}");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn structure_of_a_value_type_completes_its_documented_fields() {
    // `Структура из <Тип>` names the type of the values, not of the structure: the fields still
    // come from the bullets, and the slot is still a structure.
    let items = complete(
        "\n//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl\n\
// Возвращаемое значение:\n\
//   Структура Из КлючИЗначение - таблицы отчёта:\n\
//    * Ключ - Строка - имя таблицы.\n\
//    * Значение - Число - число строк.\n\
Функция Данные() Экспорт\n\
\tВозврат Новый Структура;\n\
КонецФункции\n\
//- /test.bsl\n\
Процедура Тест()\n\
\tР = ПервыйОбщийМодуль.Данные();\n\
\tР.$0\n\
КонецПроцедуры\n",
    );
    let labels = labels(&items);
    assert!(labels.iter().any(|l| l == "Ключ"), "documented field Ключ missing: {labels:?}");
    assert!(
        labels.iter().any(|l| l == "Значение"),
        "documented field Значение missing: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "Вставить"),
        "platform Структура methods must still be offered: {labels:?}"
    );
}

#[test]
fn structure_of_a_value_type_completes_on_a_parameter() {
    // The return path loses the `из …` tail into the description while a parameter keeps it in the
    // type, so the two slots reach the documentation parser differently.
    let items = complete(
        "\n//- /test.bsl\n\
// Параметры:\n\
//   Данные - Структура из КлючИЗначение:\n\
//    * Ключ - Строка - имя таблицы.\n\
Процедура Обработать(Данные) Экспорт\n\
\tДанные.$0\n\
КонецПроцедуры\n",
    );
    let labels = labels(&items);
    assert!(labels.iter().any(|l| l == "Ключ"), "documented field missing: {labels:?}");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn fields_documented_under_an_array_stay_off_the_bare_structure_beside_it() {
    // Two untyped structures in one slot, one set of bullets: they describe the array element, and
    // the alternative that is just `Структура` must not inherit them.
    let items = complete(
        "\n//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl\n\
// Возвращаемое значение:\n\
//   - Структура - без детализации\n\
//   - Массив из Структура:\n\
//    * Поле - Строка - только у элемента массива.\n\
Функция Разное() Экспорт\n\
\tВозврат Новый Структура;\n\
КонецФункции\n\
//- /test.bsl\n\
Процедура Тест()\n\
\tР = ПервыйОбщийМодуль.Разное();\n\
\tР.$0\n\
КонецПроцедуры\n",
    );
    let on_slot = labels(&items);
    assert!(!on_slot.iter().any(|l| l == "Поле"), "field of the array element leaked: {on_slot:?}");

    // The absence above is only worth anything next to the place the field must appear: iterating
    // the same slot reaches the array element, and there `Поле` is documented.
    let element = complete(
        "\n//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl\n\
// Возвращаемое значение:\n\
//   - Структура - без детализации\n\
//   - Массив из Структура:\n\
//    * Поле - Строка - только у элемента массива.\n\
Функция Разное() Экспорт\n\
\tВозврат Новый Структура;\n\
КонецФункции\n\
//- /test.bsl\n\
Процедура Тест()\n\
\tР = ПервыйОбщийМодуль.Разное();\n\
\tДля Каждого Элемент Из Р Цикл\n\
\t\tЭлемент.$0\n\
\tКонецЦикла;\n\
КонецПроцедуры\n",
    );
    let on_element = labels(&element);
    assert!(on_element.iter().any(|l| l == "Поле"), "field missing on the element: {on_element:?}");
}

#[test]
fn a_structure_documented_without_fields_keeps_the_keys_the_body_proves() {
    // Documentation that only names `Структура` says less than the body does. Letting the name
    // stand alone would erase the keys the constructor proves, and documentation must add, never
    // remove.
    for documented in [
        "// Возвращаемое значение:\n//   Структура - параметры соединения.\n",
        "// Возвращаемое значение:\n//   Структура из КлючИЗначение - параметры.\n",
    ] {
        let labels = labels(&complete(&format!(
            "//- /test.bsl\n\
{documented}Функция ПараметрыСоединения()\n\
\tПар = Новый Структура(\"Таймаут, Адрес\");\n\
\tВозврат Пар;\n\
КонецФункции\n\
\n\
Процедура Тест()\n\
\tСтр = ПараметрыСоединения();\n\
\tСтр.$0\n\
КонецПроцедуры\n"
        )));
        assert!(labels.iter().any(|l| l == "Таймаут"), "key from the body lost: {labels:?}");
    }

    // The control: once the documentation does carry fields, they are the answer and the body's
    // keys give way — otherwise the assertion above would pass on a build that ignores docs.
    let documented = labels(&complete(
        "//- /test.bsl\n// Возвращаемое значение:\n//   Структура из КлючИЗначение:\n//    * Ключ - Строка - имя.\nФункция ПараметрыСоединения()\n\tПар = Новый Структура(\"Таймаут, Адрес\");\n\tВозврат Пар;\nКонецФункции\n\nПроцедура Тест()\n\tСтр = ПараметрыСоединения();\n\tСтр.$0\nКонецПроцедуры\n",
    ));
    assert!(documented.iter().any(|l| l == "Ключ"), "documented field missing: {documented:?}");
    assert!(!documented.iter().any(|l| l == "Таймаут"), "documentation must win: {documented:?}");
}

#[test]
fn the_collection_marker_is_read_in_either_language() {
    // Documentation mixes the languages (`Structure из …`, `Массив of …`). Recognising one pairing
    // in one parser and another pairing in the other is exactly the disagreement that silently
    // costs a slot its fields.
    let labels = labels(&complete(
        "\n//- /test.bsl\n\
// Параметры:\n\
//   Данные - Structure из КлючИЗначение:\n\
//    * Ключ - Строка - имя таблицы.\n\
Процедура Обработать(Данные) Экспорт\n\
\tДанные.$0\n\
КонецПроцедуры\n",
    ));
    assert!(labels.iter().any(|l| l == "Ключ"), "documented field missing: {labels:?}");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn an_optional_structure_documented_without_fields_keeps_the_keys_the_body_proves() {
    // `Неопределено, Структура` is how an optional result is written. The untyped structure then
    // stands in a union arm, and filling only a bare one leaves the common case documented into
    // silence.
    let body = "Функция ПараметрыСоединения()\n\
\tПар = Новый Структура(\"Таймаут, Адрес\");\n\
\tВозврат Пар;\n\
КонецФункции\n\
\n\
Процедура Тест()\n\
\tСтр = ПараметрыСоединения();\n\
\tСтр.$0\n\
КонецПроцедуры\n";

    let optional = labels(&complete(&format!(
        "//- /test.bsl\n// Возвращаемое значение:\n//   Неопределено, Структура - параметры соединения.\n{body}"
    )));
    assert!(optional.iter().any(|l| l == "Таймаут"), "key from the body lost: {optional:?}");

    // The control that keeps the assertion above honest: documentation that declares a collection
    // is a statement about the shape, and the body's structure must not overwrite it.
    let collection = labels(&complete(&format!(
        "//- /test.bsl\n// Возвращаемое значение:\n//   Массив из Структура - строки таблицы.\n{body}"
    )));
    assert!(
        collection.iter().any(|l| l == "Добавить"),
        "the documented Массив must stand: {collection:?}"
    );
    assert!(
        !collection.iter().any(|l| l == "Таймаут"),
        "keys leaked past the array: {collection:?}"
    );
}

#[test]
fn an_optional_body_still_proves_its_keys() {
    // A method that answers `Неопределено` on one path and a structure on another has both in one
    // inferred type. The keys live on the structure alone, and documenting the result must not cost
    // the caller everything the body proved.
    let body = "Функция ПараметрыСоединения(Флаг)\n\
\tЕсли Флаг Тогда\n\
\t\tВозврат Неопределено;\n\
\tКонецЕсли;\n\
\tПар = Новый Структура(\"Таймаут, Адрес\");\n\
\tВозврат Пар;\n\
КонецФункции\n\
\n\
Процедура Тест()\n\
\tСтр = ПараметрыСоединения(Ложь);\n\
\tСтр.$0\n\
КонецПроцедуры\n";

    for documented in [
        "// Возвращаемое значение:\n//   Неопределено, Структура - параметры соединения.\n",
        "// Возвращаемое значение:\n//   Структура - параметры соединения.\n",
    ] {
        let labels = labels(&complete(&format!("//- /test.bsl\n{documented}{body}")));
        assert!(labels.iter().any(|l| l == "Таймаут"), "key from the body lost: {labels:?}");
    }

    // The control: the same body without documentation is where the keys come from, so the
    // assertions above cannot pass on a build that has lost them altogether.
    let undocumented = labels(&complete(&format!("//- /test.bsl\n{body}")));
    assert!(undocumented.iter().any(|l| l == "Таймаут"), "body keys missing: {undocumented:?}");
}
