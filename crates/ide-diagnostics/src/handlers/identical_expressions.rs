use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::{BinaryOp, Body, BodySourceMap, Expr, ExprId, IdConversion, Literal, UnaryOp};
use std::collections::HashSet;
use stdx::case::CaseExt;
use syntax::{SyntaxKind, SyntaxNode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::IdenticalExpressions;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }

    check_body_exprs(ctx.body(), ctx.source_map(), acc, code, ctx);
    check_preprocessor_split_expressions(ctx, acc, code);
}

fn are_exprs_semantically_equal(lhs_id: ExprId, rhs_id: ExprId, body: &Body) -> bool {
    let lhs = body.expr(lhs_id);
    let rhs = body.expr(rhs_id);

    match (lhs, rhs) {
        (
            Expr::BinaryOp { lhs: l_lhs, rhs: l_rhs, op: l_op },
            Expr::BinaryOp { lhs: r_lhs, rhs: r_rhs, op: r_op },
        ) => {
            l_op == r_op
                && are_exprs_semantically_equal(
                    ExprId::from_idx(*l_lhs),
                    ExprId::from_idx(*r_lhs),
                    body,
                )
                && are_exprs_semantically_equal(
                    ExprId::from_idx(*l_rhs),
                    ExprId::from_idx(*r_rhs),
                    body,
                )
        }

        (Expr::UnaryOp { expr: l_expr, op: l_op }, Expr::UnaryOp { expr: r_expr, op: r_op }) => {
            l_op == r_op
                && are_exprs_semantically_equal(
                    ExprId::from_idx(*l_expr),
                    ExprId::from_idx(*r_expr),
                    body,
                )
        }

        (Expr::Literal(l_lit), Expr::Literal(r_lit)) => l_lit == r_lit,

        // BSL names are case-insensitive, so `Справочники` and `сПРАВОЧНИКИ` are the
        // same receiver. Comparing them exactly hid duplicate chains written in
        // different case — and now that a three-level chain lowers to `Field`/`Path`
        // instead of one folded node, it would hide those too.
        (Expr::Path(l_name), Expr::Path(r_name)) => l_name.eq_ignore_case(r_name),

        (
            Expr::Field { base: l_base, field: l_field },
            Expr::Field { base: r_base, field: r_field },
        ) => {
            l_field.eq_ignore_case(r_field)
                && are_exprs_semantically_equal(
                    ExprId::from_idx(*l_base),
                    ExprId::from_idx(*r_base),
                    body,
                )
        }

        (
            Expr::Index { base: l_base, index: l_index },
            Expr::Index { base: r_base, index: r_index },
        ) => {
            are_exprs_semantically_equal(ExprId::from_idx(*l_base), ExprId::from_idx(*r_base), body)
                && are_exprs_semantically_equal(
                    ExprId::from_idx(*l_index),
                    ExprId::from_idx(*r_index),
                    body,
                )
        }

        (
            Expr::Call { callee: l_callee, args: l_args },
            Expr::Call { callee: r_callee, args: r_args },
        ) => {
            if l_args.len() != r_args.len() {
                return false;
            }
            if !are_exprs_semantically_equal(
                ExprId::from_idx(*l_callee),
                ExprId::from_idx(*r_callee),
                body,
            ) {
                return false;
            }
            l_args.iter().zip(r_args.iter()).all(|(l_arg, r_arg)| {
                are_exprs_semantically_equal(
                    ExprId::from_idx(*l_arg),
                    ExprId::from_idx(*r_arg),
                    body,
                )
            })
        }

        (
            Expr::Ternary { condition: l_cond, then_expr: l_then, else_expr: l_else },
            Expr::Ternary { condition: r_cond, then_expr: r_then, else_expr: r_else },
        ) => {
            are_exprs_semantically_equal(ExprId::from_idx(*l_cond), ExprId::from_idx(*r_cond), body)
                && are_exprs_semantically_equal(
                    ExprId::from_idx(*l_then),
                    ExprId::from_idx(*r_then),
                    body,
                )
                && are_exprs_semantically_equal(
                    ExprId::from_idx(*l_else),
                    ExprId::from_idx(*r_else),
                    body,
                )
        }

        (
            Expr::New { type_name: l_type, args: l_args },
            Expr::New { type_name: r_type, args: r_args },
        ) => {
            if l_type != r_type || l_args.len() != r_args.len() {
                return false;
            }
            l_args.iter().zip(r_args.iter()).all(|(l_arg, r_arg)| {
                are_exprs_semantically_equal(
                    ExprId::from_idx(*l_arg),
                    ExprId::from_idx(*r_arg),
                    body,
                )
            })
        }

        _ => false,
    }
}

