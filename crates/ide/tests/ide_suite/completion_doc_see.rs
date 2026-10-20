//! Member completion where the slot's type is documented by pointing at another method.
//!
//! Fields live in the TARGET's doc-comment, never in its body: keys a body proves already reach a
//! caller through the ordinary signature, so a fixture resting on them would pass on a build where
//! none of this exists.

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

fn labels(fixture: &str) -> Vec<String> {
    let (analysis, file_id, offset) = setup(fixture);
    let items: Vec<CompletionItem> = analysis.completions(file_id, offset, None, ide::Locale::Ru);
    items.iter().map(|item| item.label.clone()).collect()
}

fn assert_offers(fixture: &str, expected: &[&str]) {
    let labels = labels(fixture);
    for field in expected {
        assert!(labels.contains(&(*field).to_string()), "ожидалось поле {field}, есть: {labels:?}");
    }
}

fn assert_does_not_offer(fixture: &str, unexpected: &[&str]) {
    let labels = labels(fixture);
    for field in unexpected {
        assert!(!labels.contains(&(*field).to_string()), "поле {field} предлагаться не должно");
    }
}

const TARGET: &str = r#"
//- /CommonModules/Настройки/Ext/Module.bsl
// Возвращаемое значение:
//   Структура:
//    * Таймаут - Число - секунды ожидания.
//    * Адрес - Строка - куда обращаться.
Функция Соединение() Экспорт
	Возврат Новый Структура;
КонецФункции

// Параметры:
//   Параметры - Структура:
//    * Режим - Строка - режим работы.
Процедура Применить(Параметры) Экспорт
КонецПроцедуры

//- /CommonModules/Обёртка/Ext/Module.bsl
// Возвращаемое значение:
//   см. Настройки.Соединение
Функция Соединение() Экспорт
	Возврат Неопределено;
КонецФункции
"#;

#[test]
fn a_result_documented_by_reference_completes_the_targets_fields() {
    // The reference must stand in the doc-comment of the method BEING CALLED. Calling a method
    // that documents its fields inline exercises the existing inline path and would pass with this
    // feature removed entirely.
    let fixture = format!(
        "{TARGET}
//- /test.bsl
Процедура Тест()
	Соединение = Обёртка.Соединение();
	Соединение.$0
КонецПроцедуры
"
    );

    assert_offers(&fixture, &["Таймаут", "Адрес"]);
}

#[test]
fn a_parameter_documented_by_reference_is_typed_inside_the_body_that_receives_it() {
    // The materialised signature serves calls from outside; inside the body a parameter is a
    // binding with no reaching write, so without seeding the fields are offered at every call site
    // and nowhere in the method that actually uses them.
    let fixture = format!(
        "{TARGET}
//- /test.bsl
// Параметры:
//   Соединение - см. Настройки.Соединение
Процедура Тест(Соединение) Экспорт
	Соединение.$0
КонецПроцедуры
"
    );

    assert_offers(&fixture, &["Таймаут", "Адрес"]);
}

#[test]
fn the_third_segment_types_a_slot_from_the_targets_parameter() {
    let fixture = format!(
        "{TARGET}
//- /test.bsl
// Параметры:
//   Параметры - см. Настройки.Применить.Параметры
Процедура Тест(Параметры) Экспорт
	Параметры.$0
КонецПроцедуры
"
    );

    assert_offers(&fixture, &["Режим"]);
}

#[test]
fn an_element_of_an_array_documented_by_reference_completes_too() {
    let fixture = format!(
        "{TARGET}
//- /CommonModules/Список/Ext/Module.bsl
// Возвращаемое значение:
//   Массив из см. Настройки.Соединение
Функция Все() Экспорт
	Возврат Новый Массив;
КонецФункции

//- /test.bsl
Процедура Тест()
	Все = Список.Все();
	Для Каждого Элемент Из Все Цикл
		Элемент.$0
	КонецЦикла;
КонецПроцедуры
"
    );

    assert_offers(&fixture, &["Таймаут", "Адрес"]);
}

