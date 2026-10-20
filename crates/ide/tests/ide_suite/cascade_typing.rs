use hir::{Builders, DefDatabase, DefWithBodyId, HirDatabase, ModuleId, Name};
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

fn var_ty(db: &RootDatabaseImpl, fid: FileId, name: &str) -> hir::TypeId {
    let result = db.infer(fid);
    let key = name.to_lowercase();
    result.var_types.get(&key).copied().unwrap_or_else(|| db.unknown())
}

#[test]
fn bare_fn_cascade_string() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция ЛокФн()
    Возврат "hello";
КонецФункции

Процедура P()
    Х = ЛокФн();
КонецПроцедуры
"#,
    );
    assert_eq!(var_ty(&db, fid, "Х"), db.string(None, false));
}

#[test]
fn bare_fn_cascade_two_step_chain() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция G()
    Возврат "x";
КонецФункции

Функция F()
    Возврат G();
КонецФункции

Процедура P()
    Х = F();
КонецПроцедуры
"#,
    );
    assert_eq!(var_ty(&db, fid, "Х"), db.string(None, false));
}

#[test]
fn bare_fn_cascade_reaches_the_method_past_a_parameter() {
    // `Foo(...)` looks among methods only: the parameter `Foo` does not take the call.
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция Foo()
    Возврат "module-method";
КонецФункции

Процедура P(Foo)
    Х = Foo();
КонецПроцедуры
"#,
    );
    assert_eq!(var_ty(&db, fid, "Х"), db.string(None, false));
}

#[test]
fn self_recursion_under_cascade_yields_unknown() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция F()
    Возврат F();
КонецФункции

Процедура P()
    Х = F();
КонецПроцедуры
"#,
    );
    assert_eq!(var_ty(&db, fid, "Х"), db.unknown());
}

#[test]
fn return_statement_propagates_cascade() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция Inner()
    Возврат "from-inner";
КонецФункции

Функция Wrap()
    Возврат Inner();
КонецФункции
"#,
    );

    let symbol_tree = db.symbol_tree(ModuleId::new(fid));
    let wrap_id = symbol_tree.find_method(&Name::new("Wrap")).expect("Wrap declared").id;
    let inner_id = symbol_tree.find_method(&Name::new("Inner")).expect("Inner declared").id;
    let wrap_owner = DefWithBodyId::Method(wrap_id.local_id);
    let inner_owner = DefWithBodyId::Method(inner_id.local_id);

    let result = db.infer(fid);
    let wrap_exprs =
        result.expr_types_by_body.get(&wrap_owner).expect("Wrap's body must have inferred exprs");
    let inner_exprs =
        result.expr_types_by_body.get(&inner_owner).expect("Inner's body must have inferred exprs");

    assert_ne!(wrap_owner, inner_owner, "Wrap and Inner must lower to distinct DefWithBodyId");
    let has_string = |tid: &hir::TypeId| *tid == db.string(None, false);
    assert!(inner_exprs.values().any(has_string));
    assert!(
        wrap_exprs.values().any(has_string),
        "Wrap's body must contain a Ty::String entry for `Inner()` via the O.11 cascade \
         (entries: {:?})",
        wrap_exprs.values().collect::<Vec<_>>(),
    );
}
