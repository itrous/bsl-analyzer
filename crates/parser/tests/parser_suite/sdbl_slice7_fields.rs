use parser::parse_sdbl;
use syntax::SyntaxKind;

fn tree(input: &str) -> String {
    let parse = parse_sdbl(input);
    format!("{:#?}", parse.syntax_node())
}

fn count_nodes(tree: &str, kind: &str) -> usize {
    let needle = format!("{kind}@");
    tree.matches(&needle).count()
}

fn parse_clean(input: &str) {
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "Expected clean parse for {input:?}, got errors: {:?}",
        parse.errors()
    );
}

#[test]
fn test_single_field() {
    parse_clean("SELECT Name FROM T");
    let t = tree("SELECT Name FROM T");
    assert_eq!(count_nodes(&t, "SDBL_SELECTED_FIELD"), 1);
    assert_eq!(count_nodes(&t, "SDBL_FIELD_LIST"), 1);
}

#[test]
fn test_two_fields() {
    parse_clean("SELECT Name, Code FROM Products");
    let t = tree("SELECT Name, Code FROM Products");
    assert_eq!(count_nodes(&t, "SDBL_SELECTED_FIELD"), 2);
}

#[test]
fn test_four_fields() {
    parse_clean("SELECT A, B, C, D FROM T");
    let t = tree("SELECT A, B, C, D FROM T");
    assert_eq!(count_nodes(&t, "SDBL_SELECTED_FIELD"), 4);
}

#[test]
fn test_trailing_comma_recoverable() {
    let t = tree("SELECT Name, FROM Products");
    assert!(
        t.contains("SDBL_FROM_CLAUSE"),
        "FROM clause must parse after trailing comma. Tree: {}",
        t
    );
}

#[test]
fn test_bare_asterisk() {
    parse_clean("SELECT * FROM T");
    let t = tree("SELECT * FROM T");
    assert_eq!(count_nodes(&t, "SDBL_ASTERISK_FIELD"), 1);
}

#[test]
fn test_qualified_asterisk_english() {
    parse_clean("SELECT Products.* FROM Products");
    let t = tree("SELECT Products.* FROM Products");
    assert_eq!(count_nodes(&t, "SDBL_ASTERISK_FIELD"), 1);
}

#[test]
fn test_qualified_asterisk_russian() {
    parse_clean("ВЫБРАТЬ Товары.* ИЗ Товары");
    let t = tree("ВЫБРАТЬ Товары.* ИЗ Товары");
    assert_eq!(count_nodes(&t, "SDBL_ASTERISK_FIELD"), 1);
}

#[test]
fn test_multi_segment_asterisk_not_detected_by_predicate() {
    let t = tree("SELECT Catalog.Products.* FROM Products");
    assert_eq!(
        count_nodes(&t, "SDBL_ASTERISK_FIELD"),
        0,
        "Multi-segment Catalog.Products.* must not enter via is_asterisk_start. Tree: {}",
        t
    );
}

#[test]
fn test_temp_table_asterisk_not_parsed_as_asterisk_field() {
    let t = tree("SELECT #Temp.* FROM #Temp");
    assert_eq!(
        count_nodes(&t, "SDBL_ASTERISK_FIELD"),
        0,
        "Temp-table-prefixed #Temp.* must not be detected as an asterisk field. Tree: {}",
        t
    );
}

#[test]
fn test_asterisk_with_regular_field() {
    parse_clean("SELECT T.*, Name FROM T");
    let t = tree("SELECT T.*, Name FROM T");
    assert_eq!(count_nodes(&t, "SDBL_ASTERISK_FIELD"), 1);
    assert_eq!(count_nodes(&t, "SDBL_SELECTED_FIELD"), 2);
}

#[test]
fn test_alias_with_as() {
    parse_clean("SELECT Name AS ProductName FROM T");
    let t = tree("SELECT Name AS ProductName FROM T");
    assert_eq!(count_nodes(&t, "SDBL_ALIAS"), 1);
}

#[test]
fn test_alias_with_kak() {
    parse_clean("ВЫБРАТЬ Имя КАК Имя2 ИЗ Товары");
    let t = tree("ВЫБРАТЬ Имя КАК Имя2 ИЗ Товары");
    assert_eq!(count_nodes(&t, "SDBL_ALIAS"), 1);
}

#[test]
fn test_alias_bare_identifier() {
    parse_clean("SELECT Name ProductName FROM T");
    let t = tree("SELECT Name ProductName FROM T");
    assert_eq!(count_nodes(&t, "SDBL_ALIAS"), 1);
}

