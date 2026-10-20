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

fn first_of_kind(root: &syntax::SyntaxNode, kind: SyntaxKind) -> syntax::SyntaxNode {
    root.descendants()
        .find(|n| n.kind() == kind)
        .unwrap_or_else(|| panic!("Expected at least one {kind:?} in tree:\n{root:#?}"))
}

fn first_selected_field_direct_child_kinds(input: &str) -> Vec<SyntaxKind> {
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let field = first_of_kind(&root, SyntaxKind::SDBL_SELECTED_FIELD);
    field.children().map(|n| n.kind()).collect()
}

#[test]
fn test_slice10b_comparison_eq() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ А = 1");
    assert!(
        count_kind(&root, SyntaxKind::SDBL_COMPARISON_EXPR) >= 1,
        "= must emit SdblComparisonExpr"
    );
}

#[test]
fn test_slice10b_comparison_neq() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ А <> 1");
    assert!(count_kind(&root, SyntaxKind::SDBL_COMPARISON_EXPR) >= 1);
}

#[test]
fn test_slice10b_comparison_lt() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ А < 1");
    assert!(count_kind(&root, SyntaxKind::SDBL_COMPARISON_EXPR) >= 1);
}

#[test]
fn test_slice10b_comparison_le() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ А <= 1");
    assert!(count_kind(&root, SyntaxKind::SDBL_COMPARISON_EXPR) >= 1);
}

#[test]
fn test_slice10b_comparison_gt() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ А > 1");
    assert!(count_kind(&root, SyntaxKind::SDBL_COMPARISON_EXPR) >= 1);
}

#[test]
fn test_slice10b_comparison_ge() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ А >= 1");
    assert!(count_kind(&root, SyntaxKind::SDBL_COMPARISON_EXPR) >= 1);
}

#[test]
fn test_slice10b_in_value_list_two_elements() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Поле В (1, 2)");
    assert!(count_kind(&root, SyntaxKind::SDBL_IN_EXPR) >= 1);
}

#[test]
fn test_slice10b_in_empty_list_recoverable() {
    let parse = parse_sdbl("ВЫБРАТЬ * ИЗ Т ГДЕ Поле В ()");
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_IN_EXPR"),
        "Empty IN () must still emit SdblInExpr (recoverable). Tree: {}",
        tree
    );
}

#[test]
fn test_slice10b_not_in_with_subquery() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Поле НЕ В (ВЫБРАТЬ Х ИЗ С)");
    let in_expr = first_of_kind(&root, SyntaxKind::SDBL_IN_EXPR);
    let in_text = in_expr.text().to_string().to_uppercase();
    assert!(in_text.contains("НЕ"));
    assert!(in_text.contains("ВЫБРАТЬ"));
    assert!(
        count_kind(&root, SyntaxKind::SDBL_SUBQUERY) >= 1,
        "IN with subquery must produce SdblSubquery"
    );
}

#[test]
fn test_slice10b_in_hierarchy_canonical_russian() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Товары.Ссылка В ИЕРАРХИИ (&Корень)");
    assert!(
        count_kind(&root, SyntaxKind::SDBL_IN_HIERARCHY_EXPR) >= 1,
        "В ИЕРАРХИИ must emit SdblInHierarchyExpr"
    );
}

#[test]
fn test_slice10b_in_hierarchy_english() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Field IN HIERARCHY (&Root)");
    assert!(count_kind(&root, SyntaxKind::SDBL_IN_HIERARCHY_EXPR) >= 1);
}

#[test]
fn test_slice10b_in_hierarchy_with_subquery() {
    let root = parse_no_errors(
        "ВЫБРАТЬ * ИЗ Т ГДЕ Поле В ИЕРАРХИИ (ВЫБРАТЬ Ссылка ИЗ Справочник.Подразделения)",
    );
    assert!(
        count_kind(&root, SyntaxKind::SDBL_IN_HIERARCHY_EXPR) >= 1,
        "В ИЕРАРХИИ with subquery must emit SdblInHierarchyExpr"
    );
    assert!(
        count_kind(&root, SyntaxKind::SDBL_SUBQUERY) >= 1,
        "В ИЕРАРХИИ with subquery must produce SdblSubquery"
    );
}

#[test]
fn test_slice10b_is_null_russian_canonical() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Поле ЕСТЬ NULL");
    assert!(count_kind(&root, SyntaxKind::SDBL_IS_NULL_EXPR) >= 1);
}

#[test]
fn test_slice10b_is_null_english() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Поле IS NULL");
    assert!(count_kind(&root, SyntaxKind::SDBL_IS_NULL_EXPR) >= 1);
}

#[test]
fn test_slice10b_is_not_null_english() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Поле IS NOT NULL");
    let is_null = first_of_kind(&root, SyntaxKind::SDBL_IS_NULL_EXPR);
    assert!(
        is_null.text().to_string().to_uppercase().contains("NOT"),
        "IS NOT NULL must carry NOT inside the SdblIsNullExpr text"
    );
}

