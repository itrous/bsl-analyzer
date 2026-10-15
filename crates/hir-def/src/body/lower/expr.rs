use bsl_platform::capability::{Category as CapabilityCategory, EntryKind as CapabilityEntryKind};
use intern::NormName;
use stdx::case::CaseExt;
use syntax::ast_utils::{active_children, active_children_with_tokens, field_tail_name_token};
use syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

use crate::body::{
    Body, BodyDiagnostic, ExternalRef, MagicNumberContext, ManagerType, RedundantAccessKind,
};
use crate::hir::{BinaryOp, Expr, ExprIdx, Literal, UnaryOp};
use crate::Name;

use super::diagnostics::{is_deprecated_method, is_followed_by_loop_exit};
use super::utils::{extract_string_content, looks_like_sdbl};
use super::LoweringCtx;

fn field_name_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    field_tail_name_token(node)
}

pub(crate) fn lower_expr_node(ctx: &mut LoweringCtx, node: &SyntaxNode) -> ExprIdx {
    let actual_node = if node.kind() == SyntaxKind::EXPR {
        active_children(node).next().unwrap_or_else(|| node.clone())
    } else {
        node.clone()
    };

    lower_expr(ctx, &actual_node)
}

fn lower_expr(ctx: &mut LoweringCtx, node: &SyntaxNode) -> ExprIdx {
    let range = node.text_range();

    let expr = match node.kind() {
        SyntaxKind::LITERAL => lower_literal(ctx, node),
        SyntaxKind::BINARY_EXPR => lower_binary_expr(ctx, node),
        SyntaxKind::UNARY_EXPR => lower_unary_expr(ctx, node),
        SyntaxKind::TERNARY_EXPR => lower_ternary_expr(ctx, node),
        SyntaxKind::CALL_EXPR => lower_call_expr(ctx, node),
        SyntaxKind::INDEX_EXPR => lower_index_expr(ctx, node),
        SyntaxKind::FIELD_EXPR => lower_field_expr(ctx, node),
        SyntaxKind::NEW_EXPR => lower_new_expr(ctx, node),
        SyntaxKind::PAREN_EXPR => {
            return active_children(node)
                .next()
                .map(|n| lower_expr_node(ctx, &n))
                .unwrap_or_else(|| ctx.missing_expr());
        }
        SyntaxKind::AWAIT_EXPR => {
            return active_children(node)
                .next()
                .map(|n| lower_expr_node(ctx, &n))
                .unwrap_or_else(|| ctx.missing_expr());
        }
        SyntaxKind::IDENT => {
            let text = node.text().to_string();

            use super::diagnostics::{
                is_call_expr_callee, is_field_access_field, is_this_form_identifier,
            };
            if is_this_form_identifier(&text)
                && !ctx.param_names.contains(&NormName::intern(&text))
                && !is_call_expr_callee(node)
                && !is_field_access_field(node)
            {
                ctx.emit(BodyDiagnostic::UsingThisForm { range });
            }

            Expr::Path(Name::new(&text))
        }
        SyntaxKind::EXPR => {
            return active_children(node)
                .next()
                .map(|n| lower_expr(ctx, &n))
                .unwrap_or_else(|| ctx.missing_expr());
        }
        _ => {
            if let Some(ident) = node
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| tok.kind() == SyntaxKind::IDENT)
            {
                Expr::Path(Name::new(ident.text()))
            } else {
                Expr::Missing
            }
        }
    };

    let expr_id = ctx.alloc_expr(expr, range);

    if let Some(idx) =
        ctx.pending_sdbl.iter().position(|(literal_range, _)| *literal_range == range)
    {
        let (literal_range, query) = ctx.pending_sdbl.remove(idx);
        ctx.body.sdbl_exprs.push((
            expr_id,
            cfg_types::LocalRange::of_detached_node(literal_range),
            query,
        ));
    }

    expr_id
}