#[test]
fn test_alias_case_insensitive_as() {
    parse_clean("SELECT Name as Alias1 FROM T");
    parse_clean("SELECT Name As Alias2 FROM T");
    parse_clean("SELECT Name AS Alias3 FROM T");
}

#[test]
fn test_alias_clause_keyword_guard() {
    let t = tree("SELECT x FROM T");
    assert!(t.contains("SDBL_FROM_CLAUSE"), "FROM clause must parse. Tree: {}", t);
    assert_eq!(
        count_nodes(&t, "SDBL_ALIAS"),
        0,
        "Clause keyword FROM must not be captured as alias. Tree: {}",
        t
    );
}

#[test]
fn test_alias_as_without_name_recoverable() {
    let t = tree("SELECT x AS FROM T");
    assert!(t.contains("SDBL_ALIAS"), "Alias node expected. Tree: {}", t);
    assert!(t.contains("ERROR"), "Empty alias name expected as ERROR. Tree: {}", t);
    assert!(t.contains("SDBL_FROM_CLAUSE"), "FROM must still parse. Tree: {}", t);
}

#[test]
fn test_multi_field_mixed_aliases() {
    parse_clean("SELECT Name AS N, Code C FROM Products");
    let t = tree("SELECT Name AS N, Code C FROM Products");
    assert_eq!(count_nodes(&t, "SDBL_SELECTED_FIELD"), 2);
    assert_eq!(count_nodes(&t, "SDBL_ALIAS"), 2);
}

#[test]
fn test_into_english_simple() {
    parse_clean("SELECT Name INTO TempNames FROM T");
    let t = tree("SELECT Name INTO TempNames FROM T");
    assert_eq!(count_nodes(&t, "SDBL_INTO_CLAUSE"), 1);
    assert_eq!(count_nodes(&t, "SDBL_TEMP_TABLE_NAME"), 1);
}

#[test]
fn test_into_russian_pomestit() {
    parse_clean("ВЫБРАТЬ Имя ПОМЕСТИТЬ ВремТаблица ИЗ Товары");
    let t = tree("ВЫБРАТЬ Имя ПОМЕСТИТЬ ВремТаблица ИЗ Товары");
    assert_eq!(count_nodes(&t, "SDBL_INTO_CLAUSE"), 1);
    assert_eq!(count_nodes(&t, "SDBL_TEMP_TABLE_NAME"), 1);
}

#[test]
fn test_into_before_from_ordering() {
    parse_clean("SELECT Name INTO TempNames FROM Products");
    let t = tree("SELECT Name INTO TempNames FROM Products");
    let into_pos = t.find("SDBL_INTO_CLAUSE").expect("INTO must parse");
    let from_pos = t.find("SDBL_FROM_CLAUSE").expect("FROM must parse");
    assert!(into_pos < from_pos, "INTO must appear before FROM in the tree. Tree: {}", t);
}

#[test]
fn test_into_semicolon_recoverable() {
    let t = tree("SELECT Name INTO ;");
    assert!(t.contains("SDBL_INTO_CLAUSE"), "INTO clause still emitted. Tree: {}", t);
    assert_eq!(
        count_nodes(&t, "SDBL_TEMP_TABLE_NAME"),
        0,
        "Missing-identifier path must not emit SDBL_TEMP_TABLE_NAME. Tree: {}",
        t
    );
    assert!(t.contains("ERROR@"), "Missing-identifier path must emit an ERROR marker. Tree: {}", t);
}

#[test]
fn test_query_wrapper_minimal_shape() {
    parse_clean("SELECT 1");
    let t = tree("SELECT 1");
    assert_eq!(count_nodes(&t, "SDBL_QUERY"), 1);
    assert_eq!(count_nodes(&t, "SDBL_FIELD_LIST"), 1);
    assert_eq!(count_nodes(&t, "SDBL_SELECTED_FIELD"), 1);
}

#[test]
fn test_query_missing_select_keyword_recoverable() {
    let t = tree("FROM Products");
    assert_eq!(
        count_nodes(&t, "SDBL_QUERY"),
        1,
        "SDBL_QUERY must exist even without SELECT keyword. Tree: {}",
        t
    );
    assert!(t.contains("ERROR"), "Missing SELECT must produce an ERROR marker. Tree: {}", t);
}

