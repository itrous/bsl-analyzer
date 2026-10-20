use lexer::sdbl::{tokenize_sdbl, SdblToken, SdblTokenKind};

fn significant(tokens: &[SdblToken]) -> Vec<(SdblTokenKind, String)> {
    tokens
        .iter()
        .filter(|t| t.kind != SdblTokenKind::Whitespace && t.kind != SdblTokenKind::Newline)
        .map(|t| (t.kind, t.text.to_string()))
        .collect()
}

fn single_kind(src: &str) -> SdblTokenKind {
    let toks: Vec<_> = tokenize_sdbl(src)
        .into_iter()
        .filter(|t| t.kind != SdblTokenKind::Whitespace && t.kind != SdblTokenKind::Newline)
        .collect();
    assert_eq!(toks.len(), 1, "expected exactly one token for {src:?}, got {toks:#?}");
    toks[0].kind
}

#[test]
fn clause_starters_bilingual() {
    let pairs: &[(&str, &str, SdblTokenKind)] = &[
        ("ВЫБРАТЬ", "SELECT", SdblTokenKind::KwSelect),
        ("ИЗ", "FROM", SdblTokenKind::KwFrom),
        ("ПОМЕСТИТЬ", "INTO", SdblTokenKind::KwInto),
        ("ГДЕ", "WHERE", SdblTokenKind::KwWhere),
        ("СГРУППИРОВАТЬ", "GROUP", SdblTokenKind::KwGroup),
        ("УПОРЯДОЧИТЬ", "ORDER", SdblTokenKind::KwOrder),
        ("ИМЕЮЩИЕ", "HAVING", SdblTokenKind::KwHaving),
        ("ИТОГИ", "TOTALS", SdblTokenKind::KwTotals),
        ("ОБЪЕДИНИТЬ", "UNION", SdblTokenKind::KwUnion),
        ("ВСЕ", "ALL", SdblTokenKind::KwAll),
        ("РАЗЛИЧНЫЕ", "DISTINCT", SdblTokenKind::KwDistinct),
        ("ПЕРВЫЕ", "TOP", SdblTokenKind::KwTop),
    ];
    for (rus, eng, expected) in pairs {
        assert_eq!(single_kind(rus), *expected, "Russian {rus} should lex as {expected:?}");
        assert_eq!(single_kind(eng), *expected, "English {eng} should lex as {expected:?}");
    }
}

#[test]
fn join_family_bilingual() {
    let pairs: &[(&str, &str, SdblTokenKind)] = &[
        ("СОЕДИНЕНИЕ", "JOIN", SdblTokenKind::KwJoin),
        ("ВНУТРЕННЕЕ", "INNER", SdblTokenKind::KwInner),
        ("ЛЕВОЕ", "LEFT", SdblTokenKind::KwLeft),
        ("ПРАВОЕ", "RIGHT", SdblTokenKind::KwRight),
        ("ПОЛНОЕ", "FULL", SdblTokenKind::KwFull),
        ("ВНЕШНЕЕ", "OUTER", SdblTokenKind::KwOuter),
    ];
    for (rus, eng, expected) in pairs {
        assert_eq!(single_kind(rus), *expected);
        assert_eq!(single_kind(eng), *expected);
    }
}

#[test]
fn aliasing_and_predicates_bilingual() {
    let pairs: &[(&str, &str, SdblTokenKind)] = &[
        ("КАК", "AS", SdblTokenKind::KwAs),
        ("В", "IN", SdblTokenKind::KwIn),
        ("МЕЖДУ", "BETWEEN", SdblTokenKind::KwBetween),
        ("ПОДОБНО", "LIKE", SdblTokenKind::KwLike),
        ("ЕСТЬ", "IS", SdblTokenKind::KwIs),
    ];
    for (rus, eng, expected) in pairs {
        assert_eq!(single_kind(rus), *expected);
        assert_eq!(single_kind(eng), *expected);
    }
}

#[test]
fn case_family_bilingual() {
    let pairs: &[(&str, &str, SdblTokenKind)] = &[
        ("ВЫБОР", "CASE", SdblTokenKind::KwCase),
        ("КОГДА", "WHEN", SdblTokenKind::KwWhen),
        ("ТОГДА", "THEN", SdblTokenKind::KwThen),
        ("ИНАЧЕ", "ELSE", SdblTokenKind::KwElse),
        ("КОНЕЦ", "END", SdblTokenKind::KwEnd),
    ];
    for (rus, eng, expected) in pairs {
        assert_eq!(single_kind(rus), *expected);
        assert_eq!(single_kind(eng), *expected);
    }
}

