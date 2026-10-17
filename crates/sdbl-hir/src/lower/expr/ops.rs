use crate::hir::ExprHir;
use crate::lower::context::LoweringContext;
use crate::types::SdblType;

impl LoweringContext<'_> {
    pub(super) fn lower_binary_expr(&mut self, node: &syntax::SyntaxNode) -> ExprHir {
        use crate::hir::BinaryOp;
        use syntax::SyntaxKind;

        tracing::debug!(
            node_text = %node.text(),
            node_kind = ?node.kind(),
            "DIAGNOSTIC LOWERING: lower_binary_expr called"
        );

        for element in node.children_with_tokens() {
            if let Some(token) = element.as_token() {
                match token.kind() {
                    SyntaxKind::KW_AND | SyntaxKind::KW_OR | SyntaxKind::KW_NOT => {
                        self.record_token(token, crate::source_map::TokenCategory::Operator);
                    }
                    SyntaxKind::EQ
                    | SyntaxKind::NEQ
                    | SyntaxKind::LT
                    | SyntaxKind::LE
                    | SyntaxKind::GT
                    | SyntaxKind::GE
                    | SyntaxKind::PLUS
                    | SyntaxKind::MINUS
                    | SyntaxKind::STAR
                    | SyntaxKind::SLASH => {
                        self.record_token(token, crate::source_map::TokenCategory::Operator);
                    }
                    _ => {}
                }
            }
        }

        let children: Vec<_> = node.children().collect();

        if children.len() == 1 {
            tracing::debug!(
                node_text = %node.text(),
                "DIAGNOSTIC LOWERING: Binary expr with no operator - unwrapping child"
            );
            return self.lower_expr(&children[0]);
        }

        if children.is_empty() {
            return ExprHir::Missing { range: node.text_range() };
        }

        // Операция берётся из токена перед операндом, а не из подстроки в
        // тексте узла: текст узла содержит и литералы, и комментарии, а в
        // цепочке `а + б - в` подстрока не различает, какая операция чья.
        let default_op = match node.kind() {
            SyntaxKind::SDBL_LOGICAL_AND_EXPR => BinaryOp::And,
            SyntaxKind::SDBL_LOGICAL_OR_EXPR => BinaryOp::Or,
            _ => BinaryOp::Eq,
        };

        let mut result: Option<ExprHir> = None;
        let mut pending_op: Option<BinaryOp> = None;

        for element in node.children_with_tokens() {
            match element {
                syntax::NodeOrToken::Token(token) => {
                    if let Some(op) = binary_op_of(token.kind()) {
                        pending_op = Some(op);
                    }
                }
                syntax::NodeOrToken::Node(child) => {
                    let operand = self.lower_expr(&child);
                    result = Some(match result {
                        None => operand,
                        Some(lhs) => {
                            let op = pending_op.take().unwrap_or(default_op);
                            ExprHir::BinaryOp {
                                lhs: Box::new(lhs),
                                op,
                                rhs: Box::new(operand),
                                ty: result_ty(op),
                                range: node.text_range(),
                            }
                        }
                    });
                }
            }
        }

        result.unwrap_or_else(|| ExprHir::Missing { range: node.text_range() })
    }

    pub(super) fn lower_unary_expr(&mut self, node: &syntax::SyntaxNode) -> ExprHir {
        use crate::hir::UnaryOp;

        let expr = node
            .children()
            .next()
            .map(|n| self.lower_expr(&n))
            .unwrap_or_else(|| ExprHir::Missing { range: node.text_range() });

        let not_token = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|token| token.kind() == syntax::SyntaxKind::KW_NOT);

        let (op, ty) = if let Some(token) = not_token {
            self.record_token(&token, crate::source_map::TokenCategory::Operator);
            (UnaryOp::Not, SdblType::Boolean)
        } else if node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|token| token.kind() == syntax::SyntaxKind::MINUS)
        {
            if let Some(token) = node
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|token| token.kind() == syntax::SyntaxKind::MINUS)
            {
                self.record_token(&token, crate::source_map::TokenCategory::Operator);
            }
            (UnaryOp::Neg, SdblType::number())
        } else {
            if let Some(token) = node
                .children_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|token| token.kind() == syntax::SyntaxKind::PLUS)
            {
                self.record_token(&token, crate::source_map::TokenCategory::Operator);
            }
            (UnaryOp::Pos, expr.ty().clone())
        };

        ExprHir::UnaryOp { op, expr: Box::new(expr), ty, range: node.text_range() }
    }
}

fn binary_op_of(kind: syntax::SyntaxKind) -> Option<crate::hir::BinaryOp> {
    use crate::hir::BinaryOp;
    use syntax::SyntaxKind;

    Some(match kind {
        SyntaxKind::KW_AND => BinaryOp::And,
        SyntaxKind::KW_OR => BinaryOp::Or,
        SyntaxKind::EQ => BinaryOp::Eq,
        SyntaxKind::NEQ => BinaryOp::Ne,
        SyntaxKind::LT => BinaryOp::Lt,
        SyntaxKind::LE => BinaryOp::Le,
        SyntaxKind::GT => BinaryOp::Gt,
        SyntaxKind::GE => BinaryOp::Ge,
        SyntaxKind::PLUS => BinaryOp::Add,
        SyntaxKind::MINUS => BinaryOp::Sub,
        SyntaxKind::STAR => BinaryOp::Mul,
        SyntaxKind::SLASH => BinaryOp::Div,
        SyntaxKind::PERCENT => BinaryOp::Mod,
        _ => return None,
    })
}

fn result_ty(op: crate::hir::BinaryOp) -> SdblType {
    if op.is_comparison() || op.is_logical() {
        SdblType::Boolean
    } else if op.is_arithmetic() {
        SdblType::number()
    } else {
        SdblType::Unknown
    }
}