#[test]
fn test_nodekind_identity_selected_field_with_alias() {
    let with_as = tree("SELECT Name AS N FROM T");
    let without_as = tree("SELECT Name N FROM T");
    for t in [&with_as, &without_as] {
        assert_eq!(count_nodes(t, "SDBL_SELECTED_FIELD"), 1, "Tree: {}", t);
        assert_eq!(count_nodes(t, "SDBL_ALIAS"), 1, "Tree: {}", t);
    }
}

#[test]
fn test_bilingual_full_prefix() {
    parse_clean("ВЫБРАТЬ Имя КАК Наименование ПОМЕСТИТЬ ВремТаблица ИЗ Товары");
    let t = tree("ВЫБРАТЬ Имя КАК Наименование ПОМЕСТИТЬ ВремТаблица ИЗ Товары");
    assert_eq!(count_nodes(&t, "SDBL_SELECTED_FIELD"), 1);
    assert_eq!(count_nodes(&t, "SDBL_ALIAS"), 1);
    assert_eq!(count_nodes(&t, "SDBL_INTO_CLAUSE"), 1);
    assert_eq!(count_nodes(&t, "SDBL_FROM_CLAUSE"), 1);
}

#[test]
fn test_into_drop_package_integration() {
    parse_clean("SELECT Name INTO TmpTable FROM T; DROP TmpTable");
    let t = tree("SELECT Name INTO TmpTable FROM T; DROP TmpTable");
    assert_eq!(count_nodes(&t, "SDBL_INTO_CLAUSE"), 1);
    assert_eq!(count_nodes(&t, "SDBL_DROP_QUERY"), 1);
}

#[test]
fn test_slice7_field_recovery_stops_on_clause_keyword_at_any_depth_ru() {
    let input = "ВЫБРАТЬ 1 ( ИЗ T2 КАК Т";
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
        "ИЗ clause keyword must not be consumed by recover_field_to_alias_or_delimiter at depth>0.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice7_field_recovery_stops_on_clause_keyword_at_any_depth_en() {
    let input = "SELECT 1 ( FROM T2 AS T";
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
        "FROM clause keyword must not be consumed by recover_field_to_alias_or_delimiter at depth>0.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice7_field_recovery_stops_on_clause_keyword_inside_case_and_paren() {
    let input = "ВЫБРАТЬ 1 ( ВЫБОР ИЗ T2 КАК Т";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let from_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert!(
        from_clauses >= 1,
        "Outer ВЫБРАТЬ must keep its ИЗ clause even when recovery is at case_depth>0 AND paren_depth>0.\nTree: {:#?}",
        root
    );

    let bad_error = root.descendants().filter(|n| n.kind() == SyntaxKind::ERROR).any(|err| {
        err.descendants_with_tokens()
            .filter_map(|nt| nt.into_token())
            .any(|t| t.text().eq_ignore_ascii_case("ИЗ"))
    });
    assert!(
        !bad_error,
        "ИЗ clause keyword must not be consumed by recover_field_to_alias_or_delimiter at case_depth>0 AND paren_depth>0.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice7_field_recovery_stops_on_semicolon_at_any_depth() {
    let input = "ВЫБРАТЬ 1 (; УНИЧТОЖИТЬ T";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let drop_queries =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_DROP_QUERY).count();
    assert!(
        drop_queries >= 1,
        "Second statement (УНИЧТОЖИТЬ T) must parse as SDBL_DROP_QUERY despite the unterminated nested `(`.\nTree: {:#?}",
        root
    );

    let bad_error = root.descendants().filter(|n| n.kind() == SyntaxKind::ERROR).any(|err| {
        err.descendants_with_tokens().filter_map(|nt| nt.into_token()).any(|t| t.text() == ";")
    });
    assert!(
        !bad_error,
        "; statement separator must not be consumed by recover_field_to_alias_or_delimiter at depth>0.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice7_field_recovery_breaks_at_eof_inside_unterminated_paren() {
    let input = "ВЫБРАТЬ 1 (";
    let _ = parse_sdbl(input);
}

#[test]
fn test_slice7_field_recovery_does_not_stop_on_nested_select_at_depth() {
    let input = "ВЫБРАТЬ 1 ( ВЫБРАТЬ X ) ИЗ T";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let from_clauses =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert!(
        from_clauses >= 1,
        "Outer ИЗ T must survive an unterminated nested ВЫБРАТЬ in field recovery.\nTree: {:#?}",
        root
    );
}

#[test]
fn test_slice7_field_recovery_inner_from_misattribution_gate() {
    let input = "ВЫБРАТЬ 1 ( ВЫБРАТЬ X ИЗ Y ) ИЗ T";
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
