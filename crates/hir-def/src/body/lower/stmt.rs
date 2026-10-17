use intern::NormName;
use stdx::case::CaseExt;
use syntax::{
    ast::{AstNode, PreIfDir},
    ast_utils::active_children,
    SyntaxKind, SyntaxNode,
};
use text_size::TextRange;

use crate::body::BodyDiagnostic;
use crate::hir::{Binding, BindingIdx, Expr, Stmt, StmtIdx};
use crate::{Name, StmtId};
use cfg_types::IdConversion;

use super::control_flow::is_statement_node;
use super::diagnostics::{
    check_commit_transaction_in_try, check_duplicated_code_blocks,
    check_rollback_transaction_in_try, extend_range_with_semicolon,
    is_global_begin_transaction_call, is_global_commit_transaction_call,
    is_global_rollback_transaction_call, is_inside_try_body,
};
use super::expr::{exprs_are_equal, lower_expr_node};
use super::preproc::lower_preproc_if;
use super::LoweringCtx;

fn find_if_then_range(if_stmt: &SyntaxNode) -> TextRange {
    let mut start = None;
    let mut end = None;

    for token in if_stmt.descendants_with_tokens().filter_map(|el| el.into_token()) {
        if matches!(token.kind(), SyntaxKind::KW_IF) && start.is_none() {
            start = Some(token.text_range().start());
        }
        if matches!(token.kind(), SyntaxKind::KW_THEN) && start.is_some() && end.is_none() {
            end = Some(token.text_range().end());
            break;
        }
    }

    match (start, end) {
        (Some(s), Some(e)) => TextRange::new(s, e),
        _ => if_stmt.text_range(),
    }
}

fn find_elsif_then_range(elsif_clause: &SyntaxNode) -> TextRange {
    let mut start = None;
    let mut end = None;

    for token in elsif_clause.descendants_with_tokens().filter_map(|el| el.into_token()) {
        if matches!(token.kind(), SyntaxKind::KW_ELSIF) && start.is_none() {
            start = Some(token.text_range().start());
        }
        if matches!(token.kind(), SyntaxKind::KW_THEN) && start.is_some() && end.is_none() {
            end = Some(token.text_range().end());
            break;
        }
    }

    match (start, end) {
        (Some(s), Some(e)) => TextRange::new(s, e),
        _ => elsif_clause.text_range(),
    }
}

fn find_else_range(else_clause: &SyntaxNode) -> TextRange {
    for token in else_clause.descendants_with_tokens().filter_map(|el| el.into_token()) {
        if matches!(token.kind(), SyntaxKind::KW_ELSE) {
            return token.text_range();
        }
    }
    else_clause.text_range()
}

fn normalize_condition(condition: &str) -> String {
    let mut result = String::new();
    let mut in_string = false;

    for ch in condition.chars() {
        if ch == '"' {
            in_string = !in_string;
            result.push(ch);
            continue;
        }

        if ch.is_whitespace() && !in_string {
            continue;
        }

        if in_string {
            result.push(ch);
        } else {
            result.push(ch.to_lowercase().next().unwrap_or(ch));
        }
    }

    result
}

fn check_duplicated_conditions(ctx: &mut LoweringCtx, condition_nodes: &[SyntaxNode]) {
    if condition_nodes.len() < 2 {
        return;
    }

    use std::collections::HashMap;

    let mut condition_map: HashMap<String, Vec<(usize, &SyntaxNode)>> = HashMap::new();

    for (i, node) in condition_nodes.iter().enumerate() {
        let text = node.text().to_string();
        let normalized = normalize_condition(&text);
        condition_map.entry(normalized).or_default().push((i, node));
    }

    for (_normalized_text, occurrences) in condition_map {
        if occurrences.len() > 1 {
            for (_idx, node) in occurrences.iter().skip(1) {
                let range = node.text_range();
                ctx.emit(BodyDiagnostic::IfElseDuplicatedCondition {
                    first_occurrence_index: occurrences[0].0,
                    range,
                });
            }
        }
    }
}

fn find_while_do_range(while_stmt: &SyntaxNode) -> TextRange {
    let mut start = None;
    let mut end = None;

    for token in while_stmt.descendants_with_tokens().filter_map(|el| el.into_token()) {
        if matches!(token.kind(), SyntaxKind::KW_WHILE) && start.is_none() {
            start = Some(token.text_range().start());
        }
        if matches!(token.kind(), SyntaxKind::KW_DO) && start.is_some() && end.is_none() {
            end = Some(token.text_range().end());
            break;
        }
    }

    match (start, end) {
        (Some(s), Some(e)) => TextRange::new(s, e),
        _ => while_stmt.text_range(),
    }
}