#[test]
fn logical_operators_bilingual() {
    assert_eq!(single_kind("И"), SdblTokenKind::OpAnd);
    assert_eq!(single_kind("AND"), SdblTokenKind::OpAnd);
    assert_eq!(single_kind("ИЛИ"), SdblTokenKind::OpOr);
    assert_eq!(single_kind("OR"), SdblTokenKind::OpOr);
    assert_eq!(single_kind("НЕ"), SdblTokenKind::OpNot);
    assert_eq!(single_kind("NOT"), SdblTokenKind::OpNot);
}

#[test]
fn boolean_and_null_literals() {
    assert_eq!(single_kind("ИСТИНА"), SdblTokenKind::LitTrue);
    assert_eq!(single_kind("TRUE"), SdblTokenKind::LitTrue);
    assert_eq!(single_kind("ЛОЖЬ"), SdblTokenKind::LitFalse);
    assert_eq!(single_kind("FALSE"), SdblTokenKind::LitFalse);
    assert_eq!(single_kind("NULL"), SdblTokenKind::LitNull);
}

#[test]
fn case_insensitivity_english() {
    for s in ["SELECT", "Select", "select", "sElEcT"] {
        let toks = tokenize_sdbl(s);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, SdblTokenKind::KwSelect);
        assert_eq!(toks[0].text.as_str(), s);
    }
}

#[test]
fn case_insensitivity_russian() {
    for s in ["ВЫБРАТЬ", "Выбрать", "выбрать", "вЫбРаТь"] {
        let toks = tokenize_sdbl(s);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, SdblTokenKind::KwSelect);
        assert_eq!(toks[0].text.as_str(), s);
    }
}

#[test]
fn case_insensitivity_join_family() {
    assert_eq!(single_kind("Left"), SdblTokenKind::KwLeft);
    assert_eq!(single_kind("lEfT"), SdblTokenKind::KwLeft);
    assert_eq!(single_kind("inner"), SdblTokenKind::KwInner);
    assert_eq!(single_kind("OUTER"), SdblTokenKind::KwOuter);
}

#[test]
fn case_insensitivity_case_family() {
    assert_eq!(single_kind("Case"), SdblTokenKind::KwCase);
    assert_eq!(single_kind("WHEN"), SdblTokenKind::KwWhen);
    assert_eq!(single_kind("tHeN"), SdblTokenKind::KwThen);
    assert_eq!(single_kind("else"), SdblTokenKind::KwElse);
    assert_eq!(single_kind("End"), SdblTokenKind::KwEnd);
}

#[test]
fn case_insensitivity_predicates() {
    assert_eq!(single_kind("Between"), SdblTokenKind::KwBetween);
    assert_eq!(single_kind("lIkE"), SdblTokenKind::KwLike);
    assert_eq!(single_kind("is"), SdblTokenKind::KwIs);
}

#[test]
fn keyword_prefix_identifiers_lex_as_ident() {
    for s in ["SELECTED", "FROMAGE", "WHEREVER", "ANDROID", "UNIONIZED", "BETWEEN_INDEX"] {
        let toks = tokenize_sdbl(s);
        assert_eq!(toks.len(), 1, "expected one token for {s:?}");
        assert_eq!(toks[0].kind, SdblTokenKind::Ident, "{s:?} should lex as Ident");
        assert_eq!(toks[0].text.as_str(), s);
    }
}

#[test]
fn keyword_prefix_identifiers_lex_as_ident_cyrillic() {
    for s in ["ИСТИНАLIKE", "ВЫБРАТЬ_КОЛОНКА", "СОЕДИНЕНИЕ1С"] {
        let toks = tokenize_sdbl(s);
        assert_eq!(toks.len(), 1, "expected one token for {s:?}");
        assert_eq!(toks[0].kind, SdblTokenKind::Ident);
        assert_eq!(toks[0].text.as_str(), s);
    }
}

#[test]
fn keyword_plus_digit_is_ident() {
    for s in ["SELECT1", "FROM2", "WHERE3", "CASE4"] {
        let toks = tokenize_sdbl(s);
        assert_eq!(toks.len(), 1, "expected one token for {s:?}");
        assert_eq!(toks[0].kind, SdblTokenKind::Ident);
        assert_eq!(toks[0].text.as_str(), s);
    }
}

#[test]
fn fused_keywords_lex_as_ident() {
    for s in ["CASEWHEN", "SELECTFROM", "GROUPBY"] {
        let toks = tokenize_sdbl(s);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, SdblTokenKind::Ident, "{s:?} should lex as Ident");
    }
}

