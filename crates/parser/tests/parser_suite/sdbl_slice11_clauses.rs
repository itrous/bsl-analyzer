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

fn find_kind(root: &syntax::SyntaxNode, kind: SyntaxKind) -> syntax::SyntaxNode {
    root.descendants()
        .find(|n| n.kind() == kind)
        .unwrap_or_else(|| panic!("Tree must contain {:?}; got: {:#?}", kind, root))
}

#[test]
fn test_slice11_where_canonical_ru() {
    let root = assert_clean("ВЫБРАТЬ Наименование ИЗ Справочник.Контрагенты ГДЕ Активен = ИСТИНА");
    find_kind(&root, SyntaxKind::SDBL_WHERE_CLAUSE);
}

#[test]
fn test_slice11_where_canonical_en() {
    let root = assert_clean("SELECT Name FROM Catalog.Counterparties WHERE Active = TRUE");
    find_kind(&root, SyntaxKind::SDBL_WHERE_CLAUSE);
}

#[test]
fn test_slice11_where_logical_or_expr_direct_child() {
    let root = assert_clean("ВЫБРАТЬ * ИЗ Т ГДЕ A = 1 ИЛИ B = 2");
    let where_clause = find_kind(&root, SyntaxKind::SDBL_WHERE_CLAUSE);
    let direct_kinds: Vec<_> = where_clause.children().map(|c| c.kind()).collect();
    assert!(
        direct_kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "SdblWhereClause must have SdblLogicalOrExpr as a direct \
         child (Slice 10a wrapping contract). Got: {:?}",
        direct_kinds,
    );
}

#[test]
fn test_slice11_where_kw_or_subquery_isolated() {
    use syntax::NodeOrToken;
    fn count_kw_or_excluding_subqueries(node: &syntax::SyntaxNode) -> usize {
        let mut total = 0usize;
        for child in node.children_with_tokens() {
            match child {
                NodeOrToken::Token(t) if t.kind() == SyntaxKind::KW_OR => {
                    total += 1;
                }
                NodeOrToken::Node(n)
                    if !matches!(
                        n.kind(),
                        SyntaxKind::SDBL_SUBQUERY
                            | SyntaxKind::SDBL_SUBQUERY_EXPR
                            | SyntaxKind::SDBL_SELECT_QUERY,
                    ) =>
                {
                    total += count_kw_or_excluding_subqueries(&n);
                }
                _ => {}
            }
        }
        total
    }

    let root = assert_clean("ВЫБРАТЬ * ИЗ Т ГДЕ A В (ВЫБРАТЬ X ИЗ С ГДЕ X = 1 ИЛИ X = 2)");
    let where_clauses: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE).collect();
    assert_eq!(where_clauses.len(), 2);

    let mut sorted = where_clauses.clone();
    sorted.sort_by_key(|w| {
        w.ancestors()
            .filter(|a| {
                matches!(
                    a.kind(),
                    SyntaxKind::SDBL_SUBQUERY
                        | SyntaxKind::SDBL_SUBQUERY_EXPR
                        | SyntaxKind::SDBL_SELECT_QUERY,
                )
            })
            .count()
    });
    let outer = &sorted[0];
    let inner = &sorted[1];

    assert_eq!(
        count_kw_or_excluding_subqueries(outer),
        0,
        "Outer SdblWhereClause walk must skip the subquery → zero KW_OR",
    );
    assert_eq!(
        count_kw_or_excluding_subqueries(inner),
        1,
        "Inner subquery's SdblWhereClause walk must find one KW_OR",
    );
}

#[test]
fn test_slice11_group_by_canonical_ru() {
    let root =
        assert_clean("ВЫБРАТЬ Товар, СУММА(Количество) ИЗ ПродажиТовары СГРУППИРОВАТЬ ПО Товар");
    find_kind(&root, SyntaxKind::SDBL_GROUP_CLAUSE);
}

#[test]
fn test_slice11_group_by_canonical_en() {
    let root = assert_clean(
        "SELECT Customer, Product, SUM(Quantity) FROM Sales GROUP BY Customer, Product",
    );
    find_kind(&root, SyntaxKind::SDBL_GROUP_CLAUSE);
}

#[test]
fn test_slice11_group_missing_by_recovery() {
    let parse = parse_sdbl("ВЫБРАТЬ A ИЗ Т СГРУППИРОВАТЬ A");
    let root = parse.syntax_node();
    let group = find_kind(&root, SyntaxKind::SDBL_GROUP_CLAUSE);
    assert_eq!(
        group.children().count(),
        0,
        "Bare-keyword shape on missing-BY: zero direct child nodes",
    );
}