fn lower_literal(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Expr {
    let token = node.children_with_tokens().filter_map(|el| el.into_token()).find(|tok| {
        matches!(
            tok.kind(),
            SyntaxKind::DECIMAL
                | SyntaxKind::FLOAT
                | SyntaxKind::STRING
                | SyntaxKind::STRING_START
                | SyntaxKind::DATE
                | SyntaxKind::KW_TRUE
                | SyntaxKind::KW_FALSE
                | SyntaxKind::KW_UNDEFINED
                | SyntaxKind::KW_NULL
        )
    });

    let Some(token) = token else {
        return Expr::Missing;
    };

    let literal = match token.kind() {
        SyntaxKind::DECIMAL | SyntaxKind::FLOAT => {
            let text = token.text().replace(' ', "");
            let value = text.parse::<f64>().unwrap_or(0.0);

            let value = ordered_float::NotNan::new(value)
                .unwrap_or_else(|_| ordered_float::NotNan::new(0.0).unwrap());

            let context = determine_magic_number_context(&token);
            ctx.emit(BodyDiagnostic::MagicNumber {
                value: text.clone(),
                range: token.text_range(),
                context,
            });

            Literal::Number(value)
        }
        SyntaxKind::STRING | SyntaxKind::STRING_START => {
            let torn = syntax::sdbl_query::extract_torn_literal(node);
            let value = match &torn {
                Some((text, _)) => text.clone(),
                None => extract_string_content(node).unwrap_or_default(),
            };

            if looks_like_sdbl(&value) {
                let (sdbl_text, quote_corrections, literal_map) = match torn {
                    Some((text, map)) => (text, Vec::new(), Some(map)),
                    None => {
                        let (text, corrections) = syntax::extract_sdbl_with_corrections(node)
                            .unwrap_or_else(|| (value.clone(), vec![]));
                        (text, corrections, None)
                    }
                };

                let sdbl_ast = parser::parse_sdbl_with_shared_cache(&sdbl_text);
                let literal_text = node.text().to_string();
                let error_ranges_in_bsl: Vec<_> =
                    syntax::sdbl_query::collect_query_parse_errors(&sdbl_ast)
                        .into_iter()
                        .map(|(query_range, err)| {
                            let bsl_range = match &literal_map {
                                Some(map) => map.map_range_to_literal(query_range),
                                None => syntax::sdbl_query::map_range_query_to_literal(
                                    &literal_text,
                                    query_range,
                                ),
                            };
                            (bsl_range, err)
                        })
                        .collect();

                let query = syntax::SdblQuery {
                    query_text: sdbl_text,
                    query_ast: Some(sdbl_ast),
                    quote_corrections,
                    error_ranges_in_bsl,
                    literal_map,
                };

                ctx.pending_sdbl.push((node.text_range(), query));
            }

            Literal::String(value)
        }
        SyntaxKind::DATE => {
            let text = token.text();
            let value = text.trim_start_matches('\'').trim_end_matches('\'').to_string();
            Literal::Date(value)
        }
        SyntaxKind::KW_TRUE => Literal::Bool(true),
        SyntaxKind::KW_FALSE => Literal::Bool(false),
        SyntaxKind::KW_UNDEFINED => Literal::Undefined,
        SyntaxKind::KW_NULL => Literal::Null,
        _ => return Expr::Missing,
    };

    Expr::Literal(literal)
}

fn lower_binary_expr(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Expr {
    let mut children = active_children(node);

    let lhs_node = match children.next() {
        Some(n) => n,
        None => return Expr::Missing,
    };
    let lhs = lower_expr_node(ctx, &lhs_node);

    let op_token = node.children_with_tokens().filter_map(|el| el.into_token()).find(|tok| {
        matches!(
            tok.kind(),
            SyntaxKind::PLUS
                | SyntaxKind::MINUS
                | SyntaxKind::STAR
                | SyntaxKind::SLASH
                | SyntaxKind::PERCENT
                | SyntaxKind::EQ
                | SyntaxKind::NEQ
                | SyntaxKind::LT
                | SyntaxKind::LE
                | SyntaxKind::GT
                | SyntaxKind::GE
                | SyntaxKind::KW_AND
                | SyntaxKind::KW_OR
        )
    });

    let op = op_token
        .map(|tok| match tok.kind() {
            SyntaxKind::PLUS => BinaryOp::Add,
            SyntaxKind::MINUS => BinaryOp::Sub,
            SyntaxKind::STAR => BinaryOp::Mul,
            SyntaxKind::SLASH => BinaryOp::Div,
            SyntaxKind::PERCENT => BinaryOp::Mod,
            SyntaxKind::EQ => BinaryOp::Eq,
            SyntaxKind::NEQ => BinaryOp::Neq,
            SyntaxKind::LT => BinaryOp::Lt,
            SyntaxKind::LE => BinaryOp::Le,
            SyntaxKind::GT => BinaryOp::Gt,
            SyntaxKind::GE => BinaryOp::Ge,
            SyntaxKind::KW_AND => BinaryOp::And,
            SyntaxKind::KW_OR => BinaryOp::Or,
            _ => BinaryOp::Add,
        })
        .unwrap_or(BinaryOp::Add);

    let rhs_node = match children.next() {
        Some(n) => n,
        None => return Expr::Missing,
    };
    let rhs = lower_expr_node(ctx, &rhs_node);

    if matches!(op, BinaryOp::Add) {
        let mut current = rhs_node.clone();
        let mut unary_plus_token = None;

        while current.kind() == SyntaxKind::EXPR {
            if let Some(plus_tok) = current
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| matches!(tok.kind(), SyntaxKind::PLUS))
            {
                unary_plus_token = Some(plus_tok);
                break;
            }

            if let Some(child) = active_children(&current).next() {
                current = child;
            } else {
                break;
            }
        }

        if let Some(unary_plus_tok) = unary_plus_token {
            let has_numeric_literal = current
                .descendants()
                .find(|n| n.kind() == SyntaxKind::LITERAL)
                .map(|lit| {
                    lit.children_with_tokens()
                        .filter_map(|el| el.into_token())
                        .any(|tok| matches!(tok.kind(), SyntaxKind::DECIMAL | SyntaxKind::FLOAT))
                })
                .unwrap_or(false);

            if !has_numeric_literal {
                ctx.emit(BodyDiagnostic::UnaryPlusInConcatenation {
                    range: unary_plus_tok.text_range(),
                });
            }
        }
    }

    Expr::BinaryOp { lhs, rhs, op }
}

fn lower_unary_expr(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Expr {
    let op_token = node.children_with_tokens().filter_map(|el| el.into_token()).find(|tok| {
        matches!(tok.kind(), SyntaxKind::MINUS | SyntaxKind::PLUS | SyntaxKind::KW_NOT)
    });

    let op = op_token
        .map(|tok| match tok.kind() {
            SyntaxKind::MINUS => UnaryOp::Neg,
            SyntaxKind::PLUS => UnaryOp::Plus,
            SyntaxKind::KW_NOT => UnaryOp::Not,
            _ => UnaryOp::Neg,
        })
        .unwrap_or(UnaryOp::Neg);

    let expr_node = match active_children(node).next() {
        Some(n) => n,
        None => {
            let missing = ctx.missing_expr();
            return Expr::UnaryOp { expr: missing, op };
        }
    };
    let expr = lower_expr_node(ctx, &expr_node);

    Expr::UnaryOp { expr, op }
}

fn lower_ternary_expr(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Expr {
    ctx.emit(BodyDiagnostic::TernaryOperatorUsage { range: node.text_range() });

    let mut children = active_children(node);

    let condition =
        children.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    let then_expr =
        children.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    let else_expr =
        children.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    Expr::Ternary { condition, then_expr, else_expr }
}

fn lower_call_expr(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Expr {
    let mut children = active_children(node);

    let callee_node = match children.next() {
        Some(n) => n,
        None => return Expr::Missing,
    };

    let actual_callee = if callee_node.kind() == SyntaxKind::EXPR {
        active_children(&callee_node).next().unwrap_or_else(|| callee_node.clone())
    } else {
        callee_node.clone()
    };

    let mut is_safe_mode_query_call = false;
    let mut is_str_template_call = false;

    if actual_callee.kind() == SyntaxKind::IDENT {
        let name = actual_callee.text().to_string();

        use super::diagnostics::is_safe_mode_query;
        if is_safe_mode_query(&name) {
            is_safe_mode_query_call = true;
        }

        use super::diagnostics::is_deprecated_global_method_8312;

        if is_deprecated_global_method_8312(&name) {
            ctx.emit(BodyDiagnostic::DeprecatedAttribute8312 {
                name: name.clone(),
                kind: crate::body::DeprecatedKind8312::GlobalMethod,
                owner: None,
                range: actual_callee.text_range(),
            });
        }

        use super::diagnostics::is_deprecated_current_date;

        if is_deprecated_current_date(&name) {
            ctx.emit(BodyDiagnostic::DeprecatedCurrentDate {
                name: name.clone(),
                range: actual_callee.text_range(),
            });
        }

        use super::diagnostics::is_deprecated_find;

        if is_deprecated_find(&name) {
            ctx.emit(BodyDiagnostic::DeprecatedFind {
                name: name.clone(),
                range: actual_callee.text_range(),
            });
        }

        use super::diagnostics::is_deprecated_message;

        if is_deprecated_message(&name) {
            ctx.emit(BodyDiagnostic::DeprecatedMessage {
                name: name.clone(),
                range: actual_callee.text_range(),
            });
        }

        use super::diagnostics::is_temp_files_dir;

        if is_temp_files_dir(&name) {
            ctx.emit(BodyDiagnostic::TempFilesDir {
                name: name.clone(),
                range: actual_callee.text_range(),
            });
        }

        use super::diagnostics::{is_deprecated_managed_form, is_type_method};

        if is_type_method(&name) {
            if let Some(arg_list) = active_children(node).find(|n| n.kind() == SyntaxKind::ARG_LIST)
            {
                if let Some(first_arg) = active_children(&arg_list).next() {
                    if let Some(string_token) = first_arg
                        .descendants_with_tokens()
                        .filter_map(|el| el.into_token())
                        .find(|tok| tok.kind() == SyntaxKind::STRING)
                    {
                        let text = string_token.text();
                        if text.len() >= 2 {
                            let inner = &text[1..text.len() - 1];
                            let content = inner.replace("\"\"", "\"");

                            if is_deprecated_managed_form(&content) {
                                ctx.emit(BodyDiagnostic::DeprecatedTypeManagedForm {
                                    type_name: content,
                                    range: string_token.text_range(),
                                });
                            }
                        }
                    }
                }
            }
        }

        let name_lower = name.fold_lower();
        if (name_lower == "eval" || name_lower == "вычислить") && !ctx.is_client_only {
            ctx.emit(BodyDiagnostic::ExecuteExternalCode { range: node.text_range() });
        }

        if is_os_users_method(&name) {
            ctx.emit(BodyDiagnostic::OSUsersMethod { range: actual_callee.text_range() });
        }

        use super::diagnostics::check_try_number_call;
        if let Some(range) = check_try_number_call(node) {
            ctx.emit(BodyDiagnostic::TryNumber { range });
        }

        if is_write_log_event_method(&name) {
            check_write_log_event_call(ctx, node);
        }

        if is_form_data_to_value_global_method(&name) && !ctx.has_no_context_annotation {
            ctx.emit(BodyDiagnostic::FormDataToValue { range: actual_callee.text_range() });
        }

        if is_get_form_global_method(&name) {
            ctx.emit(BodyDiagnostic::GetFormMethod {
                method_name: name.clone(),
                range: actual_callee.text_range(),
            });
        }

        if is_proceed_with_call_method(&name_lower) && !ctx.is_instead_method {
            ctx.emit(BodyDiagnostic::WrongUseFunctionProceedWithCall {
                range: actual_callee.text_range(),
            });
        }

        if is_str_template_method(&name) {
            is_str_template_call = true;
        }

        if is_deprecated_method(&name) {
            ctx.emit(BodyDiagnostic::DeprecatedMethod {
                name: name.clone(),
                range: node.text_range(),
            });
        }

        let name_norm = NormName::intern(&name);
        if !ctx.local_vars.contains_key(&name_norm) && !ctx.param_names.contains(&name_norm) {
            ctx.emit(BodyDiagnostic::DeprecatedMethodCall {
                callee: name,
                module: None,
                range: actual_callee.text_range(),
            });
        }

        use super::diagnostics::get_modal_method_replacement;
        if let Some(replacement) = get_modal_method_replacement(&name_lower) {
            ctx.emit(BodyDiagnostic::UsingModalWindows {
                method_name: actual_callee.text().to_string(),
                replacement: replacement.to_string(),
                range: node.text_range(),
            });
        }

        use super::diagnostics::get_synchronous_call_replacement;
        if let Some(replacement) = get_synchronous_call_replacement(&name_lower) {
            ctx.emit(BodyDiagnostic::UsingSynchronousCalls {
                method_name: actual_callee.text().to_string(),
                replacement: replacement.to_string(),
                range: node.text_range(),
            });
        }
    } else if actual_callee.kind() == SyntaxKind::FIELD_EXPR {
        let idents: Vec<_> = actual_callee
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind() == SyntaxKind::IDENT)
            .collect();

        tracing::debug!(
            idents_count = idents.len(),
            callee_text = %actual_callee.text(),
            "lower_call_expr: FIELD_EXPR found"
        );

        if idents.len() == 2 {
            let module_name = idents[0].text();
            let method_name_str = idents[1].text();
            let key = NormName::intern(module_name);

            if !ctx.local_vars.contains_key(&key) && !ctx.param_names.contains(&key) {
                ctx.external_refs.push(crate::body::ExternalRef::QualifiedCall {
                    receiver: crate::Name::new(module_name),
                    method: crate::Name::new(method_name_str),
                    range: actual_callee.text_range(),
                });

                ctx.emit(BodyDiagnostic::DeprecatedMethodCall {
                    callee: method_name_str.to_string(),
                    module: Some(module_name.to_string()),
                    range: idents[1].text_range(),
                });
            }
        }

        if let Some(method_token) = idents.last() {
            let method_name = method_token.text();

            if is_get_form_method(method_name) {
                ctx.emit(BodyDiagnostic::GetFormMethod {
                    method_name: method_name.to_string(),
                    range: method_token.text_range(),
                });
            }
        }

        check_using_external_code_tools(ctx, &actual_callee, &idents, node);
    }

    let callee = lower_expr_node(ctx, &callee_node);

    let arg_list_node = active_children(node).find(|n| n.kind() == SyntaxKind::ARG_LIST);

    let args =
        arg_list_node.as_ref().map(|arg_list| lower_arg_list(ctx, arg_list)).unwrap_or_default();

    if is_safe_mode_query_call && is_unsafe_safe_mode_context(node) {
        ctx.emit(BodyDiagnostic::UnsafeSafeModeMethodCall { range: actual_callee.text_range() });
    }

    if is_str_template_call {
        if let Some(ref arg_list) = arg_list_node {
            if let Some(first_arg) = active_children(arg_list).next() {
                if let Some(template_string) = find_string_in_node(&first_arg) {
                    let param_count = arg_list
                        .children_with_tokens()
                        .filter(|el| el.as_token().is_some_and(|t| t.kind() == SyntaxKind::COMMA))
                        .count();

                    if is_wrong_str_template(&template_string, param_count) {
                        ctx.emit(BodyDiagnostic::IncorrectUseOfStrTemplate {
                            range: node.text_range(),
                        });
                    }
                }
            }
        }
    }

    if ctx.in_loop() && actual_callee.kind() == SyntaxKind::FIELD_EXPR {
        if let Some(method_token) = actual_callee
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| matches!(tok.kind(), SyntaxKind::IDENT | SyntaxKind::KW_EXECUTE))
            .last()
        {
            let method_name = method_token.text().fold_lower();
            if matches!(method_name.as_str(), "execute" | "выполнить") {
                let receiver = match ctx.body.expr_idx(callee) {
                    Expr::Field { base, .. } => Some(*base),
                    Expr::MethodCall { receiver, .. } => Some(*receiver),
                    _ => None,
                };

                if let Some(receiver_id) = receiver {
                    if let Some(var_name) = extract_receiver_name(ctx, receiver_id) {
                        if ctx.is_query_var(&var_name) {
                            ctx.emit(BodyDiagnostic::CreateQueryInCycle {
                                range: node.text_range(),
                            });
                        }
                    }
                }
            }
        }
    }

    if actual_callee.kind() == SyntaxKind::FIELD_EXPR {
        let object_name_opt = active_children(&actual_callee).next().and_then(|base_node| {
            if base_node.kind() == SyntaxKind::IDENT {
                return Some(base_node.text().to_string());
            }

            if base_node.kind() == SyntaxKind::EXPR {
                if let Some(ident_child) =
                    active_children(&base_node).find(|n| n.kind() == SyntaxKind::IDENT)
                {
                    return Some(ident_child.text().to_string());
                }
            }

            base_node
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| tok.kind() == SyntaxKind::IDENT)
                .map(|tok| tok.text().to_string())
        });

        let method_token_opt = actual_callee
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind() == SyntaxKind::IDENT)
            .last();

        if let (Some(object_name), Some(method_token)) = (object_name_opt, method_token_opt) {
            use super::diagnostics::is_deprecated_attribute_8312;

            if let Some(kind) =
                is_deprecated_attribute_8312(&object_name, method_token.text(), true)
            {
                ctx.emit(BodyDiagnostic::DeprecatedAttribute8312 {
                    name: method_token.text().to_string(),
                    kind,
                    owner: Some(object_name.clone()),
                    range: method_token.text_range(),
                });
            }
        }
    }

    if actual_callee.kind() == SyntaxKind::FIELD_EXPR {
        if let Some(method_token) = actual_callee
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind() == SyntaxKind::IDENT)
            .last()
        {
            let method_name = method_token.text().fold_lower();
            if matches!(method_name.as_str(), "delete" | "удалить") {
                let receiver = match ctx.body.expr_idx(callee) {
                    Expr::Field { base, .. } => Some(*base),
                    Expr::MethodCall { receiver, .. } => Some(*receiver),
                    _ => None,
                };

                if let Some(receiver_id) = receiver {
                    if let Some(collection_text) = ctx.matches_foreach_collection(receiver_id) {
                        if !is_followed_by_loop_exit(node) {
                            ctx.emit(BodyDiagnostic::DeletingCollectionItem {
                                collection_text: collection_text.to_string(),
                                range: node.text_range(),
                            });
                        }
                    }
                }
            }
        }
    }

    if actual_callee.kind() == SyntaxKind::FIELD_EXPR {
        if let Some(method_token) = actual_callee
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind() == SyntaxKind::IDENT)
            .last()
        {
            let method_name = method_token.text().fold_lower();
            if matches!(method_name.as_str(), "insert" | "вставить" | "add" | "добавить")
            {
                let receiver = match ctx.body.expr_idx(callee) {
                    Expr::Field { base, .. } => Some(*base),
                    Expr::MethodCall { receiver, .. } => Some(*receiver),
                    _ => None,
                };

                if let Some(receiver_id) = receiver {
                    for &arg_id in args.iter() {
                        if exprs_are_equal(&ctx.body, receiver_id, arg_id) {
                            ctx.emit(BodyDiagnostic::SelfInsertion { range: node.text_range() });
                            break;
                        }
                    }
                }
            }
        }
    }

    if actual_callee.kind() == SyntaxKind::FIELD_EXPR {
        if let Some(method_token) = actual_callee
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind() == SyntaxKind::IDENT)
            .last()
        {
            use super::diagnostics::is_find_element_method;
            if is_find_element_method(method_token.text()) {
                let has_literal_first_arg = check_find_element_first_arg(&args, ctx);
                if has_literal_first_arg {
                    let range = if let Some(ref arg_list) = arg_list_node {
                        method_token.text_range().cover(arg_list.text_range())
                    } else {
                        method_token.text_range()
                    };
                    ctx.emit(BodyDiagnostic::UsingFindElementByString { range });
                }
            }
        }
    }

    if actual_callee.kind() == SyntaxKind::FIELD_EXPR {
        if let Some(method_token) = actual_callee
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind() == SyntaxKind::IDENT)
            .last()
        {
            use super::diagnostics::is_find_by_code_method;
            if is_find_by_code_method(method_token.text()) {
                if let Some(receiver) = active_children(&actual_callee).next() {
                    if receiver.kind() == SyntaxKind::FIELD_EXPR {
                        let object_name = receiver
                            .children_with_tokens()
                            .filter_map(|e| e.into_token())
                            .filter(|t| t.kind() == SyntaxKind::IDENT)
                            .last()
                            .map(|t| t.text().to_string());

                        let manager_name = active_children(&receiver).next().and_then(|base| {
                            if let Some(token) = base.first_token() {
                                if token.kind() == SyntaxKind::IDENT {
                                    return Some(token.text().to_string());
                                }
                            }
                            None
                        });

                        if let (Some(manager), Some(object)) = (manager_name, object_name) {
                            ctx.emit(BodyDiagnostic::UnsafeFindByCode {
                                manager_name: manager,
                                object_name: object,
                                range: method_token.text_range(),
                            });
                        }
                    }
                }
            }
        }
    }

    if actual_callee.kind() == SyntaxKind::IDENT {
        let callee_name = actual_callee.text().to_string();

        let is_local = {
            let key = NormName::intern(&callee_name);
            ctx.local_vars.contains_key(&key) || ctx.param_names.contains(&key)
        };

        if !is_local {
            let arg_presence = arg_list_node.as_ref().map(extract_arg_presence).unwrap_or_default();

            ctx.emit(BodyDiagnostic::MissedRequiredParameter {
                callee: callee_name,
                module: None,
                mdo_type: None,
                mdo_name: None,
                args: arg_presence,
                range: node.text_range(),
            });
        }
    }

    if actual_callee.kind() == SyntaxKind::FIELD_EXPR {
        record_qualified_call_facts(ctx, node, &actual_callee, arg_list_node.as_ref());
    }

    Expr::Call { callee, args: args.into_boxed_slice() }
}

