use crate::{handlers, BodyContext, Diagnostic};
use hir::LocalRange;
use syntax::{SyntaxNode, SyntaxToken};

/// One walk over the body's nodes and tokens for every single-pass check.
/// For module-level code the walk skips method subtrees, which their own
/// bodies cover, so each node of the file is visited exactly once.
pub fn collect_body_single_pass(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    if !ctx.config.any_enabled(crate::runner::SINGLE_PASS_DIAGNOSTICS) {
        return;
    }

    let _span = tracing::debug_span!("collect_body_single_pass").entered();

    for node in ctx.nodes() {
        check_node_handlers(&node, acc, ctx);

        for element in node.children_with_tokens() {
            if let Some(token) = element.into_token() {
                check_token_handlers(&token, acc, ctx);
            }
        }
    }
}

#[inline]
fn check_node_handlers(
    node: &SyntaxNode,
    acc: &mut Vec<Diagnostic<LocalRange>>,
    ctx: &BodyContext,
) {
    handlers::useless_ternary_operator::check_node(node, acc, ctx);
    handlers::double_negatives::check_node(node, acc, ctx);
    handlers::unknown_preprocessor_symbol::check_node(node, acc, ctx);
    handlers::bad_words::check_node(node, acc, ctx);
    handlers::typo::check_node(node, acc, ctx);
    handlers::nested_ternary_operator::check_node(node, acc, ctx);
    handlers::postfix_access_on_expression::check_node(node, acc, ctx);
}

#[inline]
fn check_token_handlers(
    token: &SyntaxToken,
    acc: &mut Vec<Diagnostic<LocalRange>>,
    ctx: &BodyContext,
) {
    handlers::yo_letter_usage::check_token(token, acc, ctx);
    handlers::magic_date::check_token(token, acc, ctx);
    handlers::typo::check_token(token, acc, ctx);
    handlers::using_hardcode_path::check_token(token, acc, ctx);
}

#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, DiagnosticsConfig};

    #[test]
    fn one_string_occurrence_inside_a_method_emits_one_raw_typo() {
        let source = "Процедура Тест()\n\tСообщить(\"гатово\");\nКонецПроцедуры";
        let (db, file_id) = crate::test_utils::create_test_db(source);
        let provider = ide_db::SalsaProvider::new(&db, None);
        let mut config = DiagnosticsConfig::default();
        config.enabled.push(DiagnosticCode::Typo);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);
        let diagnostics = crate::body::for_each_body_context(&ctx, |body_ctx, acc| {
            super::collect_body_single_pass(body_ctx, acc)
        });
        let typos: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::Typo)
            .collect();

        assert_eq!(typos.len(), 1);
        let start: usize = typos[0].range.start().into();
        let end: usize = typos[0].range.end().into();
        assert_eq!(source.get(start..end), Some("гатово"));
    }
}
