use hir::{DefDatabase, MethodIdInput, ModuleId, Name};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::sync::Arc;
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

#[test]
fn method_body_matches_module_bodies_slice() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура Foo()
    А = 1;
    Б = 2;
КонецПроцедуры
"#,
    );

    let module_id = ModuleId::new(fid);
    let symbol_tree = db.symbol_tree(module_id);
    let mid = symbol_tree.find_method(&Name::new("Foo")).expect("Foo declared").id;
    let method_input = MethodIdInput::new(&db, mid);

    let body = db.method_body(method_input);
    let module_bodies = db.module_bodies(module_id);
    let whole = module_bodies.body(mid.local_id).expect("module_bodies has Foo");

    assert_eq!(
        body.stmt_count(),
        whole.stmt_count(),
        "per-method lowering must match module_bodies slice"
    );
    assert!(body.stmt_count() >= 2, "two assignments must produce at least two stmts");
}

#[test]
fn method_body_handles_bilingual_decl() {
    for (src, name) in [
        ("Процедура Тест()\n    А = 1;\nКонецПроцедуры", "Тест"),
        ("Procedure Test()\n    A = 1;\nEndProcedure", "Test"),
        ("Функция Ф()\n    Возврат 1;\nКонецФункции", "Ф"),
        ("Function F()\n    Return 1;\nEndFunction", "F"),
    ] {
        let fixture = format!("//- /test.bsl\n{src}\n");
        let (db, fid) = setup(&fixture);
        let module_id = ModuleId::new(fid);
        let symbol_tree = db.symbol_tree(module_id);
        let mid = symbol_tree
            .find_method(&Name::new(name))
            .unwrap_or_else(|| panic!("method `{name}` must lower"))
            .id;
        let method_input = MethodIdInput::new(&db, mid);

        let body = db.method_body(method_input);
        assert!(body.stmt_count() > 0, "bilingual method must lower (source: {src})");
    }
}

#[test]
fn method_body_caches_via_salsa() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    А = 1;
КонецПроцедуры
"#,
    );

    let module_id = ModuleId::new(fid);
    let symbol_tree = db.symbol_tree(module_id);
    let mid = symbol_tree.find_method(&Name::new("Тест")).expect("Тест declared").id;
    let method_input = MethodIdInput::new(&db, mid);

    let first = db.method_body(method_input);
    let second = db.method_body(method_input);
    assert!(
        Arc::ptr_eq(&first, &second),
        "salsa-cached query must return the same Arc on a cache hit"
    );
}
