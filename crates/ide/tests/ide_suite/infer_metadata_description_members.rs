//! Members of the configuration's metadata descriptions (`Метаданные.Справочники.X.<член>`)
//! are checked against the platform's description of that kind.

use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::FileId;

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn unresolved_fields(code: &str) -> Vec<String> {
    let fixture = format!("//- /test.bsl\n{code}");
    let (db, file_id) = setup(&fixture);
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(field_name.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn missing_member_of_a_catalog_description_is_reported() {
    // The platform answers «Поле объекта не обнаружено (ПредопределенныеЗначения)» at run
    // time: the description of a catalog has `Предопределенные`, not this.
    let found = unresolved_fields(
        r#"Процедура Тест()
    Для Каждого Описание Из Метаданные.Справочники.Справочник1.ПредопределенныеЗначения Цикл
    КонецЦикла;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["ПредопределенныеЗначения".to_string()]);
}

#[test]
fn documented_members_of_descriptions_resolve() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    Описание = Метаданные.Справочники.Справочник1;
    Имя = Описание.Имя;
    Синоним = Описание.Синоним;
    Реквизиты = Описание.Реквизиты;
    Табличные = Описание.ТабличныеЧасти;
    Предопределенные = Описание.Предопределенные;
    Полное = Описание.ПолноеИмя();
    Attributes = Metadata.Catalogs.Справочник1.Attributes;
    Измерения = Метаданные.РегистрыСведений.РегистрСведений1.Измерения;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "documented members must resolve, got: {found:?}");
}

#[test]
fn description_reached_through_a_local_collection_is_checked() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    Коллекция = Метаданные.Справочники;
    Есть = Коллекция.Справочник1.Иерархический;
    Нет = Коллекция.Справочник1.НетТакогоСвойства;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["НетТакогоСвойства".to_string()]);
}

#[test]
fn description_reassigned_to_an_unknown_value_stays_soft() {
    // The second line gives `Описание` a value of unknown kind; judging `ТабличныеЧасти` by
    // the information register of the first line would accuse code valid on a catalog.
    let found = unresolved_fields(
        r#"Процедура Тест(Вид, Имя)
    Описание = Метаданные.РегистрыСведений.РегистрСведений1;
    Описание = Метаданные[Вид][Имя];
    Есть = Описание.ТабличныеЧасти;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "a description reached through a variable must stay soft: {found:?}");
}

#[test]
fn untrusted_metadata_shapes_stay_silent() {
    // Nested collections, generic descriptions, the configuration root and the kinds whose
    // platform member lists are not trusted as complete must not accuse anything.
    let found = unresolved_fields(
        r#"Процедура Тест()
    А = Метаданные.Справочники.Справочник1.Реквизиты.НетТакогоРеквизита;
    Б = Метаданные.НайтиПоПолномуИмени("Справочник.Справочник1").НетТакогоСвойства;
    В = Метаданные.ОсновнаяРоль;
    Г = Метаданные.ОбщиеМодули.НетТакогоМодуля;
    Д = Метаданные.Справочники.Справочник1.ОсновнаяФормаОбъекта.НетТакогоСвойства;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "untrusted metadata shapes must stay soft: {found:?}");
}

fn var_type(code: &str, var_lower: &str) -> Option<hir::TypeId> {
    let fixture = format!("//- /test.bsl\n{code}");
    let (db, file_id) = setup(&fixture);
    db.infer(file_id).var_types.get(var_lower).copied()
}

#[test]
fn an_unknown_value_resets_the_type_of_a_local_from_that_point() {
    let code = r#"Процедура Тест(Вид, Имя)
    Описание = Метаданные.Справочники.Справочник1;
    ДоСброса = Описание;
    Описание = Метаданные[Вид][Имя];
    ПослеСброса = Описание;
КонецПроцедуры
"#;
    assert!(var_type(code, "досброса").is_some(), "the description must type before the reset");
    assert_eq!(var_type(code, "послесброса"), None, "the read after the reset must be unknown");
}

#[test]
fn an_unknown_collection_resets_the_loop_variable() {
    let code = r#"Процедура Тест(Вид)
    Для Каждого Описание Из Метаданные.Справочники Цикл
        ВПервом = Описание;
    КонецЦикла;
    Для Каждого Описание Из Метаданные[Вид] Цикл
        ВоВтором = Описание;
    КонецЦикла;
КонецПроцедуры
"#;
    assert!(var_type(code, "впервом").is_some(), "a catalog collection types its element");
    assert_eq!(var_type(code, "вовтором"), None, "the second loop must not inherit the first");
}

