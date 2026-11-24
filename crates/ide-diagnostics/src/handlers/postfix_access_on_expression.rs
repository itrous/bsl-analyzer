use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::LocalRange;
use ide_db::TextRange;
use syntax::{SyntaxElement, SyntaxKind, SyntaxNode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 2,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

/// Reports `.`, `[…]` or `(…)` written directly after an operand the platform does not
/// let them follow.
///
/// The parser builds these chains on purpose — the tree must survive the editor's
/// half-typed text — but the platform compiler refuses them at the operator («Неопознанный
/// оператор» in a statement, «Ожидается символ ')'» inside an argument list), and a module
/// that does not compile fails as a whole at its first call. Measured on 8.3.17 and on
/// 8.3.27: every postfix operator is refused after a parenthesised expression, after `Новый`
/// in both of its forms and after a literal; after `?(…)` the index and the call are
/// refused, while `?(…).Свойство` and `?(…).Метод()` compile — so the ternary is judged by
/// the operator, not as a whole.
///
/// A dot right after `Новый` is the parser's own error, which wraps the dot in an error
/// node; the operator is then not a child of the chain node and nothing is reported twice.
pub fn check_node(node: &SyntaxNode, acc: &mut Vec<Diagnostic<LocalRange>>, ctx: &BodyContext) {
    let code = DiagnosticCode::PostfixAccessOnExpression;
    let operator = match node.kind() {
        SyntaxKind::FIELD_EXPR => SyntaxKind::DOT,
        SyntaxKind::INDEX_EXPR => SyntaxKind::L_BRACKET,
        SyntaxKind::CALL_EXPR => SyntaxKind::ARG_LIST,
        _ => return,
    };
    if ctx.is_disabled_with_metadata(code) {
        return;
    }
    let Some(receiver) = node.first_child() else {
        return;
    };
    if !is_refused(receiver.kind(), node.kind()) || !is_complete(&receiver) {
        return;
    }
    let Some(operator_start) = node
        .children_with_tokens()
        .skip_while(|element| element.as_node() != Some(&receiver))
        .skip(1)
        .find(|element| element.kind() == operator)
        .map(|element: SyntaxElement| element.text_range().start())
    else {
        return;
    };
    let range = TextRange::new(operator_start, node.text_range().end());
    acc.push(Diagnostic {
        code,
        message: message(receiver.kind()).to_string(),
        severity: ctx.severity(code),
        range: LocalRange::of_detached_node(range),
        tags: ctx.tags(code),
        fixes: vec![],
    });
}

fn is_refused(receiver: SyntaxKind, access: SyntaxKind) -> bool {
    match receiver {
        SyntaxKind::PAREN_EXPR | SyntaxKind::NEW_EXPR | SyntaxKind::LITERAL => true,
        SyntaxKind::TERNARY_EXPR => access != SyntaxKind::FIELD_EXPR,
        _ => false,
    }
}

/// A bracketed receiver without its closing parenthesis is already the parser's error;
/// the operator after it is the parser's recovery, not something the author wrote.
fn is_complete(receiver: &SyntaxNode) -> bool {
    match receiver.kind() {
        SyntaxKind::PAREN_EXPR | SyntaxKind::TERNARY_EXPR => {
            receiver.last_token().is_some_and(|token| token.kind() == SyntaxKind::R_PAREN)
        }
        _ => true,
    }
}

fn message(receiver: SyntaxKind) -> &'static str {
    match receiver {
        SyntaxKind::PAREN_EXPR => {
            "Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную"
        }
        SyntaxKind::NEW_EXPR => {
            "Обращение к результату оператора Новый не компилируется: сохраните созданный объект в переменную"
        }
        SyntaxKind::TERNARY_EXPR => {
            "Индекс или вызов у результата ?(…) не компилируется: сохраните значение в переменную"
        }
        _ => "Обращение к литералу не компилируется",
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{
        check_diagnostics_for, check_diagnostics_for_with_config, format_diags,
    };
    use crate::{DiagnosticCode, DiagnosticsConfig};
    use expect_test::expect;

    fn findings(code: &str) -> String {
        let diags = check_diagnostics_for(code, DiagnosticCode::PostfixAccessOnExpression);
        format_diags(code, &diags)
    }

    fn parse_errors(code: &str) -> String {
        let diags = check_diagnostics_for(code, DiagnosticCode::ParseError);
        format_diags(code, &diags)
    }

    #[test]
    fn refused_receivers_are_reported_at_the_operator() {
        let code = r#"Процедура Тест(ИмяФайла, Ф, С, М)
    Р = (Новый Файл(ИмяФайла)).Размер();
    Р = (Ф).Размер();
    (Ф).Размер();
    Р = ((Ф)).Размер();
    Р = (С).А;
    (С).А = 7;
    Р = (М)[0];
    Р = (Ф)(1);
    Р = (1).Х;
    Р = Новый Массив(2)[0];
    Р = Новый("Массив", М)[0];
    Р = Новый Массив[0];
    Р = Новый Массив(2)(1);
    Р = "абв".Длина;
    Р = "абв"[0];
    Р = "абв"(1);
    Р = Неопределено.Х;
    Р = Неопределено(1);
    Р = Истина.Х;
    Р = Истина[0];
    Р = '20200101'.Х;
    Р = 1[0];
    Р = ?(Истина, М, М)[0];
    Р = ?(Истина, М, М)(1);
КонецПроцедуры"#;
        expect![[r#"
            PostfixAccessOnExpression @ 2:31..2:38
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 3:12..3:19
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 4:8..4:15
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 5:14..5:21
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 6:12..6:14
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 7:8..7:10
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 8:12..8:15
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 9:12..9:15
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 10:12..10:14
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 11:24..11:27
              message: Обращение к результату оператора Новый не компилируется: сохраните созданный объект в переменную
              severity: Critical
            PostfixAccessOnExpression @ 12:27..12:30
              message: Обращение к результату оператора Новый не компилируется: сохраните созданный объект в переменную
              severity: Critical
            PostfixAccessOnExpression @ 13:21..13:24
              message: Обращение к результату оператора Новый не компилируется: сохраните созданный объект в переменную
              severity: Critical
            PostfixAccessOnExpression @ 14:24..14:27
              message: Обращение к результату оператора Новый не компилируется: сохраните созданный объект в переменную
              severity: Critical
            PostfixAccessOnExpression @ 15:14..15:20
              message: Обращение к литералу не компилируется
              severity: Critical
            PostfixAccessOnExpression @ 16:14..16:17
              message: Обращение к литералу не компилируется
              severity: Critical
            PostfixAccessOnExpression @ 17:14..17:17
              message: Обращение к литералу не компилируется
              severity: Critical
            PostfixAccessOnExpression @ 18:21..18:23
              message: Обращение к литералу не компилируется
              severity: Critical
            PostfixAccessOnExpression @ 19:21..19:24
              message: Обращение к литералу не компилируется
              severity: Critical
            PostfixAccessOnExpression @ 20:15..20:17
              message: Обращение к литералу не компилируется
              severity: Critical
            PostfixAccessOnExpression @ 21:15..21:18
              message: Обращение к литералу не компилируется
              severity: Critical
            PostfixAccessOnExpression @ 22:19..22:21
              message: Обращение к литералу не компилируется
              severity: Critical
            PostfixAccessOnExpression @ 23:10..23:13
              message: Обращение к литералу не компилируется
              severity: Critical
            PostfixAccessOnExpression @ 24:24..24:27
              message: Индекс или вызов у результата ?(…) не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 25:24..25:27
              message: Индекс или вызов у результата ?(…) не компилируется: сохраните значение в переменную
              severity: Critical"#]].assert_eq(&findings(code));
        assert_eq!(parse_errors(code), "");
    }

    #[test]
    fn accepted_chains_stay_silent() {
        let code = r#"Процедура Тест(ИмяФайла, Ф, С, М)
    Р = (С.А);
    Р = (Ф.Размер());
    Р = (Новый Файл(ИмяФайла));
    Р = СтрРазделить("а,б", ",")[0];
    Р = ?(Истина, Ф, Ф).Размер();
    Р = ?(Истина, С, С).А;
    Р = Метаданные.Справочники.Количество();
    Р = Метаданные.Справочники.Получить(0).Имя;
    Р = Ф.Размер().Х;
    Р = М[0][1];
    Р = Новый Файл;
    Н = Новый Массив(2);
    Р = Н[0];
    Ф.Размер();
    С.А = 7;
КонецПроцедуры"#;
        assert_eq!(findings(code), "");
        // Silence must come from the rule, not from a tree the parser gave up on.
        assert_eq!(parse_errors(code), "");
    }

    #[test]
    fn parenthesized_constructor_in_a_negated_condition_is_reported() {
        let code = include_str!("fixtures/parenthesized_new_postfix.bsl");
        let diagnostics = check_diagnostics_for(code, DiagnosticCode::PostfixAccessOnExpression);
        let spans: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                &code[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]
            })
            .collect();

        assert_eq!(spans, [".Существует", ".ПолноеИмя"]);
        assert_eq!(parse_errors(code), "");
    }

    #[test]
    fn a_dot_after_new_is_left_to_the_parser() {
        let code = r#"Процедура Тест(ИмяФайла)
    Р = Новый Файл(ИмяФайла).Размер();
    Р = Новый("Массив").Количество();
КонецПроцедуры"#;
        assert_eq!(findings(code), "");
        expect![[r#"
            ParseError @ 2:29..2:30
              message: После конструктора нельзя обращаться к свойству или методу напрямую
              severity: Critical
            ParseError @ 3:24..3:25
              message: После конструктора нельзя обращаться к свойству или методу напрямую
              severity: Critical"#]]
        .assert_eq(&parse_errors(code));
    }

    #[test]
    fn an_unfinished_ternary_is_left_to_the_parser() {
        let code = r#"Процедура Тест(М)
    Р = ?[0];
КонецПроцедуры"#;
        assert_eq!(findings(code), "");
        assert_ne!(parse_errors(code), "");
    }

    #[test]
    fn one_report_per_chain() {
        let code = r#"Процедура Тест(Ф, М)
    Р = (Ф).Родитель.Родитель.Имя();
    Р = (М)[0][1];
КонецПроцедуры"#;
        expect![[r#"
            PostfixAccessOnExpression @ 2:12..2:21
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical
            PostfixAccessOnExpression @ 3:12..3:15
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical"#]].assert_eq(&findings(code));
    }

    #[test]
    fn a_disabled_rule_reports_nothing() {
        let code = r#"Процедура Тест(Ф)
    Р = (Ф).Размер();
КонецПроцедуры"#;
        let mut config = DiagnosticsConfig::default();
        config.disabled.push(DiagnosticCode::PostfixAccessOnExpression);
        let diags = check_diagnostics_for_with_config(
            code,
            config,
            DiagnosticCode::PostfixAccessOnExpression,
        );
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(check_diagnostics_for(code, DiagnosticCode::PostfixAccessOnExpression).len(), 1);
    }

    #[test]
    fn module_level_statements_are_checked_too() {
        let code = r#"Перем Р;
Р = (Р).А;"#;
        expect![[r#"
            PostfixAccessOnExpression @ 2:8..2:10
              message: Обращение к результату выражения в скобках не компилируется: сохраните значение в переменную
              severity: Critical"#]].assert_eq(&findings(code));
    }
}