/// A DISPLAY rendering of an expression, keeping the case the author wrote — it goes
/// into diagnostic messages. Callers that need an identity key fold it themselves
/// (`operand_str.fold_lower()`); folding here would lowercase the message instead.
fn expr_to_string(expr_id: ExprId, body: &Body) -> String {
    let expr = body.expr(expr_id);
    match expr {
        Expr::BinaryOp { lhs, rhs, op } => {
            let lhs_str = expr_to_string(ExprId::from_idx(*lhs), body);
            let rhs_str = expr_to_string(ExprId::from_idx(*rhs), body);
            let op_str = match op {
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::Eq => "=",
                BinaryOp::Neq => "<>",
                BinaryOp::Lt => "<",
                BinaryOp::Le => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::Ge => ">=",
                BinaryOp::And => "and",
                BinaryOp::Or => "or",
            };
            format!("{} {} {}", lhs_str, op_str, rhs_str)
        }
        Expr::UnaryOp { expr, op } => {
            let expr_str = expr_to_string(ExprId::from_idx(*expr), body);
            let op_str = match op {
                UnaryOp::Not => "НЕ",
                UnaryOp::Neg => "-",
                UnaryOp::Plus => "+",
            };
            format!("{} {}", op_str, expr_str)
        }
        Expr::Literal(lit) => match lit {
            Literal::Bool(b) => b.to_string(),
            Literal::Number(n) => n.to_string(),
            Literal::String(s) => format!("\"{}\"", s),
            Literal::Date(d) => format!("'{}'", d),
            Literal::Undefined => "undefined".to_string(),
            Literal::Null => "null".to_string(),
        },
        Expr::Path(name) => name.as_str().to_string(),
        Expr::Field { base, field } => {
            format!("{}.{}", expr_to_string(ExprId::from_idx(*base), body), field.as_str())
        }
        Expr::Index { base, index } => {
            format!(
                "{}[{}]",
                expr_to_string(ExprId::from_idx(*base), body),
                expr_to_string(ExprId::from_idx(*index), body)
            )
        }
        Expr::Call { callee, args } => {
            let callee_str = expr_to_string(ExprId::from_idx(*callee), body);
            let args_str = args
                .iter()
                .map(|arg| expr_to_string(ExprId::from_idx(*arg), body))
                .collect::<Vec<_>>()
                .join(",");
            format!("{}({})", callee_str, args_str)
        }
        Expr::Ternary { condition, then_expr, else_expr } => {
            format!(
                "?({},{},{})",
                expr_to_string(ExprId::from_idx(*condition), body),
                expr_to_string(ExprId::from_idx(*then_expr), body),
                expr_to_string(ExprId::from_idx(*else_expr), body)
            )
        }
        Expr::New { type_name, args } => {
            let type_str = type_name.as_ref().map(|t| t.as_str()).unwrap_or("?");
            let args_str = args
                .iter()
                .map(|arg| expr_to_string(ExprId::from_idx(*arg), body))
                .collect::<Vec<_>>()
                .join(",");
            format!("new({}({}))", type_str.fold_lower(), args_str)
        }
        Expr::MethodCall { receiver, method, args } => {
            let receiver_str = expr_to_string(ExprId::from_idx(*receiver), body);
            let args_str = args
                .iter()
                .map(|arg| expr_to_string(ExprId::from_idx(*arg), body))
                .collect::<Vec<_>>()
                .join(",");
            format!("{}.{}({})", receiver_str, method.as_str().fold_lower(), args_str)
        }
        Expr::Array(elements) => {
            let elements_str = elements
                .iter()
                .map(|elem| expr_to_string(ExprId::from_idx(*elem), body))
                .collect::<Vec<_>>()
                .join(",");
            format!("[{}]", elements_str)
        }
        Expr::Await { expr } => {
            format!("await({})", expr_to_string(ExprId::from_idx(*expr), body))
        }
        Expr::Missing => "<missing>".to_string(),
    }
}