fn find_for_do_range(for_stmt: &SyntaxNode) -> TextRange {
    let mut start = None;
    let mut end = None;

    for token in for_stmt.descendants_with_tokens().filter_map(|el| el.into_token()) {
        if matches!(token.kind(), SyntaxKind::KW_FOR) && start.is_none() {
            start = Some(token.text_range().start());
        }
        if matches!(token.kind(), SyntaxKind::KW_DO) && start.is_some() && end.is_none() {
            end = Some(token.text_range().end());
            break;
        }
    }

    match (start, end) {
        (Some(s), Some(e)) => TextRange::new(s, e),
        _ => for_stmt.text_range(),
    }
}

fn find_foreach_do_range(foreach_stmt: &SyntaxNode) -> TextRange {
    let mut start = None;
    let mut end = None;

    for token in foreach_stmt.descendants_with_tokens().filter_map(|el| el.into_token()) {
        if matches!(token.kind(), SyntaxKind::KW_FOR) && start.is_none() {
            start = Some(token.text_range().start());
        }
        if matches!(token.kind(), SyntaxKind::KW_DO) && start.is_some() && end.is_none() {
            end = Some(token.text_range().end());
            break;
        }
    }

    match (start, end) {
        (Some(s), Some(e)) => TextRange::new(s, e),
        _ => foreach_stmt.text_range(),
    }
}

pub(crate) fn lower_params(ctx: &mut LoweringCtx, param_list: &SyntaxNode) -> Vec<BindingIdx> {
    let mut params = Vec::new();

    for param in param_list.children().filter(|n| n.kind() == SyntaxKind::PARAM) {
        if let Some(binding_id) = lower_param(ctx, &param) {
            params.push(binding_id);
        }
    }

    params
}

fn lower_param(ctx: &mut LoweringCtx, param: &SyntaxNode) -> Option<BindingIdx> {
    let name_token = param
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::IDENT)?;

    let is_val = param
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|tok| tok.kind() == SyntaxKind::KW_VAL);

    ctx.register_param(name_token.text());

    if !is_val {
        ctx.by_ref_param_names.insert(NormName::intern(name_token.text()));
    }

    let default_value = param
        .children()
        .find(|n| n.kind() == SyntaxKind::EXPR)
        .map(|expr_node| lower_expr_node(ctx, &expr_node));

    let binding = if let Some(default_expr_id) = default_value {
        Binding::with_default(Name::new(name_token.text()), is_val, default_expr_id)
    } else {
        Binding::new(Name::new(name_token.text()), is_val)
    };

    let binding_id = ctx.alloc_binding(binding, name_token.text_range());

    if is_val {
        ctx.by_value_params.insert(name_token.text().fold_lower(), binding_id);
    }

    let name_lower = name_token.text().fold_lower();
    if name_lower == "отказ" || name_lower == "cancel" {
        ctx.cancel_params.insert(name_lower);
    }

    Some(binding_id)
}

pub(crate) fn lower_stmt_list(ctx: &mut LoweringCtx, stmt_list: &SyntaxNode) -> Vec<StmtIdx> {
    lower_stmt_list_with_unreachable(ctx, stmt_list, true)
}

