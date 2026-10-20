use std::sync::Arc;

use hir::{
    infer_method_query, infer_module_code_query, infer_query, Builders, DefDatabase, DefWithBodyId,
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

#[test]
fn wrapper_salsa_cache_hit_shares_arc() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура P()
    Х = "hello";
КонецПроцедуры
"#,
    );
    let input = FileIdInput::new(&db, fid);
    let r1 = infer_query(&db, input);
    let r2 = infer_query(&db, input);
    assert!(
        Arc::ptr_eq(&r1, &r2),
        "infer_query must hit the Salsa cache on a second call within the same revision"
    );
}

#[test]
fn wrapper_fold_preserves_per_body_expr_types() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
А = 1;

Процедура P()
    Б = "x";
КонецПроцедуры
"#,
    );
    let aggregate = infer_query(&db, FileIdInput::new(&db, fid));

    let module_code = infer_module_code_query(&db, FileIdInput::new(&db, fid));
    assert_eq!(
        aggregate.expr_types_by_body.get(&DefWithBodyId::ModuleCode),
        Some(&module_code.expr_types),
        "module-code expr_types slice missing or diverges from infer_module_code_query"
    );

    let symbol_tree = db.symbol_tree(ModuleId::new(fid));
    let pid = symbol_tree.find_method(&Name::new("P")).expect("Procedure P declared").id;
    let owner = DefWithBodyId::Method(pid.local_id);
    let per_method = infer_method_query(&db, MethodIdInput::new(&db, pid));
    assert_eq!(
        aggregate.expr_types_by_body.get(&owner),
        Some(&per_method.expr_types),
        "Method P expr_types slice missing or diverges from infer_method_query"
    );
}

#[test]
fn wrapper_aggregate_ordering_is_deterministic_across_runs() {
    let fixture = r#"
//- /test.bsl
А = 1;

Процедура Первая()
    Х = "x";
КонецПроцедуры

Процедура Вторая()
    Y = 2;
КонецПроцедуры

Процедура Третья()
    Z = Истина;
КонецПроцедуры
"#;
    let (db1, fid1) = setup(fixture);
    let (db2, fid2) = setup(fixture);

    let r1 = infer_query(&db1, FileIdInput::new(&db1, fid1));
    let r2 = infer_query(&db2, FileIdInput::new(&db2, fid2));

    assert_eq!(
        r1.diagnostics, r2.diagnostics,
        "diagnostics Vec order must be identical across two runs of the wrapper"
    );
    assert_eq!(
        r1.call_arg_bindings, r2.call_arg_bindings,
        "call_arg_bindings Vec order must be identical across two runs"
    );
    assert_eq!(
        r1.var_types, r2.var_types,
        "var_types final last-write-wins state must be identical across two runs"
    );
}

#[test]
fn wrapper_diagnostics_carry_owner_for_both_module_code_and_method() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура P()
    Возврат "x";
КонецПроцедуры
"#,
    );
    let result = infer_query(&db, FileIdInput::new(&db, fid));
    for (owner, _diag) in &result.diagnostics {
        assert!(
            result.expr_types_by_body.contains_key(owner),
            "diagnostic owner {owner:?} must also appear in expr_types_by_body — fold-pipeline divergence"
        );
    }
}

#[test]
fn wrapper_clones_per_body_maps_into_aggregate() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура P()
    Х = 42;
КонецПроцедуры
"#,
    );
    let aggregate = infer_query(&db, FileIdInput::new(&db, fid));

    let symbol_tree = db.symbol_tree(ModuleId::new(fid));
    let pid = symbol_tree.find_method(&Name::new("P")).expect("Procedure P declared").id;
    let owner = DefWithBodyId::Method(pid.local_id);
    let per_method = infer_method_query(&db, MethodIdInput::new(&db, pid));

    assert_eq!(
        aggregate.expr_types_by_body.get(&owner).map(|m| m.len()),
        Some(per_method.expr_types.len())
    );
    if let Some(per_method_val) = per_method.var_types.get("х") {
        assert_eq!(aggregate.var_types.get("х"), Some(per_method_val));
    }
    let agg_x = aggregate.var_types.get("х").copied();
    assert_eq!(agg_x, Some(db.number(None, None)));
}
