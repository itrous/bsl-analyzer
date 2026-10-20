use parser::parse_sdbl;
use syntax::{
    ast::{AstNode, SdblFromClause, SdblQueryPackage},
    SyntaxKind,
};

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

fn from_clause_of(input: &str) -> SdblFromClause {
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    root.descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE)
        .and_then(SdblFromClause::cast)
        .expect("SdblFromClause must be present")
}

#[test]
fn test_from_single_table_english() {
    parse_clean("SELECT * FROM Products");
    let t = tree("SELECT * FROM Products");
    assert_eq!(count_nodes(&t, "SDBL_FROM_CLAUSE"), 1);
    assert_eq!(count_nodes(&t, "SDBL_DATA_SOURCE"), 1);
}

#[test]
fn test_from_single_table_russian() {
    parse_clean("ВЫБРАТЬ * ИЗ Товары");
    let t = tree("ВЫБРАТЬ * ИЗ Товары");
    assert_eq!(count_nodes(&t, "SDBL_FROM_CLAUSE"), 1);
    assert_eq!(count_nodes(&t, "SDBL_DATA_SOURCE"), 1);
}

#[test]
fn test_from_two_sources_comma_separated() {
    parse_clean("SELECT * FROM T1, T2");
    let from = from_clause_of("SELECT * FROM T1, T2");
    assert_eq!(from.data_sources().count(), 2, "Two data sources expected");
}

#[test]
fn test_from_three_sources_comma_separated() {
    parse_clean("SELECT * FROM T1, T2, T3");
    let from = from_clause_of("SELECT * FROM T1, T2, T3");
    assert_eq!(from.data_sources().count(), 3);
}

#[test]
fn test_from_subquery_source_with_as_alias() {
    parse_clean("SELECT * FROM (SELECT 1) AS S");
    let t = tree("SELECT * FROM (SELECT 1) AS S");
    assert_eq!(count_nodes(&t, "SDBL_DATA_SOURCE"), 1);
    assert!(
        count_nodes(&t, "SDBL_SUBQUERY") >= 2,
        "Inner SdblSubquery must appear under the outer SdblDataSource"
    );
}

#[test]
fn test_from_subquery_source_russian_alias() {
    parse_clean("ВЫБРАТЬ * ИЗ (ВЫБРАТЬ 1) КАК С");
    let from = from_clause_of("ВЫБРАТЬ * ИЗ (ВЫБРАТЬ 1) КАК С");
    let source = from.data_sources().next().expect("single data source");
    assert!(source.subquery().is_some(), "SdblSubquery direct child of SdblDataSource");
    let alias = source.alias().expect("alias attached at data-source level");
    assert!(alias.has_as_keyword(), "КАК sets has_as_keyword()");
    assert_eq!(alias.name().as_deref(), Some("С"));
}

#[test]
fn test_from_subquery_source_bare_implicit_alias() {
    parse_clean("SELECT * FROM (SELECT 1) Sq");
    let from = from_clause_of("SELECT * FROM (SELECT 1) Sq");
    let source = from.data_sources().next().expect("single data source");
    let alias = source.alias().expect("bare alias is attached as SdblAlias");
    assert!(!alias.has_as_keyword(), "Bare alias must not carry AS/КАК");
    assert_eq!(alias.name().as_deref(), Some("Sq"));
}

#[test]
fn test_from_subquery_source_nested() {
    // `Outer` collides with the OUTER join keyword by text but is a valid alias.
    let input = "SELECT * FROM (SELECT * FROM (SELECT 1) AS Middle) AS Outer";
    parse_clean(input);
    let parse = parse_sdbl(input);
    assert_eq!(parse.syntax_node().text().to_string(), input, "Root must cover full input");
    let t = tree(input);
    assert!(count_nodes(&t, "SDBL_DATA_SOURCE") >= 2);
    assert!(count_nodes(&t, "SDBL_ALIAS") >= 2);
}

#[test]
fn test_from_subquery_source_with_inner_union() {
    parse_clean("SELECT * FROM (SELECT 1 UNION ALL SELECT 2) AS U");
}

#[test]
fn test_from_parameter_source_english() {
    parse_clean("SELECT * FROM &Tmp AS T");
}

#[test]
fn test_from_parameter_source_russian_with_kak() {
    parse_clean("ВЫБРАТЬ * ИЗ &ТЗ КАК ТЗ");
    let from = from_clause_of("ВЫБРАТЬ * ИЗ &ТЗ КАК ТЗ");
    let source = from.data_sources().next().expect("single data source");
    let table_ref = source.table_ref().expect("SdblTableRef for &ТЗ");
    assert!(
        table_ref.syntax().children().any(|n| n.kind() == SyntaxKind::SDBL_PARAMETER),
        "SdblParameter must be a direct child of SdblTableRef",
    );
}

#[test]
fn test_from_parameter_source_without_alias() {
    parse_clean("ВЫБРАТЬ * ИЗ &ТЗ");
    let from = from_clause_of("ВЫБРАТЬ * ИЗ &ТЗ");
    let source = from.data_sources().next().expect("single data source");
    assert!(source.alias().is_none(), "Parameter source without alias has no SdblAlias");
}

#[test]
fn test_from_parameter_source_in_multi_source_list() {
    parse_clean("SELECT * FROM Products, &Tmp AS T");
    let from = from_clause_of("SELECT * FROM Products, &Tmp AS T");
    assert_eq!(from.data_sources().count(), 2);
}

#[test]
fn test_from_parenthesised_parameter_enters_subquery_branch() {
    let input = "ВЫБРАТЬ * ИЗ (&Tmp)";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let has_param = root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_PARAMETER);
    assert!(!has_param, "Parenthesised &Tmp must not produce SdblParameter at the outer level",);
}

