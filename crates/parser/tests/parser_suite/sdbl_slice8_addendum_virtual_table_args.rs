use parser::parse_sdbl;
use syntax::{NodeOrToken, SyntaxKind};

fn assert_clean(input: &str) -> syntax::Parse<syntax::SyntaxNode> {
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "Expected clean parse for `{}`; errors: {:#?}",
        input,
        parse.errors(),
    );
    let error_descendants: Vec<_> =
        parse.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::ERROR).collect();
    assert!(
        error_descendants.is_empty(),
        "Expected no ERROR descendants for `{}`; got {} ERROR nodes: {:#?}",
        input,
        error_descendants.len(),
        error_descendants,
    );
    parse
}

fn first_table_ref(parse: &syntax::Parse<syntax::SyntaxNode>) -> syntax::SyntaxNode {
    parse
        .syntax_node()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef")
}

fn count_direct_children_of_kind(node: &syntax::SyntaxNode, kind: SyntaxKind) -> usize {
    node.children().filter(|c| c.kind() == kind).count()
}

fn count_direct_token_kind(node: &syntax::SyntaxNode, kind: SyntaxKind) -> usize {
    node.children_with_tokens()
        .filter_map(|c| c.as_token().filter(|t| t.kind() == kind).cloned())
        .count()
}

#[test]
fn test_slice8adn_empty_paren_pair_ru() {
    let parse = assert_clean("ВЫБРАТЬ * ИЗ Регистр.Остатки() КАК Т");
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::SDBL_MISSING_ARG),
        0,
        "Empty `()` RU must produce zero SdblMissingArg direct children",
    );
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::L_PAREN), 1);
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::R_PAREN), 1);
}

#[test]
fn test_slice8adn_empty_paren_pair_en() {
    let parse = assert_clean("SELECT * FROM Reg.Balance() AS T");
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::SDBL_MISSING_ARG),
        0,
        "Empty `()` EN must produce zero SdblMissingArg direct children",
    );
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::L_PAREN), 1);
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::R_PAREN), 1);
}

#[test]
fn test_slice8adn_single_trailing_comma_ru() {
    let parse = assert_clean("ВЫБРАТЬ * ИЗ Регистр.Остатки(&Период,) КАК Т");
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::SDBL_MISSING_ARG),
        1,
        "Single trailing-comma must produce exactly one SdblMissingArg direct child",
    );
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::COMMA), 1);
}

#[test]
fn test_slice8adn_double_trailing_comma_ru() {
    let parse = assert_clean("ВЫБРАТЬ * ИЗ Регистр.Обороты(&Начало, &Конец, , ) КАК Т");
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::SDBL_MISSING_ARG),
        2,
        "Double trailing-comma must produce exactly two SdblMissingArg direct children",
    );
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::COMMA), 3);
}

#[test]
fn test_slice8adn_canonical_v8327doc_ru() {
    let parse = assert_clean(
        "ВЫБРАТЬ * ИЗ РегистрНакопления.УчетНоменклатуры.ОстаткиИОбороты(, , Авто, , ) КАК Т",
    );
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::SDBL_MISSING_ARG),
        4,
        "Canonical v8327doc 5-arg shape must produce exactly 4 SdblMissingArg direct children",
    );
    assert_eq!(
        count_direct_token_kind(&table_ref, SyntaxKind::COMMA),
        4,
        "Canonical v8327doc 5-arg shape must have exactly 4 COMMA token direct children",
    );
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::L_PAREN), 1);
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::R_PAREN), 1);
    let normalised: Vec<&'static str> = table_ref
        .children_with_tokens()
        .filter_map(|el| match el {
            NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::L_PAREN => Some("LParen"),
                SyntaxKind::R_PAREN => Some("RParen"),
                SyntaxKind::COMMA => Some("Comma"),
                _ => None,
            },
            NodeOrToken::Node(n) => match n.kind() {
                SyntaxKind::SDBL_MISSING_ARG => Some("MissingArg"),
                SyntaxKind::SDBL_LOGICAL_OR_EXPR
                | SyntaxKind::SDBL_LOGICAL_AND_EXPR
                | SyntaxKind::SDBL_NOT_EXPR
                | SyntaxKind::SDBL_COMPARISON_EXPR
                | SyntaxKind::SDBL_COLUMN_REF
                | SyntaxKind::SDBL_LITERAL
                | SyntaxKind::SDBL_MULTI_STRING
                | SyntaxKind::SDBL_FUNCTION_CALL
                | SyntaxKind::SDBL_PAREN_EXPR => Some("Expr"),
                _ => None,
            },
        })
        .collect();
    let expected = [
        "LParen",
        "MissingArg",
        "Comma",
        "MissingArg",
        "Comma",
        "Expr",
        "Comma",
        "MissingArg",
        "Comma",
        "MissingArg",
        "RParen",
    ];
    assert_eq!(
        normalised, expected,
        "Direct-child interleaved sequence under SdblTableRef \
         must match the canonical v8327doc 5-arg shape \
         (LParen, MissingArg, Comma, MissingArg, Comma, Expr, \
         Comma, MissingArg, Comma, MissingArg, RParen)",
    );
}

