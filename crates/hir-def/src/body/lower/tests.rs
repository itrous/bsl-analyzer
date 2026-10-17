use base_db::{RootQueryDb, SourceDatabase};
use ide_db::RootDatabaseImpl;
use stdx::case::CaseExt;
use syntax::{SyntaxKind, SyntaxNode};
use vfs::FileId;

use crate::body::BodyDiagnostic;
use crate::hir::{Expr, Literal, Stmt};
use crate::{BindingId, IdConversion};

use super::lower_method;

fn parse_method(code: &str) -> SyntaxNode {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId::from_raw(0);
    db.set_file_text(file_id, code);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    root.descendants()
        .find(|n| matches!(n.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
        .expect("No method found in test code")
}

#[test]
fn test_lower_empty_procedure() {
    let method = parse_method("Процедура Тест() КонецПроцедуры");
    let result = lower_method(&method, false);

    assert_eq!(result.body.params.len(), 0);
    assert!(!result.diagnostics.iter().any(|d| matches!(d, BodyDiagnostic::EmptyCodeBlock { .. })));
}

#[test]
fn test_lower_function_without_return() {
    let method = parse_method("Функция Тест() КонецФункции");
    let result = lower_method(&method, true);

    assert!(result
        .diagnostics
        .iter()
        .any(|d| matches!(d, BodyDiagnostic::FunctionShouldHaveReturn { .. })));
}

#[test]
fn test_if_branch_with_extension_directives_is_not_empty() {
    let method = parse_method(
        "Процедура Тест()
            Если Условие Тогда
                #Удаление
                А = 1;
                #КонецУдаления
                #Вставка
                Б = 2;
                #КонецВставки
            КонецЕсли;
        КонецПроцедуры",
    );
    let result = lower_method(&method, false);

    assert!(
        !result.diagnostics.iter().any(|d| matches!(d, BodyDiagnostic::EmptyCodeBlock { .. })),
        "an Если branch holding #Вставка/#Удаление directives is not an empty code block"
    );
}

#[test]
fn test_genuinely_empty_if_branch_is_still_flagged() {
    let method = parse_method(
        "Процедура Тест()
            Если Условие Тогда
            КонецЕсли;
        КонецПроцедуры",
    );
    let result = lower_method(&method, false);

    assert!(
        result.diagnostics.iter().any(|d| matches!(d, BodyDiagnostic::EmptyCodeBlock { .. })),
        "a branch with no statements and no extension directives is still an empty code block"
    );
}

#[test]
fn test_lower_function_with_return() {
    let method = parse_method(
        "Функция Тест()
            Возврат 42;
        КонецФункции",
    );
    let result = lower_method(&method, true);

    assert!(!result
        .diagnostics
        .iter()
        .any(|d| matches!(d, BodyDiagnostic::FunctionShouldHaveReturn { .. })));
}

#[test]
fn test_lower_procedure_with_params() {
    let method = parse_method("Процедура Тест(А, Знач Б, В = 1) КонецПроцедуры");
    let result = lower_method(&method, false);

    assert_eq!(result.body.params.len(), 3);

    let param1 = result.body.binding(BindingId::from_idx(result.body.params[0]));
    assert_eq!(param1.name.as_str(), "А");
    assert!(!param1.is_val);
    assert!(param1.default_value.is_none(), "param А should not have default value");

    let param2 = result.body.binding(BindingId::from_idx(result.body.params[1]));
    assert_eq!(param2.name.as_str(), "Б");
    assert!(param2.is_val);
    assert!(param2.default_value.is_none(), "param Б should not have default value");

    let param3 = result.body.binding(BindingId::from_idx(result.body.params[2]));
    assert_eq!(param3.name.as_str(), "В");
    assert!(!param3.is_val);
    assert!(param3.default_value.is_some(), "param В should have default value");

    let default_expr_id = param3.default_value.unwrap();
    let default_expr = result.body.expr_idx(default_expr_id);
    assert!(
        matches!(default_expr, Expr::Literal(Literal::Number(_))),
        "default value should be a number literal"
    );
}

#[test]
fn test_lower_assignment() {
    let method = parse_method(
        "Процедура Тест()
            А = 42;
        КонецПроцедуры",
    );
    let result = lower_method(&method, false);

    assert_eq!(result.body.body_stmts.len(), 1);
    let stmt = result.body.stmt_idx(result.body.body_stmts[0]);
    assert!(matches!(stmt, Stmt::Assign { .. }));
}

#[test]
fn test_lower_self_assign() {
    let method = parse_method(
        "Процедура Тест()
            А = А;
        КонецПроцедуры",
    );
    let result = lower_method(&method, false);

    assert!(result.diagnostics.iter().any(|d| matches!(d, BodyDiagnostic::SelfAssign { .. })));
}

#[test]
fn test_lower_if_stmt() {
    let method = parse_method(
        "Процедура Тест()
            Если Истина Тогда
                А = 1;
            КонецЕсли;
        КонецПроцедуры",
    );
    let result = lower_method(&method, false);

    assert_eq!(result.body.body_stmts.len(), 1);
    let stmt = result.body.stmt_idx(result.body.body_stmts[0]);
    assert!(matches!(stmt, Stmt::If { .. }));
}

#[test]
fn statements_inside_region_are_lowered() {
    // Flat region markers must not swallow the statements between them.
    let method = parse_method(
        "Процедура Тест()
            #Область Р
            А = 1;
            Б = 2;
            #КонецОбласти
        КонецПроцедуры",
    );
    let result = lower_method(&method, false);

    assert_eq!(result.body.body_stmts.len(), 2, "both assignments must reach the HIR body");
    for &id in result.body.body_stmts.iter() {
        assert!(matches!(result.body.stmt_idx(id), Stmt::Assign { .. }));
    }
}

#[test]
fn region_crossing_if_keeps_if_in_hir() {
    let method = parse_method(
        "Процедура Тест()
            #Область Р
            Если Истина Тогда
                А = 1;
            #КонецОбласти
            КонецЕсли;
        КонецПроцедуры",
    );
    let result = lower_method(&method, false);

    assert_eq!(result.body.body_stmts.len(), 1);
    assert!(matches!(result.body.stmt_idx(result.body.body_stmts[0]), Stmt::If { .. }));
}

#[test]
fn function_return_inside_region_is_seen() {
    // A trailing region marker after Возврат must not hide the return.
    let method = parse_method(
        "Функция Тест()
            #Область Р
            Возврат 1;
            #КонецОбласти
        КонецФункции",
    );
    let result = lower_method(&method, true);

    assert!(!result
        .diagnostics
        .iter()
        .any(|d| matches!(d, BodyDiagnostic::FunctionShouldHaveReturn { .. })));
}

#[test]
fn test_sdbl_collected_in_hir() {
    let method = parse_method(
        r#"
Процедура Тест()
    Запрос = "SELECT Ссылка FROM Справочник.Валюты";
    Результат = Запрос.Выполнить();
КонецПроцедуры
"#,
    );
    let result = lower_method(&method, false);

    assert_eq!(result.body.sdbl_exprs.len(), 1);

    let (expr_id, _literal, query_info) = &result.body.sdbl_exprs[0];
    assert!(query_info.is_valid());
    assert!(query_info.query_text.contains("SELECT"));

    match result.body.expr_idx(*expr_id) {
        Expr::Literal(Literal::String(_)) => {}
        _ => panic!("Expected string literal"),
    }
}

#[test]
fn test_sdbl_multiline_query() {
    let method = parse_method(
        r#"
Функция ПолучитьДанные()
    Запрос = "SELECT
             |    Ссылка,
             |    Наименование
             |FROM Справочник.Валюты";
    Возврат Запрос.Выполнить();
КонецФункции
"#,
    );
    let result = lower_method(&method, true);

    assert_eq!(result.body.sdbl_exprs.len(), 1);

    let (_expr_id, _literal, query_info) = &result.body.sdbl_exprs[0];
    assert!(query_info.is_valid());
    assert!(query_info.query_text.contains("Наименование"));
}

#[test]
fn test_short_strings_ignored() {
    let method = parse_method(
        r#"
Процедура Тест()
    Х = "SELECT";
    Y = "Test";
КонецПроцедуры
"#,
    );
    let result = lower_method(&method, false);

    assert_eq!(result.body.sdbl_exprs.len(), 0);
}

#[test]
fn test_multiple_queries_in_method() {
    let method = parse_method(
        r#"
Процедура МножественныеЗапросы()
    Запрос1 = "SELECT Ссылка FROM Справочник.Валюты";
    Запрос2 = "ВЫБРАТЬ Наименование ИЗ Справочник.Номенклатура";
    Результат1 = Запрос1.Выполнить();
    Результат2 = Запрос2.Выполнить();
КонецПроцедуры
"#,
    );
    let result = lower_method(&method, false);

    assert_eq!(result.body.sdbl_exprs.len(), 2);

    assert!(result.body.sdbl_exprs[0].2.query_text.contains("SELECT"));
    assert!(result.body.sdbl_exprs[1].2.query_text.contains("ВЫБРАТЬ"));
}

#[test]
fn test_if_else_duplicated_code_block() {
    let method = parse_method(
        r#"Процедура Тест()
    Если x = 1 Тогда
        А = 1;
        Б = 2;
    Иначе
        А = 1;
        Б = 2;
    КонецЕсли;
КонецПроцедуры"#,
    );
    let result = lower_method(&method, false);

    let diags: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| matches!(d, BodyDiagnostic::IfElseDuplicatedCodeBlock { .. }))
        .collect();
    assert_eq!(diags.len(), 1, "Should detect 1 duplicated code block");
}

#[test]
fn test_if_else_different_blocks() {
    let method = parse_method(
        r#"Процедура Тест()
    Если x = 1 Тогда
        А = 1;
    Иначе
        А = 2;
    КонецЕсли;
КонецПроцедуры"#,
    );
    let result = lower_method(&method, false);

    let diags: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| matches!(d, BodyDiagnostic::IfElseDuplicatedCodeBlock { .. }))
        .collect();
    assert_eq!(diags.len(), 0, "Different blocks should not trigger diagnostic");
}