#[test]
fn test_slice10b_between_canonical_russian_dates() {
    let root = parse_no_errors(
        "ВЫБРАТЬ * ИЗ Т ГДЕ Дата МЕЖДУ ДАТАВРЕМЯ(2012, 10, 01) И ДАТАВРЕМЯ(2012, 10, 31)",
    );
    assert!(count_kind(&root, SyntaxKind::SDBL_BETWEEN_EXPR) >= 1);
}

#[test]
fn test_slice10b_between_integer_bounds() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Поле МЕЖДУ 1 И 5");
    assert!(count_kind(&root, SyntaxKind::SDBL_BETWEEN_EXPR) >= 1);
}

#[test]
fn test_slice10b_between_missing_and_recovery() {
    let parse = parse_sdbl("ВЫБРАТЬ * ИЗ Т ГДЕ Поле МЕЖДУ 1");
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_BETWEEN_EXPR"),
        "BETWEEN without AND must still emit SdblBetweenExpr (recovery). Tree: {}",
        tree
    );
}

#[test]
fn test_slice10b_like_canonical_russian() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Наименование ПОДОБНО \"%Иван%\"");
    assert!(count_kind(&root, SyntaxKind::SDBL_LIKE_EXPR) >= 1);
}

#[test]
fn test_slice10b_like_with_escape_local_allowance() {
    let parse = parse_sdbl("ВЫБРАТЬ * ИЗ Т ГДЕ Поле ПОДОБНО \"abc%\" СПЕЦСИМВОЛ \"!\"");
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_LIKE_EXPR"),
        "ПОДОБНО ... СПЕЦСИМВОЛ must emit SdblLikeExpr.\nTree: {}",
        tree
    );
}

#[test]
fn test_slice10b_refs_canonical_russian() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Регистратор ССЫЛКА Документ.ПриходнаяНакладная");
    assert!(count_kind(&root, SyntaxKind::SDBL_REFS_EXPR) >= 1);
}

#[test]
fn test_slice10b_refs_deep_mdo_chain() {
    let root = parse_no_errors("SELECT * FROM T WHERE Field REFS Catalog.Products.Item");
    let refs = first_of_kind(&root, SyntaxKind::SDBL_REFS_EXPR);
    let text = refs.text().to_string();
    assert!(text.contains("Catalog"));
    assert!(text.contains("Products"));
    assert!(text.contains("Item"));
}

#[test]
fn test_slice10b_cast_number_two_args() {
    let root = parse_no_errors("ВЫБРАТЬ ВЫРАЗИТЬ(Поле КАК ЧИСЛО(8, 2)) ИЗ Т");
    assert!(count_kind(&root, SyntaxKind::SDBL_TYPE) >= 1);
    assert!(count_kind(&root, SyntaxKind::SDBL_FUNCTION_CALL) >= 1);
}

#[test]
fn test_slice10b_cast_string_with_length() {
    let root = parse_no_errors("ВЫБРАТЬ ВЫРАЗИТЬ(Поле КАК СТРОКА(200)) ИЗ Т");
    assert!(count_kind(&root, SyntaxKind::SDBL_TYPE) >= 1);
}

#[test]
fn test_slice10b_cast_date_no_params() {
    let root = parse_no_errors("ВЫБРАТЬ ВЫРАЗИТЬ(Поле КАК ДАТА) ИЗ Т");
    assert!(count_kind(&root, SyntaxKind::SDBL_TYPE) >= 1);
}

#[test]
fn test_slice10b_cast_mdo_with_member_access_canonical() {
    let root = parse_no_errors(
        "ВЫБРАТЬ ВЫРАЗИТЬ(Регистратор КАК Документ.ПриходнаяНакладная).Поставщик ИЗ Т",
    );
    let func_call = first_of_kind(&root, SyntaxKind::SDBL_FUNCTION_CALL);
    let func_text = func_call.text().to_string();
    assert!(
        func_text.contains("Поставщик"),
        "Member access on CAST result must be preserved as a child of SdblFunctionCall. Got: {}",
        func_text
    );
    assert!(count_kind(&root, SyntaxKind::SDBL_TYPE) >= 1);
}

#[test]
fn test_slice10b_cast_mdo_simple() {
    let root = parse_no_errors("ВЫБРАТЬ ВЫРАЗИТЬ(Поле КАК Справочник.Товары) ИЗ Т");
    assert!(count_kind(&root, SyntaxKind::SDBL_TYPE) >= 1);
}

