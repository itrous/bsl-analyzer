use parser::parse_sdbl;
use syntax::{
    ast::{AstNode, SdblQueryPackage, SdblSubquery},
    SyntaxKind,
};

fn package(input: &str) -> SdblQueryPackage {
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "unexpected parse errors for {input:?}: {errors:?}",
        errors = parse.errors()
    );
    SdblQueryPackage::cast(parse.syntax_node()).expect("top node is SdblQueryPackage")
}

fn package_allow_errors(input: &str) -> SdblQueryPackage {
    let parse = parse_sdbl(input);
    SdblQueryPackage::cast(parse.syntax_node()).expect("top node is SdblQueryPackage")
}

fn count_kind(pkg: &SdblQueryPackage, kind: SyntaxKind) -> usize {
    pkg.syntax().descendants().filter(|n| n.kind() == kind).count()
}

fn top_level_select_count(pkg: &SdblQueryPackage) -> usize {
    pkg.queries().count()
}

#[test]
fn test_single_select_is_a_package() {
    let pkg = package("SELECT Name FROM Products");
    assert_eq!(top_level_select_count(&pkg), 1);
}

#[test]
fn test_two_selects_separated_by_semicolon() {
    let pkg = package("SELECT Name FROM Products; SELECT Code FROM Services");
    assert_eq!(top_level_select_count(&pkg), 2);
}

#[test]
fn test_three_selects_separated_by_semicolons() {
    let pkg = package(
        "SELECT Name FROM Products;\n\
         SELECT Code FROM Services;\n\
         SELECT Price FROM Prices",
    );
    assert_eq!(top_level_select_count(&pkg), 3);
}

#[test]
fn test_trailing_semicolon_is_accepted() {
    let pkg = package("SELECT Name FROM Products;");
    assert_eq!(top_level_select_count(&pkg), 1);
}

#[test]
fn test_two_selects_with_trailing_semicolon() {
    let pkg = package("SELECT Name FROM Products; SELECT Code FROM Services;");
    assert_eq!(top_level_select_count(&pkg), 2);
}

#[test]
fn test_empty_input_yields_empty_package() {
    let pkg = package("");
    assert_eq!(top_level_select_count(&pkg), 0);
    assert_eq!(count_kind(&pkg, SyntaxKind::SDBL_DROP_QUERY), 0);
}

#[test]
fn test_whitespace_only_input_yields_empty_package() {
    let pkg = package("   \n\t  \n");
    assert_eq!(top_level_select_count(&pkg), 0);
}

#[test]
fn test_comment_only_input_yields_empty_package() {
    let pkg = package("// just a comment\n");
    assert_eq!(top_level_select_count(&pkg), 0);
}

#[test]
fn test_drop_english_keyword() {
    let parse = parse_sdbl("DROP T");
    assert!(!parse.has_errors(), "parse errors: {:?}", parse.errors());
    let root = parse.syntax_node();
    let pkg = SdblQueryPackage::cast(root.clone()).expect("package");
    assert_eq!(count_kind(&pkg, SyntaxKind::SDBL_DROP_QUERY), 1);
    assert_eq!(top_level_select_count(&pkg), 0);
    assert_eq!(
        root.children().filter(|n| n.kind() == SyntaxKind::SDBL_DROP_QUERY).count(),
        1,
        "SdblDropQuery sits directly under SdblQueryPackage"
    );
}

#[test]
fn test_drop_russian_keyword() {
    let pkg = package("УНИЧТОЖИТЬ ВТ");
    assert_eq!(count_kind(&pkg, SyntaxKind::SDBL_DROP_QUERY), 1);
    assert_eq!(top_level_select_count(&pkg), 0);
}

#[test]
fn test_drop_with_trailing_semicolon() {
    let pkg = package("DROP T;");
    assert_eq!(count_kind(&pkg, SyntaxKind::SDBL_DROP_QUERY), 1);
}

