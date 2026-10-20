//! `method_return_type_query` projects `infer_method_query` instead of running a
//! second `infer_all`. These tests pin the properties that make that projection
//! safe: recursion converges instead of panicking (both queries carry Fixpoint
//! recovery), the full per-method result matches the whole-file `infer_query`
//! slice even for recursive bodies, and the converged value is independent of
//! which query the demand enters first (batch loop vs cross-module call resolve
//! the same SCC through different cycle heads).

use hir::{
    infer_method_query, infer_query, method_return_type_query, Builders, DefDatabase,
    DefWithBodyId, MethodId, MethodIdInput, ModuleId, Name, TypeId,
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

fn return_ty(db: &RootDatabaseImpl, file_id: FileId, name: &str) -> TypeId {
    let mid = find_method(db, file_id, name);
    method_return_type_query(db, MethodIdInput::new(db, mid))
}

/// Per-method inference must equal the whole-file `infer_query` slice even when the
/// method is self-recursive — i.e. the Fixpoint head (`infer_method` on batch entry)
/// converges to the same body result the aggregate fold produces.
#[test]
fn self_recursion_full_result_parity() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция M()
    Возврат M();
КонецФункции
"#,
    );
    let mid = find_method(&db, fid, "M");
    let owner = DefWithBodyId::Method(mid.local_id);

    let per_method = infer_method_query(&db, MethodIdInput::new(&db, mid));
    let aggregate = infer_query(&db, FileIdInput::new(&db, fid));
    let agg_slice = aggregate
        .expr_types_by_body
        .get(&owner)
        .expect("infer_query missing expr_types for self-recursive M");

    assert_eq!(
        &per_method.expr_types, agg_slice,
        "self-recursive M expr_types diverge between infer_method (Fixpoint head) and infer_query"
    );
    assert_eq!(
        method_return_type_query(&db, MethodIdInput::new(&db, mid)),
        db.unknown(),
        "a pure self-recursion has no concrete return type"
    );
}

/// Mutual recursion A->B->A must not panic (neither query is `Panic` strategy) and
/// must converge through the union fixpoint to the base-case type.
#[test]
fn mutual_recursion_converges_to_base_case() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция A(Х)
    Если Х Тогда
        Возврат "base";
    Иначе
        Возврат B(Х);
    КонецЕсли;
КонецФункции

Функция B(Х)
    Возврат A(Х);
КонецФункции
"#,
    );
    let string_ty = db.string(None, false);

    assert_eq!(
        return_ty(&db, fid, "A"),
        string_ty,
        "A's return must converge to the base-case String through the A<->B fixpoint"
    );
    assert_eq!(
        return_ty(&db, fid, "B"),
        string_ty,
        "B's return is A's return, which converges to String"
    );

    for name in ["A", "B"] {
        let mid = find_method(&db, fid, name);
        let owner = DefWithBodyId::Method(mid.local_id);
        let per_method = infer_method_query(&db, MethodIdInput::new(&db, mid));
        let aggregate = infer_query(&db, FileIdInput::new(&db, fid));
        let agg_slice = aggregate
            .expr_types_by_body
            .get(&owner)
            .unwrap_or_else(|| panic!("infer_query missing expr_types for {name}"));
        assert_eq!(
            &per_method.expr_types, agg_slice,
            "mutually recursive {name} expr_types diverge between infer_method and infer_query"
        );
    }
}

/// The converged result must not depend on entry order. Cross-module callers enter
/// via `method_return_type` (its own Fixpoint head); the batch loop enters via
/// `infer_method`/`infer_query` (that head). Both must reach the same fixpoint.
#[test]
fn entry_order_invariance_for_recursive_scc() {
    const SOURCE: &str = r#"
//- /test.bsl
Функция A(Х)
    Если Х Тогда
        Возврат "base";
    Иначе
        Возврат B(Х);
    КонецЕсли;
КонецФункции

Функция B(Х)
    Возврат A(Х);
КонецФункции
"#;

    // Cross-module entry: ask for the return type first (method_return_type head).
    let (db_mrt_first, fid1) = setup(SOURCE);
    let mrt_first_a = return_ty(&db_mrt_first, fid1, "A");
    let mid_a1 = find_method(&db_mrt_first, fid1, "A");
    let mrt_first_exprs =
        infer_method_query(&db_mrt_first, MethodIdInput::new(&db_mrt_first, mid_a1))
            .expr_types
            .clone();

    // Batch entry: drive the whole-file fold first (infer_method/infer_query head).
    let (db_infer_first, fid2) = setup(SOURCE);
    let _ = infer_query(&db_infer_first, FileIdInput::new(&db_infer_first, fid2));
    let infer_first_a = return_ty(&db_infer_first, fid2, "A");
    let mid_a2 = find_method(&db_infer_first, fid2, "A");
    let infer_first_exprs =
        infer_method_query(&db_infer_first, MethodIdInput::new(&db_infer_first, mid_a2))
            .expr_types
            .clone();

    assert_eq!(
        mrt_first_a, infer_first_a,
        "A's return type must be identical whether method_return_type or infer_query is entered first"
    );
    assert_eq!(
        mrt_first_exprs, infer_first_exprs,
        "A's expr_types must be identical regardless of cycle-head entry order"
    );
}