#[test]
fn a_reference_that_resolves_to_nothing_offers_nothing_of_its_own() {
    // The permissive type has to stay permissive: an unresolvable reference must not turn into a
    // narrow type that starts refusing things.
    let fixture = format!(
        "{TARGET}
//- /test.bsl
Процедура Тест()
	Значение = Обёртка.Соединение();
	Значение.$0
КонецПроцедуры
"
    );
    // Control: the resolvable reference does offer them, so the negative below is about resolution
    // failing rather than about completion being broken.
    assert_offers(&fixture, &["Таймаут"]);

    let prose = format!(
        "{TARGET}
//- /CommonModules/Проза/Ext/Module.bsl
// Возвращаемое значение:
//   см. в описании выше
Функция Значение() Экспорт
	Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
	Значение = Проза.Значение();
	Значение.$0
КонецПроцедуры
"
    );

    assert_does_not_offer(&prose, &["Таймаут", "Адрес"]);
}

#[test]
fn a_target_whose_keys_only_its_body_proves_gives_the_slot_nothing() {
    // The boundary of this feature, stated as a test rather than as a promise: resolving a
    // reference reads documentation and only documentation.
    let fixture = r#"
//- /CommonModules/Телом/Ext/Module.bsl
// Возвращаемое значение:
//   Структура
Функция Соединение() Экспорт
	Результат = Новый Структура;
	Результат.Вставить("ТолькоИзТела", 1);
	Возврат Результат;
КонецФункции

//- /test.bsl
// Параметры:
//   Соединение - см. Телом.Соединение
Процедура Тест(Соединение) Экспорт
	Соединение.$0
КонецПроцедуры
"#;

    assert_does_not_offer(fixture, &["ТолькоИзТела"]);
}

#[test]
fn a_reference_does_not_take_away_the_keys_a_body_already_proved() {
    let fixture = r#"
//- /CommonModules/Цель/Ext/Module.bsl
// Возвращаемое значение:
//   Структура:
//    * Таймаут - Число - секунды.
Функция Создать() Экспорт
	Возврат Новый Структура;
КонецФункции

//- /CommonModules/Обёртка/Ext/Module.bsl
// Возвращаемое значение: см. Цель.Создать
Функция Получить() Экспорт
	Результат = Новый Структура;
	Результат.Вставить("ИзТела", 1);
	Возврат Результат;
КонецФункции

//- /test.bsl
Процедура Тест()
	Значение = Обёртка.Получить();
	Значение.$0
КонецПроцедуры
"#;
    // Both halves matter: the reference must not cost the caller what it already had, and the
    // fixture must still be one where the reference resolves — otherwise the assertion is about
    // nothing. The two parsers disagree on a one-line section header, so here the signature falls
    // back to the body and tier 2 must stand aside.
    assert_offers(fixture, &["ИзТела"]);
    assert_does_not_offer(fixture, &["Таймаут"]);
}

#[test]
fn a_reference_inside_a_returned_structures_field_resolves_too() {
    // The guard that keeps body-proven keys must test where those keys came from, not whether any
    // exist: a return documented `Структура:` with a field `* Поле - см. X.Y` also arrives at the
    // merge carrying fields, and it is precisely the slot the resolved answer improves.
    let fixture = format!(
        "{TARGET}
//- /CommonModules/Хост/Ext/Module.bsl
// Возвращаемое значение:
//   Структура:
//    * Соединение - см. Настройки.Соединение - параметры.
Функция Получить() Экспорт
	Возврат Новый Структура;
КонецФункции

//- /test.bsl
Процедура Тест()
	Р = Хост.Получить();
	Р.Соединение.$0
КонецПроцедуры
"
    );
    assert_offers(&fixture, &["Таймаут", "Адрес"]);
}