#[test]
fn test_select_then_drop_same_package() {
    let pkg = package("SELECT Name FROM Products; DROP T");
    assert_eq!(top_level_select_count(&pkg), 1);
    assert_eq!(count_kind(&pkg, SyntaxKind::SDBL_DROP_QUERY), 1);
}

#[test]
fn test_drop_then_select_same_package() {
    let pkg = package("DROP T; SELECT Name FROM Products");
    assert_eq!(top_level_select_count(&pkg), 1);
    assert_eq!(count_kind(&pkg, SyntaxKind::SDBL_DROP_QUERY), 1);
}

#[test]
fn test_union_english() {
    let pkg = package("SELECT Name FROM Products UNION SELECT Name FROM Services");
    assert_eq!(top_level_select_count(&pkg), 1);
    let subquery = pkg.queries().next().and_then(|q| q.subquery()).expect("subquery");
    assert_eq!(subquery.union_clauses().count(), 1);
}

#[test]
fn test_union_all_english() {
    let pkg = package("SELECT Name FROM Products UNION ALL SELECT Name FROM Services");
    assert_eq!(top_level_select_count(&pkg), 1);
    let subquery = pkg.queries().next().and_then(|q| q.subquery()).expect("subquery");
    assert_eq!(subquery.union_clauses().count(), 1);
}

#[test]
fn test_union_chain_english() {
    let pkg = package("SELECT A FROM T1 UNION SELECT A FROM T2 UNION ALL SELECT A FROM T3");
    let subquery = pkg.queries().next().and_then(|q| q.subquery()).expect("subquery");
    assert_eq!(subquery.union_clauses().count(), 2);
}

#[test]
fn test_union_russian() {
    let pkg = package("ВЫБРАТЬ Наименование ИЗ Товары ОБЪЕДИНИТЬ ВЫБРАТЬ Наименование ИЗ Услуги");
    let subquery = pkg.queries().next().and_then(|q| q.subquery()).expect("subquery");
    assert_eq!(subquery.union_clauses().count(), 1);
}

#[test]
fn test_union_all_russian() {
    let pkg = package("ВЫБРАТЬ Код ИЗ Товары ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Код ИЗ Услуги");
    let subquery = pkg.queries().next().and_then(|q| q.subquery()).expect("subquery");
    assert_eq!(subquery.union_clauses().count(), 1);
}

#[test]
fn test_union_and_union_all_share_one_node_kind() {
    let with_all = package("SELECT A FROM T1 UNION ALL SELECT A FROM T2");
    let without_all = package("SELECT A FROM T1 UNION SELECT A FROM T2");

    let one_clause = with_all
        .queries()
        .next()
        .and_then(|q| q.subquery())
        .expect("subquery with ALL")
        .union_clauses()
        .next()
        .expect("union clause with ALL");
    assert!(one_clause.has_all(), "UNION ALL → has_all() = true");

    let other_clause = without_all
        .queries()
        .next()
        .and_then(|q| q.subquery())
        .expect("subquery without ALL")
        .union_clauses()
        .next()
        .expect("union clause without ALL");
    assert!(!other_clause.has_all(), "UNION → has_all() = false");

    assert_eq!(
        one_clause.syntax().kind(),
        other_clause.syntax().kind(),
        "UNION and UNION ALL must share one node kind"
    );
    assert_eq!(one_clause.syntax().kind(), SyntaxKind::SDBL_UNION_CLAUSE);
}

#[test]
fn test_subquery_in_from_is_closed_by_paren() {
    let pkg = package("SELECT * FROM (SELECT 1)");
    let subqueries: Vec<_> =
        pkg.syntax().descendants().filter(|n| n.kind() == SyntaxKind::SDBL_SUBQUERY).collect();
    assert_eq!(subqueries.len(), 2, "outer + inner subquery");
    assert_eq!(top_level_select_count(&pkg), 1);
}