#[test]
fn test_slice10b_case_searched_canonical_russian() {
    let root = parse_no_errors(
        "ВЫБРАТЬ ВЫБОР КОГДА Товары.ЭтоГруппа = ИСТИНА ТОГДА \"Это группа\" ИНАЧЕ \"Это элемент\" КОНЕЦ КАК ПризнакГруппы ИЗ Т",
    );
    let case = first_of_kind(&root, SyntaxKind::SDBL_CASE_EXPR);
    let first_child_kind = case.children().next().map(|n| n.kind());
    assert_eq!(
        first_child_kind,
        Some(SyntaxKind::SDBL_WHEN_CLAUSE),
        "Searched CASE first child node must be SdblWhenClause (no operand). Got: {:?}",
        first_child_kind
    );
}

#[test]
fn test_slice10b_case_simple_form_operand_first() {
    let root = parse_no_errors("ВЫБРАТЬ ВЫБОР Поле КОГДА 1 ТОГДА \"А\" ИНАЧЕ \"Б\" КОНЕЦ ИЗ Т");
    let case = first_of_kind(&root, SyntaxKind::SDBL_CASE_EXPR);
    let first_child_kind = case.children().next().map(|n| n.kind());
    assert_ne!(
        first_child_kind,
        Some(SyntaxKind::SDBL_WHEN_CLAUSE),
        "Simple CASE first child node must be the operand, not SdblWhenClause. Got: {:?}",
        first_child_kind
    );
}

#[test]
fn test_slice10b_case_multiple_when_clauses() {
    let root =
        parse_no_errors("ВЫБРАТЬ ВЫБОР КОГДА А = 1 ТОГДА \"X\" КОГДА А = 2 ТОГДА \"Y\" КОНЕЦ ИЗ Т");
    assert!(
        count_kind(&root, SyntaxKind::SDBL_WHEN_CLAUSE) >= 2,
        "CASE with two WHEN clauses must emit two SdblWhenClause nodes"
    );
}

#[test]
fn test_slice10b_count_asterisk() {
    let root = parse_no_errors("ВЫБРАТЬ КОЛИЧЕСТВО(*) ИЗ Т");
    assert!(count_kind(&root, SyntaxKind::SDBL_FUNCTION_CALL) >= 1);
}

#[test]
fn test_slice10b_dot_chain_column_ref() {
    let root = parse_no_errors("ВЫБРАТЬ Т.Х.Y ИЗ Т");
    let column_ref = first_of_kind(&root, SyntaxKind::SDBL_COLUMN_REF);
    let text = column_ref.text().to_string();
    assert!(text.contains("Т") && text.contains("Х") && text.contains("Y"));
}

#[test]
fn test_slice10b_inline_tabular_fields() {
    let root = parse_no_errors("ВЫБРАТЬ Т.ТабЧасть.(Поле1, Поле2) ИЗ Т");
    assert!(count_kind(&root, SyntaxKind::SDBL_INLINE_TABLE_FIELDS) >= 1);
    let inline = first_of_kind(&root, SyntaxKind::SDBL_INLINE_TABLE_FIELDS);
    let inner_field_count =
        inline.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_SELECTED_FIELD).count();
    assert!(
        inner_field_count >= 2,
        "SdblInlineTableFields must contain at least two SdblSelectedField descendants (one per field)"
    );
}

#[test]
fn test_slice10b_distinct_aggregate_prefix_canonical() {
    let root = parse_no_errors("ВЫБРАТЬ КОЛИЧЕСТВО(РАЗЛИЧНЫЕ ЗаказТовара.Клиент) ИЗ Т");
    assert!(count_kind(&root, SyntaxKind::SDBL_FUNCTION_CALL) >= 1);
    let func = first_of_kind(&root, SyntaxKind::SDBL_FUNCTION_CALL);
    assert!(
        func.text().to_string().to_uppercase().contains("РАЗЛИЧНЫЕ"),
        "Aggregate function with DISTINCT prefix must keep РАЗЛИЧНЫЕ in its text"
    );
}

#[test]
fn test_slice10b_func_call_clause_keyword_recovery_en() {
    let input = "SELECT func(x, FROM T)";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let from_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert!(
        from_clauses >= 1,
        "Outer SELECT must keep its FROM clause despite the unbalanced func call.\nTree: {:#?}",
        root
    );

    let func_call = first_of_kind(&root, SyntaxKind::SDBL_FUNCTION_CALL);
    let func_text = func_call.text().to_string();
    assert!(
        !func_text.to_uppercase().contains("FROM"),
        "Function call must NOT consume FROM as an argument: got `{}`",
        func_text
    );
}

#[test]
fn test_slice10b_func_call_clause_keyword_recovery_ru() {
    let input = "ВЫБРАТЬ функ(х, ИЗ Т)";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let from_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert!(
        from_clauses >= 1,
        "Outer ВЫБРАТЬ must keep its ИЗ clause despite the unbalanced func call.\nTree: {:#?}",
        root
    );

    let func_call = first_of_kind(&root, SyntaxKind::SDBL_FUNCTION_CALL);
    let func_text = func_call.text().to_string();
    assert!(
        !func_text.to_uppercase().contains("ИЗ"),
        "Function call must NOT consume ИЗ as an argument: got `{}`",
        func_text
    );
}