pub(super) fn lower_stmt_list_with_unreachable(
    ctx: &mut LoweringCtx,
    stmt_list: &SyntaxNode,
    emit_diagnostics: bool,
) -> Vec<StmtIdx> {
    let mut stmts = Vec::new();

    let mut pending_begin_transaction: Option<SyntaxNode> = None;

    let is_top_level = !is_inside_try_body(stmt_list);

    for child in stmt_list.children() {
        if child.kind() == SyntaxKind::PRE_IF_DIR {
            if emit_diagnostics && pending_begin_transaction.is_some() {
                if pre_if_all_branches_open_with_try(&child) {
                    pending_begin_transaction = None;
                } else if let Some(pending_node) = pending_begin_transaction.take() {
                    let extended_range =
                        extend_range_with_semicolon(&pending_node, pending_node.text_range());
                    ctx.emit(BodyDiagnostic::BeginTransactionBeforeTryCatch {
                        range: extended_range,
                    });
                }
            }

            if let Some(stmt) = lower_preproc_if(ctx, &child) {
                let stmt_id = ctx.alloc_stmt(stmt, child.text_range());
                stmts.push(stmt_id);
            }
            continue;
        }
        // Flat region markers are skipped; their statements are siblings here.
        if !is_statement_node(&child) {
            continue;
        }

        if emit_diagnostics && child.kind() == SyntaxKind::TRY_STMT {
            pending_begin_transaction = None;
        }

        if emit_diagnostics {
            let is_begin_trans = is_global_begin_transaction_call(&child);

            if is_begin_trans {
                if let Some(pending_node) = pending_begin_transaction.take() {
                    let extended_range =
                        extend_range_with_semicolon(&pending_node, pending_node.text_range());
                    ctx.emit(BodyDiagnostic::BeginTransactionBeforeTryCatch {
                        range: extended_range,
                    });
                }

                if is_inside_try_body(&child) {
                    let extended_range = extend_range_with_semicolon(&child, child.text_range());
                    ctx.emit(BodyDiagnostic::BeginTransactionBeforeTryCatch {
                        range: extended_range,
                    });
                } else {
                    pending_begin_transaction = Some(child.clone());
                }
            } else if child.kind() == SyntaxKind::TRY_STMT {
                pending_begin_transaction = None;
            } else if pending_begin_transaction.is_some() {
                if let Some(pending_node) = pending_begin_transaction.take() {
                    let extended_range =
                        extend_range_with_semicolon(&pending_node, pending_node.text_range());
                    ctx.emit(BodyDiagnostic::BeginTransactionBeforeTryCatch {
                        range: extended_range,
                    });
                }
            }

            if is_top_level && is_global_commit_transaction_call(&child) {
                let extended_range = extend_range_with_semicolon(&child, child.text_range());
                ctx.emit(BodyDiagnostic::CommitTransactionOutsideTryCatch {
                    range: extended_range,
                });
            }

            if is_top_level && is_global_rollback_transaction_call(&child) {
                let extended_range = extend_range_with_semicolon(&child, child.text_range());
                ctx.emit(BodyDiagnostic::WrongUseOfRollbackTransactionMethod {
                    range: extended_range,
                });
            }
        }

        if let Some(stmt_id) = lower_stmt(ctx, &child) {
            stmts.push(stmt_id);

            if emit_diagnostics && !should_skip_one_statement_per_line(&child) {
                ctx.track_statement_line(child.text_range());
            }

            if emit_diagnostics
                && !should_skip_semicolon_check(&child)
                && !has_trailing_semicolon(&child)
            {
                let range = last_token_range(&child);
                ctx.emit(BodyDiagnostic::MissingSemicolon { range });
            }
        }
    }

    if emit_diagnostics {
        if let Some(pending_node) = pending_begin_transaction {
            let extended_range =
                extend_range_with_semicolon(&pending_node, pending_node.text_range());
            ctx.emit(BodyDiagnostic::BeginTransactionBeforeTryCatch { range: extended_range });
        }
    }

    stmts
}

pub(crate) fn pre_if_all_branches_open_with_try(node: &SyntaxNode) -> bool {
    let Some(pre_if) = PreIfDir::cast(node.clone()) else {
        return false;
    };

    if !branch_opens_with_try(pre_if.then_body_nodes()) {
        return false;
    }

    for elsif in pre_if.elsif_clauses() {
        if !branch_opens_with_try(elsif.body_nodes()) {
            return false;
        }
    }

    let Some(else_clause) = pre_if.else_clause() else {
        return false;
    };

    branch_opens_with_try(else_clause.body_nodes())
}

fn branch_opens_with_try(mut branch_nodes: impl Iterator<Item = SyntaxNode>) -> bool {
    branch_nodes.find(is_statement_node).is_some_and(|node| node.kind() == SyntaxKind::TRY_STMT)
}

pub(crate) fn lower_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<StmtIdx> {
    let range = node.text_range();
    let kind = node.kind();

    let stmt = match kind {
        SyntaxKind::ASSIGN_STMT => lower_assign_stmt(ctx, node),
        SyntaxKind::CALL_STMT => lower_call_stmt(ctx, node),
        SyntaxKind::RETURN_STMT => lower_return_stmt(ctx, node),
        SyntaxKind::IF_STMT => lower_if_stmt(ctx, node),
        SyntaxKind::WHILE_STMT => lower_while_stmt(ctx, node),
        SyntaxKind::FOR_STMT => lower_for_stmt(ctx, node),
        SyntaxKind::FOR_EACH_STMT => lower_for_each_stmt(ctx, node),
        SyntaxKind::TRY_STMT => lower_try_stmt(ctx, node),
        SyntaxKind::VAR_DEF => lower_var_decl(ctx, node),
        SyntaxKind::RAISE_STMT => lower_raise_stmt(ctx, node),
        SyntaxKind::BREAK_STMT => {
            if !ctx.in_loop() {
                let extended = extend_range_with_semicolon(node, range);
                ctx.emit(BodyDiagnostic::MisplacedLoopControl {
                    range: extended,
                    is_continue: false,
                });
            }
            Some(Stmt::Break)
        }
        SyntaxKind::CONTINUE_STMT => {
            if !ctx.in_loop() {
                let extended = extend_range_with_semicolon(node, range);
                ctx.emit(BodyDiagnostic::MisplacedLoopControl {
                    range: extended,
                    is_continue: true,
                });
            }
            Some(Stmt::Continue)
        }
        SyntaxKind::GOTO_STMT => lower_goto_stmt(ctx, node),
        SyntaxKind::LABEL_STMT => lower_label_stmt(node),
        SyntaxKind::EXECUTE_STMT => lower_execute_stmt(ctx, node),
        SyntaxKind::ADD_HANDLER_STMT => lower_add_handler_stmt(ctx, node),
        SyntaxKind::REMOVE_HANDLER_STMT => lower_remove_handler_stmt(ctx, node),
        SyntaxKind::EMPTY_STMT => {
            let has_error = node
                .parent()
                .map(|p| p.children().any(|c| c.kind() == SyntaxKind::ERROR))
                .unwrap_or(false);

            if !has_error {
                ctx.emit(BodyDiagnostic::EmptyStatement { range });
            }

            return None;
        }
        SyntaxKind::ERROR => try_lower_recovered_expr_stmt(ctx, node),
        _ => return None,
    }?;

    Some(ctx.alloc_stmt(stmt, range))
}

