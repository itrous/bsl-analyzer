use parser::parse_sdbl;

fn assert_clean_parse(parse: &syntax::Parse<syntax::SyntaxNode>, input: &str) {
    use syntax::SyntaxKind;
    assert!(
        !parse.has_errors(),
        "Expected clean parse for `{}`; got errors: {:#?}",
        input,
        parse.errors(),
    );
    let error_nodes: Vec<_> =
        parse.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::ERROR).collect();
    assert!(
        error_nodes.is_empty(),
        "Expected clean parse for `{}` — but tree contains {} ERROR recovery node(s): {:#?}",
        input,
        error_nodes.len(),
        error_nodes,
    );
}

fn first_join_clause(input: &str) -> syntax::SyntaxNode {
    use syntax::SyntaxKind;
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    parse
        .syntax_node()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_JOIN_CLAUSE)
        .expect("Tree must contain SdblJoinClause")
}

fn first_data_source(input: &str) -> syntax::ast::SdblDataSource {
    use syntax::ast::{AstNode, SdblQueryPackage};
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let package = SdblQueryPackage::cast(root).expect("query package");
    let select_query = package.queries().next().expect("query");
    let main = select_query.subquery().and_then(|s| s.main_query()).expect("main query");
    let from = main.from_clause().expect("FROM clause");
    let ds = from.data_sources().next().expect("first data source");
    ds
}

#[test]
fn test_slice9_a1_inner_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "ВЫБРАТЬ * ИЗ Т1 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Inner);
    assert!(join.data_source().is_some(), "JOIN must carry a joined SdblDataSource");
}

#[test]
fn test_slice9_a1_left_outer_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "ВЫБРАТЬ * ИЗ Т1 ЛЕВОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Left);
}

#[test]
fn test_slice9_a1_right_outer_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "ВЫБРАТЬ * ИЗ Т1 ПРАВОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Right);
}

#[test]
fn test_slice9_a1_full_outer_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "ВЫБРАТЬ * ИЗ Т1 ПОЛНОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Full);
}

#[test]
fn test_slice9_b_inner_join_en() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join =
        SdblJoinClause::cast(first_join_clause("SELECT * FROM T1 INNER JOIN T2 ON T1.A = T2.A"))
            .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Inner);
}

#[test]
fn test_slice9_b_left_outer_join_en() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "SELECT * FROM T1 LEFT OUTER JOIN T2 ON T1.A = T2.A",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Left);
}

#[test]
fn test_slice9_b_right_outer_join_en() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "SELECT * FROM T1 RIGHT OUTER JOIN T2 ON T1.A = T2.A",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Right);
}

#[test]
fn test_slice9_b_full_outer_join_en() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "SELECT * FROM T1 FULL OUTER JOIN T2 ON T1.A = T2.A",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Full);
}

#[test]
fn test_slice9_c_bare_join_en() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause("SELECT * FROM T1 JOIN T2 ON T1.A = T2.A"))
        .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Inner);
}

#[test]
fn test_slice9_d_bare_full_ru_local_allowance() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "ВЫБРАТЬ * ИЗ Т1 ПОЛНОЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Full);
}

#[test]
fn test_slice9_d_bare_left_ru_local_allowance() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "ВЫБРАТЬ * ИЗ Т1 ЛЕВОЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Left);
}

#[test]
fn test_slice9_d_bare_right_ru_local_allowance() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join = SdblJoinClause::cast(first_join_clause(
        "ВЫБРАТЬ * ИЗ Т1 ПРАВОЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А",
    ))
    .expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Right);
}

#[test]
fn test_slice9_chapter48_chained_joins_same_source() {
    let t1_source =
        first_data_source("SELECT * FROM T1 JOIN T2 ON T1.A = T2.A JOIN T3 ON T1.B = T3.B");
    let join_count = t1_source.join_clauses().count();
    assert_eq!(
        join_count, 2,
        "Both chained JOINs must attach as direct children of T1's SdblDataSource",
    );
}

#[test]
fn test_slice9_chapter48_nested_join_inside_join() {
    use syntax::ast::JoinType;
    let t1_source =
        first_data_source("SELECT * FROM T1 LEFT JOIN T2 JOIN T3 ON T2.B = T3.B ON T1.A = T2.A");
    let outer_join = t1_source.join_clauses().next().expect("outer LEFT JOIN");
    assert_eq!(outer_join.join_type(), JoinType::Left);
    let t2_source = outer_join.data_source().expect("T2 source under outer JOIN");
    let inner_join = t2_source.join_clauses().next().expect("inner JOIN attached to T2");
    assert_eq!(
        inner_join.join_type(),
        JoinType::Inner,
        "Inner bare JOIN must default to Inner via parent-tokens fallback over T2's data source",
    );
}

#[test]
fn test_slice9_inv7_subquery_join_shape() {
    let s_source =
        first_data_source("SELECT * FROM (SELECT * FROM T1) AS S LEFT JOIN T2 ON S.A = T2.A");
    assert!(
        s_source.subquery().is_some(),
        "Outer SdblDataSource must carry SdblSubquery as direct child",
    );
    assert!(
        s_source.join_clauses().next().is_some(),
        "Outer SdblDataSource must also carry the LEFT JOIN as direct child",
    );
}

#[test]
fn test_slice9_inv7_virtual_table_join_shape() {
    let r_source = first_data_source(
        "ВЫБРАТЬ * ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(&Дата) КАК Р \
         ЛЕВОЕ СОЕДИНЕНИЕ Т2 ПО Р.Х = Т2.Х",
    );
    assert!(
        r_source.table_ref().is_some(),
        "Outer SdblDataSource must carry SdblTableRef (virtual table) as direct child",
    );
    assert!(
        r_source.join_clauses().next().is_some(),
        "Outer SdblDataSource must also carry the JOIN as direct child",
    );
}

#[test]
fn test_slice9_inv7_or_in_on_shape() {
    use syntax::SyntaxKind;
    let join = first_join_clause("SELECT * FROM T1 JOIN T2 ON T1.A = T2.A OR T1.B = T2.B");
    let direct_kinds: Vec<SyntaxKind> = join.children().map(|c| c.kind()).collect();
    assert!(
        direct_kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "ON-condition must wrap in SdblLogicalOrExpr direct child of SdblJoinClause. \
         Got: {:?}",
        direct_kinds,
    );
}
