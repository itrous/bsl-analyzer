use syntax::sdbl_query::{collect_query_parse_errors, extract_torn_literal};
use syntax::{SyntaxKind, TextRange, TextSize};

const ISSUE_SOURCE: &str =
    include_str!("../../../parser/tests/fixtures/issue150_ext_directives.bsl");

fn torn_literal(source: &str) -> syntax::SyntaxNode {
    let parsed = parser::parse(source);
    assert!(!parsed.has_errors(), "BSL parse errors: {:?}", parsed.errors());
    parsed
        .syntax_node()
        .descendants()
        .find(|node| {
            node.kind() == SyntaxKind::LITERAL
                && node
                    .descendants_with_tokens()
                    .any(|element| element.kind() == SyntaxKind::PRE_INSERT)
        })
        .expect("torn literal")
}

fn range_of(text: &str, needle: &str) -> TextRange {
    let start = text.find(needle).unwrap_or_else(|| panic!("missing {needle:?} in {text:?}"));
    TextRange::at(TextSize::from(start as u32), TextSize::from(needle.len() as u32))
}

#[test]
fn issue_query_projection_contains_only_active_text() {
    let literal = torn_literal(ISSUE_SOURCE);
    let (query, map) = extract_torn_literal(&literal).expect("directive-aware projection");

    assert_eq!(
        query,
        "\nВЫБРАТЬ\n\tТ.Ссылка КАК Ссылка,\n\tТ.Договор КАК Договор,\n\tТ.Дата КАК Дата\nИЗ\n\tСправочник.Товары КАК Т"
    );
    assert!(!query.contains("#Вставка"));

    for active in ["Т.Договор", "Т.Дата", "Справочник.Товары"] {
        let query_range = range_of(&query, active);
        let mapped = map.map_range_to_literal(query_range);
        assert_eq!(&literal.text().to_string()[mapped], active);
    }

    let query_ast = parser::parse_sdbl(&query);
    assert!(
        collect_query_parse_errors(&query_ast).is_empty(),
        "the recovered query must be valid: {:?}",
        collect_query_parse_errors(&query_ast)
    );
}

#[test]
fn projection_drops_deleted_field_and_deleted_string_tail() {
    let source = r#"Функция Тест()
    Текст = "ВЫБРАТЬ
    #Удаление
    |Удаленное КАК Старое";
    #КонецУдаления
    #Вставка
    |Активное КАК Новое
    #КонецВставки
    |ИЗ Таблица";
    Возврат Текст;
КонецФункции"#;
    let literal = torn_literal(source);
    let (query, map) = extract_torn_literal(&literal).expect("projection");
    assert_eq!(query, "ВЫБРАТЬ\nАктивное КАК Новое\nИЗ Таблица");
    assert!(!query.contains("Удаленное"));

    let literal_text = literal.text().to_string();
    let removed = literal_text.find("Удаленное").expect("removed field") as u32;
    assert_eq!(map.map_offset_to_text(removed.into()), None);
    let removed_tail = literal_text.find("Старое\"").expect("removed tail") as u32 + 6;
    assert_eq!(map.map_offset_to_text(removed_tail.into()), None);

    let active = literal_text.find("Активное").expect("active field") as u32;
    let query_active = query.find("Активное").expect("projected active field") as u32;
    assert_eq!(map.map_offset_to_text(active.into()), Some(query_active.into()));
}

