use parser::parse_sdbl;
use syntax::SyntaxKind;

fn parse_no_errors(input: &str) -> syntax::SyntaxNode {
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "Expected clean parse for {input:?}, got errors: {:?}",
        parse.errors()
    );
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");
    root
}

fn count_kind(root: &syntax::SyntaxNode, kind: SyntaxKind) -> usize {
    root.descendants().filter(|n| n.kind() == kind).count()
}

fn first_with_token(
    root: &syntax::SyntaxNode,
    wrapper_kind: SyntaxKind,
    direct_token: SyntaxKind,
) -> syntax::SyntaxNode {
    root.descendants()
        .filter(|n| n.kind() == wrapper_kind)
        .find(|n| {
            n.children_with_tokens()
                .filter_map(|c| c.into_token())
                .any(|t| t.kind() == direct_token)
        })
        .unwrap_or_else(|| {
            panic!(
                "Expected {wrapper_kind:?} with a direct {direct_token:?} token child in tree:\n{root:#?}"
            )
        })
}

#[test]
fn test_slice10a_expression_entry_in_select_field() {
    let root = parse_no_errors("ВЫБРАТЬ А ИЛИ Б ИЗ Т");
    let or_count = count_kind(&root, SyntaxKind::SDBL_LOGICAL_OR_EXPR);
    assert!(
        or_count >= 1,
        "SELECT-field `expression` entry must produce at least one SdblLogicalOrExpr",
    );
}

#[test]
fn test_slice10a_logical_expression_entry_in_where() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ А ИЛИ Б");
    let or_count = count_kind(&root, SyntaxKind::SDBL_LOGICAL_OR_EXPR);
    assert!(
        or_count >= 1,
        "WHERE `logical_expression` entry must produce at least one SdblLogicalOrExpr",
    );
}

#[test]
fn test_slice10a_precedence_not_binds_tighter_than_and() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ НЕ А И Б");
    let and_with_kw =
        first_with_token(&root, SyntaxKind::SDBL_LOGICAL_AND_EXPR, SyntaxKind::KW_AND);
    let not_under_and = and_with_kw
        .children()
        .find(|n| n.kind() == SyntaxKind::SDBL_NOT_EXPR)
        .expect("SdblNotExpr must be a direct child of the SdblLogicalAndExpr");
    let not_text = not_under_and.text().to_string();
    assert!(
        not_text.contains('А') && !not_text.contains('Б'),
        "НЕ must wrap А alone, not the А И Б pair; got {not_text:?}",
    );
}

#[test]
fn test_slice10a_precedence_and_binds_tighter_than_or() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ А ИЛИ Б И Г");
    let or_with_kw = first_with_token(&root, SyntaxKind::SDBL_LOGICAL_OR_EXPR, SyntaxKind::KW_OR);
    let and_under_or =
        or_with_kw.children().find(|n| n.kind() == SyntaxKind::SDBL_LOGICAL_AND_EXPR);
    assert!(
        and_under_or.is_some(),
        "SdblLogicalAndExpr must sit under SdblLogicalOrExpr (AND binds tighter than OR)",
    );
}

#[test]
fn test_slice10a_precedence_mul_binds_tighter_than_add() {
    let root = parse_no_errors("ВЫБРАТЬ А + Б * Г ИЗ Т");
    let add_with_plus = first_with_token(&root, SyntaxKind::SDBL_ADDITIVE_EXPR, SyntaxKind::PLUS);
    let mul_under_add =
        add_with_plus.children().find(|n| n.kind() == SyntaxKind::SDBL_MULTIPLICATIVE_EXPR);
    assert!(
        mul_under_add.is_some(),
        "SdblMultiplicativeExpr must sit under SdblAdditiveExpr (* tighter than +)",
    );
}

#[test]
fn test_slice10a_multi_not_right_recursive() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ НЕ НЕ А");
    let outer_not = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_NOT_EXPR)
        .expect("outer SdblNotExpr");
    let inner_not = outer_not
        .children()
        .find(|n| n.kind() == SyntaxKind::SDBL_NOT_EXPR)
        .expect("inner SdblNotExpr nested directly inside outer (right-recursive)");
    assert!(
        inner_not.text().to_string().contains("НЕ"),
        "Inner SdblNotExpr text must include the second НЕ token",
    );
}

