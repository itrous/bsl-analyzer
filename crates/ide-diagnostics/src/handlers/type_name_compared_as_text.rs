use hir::{BinaryOp, Body, Expr, ExprId, ExprIdx, IdConversion, Literal, LocalRange, Stmt};
use stdx::case::fold_lower_per_char;

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

/// Lowercased spellings (Russian and English) of the functions the predicate matches.
const STRING_FUNCTION: &[&str] = &["строка", "string"];
const TYPE_OF_FUNCTION: &[&str] = &["типзнч", "typeof"];
/// Functions that may wrap the type name without changing what is being decided on.
const TRANSPARENT_WRAPPERS: &[&str] = &[
    "лев",
    "left",
    "прав",
    "right",
    "сокрлп",
    "trimall",
    "сокрл",
    "triml",
    "сокрп",
    "trimr",
    "врег",
    "upper",
    "нрег",
    "lower",
    "стрдлина",
    "strlen",
];
const FIND_FUNCTION: &[&str] = &["найти", "find"];
const NSTR_FUNCTION: &[&str] = &["нстр", "nstr"];
const AFFIX_FUNCTIONS: &[&str] =
    &["стрначинаетсяс", "strstartswith", "стрзаканчиваетсяна", "strendswith"];

/// What a comparison operand turned out to render.
enum Rendered {
    /// The `Строка(ТипЗнч(...))` call itself, written in place.
    InPlace(ExprId),
    /// A local variable every assignment of which is such a call.
    ViaVariable,
}