#[test]
fn missing_member_through_a_local_is_reported_until_the_local_is_rebound() {
    let found = unresolved_fields(
        r#"Процедура Тест(Вид, Имя)
    Описание = Метаданные.Справочники.Справочник1;
    Нет = Описание.ПредопределенныеЗначения;
    Описание = Метаданные[Вид][Имя];
    Есть = Описание.ПредопределенныеЗначения;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["ПредопределенныеЗначения".to_string()]);
}

#[test]
fn missing_member_through_a_loop_variable_is_reported() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    Для Каждого Описание Из Метаданные.Справочники Цикл
        Есть = Описание.Иерархический;
        Нет = Описание.ПредопределенныеЗначения;
    КонецЦикла;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["ПредопределенныеЗначения".to_string()]);
}

#[test]
fn a_branch_holding_an_unknown_value_keeps_the_merge_soft() {
    // Whichever branch comes last textually, one path reaching the read holds a value of
    // unknown kind, so the member may exist.
    let found = unresolved_fields(
        r#"Процедура Тест(Условие, Вид, Имя)
    Если Условие Тогда
        Первое = Метаданные.Справочники.Справочник1;
    Иначе
        Первое = Метаданные[Вид][Имя];
    КонецЕсли;
    А = Первое.ПредопределенныеЗначения;
    Если Условие Тогда
        Второе = Метаданные[Вид][Имя];
    Иначе
        Второе = Метаданные.Справочники.Справочник1;
    КонецЕсли;
    Б = Второе.ПредопределенныеЗначения;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "a merge with an unknown path must stay soft: {found:?}");
}

#[test]
fn branches_of_the_same_kind_keep_the_merge_checked() {
    let found = unresolved_fields(
        r#"Процедура Тест(Условие)
    Если Условие Тогда
        Описание = Метаданные.Справочники.Справочник1;
    Иначе
        Описание = Метаданные.Справочники.СправочникСМенеджером;
    КонецЕсли;
    Нет = Описание.ПредопределенныеЗначения;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["ПредопределенныеЗначения".to_string()]);
}

#[test]
fn a_loop_back_edge_holding_an_unknown_value_keeps_the_read_soft() {
    // On the second iteration `Описание` holds whatever `Метаданные[Вид][Имя]` returned.
    let found = unresolved_fields(
        r#"Процедура Тест(Условие, Вид, Имя)
    Описание = Метаданные.Справочники.Справочник1;
    Пока Условие Цикл
        Есть = Описание.ПредопределенныеЗначения;
        Описание = Метаданные[Вид][Имя];
    КонецЦикла;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "a back-edge with an unknown value must stay soft: {found:?}");
}

