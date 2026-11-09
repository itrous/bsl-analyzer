use hir::{Body, Expr, ExprId, ExprIdx, IdConversion, Literal, LocalRange, Stmt, StmtIdx};
use stdx::case::fold_lower_per_char;

use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const MESSAGE: &str = "Запись в режиме загрузки данных с режимом проведения: платформа отвечает «Действие не может выполняться в режиме загрузки данных»";

/// Lowercased spellings (Russian and English) of the pieces the predicate matches.
const EXCHANGE_PROPERTY: &[&str] = &["обменданными", "dataexchange"];
const LOAD_PROPERTY: &[&str] = &["загрузка", "load"];
const WRITE_METHOD: &[&str] = &["записать", "write"];
const WRITE_MODE_ENUM: &[&str] = &["режимзаписидокумента", "documentwritemode"];
const POSTING_MODES: &[&str] = &["проведение", "отменапроведения", "posting", "undoposting"];

/// Reports `<Имя>.Записать(РежимЗаписиДокумента.Проведение | ОтменаПроведения)` after
/// `<Имя>.ОбменДанными.Загрузка = Истина` on the same local variable: the platform
/// always refuses it at runtime with «Действие не может выполняться в режиме загрузки
/// данных» (measured on 8.3.27.2214; the legal pair is the load flag with
/// `РежимЗаписиДокумента.Запись`).
///
/// Deliberately conservative: the flag counts as set only when it was assigned the
/// literal `Истина` in the same statement list as the write, or in an enclosing one,
/// before it. A flag set inside a nested block does not leak out of it, and any other
/// assignment to the flag or to the variable (in this list or a nested block) drops it.
pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::PostingWhileDataExchangeLoading;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }
    let body = ctx.body();
    if body.is_lowering_incomplete() {
        return;
    }
    let mut flagged: Vec<String> = Vec::new();
    let mut writes: Vec<ExprIdx> = Vec::new();
    walk(body, body.body_stmts_typed(), &mut flagged, &mut writes);

    for call in writes {
        let Some(range) = ctx.source_map().expr_range(ExprId::from_idx(call)) else {
            continue;
        };
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

fn walk(body: &Body, stmts: &[StmtIdx], flagged: &mut Vec<String>, writes: &mut Vec<ExprIdx>) {
    for &idx in stmts {
        match body.stmt_idx(idx) {
            Stmt::Assign { target, value } => apply_assignment(body, *target, *value, flagged),
            Stmt::Expr(expr) => {
                if is_posting_write_of_flagged(body, *expr, flagged) {
                    writes.push(*expr);
                }
            }
            Stmt::If(if_stmt) => {
                nested(body, &if_stmt.then_branch, flagged, writes);
                for (_, branch) in if_stmt.elsif_branches.iter() {
                    nested(body, branch, flagged, writes);
                }
                if let Some(branch) = &if_stmt.else_branch {
                    nested(body, branch, flagged, writes);
                }
            }
            Stmt::PreprocIf(preproc) => {
                for branch in preproc.branches() {
                    nested(body, branch.stmts, flagged, writes);
                }
            }
            Stmt::While { body: inner, .. }
            | Stmt::For { body: inner, .. }
            | Stmt::ForEach { body: inner, .. } => nested(body, inner, flagged, writes),
            Stmt::Try { body: inner, except } => {
                nested(body, inner, flagged, writes);
                nested(body, except, flagged, writes);
            }
            _ => {}
        }
    }
}

/// Walks a nested block with a copy of the state: whatever the block cleared is
/// cleared outside too, whatever it set stays inside.
fn nested(body: &Body, stmts: &[StmtIdx], flagged: &mut Vec<String>, writes: &mut Vec<ExprIdx>) {
    let mut inner = flagged.clone();
    walk(body, stmts, &mut inner, writes);
    flagged.retain(|name| inner.contains(name));
}

fn apply_assignment(body: &Body, target: ExprIdx, value: ExprIdx, flagged: &mut Vec<String>) {
    if let Some(name) = load_flag_owner(body, target) {
        let is_true = matches!(body.expr_idx(value), Expr::Literal(Literal::Bool(true)));
        flagged.retain(|n| *n != name);
        if is_true {
            flagged.push(name);
        }
    } else if let Expr::Path(name) = body.expr_idx(target) {
        let name = fold_lower_per_char(name.as_str());
        flagged.retain(|n| *n != name);
    }
}

/// `<Имя>.ОбменДанными.Загрузка` -> the folded `<Имя>` of a bare local variable.
fn load_flag_owner(body: &Body, target: ExprIdx) -> Option<String> {
    let Expr::Field { base, field } = body.expr_idx(target) else { return None };
    if !is_one_of(field.as_str(), LOAD_PROPERTY) {
        return None;
    }
    let Expr::Field { base: owner, field: exchange } = body.expr_idx(*base) else { return None };
    if !is_one_of(exchange.as_str(), EXCHANGE_PROPERTY) {
        return None;
    }
    bare_name(body, *owner)
}

fn is_posting_write_of_flagged(body: &Body, expr: ExprIdx, flagged: &[String]) -> bool {
    let Expr::Call { callee, args } = body.expr_idx(expr) else { return false };
    let Expr::Field { base: receiver, field: method } = body.expr_idx(*callee) else {
        return false;
    };
    if !is_one_of(method.as_str(), WRITE_METHOD) {
        return false;
    }
    let Some(name) = bare_name(body, *receiver) else { return false };
    if !flagged.contains(&name) {
        return false;
    }
    let Some(&mode) = args.first() else { return false };
    let Expr::Field { base, field } = body.expr_idx(mode) else { return false };
    let Expr::Path(enum_name) = body.expr_idx(*base) else { return false };
    is_one_of(enum_name.as_str(), WRITE_MODE_ENUM) && is_one_of(field.as_str(), POSTING_MODES)
}

fn bare_name(body: &Body, expr: ExprIdx) -> Option<String> {
    match body.expr_idx(expr) {
        Expr::Path(name) => Some(fold_lower_per_char(name.as_str())),
        _ => None,
    }
}

fn is_one_of(name: &str, folded_options: &[&str]) -> bool {
    folded_options.contains(&fold_lower_per_char(name).as_str())
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_for, range_to_line_col};
    use crate::DiagnosticCode;

    fn run(body: &str) -> Vec<crate::Diagnostic> {
        let code = format!("Процедура Проба()\n{body}\nКонецПроцедуры\n");
        check_diagnostics_for(&code, DiagnosticCode::PostingWhileDataExchangeLoading)
    }

    #[test]
    fn both_posting_modes_after_the_load_flag_are_flagged_on_the_write_call() {
        let body = "    Д.ОбменДанными.Загрузка = Истина;\n    Д.Записать(РежимЗаписиДокумента.Проведение);\n    Д.Записать(РежимЗаписиДокумента.ОтменаПроведения);";
        let d = run(body);
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(d[0].message.contains("Действие не может выполняться в режиме загрузки данных"));
        let text = format!("Процедура Проба()\n{body}\nКонецПроцедуры\n");
        for (finding, line) in d.iter().zip([2u32, 3]) {
            let (start_line, _, _, _) = range_to_line_col(&text, finding.range);
            assert_eq!(start_line, line, "{finding:?}");
        }
    }

    #[test]
    fn english_spelling_of_the_flag_and_the_modes_is_flagged() {
        let d = run("    D.DataExchange.Load = True;\n    D.Write(DocumentWriteMode.Posting);\n    D.Write(DocumentWriteMode.UndoPosting);");
        assert_eq!(d.len(), 2, "{d:?}");
    }

    #[test]
    fn russian_and_english_parity_fixtures_each_emit_one_identical_finding() {
        let ru = check_diagnostics_for(
            "Процедура Тест(Документ)\n    Документ.ОбменДанными.Загрузка = Истина;\n    Документ.Записать(РежимЗаписиДокумента.Проведение);\nКонецПроцедуры",
            DiagnosticCode::PostingWhileDataExchangeLoading,
        );
        let en = check_diagnostics_for(
            "Procedure Test(Document)\n    Document.DataExchange.Load = True;\n    Document.Write(DocumentWriteMode.Posting);\nEndProcedure",
            DiagnosticCode::PostingWhileDataExchangeLoading,
        );
        assert_eq!(ru.len(), 1, "{ru:?}");
        assert_eq!(en.len(), 1, "{en:?}");
        assert_eq!(ru[0].message, en[0].message);
        assert_eq!(ru[0].severity, en[0].severity);
    }

    #[test]
    fn load_flag_with_the_plain_write_mode_is_not_flagged() {
        let d = run("    Д.ОбменДанными.Загрузка = Истина;\n    Д.Записать(РежимЗаписиДокумента.Запись);\n    Д.Записать();");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn posting_write_without_the_load_flag_is_not_flagged() {
        let d = run("    Д.Записать(РежимЗаписиДокумента.Проведение);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn intervening_reset_of_the_load_flag_cancels_the_finding() {
        let d = run("    Д.ОбменДанными.Загрузка = Истина;\n    Д.ОбменДанными.Загрузка = Ложь;\n    Д.Записать(РежимЗаписиДокумента.Проведение);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn load_flag_of_another_variable_does_not_make_the_write_a_finding() {
        let d = run("    Д1.ОбменДанными.Загрузка = Истина;\n    Д2.Записать(РежимЗаписиДокумента.Проведение);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn write_mode_taken_from_a_variable_is_not_flagged() {
        let d = run("    Д.ОбменДанными.Загрузка = Истина;\n    Режим = РежимЗаписиДокумента.Проведение;\n    Д.Записать(Режим);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn load_flag_taken_from_an_expression_is_not_flagged() {
        let d = run(
            "    Д.ОбменДанными.Загрузка = Флаг;\n    Д.Записать(РежимЗаписиДокумента.Проведение);",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn load_flag_set_only_inside_a_condition_is_not_flagged_after_it() {
        let d = run("    Если Условие Тогда\n        Д.ОбменДанными.Загрузка = Истина;\n    КонецЕсли;\n    Д.Записать(РежимЗаписиДокумента.Проведение);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn load_flag_set_before_a_condition_is_flagged_inside_it() {
        let d = run("    Д.ОбменДанными.Загрузка = Истина;\n    Если Условие Тогда\n        Д.Записать(РежимЗаписиДокумента.Проведение);\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn write_on_a_complex_receiver_is_not_flagged() {
        let d = run("    Д.ОбменДанными.Загрузка = Истина;\n    Д.Ссылка.ПолучитьОбъект().Записать(РежимЗаписиДокумента.Проведение);");
        assert!(d.is_empty(), "{d:?}");
    }
}