#[test]
fn test_union_inside_parenthesised_subquery_stays_inside() {
    let pkg = package("SELECT * FROM (SELECT 1 UNION SELECT 2) AS s");
    let root = pkg.syntax();
    let mut subqueries: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_SUBQUERY).collect();
    assert_eq!(subqueries.len(), 2);
    subqueries.sort_by_key(|n| usize::from(n.text_range().start()));
    let outer = SdblSubquery::cast(subqueries.remove(0)).expect("outer");
    let inner = SdblSubquery::cast(subqueries.remove(0)).expect("inner");
    assert_eq!(outer.union_clauses().count(), 0, "outer subquery has no UNION clause");
    assert_eq!(inner.union_clauses().count(), 1, "inner subquery owns the UNION clause");
}

#[test]
fn test_package_boundary_after_parenthesised_subquery() {
    let pkg = package("SELECT * FROM (SELECT 1); SELECT 2");
    assert_eq!(top_level_select_count(&pkg), 2);
    let subqueries =
        pkg.syntax().descendants().filter(|n| n.kind() == SyntaxKind::SDBL_SUBQUERY).count();
    assert_eq!(subqueries, 3);
}

#[test]
fn test_subquery_in_where_does_not_steal_outer_union() {
    let pkg = package(
        "SELECT Name FROM Products WHERE Id IN (SELECT Id FROM Archive) \
         UNION ALL SELECT Name FROM Services",
    );
    let outer = pkg.queries().next().and_then(|q| q.subquery()).expect("outer subquery");
    assert_eq!(outer.union_clauses().count(), 1);

    let mut subqueries: Vec<_> =
        pkg.syntax().descendants().filter(|n| n.kind() == SyntaxKind::SDBL_SUBQUERY).collect();
    assert_eq!(subqueries.len(), 2);
    subqueries.sort_by_key(|n| usize::from(n.text_range().start()));
    let inner = SdblSubquery::cast(subqueries.remove(1)).expect("inner subquery");
    assert_eq!(inner.union_clauses().count(), 0, "inner subquery in WHERE has no UNION clause");
}

#[test]
fn test_bilingual_three_item_package() {
    let pkg = package(
        "ВЫБРАТЬ Поле ИЗ Таблица1 ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Поле ИЗ Таблица2; \
         УНИЧТОЖИТЬ ВТ; \
         SELECT Field FROM Table3",
    );
    assert_eq!(top_level_select_count(&pkg), 2);
    assert_eq!(count_kind(&pkg, SyntaxKind::SDBL_DROP_QUERY), 1);

    let first_subquery = pkg.queries().next().and_then(|q| q.subquery()).expect("first subquery");
    assert_eq!(first_subquery.union_clauses().count(), 1);
}

#[test]
fn test_select_query_wraps_exactly_one_subquery() {
    let pkg = package("SELECT Name FROM Products");
    let q = pkg.queries().next().expect("select query");
    assert_eq!(q.syntax().kind(), SyntaxKind::SDBL_SELECT_QUERY);
    let direct_subqueries: Vec<_> =
        q.syntax().children().filter(|n| n.kind() == SyntaxKind::SDBL_SUBQUERY).collect();
    assert_eq!(
        direct_subqueries.len(),
        1,
        "SdblSelectQuery has exactly one SdblSubquery direct child"
    );
}

#[test]
fn test_drop_missing_identifier_is_recoverable() {
    let pkg = package_allow_errors("DROP ");
    assert_eq!(
        count_kind(&pkg, SyntaxKind::SDBL_DROP_QUERY),
        1,
        "SdblDropQuery node is still produced for IDE recovery"
    );
    let drop_node = pkg
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_DROP_QUERY)
        .expect("SdblDropQuery");
    let has_error_marker = drop_node.descendants().any(|n| n.kind() == SyntaxKind::ERROR);
    assert!(
        has_error_marker,
        "missing identifier triggers an Error sub-node per \
         sdbl-clean-room-slice6.md § Preserved pre-refactor behaviours"
    );
}