#[test]
fn a_parameter_holding_a_description_stays_soft() {
    // A parameter's value comes from the caller; nothing proves its kind here.
    let found = unresolved_fields(
        r#"Процедура Тест(Описание)
    Есть = Описание.ПредопределенныеЗначения;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_loop_rebinds_its_variable_inside_and_vouches_for_nothing_after() {
    // Inside the loop the header has replaced whatever `Описание` held before it; after the
    // loop it holds the last element or, with no iteration, that earlier value.
    let found = unresolved_fields(
        r#"Процедура Тест(Вид, Имя)
    Описание = Метаданные[Вид][Имя];
    Для Каждого Описание Из Метаданные.Справочники Цикл
        Нет = Описание.ПредопределенныеЗначения;
    КонецЦикла;
    После = Описание.ПредопределенныеЗначения;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["ПредопределенныеЗначения".to_string()]);
}

#[test]
fn a_loop_variable_written_in_its_body_stays_soft() {
    // Reaching definitions do not see the header rebind the variable, so a write anywhere
    // in the body counts as reaching every read in it - wider than the platform, never
    // louder.
    let found = unresolved_fields(
        r#"Процедура Тест(Вид, Имя, Условие)
    Для Каждого Описание Из Метаданные.Справочники Цикл
        Есть = Описание.ПредопределенныеЗначения;
        Если Условие Тогда
            Описание = Метаданные[Вид][Имя];
        КонецЕсли;
    КонецЦикла;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_loop_over_a_local_collection_of_two_kinds_stays_soft() {
    // `Коллекция` holds catalogs on one path and documents on the other; the walk types it by
    // the documents written last, yet `Иерархический` is valid on the catalog path.
    let found = unresolved_fields(
        r#"Процедура Тест(Условие)
    Коллекция = Метаданные.Справочники;
    Если Условие Тогда
        Коллекция = Метаданные.Документы;
    КонецЕсли;
    Для Каждого Элемент Из Коллекция Цикл
        Иер = Элемент.Иерархический;
    КонецЦикла;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_chain_through_a_local_collection_of_two_shapes_stays_soft() {
    let found = unresolved_fields(
        r#"Процедура Тест(Условие)
    Если Условие Тогда
        Коллекция = Новый Структура("Справочник1", Новый Структура("Нет", 1));
    Иначе
        Коллекция = Метаданные.Справочники;
    КонецЕсли;
    Х = Коллекция.Справочник1.Нет;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_description_copied_from_a_local_of_two_shapes_stays_soft() {
    let found = unresolved_fields(
        r#"Процедура Тест(Условие)
    Если Условие Тогда
        Коллекция = Новый Структура("Справочник1", Новый Структура("Нет", 1));
    Иначе
        Коллекция = Метаданные.Справочники;
    КонецЕсли;
    Описание = Коллекция.Справочник1;
    Х = Описание.Нет;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_local_passed_by_reference_stays_soft() {
    // A parameter without `Знач` is passed by reference: the callee may have replaced the
    // description with one that has the member.
    let found = unresolved_fields(
        r#"Процедура Тест()
    Описание = Метаданные.Справочники.Справочник1;
    Заполнить(Описание);
    Изм = Описание.Измерения;
КонецПроцедуры

Процедура Заполнить(Парам)
    Парам = Метаданные.РегистрыСведений.РегистрСведений1;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_local_rewritten_by_execute_stays_soft() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    Описание = Метаданные.Справочники.Справочник1;
    Выполнить("Описание = Метаданные.РегистрыСведений.РегистрСведений1");
    Изм = Описание.Измерения;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_write_overwritten_later_in_a_try_body_reaches_the_code_after_it() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    Описание = Метаданные.РегистрыСведений.РегистрСведений1;
    Попытка
        Описание = Метаданные.Справочники.Справочник1;
        Х = 1 / 0;
        Описание = Метаданные.РегистрыСведений.РегистрСведений1;
    Исключение
    КонецПопытки;
    Есть = Описание.Иерархический;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_write_interrupted_in_a_try_body_reaches_the_handler() {
    // The handler is entered after the catalog write and before the register one.
    let found = unresolved_fields(
        r#"Процедура Тест()
    Описание = Метаданные.РегистрыСведений.РегистрСведений1;
    Попытка
        Описание = Метаданные.Справочники.Справочник1;
        Х = 1 / 0;
        Описание = Метаданные.РегистрыСведений.РегистрСведений1;
    Исключение
        Есть = Описание.Иерархический;
    КонецПопытки;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_write_inside_a_try_body_is_checked_within_it() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    Попытка
        Описание = Метаданные.Справочники.Справочник1;
        Нет = Описание.Измерения;
    Исключение
    КонецПопытки;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["Измерения".to_string()]);
}

#[test]
fn a_loop_over_a_local_collection_of_one_kind_is_checked() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    Коллекция = Метаданные.Справочники;
    Для Каждого Элемент Из Коллекция Цикл
        Есть = Элемент.Иерархический;
        Нет = Элемент.Измерения;
    КонецЦикла;
    Описание = Коллекция.Справочник1;
    ТожеНет = Описание.Измерения;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["Измерения".to_string(), "Измерения".to_string()]);
}

#[test]
fn a_local_copied_through_a_long_chain_of_branches_is_resolved_once_per_read() {
    // Every link has three definitions copying the previous local; following each path
    // separately would visit 3^12 of them.
    let mut code =
        String::from("Процедура Тест(Условие)\n    Л0 = Метаданные.Справочники.Справочник1;\n");
    for k in 1..=12 {
        let p = k - 1;
        code.push_str(&format!(
            "    Если Условие Тогда Л{k} = Л{p}; ИначеЕсли Условие Тогда Л{k} = Л{p}; \
             Иначе Л{k} = Л{p}; КонецЕсли;\n"
        ));
    }
    code.push_str("    Нет = Л12.Измерения;\nКонецПроцедуры\n");
    assert_eq!(unresolved_fields(&code), vec!["Измерения".to_string()]);
}