fn lower_assign_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let mut children = active_children(node).peekable();

    let target_node = children.next()?;
    let target = lower_expr_node(ctx, &target_node);

    let target_name = if let Expr::Path(name) = ctx.body.expr_idx(target) {
        Some((name.clone(), get_target_range(&target_node)))
    } else {
        None
    };
    if let Some((ref name, range)) = target_name {
        let key = name.as_str().fold_lower();
        let norm_key = NormName::intern(name.as_str());

        let existing_binding_kind = if ctx.local_vars.contains_key(&norm_key) {
            Some(crate::body::ExistingBindingKind::Local)
        } else if ctx.param_names.contains(&norm_key) {
            Some(crate::body::ExistingBindingKind::Param)
        } else {
            None
        };

        if !ctx.local_vars.contains_key(&norm_key) && !ctx.param_names.contains(&norm_key) {
            ctx.register_local_var(name.clone(), range);
        }

        ctx.emit(BodyDiagnostic::CommonModuleAssign {
            variable_name: name.as_str().to_string(),
            range,
            existing_binding_kind,
        });

        let name_lower = key.as_str();
        if name_lower == "этотобъект" || name_lower == "thisobject" {
            ctx.emit(BodyDiagnostic::ThisObjectAssign { range });
        }

        if ctx.is_function && ctx.by_ref_param_names.contains(&norm_key) {
            ctx.emit(BodyDiagnostic::FunctionOutParameter {
                name: name.as_str().to_string(),
                range,
            });
        }

        if let Some(&param_id) = ctx.by_value_params.get(&key) {
            let opaque_param_id = cfg_types::BindingId::from_idx(param_id);
            ctx.emit(BodyDiagnostic::RewriteMethodParameter {
                param_id: opaque_param_id,
                stmt_id: StmtId::from_raw(la_arena::RawIdx::from(0)),
                stmt_range: node.text_range(),
                ident_range: range,
            });
        }
    }

    let value_node = children.next()?;
    let value = lower_expr_node(ctx, &value_node);

    if exprs_are_equal(&ctx.body, target, value) {
        ctx.emit(BodyDiagnostic::SelfAssign { range: node.text_range() });
    }

    if let Some((ref name, _)) = target_name {
        let key = name.as_str().fold_lower();
        if ctx.cancel_params.contains(&key) && !is_valid_cancel_assignment(ctx, value, &key) {
            let range = extend_range_with_semicolon(node, node.text_range());
            ctx.emit(BodyDiagnostic::UsingCancelParameter { range });
        }
    }

    if let Some((target_name, _)) = target_name {
        use super::QueryVarType;

        if let Expr::New { type_name: Some(type_name), .. } = ctx.body.expr_idx(value) {
            let type_str = type_name.as_str().fold_lower();
            let query_type = match type_str.as_str() {
                "запрос" | "query" => QueryVarType::Query,
                "построительзапроса" | "querybuilder" => {
                    QueryVarType::QueryBuilder
                }
                "построительотчета" | "reportbuilder" => {
                    QueryVarType::ReportBuilder
                }
                _ => QueryVarType::Undefined,
            };
            ctx.register_query_var(target_name.as_str().to_string(), query_type);
        } else if let Expr::Path(source_name) = ctx.body.expr_idx(value) {
            if let Some(source_type) = ctx.get_query_var_type(source_name.as_str()) {
                ctx.register_query_var(target_name.as_str().to_string(), source_type);
            }
        }
    }

    Some(Stmt::Assign { target, value })
}

fn get_target_range(node: &SyntaxNode) -> TextRange {
    fn find_ident(node: &SyntaxNode) -> Option<TextRange> {
        for token in node.descendants_with_tokens() {
            if token.kind() == SyntaxKind::IDENT {
                return Some(token.text_range());
            }
        }
        None
    }

    find_ident(node).unwrap_or_else(|| node.text_range())
}

