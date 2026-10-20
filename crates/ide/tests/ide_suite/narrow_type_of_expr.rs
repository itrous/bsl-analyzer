use hir::{
    narrow_query, narrowed_type_at, Body, Builders, DefDatabase, DefWithBodyId, ExprId,
    IdConversion, ModuleId, Name, Semantics, Type,
};
use ide_db::base_db::{RootQueryDb, SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::collections::HashSet;
use syntax::{SyntaxKind, SyntaxNode, TextRange};
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
    (db, test_file)
}

fn first_method_owner(db: &RootDatabaseImpl, file_id: FileId) -> DefWithBodyId {
    let module_bodies = db.module_bodies(ModuleId::new(file_id));
    let (local_id, _body, _source_map) =
        module_bodies.method_bodies().next().expect("fixture declares a method body");
    DefWithBodyId::Method(local_id)
}

fn expr_id_at_range(db: &RootDatabaseImpl, file_id: FileId, range: TextRange) -> ExprId {
    let module_bodies = db.module_bodies(ModuleId::new(file_id));
    let found = module_bodies
        .method_bodies()
        .find_map(|(_local_id, _body, source_map)| source_map.expr_at_range(range));
    found.unwrap_or_else(|| panic!("BodySourceMap has no expression at range {:?}", range))
}

fn body_of(db: &RootDatabaseImpl, file_id: FileId, owner: DefWithBodyId) -> Body {
    let module_bodies = db.module_bodies(ModuleId::new(file_id));
    match owner {
        DefWithBodyId::Method(local_id) => {
            module_bodies.body(local_id).expect("method body lowered").clone()
        }
        DefWithBodyId::ModuleCode => {
            module_bodies.module_code().expect("module-level body lowered").clone()
        }
    }
}

fn nth_ident_expr_at_distinct_position(root: &SyntaxNode, ident: &str, nth: usize) -> SyntaxNode {
    let mut seen: HashSet<TextRange> = HashSet::new();
    root.descendants()
        .filter(|n| n.kind() == SyntaxKind::EXPR && n.text() == ident)
        .filter(|n| seen.insert(n.text_range()))
        .nth(nth)
        .unwrap_or_else(|| {
            panic!("fixture missing EXPR(IDENT({ident})) at distinct position index {nth}")
        })
}

#[test]
fn narrow_query_returns_some_for_body_with_guard() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 42;
    Если ТипЗнч(Х) = Тип("Строка") Тогда
        А = Х;
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);

    let result = narrow_query(&db, file_id, owner).expect("narrow_query must converge");
    assert!(
        result.cfg().vertices().count() > 0,
        "CFG built for a non-empty body must have at least one vertex"
    );
}

#[test]
fn narrowed_type_at_then_body_returns_narrowed_ty() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 42;
    Если ТипЗнч(Х) = Тип("Строка") Тогда
        А = Х;
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let then_rhs = nth_ident_expr_at_distinct_position(&root, "Х", 1);
    let expr_id = expr_id_at_range(&db, file_id, then_rhs.text_range());

    let result = narrow_query(&db, file_id, owner).expect("narrow_query must converge");
    assert_eq!(
        narrowed_type_at(
            &db,
            &result,
            &body_of(&db, file_id, owner),
            expr_id.to_idx(),
            &Name::new("Х")
        ),
        Some(db.string(None, false)),
        "then-body `Х` must observe the narrowed Ty::String (True-edge overlay)"
    );

    let sema = Semantics::new(&db);
    assert_eq!(
        sema.type_of_expr(file_id, &then_rhs),
        db.string(None, false),
        "Semantics::type_of_expr merges the narrowed overlay onto the base"
    );
}