/// Record what a qualified call tells the layers that cannot ask inference: the
/// dependency edge it implies, and the two syntactic diagnostics that read the
/// spelling of the chain.
///
/// It builds no expression: the call keeps the shape it has in the source. The name
/// says `record`, not `lower`, because a lowering that also decided ownership is
/// exactly what this stage stopped doing.
fn record_qualified_call_facts(
    ctx: &mut LoweringCtx,
    call_node: &SyntaxNode,
    field_expr_node: &SyntaxNode,
    arg_list_node: Option<&SyntaxNode>,
) {
    let Some(call_info) = analyze_qualified_call(field_expr_node, ctx) else { return };
    let Some(field_token) = field_name_token(field_expr_node) else { return };
    let field_name = Name::new(field_token.text());

    let arg_presence = arg_list_node.map(extract_arg_presence).unwrap_or_default();

    match call_info {
        QualifiedCallInfo::TwoLevel { module } => {
            let is_this_object = {
                let lower = module.fold_lower();
                lower == "этотобъект" || lower == "thisobject"
            };

            if is_this_object {
                ctx.emit(BodyDiagnostic::MissedRequiredParameter {
                    callee: field_name.as_str().to_string(),
                    module: None,
                    mdo_type: None,
                    mdo_name: None,
                    args: arg_presence,
                    range: call_node.text_range(),
                });
            }
        }
        QualifiedCallInfo::ThreeLevel { mdo_type, mdo_name } => {
            if let Some(manager_type) = ManagerType::from_name(&mdo_type) {
                ctx.external_refs.push(ExternalRef::ManagerAccess {
                    manager_type,
                    object_name: Name::new(&mdo_name),
                    method: Some(field_name.clone()),
                    range: call_node.text_range(),
                });
            }

            // No node of its own: the chain keeps the Expr language of its source,
            // `Call{Field{Field{Path}}}`. Whether the root name belongs to the
            // collection, and which object the middle segment names, are questions
            // lowering cannot answer — they need a resolver. Dependency discovery
            // keeps its answer above (`ExternalRef`), because it runs before
            // inference and may over-approximate; typing, diagnostics and
            // completion read the resolved type instead.
        }
    }
}