#[test]
fn test_if_elsif_duplicated_code_block() {
    let method = parse_method(
        r#"Процедура Тест()
    Если x = 1 Тогда
        А = 1;
    ИначеЕсли x = 2 Тогда
        А = 1;
    КонецЕсли;
КонецПроцедуры"#,
    );
    let result = lower_method(&method, false);

    let diags: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| matches!(d, BodyDiagnostic::IfElseDuplicatedCodeBlock { .. }))
        .collect();
    assert_eq!(diags.len(), 1, "Should detect duplicated if/elsif blocks");
}

#[test]
fn test_if_else_empty_blocks_not_duplicated() {
    let method = parse_method(
        r#"Процедура Тест()
    Если x = 1 Тогда
    Иначе
    КонецЕсли;
КонецПроцедуры"#,
    );
    let result = lower_method(&method, false);

    let diags: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| matches!(d, BodyDiagnostic::IfElseDuplicatedCodeBlock { .. }))
        .collect();
    assert_eq!(diags.len(), 0, "Empty blocks should not trigger duplicate diagnostic");
}

#[test]
fn test_if_else_duplicated_range_correct() {
    let code = r#"Процедура Тест()
    Если x = 1 Тогда
        А = 1;
    Иначе
        А = 1;
    КонецЕсли;
КонецПроцедуры"#;
    let method = parse_method(code);
    let result = lower_method(&method, false);

    let diags: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| matches!(d, BodyDiagnostic::IfElseDuplicatedCodeBlock { .. }))
        .collect();
    assert_eq!(diags.len(), 1);

    if let BodyDiagnostic::IfElseDuplicatedCodeBlock { range } = diags[0] {
        let base = cfg_types::MethodOffset::new(method.text_range().start());
        let text = &code[range.lift(base)];
        assert!(text.contains("А = 1"), "Range should cover the duplicated statement");
    }
}