#[test]
fn narrowed_type_at_guard_receiver_returns_pre_narrow_reaching_ty() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 42;
    Если ТипЗнч(Х) = Тип("Строка") Тогда
        А = Х;
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let receiver = nth_ident_expr_at_distinct_position(&root, "Х", 0);
    let expr_id = expr_id_at_range(&db, file_id, receiver.text_range());

    let result = narrow_query(&db, file_id, owner).expect("narrow_query must converge");
    assert_eq!(
        narrowed_type_at(
            &db,
            &result,
            &body_of(&db, file_id, owner),
            expr_id.to_idx(),
            &Name::new("Х")
        ),
        Some(db.number(None, None)),
        "guard-receiver `Х` observes the pre-narrow reaching type (Number from `Х = 42`), \
         not the narrowed one (String)"
    );

    let sema = Semantics::new(&db);
    assert_eq!(
        sema.type_of_expr(file_id, &receiver),
        db.number(None, None),
        "hover on guard receiver returns the pre-narrow Number"
    );
}

#[test]
fn narrowed_type_at_after_one_sided_if_on_parameter_drops() {
    let fixture = r#"
//- /test.bsl
Процедура П(Х)
    Если ТипЗнч(Х) = Тип("Строка") Тогда
        А = Х;
    КонецЕсли;
    Б = Х;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let after_if = nth_ident_expr_at_distinct_position(&root, "Х", 2);
    let expr_id = expr_id_at_range(&db, file_id, after_if.text_range());

    let result = narrow_query(&db, file_id, owner).expect("narrow_query must converge");
    assert_eq!(
        narrowed_type_at(
            &db,
            &result,
            &body_of(&db, file_id, owner),
            expr_id.to_idx(),
            &Name::new("Х")
        ),
        None,
        "post-КонецЕсли `Х` must drop the one-sided narrowing (entry only in True branch)"
    );
}

#[test]
fn narrow_query_is_deterministic() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 42;
    Если ТипЗнч(Х) = Тип("Строка") Тогда
        А = Х;
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);

    let r1 = narrow_query(&db, file_id, owner).expect("first call must converge");
    let r2 = narrow_query(&db, file_id, owner).expect("second call must converge");

    assert_eq!(
        *r1, *r2,
        "narrow_query is a pure function of the database state — two calls must agree"
    );
}

#[test]
fn narrow_is_case_insensitive_across_guard_and_hover() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 42;
    Если ТипЗнч(х) = Тип("Строка") Тогда
        А = Х;
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let then_rhs = nth_ident_expr_at_distinct_position(&root, "Х", 0);
    let expr_id = expr_id_at_range(&db, file_id, then_rhs.text_range());

    let result = narrow_query(&db, file_id, owner).expect("narrow_query must converge");
    assert_eq!(
        narrowed_type_at(
            &db,
            &result,
            &body_of(&db, file_id, owner),
            expr_id.to_idx(),
            &Name::new("Х")
        ),
        Some(db.string(None, false)),
        "mixed-case guard receiver (`х`) must narrow the uppercase reference (`Х`)"
    );

    let sema = Semantics::new(&db);
    assert_eq!(
        sema.type_of_expr(file_id, &then_rhs),
        db.string(None, false),
        "hover under Semantics::type_of_expr must see the case-folded narrowing"
    );
}

#[test]
fn narrow_query_handles_module_code_body() {
    let fixture = r#"
//- /test.bsl
Х = 42;
Если ТипЗнч(Х) = Тип("Строка") Тогда
    А = Х;
КонецЕсли;
"#;
    let (db, file_id) = setup(fixture);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let then_rhs = nth_ident_expr_at_distinct_position(&root, "Х", 1);
    let module_bodies = db.module_bodies(ModuleId::new(file_id));
    let module_code = module_bodies.module_code_result().expect("module-level body lowered");
    let expr_id = module_code
        .source_map()
        .expr_at_range(then_rhs.text_range())
        .expect("module-level BodySourceMap must locate the then-body Х");

    let result = narrow_query(&db, file_id, DefWithBodyId::ModuleCode)
        .expect("narrow_query must converge for ModuleCode");
    assert_eq!(
        narrowed_type_at(
            &db,
            &result,
            &body_of(&db, file_id, DefWithBodyId::ModuleCode),
            expr_id.to_idx(),
            &Name::new("Х"),
        ),
        Some(db.string(None, false)),
        "module-code narrowing must reach the then-body Х"
    );

    let sema = Semantics::new(&db);
    assert_eq!(
        sema.type_of_expr(file_id, &then_rhs),
        db.string(None, false),
        "Semantics::type_of_expr on module-level then-body must merge the ModuleCode narrowing"
    );
}

