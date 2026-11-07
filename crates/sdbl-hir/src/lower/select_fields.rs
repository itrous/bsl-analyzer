use crate::hir::{ExprHir, FieldHir, Name, SelectHir};
use crate::types::SdblType;
use syntax::ast::AstNode;

use super::context::LoweringContext;

impl LoweringContext<'_> {
    pub(super) fn lower_field_list(
        &mut self,
        field_list: Option<syntax::ast::SdblFieldList>,
        distinct: bool,
        top: Option<u32>,
    ) -> SelectHir {
        let Some(fl) = field_list else {
            return SelectHir::empty();
        };

        let fields: Vec<FieldHir> = fl.fields().map(|f| self.lower_selected_field(&f)).collect();

        SelectHir { fields, distinct, top }
    }

    pub(super) fn extract_limitations(
        &mut self,
        query_node: &syntax::SyntaxNode,
    ) -> (bool, Option<u32>, Option<syntax::TextRange>) {
        let mut distinct = false;
        let mut top = None;
        let mut top_range = None;

        for child in query_node.children() {
            if child.kind() == syntax::SyntaxKind::SDBL_LIMITATIONS {
                for element in child.descendants_with_tokens() {
                    if let Some(token) = element.as_token() {
                        if token.kind() == syntax::SyntaxKind::IDENT {
                            let text_upper = token.text().to_uppercase();
                            if text_upper == "DISTINCT" || text_upper == "РАЗЛИЧНЫЕ" {
                                distinct = true;
                                self.record_token(
                                    token,
                                    crate::source_map::TokenCategory::Modifier,
                                );
                            }
                        }
                    }
                }

                for top_child in child.children() {
                    if top_child.kind() == syntax::SyntaxKind::SDBL_TOP_CLAUSE {
                        for element in top_child.descendants_with_tokens() {
                            if let Some(token) = element.as_token() {
                                if token.kind() == syntax::SyntaxKind::IDENT {
                                    let text_upper = token.text().to_uppercase();
                                    if text_upper == "TOP" || text_upper == "ПЕРВЫЕ" {
                                        self.record_token(
                                            token,
                                            crate::source_map::TokenCategory::Modifier,
                                        );
                                        top_range = Some(top_child.text_range());
                                    }
                                }
                            }
                        }

                        for element in top_child.descendants_with_tokens() {
                            if let Some(token) = element.as_token() {
                                if token.kind() == syntax::SyntaxKind::DECIMAL {
                                    if let Ok(count) = token.text().parse::<u32>() {
                                        top = Some(count);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }

                break;
            }
        }

        (distinct, top, top_range)
    }

    pub(super) fn lower_selected_field(
        &mut self,
        field: &syntax::ast::SdblSelectedField,
    ) -> FieldHir {
        let field_range = field.syntax().text_range();

        if field.is_asterisk() {
            let asterisk_node =
                field.syntax().children().find_map(syntax::ast::SdblAsteriskField::cast);
            let head_token = asterisk_node.as_ref().and_then(|node| {
                node.syntax()
                    .children_with_tokens()
                    .filter_map(|el| el.into_token())
                    .find(|t| t.kind() == syntax::SyntaxKind::IDENT)
            });
            if let Some(head) = head_token {
                if self.scope.is_inaccessible_alias(head.text()) {
                    self.diagnostics.push(crate::diagnostics::SdblDiagnostic::UnavailableAlias {
                        alias: head.text().to_string(),
                        range: head.text_range(),
                    });
                }
            }
            let asterisk_qualifier = asterisk_node.and_then(|node| {
                let parts = node.qualifier_parts();
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join("."))
                }
            });
            return FieldHir {
                expr: ExprHir::Missing { range: field_range },
                alias: None,
                has_as_keyword: false,
                has_parse_error: false,
                raw_name: None,
                ty: SdblType::Unknown,
                is_asterisk: true,
                asterisk_qualifier,
                diagnostic_range: field_range,
                range: field_range,
            };
        }

        let expr = if let Some(e) = field.expression() {
            tracing::trace!(
                expr_text = %e.text(),
                expr_kind = ?e.kind(),
                "DIAGNOSTIC LOWERING: lowering SELECT field expression"
            );
            let lowered = self.lower_expr(&e);
            tracing::trace!(
                expr_text = %e.text(),
                lowered_variant = ?std::mem::discriminant(&lowered),
                lowered_type = ?lowered.ty(),
                "DIAGNOSTIC LOWERING: lowered SELECT field expression"
            );
            lowered
        } else {
            ExprHir::Missing { range: field_range }
        };

        let alias_node = field.alias();

        let has_as_keyword = alias_node.as_ref().map(|a| a.has_as_keyword()).unwrap_or(false);

        let alias = alias_node
            .as_ref()
            .and_then(|a| {
                if a.has_as_keyword() {
                    self.record_keyword_by_text(
                        a.syntax(),
                        "AS",
                        "КАК",
                        crate::source_map::TokenCategory::SpecialKeyword,
                    );
                }

                if let Some(ident_token) = a.identifier() {
                    self.source_map.add_token(
                        crate::source_map::TokenInfo::new(
                            ident_token.text_range(),
                            ident_token.kind(),
                            ident_token.text(),
                        ),
                        crate::source_map::TokenCategory::FieldAlias,
                    );
                }

                a.name()
            })
            .map(|s| Name::from(s.as_str()));

        let ty = expr.ty().clone();

        let raw_name = field.expression().and_then(|expr_node| {
            let mut last_ident = None;
            for element in expr_node.descendants_with_tokens() {
                if let Some(token) = element.as_token() {
                    if token.kind().is_name_token() {
                        last_ident = Some(token.text().to_string());
                    }
                }
            }
            last_ident.map(|s| Name::from(s.as_str()))
        });

        let has_parse_error = field
            .syntax()
            .descendants_with_tokens()
            .any(|el| el.kind() == syntax::SyntaxKind::ERROR);

        let diagnostic_range = self.compute_diagnostic_range(field, alias_node.as_ref());

        FieldHir {
            expr,
            alias,
            has_as_keyword,
            has_parse_error,
            raw_name,
            ty,
            is_asterisk: false,
            asterisk_qualifier: None,
            diagnostic_range,
            range: field_range,
        }
    }

    fn compute_diagnostic_range(
        &self,
        field: &syntax::ast::SdblSelectedField,
        alias_node: Option<&syntax::ast::SdblAlias>,
    ) -> syntax::TextRange {
        // Обратного шага через хвостовую тривию здесь нет: узел ею не
        // кончается, это норма привязки.
        let expr_range = if let Some(expr) = field.expression() {
            expr.text_range()
        } else {
            field.syntax().text_range()
        };

        if let Some(alias) = alias_node {
            if let Some(alias_ident) = alias.identifier() {
                return syntax::TextRange::new(expr_range.start(), alias_ident.text_range().end());
            }
        }

        expr_range
    }
}