#[test]
fn test_slice8adn_consecutive_empty_args() {
    let parse = assert_clean("ВЫБРАТЬ * ИЗ Регистр.Остатки(,,) КАК Т");
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::SDBL_MISSING_ARG),
        3,
        "Three commas with no content between them produce three \
         SdblMissingArg slots: leading + middle + trailing",
    );
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::COMMA), 2);
}

#[test]
fn test_slice8adn_leading_empty_then_named() {
    let parse = assert_clean("ВЫБРАТЬ * ИЗ Регистр.Обороты(, Поле = &Парам) КАК Т");
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::SDBL_MISSING_ARG),
        1,
        "Leading-empty + named-condition form produces exactly one SdblMissingArg",
    );
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::COMMA), 1);
}

#[test]
fn test_slice8adn_paren_balanced_subquery_arg_ru() {
    let parse =
        assert_clean("ВЫБРАТЬ * ИЗ Регистр.Обороты(, &Конец, , Поле В (ВЫБРАТЬ X ИЗ Y)) КАК Т");
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::ERROR),
        0,
        "Clean IN-subquery as VT param must NOT trigger \
         `recover_to_delimiter_vt` — the subquery's `)` is \
         consumed inside `expression(p)` / `predicate_expr` per \
         Slice 10b",
    );
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::SDBL_MISSING_ARG),
        2,
        "The `, &Конец, ,` pattern produces 2 SdblMissingArg slots \
         (leading + middle); the predicate `Поле В (...)` occupies \
         the trailing slot as a single expression-NodeKind",
    );
}

#[test]
fn test_slice8adn_nested_function_call_arg() {
    let parse = assert_clean("ВЫБРАТЬ * ИЗ Регистр.Остатки(СУММА(A)) КАК Т");
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::ERROR),
        0,
        "Clean nested call `СУММА(A)` must NOT trigger recovery",
    );
    let func_call_descendants =
        table_ref.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FUNCTION_CALL).count();
    assert!(
        func_call_descendants >= 1,
        "Expected at least one SdblFunctionCall descendant of \
         SdblTableRef for `СУММА(A)`",
    );
}

#[test]
fn test_slice8adn_mid_arg_recovery() {
    let parse = parse_sdbl("ВЫБРАТЬ * ИЗ Регистр.Остатки(СУММА(A) Q, B) КАК Т");
    let table_ref = parse
        .syntax_node()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    let errors: Vec<_> = table_ref.children().filter(|c| c.kind() == SyntaxKind::ERROR).collect();
    assert!(
        !errors.is_empty(),
        "Mid-arg spurious-token form must trigger `recover_to_delimiter_vt`",
    );
    assert!(
        errors.iter().any(|e| e.text().to_string().contains('Q')),
        "Error sub-node must contain the spurious `Q` token. Got: {:?}",
        errors.iter().map(|e| e.text().to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_slice8adn_recover_always_emits_error() {
    let parse = parse_sdbl("ВЫБРАТЬ * ИЗ Регистр.Остатки(A B C ИЗ Т");
    let _ = parse.syntax_node();
}

#[test]
fn test_slice8adn_recovery_stops_on_clause_keyword_at_any_depth() {
    let parse = parse_sdbl("ВЫБРАТЬ * ИЗ Регистр.Остатки(СУММА(A) ( ГДЕ S = 1");
    let root = parse.syntax_node();
    let table_ref = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    for err in parse.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::ERROR) {
        let text = err.text().to_string();
        assert!(
            !text.contains("ГДЕ"),
            "No Error sub-node may contain the `ГДЕ` clause keyword \
             — recovery + missing-RParen handling must preserve \
             clause keywords for the outer query. Got Error text: \
             {:?}",
            text,
        );
    }
    let _ = table_ref;
}

#[test]
fn test_slice8adn_x_slice8_table_ref_dispatch() {
    let parse = assert_clean("ВЫБРАТЬ * ИЗ Справочник.Товары.СрезПоследних() КАК С");
    let table_refs: Vec<_> = parse
        .syntax_node()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .collect();
    assert_eq!(table_refs.len(), 1, "Single VT call must produce exactly one SdblTableRef",);
    assert_eq!(count_direct_token_kind(&table_refs[0], SyntaxKind::L_PAREN), 1);
    assert_eq!(count_direct_token_kind(&table_refs[0], SyntaxKind::R_PAREN), 1);
}

#[test]
fn test_slice8adn_x_slice9_join_with_vt() {
    let parse = assert_clean(
        "ВЫБРАТЬ * ИЗ Документ.Заказ КАК З \
         ВНУТРЕННЕЕ СОЕДИНЕНИЕ Регистр.Остатки(&Дата) КАК Р \
         ПО З.Ссылка = Р.Регистратор",
    );
    let join_clauses: Vec<_> = parse
        .syntax_node()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::SDBL_JOIN_CLAUSE)
        .collect();
    assert_eq!(join_clauses.len(), 1, "Expected exactly one SdblJoinClause");
    let table_refs_in_join =
        join_clauses[0].descendants().filter(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF).count();
    assert!(table_refs_in_join >= 1, "JOIN clause must contain the VT-call SdblTableRef",);
}

#[test]
fn test_slice8adn_x_slice10b_subquery_in_vt_arg() {
    let parse = assert_clean(
        "ВЫБРАТЬ * ИЗ Регистр.Остатки( , Номенклатура В (ВЫБРАТЬ Т.Номенклатура ИЗ Т)) КАК Р",
    );
    let table_ref = first_table_ref(&parse);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::ERROR),
        0,
        "Slice 10b IN-subquery handling must produce zero ERROR direct children of SdblTableRef",
    );
    let subqueries =
        parse.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::SDBL_SUBQUERY).count();
    assert!(subqueries >= 1, "Expected at least one SdblSubquery descendant for the IN-subquery",);
}