fn is_valid_cancel_assignment(
    ctx: &LoweringCtx,
    value_expr: crate::hir::ExprIdx,
    cancel_name: &str,
) -> bool {
    use crate::hir::{BinaryOp, Expr, Literal};

    let value = ctx.body.expr_idx(value_expr);

    match value {
        Expr::Literal(Literal::Bool(true)) => true,
        Expr::BinaryOp { lhs, rhs, op: BinaryOp::Or } => {
            expr_contains_cancel(ctx, *lhs, cancel_name)
                || expr_contains_cancel(ctx, *rhs, cancel_name)
        }
        _ => false,
    }
}

fn expr_contains_cancel(
    ctx: &LoweringCtx,
    expr_id: crate::hir::ExprIdx,
    cancel_name: &str,
) -> bool {
    use crate::hir::Expr;

    let expr = ctx.body.expr_idx(expr_id);
    match expr {
        Expr::Path(name) => name.as_str().fold_lower() == cancel_name,
        Expr::BinaryOp { lhs, rhs, .. } => {
            expr_contains_cancel(ctx, *lhs, cancel_name)
                || expr_contains_cancel(ctx, *rhs, cancel_name)
        }
        Expr::UnaryOp { expr, .. } => expr_contains_cancel(ctx, *expr, cancel_name),
        _ => false,
    }
}

fn lower_call_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let expr_node = active_children(node).next()?;
    let expr = lower_expr_node(ctx, &expr_node);
    Some(Stmt::Expr(expr))
}

fn try_lower_recovered_expr_stmt(ctx: &mut LoweringCtx, error_node: &SyntaxNode) -> Option<Stmt> {
    if !in_recoverable_stmt_position(error_node) {
        return None;
    }
    let expr_node = error_node.children().find(|c| is_recoverable_expr(c.kind()))?;
    let expr = lower_expr_node(ctx, &expr_node);
    ctx.mark_recovered_rec(expr);
    Some(Stmt::Expr(expr))
}

/// Whether an `ERROR` node stands where a statement may stand.
///
/// Besides a statement list slot, that is the body region of a preprocessor
/// branch: `#Если`-branch statements are direct children of the directive
/// node, with no `STMT_LIST` wrapper. The header region is excluded — an
/// `ERROR` before the branch's `Тогда` (a malformed condition, or what
/// recovery consumed when `Тогда` itself is missing) is not a statement.
fn in_recoverable_stmt_position(error_node: &SyntaxNode) -> bool {
    let Some(parent) = error_node.parent() else {
        return false;
    };
    match parent.kind() {
        SyntaxKind::STMT_LIST => true,
        SyntaxKind::PRE_IF_DIR | SyntaxKind::PRE_ELSIF_CLAUSE => parent
            .children_with_tokens()
            .take_while(|child| child.as_node() != Some(error_node))
            .any(|child| child.as_token().is_some_and(|t| t.kind() == SyntaxKind::KW_THEN)),
        // `#Иначе` has no header: its body starts right after the directive
        // token, which is the clause's first child.
        SyntaxKind::PRE_ELSE_CLAUSE => true,
        _ => false,
    }
}

fn is_recoverable_expr(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::FIELD_EXPR
            | SyntaxKind::CALL_EXPR
            | SyntaxKind::INDEX_EXPR
            | SyntaxKind::NEW_EXPR
            | SyntaxKind::BINARY_EXPR
            | SyntaxKind::UNARY_EXPR
            | SyntaxKind::PAREN_EXPR
            | SyntaxKind::IDENT
            | SyntaxKind::LITERAL
            | SyntaxKind::EXPR
    )
}

fn lower_return_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let value = active_children(node).next().map(|n| lower_expr_node(ctx, &n));
    if !ctx.is_function && value.is_some() {
        ctx.emit(BodyDiagnostic::ProcedureReturnsValue { range: node.text_range() });
    }

    ctx.return_statements.push(node.text_range());

    Some(Stmt::Return { value })
}

fn has_platform_type_check(condition_node: &SyntaxNode) -> bool {
    let text = condition_node.text().to_string().fold_lower();
    text.contains("linux") || text.contains("windows") || text.contains("macos")
}

/// A `#Вставка` / `#Удаление` directive region carries extension code whose body is kept as raw
/// tokens in the standalone parse and is materialized only by the configuration merge. A branch
/// holding such a region is therefore not an empty code block, even though it lowers to no
/// statements on its own.
fn has_extension_directive(stmt_list: &SyntaxNode) -> bool {
    stmt_list
        .children()
        .any(|n| matches!(n.kind(), SyntaxKind::PRE_INSERT_DIR | SyntaxKind::PRE_DELETE_DIR))
}