#[test]
fn test_preprocessor_split_expressions() {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId::from_raw(0);
    let code = r#"
// Split expression with region
Результат = Истина
#Область ЕщеОднаОбласть
 ИЛИ Истина;
#КонецОбласти

// Split expression with preprocessor
Результат2 = Истина
#Если ВебКлиент Тогда
 ИЛИ Ложь
#Иначе
 ИЛИ ЗначениеВыражения()
#КонецЕсли
 ИЛИ Истина;
"#;

    db.set_file_text(file_id, code);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    println!("=== PARSE ERRORS ===");
    for error in parse.errors() {
        println!("{:?}", error);
    }

    println!("\n=== SYNTAX TREE ===");
    println!("{:#?}", root);

    let result = super::super::lower_module_code(&root, None);

    println!("\n=== HIR LOWERING ===");
    println!("Body stmts: {}", result.body.body_stmts.len());
    for (idx, stmt_id) in result.body.body_stmts.iter().enumerate() {
        println!("  Stmt {}: {:?}", idx, result.body.stmts[*stmt_id]);
    }

    println!("\nBody exprs: {}", result.body.exprs.len());
    for (expr_id, expr) in result.body.exprs.iter() {
        println!("  {:?}: {:?}", expr_id, expr);
    }

    println!("\nDiagnostics: {}", result.diagnostics.len());
    for diag in &result.diagnostics {
        println!("  {:?}", diag);
    }
}