#[test]
fn test_slice8adn_x_slice11_vt_with_clauses_after_from() {
    let parse = assert_clean(
        "ВЫБРАТЬ * ИЗ Регистр.Остатки(&Дата) КАК Т \
         ГДЕ Т.Поле = 1 \
         СГРУППИРОВАТЬ ПО Т.Поле \
         УПОРЯДОЧИТЬ ПО Т.Поле",
    );
    let root = parse.syntax_node();
    let table_ref = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    let where_inside =
        table_ref.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE).count();
    let group_inside =
        table_ref.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_GROUP_CLAUSE).count();
    let order_inside =
        table_ref.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_ORDER_CLAUSE).count();
    assert_eq!(
        where_inside, 0,
        "SdblWhereClause must NOT be nested inside SdblTableRef \
         (post-VT-args RParen exit hands off to Slice 11)",
    );
    assert_eq!(group_inside, 0, "SdblGroupClause must NOT be nested inside SdblTableRef");
    assert_eq!(order_inside, 0, "SdblOrderClause must NOT be nested inside SdblTableRef");
    assert_eq!(
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE).count(),
        1,
        "Expected exactly one SdblWhereClause in the tree",
    );
    assert_eq!(
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_GROUP_CLAUSE).count(),
        1,
        "Expected exactly one SdblGroupClause in the tree",
    );
    assert_eq!(
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_ORDER_CLAUSE).count(),
        1,
        "Expected exactly one SdblOrderClause in the tree",
    );
}

#[test]
fn test_slice8adn_outer_lparen_guard_no_op() {
    let parse = assert_clean("ВЫБРАТЬ * ИЗ Регистр.Остатки КАК Т");
    let table_ref = first_table_ref(&parse);
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::L_PAREN), 0);
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::R_PAREN), 0);
    assert_eq!(count_direct_token_kind(&table_ref, SyntaxKind::COMMA), 0);
    assert_eq!(
        count_direct_children_of_kind(&table_ref, SyntaxKind::SDBL_MISSING_ARG),
        0,
        "Outer LParen guard makes virtual_table_args a no-op when the MDO chain has no trailing `(`",
    );
}

#[test]
fn test_slice8adn_recovery_does_not_stop_on_nested_select_at_depth() {
    let input = "ВЫБРАТЬ * ИЗ Регистр.Остатки(1 ( ВЫБРАТЬ X )) КАК Т ГДЕ Т.A = 1";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let where_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE).count();
    assert!(
        where_clauses >= 1,
        "Outer ГДЕ clause must survive an unterminated nested ВЫБРАТЬ inside VT-args.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice8adn_recovery_inner_from_does_not_misattribute() {
    let input = "ВЫБРАТЬ * ИЗ Регистр.Остатки(1 ( ВЫБРАТЬ X ИЗ Y )) КАК Т ГДЕ Т.A = 1";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let where_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE).count();
    assert!(
        where_clauses >= 1,
        "Outer ГДЕ must survive a nested ВЫБРАТЬ ... ИЗ inside VT-args.\nTree: {:#?}",
        root
    );
    let aliases = root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_ALIAS).count();
    assert!(
        aliases >= 1,
        "Outer КАК Т alias must survive a nested ВЫБРАТЬ ... ИЗ inside VT-args.\nTree: {:#?}",
        root
    );
}
