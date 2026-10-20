use hir::{DefDatabase, HirDatabase, ModuleId, TypeId, TypeKernelDb, TypeKind};
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

fn is_value_table(db: &RootDatabaseImpl, ty: TypeId) -> bool {
    matches!(db.lookup_type(ty), TypeKind::ValueTable(_))
        || matches!(db.lookup_type(ty), TypeKind::PlatformObject(facet) if facet.name.as_str().eq_ignore_ascii_case("ТаблицаЗначений"))
}

fn is_value_tree(db: &RootDatabaseImpl, ty: TypeId) -> bool {
    matches!(db.lookup_type(ty), TypeKind::PlatformObject(facet) if facet.name.as_str().eq_ignore_ascii_case("ДеревоЗначений"))
}

fn union_has(
    db: &RootDatabaseImpl,
    ty: TypeId,
    predicate: impl Fn(&RootDatabaseImpl, TypeId) -> bool,
) -> bool {
    matches!(db.lookup_type(ty), TypeKind::Union(members) if members.iter().any(|member| predicate(db, *member)))
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn no_arg_narrows_to_value_table() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос("ВЫБРАТЬ 1 КАК Колонка");
    ТЗ = Зап.Выполнить().Выгрузить();
    Возврат ТЗ;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "тз").expect("ТЗ must be inferred");
    assert!(
        is_value_table(&db, ty),
        "no-arg .Выгрузить() must narrow to ТаблицаЗначений — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn direct_iteration_narrows_to_value_table() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос("ВЫБРАТЬ 1 КАК Колонка");
    ТЗ = Зап.Выполнить().Выгрузить(ОбходРезультатаЗапроса.Прямой);
    Возврат ТЗ;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "тз").expect("ТЗ must be inferred");
    assert!(
        is_value_table(&db, ty),
        ".Выгрузить(ОбходРезультатаЗапроса.Прямой) must narrow to ТаблицаЗначений — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn by_groups_narrows_to_value_tree() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос("ВЫБРАТЬ 1 КАК Колонка");
    Результат = Зап.Выполнить().Выгрузить(ОбходРезультатаЗапроса.ПоГруппировкам);
    Возврат Результат;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "результат").expect("Результат must be inferred");
    assert!(
        is_value_tree(&db, ty),
        ".Выгрузить(ОбходРезультатаЗапроса.ПоГруппировкам) must narrow to ДеревоЗначений — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn by_groups_with_hierarchy_narrows_to_value_tree() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос("ВЫБРАТЬ 1 КАК Колонка");
    Результат = Зап.Выполнить().Выгрузить(ОбходРезультатаЗапроса.ПоГруппировкамСИерархией);
    Возврат Результат;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "результат").expect("Результат must be inferred");
    assert!(
        is_value_tree(&db, ty),
        ".Выгрузить(ОбходРезультатаЗапроса.ПоГруппировкамСИерархией) must narrow to ДеревоЗначений — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn english_linear_narrows_to_value_table() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос("ВЫБРАТЬ 1 КАК Колонка");
    ТЗ = Зап.Выполнить().Выгрузить(QueryResultIteration.Linear);
    Возврат ТЗ;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "тз").expect("ТЗ must be inferred");
    assert!(
        is_value_table(&db, ty),
        "English QueryResultIteration.Linear must narrow to ТаблицаЗначений — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn english_by_groups_with_hierarchy_narrows_to_value_tree() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос("ВЫБРАТЬ 1 КАК Колонка");
    Результат = Зап.Выполнить().Выгрузить(QueryResultIteration.ByGroupsWithHierarchy);
    Возврат Результат;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "результат").expect("Результат must be inferred");
    assert!(
        is_value_tree(&db, ty),
        "QueryResultIteration.ByGroupsWithHierarchy must narrow to ДеревоЗначений — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn dynamic_arg_keeps_union() {
    let fixture = r#"//- /test.bsl
Функция Тест(ТипОбхода)
    Зап = Новый Запрос("ВЫБРАТЬ 1 КАК Колонка");
    Результат = Зап.Выполнить().Выгрузить(ТипОбхода);
    Возврат Результат;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "результат").expect("Результат must be inferred");
    assert!(
        union_has(&db, ty, is_value_table) && union_has(&db, ty, is_value_tree),
        "dynamic arg must preserve union — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn projection_carries_into_narrowed_value_table() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
    ТЗ = Зап.Выполнить().Выгрузить();
    Возврат ТЗ;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "тз").expect("ТЗ must be inferred");
    match db.lookup_type(ty) {
        TypeKind::ValueTable(facet) if facet.projection.is_some() => {
            let p = facet.projection.as_ref().expect("checked above");
            assert_eq!(
                p.fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
                vec!["Имя".to_string()],
                "projection must carry the single SELECT alias",
            );
        }
        other => panic!("expected Ty::ValueTable {{ projection: Some(..) }}, got {other:?}"),
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn projection_carries_through_direct_iteration_arg() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос("ВЫБРАТЬ ""x"" КАК Поле1, 1 КАК Поле2");
    ТЗ = Зап.Выполнить().Выгрузить(ОбходРезультатаЗапроса.Прямой);
    Возврат ТЗ;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "тз").expect("ТЗ must be inferred");
    let TypeKind::ValueTable(facet) = db.lookup_type(ty) else {
        panic!("expected Ty::ValueTable {{ projection: Some(..) }}, got {:?}", db.lookup_type(ty));
    };
    let p = facet.projection.as_ref().expect("ValueTable must carry projection");
    let names: Vec<_> = p.fields.iter().map(|f| f.name.clone()).collect();
    assert_eq!(names, vec!["Поле1".to_string(), "Поле2".to_string()]);
}

#[test]
fn tabular_section_unload_unaffected() {
    let fixture = r#"//- /test.bsl
Функция Тест(Документ)
    ТЗ = Документ.Товары.Выгрузить();
    Возврат ТЗ;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let _ = var_ty(&db, file_id, "тз");
}
