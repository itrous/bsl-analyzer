//! Members of a metadata collection of the configuration (`Метаданные.Справочники.<объект>`)
//! are checked against the configuration, the same way the manager collection
//! (`Справочники.<объект>`) already is.

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
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);

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
    // configuration root and anything below an existing object are not judged here.
    let found = unresolved_fields(
        r#"Процедура Тест(Описание)
    Есть = Метаданные.Справочники.Содержит(Описание);
    Найден = Метаданные.Справочники.Найти("Справочник1");
    Число = Метаданные.Документы.Количество();
    А = Метаданные.Справочники.Справочник1.Реквизиты.НетТакогоРеквизита;
    Б = Метаданные.Справочники.Справочник1.НетТакогоСвойства;
    В = Метаданные.ОсновнаяРоль;
    Г = Метаданные.ОбщиеМодули.НетТакогоМодуля;
    Д = Метаданные.ВнешниеИсточникиДанных.НетТакогоИсточника;
    Для Каждого Элемент Из Метаданные.Справочники Цикл
        Е = Элемент.НетТакогоСвойства;
    КонецЦикла;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "these shapes must stay silent: {found:?}");
}
