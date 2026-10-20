use std::sync::Arc;

use hir::{infer_module_code_query, Builders, DefWithBodyId, HirDatabase};
use ide_db::base_db::{FileIdInput, SourceDatabase, SourceRoot, SourceRootId};
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

#[test]
fn no_module_code_body_returns_default() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция F()
    Возврат "x";
КонецФункции
"#,
    );
    let input = FileIdInput::new(&db, fid);
    let result = infer_module_code_query(&db, input);
    assert_eq!(result.owner, DefWithBodyId::ModuleCode);
    assert!(result.var_types.is_empty(), "no module-code body → empty var_types");
    assert!(result.expr_types.is_empty(), "no module-code body → empty expr_types");
    assert!(result.diagnostics.is_empty());
}

#[test]
fn module_level_implicit_local_assignment_populates_var_types() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Х = "hello";
"#,
    );
    let input = FileIdInput::new(&db, fid);
    let result = infer_module_code_query(&db, input);
    assert_eq!(result.owner, DefWithBodyId::ModuleCode);
    assert_eq!(result.var_types.get("х").copied(), Some(db.string(None, false)),);
}

#[test]
fn salsa_cache_hit_shares_arc() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Перем Г;
Г = 42;
"#,
    );
    let input = FileIdInput::new(&db, fid);
    let r1 = infer_module_code_query(&db, input);
    let r2 = infer_module_code_query(&db, input);
    assert!(Arc::ptr_eq(r1, r2), "second call within the same revision must hit the Salsa cache");
}

#[test]
fn bilingual_implicit_local_shows_in_implicit_locals() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
ИмяРеквизита = "Наименование";
"#,
    );
    let input = FileIdInput::new(&db, fid);
    let result = infer_module_code_query(&db, input);
    assert!(
        result.implicit_locals.contains_key("имяреквизита"),
        "module-code implicit local must be tracked under its lowercase key (got: {:?})",
        result.implicit_locals.keys().collect::<Vec<_>>()
    );
}

#[test]
fn trait_method_delegates_to_query() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Перем А;
А = Истина;
"#,
    );
    let via_trait = db.infer_module_code(fid);
    let via_query = infer_module_code_query(&db, FileIdInput::new(&db, fid));
    assert!(
        Arc::ptr_eq(&via_trait, via_query),
        "RootDatabaseImpl::infer_module_code must delegate to the query without an extra layer"
    );
}
