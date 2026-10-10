use hir::{Builders, HirDatabase, TypeId};
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
    (db, test_file)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn for_each_over_map_yields_kluch_i_znachenie() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    М = Новый Соответствие;
    Для Каждого КЗ Из М Цикл
        Х = КЗ;
    КонецЦикла;
КонецПроцедуры
"#,
    );

    let ty = var_ty(&db, file_id, "кз");
    assert_eq!(
        ty,
        Some(db.platform_object("КлючИЗначение".to_string())),
        "loop var over Соответствие must be КлючИЗначение, got {:?}",
        ty
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn for_each_over_value_table_yields_row() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    Т = Новый ТаблицаЗначений;
    Для Каждого Стр Из Т Цикл
        Х = Стр;
    КонецЦикла;
КонецПроцедуры
"#,
    );

    let ty = var_ty(&db, file_id, "стр");
    assert_eq!(
        ty,
        Some(db.platform_object("СтрокаТаблицыЗначений".to_string())),
        "loop var over ТаблицаЗначений must be СтрокаТаблицыЗначений, got {:?}",
        ty
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn for_each_over_value_list_yields_list_item() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    С = Новый СписокЗначений;
    Для Каждого Эл Из С Цикл
        Х = Эл;
    КонецЦикла;
КонецПроцедуры
"#,
    );

    let ty = var_ty(&db, file_id, "эл");
    assert_eq!(
        ty,
        Some(db.platform_object("ЭлементСпискаЗначений".to_string())),
        "loop var over СписокЗначений must be ЭлементСпискаЗначений, got {:?}",
        ty
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn for_each_over_array_overwrites_prior_binding_with_any() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    А = Новый Массив;
    Эл = 1;
    Для Каждого Эл Из А Цикл
        Х = Эл;
    КонецЦикла;
КонецПроцедуры
"#,
    );

    assert_eq!(var_ty(&db, file_id, "эл"), Some(db.any()));
}

#[test]
fn for_each_over_string_leaves_var_types_empty() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    С = "abc";
    Для Каждого СимВ Из С Цикл
        Х = СимВ;
    КонецЦикла;
КонецПроцедуры
"#,
    );

    assert_eq!(var_ty(&db, file_id, "симв"), None);
}