#[test]
fn test_slice11_having_canonical_ru() {
    let root = assert_clean(
        "ВЫБРАТЬ Товар, СУММА(Количество) ИЗ ПродажиТовары СГРУППИРОВАТЬ ПО Товар ИМЕЮЩИЕ СУММА(Количество) > 100",
    );
    find_kind(&root, SyntaxKind::SDBL_HAVING_CLAUSE);
}

#[test]
fn test_slice11_having_canonical_en() {
    let root = assert_clean(
        "SELECT Customer, SUM(Amount) FROM Sales GROUP BY Customer HAVING SUM(Amount) > 1000",
    );
    find_kind(&root, SyntaxKind::SDBL_HAVING_CLAUSE);
}

#[test]
fn test_slice11_having_logical_expression_wrapping() {
    let root = assert_clean("ВЫБРАТЬ A ИЗ Т СГРУППИРОВАТЬ ПО A ИМЕЮЩИЕ A > 0");
    let having = find_kind(&root, SyntaxKind::SDBL_HAVING_CLAUSE);
    let direct_kinds: Vec<_> = having.children().map(|c| c.kind()).collect();
    assert!(
        direct_kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "HAVING calls expression(p) but the result wraps in \
         SdblLogicalOrExpr per Slice 10a contract. Got direct: {:?}",
        direct_kinds,
    );
}

#[test]
fn test_slice11_order_by_canonical_ru() {
    let root =
        assert_clean("ВЫБРАТЬ Период, Цена ИЗ ЦеныТоваров УПОРЯДОЧИТЬ ПО Период ВОЗР, Цена УБЫВ");
    find_kind(&root, SyntaxKind::SDBL_ORDER_CLAUSE);
}

#[test]
fn test_slice11_order_by_canonical_en() {
    let root =
        assert_clean("SELECT Period, Price FROM ProductPrices ORDER BY Period ASC, Price DESC");
    find_kind(&root, SyntaxKind::SDBL_ORDER_CLAUSE);
}

#[test]
fn test_slice11_order_by_flat_children() {
    let root = assert_clean("ВЫБРАТЬ A, B ИЗ Т УПОРЯДОЧИТЬ ПО A ВОЗР, B УБЫВ");
    let order = find_kind(&root, SyntaxKind::SDBL_ORDER_CLAUSE);
    let direct_expr_count = order
        .children()
        .filter(|c| {
            matches!(
                c.kind(),
                SyntaxKind::SDBL_LOGICAL_OR_EXPR
                    | SyntaxKind::SDBL_COLUMN_REF
                    | SyntaxKind::SDBL_FUNCTION_CALL,
            )
        })
        .count();
    assert_eq!(direct_expr_count, 2, "Two flat expression children");
}

#[test]
fn test_slice11_order_missing_by_recovery() {
    let parse = parse_sdbl("ВЫБРАТЬ A ИЗ Т УПОРЯДОЧИТЬ A");
    let root = parse.syntax_node();
    let order = find_kind(&root, SyntaxKind::SDBL_ORDER_CLAUSE);
    assert_eq!(order.children().count(), 0);
}

#[test]
fn test_slice11_order_by_hierarchy_canonical_ru() {
    let root = assert_clean(
        "ВЫБРАТЬ Наименование ИЗ Справочник.Товары УПОРЯДОЧИТЬ ПО Наименование ИЕРАРХИЯ",
    );
    let order = find_kind(&root, SyntaxKind::SDBL_ORDER_CLAUSE);

    let has_hierarchy_token = order.children_with_tokens().any(|c| {
        c.as_token().is_some_and(|t| {
            let s = t.text().to_uppercase();
            s == "HIERARCHY" || s == "ИЕРАРХИЯ"
        })
    });
    assert!(
        has_hierarchy_token,
        "ИЕРАРХИЯ must be consumed inside SdblOrderClause as a flat \
         sibling token (per ITS chapter 27 mandatory fix)",
    );
}

#[test]
fn test_slice11_autoorder_canonical_ru() {
    let root = assert_clean("ВЫБРАТЬ A ИЗ Т АВТОУПОРЯДОЧИВАНИЕ");
    find_kind(&root, SyntaxKind::SDBL_AUTOORDER);
}

#[test]
fn test_slice11_autoorder_canonical_en() {
    let root = assert_clean("SELECT A FROM T AUTOORDER");
    find_kind(&root, SyntaxKind::SDBL_AUTOORDER);
}