fn lower_arg_list(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Vec<ExprIdx> {
    if let Some(comma_range) = find_trailing_comma(node) {
        ctx.emit(BodyDiagnostic::ExtraCommas { range: comma_range });
    }

    let mut args = Vec::new();
    let mut current_expr: Option<ExprIdx> = None;
    let mut has_any_content = false;

    for child in active_children_with_tokens(node) {
        match child.kind() {
            SyntaxKind::COMMA => {
                args.push(current_expr.unwrap_or_else(|| ctx.missing_expr()));
                current_expr = None;
            }
            SyntaxKind::L_PAREN | SyntaxKind::R_PAREN => {}
            kind if kind.is_trivia() => {}
            _ => {
                if let Some(expr_node) = child.as_node() {
                    current_expr = Some(lower_expr_node(ctx, expr_node));
                    has_any_content = true;
                }
            }
        }
    }

    if has_any_content || !args.is_empty() {
        args.push(current_expr.unwrap_or_else(|| ctx.missing_expr()));
    }

    args
}

fn find_trailing_comma(arg_list: &SyntaxNode) -> Option<syntax::TextRange> {
    use syntax::NodeOrToken;

    let tokens: Vec<_> = active_children_with_tokens(arg_list).collect();
    let mut iter = tokens.iter().rev().filter(|element| !is_trivia_element(element));

    let r_paren = iter.next()?;
    if !matches!(r_paren, NodeOrToken::Token(t) if t.kind() == SyntaxKind::R_PAREN) {
        return None;
    }

    let prev = iter.next()?;
    match prev {
        NodeOrToken::Token(token) if token.kind() == SyntaxKind::COMMA => Some(token.text_range()),
        _ => None,
    }
}

fn is_trivia_element(element: &syntax::NodeOrToken<SyntaxNode, syntax::SyntaxToken>) -> bool {
    matches!(
        element,
        syntax::NodeOrToken::Token(t) if matches!(
            t.kind(),
            kind if kind.is_trivia()
        )
    )
}

fn extract_arg_presence(arg_list: &SyntaxNode) -> Vec<bool> {
    let mut args = Vec::new();
    let mut has_expr = false;

    for child in active_children_with_tokens(arg_list) {
        match child.kind() {
            SyntaxKind::COMMA => {
                args.push(has_expr);
                has_expr = false;
            }
            SyntaxKind::L_PAREN | SyntaxKind::R_PAREN => {}
            kind if kind.is_trivia() => {}
            _ => {
                has_expr = true;
            }
        }
    }

    if active_children(arg_list).count() > 0 || has_expr {
        args.push(has_expr);
    }

    args
}

fn lower_index_expr(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Expr {
    let mut children = active_children(node);

    let base =
        children.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    let index =
        children.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    Expr::Index { base, index }
}

fn lower_field_expr(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Expr {
    let mut children = active_children(node);

    let base =
        children.next().map(|n| lower_expr_node(ctx, &n)).unwrap_or_else(|| ctx.missing_expr());

    let field_token = field_name_token(node);

    let field_name =
        field_token.as_ref().map(|tok| Name::new(tok.text())).unwrap_or_else(Name::missing);

    let object_name_opt = active_children(node).next().and_then(|base_node| {
        if base_node.kind() == SyntaxKind::IDENT {
            return Some(base_node.text().to_string());
        }

        if base_node.kind() == SyntaxKind::EXPR {
            if let Some(ident_child) =
                active_children(&base_node).find(|n| n.kind() == SyntaxKind::IDENT)
            {
                return Some(ident_child.text().to_string());
            }
        }

        base_node
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| tok.kind() == SyntaxKind::IDENT)
            .map(|tok| tok.text().to_string())
    });

    if let (Some(field_tok), Some(object_name)) = (&field_token, &object_name_opt) {
        use super::diagnostics::is_deprecated_attribute_8312;

        if let Some(kind) = is_deprecated_attribute_8312(object_name, field_tok.text(), false) {
            ctx.emit(BodyDiagnostic::DeprecatedAttribute8312 {
                name: field_tok.text().to_string(),
                kind,
                owner: Some(object_name.clone()),
                range: field_tok.text_range(),
            });
        }
    }

    let direct_base_name = active_children(node).next().and_then(|base_node| {
        if base_node.kind() == SyntaxKind::IDENT {
            return Some(base_node.text().to_string());
        }
        if base_node.kind() == SyntaxKind::EXPR {
            if let Some(ident_child) =
                active_children(&base_node).find(|n| n.kind() == SyntaxKind::IDENT)
            {
                return Some(ident_child.text().to_string());
            }
        }
        None
    });
    if let Some(ref base_name) = direct_base_name {
        let lower = base_name.fold_lower();
        if lower == "этотобъект" || lower == "thisobject" {
            ctx.emit(BodyDiagnostic::RedundantAccessToObject {
                kind: RedundantAccessKind::ThisObject { prefix: base_name.clone() },
                range: node.text_range(),
            });
        }
    }

    Expr::Field { base, field: field_name }
}

fn extract_receiver_name(ctx: &LoweringCtx, expr_id: ExprIdx) -> Option<String> {
    let expr = ctx.body.expr_idx(expr_id);
    match expr {
        Expr::Path(name) => Some(name.as_str().to_string()),
        Expr::Field { base, field } => {
            let base_name = extract_receiver_name(ctx, *base)?;
            Some(format!("{}.{}", base_name, field.as_str()))
        }
        _ => None,
    }
}

enum QualifiedCallInfo {
    TwoLevel { module: String },
    ThreeLevel { mdo_type: String, mdo_name: String },
}

fn analyze_qualified_call(node: &SyntaxNode, ctx: &LoweringCtx) -> Option<QualifiedCallInfo> {
    let first_child = active_children(node).next()?;

    if first_child.kind() == SyntaxKind::FIELD_EXPR {
        let inner_base = active_children(&first_child).next()?;
        let mdo_type = match inner_base.kind() {
            SyntaxKind::IDENT => inner_base.text().to_string(),
            SyntaxKind::EXPR => {
                let ident_nodes: Vec<_> = active_children(&inner_base)
                    .filter(|n| n.kind() == SyntaxKind::IDENT)
                    .collect();
                if ident_nodes.len() == 1 {
                    ident_nodes[0].text().to_string()
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        let mdo_name = first_child
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind() == SyntaxKind::IDENT)
            .last()
            .map(|tok| tok.text().to_string())?;

        // No ownership check here. What this branch still feeds is dependency
        // discovery, which runs BEFORE inference and is allowed to over-approximate:
        // one extra edge costs one extra invalidation, a missing edge costs a wrong
        // answer. The judgement that a local or a module variable holds the name
        // belongs to inference, which has the resolver — and the guard here was
        // incomplete anyway, seeing only declarations of the body.
        bsl_metadata::MdoType::from_plural(&mdo_type)?;

        tracing::debug!(
            mdo_type = %mdo_type,
            mdo_name = %mdo_name,
            "Detected three-level call"
        );
        return Some(QualifiedCallInfo::ThreeLevel { mdo_type, mdo_name });
    }

    let module_name = if first_child.kind() == SyntaxKind::IDENT {
        Some(first_child.text().to_string())
    } else if first_child.kind() == SyntaxKind::EXPR {
        let idents: Vec<_> =
            active_children(&first_child).filter(|n| n.kind() == SyntaxKind::IDENT).collect();
        if idents.len() == 1 {
            Some(idents[0].text().to_string())
        } else {
            None
        }
    } else {
        None
    };

    let module = module_name?;

    let key = NormName::intern(&module);
    if ctx.local_vars.contains_key(&key) || ctx.param_names.contains(&key) {
        return None;
    }

    Some(QualifiedCallInfo::TwoLevel { module })
}

fn lower_new_expr(ctx: &mut LoweringCtx, node: &SyntaxNode) -> Expr {
    let type_name = node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::IDENT)
        .map(|tok| Name::new(tok.text()));

    if let Some(ref name) = type_name {
        if is_file_system_type(name.as_str()) {
            ctx.emit(BodyDiagnostic::FileSystemAccess { range: node.text_range() });
        }
    }

    let style_type_name = if let Some(ref name) = type_name {
        if is_style_element_type(name.as_str()) {
            Some(name.as_str().to_string())
        } else {
            None
        }
    } else {
        extract_type_name_from_first_arg(node).filter(|name| is_style_element_type(name))
    };

    if let Some(style_name) = style_type_name {
        ctx.emit(BodyDiagnostic::StyleElementConstructors {
            type_name: style_name,
            range: node.text_range(),
        });
    }

    let is_system_info = if let Some(ref name) = type_name {
        is_system_information_type(name.as_str())
    } else {
        extract_type_name_from_first_arg(node)
            .map(|name| is_system_information_type(&name))
            .unwrap_or(false)
    };

    if is_system_info {
        ctx.emit(BodyDiagnostic::UseSystemInformation { range: node.text_range() });
    }

    if !ctx.in_platform_guard {
        let unix_unavailable_type = if let Some(ref name) = type_name {
            is_unix_unavailable_type(name.as_str()).then(|| name.as_str().to_string())
        } else {
            extract_type_name_from_first_arg(node).filter(|name| is_unix_unavailable_type(name))
        };

        if let Some(type_name_str) = unix_unavailable_type {
            ctx.emit(BodyDiagnostic::UsingObjectNotAvailableUnix {
                type_name: type_name_str,
                range: node.text_range(),
            });
        }
    }

    let args = active_children(node)
        .find(|n| n.kind() == SyntaxKind::ARG_LIST)
        .map(|arg_list| lower_arg_list(ctx, &arg_list))
        .unwrap_or_default();

    Expr::New { type_name, args: args.into_boxed_slice() }
}

pub(crate) fn exprs_are_equal(body: &Body, lhs: ExprIdx, rhs: ExprIdx) -> bool {
    match (body.expr_idx(lhs), body.expr_idx(rhs)) {
        (Expr::Missing, Expr::Missing) => true,
        (Expr::Path(name1), Expr::Path(name2)) => name1.eq_ignore_case(name2),
        (Expr::Field { base: b1, field: f1 }, Expr::Field { base: b2, field: f2 }) => {
            f1.eq_ignore_case(f2) && exprs_are_equal(body, *b1, *b2)
        }
        (Expr::Index { base: b1, index: i1 }, Expr::Index { base: b2, index: i2 }) => {
            exprs_are_equal(body, *b1, *b2) && exprs_are_equal(body, *i1, *i2)
        }
        (
            Expr::MethodCall { receiver: r1, method: m1, .. },
            Expr::MethodCall { receiver: r2, method: m2, .. },
        ) => m1.eq_ignore_case(m2) && exprs_are_equal(body, *r1, *r2),
        (Expr::Call { callee: c1, .. }, Expr::Call { callee: c2, .. }) => {
            exprs_are_equal(body, *c1, *c2)
        }
        _ => false,
    }
}

fn is_os_users_method(name: &str) -> bool {
    bsl_platform::security::registry()
        .lookup_global(name)
        .is_some_and(|e| matches!(e.category, bsl_platform::security::Category::OsUsers))
}

fn is_file_system_type(name: &str) -> bool {
    bsl_platform::security::registry()
        .lookup_constructor(name)
        .is_some_and(|e| matches!(e.category, bsl_platform::security::Category::FileSystem))
}

fn is_style_element_type(name: &str) -> bool {
    let lower = name.fold_lower();
    matches!(lower.as_str(), "цвет" | "color" | "шрифт" | "font" | "рамка" | "border")
}

fn is_system_information_type(name: &str) -> bool {
    has_capability_entry(name, CapabilityCategory::SystemInformation, CapabilityEntryKind::Type)
}

fn is_unix_unavailable_type(name: &str) -> bool {
    has_capability_entry(name, CapabilityCategory::UnixUnavailableObject, CapabilityEntryKind::Type)
}

fn is_write_log_event_method(name: &str) -> bool {
    let lower = name.fold_lower();
    lower == "записьжурналарегистрации" || lower == "writelogevent"
}

fn check_write_log_event_call(ctx: &mut LoweringCtx, node: &SyntaxNode) {
    let arg_list = match active_children(node).find(|n| n.kind() == SyntaxKind::ARG_LIST) {
        Some(al) => al,
        None => return,
    };

    let args = collect_arguments(&arg_list);
    let arg_count = args.len();

    let log_level_empty = args.get(1).map(|a| a.is_none()).unwrap_or(true);
    let comment_empty = args.get(4).map(|a| a.is_none()).unwrap_or(true);

    let has_error_log_level =
        args.get(1).and_then(|a| a.as_ref()).map(has_error_log_level_value).unwrap_or(false);

    let has_detail_error_description =
        args.get(4).and_then(|a| a.as_ref()).map(has_detail_error_description).unwrap_or(false);

    let has_detail_error_description = if !has_detail_error_description && ctx.in_except_block {
        if let Some(comment_arg) = args.get(4).and_then(|a| a.as_ref()) {
            resolve_comment_in_except_block(comment_arg, node).unwrap_or_default()
        } else {
            false
        }
    } else {
        has_detail_error_description
    };

    ctx.emit(BodyDiagnostic::UsageWriteLogEvent {
        in_except_block: ctx.in_except_block,
        arg_count,
        log_level_empty,
        comment_empty,
        has_error_log_level,
        has_detail_error_description,
        except_has_raise: ctx.except_has_raise,
        range: node.text_range(),
    });
}

fn collect_arguments(arg_list: &SyntaxNode) -> Vec<Option<SyntaxNode>> {
    let mut args: Vec<Option<SyntaxNode>> = Vec::new();
    let mut current_arg: Option<SyntaxNode> = None;
    let mut has_content = false;

    for child in active_children_with_tokens(arg_list) {
        match child.kind() {
            SyntaxKind::COMMA => {
                args.push(current_arg.take());
                has_content = true;
            }
            SyntaxKind::L_PAREN | SyntaxKind::R_PAREN => {}
            kind if kind.is_trivia() => {}
            _ => {
                if current_arg.is_none() {
                    if let Some(node) = child.as_node() {
                        current_arg = Some(node.clone());
                        has_content = true;
                    }
                }
            }
        }
    }

    if current_arg.is_some() || has_content {
        args.push(current_arg);
    }

    args
}

fn has_error_log_level_value(arg: &SyntaxNode) -> bool {
    let text = arg.text().to_string().fold_lower();
    if text.contains("уровеньжурналарегистрации") || text.contains("eventloglevel")
    {
        return text.contains("ошибка") || text.contains("error");
    }
    true
}

fn has_detail_error_description(arg: &SyntaxNode) -> bool {
    let text = arg.text().to_string().fold_lower();
    (text.contains("подробноепредставлениеошибки") || text.contains("detailerrordescription"))
        && (text.contains("информацияобошибке") || text.contains("errorinfo"))
}

fn resolve_comment_in_except_block(arg: &SyntaxNode, call_node: &SyntaxNode) -> Option<bool> {
    let arg_text = arg.text().to_string();
    let var_name = arg_text.trim();
    if var_name.is_empty() || !var_name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let var_name = var_name.fold_lower();

    let except_clause = call_node.ancestors().find(|n| n.kind() == SyntaxKind::EXCEPT_CLAUSE)?;

    let stmt_list = active_children(&except_clause).find(|n| n.kind() == SyntaxKind::STMT_LIST)?;

    for child in active_children(&stmt_list) {
        if child.kind() == SyntaxKind::ASSIGN_STMT {
            let lhs_node = match active_children(&child).next() {
                Some(n) => n,
                None => continue,
            };
            let lhs_name = lhs_node.text().to_string().trim().fold_lower();
            if lhs_name == var_name {
                let rhs_text = child.text().to_string().fold_lower();
                let has_detail = (rhs_text.contains("подробноепредставлениеошибки")
                    || rhs_text.contains("detailerrordescription"))
                    && (rhs_text.contains("информацияобошибке") || rhs_text.contains("errorinfo"));
                return Some(has_detail);
            }
        }
    }

    Some(true)
}

fn extract_type_name_from_first_arg(node: &SyntaxNode) -> Option<String> {
    let arg_list = active_children(node).find(|n| n.kind() == SyntaxKind::ARG_LIST)?;
    let first_arg = active_children(&arg_list).next()?;

    let string_token = first_arg
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::STRING)?;

    let text = string_token.text();
    if text.len() >= 2 && (text.starts_with('"') || text.starts_with('\'')) {
        Some(text[1..text.len() - 1].to_string())
    } else {
        None
    }
}

fn is_get_form_method(name: &str) -> bool {
    has_capability_entry(name, CapabilityCategory::GetForm, CapabilityEntryKind::AnyReceiverMethod)
}

fn is_form_data_to_value_global_method(name: &str) -> bool {
    has_capability_entry(
        name,
        CapabilityCategory::FormDataToValue,
        CapabilityEntryKind::GlobalMethod,
    )
}

fn is_get_form_global_method(name: &str) -> bool {
    has_capability_entry(name, CapabilityCategory::GetForm, CapabilityEntryKind::GlobalMethod)
}

fn has_capability_entry(
    name: &str,
    category: CapabilityCategory,
    kind: CapabilityEntryKind,
) -> bool {
    bsl_platform::capability::registry().lookup(category, kind, name).is_some()
}

fn is_proceed_with_call_method(name_lower: &str) -> bool {
    matches!(name_lower, "продолжитьвызов" | "proceedwithcall")
}

fn check_using_external_code_tools(
    ctx: &mut LoweringCtx,
    _actual_callee: &SyntaxNode,
    idents: &[syntax::SyntaxToken],
    call_node: &SyntaxNode,
) {
    use super::diagnostics::{is_external_code_tools_method, is_external_code_tools_name};

    if idents.len() != 2 {
        return;
    }

    let receiver_name = idents[0].text();
    let method_name = idents[1].text();

    let receiver_key = NormName::intern(receiver_name);
    let is_local =
        ctx.local_vars.contains_key(&receiver_key) || ctx.param_names.contains(&receiver_key);

    if !is_local
        && is_external_code_tools_name(receiver_name)
        && is_external_code_tools_method(method_name)
    {
        ctx.emit(BodyDiagnostic::UsingExternalCodeTools { range: call_node.text_range() });
    }
}

fn is_str_template_method(name: &str) -> bool {
    let lower = name.fold_lower();
    matches!(lower.as_str(), "стршаблон" | "strtemplate")
}

fn is_wrong_str_template(template_string: &str, used_params_count: usize) -> bool {
    let is_wrong_call = compare_template_and_params(template_string, used_params_count);
    if !is_wrong_call {
        return false;
    }

    let cleaned = remove_double_percent(template_string);
    compare_template_and_params(&cleaned, used_params_count)
}

fn remove_double_percent(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'%' && bytes[i + 1] == b'%' {
            i += 2;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

fn parse_placeholder(bytes: &[u8], pos: usize) -> Option<(usize, usize)> {
    if pos >= bytes.len() || bytes[pos] != b'%' {
        return None;
    }

    let start = pos + 1;
    if start >= bytes.len() {
        return None;
    }

    if bytes[start] == b'(' {
        let num_start = start + 1;
        let mut num_end = num_start;
        while num_end < bytes.len() && bytes[num_end].is_ascii_digit() {
            num_end += 1;
        }
        if num_end > num_start && num_end < bytes.len() && bytes[num_end] == b')' {
            let num_str = std::str::from_utf8(&bytes[num_start..num_end]).ok()?;
            let num: usize = num_str.parse().ok()?;
            return Some((num, num_end - pos + 1));
        }
        return None;
    }

    let mut num_end = start;
    while num_end < bytes.len() && bytes[num_end].is_ascii_digit() {
        num_end += 1;
    }
    if num_end > start {
        let num_str = std::str::from_utf8(&bytes[start..num_end]).ok()?;
        let num: usize = num_str.parse().ok()?;
        return Some((num, num_end - pos));
    }

    None
}

#[allow(clippy::nonminimal_bool)]
fn compare_template_and_params(template_string: &str, used_params_count: usize) -> bool {
    let bytes = template_string.as_bytes();
    let have_params = used_params_count > 0;

    let mut has_valid_placeholder = false;
    let mut has_wrong_number = false;
    let mut used_placeholders = [false; 11];

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'%' {
                i += 2;
                continue;
            }

            if let Some((num, len)) = parse_placeholder(bytes, i) {
                if (1..=10).contains(&num) {
                    has_valid_placeholder = true;
                    used_placeholders[num] = true;
                    if num > used_params_count {
                        return true;
                    }
                } else {
                    has_wrong_number = true;
                }
                i += len;
                continue;
            }
        }
        i += 1;
    }

    if has_wrong_number {
        return true;
    }
    if has_valid_placeholder && !have_params {
        return true;
    }
    if !has_valid_placeholder && have_params {
        return true;
    }

    if has_valid_placeholder {
        for &used in used_placeholders.iter().take(used_params_count + 1).skip(1) {
            if !used {
                return true;
            }
        }
    }

    false
}

fn find_string_in_node(node: &SyntaxNode) -> Option<String> {
    for token in node.descendants_with_tokens() {
        if let syntax::NodeOrToken::Token(t) = token {
            if t.kind() == SyntaxKind::STRING {
                let text = t.text().to_string();
                if text.len() > 2 {
                    return Some(text[1..text.len() - 1].to_string());
                }
            }
        }
    }
    None
}

fn check_find_element_first_arg(args: &[ExprIdx], ctx: &LoweringCtx) -> bool {
    if args.is_empty() {
        return true;
    }

    let first_arg = args[0];
    let expr = ctx.body.expr_idx(first_arg);
    matches!(expr, Expr::Literal(Literal::String(_)) | Expr::Literal(Literal::Number(_)))
}

fn determine_magic_number_context(token: &syntax::SyntaxToken) -> MagicNumberContext {
    let mut node = token.parent();
    let mut prev: Option<SyntaxNode> = None;

    let mut in_binary_expr = false;
    let mut in_arg_list = false;
    let mut arg_index: usize = 0;
    let mut in_ternary = false;
    let mut in_call = false;
    let mut in_assign = false;
    let mut has_dot_in_assign = false;

    while let Some(current) = node {
        match current.kind() {
            SyntaxKind::PARAM => {
                return MagicNumberContext::InDefaultParam;
            }
            SyntaxKind::INDEX_EXPR => {
                return MagicNumberContext::InArrayIndex;
            }
            SyntaxKind::NEW_EXPR => {
                if let Some(type_name) = current
                    .children_with_tokens()
                    .filter_map(|el| el.into_token())
                    .find(|tok| tok.kind() == SyntaxKind::IDENT)
                {
                    let name = type_name.text().fold_lower();
                    if name.contains("структура")
                        || name.contains("structure")
                        || name.contains("соответствие")
                        || name.contains("map")
                    {
                        return MagicNumberContext::InStructureConstructor;
                    }
                    return MagicNumberContext::InConstructor { type_name: name };
                }
            }
            SyntaxKind::BINARY_EXPR => {
                in_binary_expr = true;
            }
            SyntaxKind::ARG_LIST => {
                in_arg_list = true;
                arg_index = current
                    .children_with_tokens()
                    .take_while(|child| !child.text_range().contains_range(token.text_range()))
                    .filter(|child| child.as_token().is_some_and(|t| t.kind() == SyntaxKind::COMMA))
                    .count();
            }
            SyntaxKind::TERNARY_EXPR => {
                in_ternary = true;
            }
            SyntaxKind::CALL_STMT | SyntaxKind::CALL_EXPR => {
                in_call = true;
                if let Some(method_name) = find_method_name_for_magic_number(&current) {
                    let name = method_name.fold_lower();
                    if name == "вставить" || name == "insert" {
                        return MagicNumberContext::InStructureInsert;
                    }
                    if (name == "окр" || name == "round") && arg_index == 1 {
                        return MagicNumberContext::InRoundPrecision;
                    }
                }
            }
            SyntaxKind::ASSIGN_STMT => {
                in_assign = true;
                has_dot_in_assign = current
                    .children_with_tokens()
                    .any(|el| el.as_token().is_some_and(|t| t.kind() == SyntaxKind::DOT));
            }
            SyntaxKind::RETURN_STMT => {
                return MagicNumberContext::InReturn;
            }
            // The init (`= N`) and bound (`По N`) of a For loop are boundaries, not magic
            // — but only when the number is a standalone literal. A number in a compound
            // boundary expression (`По Массив.ВГраница() - 5`) or a call stays magic,
            // matching bsl-ls. The init/bound expressions precede the `Цикл` (KW_DO)
            // keyword; numbers in the loop body sit after it and keep their own context.
            SyntaxKind::FOR_STMT if !in_binary_expr && !in_call && !in_arg_list => {
                let do_kw = current
                    .children_with_tokens()
                    .filter_map(|el| el.into_token())
                    .find(|t| t.kind() == SyntaxKind::KW_DO);
                let in_header = match (&do_kw, &prev) {
                    (Some(kw), Some(child)) => child.text_range().end() <= kw.text_range().start(),
                    _ => false,
                };
                if in_header {
                    return MagicNumberContext::InForLoopBoundary;
                }
            }
            SyntaxKind::FUNCTION_DEF | SyntaxKind::PROCEDURE_DEF => {
                break;
            }
            _ => {}
        }
        node = current.parent();
        prev = Some(current);
    }

    if in_assign {
        if has_dot_in_assign && !in_arg_list {
            return MagicNumberContext::InPropertyAssignment;
        }
        if !in_binary_expr && !in_arg_list {
            return MagicNumberContext::InSimpleAssignment;
        }
        if in_ternary && !in_binary_expr {
            return MagicNumberContext::InTernaryBranch;
        }
    }

    if in_call && in_arg_list && !in_binary_expr {
        return MagicNumberContext::InMethodCall;
    }

    if in_binary_expr {
        return MagicNumberContext::InExpression;
    }

    MagicNumberContext::Other
}

fn find_method_name_for_magic_number(node: &SyntaxNode) -> Option<String> {
    for child in node.descendants() {
        if child.kind() == SyntaxKind::FIELD_EXPR {
            return child
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == SyntaxKind::IDENT)
                .last()
                .map(|t| t.text().to_string());
        }
        if child.kind() == SyntaxKind::ARG_LIST {
            break;
        }
    }

    for child in node.children_with_tokens() {
        match child {
            syntax::NodeOrToken::Token(t) if t.kind() == SyntaxKind::IDENT => {
                return Some(t.text().to_string());
            }
            syntax::NodeOrToken::Node(n) if n.kind() == SyntaxKind::IDENT => {
                if let Some(t) = n.first_token() {
                    return Some(t.text().to_string());
                }
            }
            syntax::NodeOrToken::Node(n) if n.kind() == SyntaxKind::ARG_LIST => break,
            _ => {}
        }
    }

    None
}

fn is_unsafe_safe_mode_context(call_node: &SyntaxNode) -> bool {
    let mut current = call_node.parent();

    while let Some(node) = current {
        match node.kind() {
            SyntaxKind::UNARY_EXPR
                if node
                    .children_with_tokens()
                    .filter_map(|el| el.into_token())
                    .any(|tok| tok.kind() == SyntaxKind::KW_NOT) =>
            {
                return true;
            }
            SyntaxKind::BINARY_EXPR => {
                let has_comparison =
                    node.children_with_tokens().filter_map(|el| el.into_token()).any(|tok| {
                        matches!(
                            tok.kind(),
                            SyntaxKind::EQ
                                | SyntaxKind::NEQ
                                | SyntaxKind::LT
                                | SyntaxKind::LE
                                | SyntaxKind::GT
                                | SyntaxKind::GE
                        )
                    });
                if has_comparison {
                    return false;
                }
                let has_boolean = node
                    .children_with_tokens()
                    .filter_map(|el| el.into_token())
                    .any(|tok| matches!(tok.kind(), SyntaxKind::KW_AND | SyntaxKind::KW_OR));
                if has_boolean {
                    return true;
                }
            }
            SyntaxKind::PAREN_EXPR | SyntaxKind::EXPR => {}
            SyntaxKind::IF_STMT | SyntaxKind::ELSIF_CLAUSE => {
                if let Some(cond) = active_children(&node).find(|n| n.kind() == SyntaxKind::EXPR) {
                    let call_range = call_node.text_range();
                    let contains_call = cond
                        .descendants()
                        .any(|n| n.kind() == SyntaxKind::CALL_EXPR && n.text_range() == call_range);
                    if contains_call {
                        let has_comparison = cond.descendants().any(|n| {
                            n.kind() == SyntaxKind::BINARY_EXPR
                                && n.children_with_tokens().filter_map(|el| el.into_token()).any(
                                    |tok| {
                                        matches!(
                                            tok.kind(),
                                            SyntaxKind::EQ
                                                | SyntaxKind::NEQ
                                                | SyntaxKind::LT
                                                | SyntaxKind::LE
                                                | SyntaxKind::GT
                                                | SyntaxKind::GE
                                        )
                                    },
                                )
                        });
                        return !has_comparison;
                    }
                }
                return false;
            }
            SyntaxKind::ASSIGN_STMT => {
                if let Some(rhs_node) = active_children(&node).nth(1) {
                    let call_range = call_node.text_range();
                    if rhs_node.text_range() == call_range {
                        return false;
                    }
                    if rhs_node.kind() == SyntaxKind::EXPR {
                        if let Some(inner) = active_children(&rhs_node).next() {
                            if inner.text_range() == call_range {
                                return false;
                            }
                        }
                    }
                }
                return false;
            }
            SyntaxKind::ARG_LIST | SyntaxKind::CALL_EXPR | SyntaxKind::CALL_STMT => {
                return false;
            }
            SyntaxKind::STMT_LIST
            | SyntaxKind::PROCEDURE_DEF
            | SyntaxKind::FUNCTION_DEF
            | SyntaxKind::SOURCE_FILE => {
                break;
            }
            _ => {}
        }
        current = node.parent();
    }

    false
}
