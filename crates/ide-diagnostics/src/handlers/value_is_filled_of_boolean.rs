use hir::{
    BinaryOp, Expr, ExprId, ExprIdx, IdConversion, Literal, LocalOffset, LocalRange, Stmt, StmtIdx,
    UnaryOp,
};
use stdx::case::eq_ignore_case;

use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 2,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious, MetadataTag::Badpractice],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const MESSAGE: &str = "ЗначениеЗаполнено от Булево всегда возвращает Истина (замер 8.3.17 и 8.3.27): условие не проверяет ничего, нужен сам флаг";

/// Reports `ЗначениеЗаполнено(X)` / `ValueIsFilled(X)` whose single argument is `Булево` by
/// construction. The platform answers `Истина` for a Boolean of either value (measured on
/// 8.3.17.1549 and 8.3.27.2214), so the check verifies nothing.
///
/// The inferred type alone is not enough and is only the last gate. Inference drops
/// `Неопределено` and unknown arms from a union, so a variable that is `Неопределено` on one
/// path, or receives an unknown value on another, or comes back from a function that also
/// returns something unknown, is inferred as plain `Булево` while the check on it is
/// meaningful. A query field is `NULL` for an empty selection or an outer join. Therefore the
/// argument must be Boolean structurally:
///
/// * a Boolean literal, a comparison, `И` / `ИЛИ`, `Не`, or a conditional of such values;
/// * a Boolean attribute read through a reference to a metadata object (`Ссылка.Флаг`): the
///   platform has no empty value for it, unlike a query selection or a value-table row whose
///   Boolean column can be `NULL` (`ЗначениеЗаполнено(NULL)` is false);
/// * a local variable all of whose assignments in this body are such values, whose FIRST
///   assignment stands at the top level of the body before the call in a body without
///   `Перейти` (no path leaves it unset), and which is never handed to another call (it
///   could be an out-parameter, like `Структура.Свойство("Ключ", Переменная)`).
///
/// Parameters, module variables, other fields, indexes and call results are never reported.
pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::ValueIsFilledOfBoolean;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }
    let body = ctx.body();
    if body.is_lowering_incomplete() {
        return;
    }
    for (call, expr) in body.exprs_iter() {
        let Expr::Call { callee, args } = expr else { continue };
        let [argument] = &args[..] else { continue };
        let Expr::Path(name) = body.expr_idx(*callee) else { continue };
        if !is_value_is_filled(name.as_str()) {
            continue;
        }
        let Some(range) = ctx.source_map().expr_range(call) else { continue };
        if !boolean_by_construction(ctx, *argument, range.start(), 0) {
            continue;
        }
        if !inferred_boolean(ctx, *argument) {
            continue;
        }
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

const MAX_DEPTH: u32 = 4;

fn boolean_by_construction(
    ctx: &BodyContext,
    expr: ExprIdx,
    before: LocalOffset,
    depth: u32,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let body = ctx.body();
    match body.expr_idx(expr) {
        Expr::Literal(Literal::Bool(_)) => true,
        Expr::BinaryOp { op, .. } => matches!(
            op,
            BinaryOp::Eq
                | BinaryOp::Neq
                | BinaryOp::Lt
                | BinaryOp::Le
                | BinaryOp::Gt
                | BinaryOp::Ge
                | BinaryOp::And
                | BinaryOp::Or
        ),
        Expr::UnaryOp { op: UnaryOp::Not, .. } => true,
        Expr::Ternary { then_expr, else_expr, .. } => {
            boolean_by_construction(ctx, *then_expr, before, depth + 1)
                && boolean_by_construction(ctx, *else_expr, before, depth + 1)
        }
        Expr::Path(name) => local_flag_is_boolean(ctx, name.as_str(), before, depth),
        // A Boolean attribute read through a reference has no empty value: the platform
        // answers `Ложь`, never `NULL` or `Неопределено`.
        Expr::Field { base, .. } => {
            inferred_boolean(ctx, expr) && is_metadata_reference(ctx, *base, depth + 1)
        }
        _ => false,
    }
}

fn inferred_boolean(ctx: &BodyContext, expr: ExprIdx) -> bool {
    ctx.infer()
        .type_id_of_expr(ExprId::from_idx(expr))
        .is_some_and(|ty| ctx.kernel_type_is_boolean(ty))
}

fn inferred_metadata_reference(ctx: &BodyContext, expr: ExprIdx) -> bool {
    ctx.infer()
        .type_id_of_expr(ExprId::from_idx(expr))
        .is_some_and(|ty| ctx.kernel_type_is_metadata_reference(ty))
}

/// The expression is typed as a reference to a metadata object, and that typing can be
/// vouched for: a name that this body never writes (a parameter, an implicit name) or writes
/// only with references, or a field read of such a reference. A query selection, a value
/// table row, a structure, form data and a value of unknown type are not references, and a
/// field of one of them is not vouched for even when its type is a reference.
fn is_metadata_reference(ctx: &BodyContext, expr: ExprIdx, depth: u32) -> bool {
    if depth > MAX_DEPTH || !inferred_metadata_reference(ctx, expr) {
        return false;
    }
    let body = ctx.body();
    match body.expr_idx(expr) {
        Expr::Field { base, .. } => is_metadata_reference(ctx, *base, depth + 1),
        Expr::Path(name) => body.stmts_iter().all(|(_, stmt)| match stmt {
            Stmt::Assign { target, value } => {
                !matches!(body.expr_idx(*target), Expr::Path(n) if eq_ignore_case(n.as_str(), name.as_str()))
                    || inferred_metadata_reference(ctx, *value)
            }
            _ => true,
        }),
        _ => false,
    }
}

fn local_flag_is_boolean(ctx: &BodyContext, name: &str, before: LocalOffset, depth: u32) -> bool {
    let body = ctx.body();
    let is_name = |idx: ExprIdx| matches!(body.expr_idx(idx), Expr::Path(n) if eq_ignore_case(n.as_str(), name));

    let mut assignments: Vec<(u32, StmtIdx, ExprIdx)> = Vec::new();
    for (stmt_id, stmt) in body.stmts_iter() {
        if let Stmt::Assign { target, value } = stmt {
            if is_name(*target) {
                assignments.push((stmt_id.into_raw().into_u32(), stmt_id.to_idx(), *value));
            }
        }
    }
    assignments.sort_by_key(|(order, ..)| *order);
    let Some(&(_, first, _)) = assignments.first() else { return false };
    // A top-level assignment before the call lies on every path to it only while
    // control flows in text order; a `Перейти` may jump past it.
    if body.stmts_iter().any(|(_, stmt)| matches!(stmt, Stmt::Goto(_))) {
        return false;
    }
    if !body.body_stmts_typed().contains(&first) {
        return false;
    }
    let Some(first_target) = assignment_target_start(ctx, first) else { return false };
    if first_target >= before {
        return false;
    }
    if !assignments
        .iter()
        .all(|(_, _, value)| boolean_by_construction(ctx, *value, before, depth + 1))
    {
        return false;
    }
    // Any call taking the variable as an argument may write it back through an
    // out-parameter; the only call allowed is the check itself.
    for (_, expr) in body.exprs_iter() {
        let args: &[ExprIdx] = match expr {
            Expr::Call { callee, args } => {
                if matches!(body.expr_idx(*callee), Expr::Path(n) if is_value_is_filled(n.as_str()))
                {
                    continue;
                }
                args
            }
            Expr::MethodCall { args, .. } => args,
            _ => continue,
        };
        if args.iter().any(|arg| is_name(*arg)) {
            return false;
        }
    }
    true
}

fn assignment_target_start(ctx: &BodyContext, stmt: StmtIdx) -> Option<LocalOffset> {
    let Stmt::Assign { target, .. } = ctx.body().stmt_idx(stmt) else { return None };
    ctx.source_map().expr_range(ExprId::from_idx(*target)).map(|range| range.start())
}

fn is_value_is_filled(name: &str) -> bool {
    eq_ignore_case(name, "значениезаполнено") || eq_ignore_case(name, "valueisfilled")
}
#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_for, range_to_line_col};
    use crate::DiagnosticCode;

    const MESSAGE: &str = "ЗначениеЗаполнено от Булево всегда возвращает Истина (замер 8.3.17 и 8.3.27): условие не проверяет ничего, нужен сам флаг";

    fn run(body: &str) -> Vec<crate::Diagnostic> {
        let code = format!("Процедура Проба()\n{body}\nКонецПроцедуры\n");
        check_diagnostics_for(&code, DiagnosticCode::ValueIsFilledOfBoolean)
    }

    #[test]
    fn value_is_filled_of_a_variable_assigned_true_is_flagged() {
        let body = "    Флаг = Истина;\n    Если ЗначениеЗаполнено(Флаг) Тогда\n    КонецЕсли;";
        let d = run(body);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].message, MESSAGE);
        let text = format!("Процедура Проба()\n{body}\nКонецПроцедуры\n");
        let (line, start, _, end) = range_to_line_col(&text, d[0].range);
        assert_eq!((line, start, end), (2, 9, 32), "{:?}", d[0]);
    }

    #[test]
    fn value_is_filled_of_a_comparison_result_is_flagged() {
        let d =
            run("    А = 1;\n    Б = 2;\n    Если ЗначениеЗаполнено(А = Б) Тогда\n    КонецЕсли;");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_string_or_a_number_is_not_flagged() {
        let d = run("    Текст = \"а\";\n    Число = 5;\n    А = ЗначениеЗаполнено(Текст);\n    Б = ЗначениеЗаполнено(Число);\n    В = ЗначениеЗаполнено(\"а\");\n    Г = ЗначениеЗаполнено(5);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_value_of_unknown_type_is_not_flagged() {
        let d = run(
            "    А = ЗначениеЗаполнено(Неизвестно);\n    Б = ЗначениеЗаполнено(Параметр.Поле);",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_composite_value_that_may_be_boolean_is_not_flagged() {
        let d = run("    Если Условие Тогда\n        Значение = Истина;\n    Иначе\n        Значение = \"текст\";\n    КонецЕсли;\n    А = ЗначениеЗаполнено(Значение);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn english_spelling_value_is_filled_is_flagged_the_same_way() {
        let code = "Procedure Test()\n    Flag = True;\n    If ValueIsFilled(Flag) Then\n    EndIf;\nEndProcedure\n";
        let d = check_diagnostics_for(code, DiagnosticCode::ValueIsFilledOfBoolean);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].message, MESSAGE);
    }

    #[test]
    fn value_is_filled_with_no_arguments_or_two_arguments_is_not_flagged() {
        let d = run("    Флаг = Истина;\n    А = ЗначениеЗаполнено();\n    Б = ЗначениеЗаполнено(Флаг, Флаг);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn russian_and_english_parity_fixtures_each_emit_one_identical_finding() {
        let ru = check_diagnostics_for(
            "Процедура Тест()\n    Результат = ЗначениеЗаполнено(Истина);\nКонецПроцедуры",
            DiagnosticCode::ValueIsFilledOfBoolean,
        );
        let en = check_diagnostics_for(
            "Procedure Test()\n    Result = ValueIsFilled(True);\nEndProcedure",
            DiagnosticCode::ValueIsFilledOfBoolean,
        );
        assert_eq!(ru.len(), 1, "{ru:?}");
        assert_eq!(en.len(), 1, "{en:?}");
        assert_eq!(ru[0].message, en[0].message);
        assert_eq!(ru[0].severity, en[0].severity);
    }

    #[test]
    fn value_is_filled_of_a_variable_assigned_a_boolean_and_an_unknown_value_is_not_flagged() {
        let d = run("    Если Условие Тогда\n        Флаг = Истина;\n    Иначе\n        Флаг = Неизвестная();\n    КонецЕсли;\n    А = ЗначениеЗаполнено(Флаг);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_function_result_that_may_be_unknown_or_false_is_not_flagged() {
        let code = "Функция Взять(К)\n    Для Каждого Э Из К Цикл\n        Возврат Э.Значение;\n    КонецЦикла;\n    Возврат Ложь;\nКонецФункции\nПроцедура Проба(К)\n    З = Взять(К);\n    А = ЗначениеЗаполнено(З);\nКонецПроцедуры\n";
        let d = check_diagnostics_for(code, DiagnosticCode::ValueIsFilledOfBoolean);
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_variable_assigned_only_on_one_branch_is_not_flagged() {
        let d = run("    Если Условие Тогда\n        Флаг = Истина;\n    КонецЕсли;\n    А = ЗначениеЗаполнено(Флаг);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_variable_that_starts_undefined_is_not_flagged() {
        let d = run("    Флаг = Неопределено;\n    Если Условие Тогда\n        Флаг = Ложь;\n    КонецЕсли;\n    А = ЗначениеЗаполнено(Флаг);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_variable_that_a_call_may_write_back_is_not_flagged() {
        let d = run("    Флаг = Ложь;\n    Настройки.Свойство(\"Флаг\", Флаг);\n    А = ЗначениеЗаполнено(Флаг);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_parameter_is_not_flagged() {
        let code = "Процедура Проба(Флаг)\n    А = ЗначениеЗаполнено(Флаг);\nКонецПроцедуры\n";
        let d = check_diagnostics_for(code, DiagnosticCode::ValueIsFilledOfBoolean);
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_variable_whose_assignment_a_goto_may_skip_is_not_flagged() {
        let d = run("    Если Условие Тогда\n        Перейти ~Метка;\n    КонецЕсли;\n    Флаг = Истина;\n~Метка:\n    А = ЗначениеЗаполнено(Флаг);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_variable_initialised_then_set_in_a_branch_is_flagged() {
        let d = run("    Флаг = Ложь;\n    Если Условие Тогда\n        Флаг = Истина;\n    КонецЕсли;\n    А = ЗначениеЗаполнено(Флаг);");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    const CONFIGURATION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>TestConfiguration</Name>
        </Properties>
        <ChildObjects>
            <Catalog>Товары</Catalog>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#;

    const CATALOG: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <Catalog uuid="7e1c0000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Товары</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="7e1c0000-0000-0000-0000-000000000002">
                <Properties>
                    <Name>Флаг</Name>
                    <Type><v8:Type>xs:boolean</v8:Type></Type>
                </Properties>
            </Attribute>
            <Attribute uuid="7e1c0000-0000-0000-0000-000000000003">
                <Properties>
                    <Name>Текст</Name>
                    <Type><v8:Type>xs:string</v8:Type></Type>
                </Properties>
            </Attribute>
            <Attribute uuid="7e1c0000-0000-0000-0000-000000000004">
                <Properties>
                    <Name>Количество</Name>
                    <Type><v8:Type>xs:decimal</v8:Type></Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;

    /// The findings in the catalog's object module, whose body is `body`.
    fn in_object_module(body: &str) -> Vec<crate::Diagnostic> {
        let module = format!("Процедура Проба()\n{body}\nКонецПроцедуры\n");
        let path = "Catalogs/Товары/Ext/ObjectModule.bsl";
        let files = [
            ("Configuration.xml", CONFIGURATION),
            ("Catalogs/Товары.xml", CATALOG),
            (path, module.as_str()),
        ];
        crate::test_utils::check_config_files(&files, path)
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::ValueIsFilledOfBoolean)
            .collect()
    }

    #[test]
    fn value_is_filled_of_a_boolean_attribute_read_through_a_reference_is_flagged() {
        let d = in_object_module("    А = ЗначениеЗаполнено(Ссылка.Флаг);");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_string_or_number_attribute_read_through_a_reference_is_not_flagged() {
        let d = in_object_module(
            "    А = ЗначениеЗаполнено(Ссылка.Текст);\n    Б = ЗначениеЗаполнено(Ссылка.Количество);",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_field_of_a_query_selection_or_a_table_row_is_not_flagged() {
        let d = in_object_module(
            "    Выборка = Запрос.Выполнить().Выбрать();\n    А = ЗначениеЗаполнено(Выборка.Флаг);\n    Таблица = Новый ТаблицаЗначений;\n    Строка = Таблица.Добавить();\n    Б = ЗначениеЗаполнено(Строка.Флаг);",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn value_is_filled_of_a_field_whose_receiver_is_of_unknown_type_is_not_flagged() {
        let d = in_object_module("    А = ЗначениеЗаполнено(Неизвестно.Флаг);");
        assert!(d.is_empty(), "{d:?}");
    }
}