fn lower_if_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let mut children = active_children(node).peekable();

    let condition_node = children.next()?;

    let saved_platform_guard = ctx.in_platform_guard;
    if has_platform_type_check(&condition_node) {
        ctx.in_platform_guard = true;
    }

    let condition = lower_expr_node(ctx, &condition_node);

    let mut condition_nodes: Vec<SyntaxNode> = Vec::new();
    condition_nodes.push(condition_node.clone());

    let mut branch_nodes: Vec<SyntaxNode> = Vec::new();

    let then_stmt_list = children.next().filter(|n| n.kind() == SyntaxKind::STMT_LIST);
    let then_branch = then_stmt_list.as_ref().map(|n| lower_stmt_list(ctx, n)).unwrap_or_default();

    if then_branch.is_empty()
        && then_stmt_list.as_ref().is_some_and(|list| !has_extension_directive(list))
    {
        let range = find_if_then_range(node);
        ctx.emit(BodyDiagnostic::EmptyCodeBlock { range });
    }

    if let Some(stmt_list) = then_stmt_list {
        branch_nodes.push(stmt_list);
    }

    let mut elsif_branches = Vec::new();
    for elsif in node.children().filter(|n| n.kind() == SyntaxKind::ELSIF_CLAUSE) {
        let mut elsif_children = active_children(&elsif);
        if let Some(cond_node) = elsif_children.next() {
            condition_nodes.push(cond_node.clone());

            let cond = lower_expr_node(ctx, &cond_node);
            let stmt_list_node = elsif_children.find(|n| n.kind() == SyntaxKind::STMT_LIST);
            let body = stmt_list_node.as_ref().map(|n| lower_stmt_list(ctx, n)).unwrap_or_default();

            if body.is_empty()
                && stmt_list_node.as_ref().is_some_and(|list| !has_extension_directive(list))
            {
                let range = find_elsif_then_range(&elsif);
                ctx.emit(BodyDiagnostic::EmptyCodeBlock { range });
            }

            if let Some(stmt_list) = stmt_list_node {
                branch_nodes.push(stmt_list);
            }

            elsif_branches.push((cond, body.into_boxed_slice()));
        }
    }

    let else_branch =
        node.children().find(|n| n.kind() == SyntaxKind::ELSE_CLAUSE).and_then(|else_clause| {
            else_clause.children().find(|n| n.kind() == SyntaxKind::STMT_LIST).map(|n| {
                let stmts = lower_stmt_list(ctx, &n);

                if stmts.is_empty() && !has_extension_directive(&n) {
                    let range = find_else_range(&else_clause);
                    ctx.emit(BodyDiagnostic::EmptyCodeBlock { range });
                }

                branch_nodes.push(n.clone());

                stmts.into_boxed_slice()
            })
        });

    check_duplicated_code_blocks(ctx, &branch_nodes);

    check_duplicated_conditions(ctx, &condition_nodes);

    if !elsif_branches.is_empty() && else_branch.is_none() {
        if let Some(endif_token) = node
            .children_with_tokens()
            .filter_map(|element| element.into_token())
            .find(|token| token.kind() == SyntaxKind::KW_END_IF)
        {
            let range = endif_token.text_range();
            ctx.emit(BodyDiagnostic::IfElseIfEndsWithElse { range });
        }
    }

    ctx.in_platform_guard = saved_platform_guard;

    Some(Stmt::If(Box::new(crate::hir::IfStmt {
        condition,
        then_branch: then_branch.into_boxed_slice(),
        elsif_branches: elsif_branches.into_boxed_slice(),
        else_branch,
    })))
}

fn lower_while_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let mut children = active_children(node);

    let condition_node = children.next()?;
    let condition = lower_expr_node(ctx, &condition_node);

    ctx.enter_loop();

    let body = children
        .find(|n| n.kind() == SyntaxKind::STMT_LIST)
        .map(|n| {
            let stmts = lower_stmt_list(ctx, &n);

            if stmts.is_empty() && !has_extension_directive(&n) {
                let range = find_while_do_range(node);
                ctx.emit(BodyDiagnostic::EmptyCodeBlock { range });
            }

            stmts.into_boxed_slice()
        })
        .unwrap_or_default();

    ctx.leave_loop();

    Some(Stmt::While { condition, body })
}

fn lower_for_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let var_token = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::IDENT)?;

    let name = Name::new(var_token.text());
    let range = var_token.text_range();

    ctx.register_local_var(name.clone(), range);
    let var = ctx.alloc_binding(Binding::var(name), range);

    let mut expr_iter = active_children(node).filter(|n| {
        matches!(
            n.kind(),
            SyntaxKind::EXPR
                | SyntaxKind::LITERAL
                | SyntaxKind::BINARY_EXPR
                | SyntaxKind::UNARY_EXPR
                | SyntaxKind::CALL_EXPR
        )
    });

    let from =
        expr_iter.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    let to =
        expr_iter.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    ctx.enter_loop();

    let body = node
        .children()
        .find(|n| n.kind() == SyntaxKind::STMT_LIST)
        .map(|n| {
            let stmts = lower_stmt_list(ctx, &n);

            if stmts.is_empty() && !has_extension_directive(&n) {
                let range = find_for_do_range(node);
                ctx.emit(BodyDiagnostic::EmptyCodeBlock { range });
            }

            stmts.into_boxed_slice()
        })
        .unwrap_or_default();

    ctx.leave_loop();

    Some(Stmt::For { var, from, to, body })
}

