use hir::{HirDatabase, MetadataKind, TypeId, TypeKernelDb, TypeKind};
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

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

fn assert_metadata_ref(
    db: &RootDatabaseImpl,
    actual: Option<TypeId>,
    kind: MetadataKind,
    name: &str,
) {
    let actual = actual.expect("expected metadata ref type");
    assert!(
        matches!(
            db.lookup_type(actual),
            TypeKind::MetadataRef(facet)
                if facet.kind == kind && facet.name.as_str() == name
        ),
        "expected MetadataRef({kind:?}, {name}), got {:?}",
        db.lookup_type(actual)
    );
}

#[test]
fn infer_register_dimension_resolves_to_catalog_ref() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   РегистрСведенийКлючЗаписи.РегистрСведений1
Функция Ключ() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    К = ПервыйОбщийМодуль.Ключ();
    С = К.Справочник1;
    Возврат С;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_metadata_ref(&db, var_ty(&db, file_id, "с"), MetadataKind::CatalogRef, "Справочник1");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn infer_for_each_over_record_set_yields_record_kind() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   РегистрСведенийНаборЗаписей.РегистрСведений1
Функция Набор() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Н = ПервыйОбщийМодуль.Набор();
    Для Каждого Запись Из Н Цикл
        Х = Запись;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert_metadata_ref(
        &db,
        var_ty(&db, file_id, "запись"),
        MetadataKind::InformationRegisterRecord,
        "РегистрСведений1",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn infer_register_record_dimension_resolves_through_field_lookup() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   РегистрСведенийНаборЗаписей.РегистрСведений1
Функция Набор() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Н = ПервыйОбщийМодуль.Набор();
    Для Каждого Запись Из Н Цикл
        С = Запись.Справочник1;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert_metadata_ref(&db, var_ty(&db, file_id, "с"), MetadataKind::CatalogRef, "Справочник1");
}

#[test]
fn infer_record_set_dimension_regression() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   РегистрСведенийНаборЗаписей.РегистрСведений1
Функция Набор() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    Н = ПервыйОбщийМодуль.Набор();
    С = Н.Справочник1;
    Возврат С;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_metadata_ref(&db, var_ty(&db, file_id, "с"), MetadataKind::CatalogRef, "Справочник1");
}

#[test]
fn infer_register_missing_field_stays_unknown() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   РегистрСведенийКлючЗаписи.РегистрСведений1
Функция Ключ() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    К = ПервыйОбщийМодуль.Ключ();
    Х = К.НесуществующееПоле;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        None,
        "missing register part must stay Unknown (no var_types entry)",
    );
}