#[test]
fn recovery_lowers_bare_field_access_as_stmt_expr() {
    let method = parse_method(
        "Процедура Тест()
            Сп = Новый Массив;
            Сп.В
        КонецПроцедуры",
    );
    let result = super::lower_method(&method, false);

    assert!(
        result.body.body_stmts.len() >= 2,
        "expected at least assign + recovered stmt, got {}: {:?}",
        result.body.body_stmts.len(),
        result.body.body_stmts,
    );
    let last_stmt_id = *result.body.body_stmts.last().unwrap();
    let last_stmt = result.body.stmt_idx(last_stmt_id);
    let recovered_expr_id = match last_stmt {
        Stmt::Expr(id) => *id,
        other => panic!("last stmt should be Stmt::Expr (recovered), got {:?}", other),
    };

    let recovered_expr = result.body.expr_idx(recovered_expr_id);
    let base_id = match recovered_expr {
        Expr::Field { base, field } => {
            assert_eq!(
                field.as_str().fold_lower(),
                "в",
                "field name should round-trip through recovery",
            );
            *base
        }
        other => panic!("recovered expr should be Expr::Field, got {:?}", other),
    };

    let base_expr = result.body.expr_idx(base_id);
    match base_expr {
        Expr::Path(name) => assert_eq!(name.as_str().fold_lower(), "сп"),
        other => panic!("base should be Expr::Path, got {:?}", other),
    }

    use crate::ExprId;
    assert!(
        result.body.is_recovered(ExprId::from_idx(recovered_expr_id)),
        "field-access expr must be recovered",
    );
    assert!(
        result.body.is_recovered(ExprId::from_idx(base_id)),
        "receiver expr must be recovered (mark propagates recursively)",
    );
}

fn find_preproc_if(body: &crate::body::Body) -> &crate::hir::PreprocIfStmt {
    body.body_stmts
        .iter()
        .find_map(|stmt_id| match body.stmt_idx(*stmt_id) {
            Stmt::PreprocIf(preproc) => Some(preproc.as_ref()),
            _ => None,
        })
        .expect("body must contain a #Если statement")
}

fn assert_single_recovered_field_stmt(
    body: &crate::body::Body,
    branch: &[crate::hir::StmtIdx],
    expected_field: &str,
) {
    assert_eq!(branch.len(), 1, "branch must hold exactly the recovered stmt");
    let expr_id = match body.stmt_idx(branch[0]) {
        Stmt::Expr(id) => *id,
        other => panic!("branch stmt should be Stmt::Expr (recovered), got {:?}", other),
    };
    match body.expr_idx(expr_id) {
        Expr::Field { base, field } => {
            assert_eq!(field.as_str().fold_lower(), expected_field);
            match body.expr_idx(*base) {
                Expr::Path(name) => assert_eq!(name.as_str().fold_lower(), "сп"),
                other => panic!("base should be Expr::Path, got {:?}", other),
            }
        }
        other => panic!("recovered expr should be Expr::Field, got {:?}", other),
    }
    use crate::ExprId;
    assert!(
        body.is_recovered(ExprId::from_idx(expr_id)),
        "field-access expr inside the branch must be recovered",
    );
}

#[test]
fn recovery_lifts_bare_field_access_inside_preproc_then_branch() {
    let method = parse_method(
        "Процедура Тест()
            Сп = Новый Массив;
            #Если Сервер Тогда
            Сп.В
            #КонецЕсли
        КонецПроцедуры",
    );
    let result = super::lower_method(&method, false);

    let preproc = find_preproc_if(&result.body);
    assert_single_recovered_field_stmt(&result.body, &preproc.then_branch, "в");
}