/// Reports a decision taken on the TEXT of a type name: `Строка(ТипЗнч(...))` /
/// `String(TypeOf(...))` compared with a string literal. The text depends on the
/// interface language, so the comparison silently stops matching when it changes.
///
/// All three parts are required: the rendered name (in place, or one local variable
/// all of whose assignments in the method are such calls), a comparison (`=` / `<>`
/// with a literal, `Найти(name, "literal")` against a number, `СтрНачинаетсяС` /
/// `СтрЗаканчиваетсяНа` with a literal), and the literal (in place, `НСтр(...)`, or a variable assigned only string literals).
/// Transparent wrappers (`Лев`,
/// `СокрЛП`, ...) may surround the name. A name that is merely printed, thrown,
/// concatenated, returned or stored is never reported.
///
/// The finding is anchored on the `Строка(...)` call when it is written in place and
/// on the comparison when the name comes through a variable.
pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::TypeNameComparedAsText;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }
    let body = ctx.body();
    let mut anchors: Vec<ExprId> = Vec::new();
    for (expr_id, expr) in body.exprs_iter() {
        let Some(rendered) = compared_type_name(body, expr) else { continue };
        let anchor = match rendered {
            Rendered::InPlace(call) => call,
            Rendered::ViaVariable => expr_id,
        };
        if !anchors.contains(&anchor) {
            anchors.push(anchor);
        }
    }
    for anchor in anchors {
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

/// The rendered type name this expression compares with a string literal, if it is a
/// comparison of that shape.
fn compared_type_name(body: &Body, expr: &Expr) -> Option<Rendered> {
    match expr {
        Expr::BinaryOp { lhs, rhs, op: BinaryOp::Eq | BinaryOp::Neq } => {
            literal_comparison(body, *lhs, *rhs).or_else(|| find_against_number(body, *lhs, *rhs))
        }
        Expr::BinaryOp {
            lhs,
            rhs,
            op: BinaryOp::Gt | BinaryOp::Ge | BinaryOp::Lt | BinaryOp::Le,
        } => find_against_number(body, *lhs, *rhs),
        Expr::Call { callee, args } => {
            if is_global_call_of(body, *callee, AFFIX_FUNCTIONS)
                && args.len() >= 2
                && is_string_literal(body, args[1])
            {
                rendered_name(body, args[0])
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `<name> = <literal>` in either operand order.
fn literal_comparison(body: &Body, lhs: ExprIdx, rhs: ExprIdx) -> Option<Rendered> {
    if is_literal_text(body, rhs) {
        rendered_name(body, lhs)
    } else if is_literal_text(body, lhs) {
        rendered_name(body, rhs)
    } else {
        None
    }
}

/// `Найти(<name>, "literal")` compared with any number literal, on either side.
fn find_against_number(body: &Body, lhs: ExprIdx, rhs: ExprIdx) -> Option<Rendered> {
    let find = if is_number_literal(body, rhs) {
        lhs
    } else if is_number_literal(body, lhs) {
        rhs
    } else {
        return None;
    };
    let Expr::Call { callee, args } = body.expr(ExprId::from_idx(find)) else { return None };
    if !is_global_call_of(body, *callee, FIND_FUNCTION)
        || args.len() < 2
        || !is_string_literal(body, args[1])
    {
        return None;
    }
    rendered_name(body, args[0])
}
/// Strips transparent wrappers; accepts the conversion itself or a variable that
/// holds one.
fn rendered_name(body: &Body, mut idx: ExprIdx) -> Option<Rendered> {
    loop {
        let id = ExprId::from_idx(idx);
        match body.expr(id) {
            expr if is_type_name_conversion(body, expr) => return Some(Rendered::InPlace(id)),
            Expr::Call { callee, args }
                if !args.is_empty() && is_global_call_of(body, *callee, TRANSPARENT_WRAPPERS) =>
            {
                idx = args[0];
            }
            Expr::Path(name) => {
                return holds_only_type_names(body, &fold_lower_per_char(name.as_str()))
                    .then_some(Rendered::ViaVariable);
            }
            _ => return None,
        }
    }
}

/// True when the variable is assigned at least once in the method and every
/// assignment to it is a type-name conversion.
fn holds_only_type_names(body: &Body, folded_name: &str) -> bool {
    holds_only(body, folded_name, |value| is_type_name_conversion(body, value))
}

/// True when the variable is assigned at least once in the method and every
/// assignment to it satisfies `accepts`.
fn holds_only(body: &Body, folded_name: &str, accepts: impl Fn(&Expr) -> bool) -> bool {
    let mut assigned = false;
    for (_, stmt) in body.stmts_iter() {
        let Stmt::Assign { target, value } = stmt else { continue };
        let Expr::Path(name) = body.expr(ExprId::from_idx(*target)) else { continue };
        if fold_lower_per_char(name.as_str()) != folded_name {
            continue;
        }
        if !accepts(body.expr(ExprId::from_idx(*value))) {
            return false;
        }
        assigned = true;
    }
    assigned
}

/// The other side of a comparison is text fixed in the source: a string literal, a
/// localized `НСтр(...)` literal, or a local variable assigned only string literals.
fn is_literal_text(body: &Body, idx: ExprIdx) -> bool {
    match body.expr(ExprId::from_idx(idx)) {
        Expr::Literal(Literal::String(_)) => true,
        Expr::Call { callee, .. } => is_global_call_of(body, *callee, NSTR_FUNCTION),
        Expr::Path(name) => holds_only(body, &fold_lower_per_char(name.as_str()), |value| {
            matches!(value, Expr::Literal(Literal::String(_)))
        }),
        _ => false,
    }
}
fn is_type_name_conversion(body: &Body, expr: &Expr) -> bool {
    let Expr::Call { callee, args } = expr else { return false };
    is_global_call_of(body, *callee, STRING_FUNCTION)
        && args.len() == 1
        && is_call_of(body, args[0], TYPE_OF_FUNCTION)
}

fn is_string_literal(body: &Body, idx: ExprIdx) -> bool {
    matches!(body.expr(ExprId::from_idx(idx)), Expr::Literal(Literal::String(_)))
}

fn is_number_literal(body: &Body, idx: ExprIdx) -> bool {
    matches!(body.expr(ExprId::from_idx(idx)), Expr::Literal(Literal::Number(_)))
}

fn is_call_of(body: &Body, expr: ExprIdx, folded_names: &[&str]) -> bool {
    let Expr::Call { callee, .. } = body.expr(ExprId::from_idx(expr)) else { return false };
    is_global_call_of(body, *callee, folded_names)
}

fn is_global_call_of(body: &Body, callee: ExprIdx, folded_names: &[&str]) -> bool {
    let Expr::Path(name) = body.expr(ExprId::from_idx(callee)) else { return false };
    folded_names.contains(&fold_lower_per_char(name.as_str()).as_str())
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_for, range_to_line_col};
    use crate::DiagnosticCode;

    fn run(body: &str) -> Vec<crate::Diagnostic> {
        let code = format!("Процедура Проба(Значение)\n{body}\nКонецПроцедуры\n");
        check_diagnostics_for(&code, DiagnosticCode::TypeNameComparedAsText)
    }

    #[test]
    fn string_of_type_of_compared_with_a_literal_is_flagged_on_the_string_call() {
        let body = "    Если Строка(ТипЗнч(Элемент)) = \"Табличное поле\" Тогда\n    КонецЕсли;";
        let d = run(body);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(
            d[0].message,
            "Сравнение с текстом имени типа: текст зависит от языка интерфейса, сравнивать типы следует через Тип()"
        );
        let text = format!("Процедура Проба(Значение)\n{body}\nКонецПроцедуры\n");
        let (start_line, start_col, _, end_col) = range_to_line_col(&text, d[0].range);
        assert_eq!((start_line, start_col, end_col), (1, 9, 32), "{:?}", d[0]);
    }

    #[test]
    fn literal_on_the_left_and_the_not_equal_operator_are_flagged() {
        let d = run("    Если \"Табличное поле\" <> Строка(ТипЗнч(Элемент)) Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn wrapped_type_name_compared_with_a_literal_is_flagged() {
        let d = run("    Если Лев(Строка(ТипЗнч(Объект)), 8) = \"Документ\" Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
        let d = run(
            "    Если ВРег(СокрЛП(Строка(ТипЗнч(Объект)))) = \"ДОКУМЕНТ\" Тогда\n    КонецЕсли;",
        );
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn type_name_through_one_variable_is_flagged_on_the_comparison() {
        let body = "    Имя = Строка(ТипЗнч(Объект));\n    Если Лев(Имя, 24) = \"Внешняя обработка объект\" Тогда\n    КонецЕсли;";
        let d = run(body);
        assert_eq!(d.len(), 1, "{d:?}");
        let text = format!("Процедура Проба(Значение)\n{body}\nКонецПроцедуры\n");
        let (start_line, start_col, _, _) = range_to_line_col(&text, d[0].range);
        assert_eq!((start_line, start_col), (2, 9), "{:?}", d[0]);
    }

    #[test]
    fn find_in_the_type_name_against_zero_is_flagged() {
        let d = run("    Если Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Внешняя обработка\") > 0 Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
        let d =
            run("    Если Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Отчет\") = 0 Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
    }
    #[test]
    fn starts_with_and_ends_with_a_literal_are_flagged() {
        let d = run(
            "    Если СтрНачинаетсяС(Строка(ТипЗнч(Объект)), \"Документ\") Тогда\n    КонецЕсли;",
        );
        assert_eq!(d.len(), 1, "{d:?}");
        let d = run(
            "    Если СтрЗаканчиваетсяНа(Строка(ТипЗнч(Объект)), \"Ссылка\") Тогда\n    КонецЕсли;",
        );
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn english_spellings_are_flagged_the_same_way() {
        let d = run("    If Left(String(TypeOf(Object)), 8) = \"Document\" Then\n    EndIf;");
        assert_eq!(d.len(), 1, "{d:?}");
        let d = run("    If Find(String(TypeOf(Object)), \"Report\") > 0 Then\n    EndIf;");
        assert_eq!(d.len(), 1, "{d:?}");
        let d = run("    If StrStartsWith(String(TypeOf(Object)), \"Doc\") Then\n    EndIf;");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn exception_text_with_the_type_name_is_not_flagged() {
        let d = run("    ВызватьИсключение \"тип: \" + Строка(ТипЗнч(Значение));");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn type_name_that_is_only_concatenated_or_stored_is_not_flagged() {
        let d = run("    Ключ = Строка(ТипЗнч(Значение)) + \":\";");
        assert!(d.is_empty(), "{d:?}");
        let d = run("    Имя = Строка(ТипЗнч(Значение));\n    Сообщить(Имя);\n    Возврат Имя;");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn comparison_with_another_type_name_or_a_parameter_is_not_flagged() {
        let d = run("    Если Строка(ТипЗнч(А)) = Строка(ТипЗнч(Б)) Тогда\n    КонецЕсли;");
        assert!(d.is_empty(), "{d:?}");
        let d = run("    Если Строка(ТипЗнч(А)) = Значение Тогда\n    КонецЕсли;");
        assert!(d.is_empty(), "{d:?}");
        let d = run("    Если Строка(ТипЗнч(А)) = Фраза Тогда\n    КонецЕсли;");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn find_against_any_number_literal_is_flagged() {
        let d = run("    Если Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Внешняя обработка объект:\") = 1 Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
        let d =
            run("    Если 0 < Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Отчет\") Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
        let d =
            run("    Если Найти(Строка(ТипЗнч(ЭтотОбъект)), \"Отчет\") >= 1 Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn type_name_compared_with_a_variable_assigned_only_a_literal_is_flagged() {
        let body = "    Фраза = \"Внешняя обработка объект\";\n    Имя = Строка(ТипЗнч(Объект));\n    Если Лев(Имя, СтрДлина(Фраза)) = Фраза Тогда\n    КонецЕсли;";
        let d = run(body);
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn type_name_compared_with_a_localized_literal_is_flagged() {
        let d =
            run("    Если Строка(ТипЗнч(Объект)) = НСтр(\"ru='Документ'\") Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn type_name_compared_with_a_parameter_is_not_flagged() {
        let d = run("    Если Строка(ТипЗнч(Объект)) = Значение Тогда\n    КонецЕсли;");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn variable_assigned_a_literal_and_something_else_is_not_flagged_as_the_other_side() {
        let d = run("    Фраза = \"Документ\";\n    Фраза = Значение;\n    Если Строка(ТипЗнч(Объект)) = Фраза Тогда\n    КонецЕсли;");
        assert!(d.is_empty(), "{d:?}");
    }
    #[test]
    fn variable_assigned_a_type_name_and_something_else_is_not_flagged() {
        let d = run("    Имя = Строка(ТипЗнч(Объект));\n    Имя = \"Документ\";\n    Если Имя = \"Документ\" Тогда\n    КонецЕсли;");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn plain_string_comparison_without_a_type_name_is_not_flagged() {
        let d = run("    Если Строка(Значение) = \"Табличное поле\" Тогда\n    КонецЕсли;");
        assert!(d.is_empty(), "{d:?}");
        let d = run("    Если ТипЗнч(Значение) = Тип(\"Строка\") Тогда\n    КонецЕсли;");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn each_comparison_is_reported_once() {
        let d = run("    Если Строка(ТипЗнч(А)) = \"X\" Или Строка(ТипЗнч(Б)) = \"Y\" Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 2, "{d:?}");
        assert_ne!(d[0].range, d[1].range);
    }
}
