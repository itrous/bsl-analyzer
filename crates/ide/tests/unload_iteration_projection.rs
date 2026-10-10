use hir::{Builders, DefDatabase, HirDatabase, ModuleId, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

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
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    let _ = db.module_bodies(ModuleId::new(test_file));
    (db, test_file)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn for_each_over_projected_value_table_yields_projected_row() {
    let fixture = r#"//- /test.bsl
Процедура Тест()
    Зап = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
    ТЗ = Зап.Выполнить().Выгрузить();
    Для Каждого Стр Из ТЗ Цикл
        Х = Стр;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let row_ty = var_ty(&db, file_id, "стр").expect("Стр must be inferred");
    let TypeKind::ValueTableRow(facet) = db.lookup_type(row_ty) else {
        panic!(
            "Стр must be Ty::ValueTableRow {{ projection: Some(..) }}, got {:?}",
            db.lookup_type(row_ty)
        );
    };
    let p = facet.projection.as_ref().expect("Стр must carry projection");
    assert_eq!(
        p.fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        vec!["Имя".to_string()],
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn projected_row_column_resolves_via_projection() {
    let fixture = r#"//- /test.bsl
Процедура Тест()
    Зап = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
    ТЗ = Зап.Выполнить().Выгрузить();
    Для Каждого Стр Из ТЗ Цикл
        Х = Стр.Имя;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let x_ty = var_ty(&db, file_id, "х").expect("Х must be inferred");
    assert!(
        x_ty == db.string(None, false),
        "Стр.Имя must resolve to Ty::String via projection — got {x_ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn helper_function_propagates_projection_through_unload_and_iteration() {
    let fixture = r#"//- /test.bsl
Функция ПолучитьТЗ()
    Зап = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
    Возврат Зап.Выполнить().Выгрузить();
КонецФункции

Процедура Тест()
    ТЗ = ПолучитьТЗ();
    Для Каждого Стр Из ТЗ Цикл
        Х = Стр.Имя;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let x_ty = var_ty(&db, file_id, "х").expect("Х must be inferred");
    assert!(
        x_ty == db.string(None, false),
        "helper-returned ТЗ's row.Имя must resolve via projection — got {x_ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn projection_less_value_table_keeps_platform_row() {
    let fixture = r#"//- /test.bsl
Процедура Тест()
    ТЗ = Новый ТаблицаЗначений;
    Для Каждого Стр Из ТЗ Цикл
        Х = Стр;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let row_ty = var_ty(&db, file_id, "стр").expect("Стр must be inferred");
    match db.lookup_type(row_ty) {
        TypeKind::PlatformObject(facet) if facet.name.as_str() == "СтрокаТаблицыЗначений" => {}
        TypeKind::ValueTableRow(facet) if facet.projection.is_none() => {}
        other => panic!(
            "non-projected ТЗ row must be platform СтрокаТаблицыЗначений (or its dedicated variant), got {other:?}",
        ),
    }
}