#[test]
fn test_slice10a_multi_unary_right_recursive() {
    let root = parse_no_errors("ВЫБРАТЬ - - А ИЗ Т");
    let outer_unary = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_UNARY_EXPR)
        .expect("outer SdblUnaryExpr");
    let inner_unary = outer_unary
        .children()
        .find(|n| n.kind() == SyntaxKind::SDBL_UNARY_EXPR)
        .expect("inner SdblUnaryExpr nested directly inside outer (right-recursive)");
    assert!(
        inner_unary.text().to_string().contains('-'),
        "Inner SdblUnaryExpr text must include the second `-` token",
    );
}

#[test]
fn test_slice10a_flat_additive_three_operands() {
    let root = parse_no_errors("ВЫБРАТЬ А + Б + Г ИЗ Т");
    let additives: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_ADDITIVE_EXPR).collect();
    assert_eq!(additives.len(), 1, "Exactly one SdblAdditiveExpr — wrapper is FLAT not nested");
    let plus_count = additives[0]
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .filter(|t| t.kind() == SyntaxKind::PLUS)
        .count();
    assert_eq!(
        plus_count, 2,
        "FLAT SdblAdditiveExpr for `А + Б + Г` must have exactly 2 PLUS direct token children",
    );
}

#[test]
fn test_slice10a_flat_logical_or_three_operands() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ А ИЛИ Б ИЛИ Г");
    let or_with_kw = first_with_token(&root, SyntaxKind::SDBL_LOGICAL_OR_EXPR, SyntaxKind::KW_OR);
    let or_count = or_with_kw
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .filter(|t| t.kind() == SyntaxKind::KW_OR)
        .count();
    assert_eq!(
        or_count, 2,
        "FLAT SdblLogicalOrExpr for `А ИЛИ Б ИЛИ Г` must have exactly 2 KW_OR direct token children",
    );
}

#[test]
fn test_slice10a_trivia_newlines_around_operator() {
    let root = parse_no_errors("ВЫБРАТЬ 1\n+\n2 ИЗ Т");
    let add_with_plus = first_with_token(&root, SyntaxKind::SDBL_ADDITIVE_EXPR, SyntaxKind::PLUS);
    let add_text = add_with_plus.text().to_string();
    assert!(
        add_text.contains("\n") && add_text.contains('+'),
        "SdblAdditiveExpr text must preserve trivia (newlines around `+`); got {add_text:?}",
    );
}

#[test]
fn test_slice10a_trivia_mixed_whitespace_around_operator() {
    let root = parse_no_errors("ВЫБРАТЬ 1\n\t+\t\n2 ИЗ Т");
    let add_with_plus = first_with_token(&root, SyntaxKind::SDBL_ADDITIVE_EXPR, SyntaxKind::PLUS);
    let add_text = add_with_plus.text().to_string();
    assert!(
        add_text.contains('+') && add_text.contains('\n') && add_text.contains('\t'),
        "SdblAdditiveExpr text must preserve mixed whitespace/newline/tab trivia around `+`; got {add_text:?}",
    );
}

#[test]
fn test_slice10a_atom_numeric_literal() {
    let root = parse_no_errors("ВЫБРАТЬ 222.77 ИЗ Т");
    assert!(
        count_kind(&root, SyntaxKind::SDBL_LITERAL) >= 1,
        "Numeric literal must emit SdblLiteral",
    );
}

#[test]
fn test_slice10a_atom_russian_boolean_true_false() {
    let root_true = parse_no_errors("ВЫБРАТЬ ИСТИНА ИЗ Т");
    assert!(count_kind(&root_true, SyntaxKind::SDBL_LITERAL) >= 1, "ИСТИНА must emit SdblLiteral",);
    let root_false = parse_no_errors("ВЫБРАТЬ ЛОЖЬ ИЗ Т");
    assert!(count_kind(&root_false, SyntaxKind::SDBL_LITERAL) >= 1, "ЛОЖЬ must emit SdblLiteral",);
}

#[test]
fn test_slice10a_atom_undefined_literal() {
    let root = parse_no_errors("ВЫБРАТЬ НЕОПРЕДЕЛЕНО ИЗ Т");
    assert!(count_kind(&root, SyntaxKind::SDBL_LITERAL) >= 1, "НЕОПРЕДЕЛЕНО must emit SdblLiteral",);
}

