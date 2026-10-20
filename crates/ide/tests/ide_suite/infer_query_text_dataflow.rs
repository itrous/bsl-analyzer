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

fn query_no_projection(db: &RootDatabaseImpl, ty: TypeId) -> bool {
    match db.lookup_type(ty) {
        TypeKind::Query { projections } => projections.iter().all(Option::is_none),
        _ => false,
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn straight_line_text_assign_refines_projection_through_execute_select() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос;
    Зап.Текст = "ВЫБРАТЬ ""abc"" КАК Имя";
    Х = Зап.Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "single literal write to Зап.Текст must let .Выбрать().Имя resolve to Ty::String",
    );
}

#[test]
fn text_append_idiom_collapses_refinement_to_none() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос;
    Зап.Текст = "ВЫБРАТЬ ""abc"" КАК Имя";
    Зап.Текст = Зап.Текст + " ИЗ Справочник.Товары";
    Х = Зап.Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х");
    assert!(
        ty.is_none_or(|t| t != db.string(None, false)),
        "append idiom must not propagate the first literal's projection — got {ty:?}",
    );
}

#[test]
fn divergent_branch_literals_collapse_refinement_to_none() {
    let fixture = r#"//- /test.bsl
Функция Тест(Флаг)
    Зап = Новый Запрос;
    Если Флаг Тогда
        Зап.Текст = "ВЫБРАТЬ ""abc"" КАК Имя";
    Иначе
        Зап.Текст = "ВЫБРАТЬ 42 КАК Цена";
    КонецЕсли;
    Х = Зап.Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х");
    assert!(
        ty.is_none_or(|t| t != db.string(None, false)),
        "divergent-branch literals must not pick one — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn unrelated_field_writes_do_not_block_refinement() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос;
    Зап.Текст = "ВЫБРАТЬ ""abc"" КАК Имя";
    Зап.Параметры.Вставить("Foo", "Bar");
    Х = Зап.Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "intervening .Параметры call must not block .Текст refinement",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn no_text_assignment_keeps_receiver_unrefined() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос;
    Выборка = Зап.Выполнить().Выбрать();
    Возврат Выборка;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "выборка").expect("выборка must be inferred");
    let projections = match db.lookup_type(ty) {
        TypeKind::QueryResultSelection(facet) => facet.projection.clone(),
        other => panic!("expected Ty::QueryResultSelection, got {other:?}"),
    };
    assert!(
        projections.is_none(),
        "no Зап.Текст write reaches the dispatch — selection must carry no projection",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn loop_carried_text_write_collapses_refinement_to_none() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Зап = Новый Запрос;
    Для i = 1 По 3 Цикл
        Зап.Текст = "ВЫБРАТЬ ""abc"" КАК Имя";
    КонецЦикла;
    Х = Зап.Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    let _ = ty;
    let chain_ty = var_ty(&db, file_id, "х");
    assert!(
        chain_ty.is_some(),
        "loop-body Зап.Текст assignment must not panic the refinement helper",
    );
}

#[test]
fn unbound_receiver_keeps_chain_unrefined() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Х = Зап.Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х");
    assert!(
        ty.is_none_or(|t| t != db.string(None, false)),
        "unbound `Зап` receiver must not produce a Ty::String chain — got {ty:?}",
    );
    let _ = query_no_projection;
}