fn lower_for_each_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let var_token = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::IDENT)?;

    let name = Name::new(var_token.text());
    let range = var_token.text_range();

    ctx.register_local_var(name.clone(), range);
    let var = ctx.alloc_binding(Binding::var(name), range);

    let collection_node = active_children(node).find(|n| {
        matches!(
            n.kind(),
            SyntaxKind::EXPR
                | SyntaxKind::CALL_EXPR
                | SyntaxKind::FIELD_EXPR
                | SyntaxKind::INDEX_EXPR
        )
    });

    let collection = collection_node
        .as_ref()
        .map(|n| lower_expr_node(ctx, n))
        .unwrap_or_else(|| ctx.missing_expr());

    let collection_text = collection_node
        .as_ref()
        .map(|n| n.text().to_string())
        .unwrap_or_else(|| String::from("<unknown>"));

    ctx.enter_loop();

    ctx.enter_foreach(collection, collection_text);

    let body = node
        .children()
        .find(|n| n.kind() == SyntaxKind::STMT_LIST)
        .map(|n| {
            let stmts = lower_stmt_list(ctx, &n);

            if stmts.is_empty() && !has_extension_directive(&n) {
                let range = find_foreach_do_range(node);
                ctx.emit(BodyDiagnostic::EmptyCodeBlock { range });
            }

            stmts.into_boxed_slice()
        })
        .unwrap_or_default();

    if let Some(stmt_list_node) = node.children().find(|n| n.kind() == SyntaxKind::STMT_LIST) {
        if !check_iterator_usage_in_body(&stmt_list_node, var_token.text()) {
            ctx.emit(BodyDiagnostic::UselessForEach {
                iterator_name: var_token.text().to_string(),
                range,
            });
        }
    }

    ctx.leave_foreach();

    ctx.leave_loop();

    Some(Stmt::ForEach { var, collection, body })
}

fn lower_try_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let violations = check_commit_transaction_in_try(node);
    for (commit_node, _violation) in violations {
        let extended_range = extend_range_with_semicolon(&commit_node, commit_node.text_range());
        ctx.emit(BodyDiagnostic::CommitTransactionOutsideTryCatch { range: extended_range });
    }

    let rollback_violations = check_rollback_transaction_in_try(node);
    for rollback_node in rollback_violations {
        let extended_range =
            extend_range_with_semicolon(&rollback_node, rollback_node.text_range());
        ctx.emit(BodyDiagnostic::WrongUseOfRollbackTransactionMethod { range: extended_range });
    }

    let body = node
        .children()
        .find(|n| n.kind() == SyntaxKind::STMT_LIST)
        .map(|n| lower_stmt_list(ctx, &n).into_boxed_slice())
        .unwrap_or_default();

    let except = node
        .children()
        .find(|n| n.kind() == SyntaxKind::EXCEPT_CLAUSE)
        .and_then(|except_clause| {
            except_clause.children().find(|n| n.kind() == SyntaxKind::STMT_LIST).map(|n| {
                ctx.in_except_block = true;
                ctx.except_has_raise = n.descendants().any(|d| d.kind() == SyntaxKind::RAISE_STMT);

                let stmts = lower_stmt_list(ctx, &n).into_boxed_slice();

                ctx.in_except_block = false;
                ctx.except_has_raise = false;

                stmts
            })
        })
        .unwrap_or_default();

    Some(Stmt::Try { body, except })
}

fn lower_raise_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let value = node.children().next().map(|n| lower_expr_node(ctx, &n));
    Some(Stmt::Raise { value })
}

fn lower_goto_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let label_token = label_name_token(node)?;

    let range = TextRange::new(node.text_range().start(), label_token.text_range().end());
    ctx.emit(BodyDiagnostic::UsingGoto { range });

    Some(Stmt::Goto(Name::new(label_token.text())))
}

fn lower_label_stmt(node: &SyntaxNode) -> Option<Stmt> {
    let label_token = label_name_token(node)?;

    Some(Stmt::Label(Name::new(label_token.text())))
}

/// The label name: an identifier, or a keyword the parser took as the name
/// right behind the tilde (`~КонецЦикла`).
fn label_name_token(node: &SyntaxNode) -> Option<syntax::SyntaxToken> {
    let mut after_tilde = false;
    for token in node.children_with_tokens().filter_map(|el| el.into_token()) {
        let kind = token.kind();
        if kind == SyntaxKind::IDENT || (after_tilde && kind.is_keyword()) {
            return Some(token);
        }
        if !kind.is_trivia() {
            after_tilde = kind == SyntaxKind::TILDE;
        }
    }
    None
}