#[test]
fn recovery_lifts_bare_field_access_inside_preproc_elsif_and_else_branches() {
    let method = parse_method(
        "Процедура Тест()
            Сп = Новый Массив;
            #Если Сервер Тогда
                А = 1;
            #ИначеЕсли Клиент Тогда
                Сп.В
            #Иначе
                Сп.Д
            #КонецЕсли
        КонецПроцедуры",
    );
    let result = super::lower_method(&method, false);

    let preproc = find_preproc_if(&result.body);
    assert_eq!(preproc.elsif_branches.len(), 1);
    assert_single_recovered_field_stmt(&result.body, &preproc.elsif_branches[0].2, "в");
    let else_branch = preproc.else_branch.as_ref().expect("fixture has #Иначе");
    assert_single_recovered_field_stmt(&result.body, else_branch, "д");
}

#[test]
fn recovery_does_not_lift_header_garbage_when_then_is_missing() {
    // Без `Тогда` восстановление заголовка съедает начало следующей строки:
    // ERROR с идентификатором стоит в позиции заголовка, не тела ветки, и
    // подниматься как statement не должен.
    let method = parse_method(
        "Процедура Тест()
            #Если Сервер
            А = 1;
            #КонецЕсли
        КонецПроцедуры",
    );
    let result = super::lower_method(&method, false);

    let preproc = find_preproc_if(&result.body);
    let lifted_header_ident =
        preproc.then_branch.iter().any(|stmt_id| match result.body.stmt_idx(*stmt_id) {
            Stmt::Expr(expr_id) => matches!(
                result.body.expr_idx(*expr_id),
                Expr::Path(name) if name.as_str().fold_lower() == "а"
            ),
            _ => false,
        });
    assert!(!lifted_header_ident, "header-position ERROR must not be lifted into the branch body",);
}

#[test]
fn recovery_ignores_malformed_condition_inside_pre_expr() {
    let method = parse_method(
        "Процедура Тест()
            #Если Сервер И Тогда
            А = 1;
            #КонецЕсли
        КонецПроцедуры",
    );
    let result = super::lower_method(&method, false);

    let preproc = find_preproc_if(&result.body);
    assert_eq!(
        preproc.then_branch.len(),
        1,
        "branch must hold only the assignment, nothing lifted from the header",
    );
    assert!(
        matches!(result.body.stmt_idx(preproc.then_branch[0]), Stmt::Assign { .. }),
        "the only branch stmt must be the assignment",
    );
}

#[test]
fn recovery_does_not_kick_in_for_well_formed_call_stmt() {
    let method = parse_method(
        "Процедура Тест()
            Сп = Новый Массив;
            Сп.Добавить(1);
        КонецПроцедуры",
    );
    let result = super::lower_method(&method, false);

    let any_recovered = result.body.exprs_iter().any(|(id, _)| result.body.is_recovered(id));
    assert!(!any_recovered, "well-formed call must not be flagged as recovered");
}

#[test]
fn inline_directives_leave_only_active_condition_in_hir() {
    let method = parse_method(
        r#"Функция Проверка(Данные)
    Если Не ЗначениеЗаполнено(Данные.Процент)
        #Вставка
        И Не Данные.Флаг
        #КонецВставки
        И Данные.Сумма > 0 Тогда
        Возврат Ложь;
    КонецЕсли;
    Возврат Истина;
КонецФункции"#,
    );
    let result = lower_method(&method, true);
    let if_stmt = match result.body.stmt_idx(result.body.body_stmts[0]) {
        Stmt::If(stmt) => stmt,
        other => panic!("expected If, got {other:?}"),
    };

    let mut fields = Vec::new();
    let mut and_count = 0;
    fn visit(
        body: &crate::body::Body,
        id: crate::hir::ExprIdx,
        fields: &mut Vec<String>,
        and_count: &mut usize,
    ) {
        match body.expr_idx(id) {
            Expr::BinaryOp { lhs, rhs, op } => {
                if *op == crate::hir::BinaryOp::And {
                    *and_count += 1;
                }
                visit(body, *lhs, fields, and_count);
                visit(body, *rhs, fields, and_count);
            }
            Expr::UnaryOp { expr, .. } => visit(body, *expr, fields, and_count),
            Expr::Call { callee, args } => {
                visit(body, *callee, fields, and_count);
                for &arg in args.iter() {
                    visit(body, arg, fields, and_count);
                }
            }
            Expr::Field { base, field } => {
                fields.push(field.as_str().to_string());
                visit(body, *base, fields, and_count);
            }
            _ => {}
        }
    }
    visit(&result.body, if_stmt.condition, &mut fields, &mut and_count);
    assert_eq!(and_count, 2, "the inserted conjunction must participate in the condition");
    assert_eq!(fields, ["Процент", "Флаг", "Сумма"]);
    assert!(!result.body.exprs_iter().any(|(_, expr)| matches!(expr, Expr::Missing)));
}

