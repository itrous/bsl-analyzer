use crate::hir::ExprHir;
use crate::hir::{SdblHir, SelectHir};
use crate::types::SdblType;

use crate::lower::context::LoweringContext;

fn is_column_ref_pattern(expr: &ExprHir) -> bool {
    match expr {
        ExprHir::ColumnRef { .. } => true,
        ExprHir::UnaryOp { expr: inner, .. } => is_column_ref_pattern(inner),
        _ => false,
    }
}

impl LoweringContext<'_> {
    fn lower_in_subquery(&mut self, node: &syntax::SyntaxNode) -> SdblHir {
        use syntax::ast::AstNode;

        let subquery_node = if node.kind() == syntax::SyntaxKind::SDBL_SUBQUERY {
            Some(node.clone())
        } else {
            node.descendants().find(|n| n.kind() == syntax::SyntaxKind::SDBL_SUBQUERY)
        };

        if let Some(sq_node) = subquery_node {
            if let Some(subquery) = syntax::ast::SdblSubquery::cast(sq_node) {
                let queries: Vec<_> = subquery.queries().collect();
                let has_union_siblings = queries.len() > 1;

                if let Some(main_query) = queries.first() {
                    self.scope.push_frame();
                    let mut main_hir = self.lower_query(main_query, has_union_siblings, true);
                    self.scope.pop_frame();

                    for union_query in queries.iter().skip(1) {
                        self.scope.push_frame();
                        let union_hir = self.lower_query(union_query, true, false);
                        self.scope.pop_frame();
                        main_hir.diagnostics.extend(union_hir.diagnostics);
                    }

                    return main_hir;
                }
            }
        }

        SdblHir {
            select: SelectHir { fields: Vec::new(), distinct: false, top: None },
            into_table: None,
            from: Vec::new(),
            joins: Vec::new(),
            where_clause: None,
            group_by: None,
            having: None,
            order_by: None,
            unions: Vec::new(),
            diagnostics: Vec::new(),
            range: node.text_range(),
        }
    }

    pub(super) fn lower_is_null_expr(&mut self, node: &syntax::SyntaxNode) -> ExprHir {
        let expr = node
            .children()
            .next()
            .map(|n| self.lower_expr(&n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        let negated = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|token| token.kind() == syntax::SyntaxKind::KW_NOT);

        self.record_keyword_by_text(
            node,
            "IS",
            "ЕСТЬ",
            crate::source_map::TokenCategory::SpecialKeyword,
        );

        if negated {
            self.record_direct_token(node, syntax::SyntaxKind::KW_NOT);
        }

        self.record_keyword_by_text(
            node,
            "NULL",
            "NULL",
            crate::source_map::TokenCategory::SpecialKeyword,
        );

        ExprHir::IsNull {
            expr: Box::new(expr),
            negated,
            ty: SdblType::Boolean,
            range: node.text_range(),
        }
    }

    pub(super) fn lower_in_expr(&mut self, node: &syntax::SyntaxNode) -> ExprHir {
        use crate::hir::InValues;

        let expr = node
            .children()
            .next()
            .map(|n| self.lower_expr(&n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        let negated = node.descendants_with_tokens().any(|el| {
            el.as_token().map(|t| t.kind() == syntax::SyntaxKind::KW_NOT).unwrap_or(false)
        });

        if negated {
            self.record_direct_token(node, syntax::SyntaxKind::KW_NOT);
        }

        for element in node.descendants_with_tokens() {
            if let Some(token) = element.as_token() {
                if token.kind() == syntax::SyntaxKind::KW_IN {
                    self.record_token(token, crate::source_map::TokenCategory::SpecialKeyword);
                    break;
                }
            }
        }

        let mut children = node.children().skip(1);
        let values = if let Some(child) = children.next() {
            if matches!(
                child.kind(),
                syntax::SyntaxKind::SDBL_SUBQUERY_EXPR
                    | syntax::SyntaxKind::SDBL_SELECT_QUERY
                    | syntax::SyntaxKind::SDBL_SUBQUERY
            ) {
                let subquery_hir = self.lower_in_subquery(&child);
                InValues::Subquery(Box::new(subquery_hir))
            } else {
                let mut values = vec![self.lower_expr(&child)];
                for expr_node in children {
                    if matches!(
                        expr_node.kind(),
                        syntax::SyntaxKind::SDBL_COLUMN_REF
                            | syntax::SyntaxKind::SDBL_LITERAL
                            | syntax::SyntaxKind::SDBL_MULTI_STRING
                            | syntax::SyntaxKind::SDBL_FUNCTION_CALL
                            | syntax::SyntaxKind::SDBL_PAREN_EXPR
                            | syntax::SyntaxKind::SDBL_LOGICAL_OR_EXPR
                            | syntax::SyntaxKind::SDBL_LOGICAL_AND_EXPR
                            | syntax::SyntaxKind::SDBL_COMPARISON_EXPR
                            | syntax::SyntaxKind::SDBL_ADDITIVE_EXPR
                            | syntax::SyntaxKind::SDBL_MULTIPLICATIVE_EXPR
                            | syntax::SyntaxKind::SDBL_UNARY_EXPR
                            | syntax::SyntaxKind::SDBL_PARAMETER
                    ) {
                        values.push(self.lower_expr(&expr_node));
                    }
                }
                InValues::List(values)
            }
        } else {
            InValues::List(Vec::new())
        };

        ExprHir::In {
            expr: Box::new(expr),
            negated,
            values,
            ty: SdblType::Boolean,
            range: node.text_range(),
        }
    }

    pub(super) fn lower_in_hierarchy_expr(&mut self, node: &syntax::SyntaxNode) -> ExprHir {
        use crate::hir::InValues;

        let mut children = node.children();
        let expr = children
            .next()
            .map(|n| self.lower_expr(&n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        self.record_direct_token(node, syntax::SyntaxKind::KW_NOT);

        for element in node.descendants_with_tokens() {
            if let Some(token) = element.as_token() {
                if token.kind() == syntax::SyntaxKind::KW_IN {
                    self.record_token(token, crate::source_map::TokenCategory::SpecialKeyword);
                    break;
                }
            }
        }

        self.record_keyword_by_text(
            node,
            "HIERARCHY",
            "ИЕРАРХИИ",
            crate::source_map::TokenCategory::SpecialKeyword,
        );

        let Some(root_node) = children.next() else {
            return ExprHir::BinaryOp {
                lhs: Box::new(expr),
                op: crate::hir::BinaryOp::Eq,
                rhs: Box::new(ExprHir::Missing { range: node.text_range() }),
                ty: SdblType::Boolean,
                range: node.text_range(),
            };
        };

        // A hierarchy root given as a subquery must be lowered through the
        // subquery path so its tables and fields resolve and nested diagnostics
        // surface; the resolution walks only descend into `In`/`Subquery`.
        if matches!(
            root_node.kind(),
            syntax::SyntaxKind::SDBL_SUBQUERY_EXPR
                | syntax::SyntaxKind::SDBL_SELECT_QUERY
                | syntax::SyntaxKind::SDBL_SUBQUERY
        ) {
            let subquery_hir = self.lower_in_subquery(&root_node);
            return ExprHir::In {
                expr: Box::new(expr),
                negated: false,
                values: InValues::Subquery(Box::new(subquery_hir)),
                ty: SdblType::Boolean,
                range: node.text_range(),
            };
        }

        let root_expr = self.lower_expr(&root_node);
        ExprHir::BinaryOp {
            lhs: Box::new(expr),
            op: crate::hir::BinaryOp::Eq,
            rhs: Box::new(root_expr),
            ty: SdblType::Boolean,
            range: node.text_range(),
        }
    }

    pub(super) fn lower_between_expr(&mut self, node: &syntax::SyntaxNode) -> ExprHir {
        let mut children = node.children();
        let expr = children
            .next()
            .map(|n| self.lower_expr(&n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        let negated = node.descendants_with_tokens().any(|el| {
            el.as_token().map(|t| t.kind() == syntax::SyntaxKind::KW_NOT).unwrap_or(false)
        });

        if negated {
            self.record_direct_token(node, syntax::SyntaxKind::KW_NOT);
        }

        self.record_direct_token(node, syntax::SyntaxKind::KW_AND);

        self.record_keyword_by_text(
            node,
            "BETWEEN",
            "МЕЖДУ",
            crate::source_map::TokenCategory::SpecialKeyword,
        );

        let low = children
            .next()
            .map(|n| self.lower_expr(&n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        let high = children
            .next()
            .map(|n| self.lower_expr(&n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        ExprHir::Between {
            expr: Box::new(expr),
            negated,
            low: Box::new(low),
            high: Box::new(high),
            ty: SdblType::Boolean,
            range: node.text_range(),
        }
    }

    pub(super) fn lower_like_expr(&mut self, node: &syntax::SyntaxNode) -> ExprHir {
        let mut children_iter = node.children();
        let first_child = children_iter.next();
        let expr = first_child
            .as_ref()
            .map(|n| self.lower_expr(n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        let negated = node.descendants_with_tokens().any(|el| {
            el.as_token().map(|t| t.kind() == syntax::SyntaxKind::KW_NOT).unwrap_or(false)
        });

        if negated {
            self.record_direct_token(node, syntax::SyntaxKind::KW_NOT);
        }

        self.record_keyword_by_text(
            node,
            "LIKE",
            "ПОДОБНО",
            crate::source_map::TokenCategory::SpecialKeyword,
        );

        let pattern_child = children_iter.next();
        let pattern = pattern_child
            .as_ref()
            .map(|n| self.lower_expr(n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        let escape_child = children_iter.next();
        let escape = escape_child.as_ref().map(|n| Box::new(self.lower_expr(n)));

        if escape.is_some() {
            self.record_keyword_by_text(
                node,
                "ESCAPE",
                "СПЕЦСИМВОЛ",
                crate::source_map::TokenCategory::SpecialKeyword,
            );
        }

        let tight_range = {
            let start = node.text_range().start();
            let end = node
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .filter(|t| {
                    !matches!(
                        t.kind(),
                        syntax::SyntaxKind::WHITESPACE
                            | syntax::SyntaxKind::NEWLINE
                            | syntax::SyntaxKind::COMMENT
                    )
                })
                .last()
                .map(|t| t.text_range().end())
                .unwrap_or_else(|| node.text_range().end());
            syntax::TextRange::new(start, end)
        };

        let kind = if is_column_ref_pattern(&pattern) {
            crate::diagnostics::LikeUsageKind::Incorrect
        } else {
            crate::diagnostics::LikeUsageKind::Allowed
        };
        self.diagnostics
            .push(crate::diagnostics::SdblDiagnostic::LikeUsage { range: tight_range, kind });

        ExprHir::Like {
            expr: Box::new(expr),
            negated,
            pattern: Box::new(pattern),
            escape,
            ty: SdblType::Boolean,
            range: node.text_range(),
        }
    }

    pub(super) fn lower_refs_expr(&mut self, node: &syntax::SyntaxNode) -> ExprHir {
        let mut children = node.children();
        let expr = children
            .next()
            .map(|n| self.lower_expr(&n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        self.record_keyword_by_text(
            node,
            "REFS",
            "ССЫЛКА",
            crate::source_map::TokenCategory::SpecialKeyword,
        );

        ExprHir::BinaryOp {
            lhs: Box::new(expr),
            op: crate::hir::BinaryOp::Eq,
            rhs: Box::new(ExprHir::Missing { range: node.text_range() }),
            ty: SdblType::Boolean,
            range: node.text_range(),
        }
    }

    fn record_direct_token(&mut self, node: &syntax::SyntaxNode, kind: syntax::SyntaxKind) {
        if let Some(token) = node
            .children_with_tokens()
            .filter_map(|element| element.into_token())
            .find(|token| token.kind() == kind)
        {
            self.record_token(&token, crate::source_map::TokenCategory::Operator);
        }
    }
}
