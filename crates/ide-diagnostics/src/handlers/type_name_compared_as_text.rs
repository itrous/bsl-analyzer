use hir::{BinaryOp, Expr, ExprId, ExprIdx, IdConversion, Literal, LocalRange, ValueOrigin};

use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Badpractice],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const MESSAGE: &str = "Сравнение с текстом имени типа: текст зависит от языка интерфейса, сравнивать типы следует через Тип()";

/// Reports a comparison (`=` / `<>`) of a string literal written in place with a value that is
/// the text of a type name on EVERY path reaching the comparison. The text depends on the
/// interface language, so the comparison silently stops matching when it changes.
///
/// The rule identifies nothing by the spelling of a function or a type: whether the operand is
/// such a text is the answer of the value-origin walk (`BodyContext::value_origin`), which
/// follows locals, branches and the functions the platform data marks as keeping the text of
/// their argument. This file carries no name list, and a test of it keeps it so.
///
/// The finding is anchored on the operand when it is a call, otherwise on the comparison.
pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::TypeNameComparedAsText;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }
    let body = ctx.body();
    for (expr_id, expr) in body.exprs_iter() {
        let Expr::BinaryOp { lhs, rhs, op: BinaryOp::Eq | BinaryOp::Neq } = expr else { continue };
        let operand = if is_string_literal(body, *rhs) {
            *lhs
        } else if is_string_literal(body, *lhs) {
            *rhs
        } else {
            continue;
        };
        let operand = ExprId::from_idx(operand);
        if !is_reportable(ctx.value_origin(operand)) {
            continue;
        }
        let anchor =
            if matches!(body.expr(operand), Expr::Call { .. }) { operand } else { expr_id };
        let Some(range) = ctx.source_map().expr_range(anchor) else { continue };
        acc.push(Diagnostic {
            code,
            message: MESSAGE.to_string(),
            severity: ctx.severity(code),
            range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

/// Only a proven type-name text is reported; no `Unproven` reason is. The weakest reason,
/// `RewriteDirectionUnknown`, means nothing is proven about the value (the walk returns it before
/// it looks at the origin), so reporting it would flood the rule: 11582 findings against 10 over
/// the same five configurations.
fn is_reportable(origin: ValueOrigin) -> bool {
    matches!(origin, ValueOrigin::TypeNameText)
}

fn is_string_literal(body: &hir::Body, idx: ExprIdx) -> bool {
    matches!(body.expr(ExprId::from_idx(idx)), Expr::Literal(Literal::String(_)))
}