#[test]
fn test_slice10b_select_field_comparison_descendant_guard() {
    let kinds = first_selected_field_direct_child_kinds("ВЫБРАТЬ Поле = 1 ИЗ Т");
    assert!(
        kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "SelectedField must have SdblLogicalOrExpr as direct child. Got: {:?}",
        kinds
    );
    assert!(
        !kinds.contains(&SyntaxKind::SDBL_COMPARISON_EXPR),
        "SelectedField must NOT have bare SdblComparisonExpr. Got: {:?}",
        kinds
    );
}

#[test]
fn test_slice10b_select_field_in_descendant_guard() {
    let kinds = first_selected_field_direct_child_kinds("ВЫБРАТЬ Поле В (1, 2) ИЗ Т");
    assert!(kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR));
    assert!(!kinds.contains(&SyntaxKind::SDBL_IN_EXPR));
}

#[test]
fn test_slice10b_select_field_between_descendant_guard() {
    let kinds = first_selected_field_direct_child_kinds("ВЫБРАТЬ Поле МЕЖДУ 1 И 5 ИЗ Т");
    assert!(kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR));
    assert!(!kinds.contains(&SyntaxKind::SDBL_BETWEEN_EXPR));
}

#[test]
fn test_slice10b_select_field_is_null_descendant_guard() {
    let kinds = first_selected_field_direct_child_kinds("ВЫБРАТЬ Поле ЕСТЬ NULL ИЗ Т");
    assert!(kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR));
    assert!(!kinds.contains(&SyntaxKind::SDBL_IS_NULL_EXPR));
}

#[test]
fn test_slice10b_select_field_case_descendant_guard() {
    let kinds =
        first_selected_field_direct_child_kinds("ВЫБРАТЬ ВЫБОР КОГДА 1 = 1 ТОГДА \"А\" КОНЕЦ ИЗ Т");
    assert!(kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR));
    assert!(!kinds.contains(&SyntaxKind::SDBL_CASE_EXPR));
}

#[test]
fn test_slice10b_not_between_captures_kwnot() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Поле НЕ МЕЖДУ 1 И 5");
    let between = first_of_kind(&root, SyntaxKind::SDBL_BETWEEN_EXPR);
    let between_text = between.text().to_string().to_uppercase();
    assert!(
        between_text.contains("НЕ"),
        "NOT BETWEEN must keep НЕ inside SdblBetweenExpr; got `{}`",
        between_text
    );
    assert!(between_text.contains("МЕЖДУ"));
}

#[test]
fn test_slice10b_not_like_captures_kwnot() {
    let root = parse_no_errors("ВЫБРАТЬ * ИЗ Т ГДЕ Поле НЕ ПОДОБНО \"X%\"");
    let like = first_of_kind(&root, SyntaxKind::SDBL_LIKE_EXPR);
    let like_text = like.text().to_string().to_uppercase();
    assert!(
        like_text.contains("НЕ"),
        "NOT LIKE must keep НЕ inside SdblLikeExpr; got `{}`",
        like_text
    );
    assert!(like_text.contains("ПОДОБНО"));
}

#[test]
fn test_slice10b_orphan_not_no_predicate_wrapper() {
    let parse = parse_sdbl("ВЫБРАТЬ * ИЗ Т ГДЕ 1 НЕ 2");
    let root = parse.syntax_node();
    let predicate_wrapper_count = root
        .descendants()
        .filter(|n| {
            matches!(
                n.kind(),
                SyntaxKind::SDBL_IN_EXPR
                    | SyntaxKind::SDBL_IN_HIERARCHY_EXPR
                    | SyntaxKind::SDBL_IS_NULL_EXPR
                    | SyntaxKind::SDBL_BETWEEN_EXPR
                    | SyntaxKind::SDBL_LIKE_EXPR
                    | SyntaxKind::SDBL_REFS_EXPR
                    | SyntaxKind::SDBL_COMPARISON_EXPR
            )
        })
        .count();
    assert_eq!(
        predicate_wrapper_count, 0,
        "Orphan-NOT input `1 НЕ 2` must NOT emit any predicate/comparison wrapper (mini-spec §IDE-recovery allowances #14). Tree:\n{:#?}",
        root
    );

    let has_ne_token = root
        .descendants_with_tokens()
        .filter_map(|c| c.into_token())
        .any(|t| t.text().eq_ignore_ascii_case("НЕ"));
    assert!(
        has_ne_token,
        "Orphan НЕ must remain as a stray token in the syntax tree.\nTree:\n{:#?}",
        root
    );
}
