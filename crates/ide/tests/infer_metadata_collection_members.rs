//! Members of a metadata collection of the configuration (`Метаданные.Справочники.<объект>`)
//! are checked against the configuration, the same way the manager collection
//! (`Справочники.<объект>`) already is.

use hir::{Builders, HirDatabase, InferenceDiagnostic};
use ide::{Analysis, DiagnosticCode, DiagnosticsConfig};
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
fn objects_the_configuration_lacks_are_reported_in_an_overridable_module() {
    // The shape of a BSP `...Переопределяемый` module left in a trimmed configuration: the
    // list names objects of the full library, and the ones this configuration does not
    // have fail at run time with «Поле объекта не обнаружено». `Справочник1` and
    // `ПланВидовХарактеристик1` exist and stay silent.
    let found = unresolved_fields(
        r#"Процедура ПриОпределенииОбъектовСРедактируемымиРеквизитами(Объекты) Экспорт
    Объекты.Вставить(Метаданные.ПланыВидовХарактеристик.ДополнительныеРеквизитыИСведения.ПолноеИмя(), "РеквизитыРедактируемыеВГрупповойОбработке");
    Объекты.Вставить(Метаданные.ПланыВидовХарактеристик.ПланВидовХарактеристик1.ПолноеИмя(), "РеквизитыРедактируемыеВГрупповойОбработке");
    Объекты.Вставить(Метаданные.Справочники.Справочник1.ПолноеИмя(), "РеквизитыРедактируемыеВГрупповойОбработке");
    Объекты.Вставить(Метаданные.Справочники.ВариантыОтчетов.ПолноеИмя(), "РеквизитыРедактируемыеВГрупповойОбработке");
    Объекты.Вставить(Metadata.Catalogs.ГруппыДоступа.FullName(), "РеквизитыНеРедактируемыеВГрупповойОбработке");
КонецПроцедуры
"#,
    );
    assert_eq!(
        found,
        vec![
            "ДополнительныеРеквизитыИСведения".to_string(),
            "ВариантыОтчетов".to_string(),
            "ГруппыДоступа".to_string(),
        ]
    );
}

#[test]
fn collection_reached_through_a_local_keeps_its_kind() {
    let found = unresolved_fields(
        r#"Процедура Тест()
    Коллекция = Метаданные.Справочники;
    Есть = Коллекция.Справочник1;
    Нет = Коллекция.НетТакогоСправочника;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["НетТакогоСправочника".to_string()]);
}

#[test]
fn collection_methods_and_untrusted_shapes_stay_silent() {
    // The collection keeps the platform's methods; kinds without a manager, the
    // configuration root and the nested collections of an existing object
    // (`Реквизиты.<имя>` depends on the object, not on the platform) are not judged here.
    let found = unresolved_fields(
        r#"Процедура Тест(Описание)
    Есть = Метаданные.Справочники.Содержит(Описание);
    Найден = Метаданные.Справочники.Найти("Справочник1");
    Число = Метаданные.Документы.Количество();
    А = Метаданные.Справочники.Справочник1.Реквизиты.НетТакогоРеквизита;
    В = Метаданные.ОсновнаяРоль;
    Г = Метаданные.ОбщиеМодули.НетТакогоМодуля;
    Д = Метаданные.ВнешниеИсточникиДанных.НетТакогоИсточника;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "these shapes must stay silent: {found:?}");
}

#[test]
fn a_member_the_object_description_lacks_is_reported() {
    // An existing catalog's description is `ОбъектМетаданных: Справочник`, the same for
    // every catalog, so a property it lacks fails at run time with «Поле объекта не
    // обнаружено» - in a chain and through the loop element over the collection alike.
    let found = unresolved_fields(
        r#"Процедура Тест()
    Есть = Метаданные.Справочники.Справочник1.Реквизиты;
    Б = Метаданные.Справочники.Справочник1.НетТакогоСвойства;
    Для Каждого Элемент Из Метаданные.Справочники Цикл
        Имя = Элемент.Имя;
        Е = Элемент.НетТакогоСвойства;
    КонецЦикла;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["НетТакогоСвойства".to_string(), "НетТакогоСвойства".to_string()]);
}