#[test]
fn test_slice10a_atom_parameter_with_identifier() {
    let root = parse_no_errors("ВЫБРАТЬ &ДатаНачала ИЗ Т");
    let param = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_PARAMETER)
        .expect("SdblParameter must exist");
    assert!(
        param.text().to_string().contains("&ДатаНачала"),
        "SdblParameter text must cover the full `&ДатаНачала` source",
    );
}

#[test]
fn test_slice10a_string_literal_wrapper_covers_user_string() {
    let root = parse_no_errors(r#"ВЫБРАТЬ "Мария" ИЗ Т"#);
    let wrapper = root
        .descendants()
        .find(|n| {
            (n.kind() == SyntaxKind::SDBL_MULTI_STRING || n.kind() == SyntaxKind::SDBL_LITERAL)
                && n.text().to_string().contains("\"Мария\"")
        })
        .expect("`\"Мария\"` must land in some SdblLiteral / SdblMultiString wrapper");
    assert!(
        wrapper.text().to_string().starts_with('"') && wrapper.text().to_string().ends_with('"'),
        "Wrapper text must cover the full user-visible string including delimiters",
    );
}

#[test]
fn test_slice10a_string_concat_via_plus() {
    let root = parse_no_errors(r#"ВЫБРАТЬ "a" + "b" ИЗ Т"#);
    let add_with_plus = first_with_token(&root, SyntaxKind::SDBL_ADDITIVE_EXPR, SyntaxKind::PLUS);
    let add_text = add_with_plus.text().to_string();
    assert!(
        add_text.contains("\"a\"") && add_text.contains("\"b\""),
        "SdblAdditiveExpr text must cover both string operands; got {add_text:?}",
    );
}

#[test]
fn test_slice10a_paren_single_expression() {
    let root = parse_no_errors("ВЫБРАТЬ (1) ИЗ Т");
    assert!(count_kind(&root, SyntaxKind::SDBL_PAREN_EXPR) >= 1, "(1) must emit SdblParenExpr",);
    assert_eq!(
        count_kind(&root, SyntaxKind::SDBL_TUPLE_EXPR),
        0,
        "(1) must NOT emit SdblTupleExpr",
    );
}

#[test]
fn test_slice10a_tuple_two_expressions() {
    let root = parse_no_errors("ВЫБРАТЬ (1, 2) ИЗ Т");
    let tuple = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TUPLE_EXPR)
        .expect("(1, 2) must emit SdblTupleExpr");
    let comma_count = tuple
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .filter(|t| t.kind() == SyntaxKind::COMMA)
        .count();
    assert_eq!(comma_count, 1, "SdblTupleExpr for (1, 2) must have 1 COMMA direct token child",);
}

#[test]
fn test_slice10a_tuple_three_expressions() {
    let root = parse_no_errors("ВЫБРАТЬ (1, 2, 3) ИЗ Т");
    let tuple = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TUPLE_EXPR)
        .expect("(1, 2, 3) must emit SdblTupleExpr");
    let comma_count = tuple
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .filter(|t| t.kind() == SyntaxKind::COMMA)
        .count();
    assert_eq!(comma_count, 2, "SdblTupleExpr for (1, 2, 3) must have 2 COMMA direct tokens");
}

#[test]
fn test_slice10a_paren_select_routes_to_subquery() {
    let root = parse_no_errors("ВЫБРАТЬ Поле ИЗ Т ГДЕ Поле = (ВЫБРАТЬ 1)");
    assert!(
        count_kind(&root, SyntaxKind::SDBL_SUBQUERY_EXPR) >= 1,
        "(ВЫБРАТЬ ...) must route to subquery branch and emit SdblSubqueryExpr",
    );
}

#[test]
fn test_slice10a_paren_parameter_routes_to_expression_branch() {
    let root = parse_no_errors("ВЫБРАТЬ (&ДатаНачала) ИЗ Т");
    assert!(
        count_kind(&root, SyntaxKind::SDBL_PAREN_EXPR) >= 1,
        "(&ДатаНачала) in expression context must emit SdblParenExpr",
    );
    assert_eq!(
        count_kind(&root, SyntaxKind::SDBL_SUBQUERY_EXPR),
        0,
        "(&ДатаНачала) in expression context must NOT route to subquery branch",
    );
    assert!(
        count_kind(&root, SyntaxKind::SDBL_PARAMETER) >= 1,
        "Inner SdblParameter must be present",
    );
}

