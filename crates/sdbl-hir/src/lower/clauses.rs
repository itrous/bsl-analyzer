use crate::diagnostics::SdblDiagnostic;
use crate::hir::{ExprHir, FieldHir, SelectHir};
use stdx::case::CaseExt;
use syntax::ast::AstNode;

use super::context::LoweringContext;

impl LoweringContext<'_> {
    pub(super) fn lower_where_clause(
        &mut self,
        where_clause: &syntax::ast::SdblWhereClause,
    ) -> ExprHir {
        self.record_keyword_by_text(
            where_clause.syntax(),
            "WHERE",
            "ГДЕ",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        self.report_or_in_where(where_clause.syntax());

        let expr_node = where_clause.syntax().children().find(|n| {
            matches!(
                n.kind(),
                syntax::SyntaxKind::SDBL_LOGICAL_OR_EXPR
                    | syntax::SyntaxKind::SDBL_LOGICAL_AND_EXPR
                    | syntax::SyntaxKind::SDBL_NOT_EXPR
                    | syntax::SyntaxKind::SDBL_COMPARISON_EXPR
                    | syntax::SyntaxKind::SDBL_COLUMN_REF
                    | syntax::SyntaxKind::SDBL_LITERAL
                    | syntax::SyntaxKind::SDBL_MULTI_STRING
                    | syntax::SyntaxKind::SDBL_FUNCTION_CALL
                    | syntax::SyntaxKind::SDBL_PAREN_EXPR
            )
        });

        if let Some(expr) = expr_node {
            self.lower_expr(&expr)
        } else {
            ExprHir::Missing { range: where_clause.syntax().text_range() }
        }
    }

    pub(super) fn lower_having_clause(
        &mut self,
        having_clause: &syntax::ast::SdblHavingClause,
    ) -> ExprHir {
        self.record_keyword_by_text(
            having_clause.syntax(),
            "HAVING",
            "ИМЕЮЩИЕ",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        let expr_node = having_clause.syntax().children().find(|n| {
            matches!(
                n.kind(),
                syntax::SyntaxKind::SDBL_LOGICAL_OR_EXPR
                    | syntax::SyntaxKind::SDBL_LOGICAL_AND_EXPR
                    | syntax::SyntaxKind::SDBL_NOT_EXPR
                    | syntax::SyntaxKind::SDBL_COMPARISON_EXPR
                    | syntax::SyntaxKind::SDBL_COLUMN_REF
                    | syntax::SyntaxKind::SDBL_LITERAL
                    | syntax::SyntaxKind::SDBL_MULTI_STRING
                    | syntax::SyntaxKind::SDBL_FUNCTION_CALL
                    | syntax::SyntaxKind::SDBL_PAREN_EXPR
            )
        });

        if let Some(expr) = expr_node {
            self.lower_expr(&expr)
        } else {
            ExprHir::Missing { range: having_clause.syntax().text_range() }
        }
    }

    pub(super) fn lower_group_by(
        &mut self,
        group_clause: &syntax::ast::SdblGroupClause,
    ) -> crate::hir::GroupByHir {
        self.record_keyword_by_text(
            group_clause.syntax(),
            "GROUP",
            "СГРУППИРОВАТЬ",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        self.record_keyword_by_text(
            group_clause.syntax(),
            "BY",
            "ПО",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        let exprs: Vec<_> = group_clause
            .syntax()
            .children()
            .filter(|n| {
                matches!(
                    n.kind(),
                    syntax::SyntaxKind::SDBL_LOGICAL_OR_EXPR
                        | syntax::SyntaxKind::SDBL_COLUMN_REF
                        | syntax::SyntaxKind::SDBL_FUNCTION_CALL
                )
            })
            .map(|n| self.lower_expr(&n))
            .collect();

        crate::hir::GroupByHir { exprs, range: group_clause.syntax().text_range() }
    }

    pub(super) fn lower_order_by(
        &mut self,
        order_clause: &syntax::ast::SdblOrderClause,
    ) -> crate::hir::OrderByHir {
        self.record_keyword_by_text(
            order_clause.syntax(),
            "ORDER",
            "УПОРЯДОЧИТЬ",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        self.record_keyword_by_text(
            order_clause.syntax(),
            "BY",
            "ПО",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        let mut items = Vec::new();
        let mut current_expr: Option<ExprHir> = None;

        for child in order_clause.syntax().children_with_tokens() {
            match child {
                syntax::NodeOrToken::Node(node) => {
                    if matches!(
                        node.kind(),
                        syntax::SyntaxKind::SDBL_LOGICAL_OR_EXPR
                            | syntax::SyntaxKind::SDBL_COLUMN_REF
                            | syntax::SyntaxKind::SDBL_FUNCTION_CALL
                    ) {
                        if let Some(expr) = current_expr.take() {
                            items.push(crate::hir::OrderByItem {
                                expr,
                                direction: crate::hir::SortDirection::Asc,
                            });
                        }
                        current_expr = Some(self.lower_expr(&node));
                    }
                }
                syntax::NodeOrToken::Token(token) => {
                    if token.kind() == syntax::SyntaxKind::IDENT {
                        let text = token.text().to_uppercase();
                        if matches!(text.as_str(), "ASC" | "ВОЗР") {
                            if let Some(expr) = current_expr.take() {
                                items.push(crate::hir::OrderByItem {
                                    expr,
                                    direction: crate::hir::SortDirection::Asc,
                                });
                            }
                        } else if matches!(text.as_str(), "DESC" | "УБЫВ") {
                            if let Some(expr) = current_expr.take() {
                                items.push(crate::hir::OrderByItem {
                                    expr,
                                    direction: crate::hir::SortDirection::Desc,
                                });
                            }
                        }
                    }
                }
            }
        }

        if let Some(expr) = current_expr {
            items.push(crate::hir::OrderByItem { expr, direction: crate::hir::SortDirection::Asc });
        }

        crate::hir::OrderByHir { items, range: order_clause.syntax().text_range() }
    }

    pub(super) fn lower_index_by_clause(
        &mut self,
        query_node: &syntax::SyntaxNode,
        select: &SelectHir,
    ) {
        let Some(index_clause) =
            query_node.children().find(|n| n.kind() == syntax::SyntaxKind::SDBL_INDEX_BY)
        else {
            return;
        };

        self.record_keyword_by_text(
            &index_clause,
            "INDEX",
            "ИНДЕКСИРОВАТЬ",
            crate::source_map::TokenCategory::ClauseKeyword,
        );
        self.record_keyword_by_text(
            &index_clause,
            "BY",
            "ПО",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        for child in index_clause.children() {
            if let Some((column_ref, _)) = simple_selected_output_ref(&child, select) {
                if let Some(token) =
                    column_ref.children_with_tokens().find_map(|it| it.into_token())
                {
                    self.record_token(&token, crate::source_map::TokenCategory::FieldAlias);
                }
            } else if matches!(
                child.kind(),
                syntax::SyntaxKind::SDBL_LOGICAL_OR_EXPR
                    | syntax::SyntaxKind::SDBL_COLUMN_REF
                    | syntax::SyntaxKind::SDBL_FUNCTION_CALL
            ) {
                self.lower_expr(&child);
            }
        }
    }

    /// Pushes a frame holding the select list's output columns; the caller pops it.
    ///
    /// A bare name after `УПОРЯДОЧИТЬ ПО` or in `ИТОГИ` is first of all a result column.
    /// Resolving it against the sources instead finds the same name in two joined tables and
    /// reports an ambiguity the platform does not see. `opaque` closes the frame for clauses
    /// that cannot reach the sources at all.
    pub(super) fn push_select_outputs_frame(
        &mut self,
        select: &SelectHir,
        opaque: bool,
        range: text_size::TextRange,
    ) {
        self.scope.push_outputs_frame(opaque);
        let outputs = crate::hir::TableRef {
            parts: Vec::new(),
            full_name: String::new(),
            alias: None,
            metadata: Some(crate::hir::ResolvedTable::TempTable {
                name: String::new(),
                fields: select
                    .fields
                    .iter()
                    .filter_map(|field| {
                        field
                            .alias_or_name()
                            .map(|name| crate::hir::FieldDef::new(name.as_str(), field.ty.clone()))
                    })
                    .collect(),
                field_model_complete: false,
            }),
            is_virtual_table: false,
            virtual_table_params: Vec::new(),
            subquery: Vec::new(),
            range,
        };
        // A fresh frame: there is nothing for the outputs to collide with.
        let _ = self.scope.add_table(outputs);
    }

    pub(super) fn lower_totals_by_clause(
        &mut self,
        query_node: &syntax::SyntaxNode,
        select: &SelectHir,
    ) {
        let Some(totals_clause) =
            query_node.children().find(|n| n.kind() == syntax::SyntaxKind::SDBL_TOTALS_BY)
        else {
            return;
        };

        // `ИТОГИ` speaks of the result columns only: a bare name there is a select output, never
        // a source field.
        self.push_select_outputs_frame(select, true, totals_clause.text_range());

        let mut after_by = false;

        for child in totals_clause.children_with_tokens() {
            match child {
                syntax::NodeOrToken::Node(node)
                    if node.kind() == syntax::SyntaxKind::SDBL_ALIAS =>
                {
                    if let Some(alias) = syntax::ast::SdblAlias::cast(node) {
                        self.record_keyword_by_text(
                            alias.syntax(),
                            "AS",
                            "КАК",
                            crate::source_map::TokenCategory::SpecialKeyword,
                        );
                        if let Some(token) = alias.identifier() {
                            self.record_token(&token, crate::source_map::TokenCategory::FieldAlias);
                        }
                    }
                }
                syntax::NodeOrToken::Token(token) if token.kind() == syntax::SyntaxKind::IDENT => {
                    let text = token.text();
                    if text.eq_ignore_ascii_case("TOTALS") || text.eq_ignore_ascii_case("ИТОГИ")
                    {
                        self.record_token(&token, crate::source_map::TokenCategory::ClauseKeyword);
                    } else if text.eq_ignore_ascii_case("BY") || text.eq_ignore_ascii_case("ПО") {
                        self.record_token(&token, crate::source_map::TokenCategory::ClauseKeyword);
                        after_by = true;
                    } else if matches!(
                        text.to_uppercase().as_str(),
                        "ONLY" | "ТОЛЬКО" | "HIERARCHY" | "ИЕРАРХИЯ"
                    ) {
                        self.record_token(&token, crate::source_map::TokenCategory::Modifier);
                    }
                }
                syntax::NodeOrToken::Node(node) if is_sdbl_clause_expr(&node) => {
                    if after_by {
                        if let Some((column_ref, field)) = simple_selected_output_ref(&node, select)
                        {
                            if let Some(token) =
                                column_ref.children_with_tokens().find_map(|it| it.into_token())
                            {
                                self.record_token(
                                    &token,
                                    crate::source_map::TokenCategory::FieldAlias,
                                );
                            }
                            if field.ty.is_unlimited_string() {
                                self.diagnostics.push(SdblDiagnostic::UnlimitedStringUsage {
                                    field_name: field.alias_or_name().map(|name| name.to_string()),
                                    context:
                                        crate::diagnostics::UnlimitedStringUsageContext::TotalsBy,
                                    range: column_ref.text_range(),
                                });
                            }
                            continue;
                        }
                    }

                    let expr = self.lower_expr(&node);
                    if after_by {
                        self.flag_unlimited_string(
                            &expr,
                            crate::diagnostics::UnlimitedStringUsageContext::TotalsBy,
                        );
                    }
                }
                _ => {}
            }
        }

        self.scope.pop_frame();
    }

    pub(super) fn lower_drop_query(&mut self, drop_query: &syntax::SyntaxNode) {
        self.record_keyword_by_text(
            drop_query,
            "DROP",
            "УНИЧТОЖИТЬ",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        let table_token = drop_query
            .children_with_tokens()
            .filter_map(|element| element.into_token())
            .filter(|token| token.kind() == syntax::SyntaxKind::IDENT)
            .find(|token| {
                let text = token.text();
                !text.eq_ignore_ascii_case("DROP") && !text.eq_ignore_ascii_case("УНИЧТОЖИТЬ")
            });

        if let Some(table_token) = table_token {
            self.record_token(&table_token, crate::source_map::TokenCategory::TableName);
            self.scope.remove_temp_table(table_token.text());
        }
    }

    /// Reports every disjunction of this query's selection condition.
    ///
    /// Nested queries are skipped: each of them is lowered as a query of its own and reports
    /// its own condition, so descending here would report the same keyword twice.
    fn report_or_in_where(&mut self, where_clause: &syntax::SyntaxNode) {
        let mut walk = where_clause.preorder_with_tokens();
        while let Some(event) = walk.next() {
            let syntax::WalkEvent::Enter(element) = event else {
                continue;
            };
            match element {
                syntax::NodeOrToken::Node(node) if is_nested_query(&node) => walk.skip_subtree(),
                syntax::NodeOrToken::Token(token) if token.kind() == syntax::SyntaxKind::KW_OR => {
                    self.diagnostics
                        .push(SdblDiagnostic::LogicalOrInWhere { range: token.text_range() });
                }
                _ => {}
            }
        }
    }
}

fn is_sdbl_clause_expr(node: &syntax::SyntaxNode) -> bool {
    matches!(
        node.kind(),
        syntax::SyntaxKind::SDBL_LOGICAL_OR_EXPR
            | syntax::SyntaxKind::SDBL_LOGICAL_AND_EXPR
            | syntax::SyntaxKind::SDBL_NOT_EXPR
            | syntax::SyntaxKind::SDBL_COMPARISON_EXPR
            | syntax::SyntaxKind::SDBL_COLUMN_REF
            | syntax::SyntaxKind::SDBL_LITERAL
            | syntax::SyntaxKind::SDBL_MULTI_STRING
            | syntax::SyntaxKind::SDBL_FUNCTION_CALL
            | syntax::SyntaxKind::SDBL_PAREN_EXPR
    )
}

fn simple_selected_output_ref<'a>(
    node: &syntax::SyntaxNode,
    select: &'a SelectHir,
) -> Option<(syntax::SyntaxNode, &'a FieldHir)> {
    let mut column_refs =
        node.descendants().filter(|child| child.kind() == syntax::SyntaxKind::SDBL_COLUMN_REF);
    let column_ref = column_refs.next()?;
    if column_refs.next().is_some() {
        return None;
    }
    // Выражение обязано состоять из одной ссылки и ничего больше. Сравнение
    // по тексту считало бы комментарий за ссылкой лишней частью выражения и
    // молча выключало бы проверку.
    if !syntax::ast_utils::same_significant_tokens(node, &column_ref) {
        return None;
    }

    let idents: Vec<String> =
        syntax::ast_utils::direct_name_tokens(&column_ref).map(|t| t.text().to_string()).collect();
    let mut idents = idents.into_iter();

    let name = idents.next()?;
    if idents.next().is_some() {
        return None;
    }

    let matched_field = select.fields.iter().find(|field| {
        field
            .alias_or_name()
            .is_some_and(|output_name| output_name.fold_lower() == name.fold_lower())
    })?;

    Some((column_ref, matched_field))
}

fn is_nested_query(node: &syntax::SyntaxNode) -> bool {
    matches!(
        node.kind(),
        syntax::SyntaxKind::SDBL_SUBQUERY
            | syntax::SyntaxKind::SDBL_SUBQUERY_EXPR
            | syntax::SyntaxKind::SDBL_QUERY
    )
}