#[test]
fn select_adjacent_lparen() {
    let toks = significant(&tokenize_sdbl("SELECT("));
    assert_eq!(
        toks,
        vec![
            (SdblTokenKind::KwSelect, "SELECT".to_string()),
            (SdblTokenKind::LParen, "(".to_string()),
        ]
    );
}

#[test]
fn where_adjacent_parameter() {
    let toks = significant(&tokenize_sdbl("WHERE&P"));
    assert_eq!(
        toks,
        vec![
            (SdblTokenKind::KwWhere, "WHERE".to_string()),
            (SdblTokenKind::Parameter, "&P".to_string()),
        ]
    );
}

#[test]
fn true_adjacent_comma() {
    let toks = significant(&tokenize_sdbl("TRUE,FALSE"));
    assert_eq!(
        toks,
        vec![
            (SdblTokenKind::LitTrue, "TRUE".to_string()),
            (SdblTokenKind::Comma, ",".to_string()),
            (SdblTokenKind::LitFalse, "FALSE".to_string()),
        ]
    );
}

#[test]
fn is_null_in_where_adjacency() {
    let toks = significant(&tokenize_sdbl("(X IS NULL)"));
    assert_eq!(
        toks,
        vec![
            (SdblTokenKind::LParen, "(".to_string()),
            (SdblTokenKind::Ident, "X".to_string()),
            (SdblTokenKind::KwIs, "IS".to_string()),
            (SdblTokenKind::LitNull, "NULL".to_string()),
            (SdblTokenKind::RParen, ")".to_string()),
        ]
    );
}

#[test]
fn on_by_po_share_kwonorby() {
    assert_eq!(single_kind("ON"), SdblTokenKind::KwOnOrBy);
    assert_eq!(single_kind("on"), SdblTokenKind::KwOnOrBy);
    assert_eq!(single_kind("BY"), SdblTokenKind::KwOnOrBy);
    assert_eq!(single_kind("by"), SdblTokenKind::KwOnOrBy);
    assert_eq!(single_kind("ПО"), SdblTokenKind::KwOnOrBy);
    assert_eq!(single_kind("по"), SdblTokenKind::KwOnOrBy);
}

#[test]
fn kwonorby_preserves_original_text() {
    for s in ["ON", "BY", "ПО", "on", "bY", "По"] {
        let toks = tokenize_sdbl(s);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, SdblTokenKind::KwOnOrBy);
        assert_eq!(toks[0].text.as_str(), s);
    }
}

#[test]
fn keyword_text_preserved_verbatim() {
    let src = "Выбрать ПЕРВЫЕ 10 a ГДЕ b МЕЖДУ 1 И 10";
    let toks = significant(&tokenize_sdbl(src));
    let expected: Vec<(SdblTokenKind, &str)> = vec![
        (SdblTokenKind::KwSelect, "Выбрать"),
        (SdblTokenKind::KwTop, "ПЕРВЫЕ"),
        (SdblTokenKind::Decimal, "10"),
        (SdblTokenKind::Ident, "a"),
        (SdblTokenKind::KwWhere, "ГДЕ"),
        (SdblTokenKind::Ident, "b"),
        (SdblTokenKind::KwBetween, "МЕЖДУ"),
        (SdblTokenKind::Decimal, "1"),
        (SdblTokenKind::OpAnd, "И"),
        (SdblTokenKind::Decimal, "10"),
    ];
    assert_eq!(toks, expected.into_iter().map(|(k, t)| (k, t.to_string())).collect::<Vec<_>>());
}

#[test]
fn select_distinct_top_sequence() {
    let toks: Vec<_> = significant(&tokenize_sdbl("SELECT DISTINCT TOP 5 A"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        toks,
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::KwDistinct,
            SdblTokenKind::KwTop,
            SdblTokenKind::Decimal,
            SdblTokenKind::Ident,
        ]
    );
}