#[test]
fn test_slice11_totals_canonical_ru() {
    let root =
        assert_clean("ВЫБРАТЬ Товар, Количество ИЗ ПродажиТовары ИТОГИ СУММА(Количество) ПО Товар");
    find_kind(&root, SyntaxKind::SDBL_TOTALS_BY);
}

#[test]
fn test_slice11_totals_canonical_en() {
    let root = assert_clean("SELECT Product, Quantity FROM Sales TOTALS SUM(Quantity) BY Product");
    find_kind(&root, SyntaxKind::SDBL_TOTALS_BY);
}

#[test]
fn test_slice11_totals_overall_fallthrough_ru() {
    let root = assert_clean("ВЫБРАТЬ СУММА(A) ИЗ Т ИТОГИ ПО ОБЩИЕ");
    let totals = find_kind(&root, SyntaxKind::SDBL_TOTALS_BY);
    let has_expr_child = totals.children().any(|c| {
        matches!(
            c.kind(),
            SyntaxKind::SDBL_COLUMN_REF
                | SyntaxKind::SDBL_LOGICAL_OR_EXPR
                | SyntaxKind::SDBL_FUNCTION_CALL,
        )
    });
    assert!(
        has_expr_child,
        "OVERALL must fall through is_expression_start → \
         SdblColumnRef direct child",
    );
}

#[test]
fn test_slice11_totals_only_hierarchy_consumed_ru() {
    let root = assert_clean("ВЫБРАТЬ Группа ИЗ Т ИТОГИ ПО Группа ТОЛЬКО ИЕРАРХИЯ");
    let totals = find_kind(&root, SyntaxKind::SDBL_TOTALS_BY);
    let token_texts: Vec<_> = totals
        .children_with_tokens()
        .filter_map(|it| it.into_token())
        .map(|t| t.text().to_string())
        .collect();

    assert!(token_texts.iter().any(|text| text == "ТОЛЬКО"));
    assert!(token_texts.iter().any(|text| text == "ИЕРАРХИЯ"));
}

#[test]
fn test_slice11_totals_missing_by_recovery() {
    let parse = parse_sdbl("ВЫБРАТЬ A ИЗ Т ИТОГИ A");
    let root = parse.syntax_node();
    let totals = find_kind(&root, SyntaxKind::SDBL_TOTALS_BY);
    let direct_expr_count = totals
        .children()
        .filter(|c| {
            matches!(
                c.kind(),
                SyntaxKind::SDBL_LOGICAL_OR_EXPR
                    | SyntaxKind::SDBL_COLUMN_REF
                    | SyntaxKind::SDBL_FUNCTION_CALL,
            )
        })
        .count();
    assert!(
        direct_expr_count >= 1,
        "Pre-BY loop must consume `A` BEFORE the missing-BY check; \
         expected at least one expression direct child",
    );
}

#[test]
fn test_slice11_for_update_canonical_ru() {
    let root =
        assert_clean("ВЫБРАТЬ A ИЗ Справочник.Контрагенты ДЛЯ ИЗМЕНЕНИЯ Справочник.Контрагенты");
    find_kind(&root, SyntaxKind::SDBL_FOR_UPDATE);
}

#[test]
fn test_slice11_for_update_canonical_en() {
    let root = assert_clean("SELECT A FROM Catalog.X FOR UPDATE Catalog.X");
    find_kind(&root, SyntaxKind::SDBL_FOR_UPDATE);
}

#[test]
fn test_slice11_for_update_deep_mdo_chain() {
    let root = assert_clean("ВЫБРАТЬ A ИЗ Т ДЛЯ ИЗМЕНЕНИЯ Справочник.X.Y.Z");
    let for_update = find_kind(&root, SyntaxKind::SDBL_FOR_UPDATE);
    let dot_count = for_update
        .children_with_tokens()
        .filter(|c| c.as_token().is_some_and(|t| t.text() == "."))
        .count();
    assert_eq!(dot_count, 3, "Greedy MDO chain flattens `Справочник.X.Y.Z` → 3 Dot tokens",);
}

#[test]
fn test_slice11_index_by_canonical_ru() {
    let root = assert_clean("ВЫБРАТЬ Имя, Цена ИЗ Товары ИНДЕКСИРОВАТЬ ПО Имя, Цена");
    find_kind(&root, SyntaxKind::SDBL_INDEX_BY);
}

#[test]
fn test_slice11_index_by_canonical_en() {
    let root = assert_clean("SELECT Name, Price FROM Products INDEX BY Name, Price");
    find_kind(&root, SyntaxKind::SDBL_INDEX_BY);
}