#[test]
fn narrowed_type_at_else_body_inherits_reaching_when_complement_degrades() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 42;
    Если ТипЗнч(Х) = Тип("Строка") Тогда
        А = Х;
    Иначе
        Б = Х;
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let else_rhs = nth_ident_expr_at_distinct_position(&root, "Х", 2);
    let expr_id = expr_id_at_range(&db, file_id, else_rhs.text_range());

    let result = narrow_query(&db, file_id, owner).expect("narrow_query must converge");
    assert_eq!(
        narrowed_type_at(
            &db,
            &result,
            &body_of(&db, file_id, owner),
            expr_id.to_idx(),
            &Name::new("Х")
        ),
        Some(db.number(None, None)),
        "else-body Х inherits the Conditional-IN reaching type when the complement is Unknown"
    );

    let sema = Semantics::new(&db);
    assert_eq!(
        sema.type_of_expr(file_id, &else_rhs),
        db.number(None, None),
        "Semantics::type_of_expr on else-body must merge the else-IN overlay (Number)"
    );
}

#[test]
fn narrow_query_returns_none_for_unknown_owner() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 1;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = DefWithBodyId::Method(hir::MethodKey::first("НетТакой"));

    assert!(
        narrow_query(&db, file_id, owner).is_none(),
        "narrow_query must return None when the owner does not resolve to a body in the file"
    );
}

#[test]
fn type_narrowing_enabled_by_default() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 42;
    Если ТипЗнч(Х) = Тип("Строка") Тогда
        А = Х;
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(db.type_narrowing_enabled(), "fresh database must default `type_narrowing = true`");

    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let then_rhs = nth_ident_expr_at_distinct_position(&root, "Х", 1);

    let sema = Semantics::new(&db);
    assert_eq!(
        sema.type_of_expr(file_id, &then_rhs),
        db.string(None, false),
        "with default flags the narrowing overlay is applied — then-body `Х` sees `Ty::String`"
    );
}

#[test]
fn type_narrowing_disabled_skips_overlay() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 42;
    Если ТипЗнч(Х) = Тип("Строка") Тогда
        А = Х;
    КонецЕсли;
КонецПроцедуры
"#;
    let (mut db, file_id) = setup(fixture);
    db.set_type_narrowing_enabled(false);
    assert!(
        !db.type_narrowing_enabled(),
        "setter must flip the Salsa input so subsequent reads return false"
    );

    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let then_rhs = nth_ident_expr_at_distinct_position(&root, "Х", 1);

    let sema = Semantics::new(&db);
    assert_eq!(
        sema.type_of_expr(file_id, &then_rhs),
        db.number(None, None),
        "with narrowing disabled, the then-body `Х` falls back to the base `Ty::Number`"
    );

    db.set_type_narrowing_enabled(true);
    let sema = Semantics::new(&db);
    assert_eq!(
        sema.type_of_expr(file_id, &then_rhs),
        db.string(None, false),
        "re-enabling the flag restores the narrowed `Ty::String` without DB rebuild"
    );
}