#[test]
fn test_slice10a_bilingual_or_and_not() {
    let root_en = parse_no_errors("SELECT * FROM Т WHERE NOT А AND Б OR Г");
    assert!(
        count_kind(&root_en, SyntaxKind::SDBL_LOGICAL_OR_EXPR) >= 1
            && count_kind(&root_en, SyntaxKind::SDBL_LOGICAL_AND_EXPR) >= 1
            && count_kind(&root_en, SyntaxKind::SDBL_NOT_EXPR) >= 1,
        "English NOT/AND/OR forms must produce all three operator wrapper kinds",
    );
    let root_ru = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ НЕ А И Б ИЛИ Г");
    assert!(
        count_kind(&root_ru, SyntaxKind::SDBL_LOGICAL_OR_EXPR) >= 1
            && count_kind(&root_ru, SyntaxKind::SDBL_LOGICAL_AND_EXPR) >= 1
            && count_kind(&root_ru, SyntaxKind::SDBL_NOT_EXPR) >= 1,
        "Russian НЕ/И/ИЛИ forms must produce all three operator wrapper kinds",
    );
}

#[test]
fn test_slice10a_modulo_local_ide_allowance() {
    let root = parse_no_errors("ВЫБРАТЬ А % Б ИЗ Т");
    let mul_with_percent =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_MULTIPLICATIVE_EXPR).find(|n| {
            n.children_with_tokens()
                .filter_map(|c| c.into_token())
                .any(|t| t.kind() == SyntaxKind::PERCENT)
        });
    assert!(
        mul_with_percent.is_some(),
        "Slice 10a preserves `%` acceptance as a local IDE allowance — recoverable parse tree expected",
    );
}

#[test]
fn test_slice10a_x_slice6_subquery_with_union_in_expression() {
    let root = parse_no_errors("ВЫБРАТЬ Поле ИЗ Т ГДЕ Поле = (ВЫБРАТЬ 1 ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ 2)");
    assert!(
        count_kind(&root, SyntaxKind::SDBL_SUBQUERY_EXPR) >= 1
            && count_kind(&root, SyntaxKind::SDBL_UNION_CLAUSE) >= 1,
        "Subquery in expression position with UNION must emit both SdblSubqueryExpr and SdblUnionClause",
    );
}

#[test]
fn test_slice10a_x_slice7_field_with_arithmetic_alias() {
    let root = parse_no_errors("ВЫБРАТЬ А + Б КАК Сумма ИЗ Т");
    assert!(
        count_kind(&root, SyntaxKind::SDBL_ADDITIVE_EXPR) >= 1
            && count_kind(&root, SyntaxKind::SDBL_ALIAS) >= 1,
        "SELECT field with arithmetic + alias must emit SdblAdditiveExpr and SdblAlias",
    );
}

#[test]
fn test_slice10a_x_slice8_pure_logical_where_with_from_chain() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ ИСТИНА И ЛОЖЬ");
    assert!(
        count_kind(&root, SyntaxKind::SDBL_LOGICAL_AND_EXPR) >= 1,
        "Pure-Slice-10a WHERE content with bilingual booleans must emit SdblLogicalAndExpr",
    );
    assert!(
        count_kind(&root, SyntaxKind::SDBL_FROM_CLAUSE) >= 1,
        "Slice 8 SdblFromClause must still be present",
    );
}

#[test]
fn test_slice10a_null_inside_or_emits_literal() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Поле = ИСТИНА ИЛИ NULL");
    let null_token = root
        .descendants_with_tokens()
        .filter_map(|c| c.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT && t.text().eq_ignore_ascii_case("NULL"))
        .expect("NULL Ident token must be present");
    assert_eq!(
        null_token.parent().map(|p| p.kind()),
        Some(SyntaxKind::SDBL_LITERAL),
        "Bare NULL inside OR expression must still emit SdblLiteral (not SdblColumnRef)",
    );
}

