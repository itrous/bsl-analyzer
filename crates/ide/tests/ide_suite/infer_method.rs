use std::sync::Arc;

use hir::{
    infer_method_query, infer_query, Builders, DefDatabase, DefWithBodyId, HirDatabase, MethodId,
    MethodIdInput, ModuleId, Name,
};
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

fn find_method(db: &RootDatabaseImpl, file_id: FileId, name: &str) -> MethodId {
    let symbol_tree = db.symbol_tree(ModuleId::new(file_id));
    symbol_tree
        .find_method(&Name::new(name))
        .unwrap_or_else(|| panic!("expected method `{name}` in {file_id:?}"))
        .id
}

#[test]
fn implicit_local_assignment_populates_per_method_var_types() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура P()
    Х = "hello";
КонецПроцедуры
"#,
    );
    let mid = find_method(&db, fid, "P");
    let result = infer_method_query(&db, MethodIdInput::new(&db, mid));
    assert_eq!(result.owner, DefWithBodyId::Method(mid.local_id));
    assert_eq!(result.var_types.get("х").copied(), Some(db.string(None, false)));
    assert!(
        result.expr_types.values().any(|tid| *tid == db.string(None, false)),
        "Method P's body must record a Ty::String expr from the literal RHS"
    );
}

#[test]
fn per_method_parity_with_infer_query_slice() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура Один()
    А = 1;
КонецПроцедуры

Процедура Два()
    Б = "x";
КонецПроцедуры
"#,
    );
    let aggregate = infer_query(&db, FileIdInput::new(&db, fid));

    for name in ["Один", "Два"] {
        let mid = find_method(&db, fid, name);
        let owner = DefWithBodyId::Method(mid.local_id);
        let per_method = infer_method_query(&db, MethodIdInput::new(&db, mid));

        let agg_slice = aggregate
            .expr_types_by_body
            .get(&owner)
            .unwrap_or_else(|| panic!("infer_query missing expr_types for {name}"));
        assert_eq!(
            &per_method.expr_types, agg_slice,
            "expr_types slice for {name} diverges between infer_method and infer_query"
        );

        let agg_implicit = aggregate
            .implicit_locals_by_body
            .get(&owner)
            .unwrap_or_else(|| panic!("infer_query missing implicit_locals for {name}"));
        assert_eq!(
            &per_method.implicit_locals, agg_implicit,
            "implicit_locals slice for {name} diverges"
        );
    }
}

#[test]
fn bilingual_procedure_inferred_owner_matches() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция ВернутьСтроку()
    Возврат "x";
КонецФункции
"#,
    );
    let mid = find_method(&db, fid, "ВернутьСтроку");
    let result = infer_method_query(&db, MethodIdInput::new(&db, mid));
    assert_eq!(result.owner, DefWithBodyId::Method(mid.local_id));
    assert!(
        result.expr_types.values().any(|tid| *tid == db.string(None, false)),
        "Функция ВернутьСтроку must record Ty::String from its literal return"
    );
}

#[test]
fn salsa_cache_hit_shares_arc() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура P()
    Х = 42;
КонецПроцедуры
"#,
    );
    let mid = find_method(&db, fid, "P");
    let input = MethodIdInput::new(&db, mid);
    let r1 = infer_method_query(&db, input);
    let r2 = infer_method_query(&db, input);
    assert!(Arc::ptr_eq(r1, r2), "second call within the same revision must hit the Salsa cache");
}

#[test]
fn trait_method_delegates_to_query() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура P()
    А = Истина;
КонецПроцедуры
"#,
    );
    let mid = find_method(&db, fid, "P");
    let input = MethodIdInput::new(&db, mid);
    let via_trait = db.infer_method(input);
    let via_query = infer_method_query(&db, input);
    assert!(
        Arc::ptr_eq(&via_trait, via_query),
        "RootDatabaseImpl::infer_method must delegate to the query without an extra layer"
    );
}

#[test]
fn no_module_code_seed_into_method_var_types() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Х = "module-level";

Процедура P()
    Х = 42;
КонецПроцедуры
"#,
    );
    let mid = find_method(&db, fid, "P");
    let result = infer_method_query(&db, MethodIdInput::new(&db, mid));
    assert_eq!(
        result.var_types.get("х").copied(),
        Some(db.number(None, None)),
        "Method P's var_types must show its OWN assignment, not the module-code seed"
    );
}