#[test]
fn is_assignable_to_sees_narrowed_ty_from_semantics() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Х = 42;
    Если ТипЗнч(Х) = Тип("Строка") Тогда
        А = Х;
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let then_rhs = nth_ident_expr_at_distinct_position(&root, "Х", 1);

    let sema = Semantics::new(&db);
    let narrowed_ty = sema.type_of_expr(file_id, &then_rhs);
    assert_eq!(
        narrowed_ty,
        db.string(None, false),
        "precondition: narrowing overlay must reach the then-body `Х` before we start the assignability probe",
    );

    let narrowed = Type::from_id(&db, file_id, sema.type_of_expr(file_id, &then_rhs));
    let expect_string = Type::from_id(&db, file_id, db.string(None, false));
    let expect_number = Type::from_id(&db, file_id, db.number(None, None));

    assert!(
        narrowed.is_assignable_to(&expect_string),
        "narrowed `Х: String` must be assignable to a `String` slot"
    );
    assert!(
        !narrowed.is_assignable_to(&expect_number),
        "narrowed `Х: String` must NOT be assignable to a `Number` slot — confirms the \
         predicate consumes the narrowed overlay, not the base `Ty::Number` from `Х = 42`"
    );
}

#[test]
fn narrowed_type_at_after_terminating_undefined_guard_keeps_complement() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Если Истина Тогда
        Х = 42;
    Иначе
        Х = Неопределено;
    КонецЕсли;
    Если Х = Неопределено Тогда
        Возврат;
    КонецЕсли;
    Б = Х;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let after_guard = nth_ident_expr_at_distinct_position(&root, "Х", 1);
    let expr_id = expr_id_at_range(&db, file_id, after_guard.text_range());

    let result = narrow_query(&db, file_id, owner).expect("narrow_query must converge");
    assert_eq!(
        narrowed_type_at(
            &db,
            &result,
            &body_of(&db, file_id, owner),
            expr_id.to_idx(),
            &Name::new("Х")
        ),
        Some(db.number(None, None)),
        "after `Если Х = Неопределено Тогда Возврат` the then-branch terminates, so the \
         inverted guard must survive the merge: `Х` is Number, not Number|Неопределено"
    );
}

#[test]
fn narrowed_type_at_after_non_terminating_undefined_guard_drops() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Если Истина Тогда
        Х = 42;
    Иначе
        Х = Неопределено;
    КонецЕсли;
    Если Х = Неопределено Тогда
        Б = 1;
    КонецЕсли;
    В = Х;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let after_guard = nth_ident_expr_at_distinct_position(&root, "Х", 1);
    let expr_id = expr_id_at_range(&db, file_id, after_guard.text_range());

    let result = narrow_query(&db, file_id, owner).expect("narrow_query must converge");
    assert_eq!(
        narrowed_type_at(
            &db,
            &result,
            &body_of(&db, file_id, owner),
            expr_id.to_idx(),
            &Name::new("Х")
        ),
        Some(db.union(vec![db.number(None, None), db.undefined()])),
        "a non-terminating then-branch reaches the merge, so both arms survive: \
         the tracked set equals the base union — no effective narrowing"
    );
}

#[test]
fn narrowed_type_at_after_terminating_raise_guard_keeps_complement() {
    let fixture = r#"
//- /test.bsl
Процедура П()
    Если Истина Тогда
        Х = 42;
    Иначе
        Х = Неопределено;
    КонецЕсли;
    Если Х = Неопределено Тогда
        ВызватьИсключение "нет значения";
    КонецЕсли;
    Б = Х;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let owner = first_method_owner(&db, file_id);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let after_guard = nth_ident_expr_at_distinct_position(&root, "Х", 1);
    let expr_id = expr_id_at_range(&db, file_id, after_guard.text_range());

    let result = narrow_query(&db, file_id, owner).expect("narrow_query must converge");
    assert_eq!(
        narrowed_type_at(
            &db,
            &result,
            &body_of(&db, file_id, owner),
            expr_id.to_idx(),
            &Name::new("Х")
        ),
        Some(db.number(None, None)),
        "ВызватьИсключение terminates the then-branch the same way Возврат does"
    );
}