#[test]
fn test_table_ref_simple_identifier() {
    parse_clean("SELECT * FROM Products");
    let from = from_clause_of("SELECT * FROM Products");
    let source = from.data_sources().next().expect("single data source");
    assert!(source.table_ref().is_some(), "Simple identifier produces SdblTableRef");
}

#[test]
fn test_table_ref_two_segment_mdo_path() {
    parse_clean("SELECT Name FROM Catalog.Products");
    let from = from_clause_of("SELECT Name FROM Catalog.Products");
    let source = from.data_sources().next().expect("single data source");
    let table_ref = source.table_ref().expect("SdblTableRef");
    let idents: Vec<String> = table_ref
        .syntax()
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .filter(|t| t.kind() == SyntaxKind::IDENT)
        .map(|t| t.text().to_string())
        .collect();
    assert_eq!(idents, vec!["Catalog".to_string(), "Products".to_string()]);
}

#[test]
fn test_table_ref_russian_mdo_with_alias() {
    parse_clean("ВЫБРАТЬ * ИЗ Справочник.Товары КАК Т");
}

#[test]
fn test_table_ref_virtual_table_call_is_accepted() {
    parse_clean("SELECT * FROM Catalog.Products.SliceLast(&Date)");
    parse_clean("ВЫБРАТЬ * ИЗ Регистр.Остатки(, , Авто, ) КАК Т");
}

#[test]
fn test_source_alias_with_as_keyword() {
    parse_clean("SELECT * FROM Products AS P");
    let from = from_clause_of("SELECT * FROM Products AS P");
    let source = from.data_sources().next().unwrap();
    let alias = source.alias().expect("SdblAlias");
    assert!(alias.has_as_keyword());
    assert_eq!(alias.name().as_deref(), Some("P"));
}

#[test]
fn test_source_alias_with_kak_keyword() {
    parse_clean("ВЫБРАТЬ * ИЗ Товары КАК Т");
    let from = from_clause_of("ВЫБРАТЬ * ИЗ Товары КАК Т");
    let source = from.data_sources().next().unwrap();
    let alias = source.alias().expect("SdblAlias");
    assert!(alias.has_as_keyword());
    assert_eq!(alias.name().as_deref(), Some("Т"));
}

#[test]
fn test_source_alias_bare_implicit_form() {
    parse_clean("SELECT * FROM Products P");
    let from = from_clause_of("SELECT * FROM Products P");
    let source = from.data_sources().next().unwrap();
    let alias = source.alias().expect("SdblAlias");
    assert!(!alias.has_as_keyword(), "Bare alias carries no AS/КАК");
    assert_eq!(alias.name().as_deref(), Some("P"));
}

#[test]
fn test_source_alias_as_followed_by_clause_keyword_recovers() {
    let t = tree("SELECT * FROM Products AS WHERE Active = TRUE");
    assert!(t.contains("SDBL_FROM_CLAUSE"));
    assert!(t.contains("SDBL_WHERE_CLAUSE"), "WHERE must still parse after AS + clause keyword");
    assert!(t.contains("SDBL_ALIAS"));
}

#[test]
fn test_join_attaches_to_data_source_not_from_clause() {
    parse_clean("SELECT * FROM T LEFT JOIN U ON T.a = U.a");
    let from = from_clause_of("SELECT * FROM T LEFT JOIN U ON T.a = U.a");
    let source = from.data_sources().next().expect("single data source");
    assert_eq!(source.join_clauses().count(), 1, "One JOIN attached to T's data source");
}

#[test]
fn test_slice8_nodekinds_emitted_together() {
    let t = tree("ВЫБРАТЬ * ИЗ Справочник.Товары КАК Т, &Параметр, (ВЫБРАТЬ 1) КАК С");
    for kind in
        ["SDBL_FROM_CLAUSE", "SDBL_DATA_SOURCE", "SDBL_TABLE_REF", "SDBL_PARAMETER", "SDBL_ALIAS"]
    {
        assert!(count_nodes(&t, kind) >= 1, "Missing node kind: {kind}");
    }
}

#[test]
fn test_slice8_sdbl_data_source_is_direct_child_of_from_clause() {
    let from = from_clause_of("SELECT * FROM T1, T2, T3");
    let direct_children: Vec<_> =
        from.syntax().children().filter(|n| n.kind() == SyntaxKind::SDBL_DATA_SOURCE).collect();
    assert_eq!(direct_children.len(), 3);
}

#[test]
fn test_bilingual_multi_source_from_with_subquery_and_parameter() {
    parse_clean("ВЫБРАТЬ * ИЗ Catalog.Products КАК Products, &Tmp AS T, (ВЫБРАТЬ 1) КАК S");
}

#[test]
fn test_slice7_times_slice8_full_select_with_aliases_and_from_chain() {
    let input = "SELECT f1 AS a1, f2 AS a2 FROM T1 AS T, (SELECT 1) AS S";
    parse_clean(input);
    let from = from_clause_of(input);
    assert_eq!(from.data_sources().count(), 2, "Two outer data sources");
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let alias_count = root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_ALIAS).count();
    assert_eq!(alias_count, 4, "4 SdblAlias nodes: 2 field + 2 source");
}

#[test]
fn test_temp_table_source_crosses_package_boundary() {
    let input = "ВЫБРАТЬ Поле ПОМЕСТИТЬ ВремТаблица ИЗ Товары; \
                 ВЫБРАТЬ Поле ИЗ ВремТаблица";
    parse_clean(input);
    let parse = parse_sdbl(input);
    let package = SdblQueryPackage::cast(parse.syntax_node()).expect("query package");
    assert_eq!(package.queries().count(), 2);
}