#[test]
fn left_outer_join_on_sequence() {
    let toks: Vec<_> = significant(&tokenize_sdbl("LEFT OUTER JOIN T ON A = B"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        toks,
        vec![
            SdblTokenKind::KwLeft,
            SdblTokenKind::KwOuter,
            SdblTokenKind::KwJoin,
            SdblTokenKind::Ident,
            SdblTokenKind::KwOnOrBy,
            SdblTokenKind::Ident,
            SdblTokenKind::Eq,
            SdblTokenKind::Ident,
        ]
    );
}

#[test]
fn union_all_sequence() {
    let toks: Vec<_> = significant(&tokenize_sdbl("SELECT 1 UNION ALL SELECT 2"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        toks,
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::Decimal,
            SdblTokenKind::KwUnion,
            SdblTokenKind::KwAll,
            SdblTokenKind::KwSelect,
            SdblTokenKind::Decimal,
        ]
    );
}

#[test]
fn case_when_then_else_end_sequence() {
    let toks: Vec<_> = significant(&tokenize_sdbl("CASE WHEN X > 0 THEN 1 ELSE 0 END"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        toks,
        vec![
            SdblTokenKind::KwCase,
            SdblTokenKind::KwWhen,
            SdblTokenKind::Ident,
            SdblTokenKind::Gt,
            SdblTokenKind::Decimal,
            SdblTokenKind::KwThen,
            SdblTokenKind::Decimal,
            SdblTokenKind::KwElse,
            SdblTokenKind::Decimal,
            SdblTokenKind::KwEnd,
        ]
    );
}

#[test]
fn as_in_select_and_from_positions() {
    let src = "SELECT A AS B FROM T AS X";
    let toks: Vec<_> = significant(&tokenize_sdbl(src)).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        toks,
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::Ident,
            SdblTokenKind::KwAs,
            SdblTokenKind::Ident,
            SdblTokenKind::KwFrom,
            SdblTokenKind::Ident,
            SdblTokenKind::KwAs,
            SdblTokenKind::Ident,
        ]
    );
}

#[test]
fn in_predicate_with_parameter_list() {
    let toks: Vec<_> =
        significant(&tokenize_sdbl("WHERE X IN (&List)")).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        toks,
        vec![
            SdblTokenKind::KwWhere,
            SdblTokenKind::Ident,
            SdblTokenKind::KwIn,
            SdblTokenKind::LParen,
            SdblTokenKind::Parameter,
            SdblTokenKind::RParen,
        ]
    );
}

#[test]
fn between_and_predicate() {
    let toks: Vec<_> =
        significant(&tokenize_sdbl("X BETWEEN 1 AND 10")).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        toks,
        vec![
            SdblTokenKind::Ident,
            SdblTokenKind::KwBetween,
            SdblTokenKind::Decimal,
            SdblTokenKind::OpAnd,
            SdblTokenKind::Decimal,
        ]
    );
}

#[test]
fn like_string_pattern() {
    let toks: Vec<_> = tokenize_sdbl("N LIKE \"%x%\"")
        .into_iter()
        .filter(|t| t.kind != SdblTokenKind::Whitespace)
        .map(|t| t.kind)
        .collect();
    assert!(toks.contains(&SdblTokenKind::KwLike));
    assert!(matches!(toks[0], SdblTokenKind::Ident));
}

#[test]
fn slice2_addendum_kw_asc_russian() {
    assert_eq!(single_kind("ВОЗР"), SdblTokenKind::KwAsc);
}

#[test]
fn slice2_addendum_kw_desc_russian() {
    assert_eq!(single_kind("УБЫВ"), SdblTokenKind::KwDesc);
}

#[test]
fn slice2_addendum_kw_hierarchy_russian() {
    assert_eq!(single_kind("ИЕРАРХИЯ"), SdblTokenKind::KwHierarchy);
}

#[test]
fn slice2_addendum_kw_allowed_russian() {
    assert_eq!(single_kind("РАЗРЕШЕННЫЕ"), SdblTokenKind::KwAllowed);
}

#[test]
fn slice2_addendum_kw_for_russian() {
    assert_eq!(single_kind("ДЛЯ"), SdblTokenKind::KwFor);
}

#[test]
fn slice2_addendum_kw_update_russian() {
    assert_eq!(single_kind("ИЗМЕНЕНИЯ"), SdblTokenKind::KwUpdate);
}

#[test]
fn slice2_addendum_kw_index_russian() {
    assert_eq!(single_kind("ИНДЕКСИРОВАТЬ"), SdblTokenKind::KwIndex);
}

#[test]
fn slice2_addendum_kw_only_russian() {
    assert_eq!(single_kind("ТОЛЬКО"), SdblTokenKind::KwOnly);
}

#[test]
fn slice2_addendum_kw_escape_russian() {
    assert_eq!(single_kind("СПЕЦСИМВОЛ"), SdblTokenKind::KwEscape);
}

#[test]
fn slice2_addendum_kw_cast_russian() {
    assert_eq!(single_kind("ВЫРАЗИТЬ"), SdblTokenKind::KwCast);
}

#[test]
fn slice2_addendum_kw_refs_russian() {
    assert_eq!(single_kind("ССЫЛКА"), SdblTokenKind::KwRefs);
}

#[test]
fn slice2_addendum_kw_type_russian() {
    assert_eq!(single_kind("ТИП"), SdblTokenKind::KwType);
}

#[test]
fn slice2_addendum_kw_value_russian() {
    assert_eq!(single_kind("ЗНАЧЕНИЕ"), SdblTokenKind::KwValue);
}