fn is_statement_expr(expr_id: ExprId, body: &Body, _source_map: &BodySourceMap) -> bool {
    for stmt_id in body.body_stmts() {
        let stmt = body.stmt(stmt_id);
        match stmt {
            hir::Stmt::Expr(expr) if ExprId::from_idx(*expr) == expr_id => {
                return true;
            }
            hir::Stmt::Assign { value, .. } if ExprId::from_idx(*value) == expr_id => {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn check_body_exprs(
    body: &Body,
    source_map: &BodySourceMap,
    diagnostics: &mut Vec<Diagnostic<LocalRange>>,
    code: DiagnosticCode,
    ctx: &BodyContext,
) {
    for (expr_id, expr) in body.exprs_iter() {
        if let Expr::BinaryOp { lhs, rhs, op } = expr {
            check_binary_expr_hir(
                expr_id,
                ExprId::from_idx(*lhs),
                ExprId::from_idx(*rhs),
                *op,
                body,
                source_map,
                diagnostics,
                code,
                ctx,
            );
        }
    }
}

#[allow(clippy::too_many_arguments, reason = "HIR binary-expression check needs all inputs")]
fn check_binary_expr_hir(
    expr_id: ExprId,
    lhs: ExprId,
    rhs: ExprId,
    op: BinaryOp,
    body: &Body,
    source_map: &BodySourceMap,
    diagnostics: &mut Vec<Diagnostic<LocalRange>>,
    code: DiagnosticCode,
    ctx: &BodyContext,
) {
    if op == BinaryOp::Eq && is_statement_expr(expr_id, body, source_map) {
        return;
    }

    if matches!(op, BinaryOp::Add | BinaryOp::Mul) {
        return;
    }

    if matches!(op, BinaryOp::And | BinaryOp::Or) {
        if !is_nested_in_logical_chain(expr_id, op, body) {
            check_logical_chain_hir(expr_id, op, body, source_map, diagnostics, code, ctx);
        }
        return;
    }

    if are_exprs_semantically_equal(lhs, rhs, body) {
        if op == BinaryOp::Div && is_popular_division_hir(lhs, body, ctx) {
            return;
        }

        let Some(range) = source_map.expr_range(expr_id) else {
            return;
        };

        let op_text = match op {
            BinaryOp::Eq => "=",
            BinaryOp::Neq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::Sub => "-",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            _ => "?",
        };

        let lhs_text = expr_to_string(lhs, body);

        diagnostics.push(Diagnostic {
            code,
            message: format!(
                "Одинаковые выражения '{}' с обеих сторон оператора '{}'",
                lhs_text, op_text
            ),
            severity: ctx.severity(code),
            range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

fn is_popular_division_hir(expr_id: ExprId, body: &Body, ctx: &BodyContext) -> bool {
    let popular_divisors = ctx
        .config
        .get_string_param(DiagnosticCode::IdenticalExpressions, "popularDivisors")
        .unwrap_or_else(|| "60, 1024".to_string());

    if popular_divisors.trim().is_empty() {
        return false;
    }

    let divisors: HashSet<String> =
        popular_divisors.split(',').map(|s| s.trim().to_string()).collect();

    let expr = body.expr(expr_id);
    if let Expr::Literal(Literal::Number(n)) = expr {
        let text = n.to_string();
        if divisors.contains(&text) {
            return true;
        }
    }

    false
}

fn is_nested_in_logical_chain(expr_id: ExprId, op: BinaryOp, body: &Body) -> bool {
    for (_parent_id, parent_expr) in body.exprs_iter() {
        if let Expr::BinaryOp { lhs, rhs, op: parent_op } = parent_expr {
            if parent_op == &op
                && (ExprId::from_idx(*lhs) == expr_id || ExprId::from_idx(*rhs) == expr_id)
            {
                return true;
            }
        }
    }
    false
}

fn check_logical_chain_hir(
    root_expr_id: ExprId,
    chain_op: BinaryOp,
    body: &Body,
    source_map: &BodySourceMap,
    diagnostics: &mut Vec<Diagnostic<LocalRange>>,
    code: DiagnosticCode,
    ctx: &BodyContext,
) {
    let mut operands = Vec::new();
    collect_logical_chain_hir(root_expr_id, chain_op, body, &mut operands);

    let mut seen = HashSet::new();
    let mut duplicate = None;

    for &operand_id in &operands {
        let operand_str = expr_to_string(operand_id, body);
        let lowered = operand_str.fold_lower().replace(' ', "");
        let normalized = normalize_operand_hir(&lowered);

        if !seen.insert(normalized) {
            duplicate = Some(operand_str);
            break;
        }
    }

    if let Some(dup_text) = duplicate {
        if let Some(range) = source_map.expr_range(root_expr_id) {
            let op_text = match chain_op {
                BinaryOp::And => "И",
                BinaryOp::Or => "ИЛИ",
                _ => "?",
            };

            diagnostics.push(Diagnostic {
                code,
                message: format!(
                    "Повторяющееся выражение '{}' в цепочке оператора '{}'",
                    dup_text, op_text
                ),
                severity: ctx.severity(code),
                range,
                tags: ctx.tags(code),
                fixes: vec![],
            });
        }
    }
}

fn collect_logical_chain_hir(
    expr_id: ExprId,
    chain_op: BinaryOp,
    body: &Body,
    operands: &mut Vec<ExprId>,
) {
    let expr = body.expr(expr_id);

    if let Expr::BinaryOp { lhs, rhs, op } = expr {
        if op == &chain_op {
            collect_logical_chain_hir(ExprId::from_idx(*lhs), chain_op, body, operands);
            collect_logical_chain_hir(ExprId::from_idx(*rhs), chain_op, body, operands);
            return;
        }
    }

    operands.push(expr_id);
}

fn normalize_operand_hir(text: &str) -> String {
    for op in &["<>", "="] {
        if let Some(pos) = text.find(op) {
            let left = &text[..pos];
            let right = &text[pos + op.len()..];

            let mut parts = [left, right];
            parts.sort();

            return format!("{}{}{}", parts[0], op, parts[1]);
        }
    }

    text.to_string()
}

fn normalize_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .trim_matches(|c| c == '(' || c == ')')
        .to_string()
}

fn check_preprocessor_split_expressions(
    ctx: &BodyContext,
    diagnostics: &mut Vec<Diagnostic<LocalRange>>,
    code: DiagnosticCode,
) {
    for node in ctx.nodes() {
        if node.kind() != SyntaxKind::ASSIGN_STMT {
            continue;
        }

        let Some(next_sibling) = node.next_sibling() else {
            continue;
        };

        if !matches!(next_sibling.kind(), SyntaxKind::PRE_REGION_DIR | SyntaxKind::PRE_IF_DIR) {
            continue;
        }

        if !has_split_logical_operator(&next_sibling) {
            let mut found_split = false;
            let mut current_sibling = next_sibling.next_sibling();
            while let Some(sibling) = current_sibling {
                if sibling.kind() == SyntaxKind::CALL_STMT {
                    if has_split_logical_operator(&sibling) {
                        found_split = true;
                        break;
                    }
                    current_sibling = sibling.next_sibling();
                } else {
                    break;
                }
            }
            if !found_split {
                continue;
            }
        }

        let Some(assign_rhs) = extract_assign_rhs(&node) else {
            continue;
        };

        let mut all_operands = vec![normalize_text(&assign_rhs)];
        all_operands.extend(extract_preprocessor_operands(&next_sibling));

        let mut current_sibling = next_sibling.next_sibling();
        while let Some(sibling) = current_sibling {
            if sibling.kind() == SyntaxKind::CALL_STMT {
                all_operands.extend(extract_preprocessor_operands(&sibling));
                current_sibling = sibling.next_sibling();
            } else {
                break;
            }
        }

        if all_operands.len() < 2 {
            continue;
        }

        let mut seen = HashSet::new();
        for operand in &all_operands {
            if !seen.insert(operand.clone()) {
                diagnostics.push(Diagnostic {
                    code,
                    message: format!(
                        "Повторяющееся выражение '{}' в выражении, разбитом препроцессорной директивой",
                        operand
                    ),
                    severity: ctx.severity(code),
                    range: ctx.range_of(&node),
                    tags: ctx.tags(code),
                    fixes: vec![],
                });
                break;
            }
        }
    }
}

fn has_split_logical_operator(node: &SyntaxNode) -> bool {
    for descendant in node.descendants() {
        if descendant.kind() == SyntaxKind::ERROR {
            for token in descendant.children_with_tokens() {
                if let Some(token) = token.as_token() {
                    if matches!(token.kind(), SyntaxKind::KW_OR | SyntaxKind::KW_AND) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn extract_assign_rhs(assign_stmt: &SyntaxNode) -> Option<String> {
    for child in assign_stmt.children() {
        if child.kind() == SyntaxKind::EXPR {
            return Some(child.text().to_string());
        }
    }
    None
}

fn extract_preprocessor_operands(prep_dir: &SyntaxNode) -> Vec<String> {
    let mut operands = Vec::new();

    fn collect_all_operands(node: &SyntaxNode, operands: &mut Vec<String>) {
        if node.kind() == SyntaxKind::CALL_STMT {
            let expr_text: String = node
                .descendants()
                .filter(|n| n.kind() != SyntaxKind::ERROR)
                .filter(|n| matches!(n.kind(), SyntaxKind::LITERAL | SyntaxKind::IDENT))
                .map(|n| n.text().to_string())
                .collect::<Vec<_>>()
                .join("");

            if !expr_text.is_empty() {
                operands.push(normalize_text(&expr_text));
            }
        }

        if matches!(node.kind(), SyntaxKind::LITERAL | SyntaxKind::IDENT) {
            operands.push(normalize_text(&node.text().to_string()));
        }

        for child in node.children() {
            collect_all_operands(&child, operands);
        }
    }

    collect_all_operands(prep_dir, &mut operands);

    let mut seen = HashSet::new();
    operands.into_iter().filter(|op| seen.insert(op.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::check_body;
    use crate::test_utils::check_body_diagnostic;
    #[test]
    fn test_identical_comparison() {
        let code = r#"
Функция Тест()
    Если x = x Тогда
        Возврат Истина;
    КонецЕсли;
    Возврат Ложь;
КонецФункции
"#;

        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Expected 1 diagnostic, found {}", diagnostics.len());
        assert!(diagnostics[0].message.contains("x"));
    }

    #[test]
    fn test_different_expressions() {
        let code = r#"
Функция Тест()
    Если x = y Тогда
        Возврат Истина;
    КонецЕсли;
    Возврат Ложь;
КонецФункции
"#;

        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 0);
    }

    #[test]
    fn test_identical_arithmetic() {
        let code = r#"
Процедура Тест()
    Результат = a - a;
КонецПроцедуры
"#;

        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("-"));
    }

    #[test]
    fn test_addition_multiplication_allowed() {
        let code = r#"
Процедура Тест()
    Результат = x + x;  // OK
    Результат = x * x;  // OK
КонецПроцедуры
"#;

        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 0);
    }

    /// Регистр в BSL не значим, поэтому две записи одного имени — одно выражение.
    /// Цепочка проверяется отдельно от голого имени: её сравнение идёт по звеньям
    /// (`Field` над `Path`), и свёртка регистра нужна на каждом звене.
    #[test]
    fn a_chain_written_in_another_case_is_the_same_expression() {
        // Сравнение сторон оператора идёт через `are_exprs_semantically_equal` —
        // маршрут, отличный от свёртки строкового ключа у цепочки `И`, поэтому
        // проверять регистр надо именно на нём.
        let code = "Функция Тест()\n    \
                    Возврат Справочники.Товары.Найти() = сПРАВОЧНИКИ.тОВАРЫ.нАЙТИ();\n\
                    КонецФункции\n";
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "same expression on both sides: {diagnostics:#?}");

        // Контроль: РАЗНЫЕ имена по-прежнему разные, иначе проверка выше зелена сама
        // по себе.
        let distinct = "Функция Тест()\n    \
                        Возврат Справочники.Товары.Найти() = Справочники.Услуги.Найти();\n\
                        КонецФункции\n";
        assert!(check_body_diagnostic(distinct, check_body).is_empty());
    }

    #[test]
    fn test_logical_chain() {
        let code = r#"
Функция Тест()
    Возврат А И Б И Б;
КонецФункции
"#;

        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(
            diagnostics.len(),
            1,
            "Expected 1 diagnostic for duplicate Б, found {}",
            diagnostics.len()
        );
        assert!(
            diagnostics[0].message.contains("Б"),
            "Message should contain 'Б' (original case from expr_to_string)"
        );
    }

    #[test]
    fn test_fixture_self_assignment_skipped() {
        let code = r#"Функция Проверка()
    Перем1 = Перем1;
    Возврат Истина;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 0, "Self-assignment should not be flagged");
    }

    #[test]
    fn test_fixture_identical_neq_triggers() {
        let code = r#"Функция Проверка()
    Если Перем2 <> Перем2 Тогда
        Возврат Истина;
    КонецЕсли;
    Возврат Ложь;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Identical <> should trigger");
        assert!(diagnostics[0].message.contains("<>"));
    }

    #[test]
    fn test_fixture_identical_gt_in_return_triggers() {
        let code = r#"Функция Проверка()
    Возврат Перем3 > Перем3;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Identical > should trigger");
    }

    #[test]
    fn test_fixture_division_triggers_multiplication_skipped() {
        let code = r#"Процедура Проверка()
    Перем4 = Перем5 + Перем5;
    Перем6 = Перем7 / Перем7;
КонецПроцедуры
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "/ triggers, + does not");
        assert!(diagnostics[0].message.contains("/"));
    }

    #[test]
    fn test_fixture_identical_complex_expr_eq_triggers() {
        let code = r#"Функция Проверка()
    Если (Перем8 + Перем9 + Перем10) = (Перем8 + Перем9 + Перем10) Тогда
        Возврат Истина;
    КонецЕсли;
    Возврат Ложь;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Identical complex expressions should trigger");
    }

    #[test]
    fn test_fixture_identical_neq_in_return_triggers() {
        let code = r#"Функция Проверка()
    Возврат Перем11 <> Перем11;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Identical <> in return should trigger");
    }

    #[test]
    fn test_fixture_duplicate_in_or_chain_triggers() {
        let code = r#"Функция Проверка()
    Если УсловиеВыполняется() ИЛИ УсловиеВыполняется() ИЛИ УсловиеВтороеВыполняется() Тогда
        Возврат Истина;
    КонецЕсли;
    Возврат Ложь;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Duplicate in OR chain should trigger");
    }

    #[test]
    fn test_fixture_identical_lt_in_return_triggers() {
        let code = r#"Функция Проверка()
    Возврат Перем12 < Перем12;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Identical < should trigger");
    }

    #[test]
    fn test_fixture_duplicate_in_and_chain_triggers() {
        let code = r#"Функция Проверка()
    Если (Перем13 = 0) И (Перем13 = 0) Тогда
        Возврат Истина;
    КонецЕсли;
    Возврат Ложь;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Duplicate in AND chain should trigger");
    }

    #[test]
    fn test_fixture_duplicate_and_in_return_triggers() {
        let code = r#"Функция Проверка()
    Возврат Перем14 И Перем15 И Перем15;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Duplicate И in return chain should trigger");
    }

    #[test]
    fn test_fixture_subtraction_triggers_multiplication_skipped() {
        let code = r#"Процедура Проверка()
    Результат = Перем16 - Перем16;
    Результат = Результат * Результат;
КонецПроцедуры
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "- triggers, * does not");
        assert!(diagnostics[0].message.contains("-"));
    }

    #[test]
    fn test_fixture_mixed_and_or_no_duplicates() {
        let code = r#"Функция Проверка()
    Возврат Перем18 И Перем19 ИЛИ Перем18 И Перем20;
КонецФункции
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 0, "No duplicates in mixed И/ИЛИ chain");
    }

    #[test]
    fn test_fixture_module_level_complex_and_in_or() {
        let code = r#"
Б = 0;
С = (А = 1) И (Б = 1) ИЛИ (А = 1) И (Б = 1);
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Duplicate AND in OR chain at module level");
    }

    #[test]
    fn test_fixture_module_level_or_duplicate() {
        let code = r#"
Если А = 1 ИЛИ А = 1 Тогда
    Б = 1;
КонецЕсли;
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Duplicate А = 1 in module-level OR");
    }

    #[test]
    fn test_fixture_module_level_transitive_or() {
        let code = r#"
ИначеЕсли 1 = А ИЛИ А = 1 Тогда
    Б = 11;
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Transitive 1=А vs А=1 in OR chain");
    }

    #[test]
    fn test_fixture_chained_assignment_duplicate() {
        let code = r#"
Б = А = 12 ИЛИ А = 13 ИЛИ А = 12;
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Duplicate А = 12 in chained OR");
    }

    #[test]
    fn test_fixture_preprocessor_region_duplicate() {
        let code = r#"
Результат = Истина
#Область ЕщеОднаОбласть
 ИЛИ Истина;
#КонецОбласти
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Duplicate across #Область should trigger");
    }

    #[test]
    fn test_fixture_preprocessor_if_duplicate() {
        let code = r#"
Результат = Истина
#Если ВебКлиент Тогда
 ИЛИ Ложь
#Иначе
 ИЛИ ЗначениеВыражения()
#КонецЕсли
 ИЛИ Истина;
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Истина repeated across #Если should trigger");
    }

    #[test]
    fn test_fixture_preprocessor_no_duplicate() {
        let code = r#"
Результат = ЗначениеВыражения()
#Если ВебКлиент Тогда
 ИЛИ Ложь
#Иначе
 ИЛИ Отказ
#КонецЕсли
 ИЛИ Истина;
"#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 0, "No duplicates across #Если");
    }

    #[test]
    fn test_simple_or_chain() {
        let code = r#"
Функция Тест()
    Если А = 1 ИЛИ А = 1 Тогда
        Возврат Истина;
    КонецЕсли;
КонецФункции
"#;

        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(
            diagnostics.len(),
            1,
            "Should find duplicate А = 1 in OR chain, found {}",
            diagnostics.len()
        );
    }

    #[test]
    fn test_transitive_comparison() {
        let code = r#"
Функция Тест()
    Если 1 = А ИЛИ А = 1 Тогда
        Возврат Истина;
    КонецЕсли;
КонецФункции
"#;

        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(
            diagnostics.len(),
            1,
            "Should find transitive duplicate (1 = А vs А = 1), found {}",
            diagnostics.len()
        );
    }

    #[test]
    fn test_complex_and_in_or() {
        let code = r#"
С = (А = 1) И (Б = 1) ИЛИ (А = 1) И (Б = 1);
"#;

        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "Should find duplicate AND sub-expression in OR chain");
    }

    #[test]
    fn test_chained_assignment_with_or() {
        let code = r#"
Б = А = 12 ИЛИ А = 13 ИЛИ А = 12;
"#;

        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(
            diagnostics.len(),
            1,
            "Should find duplicate А = 12 in chained assignment with OR"
        );
    }

    #[test]
    fn test_preprocessor_region() {
        let code = r#"
Результат = Истина
#Область Тест
 ИЛИ Истина;
#КонецОбласти
"#;

        let _diagnostics = check_body_diagnostic(code, check_body);
    }

    #[test]
    fn test_preprocessor_if() {
        let code = r#"
Результат = Истина
#Если ВебКлиент Тогда
 ИЛИ Ложь
#Иначе
 ИЛИ ЗначениеВыражения()
#КонецЕсли
 ИЛИ Истина;
"#;

        let _diagnostics = check_body_diagnostic(code, check_body);
    }

    #[test]
    fn a_negative_number_literal_is_not_the_positive_one() {
        let different = "Функция Тест(Х)\n    Возврат Х = 1 ИЛИ Х = -1;\nКонецФункции\n";
        let diagnostics = check_body_diagnostic(different, check_body);
        assert!(diagnostics.is_empty(), "1 and -1 are different literals: {diagnostics:#?}");

        // Контроль: одинаковые отрицательные литералы по-прежнему одинаковы, иначе
        // проверка выше зелена сама по себе.
        let same = "Функция Тест(Х)\n    Возврат Х = -1 ИЛИ Х = -1;\nКонецФункции\n";
        assert_eq!(check_body_diagnostic(same, check_body).len(), 1);
    }
}