fn unresolved_field_messages(code: &str, with_configuration: bool) -> Vec<String> {
    let fixture = format!("//- /test.bsl\n{code}");
    let (mut db, file_id) = setup(&fixture);
    if !with_configuration {
        db.set_all_config_paths(Vec::new(), None);
    }
    Analysis::from_database(db)
        .diagnostics(file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diag| diag.code == DiagnosticCode::UnresolvedField)
        .map(|diag| diag.message)
        .collect()
}

const EVERY_MANAGER_BACKED_KIND: &str = r#"Процедура Тест()
    А = Метаданные.Справочники.НетСправочника;
    Б = Метаданные.Документы.НетДокумента;
    В = Метаданные.РегистрыСведений.НетРегистра;
    Г = Метаданные.Перечисления.НетПеречисления;
    Д = Метаданные.Отчеты.НетОтчета;
    Е = Метаданные.ПланыВидовХарактеристик.НетПлана;
    Ж = Метаданные.ПланыОбмена.НетПланаОбмена;
    З = Метаданные.Константы.НетКонстанты;
    И = Метаданные.Справочники.Справочник1;
    К = Метаданные.Документы.Документ1;
    Л = Метаданные.РегистрыСведений.РегистрСведений1;
    М = Метаданные.Отчеты.ТестовыйОтчёт;
    Н = Метаданные.ПланыВидовХарактеристик.ПланВидовХарактеристик1;
    О = Метаданные.Константы.ПроизвольныйКонст;
КонецПроцедуры
"#;

#[test]
fn every_manager_backed_kind_reports_a_missing_object_by_name() {
    let expected: Vec<String> = [
        "НетСправочника",
        "НетДокумента",
        "НетРегистра",
        "НетПеречисления",
        "НетОтчета",
        "НетПлана",
        "НетПланаОбмена",
        "НетКонстанты",
    ]
    .iter()
    .map(|name| format!("Поле '{name}' не найдено у типа 'КоллекцияОбъектовМетаданных'"))
    .collect();
    assert_eq!(unresolved_field_messages(EVERY_MANAGER_BACKED_KIND, true), expected);
}

#[test]
fn the_same_misses_stay_silent_when_no_configuration_is_visible() {
    let found = unresolved_field_messages(EVERY_MANAGER_BACKED_KIND, false);
    assert!(found.is_empty(), "nothing to check a member against: {found:?}");
}

#[test]
fn the_collection_is_assigned_as_the_platform_collection_it_refines() {
    let fixture = format!(
        r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   П - Число - численный аргумент
Процедура ПринимаетЧисло(П) Экспорт
КонецПроцедуры

// Параметры:
//   П - КоллекцияОбъектовМетаданных - коллекция
Процедура ПринимаетКоллекцию(П) Экспорт
КонецПроцедуры

//- /test.bsl
{}"#,
        r#"Процедура Тест()
    ПервыйОбщийМодуль.ПринимаетЧисло(Метаданные.Справочники);
    ПервыйОбщийМодуль.ПринимаетКоллекцию(Метаданные.Справочники);
    ПервыйОбщийМодуль.ПринимаетКоллекцию(Метаданные.ОбщиеМодули);
КонецПроцедуры
"#
    );
    let (db, file_id) = setup(&fixture);
    // Inference diagnostics, not the rendered ones: a rendered mismatch whose two type labels
    // read the same is dropped, and both sides of the collection read `КоллекцияОбъектовМетаданных`.
    let expected: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .chain(db.arg_diagnostics(file_id).iter())
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::TypeMismatch { expected, .. } => Some(*expected),
            _ => None,
        })
        .collect();
    assert_eq!(expected, vec![db.number(None, None)], "only the number parameter is a mismatch");
}
