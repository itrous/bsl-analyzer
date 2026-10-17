use lexer::sdbl::{tokenize_sdbl, SdblToken, SdblTokenKind};

fn kinds(tokens: &[SdblToken]) -> Vec<SdblTokenKind> {
    tokens.iter().map(|t| t.kind).collect()
}

#[test]
fn whitespace_collapses_to_single_token() {
    let tokens = tokenize_sdbl("   \t ");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Whitespace]);
    assert_eq!(tokens[0].text.as_str(), "   \t ");
}

#[test]
fn newline_is_its_own_token() {
    let tokens = tokenize_sdbl("\n");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Newline]);
}

#[test]
fn carriage_return_is_horizontal_whitespace() {
    let tokens = tokenize_sdbl("\r\n");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Whitespace, SdblTokenKind::Newline]);
}

#[test]
fn separators_produce_individual_tokens() {
    let tokens = tokenize_sdbl("().,;");
    assert_eq!(
        kinds(&tokens),
        vec![
            SdblTokenKind::LParen,
            SdblTokenKind::RParen,
            SdblTokenKind::Dot,
            SdblTokenKind::Comma,
            SdblTokenKind::Semicolon,
        ]
    );
}

#[test]
fn single_char_markers_produce_individual_tokens() {
    let tokens = tokenize_sdbl("#&|");
    assert_eq!(
        kinds(&tokens),
        vec![SdblTokenKind::Hash, SdblTokenKind::Ampersand, SdblTokenKind::Bar,]
    );
}

#[test]
fn comparison_operators_all_six() {
    let tokens = tokenize_sdbl("= <> <= < >= >");
    let op_kinds: Vec<_> =
        tokens.iter().filter(|t| t.kind != SdblTokenKind::Whitespace).map(|t| t.kind).collect();
    assert_eq!(
        op_kinds,
        vec![
            SdblTokenKind::Eq,
            SdblTokenKind::Neq,
            SdblTokenKind::Le,
            SdblTokenKind::Lt,
            SdblTokenKind::Ge,
            SdblTokenKind::Gt,
        ]
    );
}

#[test]
fn le_longest_match_beats_lt_plus_eq() {
    let tokens = tokenize_sdbl("<=");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Le]);
}

#[test]
fn ge_longest_match_beats_gt_plus_eq() {
    let tokens = tokenize_sdbl(">=");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Ge]);
}

#[test]
fn neq_longest_match_beats_lt_plus_gt() {
    let tokens = tokenize_sdbl("<>");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Neq]);
}

#[test]
fn arithmetic_operators_all_five() {
    let tokens = tokenize_sdbl("+-*/%");
    assert_eq!(
        kinds(&tokens),
        vec![
            SdblTokenKind::Plus,
            SdblTokenKind::Minus,
            SdblTokenKind::Star,
            SdblTokenKind::Slash,
            SdblTokenKind::Percent,
        ]
    );
}

#[test]
fn leading_minus_is_its_own_token() {
    let tokens = tokenize_sdbl("-5");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Minus, SdblTokenKind::Decimal]);
}

#[test]
fn integer_literal() {
    let tokens = tokenize_sdbl("42");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Decimal]);
    assert_eq!(tokens[0].text.as_str(), "42");
}

#[test]
fn fractional_literal_beats_integer_plus_dot_plus_integer() {
    let tokens = tokenize_sdbl("3.14");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Float]);
    assert_eq!(tokens[0].text.as_str(), "3.14");
}

#[test]
fn leading_dot_is_not_a_fractional_literal() {
    let tokens = tokenize_sdbl(".5");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Dot, SdblTokenKind::Decimal]);
}

#[test]
fn date_literal_eight_digits() {
    let tokens = tokenize_sdbl("'20240101'");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Date]);
}

#[test]
fn date_literal_fourteen_digits() {
    let tokens = tokenize_sdbl("'20240101120000'");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Date]);
}

#[test]
fn seven_digit_apostrophe_form_is_not_a_date() {
    let tokens = tokenize_sdbl("'1234567'");
    assert!(
        tokens.iter().all(|t| t.kind != SdblTokenKind::Date),
        "7-digit apostrophe form must not be recognised as a Date; got {:?}",
        kinds(&tokens)
    );
}

#[test]
fn fifteen_digit_apostrophe_form_is_not_a_date() {
    let tokens = tokenize_sdbl("'123456789012345'");
    assert!(
        tokens.iter().all(|t| t.kind != SdblTokenKind::Date),
        "15-digit apostrophe form must not be recognised as a Date; got {:?}",
        kinds(&tokens)
    );
}

#[test]
fn ascii_identifier() {
    let tokens = tokenize_sdbl("Name_1");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Ident]);
}

#[test]
fn cyrillic_identifier() {
    let tokens = tokenize_sdbl("Наименование");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Ident]);
}

#[test]
fn leading_underscore_identifier() {
    let tokens = tokenize_sdbl("_private");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Ident]);
}

#[test]
fn leading_digit_does_not_form_a_single_identifier() {
    let tokens = tokenize_sdbl("1abc");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Decimal, SdblTokenKind::Ident]);
}

#[test]
fn parameter_ascii_name() {
    let tokens = tokenize_sdbl("&Start");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Parameter]);
}

#[test]
fn parameter_cyrillic_name() {
    let tokens = tokenize_sdbl("&Имя");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Parameter]);
}

#[test]
fn bare_ampersand_is_not_a_parameter() {
    let tokens = tokenize_sdbl("&");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Ampersand]);
}