#[test]
fn an_arm_the_rebuild_cannot_read_does_not_cost_the_body_its_fields() {
    // What the slot offers inside the body must not depend on whether some OTHER arm of the same
    // slot happens to be a form the rebuilding parser reads.
    let fixture = r#"
//- /CommonModules/База/Ext/Module.bsl
// Возвращаемое значение:
//   Структура:
//    * Таймаут - Число - секунды.
Функция Создать() Экспорт
	Возврат Новый Структура;
КонецФункции

//- /test.bsl
// Параметры:
//   Данные - Соответствие из Строка
//          - Структура:
//    * Ключ - см. База.Создать - вложенная.
Процедура Читает(Данные) Экспорт
	Данные.$0
КонецПроцедуры
"#;
    assert_offers(fixture, &["Ключ"]);
}

/// The metadata object a common module needs to exist for the configuration; the fixture builder
/// writes bodies and `Configuration.xml`, not these.
fn common_module_xml(name: &str, index: usize) -> String {
    format!(
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-{:012}">
        <Properties>
            <Name>{name}</Name>
            <Global>false</Global>
            <Server>true</Server>
        </Properties>
    </CommonModule>
</MetaDataObject>"#,
        index + 1
    )
}

/// A bare name exported by more than one host resolves through a branch of its own, which returns
/// a union before any call candidate is built. A slot documented by reference has to be filled
/// there too, or the same documentation works for a name owned by one host and not for a name
/// owned by two.
#[test]
fn a_bare_name_owned_by_two_hosts_still_gets_what_its_documentation_points_at() {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use vfs::VfsPath;

    const DOCUMENTED: &str = "// Возвращаемое значение:\n//   см. База.Создать\nФункция ОбщаяФункция() Экспорт\n\tВозврат Неопределено;\nКонецФункции\n";
    const TARGET: &str = "// Возвращаемое значение:\n//   Структура:\n//    * Таймаут - Число - секунды.\nФункция Создать() Экспорт\n\tВозврат Новый Структура;\nКонецФункции\n";
    const CALLER: &str =
        "Процедура Тест() Экспорт\n\tЗначение = ОбщаяФункция();\n\tЗначение.\nКонецПроцедуры\n";

    // Application modules on purpose: a name is owned by more than one host only when every owner
    // is one, and two common modules would collapse into a single variant.
    let mut builder = test_fixture::CfeFixtureBuilder::new("");
    builder.add_base_module("База", TARGET);
    builder.add_base_module("Вызов", CALLER);
    let fixture = builder.build();

    let mut bodies = Vec::new();
    for (index, module) in fixture.base_modules().iter().enumerate() {
        std::fs::write(
            fixture.root().join(format!("CommonModules/{}.xml", module.name())),
            common_module_xml(module.name(), index),
        )
        .unwrap();
        let path = fixture.root().join(format!("CommonModules/{}/Ext/Module.bsl", module.name()));
        bodies.push((path, module.source().to_string()));
    }
    for kind in hir::ApplicationModuleKind::ALL {
        let path = fixture.root().join(kind.relative_path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, DOCUMENTED).unwrap();
        bodies.push((path, DOCUMENTED.to_string()));
    }

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(fixture.config_paths(), Some(bsl_metadata::ExtensionScope::InfoBase));
    let mut file_set = FileSet::default();
    for (index, (path, _)) in bodies.iter().enumerate() {
        file_set.insert(FileId(index as u32), VfsPath::new(path.to_string_lossy().into_owned()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    let mut caller = FileId(0);
    for (index, (path, body)) in bodies.iter().enumerate() {
        let file_id = FileId(index as u32);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, body);
        if path.ends_with("CommonModules/Вызов/Ext/Module.bsl") {
            caller = file_id;
        }
    }

    let offset = CALLER.find("Значение.").unwrap() + "Значение.".len();
    let analysis = Analysis::from_database(db);
    let labels: Vec<String> = analysis
        .completions(caller, offset as u32, None, ide::Locale::Ru)
        .iter()
        .map(|item| item.label.clone())
        .collect();

    assert!(labels.contains(&"Таймаут".to_string()), "есть: {labels:?}");
}