#[test]
fn test_slice11_tail_any_order_autoorder_after_totals() {
    let root = assert_clean("ВЫБРАТЬ A ИЗ Т ИТОГИ ПО A АВТОУПОРЯДОЧИВАНИЕ");
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_TOTALS_BY));
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_AUTOORDER));
}

#[test]
fn test_slice11_body_order_by_vs_tail_order_by() {
    let root = assert_clean("ВЫБРАТЬ A ИЗ Т УПОРЯДОЧИТЬ ПО A");
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_ORDER_CLAUSE));

    let root2 = assert_clean("ВЫБРАТЬ A ИЗ Т1 ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ B ИЗ Т2 УПОРЯДОЧИТЬ ПО A");
    assert_eq!(
        root2.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_ORDER_CLAUSE).count(),
        1,
        "UNION-tail ORDER BY produces exactly one SdblOrderClause",
    );
}

#[test]
fn test_slice11_tail_clauses_skip_trivia() {
    let root = assert_clean("ВЫБРАТЬ A ИЗ Т   АВТОУПОРЯДОЧИВАНИЕ\n\nУПОРЯДОЧИТЬ ПО A");
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_AUTOORDER));
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_ORDER_CLAUSE));
}

#[test]
fn test_slice11_is_clause_keyword_join_delegation() {
    let root = assert_clean("ВЫБРАТЬ A ИЗ Т1 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.X = Т2.Y");
    let join_count =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_JOIN_CLAUSE).count();
    assert_eq!(join_count, 1, "Exactly one JOIN clause must form");

    let first_data_source = find_kind(&root, SyntaxKind::SDBL_DATA_SOURCE);
    let has_alias = first_data_source.children().any(|c| c.kind() == SyntaxKind::SDBL_ALIAS);
    assert!(
        !has_alias,
        "Т1's data source must have NO alias child (ВНУТРЕННЕЕ \
         must terminate alias scan via is_join_keyword delegation)",
    );
}

#[test]
fn test_slice11_is_clause_keyword_alias_termination() {
    let root = assert_clean(
        "ВЫБРАТЬ Контрагент КАК КонтрагентАлиас ИЗ Справочник.Контрагенты ГДЕ Активен = ИСТИНА",
    );
    assert!(
        root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE),
        "ГДЕ must be recognised as a clause boundary by is_clause_keyword",
    );
}

#[test]
fn test_slice11_is_clause_keyword_for_update_mdo_break() {
    let root = assert_clean("ВЫБРАТЬ A ИЗ Т ДЛЯ ИЗМЕНЕНИЯ Справочник.X УПОРЯДОЧИТЬ ПО A");
    let for_update = find_kind(&root, SyntaxKind::SDBL_FOR_UPDATE);
    let mdo_text = for_update.text().to_string();
    assert!(
        !mdo_text.contains("УПОРЯДОЧИТЬ"),
        "FOR UPDATE MDO chain must terminate at УПОРЯДОЧИТЬ via \
         is_clause_keyword guard. Got chain text: `{}`",
        mdo_text,
    );
    assert!(
        root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_ORDER_CLAUSE),
        "ORDER BY must form as a sibling of FOR UPDATE, not nested in it",
    );
}

#[test]
fn test_slice11_x_slice7_select_field_having_predicate() {
    let root =
        assert_clean("ВЫБРАТЬ КОЛИЧЕСТВО(A) ИЗ Т СГРУППИРОВАТЬ ПО B ИМЕЮЩИЕ КОЛИЧЕСТВО(A) > 5");
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_GROUP_CLAUSE));
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_HAVING_CLAUSE));
}

#[test]
fn test_slice11_x_slice9_join_with_where_having() {
    let root = assert_clean(
        "ВЫБРАТЬ A ИЗ Т1 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.X = Т2.Y \
         ГДЕ Т1.X > 0 СГРУППИРОВАТЬ ПО Т1.X ИМЕЮЩИЕ Т1.X = 5",
    );
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_JOIN_CLAUSE));
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE));
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_GROUP_CLAUSE));
    assert!(root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_HAVING_CLAUSE));
}

#[test]
fn test_slice11_x_slice10b_predicate_in_where() {
    let root = assert_clean("ВЫБРАТЬ A ИЗ Т ГДЕ A МЕЖДУ 1 И 5");
    let where_clause = find_kind(&root, SyntaxKind::SDBL_WHERE_CLAUSE);
    assert!(
        where_clause.descendants().any(|n| n.kind() == SyntaxKind::SDBL_BETWEEN_EXPR),
        "BETWEEN predicate must lower as SdblBetweenExpr inside the \
         WHERE clause",
    );
}