#[test]
fn hash_standalone() {
    let tokens = tokenize_sdbl("#");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Hash]);
}

#[test]
fn hash_plus_identifier_is_two_tokens() {
    let tokens = tokenize_sdbl("#TempT");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Hash, SdblTokenKind::Ident]);
}

#[test]
fn line_comment_stops_at_newline() {
    let tokens = tokenize_sdbl("// hello\n");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Comment, SdblTokenKind::Newline]);
    assert_eq!(tokens[0].text.as_str(), "// hello");
}

#[test]
fn line_comment_without_trailing_newline() {
    let tokens = tokenize_sdbl("// end of file");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::Comment]);
}

#[test]
fn string_basic_open_content_close() {
    let tokens = tokenize_sdbl(r#""Hello""#);
    assert_eq!(
        kinds(&tokens),
        vec![SdblTokenKind::String, SdblTokenKind::String, SdblTokenKind::String]
    );
    assert_eq!(tokens[0].text.as_str(), "\"");
    assert_eq!(tokens[1].text.as_str(), "Hello");
    assert_eq!(tokens[2].text.as_str(), "\"");
}

#[test]
fn string_unterminated_at_eof() {
    let tokens = tokenize_sdbl("\"oops");
    assert_eq!(kinds(&tokens), vec![SdblTokenKind::String, SdblTokenKind::String]);
    assert_eq!(tokens[0].text.as_str(), "\"");
    assert_eq!(tokens[1].text.as_str(), "oops");
}

#[test]
fn string_multiline_with_bar_continuation() {
    let tokens = tokenize_sdbl("\"head\n    | tail\"");
    assert_eq!(
        kinds(&tokens),
        vec![SdblTokenKind::String, SdblTokenKind::String, SdblTokenKind::String,]
    );
    assert_eq!(tokens[1].text.as_str(), "head\n    | tail");
    assert_eq!(tokens.iter().map(|t| t.text.as_str()).collect::<String>(), "\"head\n    | tail\"");
}

#[test]
fn string_body_with_parentheses_is_preserved() {
    let tokens = tokenize_sdbl("\"(a, b)\"");
    assert!(tokens.iter().all(|t| t.kind == SdblTokenKind::String));
    assert_eq!(tokens[1].text.as_str(), "(a, b)");
}

#[test]
fn string_doubled_quote_escape_preserves_the_source_bytes() {
    let tokens = tokenize_sdbl(r#""A""B""#);
    assert_eq!(
        kinds(&tokens),
        vec![SdblTokenKind::String, SdblTokenKind::String, SdblTokenKind::String]
    );
    assert_eq!(tokens[0].text.as_str(), "\"");
    assert_eq!(tokens.iter().map(|t| t.text.as_str()).collect::<String>(), r#""A""B""#);
}

#[test]
fn string_tokens_cover_line_endings_and_keep_following_offsets() {
    for line_ending in ["\n", "\r\n", "\r"] {
        let input = format!("\"A{line_ending}\tБ\" КАК Поле");
        let tokens = tokenize_sdbl(&input);
        assert_eq!(tokens.iter().map(|t| t.text.as_str()).collect::<String>(), input);
        for token in &tokens {
            let start = token.offset;
            let end = start + token.text.len();
            assert_eq!(&input[start..end], token.text.as_str());
        }
        assert!(tokens.iter().any(|t| t.text.as_str() == "КАК"));
    }
}

#[test]
fn string_token_ranges_cover_quote_runs_and_every_utf8_truncation() {
    let samples = (1..=32)
        .map(|quotes| "\"".repeat(quotes))
        .chain([r#""А""Б"#.to_string(), "\"未\n\t知\"后".to_string()]);
    for input in samples {
        for end in (0..=input.len()).filter(|end| input.is_char_boundary(*end)) {
            let prefix = &input[..end];
            let tokens = tokenize_sdbl(prefix);
            assert_eq!(tokens.iter().map(|t| t.text.as_str()).collect::<String>(), prefix);
            let mut next = 0;
            for token in tokens {
                assert_eq!(token.offset, next);
                let end = next + token.text.len();
                assert_eq!(&prefix[next..end], token.text.as_str());
                next = end;
            }
            assert_eq!(next, prefix.len());
        }
    }
    assert!(tokenize_sdbl("").is_empty());
}

#[test]
fn bsl_lexer_control_keeps_multiline_string_source_bytes() {
    let source = "Процедура Тест()\nЗапрос.Текст = \"ВЫБРАТЬ \"\"А\"\" КАК Имя\n    |ИЗ Справочник.Товары\";\nКонецПроцедуры";
    let tokens = lexer::tokenize(source);
    assert_eq!(tokens.iter().map(|token| token.text.as_str()).collect::<String>(), source);
    for token in tokens {
        let end = token.offset + token.text.len();
        assert_eq!(&source[token.offset..end], token.text.as_str());
    }
}

#[test]
fn empty_and_separated_string_literals_keep_their_boundaries() {
    let empty = tokenize_sdbl("\"\"");
    assert_eq!(empty.iter().map(|token| token.text.as_str()).collect::<String>(), "\"\"");
    let separate = tokenize_sdbl("\"А\" \"Б\"");
    assert_eq!(separate.iter().map(|token| token.text.as_str()).collect::<String>(), "\"А\" \"Б\"");
    assert_eq!(separate.iter().filter(|token| token.kind == SdblTokenKind::String).count(), 6);
}