#[test]
fn inline_deleted_argument_is_absent_and_inserted_argument_is_present() {
    let method = parse_method(
        r#"Функция Собрать(Данные)
    Возврат Новый Структура(
        "Ссылка, Дата",
        #Удаление
        Данные.Ссылка,
        #КонецУдаления
        #Вставка
        Данные.НоваяСсылка,
        #КонецВставки
        Данные.Дата);
КонецФункции"#,
    );
    let result = lower_method(&method, true);
    let value = match result.body.stmt_idx(result.body.body_stmts[0]) {
        Stmt::Return { value: Some(value) } => *value,
        other => panic!("expected return value, got {other:?}"),
    };
    let args = match result.body.expr_idx(value) {
        Expr::New { type_name, args } => {
            assert_eq!(type_name.as_ref().map(|name| name.as_str()), Some("Структура"));
            args
        }
        other => panic!("expected New, got {other:?}"),
    };
    assert_eq!(args.len(), 3, "removed argument must not create an HIR slot");
    assert!(matches!(
        result.body.expr_idx(args[0]),
        Expr::Literal(Literal::String(text)) if text == "Ссылка, Дата"
    ));
    for (arg, expected) in [(args[1], "НоваяСсылка"), (args[2], "Дата")] {
        match result.body.expr_idx(arg) {
            Expr::Field { base, field } => {
                assert_eq!(field.as_str(), expected);
                assert!(matches!(
                    result.body.expr_idx(*base),
                    Expr::Path(name) if name.as_str() == "Данные"
                ));
            }
            other => panic!("expected active field argument, got {other:?}"),
        }
    }
    assert!(!result.body.exprs_iter().any(|(_, expr)| {
        matches!(expr, Expr::Field { field, .. } if field.as_str() == "Ссылка")
            || matches!(expr, Expr::Missing)
    }));
}

#[test]
fn torn_query_text_in_hir_matches_its_canonical_literal() {
    let with_directives = parse_method(
        r#"Функция ТекстЗапроса()
    Возврат "
    |ВЫБРАТЬ
    |    Т.Ссылка КАК Ссылка,
    #Удаление
    |    Т.Старое КАК Старое,
    #КонецУдаления
    #Вставка
    |    Т.Договор КАК Договор,
    #КонецВставки
    |    Т.Дата КАК Дата
    |ИЗ Справочник.Товары КАК Т";
КонецФункции"#,
    );
    let canonical = parse_method(
        r#"Функция ТекстЗапроса()
    Возврат "
    |ВЫБРАТЬ
    |    Т.Ссылка КАК Ссылка,
    |    Т.Договор КАК Договор,
    |    Т.Дата КАК Дата
    |ИЗ Справочник.Товары КАК Т";
КонецФункции"#,
    );
    let actual = lower_method(&with_directives, true);
    let expected = lower_method(&canonical, true);
    assert_eq!(actual.body.sdbl_exprs.len(), 1);
    assert_eq!(expected.body.sdbl_exprs.len(), 1);
    let actual_query = &actual.body.sdbl_exprs[0].2;
    let expected_query = &expected.body.sdbl_exprs[0].2;
    assert_eq!(actual_query.query_text, expected_query.query_text);
    assert!(actual_query.query_text.contains("Т.Договор"));
    assert!(!actual_query.query_text.contains("Старое"));
    assert!(!actual_query.query_text.contains("#Вставка"));
    assert!(actual_query.is_valid(), "active query must parse: {actual_query:?}");
    assert!(actual_query.literal_map.is_some(), "torn literal must carry its coordinate map");
}