#[test]
fn test_slice10a_recover_to_delimiter_stops_on_clause_keyword_at_any_depth_ru() {
    let input = "ВЫБРАТЬ СУММА(1 ( ИЗ T2";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let from_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert!(
        from_clauses >= 1,
        "Outer ВЫБРАТЬ must keep its ИЗ clause despite the unterminated nested `(`.\nTree: {:#?}",
        root
    );

    let bad_error = root.descendants().filter(|n| n.kind() == SyntaxKind::ERROR).any(|err| {
        err.descendants_with_tokens()
            .filter_map(|nt| nt.into_token())
            .any(|t| t.text().eq_ignore_ascii_case("ИЗ"))
    });
    assert!(
        !bad_error,
        "ИЗ clause keyword must not be consumed by recover_to_delimiter at depth>0.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice10a_recover_to_delimiter_stops_on_clause_keyword_at_any_depth_en() {
    let input = "SELECT SUM(1 ( FROM T2";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let from_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert!(
        from_clauses >= 1,
        "Outer SELECT must keep its FROM clause despite the unterminated nested `(`.\nTree: {:#?}",
        root
    );

    let bad_error = root.descendants().filter(|n| n.kind() == SyntaxKind::ERROR).any(|err| {
        err.descendants_with_tokens()
            .filter_map(|nt| nt.into_token())
            .any(|t| t.text().eq_ignore_ascii_case("FROM"))
    });
    assert!(
        !bad_error,
        "FROM clause keyword must not be consumed by recover_to_delimiter at depth>0.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice10a_recover_to_delimiter_does_not_stop_on_nested_select_at_depth_ru() {
    let input = "ВЫБРАТЬ СУММА(1 ( ВЫБРАТЬ X )) ИЗ T";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let from_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert!(
        from_clauses >= 1,
        "Outer ИЗ T must survive an unterminated nested ВЫБРАТЬ subquery body.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice10a_recover_to_delimiter_does_not_stop_on_nested_select_at_depth_en() {
    let input = "SELECT SUM(1 ( SELECT X )) FROM T";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let from_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert!(
        from_clauses >= 1,
        "Outer FROM T must survive an unterminated nested SELECT subquery body.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice10a_recover_to_delimiter_inner_from_misattribution_gate() {
    let input = "ВЫБРАТЬ СУММА(1 ( ВЫБРАТЬ X ИЗ Y )) ИЗ T";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let from_clauses: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).collect();
    let outer_from_text = from_clauses.first().map(|fc| fc.text().to_string()).unwrap_or_default();
    assert!(
        outer_from_text.contains('T') && !outer_from_text.contains('Y'),
        "Outer FROM clause must reference T, not the inner Y; got {outer_from_text:?}.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice10a_post_dot_accepts_kw_in_as_column_name() {
    parse_no_errors("ВЫБРАТЬ Т.В ИЗ Т КАК Т");
}

#[test]
fn test_slice10a_post_dot_accepts_soft_keywords_as_column_names() {
    parse_no_errors("ВЫБРАТЬ Т.А + Т.Б + Т.В ИЗ Т КАК Т");
}

#[test]
fn test_slice10a_post_dot_accepts_kw_and_or_not_as_column_names() {
    parse_no_errors("ВЫБРАТЬ Т.И, Т.Или, Т.Не ИЗ Т КАК Т");
}

#[test]
fn test_slice10a_post_dot_accepts_literal_keywords_as_column_names() {
    parse_no_errors("ВЫБРАТЬ Т.Истина, Т.Ложь, Т.Неопределено ИЗ Т КАК Т");
}

#[test]
fn test_slice10a_post_dot_accepts_soft_keywords_in_table_ref() {
    parse_no_errors("ВЫБРАТЬ * ИЗ Справочник.В");
}

#[test]
fn test_slice10a_post_dot_accepts_soft_keywords_in_for_update_clause() {
    parse_no_errors("ВЫБРАТЬ * ИЗ Т КАК Т ДЛЯ ИЗМЕНЕНИЯ Т.В");
}

#[test]
fn test_slice10a_post_dot_accepts_soft_keywords_in_refs_predicate() {
    parse_no_errors("ВЫБРАТЬ * ИЗ Т КАК Т ГДЕ Т.Поле ССЫЛКА Справочник.В");
}

#[test]
fn test_slice10a_post_dot_accepts_soft_keywords_after_cast_result() {
    parse_no_errors("ВЫБРАТЬ ВЫРАЗИТЬ(Т.Поле КАК Справочник.Контрагенты).В ИЗ Т КАК Т");
}
