use parser::parse_sdbl;
use syntax::SyntaxKind;

fn assert_clean(input: &str) -> syntax::SyntaxNode {
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "Expected clean parse for `{}`; got errors: {:#?}",
        input,
        parse.errors(),
    );
    let root = parse.syntax_node();
    let error_descendants: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::ERROR).collect();
    assert!(
        error_descendants.is_empty(),
        "Expected no ERROR recovery nodes for `{}`; got: {:#?}",
        input,
        error_descendants,
    );
    root
}

fn find_limitations(root: &syntax::SyntaxNode) -> syntax::SyntaxNode {
    root.descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_LIMITATIONS)
        .unwrap_or_else(|| panic!("Tree must contain SdblLimitations; got: {:#?}", root))
}

fn limitations_token_text(limitations: &syntax::SyntaxNode) -> String {
    limitations
        .children_with_tokens()
        .filter_map(|c| c.as_token().map(|t| t.text().to_string()))
        .collect::<Vec<_>>()
        .join("|")
        .to_uppercase()
}

#[test]
fn test_slice7adn_distinct_canonical_ru() {
    let root = assert_clean("ВЫБРАТЬ РАЗЛИЧНЫЕ Клиент ИЗ ЗаказТовара");
    let limitations = find_limitations(&root);
    let kw_text = limitations_token_text(&limitations);
    assert!(
        kw_text.contains("РАЗЛИЧНЫЕ"),
        "SdblLimitations must contain РАЗЛИЧНЫЕ token. Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_distinct_canonical_en() {
    let root = assert_clean("SELECT DISTINCT Name FROM Products");
    let limitations = find_limitations(&root);
    let kw_text = limitations_token_text(&limitations);
    assert!(
        kw_text.contains("DISTINCT"),
        "SdblLimitations must contain DISTINCT token. Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_top_canonical_ru() {
    let root = assert_clean("ВЫБРАТЬ ПЕРВЫЕ 5 СуммаЗаказа ИЗ ЗаказТовара");
    let limitations = find_limitations(&root);
    let top_clause = limitations
        .children()
        .find(|c| c.kind() == SyntaxKind::SDBL_TOP_CLAUSE)
        .expect("SdblLimitations must have an SdblTopClause direct child");
    let has_decimal_5 = top_clause
        .children_with_tokens()
        .filter_map(|c| c.as_token().map(|t| (t.kind(), t.text().to_string())))
        .any(|(k, t)| k == SyntaxKind::DECIMAL && t == "5");
    assert!(has_decimal_5, "SdblTopClause must contain Decimal `5`");
}

#[test]
fn test_slice7adn_top_canonical_en() {
    let root = assert_clean("SELECT TOP 100 OrderTotal FROM Orders");
    let limitations = find_limitations(&root);
    let top_clause = limitations
        .children()
        .find(|c| c.kind() == SyntaxKind::SDBL_TOP_CLAUSE)
        .expect("SdblLimitations must have an SdblTopClause direct child");
    let has_decimal_100 = top_clause
        .children_with_tokens()
        .filter_map(|c| c.as_token().map(|t| (t.kind(), t.text().to_string())))
        .any(|(k, t)| k == SyntaxKind::DECIMAL && t == "100");
    assert!(has_decimal_100, "SdblTopClause must contain Decimal `100`");
}

#[test]
fn test_slice7adn_allowed_canonical_ru() {
    let root = assert_clean("ВЫБРАТЬ РАЗРЕШЕННЫЕ Наименование ИЗ Справочник.Товары");
    let limitations = find_limitations(&root);
    let kw_text = limitations_token_text(&limitations);
    assert!(
        kw_text.contains("РАЗРЕШЕННЫЕ"),
        "SdblLimitations must contain РАЗРЕШЕННЫЕ token. Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_allowed_canonical_en() {
    let root = assert_clean("SELECT ALLOWED Name FROM Catalog.Products");
    let limitations = find_limitations(&root);
    let kw_text = limitations_token_text(&limitations);
    assert!(
        kw_text.contains("ALLOWED"),
        "SdblLimitations must contain ALLOWED token. Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_q1_distinct_before_top() {
    let root = assert_clean("ВЫБРАТЬ РАЗЛИЧНЫЕ ПЕРВЫЕ 10 Код ИЗ Товары");
    let limitations = find_limitations(&root);
    let kinds: Vec<_> = limitations.children().map(|c| c.kind()).collect();
    assert!(
        kinds.contains(&SyntaxKind::SDBL_TOP_CLAUSE),
        "Limitations must include SdblTopClause for `ПЕРВЫЕ 10`. Got: {:?}",
        kinds,
    );
    let kw_text = limitations_token_text(&limitations);
    assert!(
        kw_text.contains("РАЗЛИЧНЫЕ"),
        "Limitations must include РАЗЛИЧНЫЕ. Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_q1_top_before_allowed() {
    let root = assert_clean("ВЫБРАТЬ ПЕРВЫЕ 3 РАЗРЕШЕННЫЕ Наименование ИЗ Справочник.Товары");
    let limitations = find_limitations(&root);
    let kinds: Vec<_> = limitations.children().map(|c| c.kind()).collect();
    assert!(
        kinds.contains(&SyntaxKind::SDBL_TOP_CLAUSE),
        "Limitations must include SdblTopClause for `ПЕРВЫЕ 3`. Got: {:?}",
        kinds,
    );
    let kw_text = limitations_token_text(&limitations);
    assert!(
        kw_text.contains("РАЗРЕШЕННЫЕ"),
        "Limitations must include РАЗРЕШЕННЫЕ. Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_q1_allowed_before_distinct() {
    let root = assert_clean("ВЫБРАТЬ РАЗРЕШЕННЫЕ РАЗЛИЧНЫЕ Клиент ИЗ ЗаказТовара");
    let limitations = find_limitations(&root);
    let kw_text = limitations_token_text(&limitations);
    assert!(
        kw_text.contains("РАЗРЕШЕННЫЕ") && kw_text.contains("РАЗЛИЧНЫЕ"),
        "Limitations must include both РАЗРЕШЕННЫЕ and РАЗЛИЧНЫЕ. Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_q1_all_three_canonical_order() {
    let root = assert_clean("ВЫБРАТЬ РАЗРЕШЕННЫЕ РАЗЛИЧНЫЕ ПЕРВЫЕ 10 Клиент ИЗ ЗаказТовара");
    let limitations = find_limitations(&root);
    let kinds: Vec<_> = limitations.children().map(|c| c.kind()).collect();
    let top_count = kinds.iter().filter(|&&k| k == SyntaxKind::SDBL_TOP_CLAUSE).count();
    assert_eq!(
        top_count, 1,
        "Limitations must include exactly one SdblTopClause. Got: {:?}",
        kinds,
    );
    let kw_text = limitations_token_text(&limitations);
    assert!(
        kw_text.contains("РАЗРЕШЕННЫЕ") && kw_text.contains("РАЗЛИЧНЫЕ"),
        "Limitations must include both РАЗРЕШЕННЫЕ and РАЗЛИЧНЫЕ. Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_distinct_distinct_duplicate_tolerance() {
    let root = assert_clean("ВЫБРАТЬ РАЗЛИЧНЫЕ РАЗЛИЧНЫЕ Клиент ИЗ ЗаказТовара");
    let limitations = find_limitations(&root);
    let kw_text = limitations_token_text(&limitations);
    let occurrences = kw_text.matches("РАЗЛИЧНЫЕ").count();
    assert_eq!(
        occurrences, 2,
        "Limitations must contain two РАЗЛИЧНЫЕ tokens (Q2 \
         duplicate-qualifier loop tolerance). Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_q3_top_missing_decimal_recovery() {
    let input = "ВЫБРАТЬ ПЕРВЫЕ A ИЗ Т";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let limitations = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_LIMITATIONS)
        .expect("SdblLimitations marker must still be completed when the Decimal is missing");
    let top_clause = limitations
        .children()
        .find(|c| c.kind() == SyntaxKind::SDBL_TOP_CLAUSE)
        .expect("SdblLimitations must still have an SdblTopClause direct child");
    let decimal_count = top_clause
        .children_with_tokens()
        .filter_map(|c| c.as_token().filter(|t| t.kind() == SyntaxKind::DECIMAL).cloned())
        .count();
    assert_eq!(
        decimal_count, 0,
        "SdblTopClause must have NO Decimal token when count is missing (Q3)",
    );
    let error_children: Vec<_> =
        top_clause.children().filter(|c| c.kind() == SyntaxKind::ERROR).collect();
    assert_eq!(
        error_children.len(),
        1,
        "SdblTopClause must have exactly one ERROR sub-node child (the bumped `A` Ident)",
    );
    assert!(
        error_children[0].text().to_string().contains('A'),
        "ERROR sub-node must contain the bumped `A` Ident. Got text: {:?}",
        error_children[0].text().to_string(),
    );
    let from_clauses_count =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert_eq!(
        from_clauses_count, 1,
        "The clause after a failed count survives: the selection list left \
         empty by the failure no longer takes `ИЗ` as its only field",
    );
}

#[test]
fn test_slice7adn_x_slice7_distinct_with_alias_kak() {
    let root =
        assert_clean("ВЫБРАТЬ РАЗЛИЧНЫЕ Клиент КАК Покупатель, ДатаЗаказа КАК Дата ИЗ ЗаказТовара");
    let limitations = find_limitations(&root);
    let kw_text = limitations_token_text(&limitations);
    assert!(
        kw_text.contains("РАЗЛИЧНЫЕ"),
        "Limitations must contain РАЗЛИЧНЫЕ. Got tokens: {}",
        kw_text,
    );
    let alias_count = root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_ALIAS).count();
    assert_eq!(
        alias_count, 2,
        "Two SdblAlias nodes expected (Покупатель + Дата); got {}",
        alias_count,
    );
}