fn detached_size_lines(code: &str) -> u32 {
    let method = crate::method_syntax::detach(&parse_method(code));
    crate::method_body::lower_detached_method(&method, method.kind() == SyntaxKind::FUNCTION_DEF)
        .size_lines
}

#[test]
fn method_size_is_line_difference_of_method_range() {
    assert_eq!(detached_size_lines("Процедура Тест() КонецПроцедуры"), 0);
    assert_eq!(detached_size_lines("Процедура Тест()\nКонецПроцедуры"), 1);
    assert_eq!(detached_size_lines("Процедура Тест()\n    А = 1;\nКонецПроцедуры"), 2);
}

#[test]
fn method_size_of_long_method_counts_padding_lines() {
    let mut code = String::from("Процедура Тест()\n\n");
    for _ in 0..202 {
        code.push_str("    А = 0;\n");
    }
    code.push_str("\nКонецПроцедуры");
    assert_eq!(detached_size_lines(&code), 205);
}

#[test]
fn method_size_without_line_index_is_zero() {
    let method = parse_method("Процедура Тест()\n    А = 1;\nКонецПроцедуры");
    assert_eq!(lower_method(&method, false).size_lines, 0);
}

#[test]
fn method_size_small_spans_in_both_languages_and_method_kinds() {
    for (start, end) in [
        ("Процедура", "КонецПроцедуры"),
        ("Функция", "КонецФункции"),
        ("Procedure", "EndProcedure"),
        ("Function", "EndFunction"),
    ] {
        for size in 0..=4 {
            let separator = if size == 0 { " ".to_owned() } else { "\n".repeat(size) };
            let code = format!("{start} Test(){separator}{end}");
            assert_eq!(detached_size_lines(&code), size as u32, "{code:?}");
        }
    }
}

#[test]
fn method_size_counts_node_lines_not_statements() {
    for (code, expected) in [
        ("Procedure Test()\n    A = 1; B = 2;\nEndProcedure", 2),
        ("Procedure Test()\n\n    A = 1;\nEndProcedure", 3),
        ("Procedure Test()\n    // comment\n    A = 1;\nEndProcedure", 3),
        ("&AtServer\nProcedure Test()\n    A = 1;\nEndProcedure", 3),
        ("&НаСервере\nПроцедура Тест()\n    // комментарий\n\nКонецПроцедуры", 4),
    ] {
        assert_eq!(detached_size_lines(code), expected, "{code:?}");
    }
}

#[test]
fn method_size_ignores_file_padding_and_line_ending_width() {
    for code in [
        "Procedure Test()\n    A = 1;\nEndProcedure",
        "Function Test()\n    Return 1;\nEndFunction",
    ] {
        for prefix in ["", "// outside\n\n\n"] {
            for suffix in ["", "\n\n// outside\n"] {
                for newline in ["\n", "\r\n"] {
                    let file = format!("{prefix}{code}{suffix}").replace('\n', newline);
                    assert_eq!(detached_size_lines(&file), 2, "{file:?}");
                    let method = parse_method(&file);
                    let index = std::sync::Arc::new(line_index::LineIndex::new(&file));
                    let result = super::lower_method_with_externals(
                        &method,
                        method.kind() == SyntaxKind::FUNCTION_DEF,
                        Some(index),
                    );
                    assert_eq!(result.size_lines, 2, "file-relative index: {file:?}");
                }
            }
        }
    }
}

#[test]
fn labels_spelled_as_keywords_are_lowered_by_name() {
    let method = parse_method(
        "Функция Тест()
            Для Н = 1 По 3 Цикл
                Перейти ~КонецЦикла;
            КонецЦикла;
        ~КонецЦикла:
            Перейти ~Возврат;
        ~Возврат:
            Возврат 1;
        КонецФункции",
    );
    let result = lower_method(&method, true);

    let names: Vec<String> = result
        .body
        .stmts_iter()
        .filter_map(|(_, stmt)| match stmt {
            Stmt::Goto(name) => Some(format!("goto {}", name.as_str())),
            Stmt::Label(name) => Some(format!("label {}", name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["goto КонецЦикла", "label КонецЦикла", "goto Возврат", "label Возврат"]);
}