#[test]
fn map_handles_crlf_utf8_escaped_quotes_and_error_boundaries() {
    let source = concat!(
        "Функция Тест()\r\n",
        "    Текст = \"ВЫБРАТЬ \"\"ёж\"\" КАК Имя,\r\n",
        "    #Удаление\r\n",
        "    |ОченьДлинноеУдаленноеПоле КАК Старое,\r\n",
        "    #КонецУдаления\r\n",
        "    #Вставка\r\n",
        "    |НЕВЕРНО КАК Ошибка\r\n",
        "    #КонецВставки\r\n",
        "    |ИЗ Таблица\";\r\n",
        "КонецФункции"
    );
    let literal = torn_literal(source);
    let literal_text = literal.text().to_string();
    let (query, map) = extract_torn_literal(&literal).expect("projection");
    assert_eq!(query, "ВЫБРАТЬ \"ёж\" КАК Имя,\nНЕВЕРНО КАК Ошибка\nИЗ Таблица");

    let escaped_query = range_of(&query, "\"ёж\"");
    let escaped_source = map.map_range_to_literal(escaped_query);
    assert_eq!(&literal_text[escaped_source], "\"\"ёж\"\"");

    let bad_query = range_of(&query, "НЕВЕРНО");
    let bad_source = map.map_range_to_literal(bad_query);
    assert_eq!(&literal_text[bad_source], "НЕВЕРНО");

    let marker = literal_text.find("#Вставка").expect("marker") as u32;
    assert_eq!(map.map_offset_to_text(marker.into()), None);
    let deleted = literal_text.find("ОченьДлинное").expect("deleted text") as u32;
    assert_eq!(map.map_offset_to_text(deleted.into()), None);

    let closing_quote = literal_text.rfind('"').expect("closing quote") as u32;
    let eof = TextRange::empty(TextSize::from(query.len() as u32));
    assert_eq!(map.map_range_to_literal(eof), TextRange::empty(closing_quote.into()));
}

#[test]
fn actual_query_errors_before_and_after_insertion_map_to_exact_active_boundaries() {
    let cases = [
        (
            "before",
            r#"Функция Тест()
    Текст = "ВЫБРАТЬ Т. КАК До,
    #Вставка
    |1 КАК Вставлено
    #КонецВставки
    |ИЗ Таблица КАК Т";
КонецФункции"#,
            "КАК До",
        ),
        (
            "after",
            r#"Функция Тест()
    Текст = "ВЫБРАТЬ 1 КАК База,
    #Вставка
    |2 КАК Вставлено,
    #КонецВставки
    |Т. КАК После
    |ИЗ Таблица КАК Т";
КонецФункции"#,
            "КАК После",
        ),
    ];

    for (label, source, boundary) in cases {
        let literal = torn_literal(source);
        let literal_text = literal.text().to_string();
        let expected = literal_text.find(boundary).expect("active error boundary") as u32;
        let (query, map) = extract_torn_literal(&literal).expect("projection");
        let errors = collect_query_parse_errors(&parser::parse_sdbl(&query));
        assert!(!errors.is_empty(), "{label}: invalid query is the positive control");
        assert!(
            errors.iter().any(|(range, _)| {
                let mapped = map.map_range_to_literal(*range);
                mapped.is_empty() && u32::from(mapped.start()) == expected
            }),
            "{label}: no actual query error mapped to the exact active boundary {expected}: {errors:?}"
        );
    }
}

#[test]
fn malformed_query_errors_map_to_active_source_not_directives() {
    let source = r#"Функция Тест()
    Текст = "ВЫБРАТЬ
    #Удаление
    |Т.Старое КАК Старое,
    #КонецУдаления
    #Вставка
    |Т. КАК Ошибка
    #КонецВставки
    |ИЗ Таблица КАК Т";
КонецФункции"#;
    let literal = torn_literal(source);
    let literal_text = literal.text().to_string();
    let (query, map) = extract_torn_literal(&literal).expect("projection");
    let query_ast = parser::parse_sdbl(&query);
    let errors = collect_query_parse_errors(&query_ast);
    assert!(!errors.is_empty(), "positive control must be an invalid query");

    for (range, _) in errors {
        let mapped = map.map_range_to_literal(range);
        let mapped_text = &literal_text[mapped];
        assert!(!mapped_text.contains('#'), "error mapped onto directive: {mapped_text:?}");
        assert!(!mapped_text.contains("Старое"), "error mapped into deleted text: {mapped_text:?}");
    }
}
