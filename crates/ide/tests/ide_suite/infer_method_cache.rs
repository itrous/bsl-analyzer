use std::sync::Arc;

use hir::{infer_method_query, Builders, DefDatabase, MethodId, MethodIdInput, ModuleId, Name};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

fn setup_two_method_module(source: &str) -> (RootDatabaseImpl, FileId, MethodId, MethodId) {
    let wrapped = format!("//- /test.bsl\n{source}");
    let fixture = Fixture::parse(&wrapped);
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
    let file_id = *fixture.files.keys().next().expect("fixture must produce one file");

    let symbol_tree = db.symbol_tree(ModuleId::new(file_id));
    let mid_a =
        symbol_tree.find_method(&Name::new("A")).expect("fixture must declare procedure A").id;
    let mid_b =
        symbol_tree.find_method(&Name::new("B")).expect("fixture must declare procedure B").id;
    (db, file_id, mid_a, mid_b)
}

#[test]
fn edit_in_one_method_keeps_other_methods_infer_cell_warm() {
    const SOURCE_BEFORE: &str = r#"
Процедура A()
    Х = "fixed";
КонецПроцедуры

Процедура B()
    Y = 1;
КонецПроцедуры
"#;
    const SOURCE_AFTER: &str = r#"
Процедура A()
    Х = "fixed";
КонецПроцедуры

Процедура B()
    Y = 2;
КонецПроцедуры
"#;

    let (mut db, file_id, mid_a, mid_b) = setup_two_method_module(SOURCE_BEFORE);

    let a_input = MethodIdInput::new(&db, mid_a);
    let b_input = MethodIdInput::new(&db, mid_b);
    let a_before: Arc<_> = infer_method_query(&db, a_input).clone();
    let b_before: Arc<_> = infer_method_query(&db, b_input).clone();

    db.set_file_text(file_id, SOURCE_AFTER);

    let a_input2 = MethodIdInput::new(&db, mid_a);
    let b_input2 = MethodIdInput::new(&db, mid_b);
    let a_after: Arc<_> = infer_method_query(&db, a_input2).clone();
    let b_after: Arc<_> = infer_method_query(&db, b_input2).clone();

    assert!(
        Arc::ptr_eq(&a_before, &a_after),
        "Editing B's body invalidated A's infer_method cell — Phase L \
         per-method partitioning regression. Pre-Phase L this was \
         expected (file-wide infer_query aggregate); post-Phase L \
         A's cell must stay warm."
    );

    assert!(
        !Arc::ptr_eq(&b_before, &b_after),
        "Test fixture broken: B's body was edited but its infer_method \
         cell returned the same Arc — the edit didn't take effect or \
         Salsa skipped re-execution incorrectly."
    );

    let b_after_var = b_after.var_types.get("y").copied();
    assert!(
        matches!(b_after_var, Some(ty) if ty == db.number(None, None)),
        "B's edited body should still infer Y = <Number>; got {b_after_var:?}"
    );
}

#[test]
fn repeated_narrow_queries_within_revision_share_arc() {
    const SOURCE: &str = r#"
Процедура A()
    Х = "stable";
КонецПроцедуры

Процедура B()
    Y = 7;
КонецПроцедуры
"#;
    let (db, _file_id, mid_a, _mid_b) = setup_two_method_module(SOURCE);

    let a_input = MethodIdInput::new(&db, mid_a);
    let r1 = infer_method_query(&db, a_input);
    let r2 = infer_method_query(&db, a_input);
    let r3 = infer_method_query(&db, a_input);

    assert!(Arc::ptr_eq(r1, r2));
    assert!(Arc::ptr_eq(r2, r3));
}

#[test]
fn warming_one_method_does_not_invalidate_other() {
    const SOURCE: &str = r#"
Процедура A()
    Х = 1;
КонецПроцедуры

Процедура B()
    Y = 2;
КонецПроцедуры
"#;
    let (db, _file_id, mid_a, mid_b) = setup_two_method_module(SOURCE);

    let a_input = MethodIdInput::new(&db, mid_a);
    let b_input = MethodIdInput::new(&db, mid_b);

    let a_first = infer_method_query(&db, a_input);
    let _b = infer_method_query(&db, b_input);
    let a_second = infer_method_query(&db, a_input);

    assert!(
        Arc::ptr_eq(a_first, a_second),
        "Querying B's cell invalidated A's cell — cross-method Salsa \
         cache leak"
    );
}
