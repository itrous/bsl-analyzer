use hir::{
    method_return_type_query, Builders, DefDatabase, MethodId, MethodIdInput, ModuleId, Name,
    TypeId, TypeKernelDb, TypeKind,
};
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
        db.set_file_text(*file_id, &file.content);
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn find_method(db: &RootDatabaseImpl, file_id: FileId, name: &str) -> MethodId {
    let symbol_tree = db.symbol_tree(ModuleId::new(file_id));
    symbol_tree
        .find_method(&Name::new(name))
        .unwrap_or_else(|| panic!("expected method `{name}` in {file_id:?}"))
        .id
}

fn return_ty_for(db: &RootDatabaseImpl, file_id: FileId, name: &str) -> TypeId {
    let mid = find_method(db, file_id, name);
    let input = MethodIdInput::new(db, mid);
    method_return_type_query(db, input)
}

#[test]
fn single_return_yields_inferred_ty() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция F()
    Возврат "hello";
КонецФункции
"#,
    );
    assert_eq!(return_ty_for(&db, fid, "F"), db.string(None, false));
}

#[test]
fn no_return_yields_unknown() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура P()
    Х = 1;
КонецПроцедуры
"#,
    );
    assert_eq!(return_ty_for(&db, fid, "P"), db.unknown());
}

#[test]
fn multiple_same_return_tys_unify() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция F(X)
    Если X Тогда
        Возврат "a";
    Иначе
        Возврат "b";
    КонецЕсли;
КонецФункции
"#,
    );
    assert_eq!(return_ty_for(&db, fid, "F"), db.string(None, false));
}

#[test]
fn mixed_return_tys_yield_union() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция F(X)
    Если X Тогда
        Возврат "a";
    Иначе
        Возврат 1;
    КонецЕсли;
КонецФункции
"#,
    );
    match db.lookup_type(return_ty_for(&db, fid, "F")) {
        TypeKind::Union(variants) => {
            assert_eq!(variants.len(), 2, "Union must have exactly String and Number");
            assert!(variants.contains(&db.string(None, false)));
            assert!(variants.contains(&db.number(None, None)));
        }
        other => panic!("expected Ty::Union, got {other:?}"),
    }
}

#[test]
fn bare_return_yields_unknown() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура P()
    Возврат;
КонецПроцедуры
"#,
    );
    assert_eq!(return_ty_for(&db, fid, "P"), db.unknown());
}

#[test]
fn nested_return_in_for_loop() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция F(Коллекция)
    Для Каждого Элемент Из Коллекция Цикл
        Возврат "hit";
    КонецЦикла;
    Возврат "miss";
КонецФункции
"#,
    );
    assert_eq!(return_ty_for(&db, fid, "F"), db.string(None, false));
}

#[test]
fn self_recursion_yields_unknown() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция M()
    Возврат M();
КонецФункции
"#,
    );
    assert_eq!(return_ty_for(&db, fid, "M"), db.unknown());
}

#[test]
fn return_type_caches_via_salsa() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция F()
    Возврат "cached";
КонецФункции
"#,
    );
    let mid = find_method(&db, fid, "F");
    let input = MethodIdInput::new(&db, mid);

    let first = method_return_type_query(&db, input);
    let second = method_return_type_query(&db, input);
    assert_eq!(first, second);
    assert_eq!(first, db.string(None, false));
}
