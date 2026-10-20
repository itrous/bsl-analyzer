use hir::{Builders, DefDatabase, DefWithBodyId, HirDatabase, ModuleId, Semantics};
use ide_db::base_db::{RootQueryDb, SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use syntax::ast::{self, AstNode};
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

fn setup_with_designer(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let (mut db, file_id) = setup(fixture_text);
    db.set_all_config_paths(
        vec![(
            None,
            std::path::PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../bsl-metadata/fixtures/designer"
            )),
        )],
        None,
    );
    (db, file_id)
}

fn first_call_expr(root: &syntax::SyntaxNode, method_name: &str) -> Option<ast::CallExpr> {
    root.descendants().filter_map(ast::CallExpr::cast).find(|call| {
        call.syntax().descendants_with_tokens().any(|tok| {
            tok.as_token()
                .map(|t| t.kind() == syntax::SyntaxKind::IDENT && t.text() == method_name)
                .unwrap_or(false)
        })
    })
}

fn first_field_expr(root: &syntax::SyntaxNode, field_name: &str) -> Option<ast::FieldExpr> {
    root.descendants().filter_map(ast::FieldExpr::cast).find(|fe| {
        fe.syntax()
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|tok| tok.kind() == syntax::SyntaxKind::IDENT)
            .any(|tok| tok.text() == field_name)
    })
}

#[test]
fn type_of_expr_resolves_across_bodies() {
    let fixture = r#"
//- /test.bsl
Функция Первая()
    А = 1;
    Возврат А;
КонецФункции

Функция Вторая()
    Б = 2;
    Возврат Б;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let infer = db.infer(file_id);

    let method_bodies: Vec<_> = infer
        .expr_types_by_body
        .iter()
        .filter(|(owner, _)| matches!(owner, DefWithBodyId::Method(_)))
        .collect();
    assert_eq!(
        method_bodies.len(),
        2,
        "each function body must keep its own expr_types map after merge \
         (got {} method bodies, total entries {})",
        method_bodies.len(),
        infer.expr_types_by_body.len(),
    );
    for (owner, map) in method_bodies {
        assert!(!map.is_empty(), "method body {owner:?} must carry at least one ExprId");
    }
}

#[test]
fn type_of_call_expr_matches_infer() {
    let fixture = r#"
//- /test.bsl
Функция Тест()
    Р = 42;
    Возврат Р;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let sema = Semantics::new(&db);
    let module_id = ModuleId::new(file_id);
    let module_bodies = db.module_bodies(module_id);

    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let literal =
        root.descendants().filter_map(ast::Literal::cast).next().expect("fixture has a literal");

    assert_eq!(
        sema.type_of_expr(file_id, literal.syntax()),
        db.number(None, None),
        "Semantics::type_of_expr must return Ty::Number for literal `42`"
    );

    let infer = db.infer(file_id);
    let (owner_match, expr_match) = module_bodies
        .method_bodies()
        .find_map(|(local_id, _body, source_map)| {
            source_map
                .expr_at_range(literal.syntax().text_range())
                .map(|eid| (DefWithBodyId::Method(local_id), eid))
        })
        .expect("BodySourceMap must locate the literal ExprId");
    assert_eq!(
        infer.type_id_of_expr_in(owner_match, expr_match),
        Some(db.number(None, None)),
        "direct expr_types_by_body lookup must agree with Semantics::type_of_expr"
    );
}

