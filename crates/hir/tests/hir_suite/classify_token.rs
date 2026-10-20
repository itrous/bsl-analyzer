use hir::{classify_token, NameClass};
use parser::parse;
use syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

fn token_with_text(root: &SyntaxNode, text: &str) -> SyntaxToken {
    root.descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.text() == text)
        .unwrap_or_else(|| panic!("no token with text {text:?} in fixture"))
}

fn classify_in(src: &str, target: &str) -> NameClass {
    let parse = parse(src);
    let root = parse.syntax_node();
    let token = token_with_text(&root, target);
    classify_token(&token)
}

#[test]
fn ident_in_expression_position_is_free_name() {
    let src = r#"Процедура Тест()
    Х = МояФункция();
КонецПроцедуры
"#;
    let class = classify_in(src, "МояФункция");
    assert!(matches!(class, NameClass::FreeName { .. }), "got {class:?}");
}

#[test]
fn ident_after_dot_no_parens_is_field_name_not_call() {
    let src = r#"Процедура Тест()
    Запрос = Новый Запрос;
    Х = Запрос.Текст;
КонецПроцедуры
"#;
    let class = classify_in(src, "Текст");
    match class {
        NameClass::FieldName { is_call, token, .. } => {
            assert!(!is_call, "no parens — must be is_call=false");
            assert_eq!(token.text(), "Текст");
            assert_eq!(token.kind(), SyntaxKind::IDENT);
        }
        other => panic!("expected FieldName, got {other:?}"),
    }
}

#[test]
fn keyword_after_dot_with_parens_is_field_name_with_is_call_true() {
    let src = r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ 1";
    Результат = Запрос.Выполнить();
КонецПроцедуры
"#;
    let class = classify_in(src, "Выполнить");
    match class {
        NameClass::FieldName { is_call, token, .. } => {
            assert!(is_call, "parens follow — must be is_call=true");
            assert_eq!(token.text(), "Выполнить");
            assert_eq!(
                token.kind(),
                SyntaxKind::KW_EXECUTE,
                "lexer must keep this as KW_EXECUTE — that's exactly the case the classifier must handle"
            );
        }
        other => panic!("expected FieldName(is_call=true), got {other:?}"),
    }
}

#[test]
fn keyword_in_statement_position_is_free_name_or_keyword() {
    let src = r#"Процедура Тест()
    Выполнить("Сообщить()");
КонецПроцедуры
"#;
    let class = classify_in(src, "Выполнить");
    assert!(
        !matches!(class, NameClass::FieldName { .. }),
        "global Выполнить statement must not classify as FieldName, got {class:?}"
    );
}

#[test]
fn ident_after_kw_new_is_type_ref() {
    let src = r#"Процедура Тест()
    Х = Новый Запрос;
КонецПроцедуры
"#;
    let class = classify_in(src, "Запрос");
    match class {
        NameClass::TypeRef { token } => {
            assert_eq!(token.text(), "Запрос");
        }
        other => panic!("expected TypeRef, got {other:?}"),
    }
}

#[test]
fn boolean_keyword_is_literal_not_keyword() {
    let src = r#"Процедура Тест()
    Х = Истина;
КонецПроцедуры
"#;
    let class = classify_in(src, "Истина");
    match class {
        NameClass::Literal { token } => assert_eq!(token.kind(), SyntaxKind::KW_TRUE),
        other => panic!("expected Literal, got {other:?}"),
    }
}

#[test]
fn null_keyword_is_literal() {
    let src = r#"Процедура Тест()
    Х = Null;
КонецПроцедуры
"#;
    let class = classify_in(src, "Null");
    assert!(matches!(class, NameClass::Literal { .. }), "got {class:?}");
}

#[test]
fn control_flow_keyword_is_keyword() {
    let src = r#"Процедура Тест()
    Если Истина Тогда
    КонецЕсли;
КонецПроцедуры
"#;
    let class = classify_in(src, "Если");
    assert!(matches!(class, NameClass::Keyword { .. }), "got {class:?}");
}

#[test]
fn dot_is_other() {
    let src = r#"Процедура Тест()
    Запрос = Новый Запрос;
    Х = Запрос.Текст;
КонецПроцедуры
"#;
    let parse = parse(src);
    let root = parse.syntax_node();
    let dot = root
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::DOT)
        .expect("must have a DOT token");
    assert!(matches!(classify_token(&dot), NameClass::Other));
}

#[test]
fn whitespace_is_other() {
    let src = r#"Процедура Тест()
    Х = 1;
КонецПроцедуры
"#;
    let parse = parse(src);
    let root = parse.syntax_node();
    let ws = root
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::WHITESPACE)
        .expect("must have whitespace");
    assert!(matches!(classify_token(&ws), NameClass::Other));
}

#[test]
fn literal_keyword_after_dot_classifies_as_field_name_not_literal() {
    let src = r#"Процедура Тест()
    Х = obj.Истина;
КонецПроцедуры
"#;
    let class = classify_in(src, "Истина");
    assert!(
        matches!(class, NameClass::FieldName { .. }),
        "literal-keyword in field-tail must classify as FieldName, got {class:?}"
    );
}

#[test]
fn nested_qualified_keyword_segment_classifies_as_field_name() {
    let src = r#"Процедура Тест()
    Х = A.Выполнить.B;
КонецПроцедуры
"#;
    let class = classify_in(src, "Выполнить");
    assert!(matches!(class, NameClass::FieldName { .. }), "got {class:?}");
}
