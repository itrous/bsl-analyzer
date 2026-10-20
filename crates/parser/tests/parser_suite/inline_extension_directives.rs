use expect_test::expect_file;
use syntax::{SyntaxKind, TextRange};

const ISSUE_SOURCE: &str = include_str!("../fixtures/issue150_ext_directives.bsl");

fn canonical_issue_source() -> &'static str {
    r#"Функция ЕстьТакойМетод() Экспорт
	Возврат Истина;
КонецФункции

// Случай 1: директива внутри многострочной строковой константы (текст запроса).
Функция ТекстЗапроса() Экспорт
	Текст = "
	|ВЫБРАТЬ
	|	Т.Ссылка КАК Ссылка,
	|	Т.Договор КАК Договор,
	|	Т.Дата КАК Дата
	|ИЗ
	|	Справочник.Товары КАК Т";
	Возврат Текст;
КонецФункции

// Случай 2: директива внутри многострочного условия.
Функция Проверка(Данные) Экспорт
	Если Не ЗначениеЗаполнено(Данные.Процент)
		И Не Данные.Флаг
		И Данные.Сумма > 0 Тогда
		Возврат Ложь;
	КонецЕсли;
	Возврат Истина;
КонецФункции

// Случай 3: #Удаление внутри списка аргументов вызова.
Функция Собрать(Данные) Экспорт
	Возврат Новый Структура(
		"Ссылка, Дата",
		Данные.НоваяСсылка,
		Данные.Дата);
КонецФункции
"#
}

fn assert_clean_lossless(source: &str) -> syntax::Parse<syntax::SyntaxNode> {
    let parsed = parser::parse(source);
    assert!(!parsed.has_errors(), "unexpected errors: {:?}", parsed.errors());
    assert_eq!(parsed.syntax_node().text().to_string(), source);
    assert!(
        parsed.syntax_node().descendants().all(|node| node.kind() != SyntaxKind::ERROR),
        "clean parse must not hide recovery ERROR nodes"
    );
    parsed
}

#[test]
fn issue_example_is_lossless_and_has_one_complete_literal() {
    let parsed = assert_clean_lossless(ISSUE_SOURCE);
    assert_clean_lossless(canonical_issue_source());
    let root = parsed.syntax_node();

    let query_literals: Vec<_> = root
        .descendants()
        .filter(|node| {
            node.kind() == SyntaxKind::LITERAL && node.text().to_string().contains("Т.Договор")
        })
        .collect();
    assert_eq!(query_literals.len(), 1, "the query must remain one LITERAL");
    let literal = &query_literals[0];
    assert_eq!(
        literal
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
            .filter(|token| token.kind() == SyntaxKind::STRING_TAIL)
            .count(),
        1,
        "the active closing line must supply the only STRING_TAIL"
    );

    expect_file!["../fixtures/issue150_ext_directives.cst"].assert_eq(&format!("{root:#?}"));
}

#[test]
fn deletion_can_replace_an_operand_and_its_string_tail() {
    let operand = r#"Функция Тест()
    Возврат 1 +
    #Удаление
        2 +
    #КонецУдаления
        3;
КонецФункции"#;
    let parsed = assert_clean_lossless(operand);
    assert_eq!(
        parsed
            .syntax_node()
            .descendants()
            .filter(|node| node.kind() == SyntaxKind::PRE_DELETE_DIR)
            .count(),
        1
    );

    let tail = r#"Функция Тест()
    Текст = "ВЫБРАТЬ
    #Удаление
    |УдаленноеПоле";
    #КонецУдаления
    #Вставка
    |АктивноеПоле";
    #КонецВставки
    Возврат Текст;
КонецФункции"#;
    let parsed = assert_clean_lossless(tail);
    let literal = parsed
        .syntax_node()
        .descendants()
        .find(|node| node.kind() == SyntaxKind::LITERAL)
        .expect("literal");
    let direct_tail_texts: Vec<_> = literal
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() == SyntaxKind::STRING_TAIL)
        .map(|token| token.text().to_string())
        .collect();
    assert_eq!(direct_tail_texts, ["|АктивноеПоле\""]);
    assert!(literal.text().to_string().contains("|УдаленноеПоле\";"));
}

#[test]
fn existing_english_markers_are_case_insensitive_but_marker_text_in_data_is_not_a_directive() {
    let source = r#"Функция Test(Data)
    Text = "SELECT
    |#Insert is data,
    #iNsErT
    |Data.Value AS Value
    #eNdInSeRt
    |FROM Data";
    // #Delete is a comment
    Return New Structure(
        "Value",
        #dElEtE
        Data.OldValue,
        #eNdDeLeTe
        Data.Value);
КонецФункции"#;
    let parsed = assert_clean_lossless(source);
    let tokens: Vec<_> = parsed
        .syntax_node()
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .map(|token| token.kind())
        .collect();
    assert_eq!(tokens.iter().filter(|&&kind| kind == SyntaxKind::PRE_INSERT).count(), 1);
    assert_eq!(tokens.iter().filter(|&&kind| kind == SyntaxKind::PRE_DELETE).count(), 1);
    assert!(tokens.contains(&SyntaxKind::COMMENT));
}

#[test]
fn statement_boundary_directives_keep_their_opaque_nodes() {
    let source = r#"Процедура Тест()
    #Удаление
    А = 1;
    #КонецУдаления
    #Вставка
    Б = 2
    #КонецВставки
    В = 3;
КонецПроцедуры"#;
    let parsed = assert_clean_lossless(source);
    let kinds: Vec<_> = parsed.syntax_node().descendants().map(|node| node.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::PRE_DELETE_DIR));
    assert!(kinds.contains(&SyntaxKind::PRE_INSERT_DIR));
}

#[test]
fn real_syntax_errors_and_malformed_markers_remain_visible() {
    let missing_then = r#"Функция Тест()
    Если Истина
        #Вставка
        И Ложь
        #КонецВставки
        Возврат 1;
    КонецЕсли;
КонецФункции"#;
    assert!(parser::parse(missing_then).has_errors(), "missing Тогда must still be rejected");

    for source in [
        "Функция Тест()\nВозврат 1 #КонецВставки;\nКонецФункции",
        "Функция Тест()\nВозврат 1 + #Удаление 2;\nКонецФункции",
        "Функция Тест()\nВозврат 1 + #Вставка 2;\nКонецФункции",
    ] {
        assert!(parser::parse(source).has_errors(), "malformed marker must be reported: {source}");
    }

    let bad_operator = "Функция Тест()\nВозврат 1\n#Вставка\n+ * 2\n#КонецВставки;\nКонецФункции";
    let parsed = parser::parse(bad_operator);
    let star = bad_operator.find('*').expect("star") as u32;
    assert!(
        parsed.errors().iter().any(|error| {
            let range = error.range();
            range.contains(star.into()) || range == TextRange::empty(star.into())
        }),
        "the genuine error must stay at the original '*' offset: {:?}",
        parsed.errors()
    );
}