fn lower_execute_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    if !ctx.is_client_only {
        let range = node.text_range();
        ctx.emit(BodyDiagnostic::ExecuteExternalCode { range });
    }

    let expr = active_children(node)
        .next()
        .map(|n| lower_expr_node(ctx, &n))
        .unwrap_or_else(|| ctx.missing_expr());

    Some(Stmt::Execute { expr })
}

fn lower_add_handler_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let mut expr_iter = active_children(node);

    let event =
        expr_iter.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    let handler =
        expr_iter.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    Some(Stmt::AddHandler { event, handler })
}

fn lower_remove_handler_stmt(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let mut expr_iter = active_children(node);

    let event =
        expr_iter.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    let handler =
        expr_iter.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    Some(Stmt::RemoveHandler { event, handler })
}

pub(crate) fn should_skip_one_statement_per_line(node: &SyntaxNode) -> bool {
    if node.kind() == SyntaxKind::EMPTY_STMT {
        return true;
    }

    if node.descendants().any(|n| {
        matches!(
            n.kind(),
            SyntaxKind::PRE_IF_DIR
                | SyntaxKind::PRE_ELSIF_CLAUSE
                | SyntaxKind::PRE_ELSE_CLAUSE
                | SyntaxKind::PRE_REGION_DIR
                | SyntaxKind::PRE_DELETE_DIR
                | SyntaxKind::PRE_INSERT_DIR
        )
    }) {
        return true;
    }

    if node.descendants().any(|n| n.kind() == SyntaxKind::ERROR) {
        return true;
    }

    false
}

fn lower_var_decl(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Option<Stmt> {
    let mut bindings = Vec::new();

    for ident in node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| tok.kind() == SyntaxKind::IDENT)
    {
        let name = Name::new(ident.text());
        let range = ident.text_range();

        ctx.register_local_var(name.clone(), range);

        let binding_id = ctx.alloc_binding(Binding::var(name), range);
        bindings.push(binding_id);
    }

    if bindings.is_empty() {
        return None;
    }

    Some(Stmt::VarDecl { bindings: bindings.into_boxed_slice() })
}

pub(super) fn should_skip_semicolon_check(node: &SyntaxNode) -> bool {
    if matches!(node.kind(), SyntaxKind::EMPTY_STMT | SyntaxKind::LABEL_STMT) {
        return true;
    }
    node.descendants().any(|n| n.kind() == SyntaxKind::ERROR)
}

pub(super) fn has_trailing_semicolon(node: &SyntaxNode) -> bool {
    if syntax::trailing_semicolon(node).is_some() {
        return true;
    }

    node.children_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|t| t.kind() == SyntaxKind::SEMICOLON)
}

/// Диапазон последнего токена узла — места, где не хватает точки с запятой.
///
/// Отбора по тривии здесь нет: последний токен узла значим по норме привязки.
/// Обход идёт по всем токенам поддерева, а не через `last_token`, потому что
/// последним ребёнком узла обычно оказывается пустой `ERROR`, а он обрывает
/// спуск rowan за краевым токеном.
pub(super) fn last_token_range(node: &SyntaxNode) -> TextRange {
    node.descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .last()
        .map(|t| t.text_range())
        .unwrap_or_else(|| node.text_range())
}

fn check_iterator_usage_in_body(stmt_list: &SyntaxNode, iterator_name: &str) -> bool {
    let iterator_lower = iterator_name.fold_lower();

    for descendant in stmt_list.descendants_with_tokens() {
        if let Some(token) = descendant.into_token() {
            if token.kind() == SyntaxKind::IDENT
                && token.text().fold_lower() == iterator_lower
                && !is_direct_function_call(&token)
            {
                return true;
            }
        }
    }

    false
}

fn is_direct_function_call(token: &syntax::SyntaxToken) -> bool {
    let Some(ident_node) = token.parent() else {
        return false;
    };

    let Some(parent) = ident_node.parent() else {
        return false;
    };

    if parent.kind() == SyntaxKind::CALL_EXPR {
        return is_direct_callee(&ident_node, &parent);
    }

    if parent.kind() == SyntaxKind::CALL_STMT {
        for child in active_children(&parent) {
            if child.kind() == SyntaxKind::CALL_EXPR {
                return is_direct_callee(&ident_node, &child);
            }
        }
    }

    false
}

fn is_direct_callee(ident_node: &SyntaxNode, call_expr: &SyntaxNode) -> bool {
    if let Some(first_child) = active_children(call_expr).next() {
        if first_child.text_range() == ident_node.text_range() {
            return !active_children(call_expr).any(|c| c.kind() == SyntaxKind::FIELD_EXPR);
        }
    }
    false
}