#[test]
fn type_of_module_level_expr_resolves() {
    let fixture = r#"
//- /test.bsl
Х = 42;
"#;
    let (db, file_id) = setup(fixture);
    let sema = Semantics::new(&db);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let literal =
        root.descendants().filter_map(ast::Literal::cast).next().expect("fixture has a literal");

    assert_eq!(
        sema.type_of_expr(file_id, literal.syntax()),
        db.number(None, None),
        "Semantics::type_of_expr must resolve the module-level `42` literal"
    );

    let module_id = ModuleId::new(file_id);
    let module_bodies = db.module_bodies(module_id);
    let expr_id = module_bodies
        .module_code_result()
        .expect("module-level body must be lowered")
        .source_map()
        .expr_at_range(literal.syntax().text_range())
        .expect("module-level BodySourceMap must locate the literal");
    let infer = db.infer(file_id);
    assert_eq!(
        infer.type_id_of_expr_in(DefWithBodyId::ModuleCode, expr_id),
        Some(db.number(None, None)),
        "direct ModuleCode lookup must agree with Semantics::type_of_expr"
    );
}

#[test]
fn type_of_field_expr_matches_infer() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Ссылка();
    Р = С.Реквизит2;
    Возврат Р;
КонецФункции
"#;
    let (db, file_id) = setup_with_designer(fixture);
    let sema = Semantics::new(&db);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let field = first_field_expr(&root, "Реквизит2").expect("fixture has С.Реквизит2");

    assert_eq!(
        sema.type_of_expr(file_id, field.syntax()),
        db.number(None, None),
        "Semantics::type_of_expr on a field access must agree with inference (Ty::Number)"
    );

    let module_id = ModuleId::new(file_id);
    let module_bodies = db.module_bodies(module_id);
    let (owner, expr_id) = module_bodies
        .method_bodies()
        .find_map(|(local_id, _body, source_map)| {
            source_map
                .expr_at_range(field.syntax().text_range())
                .map(|eid| (DefWithBodyId::Method(local_id), eid))
        })
        .expect("BodySourceMap must locate the field-expr ExprId");
    let infer = db.infer(file_id);
    assert_eq!(
        infer.type_id_of_expr_in(owner, expr_id),
        Some(db.number(None, None)),
        "direct expr_types_by_body lookup must agree with Semantics::type_of_expr"
    );
}

#[test]
fn type_of_recovered_receiver_inside_preproc_branch_matches_outside() {
    let fixture = r#"
//- /test.bsl
Процедура Внутри()
    Сп = Новый Структура("Код", 1);
    #Если Сервер Тогда
    Сп.В
    #КонецЕсли
КонецПроцедуры

Процедура Снаружи()
    Сп2 = Новый Структура("Код", 1);
    Сп2.В
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let sema = Semantics::new(&db);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let receiver = |var: &str| {
        root.descendants()
            .filter_map(ast::FieldExpr::cast)
            .filter_map(|fe| fe.syntax().children().next())
            .find(|receiver| receiver.text() == var)
            .unwrap_or_else(|| panic!("fixture has a field access on `{var}`"))
    };
    let outside_ty = sema.type_of_expr(file_id, &receiver("Сп2"));
    let inside_ty = sema.type_of_expr(file_id, &receiver("Сп"));

    // Positive control: with an unresolved outside type the parity assert
    // below would pass vacuously as Unknown == Unknown.
    assert_ne!(
        outside_ty,
        db.unknown(),
        "control: recovered receiver outside #Если must resolve to a known type"
    );
    assert_eq!(
        inside_ty, outside_ty,
        "recovered receiver inside a #Если branch must type like the same code outside"
    );
}

#[test]
fn type_of_expr_unknown_for_non_expression_node() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let sema = Semantics::new(&db);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    assert_eq!(sema.type_of_expr(file_id, &root), db.unknown());
}

#[test]
fn type_of_expr_covers_call_site() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   Число
Функция Сумма() Экспорт
    Возврат 0;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Сумма();
    Возврат С;
КонецФункции
"#;
    let (db, file_id) = setup_with_designer(fixture);
    let sema = Semantics::new(&db);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let call = first_call_expr(&root, "Сумма").expect("fixture has a Сумма() call");
    assert_eq!(
        sema.type_of_expr(file_id, call.syntax()),
        db.number(None, None),
        "type_of_expr on a CallExpr must reflect the JSDoc-lowered return type"
    );
}