/// Editing a method's body must re-derive its projected return type; editing an
/// unrelated method must not change it. Guards incremental correctness of the
/// `method_return_type -> infer_method` projection edge.
#[test]
fn projection_tracks_body_edits() {
    const BEFORE: &str = r#"
//- /test.bsl
Функция A()
    Возврат "first";
КонецФункции

Функция B()
    Возврат 1;
КонецФункции
"#;
    let (mut db, fid) = setup(BEFORE);
    assert_eq!(return_ty(&db, fid, "A"), db.string(None, false));

    // Unrelated edit (B) must leave A's projected return type intact.
    db.set_file_text(
        fid,
        r#"
Функция A()
    Возврат "first";
КонецФункции

Функция B()
    Возврат 2;
КонецФункции
"#,
    );
    assert_eq!(
        return_ty(&db, fid, "A"),
        db.string(None, false),
        "editing B must not perturb A's projected return type"
    );

    // Editing A's own return expression must re-derive the projection.
    db.set_file_text(
        fid,
        r#"
Функция A()
    Возврат 42;
КонецФункции

Функция B()
    Возврат 2;
КонецФункции
"#,
    );
    assert_eq!(
        return_ty(&db, fid, "A"),
        db.number(None, None),
        "editing A's return must re-derive the projected return type from the fresh body"
    );
}

fn infer_method_cell_count(db: &RootDatabaseImpl) -> usize {
    // salsa names tracked-fn ingredients by their output type; `infer_method_query`
    // is the only query returning `Arc<BodyInferenceResult>`. The ingredient is
    // absent from the report until the first call, which counts as zero cells.
    db.memory_report()
        .into_iter()
        .find(|(name, ..)| name.contains("BodyInferenceResult"))
        .map_or(0, |(_, count, ..)| count)
}

/// The dedup invariant: `method_return_type_query` must derive its value by
/// projecting `infer_method_query` (the single body inference), not by running its
/// own `infer_all`. Calling ONLY the return-type query must therefore populate the
/// `infer_method` cell. A regression that reintroduces a private `infer_all` in
/// `method_return_type_query` would leave this count at zero.
#[test]
fn return_type_projects_through_infer_method_cell() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Функция F()
    Возврат "projected";
КонецФункции
"#,
    );
    assert_eq!(
        infer_method_cell_count(&db),
        0,
        "fixture setup must not warm any infer_method cell before the query under test"
    );

    let mid = find_method(&db, fid, "F");
    let _ = method_return_type_query(&db, MethodIdInput::new(&db, mid));

    assert!(
        infer_method_cell_count(&db) >= 1,
        "method_return_type_query must project infer_method (populating its cell), \
         not run a private infer_all"
    );
}

/// Drives the REAL resolver path: a qualified cross-file call
/// (`resolve_qualified_call -> materialise_signature_enriched -> method_return_type
/// -> infer_method`) into a recursive common-module function. The recursive callee
/// must not panic (Fixpoint recovery) and its projected return type must converge
/// and propagate back to the caller's variable across files.
#[test]
fn cross_file_qualified_call_into_recursive_callee() {
    let (db, fid) = setup(
        r#"
//- /CommonModules/Расчёты/Ext/Module.bsl
Функция Накопить(Х) Экспорт
    Если Х Тогда
        Возврат "итог";
    Иначе
        Возврат Накопить(Х);
    КонецЕсли;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = Расчёты.Накопить(Ложь);
КонецПроцедуры
"#,
    );

    let test_proc = find_method(&db, fid, "Тест");
    let result = infer_method_query(&db, MethodIdInput::new(&db, test_proc));

    assert_eq!(
        result.var_types.get("результат").copied(),
        Some(db.string(None, false)),
        "the recursive common-module callee's projected return type must converge to \
         String and propagate to the caller's variable through the qualified-call resolver"
    );
}
