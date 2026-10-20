use expect_test::{expect, Expect};
use parser::parse_sdbl;

fn check(input: &str, expected: Expect) {
    let parse = parse_sdbl(input);
    let debug_tree = format!("{:#?}", parse.syntax_node());
    expected.assert_eq(&debug_tree);
}

fn check_no_errors(input: &str) {
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Expected no errors, but got: {:#?}", parse.errors());
}

#[test]
fn test_select_asterisk() {
    check_no_errors("SELECT * FROM Table");
}

#[test]
fn test_select_single_column() {
    check_no_errors("SELECT Name FROM Products");
}

#[test]
fn test_select_multiple_columns() {
    check_no_errors("SELECT Name, Code, Description FROM Products");
}

#[test]
fn test_select_with_where() {
    check_no_errors("SELECT Name FROM Products WHERE Active = TRUE");
}

#[test]
fn test_select_table_asterisk() {
    check_no_errors("SELECT Products.* FROM Products");
}

#[test]
fn test_select_qualified_column() {
    check_no_errors("SELECT Products.Name FROM Products");
}

#[test]
fn test_alias_with_as_keyword() {
    check(
        "SELECT Name AS ProductName FROM Products",
        expect![[r#"
            SDBL_QUERY_PACKAGE@0..40
              SDBL_SELECT_QUERY@0..40
                SDBL_SUBQUERY@0..40
                  SDBL_QUERY@0..40
                    IDENT@0..6 "SELECT"
                    WHITESPACE@6..7 " "
                    SDBL_FIELD_LIST@7..26
                      SDBL_SELECTED_FIELD@7..26
                        SDBL_LOGICAL_OR_EXPR@7..11
                          SDBL_LOGICAL_AND_EXPR@7..11
                            SDBL_ADDITIVE_EXPR@7..11
                              SDBL_MULTIPLICATIVE_EXPR@7..11
                                SDBL_COLUMN_REF@7..11
                                  IDENT@7..11 "Name"
                        WHITESPACE@11..12 " "
                        SDBL_ALIAS@12..26
                          IDENT@12..14 "AS"
                          WHITESPACE@14..15 " "
                          IDENT@15..26 "ProductName"
                    WHITESPACE@26..27 " "
                    SDBL_FROM_CLAUSE@27..40
                      IDENT@27..31 "FROM"
                      WHITESPACE@31..32 " "
                      SDBL_DATA_SOURCE@32..40
                        SDBL_TABLE_REF@32..40
                          IDENT@32..40 "Products"
        "#]],
    );
}

#[test]
fn test_alias_without_as_keyword() {
    check_no_errors("SELECT Name ProductName FROM Products");
}

#[test]
fn test_multiple_aliases_with_as() {
    check_no_errors("SELECT Name AS ProductName, Code AS ProductCode FROM Products");
}

#[test]
fn test_multiple_aliases_mixed() {
    check_no_errors("SELECT Name AS ProductName, Code ProductCode FROM Products");
}

#[test]
fn test_russian_alias_with_kak() {
    check_no_errors("ВЫБРАТЬ Имя КАК ИмяПродукта ИЗ Товары");
}

#[test]
fn test_alias_case_insensitive() {
    check_no_errors("SELECT Name as ProductName FROM Products");
    check_no_errors("SELECT Name As ProductName FROM Products");
    check_no_errors("SELECT Name aS ProductName FROM Products");
}

#[test]
fn test_asterisk_no_alias() {
    check_no_errors("SELECT * FROM Products");
    check_no_errors("SELECT Products.* FROM Products");
}

#[test]
fn test_russian_table_asterisk() {
    check_no_errors("ВЫБРАТЬ Товары.* ИЗ Товары");
}

#[test]
fn test_russian_into_simple() {
    check_no_errors("ВЫБРАТЬ Поле ПОМЕСТИТЬ ВремТаблица ИЗ Товары");
}

#[test]
fn test_union_simple() {
    check_no_errors("SELECT Name FROM Products UNION SELECT Name FROM Services");
}

#[test]
fn test_union_all() {
    check_no_errors("SELECT Name FROM Products UNION ALL SELECT Name FROM Services");
}

#[test]
fn test_union_multiple() {
    check_no_errors("SELECT A FROM T1 UNION SELECT B FROM T2 UNION SELECT C FROM T3");
}

#[test]
fn test_union_with_aliases() {
    check_no_errors("SELECT Name AS N FROM Products UNION SELECT Title AS N FROM Services");
}

#[test]
fn test_subquery_in_from() {
    check_no_errors("SELECT * FROM (SELECT Name FROM Products) AS Sub");
}

#[test]
fn test_subquery_nested() {
    // `Outer` collides with the OUTER join keyword by text but is a valid alias.
    let input = "SELECT * FROM (SELECT * FROM (SELECT Name FROM Products) AS Middle) AS Outer";
    check_no_errors(input);
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");
    assert!(
        root.descendants().filter(|node| node.kind() == syntax::SyntaxKind::SDBL_SUBQUERY).count()
            >= 2,
        "Nested subquery structure should be preserved"
    );
}

#[test]
fn test_subquery_with_where() {
    check_no_errors("SELECT * FROM (SELECT Name FROM Products WHERE Active = TRUE) AS Sub");
}

#[test]
fn test_subquery_in_expression() {
    check_no_errors("SELECT Name FROM Products WHERE Code IN (SELECT Code FROM Active)");
}

#[test]
fn test_arithmetic_expressions() {
    check_no_errors("SELECT Price * Quantity AS Total FROM Orders");
    check_no_errors("SELECT Price + Tax AS TotalPrice FROM Products");
    check_no_errors("SELECT Amount - Discount AS Final FROM Sales");
}

#[test]
fn test_logical_expressions() {
    check_no_errors("SELECT * FROM Products WHERE Active = TRUE AND Price > 100");
    check_no_errors("SELECT * FROM Products WHERE Category = 1 OR Category = 2");
    check_no_errors("SELECT * FROM Products WHERE NOT Deleted");
}

#[test]
fn test_comparison_expressions() {
    check_no_errors("SELECT * FROM Products WHERE Price > 100");
    check_no_errors("SELECT * FROM Products WHERE Quantity >= 10");
    check_no_errors("SELECT * FROM Products WHERE Code <> 0");
}

#[test]
fn test_function_calls() {
    check_no_errors("SELECT COUNT(*) AS Total FROM Products");
    check_no_errors("SELECT SUM(Price) AS TotalPrice FROM Products");
    check_no_errors("SELECT YEAR(Date) AS Year FROM Orders");
}

#[test]
fn test_mdo_table_reference() {
    check_no_errors("SELECT Name FROM Catalog.Products");
    check_no_errors("SELECT Ref FROM Document.Sales");
}

#[test]
fn test_mdo_qualified_column() {
    check_no_errors("SELECT Catalog.Products.Name FROM Catalog.Products");
}

#[test]
fn test_numeric_literals() {
    check_no_errors("SELECT * FROM Products WHERE Price = 100");
    check_no_errors("SELECT * FROM Products WHERE Price = 99.99");
}

#[test]
fn test_string_literals() {
    check_no_errors(r#"SELECT * FROM Products WHERE Name = "Product""#);
}

#[test]
fn test_boolean_literals() {
    check_no_errors("SELECT * FROM Products WHERE Active = TRUE");
    check_no_errors("SELECT * FROM Products WHERE Deleted = FALSE");
}

#[test]
fn test_null_literal() {
    check_no_errors("SELECT * FROM Products WHERE Description = NULL");
}

#[test]
fn test_parameter() {
    check_no_errors("SELECT * FROM Products WHERE Code = &ProductCode");
}

#[test]
fn test_multiple_parameters() {
    check_no_errors("SELECT * FROM Products WHERE Code = &Code AND Active = &IsActive");
}

#[test]
fn test_complex_query_with_all_features() {
    check_no_errors(
        r#"SELECT
            Products.Name AS ProductName,
            Products.Code AS ProductCode,
            SUM(Sales.Amount) AS TotalSales
        FROM
            Catalog.Products AS Products,
            (SELECT ProductRef, Amount FROM Document.Sales WHERE Date >= &StartDate) AS Sales
        WHERE
            Products.Active = TRUE
            AND Sales.Amount > 0"#,
    );
}

#[test]
fn test_multiple_queries_with_semicolon() {
    check_no_errors("SELECT Name FROM Products; SELECT Code FROM Services");
}

#[test]
fn test_multiple_queries_trailing_semicolon() {
    check_no_errors("SELECT Name FROM Products; SELECT Code FROM Services;");
}

#[test]
fn test_ast_alias_has_as_keyword() {
    use syntax::ast::{AstNode, SdblQueryPackage};

    let input = "SELECT Name AS ProductName FROM Products";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let package = SdblQueryPackage::cast(root).expect("Failed to cast to SdblQueryPackage");
    let query = package.queries().next().expect("No query found");
    let subquery = query.subquery().expect("No subquery found");
    let main_query = subquery.main_query().expect("No main query found");
    let field_list = main_query.field_list().expect("No field list found");
    let field = field_list.fields().next().expect("No field found");
    let alias = field.alias().expect("No alias found");

    assert!(alias.has_as_keyword(), "Alias should have AS keyword: {:?}", alias.syntax());
    assert_eq!(alias.name(), Some("ProductName".to_string()));
}

#[test]
fn test_ast_alias_without_as_keyword() {
    use syntax::ast::{AstNode, SdblQueryPackage};

    let input = "SELECT Name ProductName FROM Products";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let package = SdblQueryPackage::cast(root).expect("Failed to cast to SdblQueryPackage");
    let query = package.queries().next().expect("No query found");
    let subquery = query.subquery().expect("No subquery found");
    let main_query = subquery.main_query().expect("No main query found");
    let field_list = main_query.field_list().expect("No field list found");
    let field = field_list.fields().next().expect("No field found");
    let alias = field.alias().expect("No alias found");

    assert!(
        !alias.has_as_keyword(),
        "Implicit alias should not have AS keyword: {:?}",
        alias.syntax()
    );
    assert_eq!(alias.name(), Some("ProductName".to_string()));
}

#[test]
fn test_ast_asterisk_field() {
    use syntax::ast::{AstNode, SdblQueryPackage};

    let input = "SELECT * FROM Products";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let package = SdblQueryPackage::cast(root).expect("Failed to cast to SdblQueryPackage");
    let query = package.queries().next().expect("No query found");
    let subquery = query.subquery().expect("No subquery found");
    let main_query = subquery.main_query().expect("No main query found");
    let field_list = main_query.field_list().expect("No field list found");
    let field = field_list.fields().next().expect("No field found");

    assert!(field.is_asterisk(), "Field should be asterisk: {:?}", field.syntax());
    assert!(field.alias().is_none(), "Asterisk should not have alias");
}

#[test]
fn test_ast_union_queries() {
    use syntax::ast::{AstNode, SdblQueryPackage};

    let input = "SELECT A FROM T1 UNION SELECT B FROM T2";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();

    let package = SdblQueryPackage::cast(root).expect("Failed to cast to SdblQueryPackage");
    let query = package.queries().next().expect("No query found");
    let subquery = query.subquery().expect("No subquery found");

    let main_query = subquery.main_query().expect("No main query found");
    let main_field_list = main_query.field_list().expect("No field list in main query");
    let main_field = main_field_list.fields().next().expect("No field in main query");
    assert_eq!(
        main_field.expression().and_then(|e| e.first_token()).map(|t| t.text().to_string()),
        Some("A".to_string())
    );

    let union_count = subquery.union_queries().count();
    assert_eq!(union_count, 1, "Should have 1 UNION query");
}

#[test]
fn test_full_outer_join_simple() {
    let input = "SELECT * FROM T1 FULL OUTER JOIN T2 ON T1.A = T2.A";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Should parse without errors");
}

#[test]
fn test_multiple_full_outer_joins() {
    let input =
        "SELECT * FROM T1 FULL OUTER JOIN T2 ON T1.A = T2.A FULL OUTER JOIN T3 ON T1.B = T3.B";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Should parse multiple JOINs");
}

#[test]
fn test_on_not_consumed_as_alias() {
    let input = "SELECT * FROM T1 JOIN T2 ON T1.ID = T2.ID";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors());
    let text = format!("{:#?}", parse.syntax_node());
    assert!(text.contains("ON"), "ON keyword should be in AST, not consumed as alias");
}

#[test]
fn test_function_with_two_arguments() {
    check_no_errors("SELECT ISNULL(A, 0) FROM T");
}

#[test]
fn test_function_with_two_arguments_and_alias() {
    check_no_errors("SELECT ISNULL(Amount, 0) AS Total FROM Products");
}

#[test]
fn test_russian_function_with_arguments() {
    check_no_errors("ВЫБРАТЬ ЕСТЬNULL(Сумма, 0) ИЗ Товары");
}

#[test]
fn test_multiple_fields_with_function_arguments() {
    check_no_errors("SELECT Name, ISNULL(Amount, 0) AS Total FROM Products");
}

#[test]
fn test_single_arg_function() {
    check_no_errors("SELECT SUM(Amount) FROM T");
    check_no_errors("SELECT YEAR(Date) FROM T");
}

#[test]
fn test_two_arg_functions() {
    check_no_errors("SELECT ISNULL(A, 0) FROM T");
    check_no_errors("SELECT SUBSTRING(Name, 1, 10) FROM T");
}

#[test]
fn test_three_arg_functions() {
    check_no_errors("SELECT SUBSTRING(Text, 1, 5) FROM T");
}

#[test]
fn test_multi_arg_with_alias() {
    check_no_errors("SELECT ISNULL(Amount, 0) AS Total FROM T");
}

#[test]
fn test_mixed_fields_and_functions() {
    check_no_errors("SELECT Name, ISNULL(Amount, 0), Code FROM T");
}

#[test]
fn test_nested_functions() {
    check_no_errors("SELECT ISNULL(SUM(Amount), 0) FROM T");
}

#[test]
fn test_russian_multi_arg_functions() {
    check_no_errors("ВЫБРАТЬ ЕСТЬNULL(Сумма, 0) ИЗ Товары");
    check_no_errors("ВЫБРАТЬ ЕСТЬNULL(Сумма, 0) КАК Итого ИЗ Товары");
}

#[test]
fn test_join_with_complex_on_condition() {
    let input =
        "SELECT * FROM T1 INNER JOIN T2 ON T1.ID = T2.ID AND (T1.Amount > 100 OR T2.Price > 500)";

    let parse = parse_sdbl(input);

    assert!(!parse.has_errors(), "Should parse JOIN with complex ON condition");

    let ast_range = parse.syntax_node().text_range();
    let ast_end: usize = ast_range.end().into();
    assert_eq!(ast_end, input.len(), "AST should cover full input");
}

#[test]
fn test_multiple_functions_with_empty_parameters() {
    let query = r#"ВЫБРАТЬ
    Обороты1.Сумма,
    Обороты2.Количество
ИЗ
    РегистрНакопления.Продажи.Обороты(, , Авто, ) КАК Обороты1,
    РегистрНакопления.Закупки.Обороты(, , , Партнер = &Партнер) КАК Обороты2"#;

    let parse = parse_sdbl(query);
    assert!(!parse.has_errors(), "Should parse multiple functions with empty parameters");
}

#[test]
fn test_independent_query_from_fixture() {
    let query = include_str!("../fixtures/sdbl_independent_package.sdbl");

    let parse = parse_sdbl(query);

    if parse.has_errors() {
        println!("\n=== Parse errors ===");
        for (i, error) in parse.errors().iter().enumerate() {
            println!("Error {}: {:?}", i + 1, error);
        }
        println!("\n=== Query length: {} chars ===", query.len());
    }

    assert!(
        !parse.has_errors(),
        "Should parse independent package with ПОМЕСТИТЬ, ОБЪЕДИНИТЬ, empty params, and IN subqueries: found {} errors",
        parse.errors().len()
    );
}

#[test]
fn test_debug_in_expression_parsing() {
    let query = "ВЫБРАТЬ X ИЗ Т ГДЕ Поле В (1, 2, 3)";
    let parse = parse_sdbl(query);
    assert!(!parse.has_errors(), "Should parse simple IN with value list");

    let query = "ВЫБРАТЬ X ИЗ Т ГДЕ Поле В (ВЫБРАТЬ Y ИЗ Т2)";
    let parse = parse_sdbl(query);
    assert!(!parse.has_errors(), "Should parse simple IN with subquery");

    let query = "ВЫБРАТЬ X ИЗ Рег.Метод(Поле В (ВЫБРАТЬ Y ИЗ Т2))";
    let parse = parse_sdbl(query);

    if parse.has_errors() {
        println!("\n=== Errors for IN inside function parameter ===");
        for error in parse.errors() {
            println!("  - {:?}", error);
        }
        println!("\n=== Syntax tree ===");
        println!("{:#?}", parse.syntax_node());
    }

    assert!(!parse.has_errors(), "Should parse IN inside function parameter");
}

#[test]
fn test_error_recovery_in_empty_value() {
    let input = "SELECT * FROM T WHERE Field IN (1, , 3)";
    let parse = parse_sdbl(input);

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("ERROR"),
        "Expected ERROR node for empty value in IN list.\nTree: {}",
        tree
    );

    assert!(
        tree.contains("SDBL_WHERE_CLAUSE"),
        "WHERE clause should be parsed despite empty IN value.\nTree: {}",
        tree
    );

    assert!(tree.contains("SDBL_IN_EXPR"), "IN expression should be parsed.\nTree: {}", tree);
}

#[test]
fn test_error_recovery_in_leading_empty() {
    let input = "SELECT * FROM T WHERE Field IN (, 2, 3)";
    let parse = parse_sdbl(input);

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(tree.contains("ERROR"), "Expected ERROR node for leading empty value.\nTree: {}", tree);
    assert!(tree.contains("SDBL_WHERE_CLAUSE"), "WHERE clause should be parsed.\nTree: {}", tree);
}

#[test]
fn test_error_recovery_in_trailing_empty() {
    let input = "SELECT * FROM T WHERE Field IN (1, 2,)";
    let parse = parse_sdbl(input);

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("ERROR"),
        "Expected ERROR node for trailing empty value.\nTree: {}",
        tree
    );
    assert!(tree.contains("SDBL_WHERE_CLAUSE"), "WHERE clause should be parsed.\nTree: {}", tree);
}

#[test]
fn test_error_recovery_function_empty_args() {
    let input = "SELECT func(, , 123) FROM T";
    let parse = parse_sdbl(input);

    let tree = format!("{:#?}", parse.syntax_node());

    let error_count = tree.matches("ERROR").count();
    assert!(
        error_count >= 2,
        "Expected at least 2 ERROR nodes for empty arguments. Got: {}.\nTree: {}",
        error_count,
        tree
    );

    assert!(
        tree.contains("SDBL_FUNCTION_CALL"),
        "Function call should be parsed despite empty args.\nTree: {}",
        tree
    );

    assert!(tree.contains("SDBL_FIELD_LIST"), "Field list should be parsed.\nTree: {}", tree);
}

#[test]
fn test_error_recovery_function_leading_empty() {
    let input = "SELECT func(, 456) FROM T";
    let parse = parse_sdbl(input);

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(tree.contains("ERROR"), "Expected ERROR node for leading empty arg.\nTree: {}", tree);
    assert!(tree.contains("SDBL_FUNCTION_CALL"), "Function call should be parsed.\nTree: {}", tree);
}

#[test]
fn test_error_recovery_function_trailing_empty() {
    let input = "SELECT func(789,) FROM T";
    let parse = parse_sdbl(input);

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(tree.contains("ERROR"), "Expected ERROR node for trailing empty arg.\nTree: {}", tree);
    assert!(tree.contains("SDBL_FUNCTION_CALL"), "Function call should be parsed.\nTree: {}", tree);
}

#[test]
fn test_error_recovery_refs_predicate() {
    let input = "SELECT * FROM T WHERE Field REFS Catalog.Products";
    let parse = parse_sdbl(input);

    assert!(!parse.has_errors(), "REFS with qualified name should parse without errors");

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(tree.contains("SDBL_REFS_EXPR"), "REFS expression should be parsed.\nTree: {}", tree);
}

#[test]
fn test_error_recovery_comprehensive() {
    let input = "ВЫБРАТЬ Поле., Поле2, , Поле3 ИЗ Таблица1 ГДЕ Поле В (1, , 3) И func(, 456) > 0";

    let parse = parse_sdbl(input);
    let tree = format!("{:#?}", parse.syntax_node());

    let error_count = tree.matches("ERROR").count();
    assert!(
        error_count >= 3,
        "Expected at least 3 ERROR nodes. Got: {}.\nTree: {}",
        error_count,
        tree
    );

    assert!(tree.contains("SDBL_FROM_CLAUSE"), "FROM clause should be parsed.\nTree: {}", tree);
    assert!(tree.contains("SDBL_WHERE_CLAUSE"), "WHERE clause should be parsed.\nTree: {}", tree);
    assert!(tree.contains("SDBL_FIELD_LIST"), "Field list should be parsed.\nTree: {}", tree);
}

#[test]
fn test_no_infinite_loop_deeply_nested_dots() {
    let input = "SELECT T.a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v.w.x.y.z FROM T";
    let parse = parse_sdbl(input);

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_COLUMN_REF"),
        "Deeply nested column ref should be parsed.\nTree: {}",
        tree
    );
}

#[test]
fn test_type_cast_with_recovery() {
    let query = r#"ВЫБРАТЬ
    Поле1 КАК alias1,
    ВЫРАЗИТЬ(Поле2 КАК СТРОКА(200)) КАК alias2,
    Поле3 КАК alias3
ИЗ Таблица
ГДЕ Условие = 1"#;

    let parse = parse_sdbl(query);

    let tree = format!("{:#?}", parse.syntax_node());

    assert!(tree.contains("SDBL_TYPE"), "Expected SDBL_TYPE node for CAST type.\nTree: {}", tree);

    assert!(tree.contains("SDBL_FROM_CLAUSE"), "FROM clause should be parsed.\nTree: {}", tree);

    assert!(tree.contains("SDBL_WHERE_CLAUSE"), "WHERE clause should be parsed.\nTree: {}", tree);

    let field_count = tree.matches("SDBL_SELECTED_FIELD").count();
    assert!(field_count >= 3, "Should parse all 3 fields. Got: {}.\nTree: {}", field_count, tree);
}

#[test]
fn test_case_in_arithmetic_with_recovery() {
    let query = r#"ВЫБРАТЬ
    Поле1 КАК alias1,
    Поле2 + ВЫБОР КОГДА x ТОГДА 1 ИНАЧЕ 2 КОНЕЦ КАК alias2,
    Поле3 КАК alias3
ИЗ Таблица
ГДЕ Условие = 1"#;

    let parse = parse_sdbl(query);

    let tree = format!("{:#?}", parse.syntax_node());

    assert!(!parse.has_errors(), "CASE in arithmetic should parse correctly.\nTree: {}", tree);

    assert!(tree.contains("SDBL_CASE_EXPR"), "Should have CASE expression node.\nTree: {}", tree);

    assert!(tree.contains("SDBL_FROM_CLAUSE"), "FROM clause should be parsed.\nTree: {}", tree);

    assert!(tree.contains("SDBL_WHERE_CLAUSE"), "WHERE clause should be parsed.\nTree: {}", tree);

    let field_count = tree.matches("SDBL_SELECTED_FIELD").count();
    assert!(field_count >= 3, "Should parse all 3 fields. Got: {}.\nTree: {}", field_count, tree);

    assert!(tree.contains("alias1"), "First field should be parsed.\nTree: {}", tree);
    assert!(tree.contains("alias2"), "Field with CASE should be parsed.\nTree: {}", tree);
    assert!(tree.contains("alias3"), "Field after CASE should be parsed.\nTree: {}", tree);
}

#[test]
fn test_simple_plus_case() {
    use parser::parse_sdbl;
    let query = "ВЫБРАТЬ Поле2 + ВЫБОР КОГДА x ТОГДА 1 КОНЕЦ КАК alias2 ИЗ T";
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(!parse.has_errors(), "CASE in arithmetic context should parse correctly");
    assert!(tree.contains("SDBL_CASE_EXPR"), "Should have CASE expression node.\nTree: {}", tree);
    assert!(tree.contains("SDBL_FROM_CLAUSE"), "FROM clause should be parsed.\nTree: {}", tree);
}
#[test]
fn test_empty_string_literal() {
    use parser::parse_sdbl;
    let query = r#"ВЫБРАТЬ x <> "" КАК result ИЗ T"#;
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());

    assert!(!parse.has_errors(), "Should parse empty string");
}

#[test]
fn test_case_with_string_concat_in_then() {
    use parser::parse_sdbl;
    let query = r#"ВЫБРАТЬ
    ВЫБОР
        КОГДА x <> ""
            ТОГДА " (" + y + ")"
        ИНАЧЕ ""
    КОНЕЦ КАК result
ИЗ T"#;
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());

    let tree = format!("{:#?}", parse.syntax_node());
    if parse.has_errors() {
        eprintln!("\nErrors: {:?}", parse.errors());
    }

    assert!(!parse.has_errors(), "Should parse CASE with string concatenation in THEN clause");
    assert!(tree.contains("SDBL_CASE_EXPR"), "Should have CASE expression");
    assert!(tree.contains("SDBL_FROM_CLAUSE"), "Should parse FROM clause");
}

#[test]
fn test_single_string_literal() {
    use parser::parse_sdbl;
    let query = r#"ВЫБРАТЬ " (" КАК result ИЗ T"#;
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());

    let tree = format!("{:#?}", parse.syntax_node());
    if parse.has_errors() {
        eprintln!("\nErrors: {:?}", parse.errors());
    }

    assert!(!parse.has_errors(), "Single string should parse correctly");
    assert!(tree.contains("SDBL_FROM_CLAUSE"), "FROM clause should parse");

    assert!(
        tree.contains("SDBL_LITERAL") || tree.contains("SDBL_MULTI_STRING"),
        "Should have string literal node.\nTree: {}",
        tree
    );
}

#[test]
fn test_simple_two_queries_with_semicolon() {
    use parser::parse_sdbl;
    use syntax::ast::{AstNode, SdblQueryPackage};

    let query = r#"ВЫБРАТЬ x;
ВЫБРАТЬ y"#;

    let parse = parse_sdbl(query);
    eprintln!("\n{:#?}", parse.syntax_node());

    let root = parse.syntax_node();
    let package = SdblQueryPackage::cast(root).expect("Should have query package");
    let count = package.queries().count();

    eprintln!("Query count: {}", count);
    assert_eq!(count, 2, "Expected 2 queries separated by semicolon");
}
#[test]
fn test_unsupported_features() {
    use parser::parse_sdbl;

    let query1 = r#"ВЫБРАТЬ ВЫБОР КОГДА x ТОГДА 1 КОНЕЦ КАК result ИЗ T"#;
    let parse1 = parse_sdbl(query1);
    eprintln!("\n=== CASE expression ===");
    eprintln!("Has errors: {}", parse1.has_errors());

    let query2 = r#"ВЫБРАТЬ "a" + "b" КАК result ИЗ T"#;
    let parse2 = parse_sdbl(query2);
    eprintln!("\n=== String concatenation ===");
    eprintln!("Has errors: {}", parse2.has_errors());

    let query3 = r#"ВЫБРАТЬ ВЫРАЗИТЬ(field КАК СТРОКА(200)) КАК result ИЗ T"#;
    let parse3 = parse_sdbl(query3);
    eprintln!("\n=== Type cast ВЫРАЗИТЬ ===");
    eprintln!("Has errors: {}", parse3.has_errors());
    let tree3 = format!("{:#?}", parse3.syntax_node());
    eprintln!("Has ERROR: {}", tree3.contains("ERROR"));

    let query4 = r#"ВЫБРАТЬ НАЧАЛОПЕРИОДА(date, ДЕНЬ) КАК result ИЗ T"#;
    let parse4 = parse_sdbl(query4);
    eprintln!("\n=== НАЧАЛОПЕРИОДА function ===");
    eprintln!("Has errors: {}", parse4.has_errors());
}
#[test]
fn test_like_predicate() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ * ИЗ T ГДЕ name ПОДОБНО "%test%""#;
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());
    eprintln!("\nHas errors: {}", parse.has_errors());

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(tree.contains("SDBL_WHERE_CLAUSE"), "Should parse WHERE clause");
}

#[test]
fn test_between_predicate() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ * ИЗ T ГДЕ price МЕЖДУ 100 И 200"#;
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());
    eprintln!("\nHas errors: {}", parse.has_errors());

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(tree.contains("SDBL_WHERE_CLAUSE"), "Should parse WHERE clause");
}

#[test]
fn test_is_null_predicate() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ * ИЗ T ГДЕ field ЕСТЬ NULL"#;
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());
    eprintln!("\nHas errors: {}", parse.has_errors());

    let tree = format!("{:#?}", parse.syntax_node());
    assert!(tree.contains("SDBL_WHERE_CLAUSE"), "Should parse WHERE clause");
}
#[test]
fn test_order_by_clause() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ name, price ИЗ T УПОРЯДОЧИТЬ ПО price УБЫВ, name"#;
    let parse = parse_sdbl(query);

    eprintln!("\n=== ORDER BY ===");
    eprintln!("Has errors: {}", parse.has_errors());
    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("Has ORDER BY node: {}", tree.contains("SDBL_ORDER_BY"));
}

#[test]
fn test_group_by_clause() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ name, СУММА(price) ИЗ T СГРУППИРОВАТЬ ПО name"#;
    let parse = parse_sdbl(query);

    eprintln!("\n=== GROUP BY ===");
    eprintln!("Has errors: {}", parse.has_errors());
    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("Has GROUP BY node: {}", tree.contains("SDBL_GROUP_BY"));
}

#[test]
fn test_group_by_debug() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ name, СУММА(price) ИЗ T СГРУППИРОВАТЬ ПО name"#;
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());

    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("\nSearching for: SDBL_GROUP_CLAUSE");
    eprintln!("Found: {}", tree.contains("SDBL_GROUP_CLAUSE"));
}
#[test]
fn test_order_by_debug() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ name, price ИЗ T УПОРЯДОЧИТЬ ПО price УБЫВ, name"#;
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());

    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("\nSearching for: SDBL_ORDER_CLAUSE");
    eprintln!("Found: {}", tree.contains("SDBL_ORDER_CLAUSE"));
}
#[test]
fn debug_complex_query() {
    use parser::parse_sdbl;

    let q5 = r#"
        ВЫБРАТЬ
            category,
            name + ВЫБОР КОГДА discount > 0 ТОГДА " (скидка)" ИНАЧЕ "" КОНЕЦ КАК display_name
        ИЗ Products
    "#;
    let p5 = parse_sdbl(q5);

    eprintln!("\n{:#?}", p5.syntax_node());

    let tree5 = format!("{:#?}", p5.syntax_node());
    eprintln!("\nHas CASE: {}", tree5.contains("SDBL_CASE_EXPR"));
    eprintln!("Has errors: {}", p5.has_errors());
}
#[test]
fn demo_all_features_fixed() {
    use parser::parse_sdbl;

    let q1 = r#"ВЫБРАТЬ name + ВЫБОР КОГДА size <> "" ТОГДА " (" + size + ")" ИНАЧЕ "" КОНЕЦ КАК display_name ИЗ T"#;
    let p1 = parse_sdbl(q1);
    assert!(!p1.has_errors(), "CASE expression should work");

    let q2 = r#"ВЫБРАТЬ "Префикс: " + field + " (суффикс)" КАК result ИЗ T"#;
    let p2 = parse_sdbl(q2);
    assert!(!p2.has_errors(), "String concatenation should work");

    let q3 = r#"ВЫБРАТЬ category, СУММА(amount) ИЗ T СГРУППИРОВАТЬ ПО category"#;
    let p3 = parse_sdbl(q3);
    let tree3 = format!("{:#?}", p3.syntax_node());
    assert!(tree3.contains("SDBL_GROUP_CLAUSE"), "GROUP BY should work");

    let q4 = r#"ВЫБРАТЬ name, price ИЗ T УПОРЯДОЧИТЬ ПО price УБЫВ, name"#;
    let p4 = parse_sdbl(q4);
    let tree4 = format!("{:#?}", p4.syntax_node());
    assert!(tree4.contains("SDBL_ORDER_CLAUSE"), "ORDER BY should work");

    let q5 = r#"ВЫБРАТЬ category, name + ВЫБОР КОГДА discount > 0 ТОГДА " (скидка)" ИНАЧЕ "" КОНЕЦ КАК display_name, СУММА(amount) КАК total ИЗ Products ГДЕ active = ИСТИНА СГРУППИРОВАТЬ ПО category, name, discount УПОРЯДОЧИТЬ ПО category, total УБЫВ"#;
    let p5 = parse_sdbl(q5);
    let tree5 = format!("{:#?}", p5.syntax_node());
    assert!(!p5.has_errors(), "Complex query should parse without errors");
    assert!(tree5.contains("SDBL_CASE_EXPR"), "Should have CASE");
    assert!(tree5.contains("SDBL_GROUP_CLAUSE"), "Should have GROUP BY");
    assert!(tree5.contains("SDBL_ORDER_CLAUSE"), "Should have ORDER BY");

    eprintln!("\n✅ All new features work correctly!");
    eprintln!("  ✅ CASE expressions");
    eprintln!("  ✅ String concatenation");
    eprintln!("  ✅ GROUP BY (СГРУППИРОВАТЬ ПО)");
    eprintln!("  ✅ ORDER BY (УПОРЯДОЧИТЬ ПО)");
}
#[test]
fn test_view_presentation() {
    use parser::parse_sdbl;

    let q1 = r#"ВЫБРАТЬ * ИЗ Справочник.Контрагенты.ПРЕДСТАВЛЕНИЕ"#;
    let p1 = parse_sdbl(q1);

    eprintln!("\n=== Test 1: Simple VIEW ===");
    eprintln!("{:#?}", p1.syntax_node());
    eprintln!("Has errors: {}", p1.has_errors());

    let q2 = r#"ВЫБРАТЬ * ИЗ Справочник.Контрагенты.ПРЕДСТАВЛЕНИЕ КАК View1"#;
    let p2 = parse_sdbl(q2);

    eprintln!("\n=== Test 2: VIEW with alias ===");
    eprintln!("Has errors: {}", p2.has_errors());

    let q3 = r#"ВЫБРАТЬ * ИЗ Справочник.Контрагенты.ПРЕДСТАВЛЕНИЕ КАК V1 ЛЕВОЕ СОЕДИНЕНИЕ Документ.ПриходнаяНакладная.ПРЕДСТАВЛЕНИЕ КАК V2 ПО V1.Ссылка = V2.Контрагент"#;
    let p3 = parse_sdbl(q3);

    eprintln!("\n=== Test 3: Multiple VIEWs with JOIN ===");
    eprintln!("Has errors: {}", p3.has_errors());
}
#[test]
fn test_view_presentation_detailed() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ Name, Code ИЗ Справочник.Контрагенты.ПРЕДСТАВЛЕНИЕ КАК V1"#;
    let parse = parse_sdbl(query);

    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("\nFull tree:\n{}", tree);

    assert!(!parse.has_errors(), "Should parse VIEW without errors");
    assert!(tree.contains("SDBL_FROM_CLAUSE"), "Should have FROM clause");
    assert!(tree.contains("SDBL_TABLE_REF"), "Should have table reference");
}
#[test]
fn test_virtual_table_debug() {
    use parser::parse_sdbl;

    let query =
        r#"ВЫБРАТЬ * ИЗ РегистрНакопления.ТоварыНаСкладах.Обороты(&Начало, &Конец, День, )"#;
    let parse = parse_sdbl(query);

    eprintln!("\n{:#?}", parse.syntax_node());
    eprintln!("\nHas errors: {}", parse.has_errors());

    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("\nSearching for nodes...");
    eprintln!("Has SDBL_VIRTUAL_TABLE: {}", tree.contains("SDBL_VIRTUAL_TABLE"));
    eprintln!("Has SDBL_TABLE_REF: {}", tree.contains("SDBL_TABLE_REF"));
    eprintln!("Has SDBL_FUNCTION_CALL: {}", tree.contains("SDBL_FUNCTION_CALL"));
}
#[test]
fn test_advanced_sdbl_constructs() {
    use parser::parse_sdbl;

    let q1 = r#"ВЫБРАТЬ Name, Price ИЗ Products АВТОУПОРЯДОЧИВАНИЕ"#;
    let p1 = parse_sdbl(q1);
    eprintln!("\n=== AUTOORDER ===");
    eprintln!("Has errors: {}", p1.has_errors());
    let tree1 = format!("{:#?}", p1.syntax_node());
    eprintln!(
        "Has AUTOORDER node: {}",
        tree1.contains("AUTOORDER") || tree1.contains("АВТОУПОРЯДОЧИВАНИЕ")
    );

    let q2 = r#"ВЫБРАТЬ Category, СУММА(Price) КАК Total ИЗ Products СГРУППИРОВАТЬ ПО Category ИТОГИ ПО Category"#;
    let p2 = parse_sdbl(q2);
    eprintln!("\n=== TOTALS BY ===");
    eprintln!("Has errors: {}", p2.has_errors());
    let tree2 = format!("{:#?}", p2.syntax_node());
    eprintln!("Has TOTALS node: {}", tree2.contains("TOTALS") || tree2.contains("ИТОГИ"));

    let q3 = r#"ВЫБРАТЬ Name ИЗ Products ДЛЯ ИЗМЕНЕНИЯ Products"#;
    let p3 = parse_sdbl(q3);
    eprintln!("\n=== FOR UPDATE OF ===");
    eprintln!("Has errors: {}", p3.has_errors());

    let q4 = r#"ВЫБРАТЬ Name ИЗ Products ИНДЕКСИРОВАТЬ ПО Name"#;
    let p4 = parse_sdbl(q4);
    eprintln!("\n=== INDEX BY ===");
    eprintln!("Has errors: {}", p4.has_errors());

    let q5 = r#"ВЫБРАТЬ РАЗРЕШЕННЫЕ Name ИЗ Products"#;
    let p5 = parse_sdbl(q5);
    eprintln!("\n=== ALLOWED ===");
    eprintln!("Has errors: {}", p5.has_errors());
    let tree5 = format!("{:#?}", p5.syntax_node());
    eprintln!("Has ALLOWED: {}", tree5.contains("РАЗРЕШЕННЫЕ") || tree5.contains("ALLOWED"));

    let q6 = r#"ВЫБРАТЬ РАЗЛИЧНЫЕ Category ИЗ Products"#;
    let p6 = parse_sdbl(q6);
    eprintln!("\n=== DISTINCT ===");
    eprintln!("Has errors: {}", p6.has_errors());
    let tree6 = format!("{:#?}", p6.syntax_node());
    eprintln!("Has DISTINCT: {}", tree6.contains("РАЗЛИЧНЫЕ") || tree6.contains("DISTINCT"));
}
#[test]
fn test_having_clause() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ category, СУММА(price) КАК total ИЗ Products СГРУППИРОВАТЬ ПО category ИМЕЮЩИЕ СУММА(price) > 1000"#;
    let parse = parse_sdbl(query);

    eprintln!("\n=== HAVING clause ===");
    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("Has errors: {}", parse.has_errors());
    eprintln!("Has HAVING node: {}", tree.contains("SDBL_HAVING_CLAUSE"));

    assert!(!parse.has_errors(), "HAVING should parse without errors");
    assert!(tree.contains("SDBL_HAVING_CLAUSE"), "Should have HAVING clause node");
}

#[test]
fn test_for_update_clause() {
    use parser::parse_sdbl;

    let q1 = r#"ВЫБРАТЬ Name ИЗ Products ДЛЯ ИЗМЕНЕНИЯ"#;
    let p1 = parse_sdbl(q1);
    let tree1 = format!("{:#?}", p1.syntax_node());

    eprintln!("\n=== FOR UPDATE without MDO ===");
    eprintln!("Has errors: {}", p1.has_errors());
    eprintln!("Has FOR UPDATE node: {}", tree1.contains("SDBL_FOR_UPDATE"));

    assert!(!p1.has_errors(), "FOR UPDATE should parse without errors");
    assert!(tree1.contains("SDBL_FOR_UPDATE"), "Should have FOR UPDATE node");

    let q2 = r#"ВЫБРАТЬ Name ИЗ Products ДЛЯ ИЗМЕНЕНИЯ Products"#;
    let p2 = parse_sdbl(q2);

    eprintln!("\n=== FOR UPDATE with MDO ===");
    eprintln!("Has errors: {}", p2.has_errors());

    assert!(!p2.has_errors(), "FOR UPDATE with MDO should parse");
}

#[test]
fn test_index_by_clause() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ Name, Price ИЗ Products ИНДЕКСИРОВАТЬ ПО Name, Price"#;
    let parse = parse_sdbl(query);

    eprintln!("\n=== INDEX BY clause ===");
    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("Has errors: {}", parse.has_errors());
    eprintln!("Has INDEX BY node: {}", tree.contains("SDBL_INDEX_BY"));

    assert!(!parse.has_errors(), "INDEX BY should parse without errors");
    assert!(tree.contains("SDBL_INDEX_BY"), "Should have INDEX BY node");
}

#[test]
fn test_autoorder_clause() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ Name, Price ИЗ Products АВТОУПОРЯДОЧИВАНИЕ"#;
    let parse = parse_sdbl(query);

    eprintln!("\n=== AUTOORDER clause ===");
    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("Has errors: {}", parse.has_errors());
    eprintln!("Has AUTOORDER node: {}", tree.contains("SDBL_AUTOORDER"));

    assert!(!parse.has_errors(), "AUTOORDER should parse without errors");
    assert!(tree.contains("SDBL_AUTOORDER"), "Should have AUTOORDER node");
}

#[test]
fn test_totals_by_clause() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ category, СУММА(amount) КАК total ИЗ Sales СГРУППИРОВАТЬ ПО category ИТОГИ ПО category"#;
    let parse = parse_sdbl(query);

    eprintln!("\n=== TOTALS BY clause ===");
    let tree = format!("{:#?}", parse.syntax_node());
    eprintln!("Has errors: {}", parse.has_errors());
    eprintln!("Has TOTALS BY node: {}", tree.contains("SDBL_TOTALS_BY"));

    assert!(!parse.has_errors(), "TOTALS BY should parse without errors");
    assert!(tree.contains("SDBL_TOTALS_BY"), "Should have TOTALS BY node");
}

#[test]
fn test_phase2_combined() {
    use parser::parse_sdbl;

    let query = r#"
        ВЫБРАТЬ
            category,
            СУММА(amount) КАК total
        ИЗ Sales
        ГДЕ active = ИСТИНА
        СГРУППИРОВАТЬ ПО category
        ИМЕЮЩИЕ СУММА(amount) > 1000
        ДЛЯ ИЗМЕНЕНИЯ Sales
        ИНДЕКСИРОВАТЬ ПО category
        УПОРЯДОЧИТЬ ПО total УБЫВ
        АВТОУПОРЯДОЧИВАНИЕ
        ИТОГИ ПО category
    "#;

    let parse = parse_sdbl(query);
    let tree = format!("{:#?}", parse.syntax_node());

    eprintln!("\n=== Combined Phase 2 features ===");
    eprintln!("Has errors: {}", parse.has_errors());
    eprintln!("Has GROUP BY: {}", tree.contains("SDBL_GROUP_CLAUSE"));
    eprintln!("Has HAVING: {}", tree.contains("SDBL_HAVING_CLAUSE"));
    eprintln!("Has FOR UPDATE: {}", tree.contains("SDBL_FOR_UPDATE"));
    eprintln!("Has INDEX BY: {}", tree.contains("SDBL_INDEX_BY"));
    eprintln!("Has ORDER BY: {}", tree.contains("SDBL_ORDER_CLAUSE"));
    eprintln!("Has AUTOORDER: {}", tree.contains("SDBL_AUTOORDER"));
    eprintln!("Has TOTALS BY: {}", tree.contains("SDBL_TOTALS_BY"));
}
#[test]
fn test_simple_is_null() {
    use parser::parse_sdbl;

    let query = r#"ВЫБРАТЬ * ИЗ T ГДЕ Field ЕСТЬ NULL"#;
    let parse = parse_sdbl(query);
    let tree = format!("{:#?}", parse.syntax_node());

    eprintln!("\n=== Simple IS NULL test ===");
    eprintln!("Has errors: {}", parse.has_errors());
    eprintln!("\n{}", tree);
}

#[test]
fn test_parameter_as_data_source() {
    check_no_errors("ВЫБРАТЬ Поле КАК Поле ИЗ &ТЗ КАК ТЗ");
    check_no_errors("ВЫБРАТЬ Поле ИЗ &ТаблицаЗначений КАК Т");
}

#[test]
fn test_parameter_as_data_source_in_batch() {
    let query = "ВЫБРАТЬ Поле КАК Поле ПОМЕСТИТЬ ВТ ИЗ &ТЗ КАК ТЗ;\n\
ВЫБРАТЬ Остатки.Номенклатура КАК Номенклатура ПОМЕСТИТЬ Результат ИЗ &Остатки КАК Остатки";

    let parse = parse_sdbl(query);
    assert!(!parse.has_errors(), "Batch with &Parameter data sources should parse without errors");

    use syntax::ast::{AstNode, SdblQueryPackage};
    let root = parse.syntax_node();
    let package = SdblQueryPackage::cast(root).expect("Should have query package");
    assert_eq!(package.queries().count(), 2, "Expected 2 queries");
}

#[test]
fn test_sdbl_constructs_for_false_positive() {
    use syntax::SyntaxKind;

    let tests: Vec<(&str, &str)> = vec![
        ("VALUE function", "ВЫБРАТЬ ЗНАЧЕНИЕ(Перечисление.Качества.Новый) ИЗ T"),
        ("NOT IN subquery", "ВЫБРАТЬ * ИЗ T ГДЕ НЕ Поле В (ВЫБРАТЬ X ИЗ T2)"),
        ("DATETIME", "ВЫБРАТЬ * ИЗ T ГДЕ Дата <> ДАТАВРЕМЯ(1, 1, 1)"),
        ("BOOLEAN", "ВЫБРАТЬ * ИЗ T ГДЕ X = ИСТИНА И Y = ЛОЖЬ"),
        ("IN with param", "ВЫБРАТЬ * ИЗ T ГДЕ Склад В (&Список)"),
        ("Virtual table with params", "ВЫБРАТЬ * ИЗ Рег.Остатки(, Поле = &Парам) КАК Т"),
        ("Virtual table empty params", "ВЫБРАТЬ * ИЗ Рег.Остатки(, ) КАК Т"),
        ("TOTALS BY", "ВЫБРАТЬ Поле ИЗ T ИТОГИ СУММА(Кол) ПО Поле"),
        ("NOT expr IN with param list", "ВЫБРАТЬ * ИЗ T ГДЕ НЕ Поле В (&Исключ)"),
        ("PRESENTATION function", "ВЫБРАТЬ ПРЕДСТАВЛЕНИЕ(Поле) ИЗ T"),
        ("ISNULL with comparison arg", "ВЫБРАТЬ ЕСТЬNULL(X.Y = ЗНАЧЕНИЕ(Справ.Пуст), ИСТИНА) ИЗ T"),
        ("Division in expression", "ВЫБРАТЬ X / Y ИЗ T"),
        (
            "ISNULL with CASE inside",
            "ВЫБРАТЬ ЕСТЬNULL(ВЫБОР КОГДА X = 0 ТОГДА 1 ИНАЧЕ X КОНЕЦ, 1) ИЗ T",
        ),
        ("IN with multiple VALUE", "ВЫБРАТЬ * ИЗ T ГДЕ Поле В (ЗНАЧЕНИЕ(Перечисление.Статусы.Новый), ЗНАЧЕНИЕ(Перечисление.Статусы.Ошибка))"),
        ("Nested NOT IS NULL", "ВЫБРАТЬ * ИЗ T ГДЕ НЕ Ссылка ЕСТЬ NULL"),
        ("NOT...NOT IS NULL", "ВЫБРАТЬ * ИЗ T ГДЕ НЕ Назначение.Договор.Ссылка ЕСТЬ NULL"),
        (
            "CASE in VT param",
            "ВЫБРАТЬ * ИЗ Рег.Остатки(, ВЫБОР КОГДА X ТОГДА ЛОЖЬ ИНАЧЕ ИСТИНА КОНЕЦ) КАК Т",
        ),
    ];

    let mut failed = false;
    for (name, query) in &tests {
        let parse = parse_sdbl(query);
        let has_error = parse.syntax_node().descendants().any(|n| n.kind() == SyntaxKind::ERROR);
        if has_error {
            failed = true;
            eprintln!("FAIL: {}: {}", name, query);
            for node in parse.syntax_node().descendants() {
                if node.kind() == SyntaxKind::ERROR {
                    eprintln!("  ERROR at {:?}: {:?}", node.text_range(), node.text());
                }
            }
        } else {
            eprintln!("OK:   {}", name);
        }
    }

    if failed {
        panic!("Some SDBL constructs produced ERROR nodes");
    }
}

#[test]
fn test_error_node_analysis() {
    use syntax::SyntaxKind;

    let cases: Vec<(&str, &str)> = vec![
        ("valid VT empty params", "ВЫБРАТЬ * ИЗ Рег.Остатки(, Поле = &Парам) КАК Т"),
        ("valid VT all empty", "ВЫБРАТЬ * ИЗ Рег.Остатки(, ) КАК Т"),
        ("invalid incomplete FROM", "ВЫБРАТЬ Поле ИЗ  "),
        ("invalid incomplete WHERE", "ВЫБРАТЬ Поле ИЗ Таблица ГДЕ Условие >"),
        ("invalid batch partial", "ВЫБРАТЬ Поле ИЗ Таблица1; ВЫБРАТЬ Поле2 ИЗ"),
    ];

    for (name, query) in &cases {
        let parse = parse_sdbl(query);
        let errors: Vec<_> =
            parse.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::ERROR).collect();

        eprintln!("\n=== {} ===", name);
        eprintln!("has_errors(): {}", parse.has_errors());
        for e in &errors {
            let parent_kind = e.parent().map(|p| format!("{:?}", p.kind())).unwrap_or_default();
            eprintln!(
                "  ERROR range={:?} empty={} text={:?} parent={}",
                e.text_range(),
                e.text_range().is_empty(),
                e.text(),
                parent_kind,
            );
        }
    }
}

#[test]
fn test_drop_table_russian() {
    check_no_errors("УНИЧТОЖИТЬ ВременнаяТаблица");
}

#[test]
fn test_drop_table_english() {
    check_no_errors("DROP TempTable");
}

#[test]
fn test_batch_with_drop() {
    // `ПОМЕСТИТЬ` precedes `ИЗ`, which is both the documented order and the
    // only one that occurs in practice.
    check_no_errors("ВЫБРАТЬ Поле ПОМЕСТИТЬ ВТ ИЗ Таблица; УНИЧТОЖИТЬ ВТ");
}

#[test]
fn test_package_with_three_statements() {
    use syntax::ast::{AstNode, SdblQueryPackage};
    let input = "SELECT Name FROM Products; \
                 SELECT Code FROM Services; \
                 SELECT Price FROM Prices";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Should parse 3-SELECT package: {:?}", parse.errors());
    let package = SdblQueryPackage::cast(parse.syntax_node()).expect("query package");
    assert_eq!(package.queries().count(), 3, "Expected 3 SELECT queries in the package");
}

#[test]
fn test_subquery_in_where_with_outer_union() {
    use syntax::{
        ast::{AstNode, SdblQueryPackage, SdblSubquery},
        SyntaxKind,
    };
    let input = "SELECT Name FROM Products WHERE Id IN (SELECT Id FROM Archive) \
                 UNION ALL SELECT Name FROM Services";
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "Should parse subquery-in-WHERE + outer UNION: {:?}",
        parse.errors()
    );
    let root = parse.syntax_node();
    let package = SdblQueryPackage::cast(root.clone()).expect("query package");
    assert_eq!(package.queries().count(), 1, "Single outer SELECT statement");
    let outer_subquery =
        package.queries().next().and_then(|q| q.subquery()).expect("outer SdblSubquery");
    assert_eq!(
        outer_subquery.union_clauses().count(),
        1,
        "UNION ALL must attach to the outer subquery",
    );
    let mut subquery_nodes: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_SUBQUERY).collect();
    assert_eq!(subquery_nodes.len(), 2, "One outer + one IN-subquery");
    subquery_nodes.sort_by_key(|n| usize::from(n.text_range().start()));
    let inner_subquery =
        SdblSubquery::cast(subquery_nodes.pop().expect("inner")).expect("cast inner SdblSubquery");
    assert_eq!(
        inner_subquery.union_clauses().count(),
        0,
        "The IN-subquery must not own the UNION clause",
    );
}

#[test]
fn test_drop_mid_package_after_union() {
    use syntax::{
        ast::{AstNode, SdblQueryPackage},
        SyntaxKind,
    };
    let input = "ВЫБРАТЬ Поле ИЗ Таблица1 ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Поле ИЗ Таблица2; \
                 УНИЧТОЖИТЬ ВТ; \
                 ВЫБРАТЬ Поле ИЗ Таблица3";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Should parse DROP-after-union package: {:?}", parse.errors());
    let root = parse.syntax_node();
    let package = SdblQueryPackage::cast(root.clone()).expect("query package");
    assert_eq!(package.queries().count(), 2, "Two SELECT statements surround the DROP");
    let first_outer_subquery = package
        .queries()
        .next()
        .and_then(|q| q.subquery())
        .expect("outer subquery of first SELECT");
    assert_eq!(
        first_outer_subquery.union_clauses().count(),
        1,
        "UNION ALL lives inside the first SELECT statement, not the package",
    );
    let drop_count = root.children().filter(|n| n.kind() == SyntaxKind::SDBL_DROP_QUERY).count();
    assert_eq!(drop_count, 1, "Exactly one DROP statement in the package");
}

#[test]
fn test_slice8_from_multi_source_with_bare_alias() {
    use syntax::{
        ast::{AstNode, SdblFromClause},
        SyntaxKind,
    };
    let input = "ВЫБРАТЬ * ИЗ Т1, Т2 А";
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "Multi-source FROM with bare alias should parse: {:?}",
        parse.errors()
    );
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input (no trailing drop)");
    let from = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE)
        .and_then(SdblFromClause::cast)
        .expect("SdblFromClause");
    let sources: Vec<_> = from.data_sources().collect();
    assert_eq!(sources.len(), 2, "Expected two SdblDataSource nodes in the FROM list");
    let second_alias = sources[1].alias().expect("Second data source must carry SdblAlias");
    assert_eq!(second_alias.name().as_deref(), Some("А"), "Bare alias name mismatch");
    assert!(
        !second_alias.has_as_keyword(),
        "Bare alias must not carry AS/КАК keyword (implicit-alias branch of source_alias)",
    );
}

#[test]
fn test_slice8_russian_subquery_source_with_alias() {
    use syntax::{
        ast::{AstNode, SdblFromClause},
        SyntaxKind,
    };
    let input = "ВЫБРАТЬ * ИЗ (ВЫБРАТЬ 1) КАК С";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Russian subquery source should parse: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input (no trailing drop)");
    let from = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE)
        .and_then(SdblFromClause::cast)
        .expect("SdblFromClause");
    let source = from.data_sources().next().expect("single SdblDataSource");
    assert!(
        source.subquery().is_some(),
        "SdblSubquery must sit as a direct child of SdblDataSource",
    );
    let alias = source.alias().expect("data source must carry SdblAlias");
    assert!(alias.has_as_keyword(), "КАК alias must set has_as_keyword() = true");
    assert_eq!(alias.name().as_deref(), Some("С"));
}

#[test]
fn test_slice8_temp_table_source_across_package_boundary() {
    use syntax::ast::{AstNode, SdblQueryPackage};
    let input = "ВЫБРАТЬ Поле ПОМЕСТИТЬ ВремТаблица ИЗ Товары; \
                 ВЫБРАТЬ Поле ИЗ ВремТаблица";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Temp-table package should parse: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input (no trailing drop)");
    let package = SdblQueryPackage::cast(root).expect("query package");
    let queries: Vec<_> = package.queries().collect();
    assert_eq!(queries.len(), 2, "Two SELECT statements around the temp table");
    let second_from = queries[1]
        .subquery()
        .and_then(|sq| sq.main_query())
        .and_then(|q| q.from_clause())
        .expect("second query must have FROM clause");
    let sources: Vec<_> = second_from.data_sources().collect();
    assert_eq!(sources.len(), 1, "Second query FROM must have a single data source");
    let table_ref = sources[0].table_ref().expect("identifier-only table_ref");
    assert_eq!(
        table_ref.syntax().text().to_string().trim(),
        "ВремТаблица",
        "Second query must reference the temp table created by the first",
    );
}

#[test]
fn test_slice8_parameter_source_without_alias() {
    use syntax::{
        ast::{AstNode, SdblFromClause},
        SyntaxKind,
    };
    let input = "ВЫБРАТЬ * ИЗ &ТЗ";
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "Parameter source without alias should parse: {:?}",
        parse.errors()
    );
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input (no trailing drop)");
    let from = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE)
        .and_then(SdblFromClause::cast)
        .expect("SdblFromClause");
    let source = from.data_sources().next().expect("single SdblDataSource");
    assert!(source.alias().is_none(), "Parameter source without alias must carry no SdblAlias");
    let table_ref = source.table_ref().expect("SdblTableRef for parameter source");
    let has_parameter =
        table_ref.syntax().children().any(|n| n.kind() == SyntaxKind::SDBL_PARAMETER);
    assert!(
        has_parameter,
        "SdblParameter must be a direct child of SdblTableRef on the &Ident path"
    );
}

#[test]
fn test_slice10a_nested_not() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ НЕ НЕ А";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Nested NOT should parse: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");
    let outer_not = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_NOT_EXPR)
        .expect("outer SdblNotExpr");
    let inner_not = outer_not
        .children()
        .find(|n| n.kind() == SyntaxKind::SDBL_NOT_EXPR)
        .expect("inner SdblNotExpr nested directly inside outer (right-recursive shape)");
    assert!(
        inner_not.text().to_string().contains("НЕ"),
        "Inner SdblNotExpr text must include the second НЕ token",
    );
}

#[test]
fn test_slice10a_not_and_binding() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ НЕ А И Б";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "NOT-AND binding should parse: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");
    let and_expr = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_LOGICAL_AND_EXPR)
        .expect("SdblLogicalAndExpr at the top");
    let not_descendants_under_and: Vec<_> =
        and_expr.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_NOT_EXPR).collect();
    assert_eq!(
        not_descendants_under_and.len(),
        1,
        "Exactly one SdblNotExpr nested under the SdblLogicalAndExpr (NOT binds А, not the AND-pair)",
    );
    let not_text = not_descendants_under_and[0].text().to_string();
    assert!(
        not_text.contains('А') && !not_text.contains('Б'),
        "SdblNotExpr must wrap А alone — not include Б; got {not_text:?}",
    );
}

#[test]
fn test_slice10a_nested_unary_minus() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ - - А ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Nested unary minus should parse: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");
    let outer_unary = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_UNARY_EXPR)
        .expect("outer SdblUnaryExpr");
    let inner_unary = outer_unary
        .children()
        .find(|n| n.kind() == SyntaxKind::SDBL_UNARY_EXPR)
        .expect("inner SdblUnaryExpr nested directly inside outer");
    assert!(
        inner_unary.text().to_string().contains('-'),
        "Inner SdblUnaryExpr text must include the second minus token",
    );
}

#[test]
fn test_slice10a_unary_inside_additive_right_operand() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ А + - Б ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Unary in additive right operand: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");
    let additive = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_ADDITIVE_EXPR)
        .expect("SdblAdditiveExpr");
    let unary_under_additive =
        additive.descendants().find(|n| n.kind() == SyntaxKind::SDBL_UNARY_EXPR);
    assert!(
        unary_under_additive.is_some(),
        "SdblUnaryExpr must appear under SdblAdditiveExpr — got tree {additive:#?}",
    );
}

#[test]
fn test_slice10a_russian_not_with_paren_and() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ НЕ (А И Б)";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "НЕ (А И Б) should parse: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");
    let not_expr =
        root.descendants().find(|n| n.kind() == SyntaxKind::SDBL_NOT_EXPR).expect("SdblNotExpr");
    let paren_in_not = not_expr.descendants().find(|n| n.kind() == SyntaxKind::SDBL_PAREN_EXPR);
    assert!(paren_in_not.is_some(), "SdblParenExpr must sit inside SdblNotExpr for `НЕ (А И Б)`",);
    let and_in_paren =
        paren_in_not.unwrap().descendants().find(|n| n.kind() == SyntaxKind::SDBL_LOGICAL_AND_EXPR);
    assert!(and_in_paren.is_some(), "SdblLogicalAndExpr must sit inside the SdblParenExpr",);
}

#[test]
fn test_slice10a_multi_string_three_tokens() {
    use syntax::SyntaxKind;
    let input = r#"ВЫБРАТЬ "a" "b" "c" ИЗ Т"#;
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");
    let multi = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_MULTI_STRING)
        .expect("SdblMultiString for the first user-visible string");
    let string_token_count = multi
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .filter(|t| t.kind() == SyntaxKind::STRING)
        .count();
    assert_eq!(
        string_token_count, 3,
        "First SdblMultiString must wrap exactly 3 STRING tokens (the lexer's open/content/close split for one user-visible string)",
    );
}

#[test]
fn test_slice10a_precedence_with_newline_trivia() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ 1\n+\n2 * 3 ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Newline trivia in precedence: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");

    let additive_with_plus = root
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::SDBL_ADDITIVE_EXPR)
        .find(|n| {
            n.children_with_tokens()
                .filter_map(|c| c.into_token())
                .any(|t| t.kind() == SyntaxKind::PLUS)
        })
        .expect("SdblAdditiveExpr with a DIRECT PLUS token child — mini-spec §AST-shape #1");

    let mul_with_star = additive_with_plus
        .children()
        .filter(|n| n.kind() == SyntaxKind::SDBL_MULTIPLICATIVE_EXPR)
        .find(|n| {
            n.children_with_tokens()
                .filter_map(|c| c.into_token())
                .any(|t| t.kind() == SyntaxKind::STAR)
        })
        .expect(
            "SdblMultiplicativeExpr with a DIRECT STAR token child — the `2 * 3` wrapper sits as a direct child of the additive node",
        );
    let mul_text = mul_with_star.text().to_string();
    assert!(
        mul_text.contains('2') && mul_text.contains('3'),
        "SdblMultiplicativeExpr must cover `2 * 3` (got {mul_text:?})",
    );

    let plus_count = additive_with_plus
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .filter(|t| t.kind() == SyntaxKind::PLUS)
        .count();
    assert_eq!(
        plus_count, 1,
        "FLAT additive wrapper for `1\\n+\\n2 * 3` must have exactly 1 PLUS direct token child"
    );
}

#[test]
fn test_slice10a_flat_additive_associativity() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ А + Б + Г ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "Expected clean parse for {input:?}, got errors: {:?}",
        parse.errors()
    );
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");
    let additives: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_ADDITIVE_EXPR).collect();
    assert_eq!(additives.len(), 1, "Exactly one SdblAdditiveExpr — wrapper is FLAT, not nested",);
    let additive = &additives[0];
    let plus_tokens = additive
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .filter(|t| t.kind() == SyntaxKind::PLUS)
        .count();
    assert_eq!(
        plus_tokens, 2,
        "FLAT SdblAdditiveExpr must have exactly 2 `+` direct token children for `А + Б + Г`",
    );
}

#[test]
fn test_slice10a_paren_single_vs_tuple_two() {
    use syntax::SyntaxKind;
    let single_input = "ВЫБРАТЬ (1) ИЗ Т";
    let single_parse = parse_sdbl(single_input);
    assert!(
        !single_parse.has_errors(),
        "Single-element paren should parse: {:?}",
        single_parse.errors()
    );
    let single_root = single_parse.syntax_node();
    assert_eq!(single_root.text().to_string(), single_input, "Single root coverage");
    let single_paren = single_root.descendants().find(|n| n.kind() == SyntaxKind::SDBL_PAREN_EXPR);
    let single_tuple = single_root.descendants().find(|n| n.kind() == SyntaxKind::SDBL_TUPLE_EXPR);
    assert!(single_paren.is_some(), "(1) must emit SdblParenExpr");
    assert!(single_tuple.is_none(), "(1) must NOT emit SdblTupleExpr");

    let tuple_input = "ВЫБРАТЬ (1, 2) ИЗ Т";
    let tuple_parse = parse_sdbl(tuple_input);
    assert!(
        !tuple_parse.has_errors(),
        "Two-element tuple should parse: {:?}",
        tuple_parse.errors()
    );
    let tuple_root = tuple_parse.syntax_node();
    assert_eq!(tuple_root.text().to_string(), tuple_input, "Tuple root coverage");
    let tuple = tuple_root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TUPLE_EXPR)
        .expect("(1, 2) must emit SdblTupleExpr");
    let comma_count = tuple
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .filter(|t| t.kind() == SyntaxKind::COMMA)
        .count();
    assert_eq!(comma_count, 1, "SdblTupleExpr for (1, 2) must have 1 COMMA direct token child");
}

#[test]
fn test_slice10a_newline_separated_logical_and() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ А\nИ\nБ";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Newline-separated AND should parse: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input including newlines");

    let and_with_kw = root
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::SDBL_LOGICAL_AND_EXPR)
        .find(|n| {
            n.children_with_tokens()
                .filter_map(|c| c.into_token())
                .any(|t| t.kind() == SyntaxKind::KW_AND)
        })
        .expect(
            "SdblLogicalAndExpr with a DIRECT KW_AND token child even when separated by newlines",
        );

    let operand_node_count = and_with_kw.children().count();
    assert!(
        operand_node_count >= 2,
        "SdblLogicalAndExpr must have at least 2 operand subtrees as direct children; got {operand_node_count}",
    );

    let and_text = and_with_kw.text().to_string();
    assert!(
        and_text.contains('А') && and_text.contains('Б'),
        "SdblLogicalAndExpr text must cover both operands (got {and_text:?})",
    );
}

#[test]
fn test_slice10a_bare_null_emits_literal_not_column_ref() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ Поле = NULL";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "WHERE …= NULL should parse: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input, "Root must cover full input");

    let null_token = root
        .descendants_with_tokens()
        .filter_map(|c| c.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT && t.text().eq_ignore_ascii_case("NULL"))
        .expect("NULL Ident token must be present");

    let null_parent_kind = null_token.parent().map(|p| p.kind());
    assert_eq!(
        null_parent_kind,
        Some(SyntaxKind::SDBL_LITERAL),
        "Bare NULL must emit SdblLiteral wrapping the Ident token; got parent {null_parent_kind:?}. Pre-Slice-10a-C2 bug placed NULL inside SdblColumnRef.",
    );

    let column_refs_with_null = root
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::SDBL_COLUMN_REF)
        .filter(|n| n.text().to_string().to_uppercase().contains("NULL"))
        .count();
    assert_eq!(
        column_refs_with_null, 0,
        "No SdblColumnRef may contain the NULL token; got {column_refs_with_null} occurrences.",
    );
}

#[test]
fn test_slice10a_select_field_null_emits_literal() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ NULL ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "SELECT NULL should parse: {:?}", parse.errors());
    let root = parse.syntax_node();
    assert_eq!(root.text().to_string(), input);

    let null_token = root
        .descendants_with_tokens()
        .filter_map(|c| c.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT && t.text().eq_ignore_ascii_case("NULL"))
        .expect("NULL Ident token must be present");
    assert_eq!(
        null_token.parent().map(|p| p.kind()),
        Some(SyntaxKind::SDBL_LITERAL),
        "Bare NULL at SELECT-field position must emit SdblLiteral",
    );
}

#[test]
fn test_slice10b_empty_in_list_recovery() {
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ Поле В ()";
    let parse = parse_sdbl(input);
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_IN_EXPR"),
        "Empty `IN ()` must still emit SdblInExpr (recoverable parse).\nTree: {}",
        tree
    );
    assert!(
        tree.contains("SDBL_WHERE_CLAUSE"),
        "WHERE clause must be parsed despite empty IN.\nTree: {}",
        tree
    );
}

#[test]
fn test_slice10b_not_in_subquery() {
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ Поле НЕ В (ВЫБРАТЬ Х ИЗ С)";
    let parse = parse_sdbl(input);
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_IN_EXPR"),
        "NOT IN with subquery must emit SdblInExpr.\nTree: {}",
        tree
    );
    assert!(
        tree.contains("SDBL_SUBQUERY"),
        "IN-subquery must produce SdblSubquery inside the parens.\nTree: {}",
        tree
    );
}

#[test]
fn test_slice10b_in_hierarchy_russian() {
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ Поле В ИЕРАРХИИ (&Корень)";
    let parse = parse_sdbl(input);
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_IN_HIERARCHY_EXPR"),
        "В ИЕРАРХИИ must emit SdblInHierarchyExpr.\nTree: {}",
        tree
    );
}

#[test]
fn test_slice10b_is_not_null_russian() {
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ Поле ЕСТЬ НЕ NULL";
    let parse = parse_sdbl(input);
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_IS_NULL_EXPR"),
        "ЕСТЬ НЕ NULL must emit SdblIsNullExpr.\nTree: {}",
        tree
    );
}

#[test]
fn test_slice10b_between_missing_and_recovery() {
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ Поле МЕЖДУ 1";
    let parse = parse_sdbl(input);
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_BETWEEN_EXPR"),
        "МЕЖДУ without AND must still emit SdblBetweenExpr (recovery).\nTree: {}",
        tree
    );
}

#[test]
fn test_slice10b_like_with_escape() {
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ Поле ПОДОБНО \"abc%\" СПЕЦСИМВОЛ \"!\"";
    let parse = parse_sdbl(input);
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_LIKE_EXPR"),
        "ПОДОБНО ... СПЕЦСИМВОЛ must emit SdblLikeExpr.\nTree: {}",
        tree
    );
}

#[test]
fn test_slice10b_refs_mdo_chain_russian() {
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ Регистратор ССЫЛКА Документ.ПриходнаяНакладная";
    let parse = parse_sdbl(input);
    assert!(
        !parse.has_errors(),
        "REFS with MDO chain must parse without errors: {:?}",
        parse.errors()
    );
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_REFS_EXPR"),
        "ССЫЛКА Документ.ПриходнаяНакладная must emit SdblRefsExpr.\nTree: {}",
        tree
    );
}

#[test]
fn test_slice10b_case_simple_form_operand_first() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ ВЫБОР Т.Х КОГДА 1 ТОГДА \"А\" ИНАЧЕ \"Б\" КОНЕЦ ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Simple CASE must parse: {:?}", parse.errors());

    let case = parse
        .syntax_node()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_CASE_EXPR)
        .expect("Tree must contain SdblCaseExpr");

    let first_child_kind = case.children().next().map(|n| n.kind());
    assert_ne!(
        first_child_kind,
        Some(SyntaxKind::SDBL_WHEN_CLAUSE),
        "Simple CASE first child node must be the operand expression, not SdblWhenClause. Got: {:?}",
        first_child_kind
    );
}

#[test]
fn test_slice10b_case_searched_form_when_first() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ ВЫБОР КОГДА Т.Х = 1 ТОГДА \"А\" КОНЕЦ ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "Searched CASE must parse: {:?}", parse.errors());

    let case = parse
        .syntax_node()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_CASE_EXPR)
        .expect("Tree must contain SdblCaseExpr");

    let first_child_kind = case.children().next().map(|n| n.kind());
    assert_eq!(
        first_child_kind,
        Some(SyntaxKind::SDBL_WHEN_CLAUSE),
        "Searched CASE first child node must be SdblWhenClause (no operand). Got: {:?}",
        first_child_kind
    );
}

#[test]
fn test_slice10b_cast_primitive_parameterised() {
    let input = "ВЫБРАТЬ ВЫРАЗИТЬ(Поле КАК СТРОКА(200)) ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "CAST(... AS STRING(200)) must parse: {:?}", parse.errors());
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_FUNCTION_CALL"),
        "CAST is dispatched as SdblFunctionCall.\nTree: {}",
        tree
    );
    assert!(tree.contains("SDBL_TYPE"), "CAST type spec must emit SdblType.\nTree: {}", tree);
}

#[test]
fn test_slice10b_cast_mdo_with_member_access() {
    let input = "ВЫБРАТЬ ВЫРАЗИТЬ(Регистратор КАК Документ.ПриходнаяНакладная).Поставщик ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "CAST(MDO).Поле must parse: {:?}", parse.errors());
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_FUNCTION_CALL"),
        "CAST is dispatched as SdblFunctionCall.\nTree: {}",
        tree
    );
    assert!(tree.contains("SDBL_TYPE"), "MDO CAST type must emit SdblType.\nTree: {}", tree);
    assert!(
        tree.contains("Поставщик"),
        "Member access on CAST result must be preserved in the tree.\nTree: {}",
        tree
    );
}

#[test]
fn test_slice10b_inline_tabular_field_syntax() {
    let input = "ВЫБРАТЬ Т.ТабЧасть.(Поле1, Поле2) ИЗ Т";
    let parse = parse_sdbl(input);
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_INLINE_TABLE_FIELDS"),
        "Inline tabular fields must emit SdblInlineTableFields.\nTree: {}",
        tree
    );
    assert!(
        tree.contains("SDBL_SELECTED_FIELD"),
        "Inline tabular fields must wrap SdblSelectedField children.\nTree: {}",
        tree
    );
}

#[test]
fn test_func_call_clause_keyword_recovery() {
    use syntax::SyntaxKind;
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

    let func_call = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_FUNCTION_CALL)
        .expect("Tree must contain SdblFunctionCall");
    let func_text = func_call.text().to_string();
    assert!(
        !func_text.to_uppercase().contains("FROM"),
        "Function call must NOT consume FROM as an argument: got `{}`",
        func_text
    );
}

#[test]
fn test_russian_func_call_clause_keyword_recovery() {
    use syntax::SyntaxKind;
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

    let func_call = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_FUNCTION_CALL)
        .expect("Tree must contain SdblFunctionCall");
    let func_text = func_call.text().to_string();
    assert!(
        !func_text.to_uppercase().contains("ИЗ"),
        "Function call must NOT consume ИЗ as an argument: got `{}`",
        func_text
    );
}

fn first_selected_field_direct_child_kinds(input: &str) -> Vec<syntax::SyntaxKind> {
    use syntax::SyntaxKind;
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let field = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_SELECTED_FIELD)
        .expect("Tree must contain SdblSelectedField");
    field.children().map(|n| n.kind()).collect()
}

#[test]
fn test_select_field_comparison_descendant_guard() {
    use syntax::SyntaxKind;
    let kinds = first_selected_field_direct_child_kinds("ВЫБРАТЬ Поле = 1 ИЗ Т");
    assert!(
        kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "SelectedField must have SdblLogicalOrExpr as direct child. Got: {:?}",
        kinds
    );
    assert!(
        !kinds.contains(&SyntaxKind::SDBL_COMPARISON_EXPR),
        "SelectedField must NOT have bare SdblComparisonExpr — it must be wrapped in LogicalOrExpr. Got: {:?}",
        kinds
    );
}

#[test]
fn test_select_field_in_descendant_guard() {
    use syntax::SyntaxKind;
    let kinds = first_selected_field_direct_child_kinds("ВЫБРАТЬ Поле В (1, 2) ИЗ Т");
    assert!(
        kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "SelectedField must have SdblLogicalOrExpr as direct child. Got: {:?}",
        kinds
    );
    assert!(
        !kinds.contains(&SyntaxKind::SDBL_IN_EXPR),
        "SelectedField must NOT have bare SdblInExpr — it must be wrapped in LogicalOrExpr. Got: {:?}",
        kinds
    );
}

#[test]
fn test_select_field_between_descendant_guard() {
    use syntax::SyntaxKind;
    let kinds = first_selected_field_direct_child_kinds("ВЫБРАТЬ Поле МЕЖДУ 1 И 5 ИЗ Т");
    assert!(
        kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "SelectedField must have SdblLogicalOrExpr as direct child. Got: {:?}",
        kinds
    );
    assert!(
        !kinds.contains(&SyntaxKind::SDBL_BETWEEN_EXPR),
        "SelectedField must NOT have bare SdblBetweenExpr — it must be wrapped in LogicalOrExpr. Got: {:?}",
        kinds
    );
}

#[test]
fn test_select_field_is_null_descendant_guard() {
    use syntax::SyntaxKind;
    let kinds = first_selected_field_direct_child_kinds("ВЫБРАТЬ Поле ЕСТЬ NULL ИЗ Т");
    assert!(
        kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "SelectedField must have SdblLogicalOrExpr as direct child. Got: {:?}",
        kinds
    );
    assert!(
        !kinds.contains(&SyntaxKind::SDBL_IS_NULL_EXPR),
        "SelectedField must NOT have bare SdblIsNullExpr — it must be wrapped in LogicalOrExpr. Got: {:?}",
        kinds
    );
}

#[test]
fn test_select_field_case_descendant_guard() {
    use syntax::SyntaxKind;
    let kinds =
        first_selected_field_direct_child_kinds("ВЫБРАТЬ ВЫБОР КОГДА 1 = 1 ТОГДА \"А\" КОНЕЦ ИЗ Т");
    assert!(
        kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "SelectedField must have SdblLogicalOrExpr as direct child. Got: {:?}",
        kinds
    );
    assert!(
        !kinds.contains(&SyntaxKind::SDBL_CASE_EXPR),
        "SelectedField must NOT have bare SdblCaseExpr — it must be wrapped in LogicalOrExpr. Got: {:?}",
        kinds
    );
}

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

fn find_first_join_clause(input: &str) -> syntax::SyntaxNode {
    use syntax::SyntaxKind;
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    parse
        .syntax_node()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_JOIN_CLAUSE)
        .expect("Tree must contain SdblJoinClause")
}

#[test]
fn test_slice9_canonical_inner_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join_node =
        find_first_join_clause("ВЫБРАТЬ * ИЗ Т1 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А");
    let join = SdblJoinClause::cast(join_node).expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Inner);
    assert!(join.data_source().is_some(), "JOIN must carry a joined SdblDataSource child");
}

#[test]
fn test_slice9_canonical_left_outer_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join_node =
        find_first_join_clause("ВЫБРАТЬ * ИЗ Т1 ЛЕВОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А");
    let join = SdblJoinClause::cast(join_node).expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Left);
}

#[test]
fn test_slice9_canonical_right_outer_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join_node =
        find_first_join_clause("ВЫБРАТЬ * ИЗ Т1 ПРАВОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А");
    let join = SdblJoinClause::cast(join_node).expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Right);
}

#[test]
fn test_slice9_canonical_full_outer_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join_node =
        find_first_join_clause("ВЫБРАТЬ * ИЗ Т1 ПОЛНОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А");
    let join = SdblJoinClause::cast(join_node).expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Full);
}

#[test]
fn test_slice9_bare_full_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join_node = find_first_join_clause("ВЫБРАТЬ * ИЗ Т1 ПОЛНОЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А");
    let join = SdblJoinClause::cast(join_node).expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Full);
}

#[test]
fn test_slice9_bare_left_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join_node = find_first_join_clause("ВЫБРАТЬ * ИЗ Т1 ЛЕВОЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А");
    let join = SdblJoinClause::cast(join_node).expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Left);
}

#[test]
fn test_slice9_bare_join_en() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join_node = find_first_join_clause("SELECT * FROM T1 JOIN T2 ON T1.A = T2.A");
    let join = SdblJoinClause::cast(join_node).expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Inner);
}

#[test]
fn test_slice9_bare_join_ru() {
    use syntax::ast::{AstNode, JoinType, SdblJoinClause};
    let join_node = find_first_join_clause("ВЫБРАТЬ * ИЗ Т1 СОЕДИНЕНИЕ Т2 ПО Т1.А = Т2.А");
    let join = SdblJoinClause::cast(join_node).expect("must cast to SdblJoinClause");
    assert_eq!(join.join_type(), JoinType::Inner);
}

#[test]
fn test_slice9_chained_joins_same_source() {
    use syntax::ast::{AstNode, SdblQueryPackage};
    let input = "SELECT * FROM T1 JOIN T2 ON T1.A = T2.A JOIN T3 ON T1.B = T3.B";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let package = SdblQueryPackage::cast(root).expect("query package");
    let select_query = package.queries().next().expect("query");
    let main = select_query.subquery().and_then(|s| s.main_query()).expect("main query");
    let from = main.from_clause().expect("FROM clause");
    let t1_source = from.data_sources().next().expect("first data source");
    let join_count = t1_source.join_clauses().count();
    assert_eq!(
        join_count, 2,
        "Both chained JOINs must attach as direct children of T1's SdblDataSource",
    );
}

#[test]
fn test_slice9_nested_join_inside_join() {
    use syntax::ast::{AstNode, JoinType, SdblQueryPackage};
    let input = "SELECT * FROM T1 LEFT JOIN T2 JOIN T3 ON T2.B = T3.B ON T1.A = T2.A";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let package = SdblQueryPackage::cast(root).expect("query package");
    let select_query = package.queries().next().expect("query");
    let main = select_query.subquery().and_then(|s| s.main_query()).expect("main query");
    let from = main.from_clause().expect("FROM clause");
    let t1_source = from.data_sources().next().expect("T1 source");
    let outer_join = t1_source.join_clauses().next().expect("outer LEFT JOIN");
    assert_eq!(outer_join.join_type(), JoinType::Left);
    let t2_source = outer_join.data_source().expect("T2 source under outer JOIN");
    let inner_join =
        t2_source.join_clauses().next().expect("inner JOIN attached to T2's data source");
    assert_eq!(
        inner_join.join_type(),
        JoinType::Inner,
        "Inner bare JOIN must default to Inner via parent-tokens fallback over T2's data source",
    );
}

#[test]
fn test_slice9_from_subquery_with_join_ast_shape() {
    use syntax::ast::{AstNode, SdblQueryPackage};
    let input = "SELECT * FROM (SELECT * FROM T1) AS S LEFT JOIN T2 ON S.A = T2.A";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let package = SdblQueryPackage::cast(root).expect("query package");
    let select_query = package.queries().next().expect("query");
    let main = select_query.subquery().and_then(|s| s.main_query()).expect("main query");
    let from = main.from_clause().expect("FROM clause");
    let s_source = from.data_sources().next().expect("subquery data source");
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
fn test_slice9_from_virtual_table_with_join_ast_shape() {
    use syntax::ast::{AstNode, SdblQueryPackage};
    let input = "ВЫБРАТЬ * ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(&Дата) КАК Р \
                 ЛЕВОЕ СОЕДИНЕНИЕ Т2 ПО Р.Х = Т2.Х";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let package = SdblQueryPackage::cast(root).expect("query package");
    let select_query = package.queries().next().expect("query");
    let main = select_query.subquery().and_then(|s| s.main_query()).expect("main query");
    let from = main.from_clause().expect("FROM clause");
    let r_source = from.data_sources().next().expect("virtual-table data source");
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
fn test_slice9_or_in_on_ast_shape() {
    use syntax::SyntaxKind;
    let input = "SELECT * FROM T1 JOIN T2 ON T1.A = T2.A OR T1.B = T2.B";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let join = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_JOIN_CLAUSE)
        .expect("Tree must contain SdblJoinClause");
    let direct_kinds: Vec<SyntaxKind> = join.children().map(|c| c.kind()).collect();
    assert!(
        direct_kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "ON-condition must wrap in SdblLogicalOrExpr direct child of SdblJoinClause. Got: {:?}",
        direct_kinds,
    );
}

#[test]
fn test_slice9_missing_join_keyword_current_behavior() {
    use syntax::SyntaxKind;
    let parse = parse_sdbl("SELECT * FROM T1 LEFT T2 ON A = B");
    let root = parse.syntax_node();
    let join = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_JOIN_CLAUSE)
        .expect("SdblJoinClause marker must still be completed on missing JOIN keyword");
    let error_children: Vec<_> =
        join.children().filter(|c| c.kind() == SyntaxKind::ERROR).collect();
    assert_eq!(
        error_children.len(),
        1,
        "Current behavior: exactly one ERROR node attaches as direct child of SdblJoinClause",
    );
    let error_text = error_children[0].text().to_string();
    assert!(
        error_text.contains("T2"),
        "Current behavior: `p.error()` BUMPS T2 into the ERROR node. Got: `{}`",
        error_text,
    );
}

#[test]
fn test_slice9_missing_on_current_behavior() {
    use syntax::SyntaxKind;
    let parse = parse_sdbl("SELECT * FROM T1 JOIN T2 A = B");
    let root = parse.syntax_node();
    let join = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_JOIN_CLAUSE)
        .expect("SdblJoinClause marker must still be completed on missing ON keyword");
    let error_children: Vec<_> =
        join.children().filter(|c| c.kind() == SyntaxKind::ERROR).collect();
    assert_eq!(
        error_children.len(),
        1,
        "Current behavior: exactly one ERROR node attaches as direct child of SdblJoinClause",
    );
    let error_text = error_children[0].text().to_string();
    assert!(
        error_text.contains('='),
        "Current behavior: `p.error()` BUMPS the `=` token (after T2's alias `A` was consumed) into the ERROR node. Got: `{}`",
        error_text,
    );
}

fn count_kw_or_excluding_subqueries(node: &syntax::SyntaxNode) -> usize {
    use syntax::{NodeOrToken, SyntaxKind};
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

#[test]
fn test_slice11_where_kw_or_recursive_walk_reachable() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ A = 1 ИЛИ B = 2";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let where_clause = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE)
        .expect("Tree must contain SdblWhereClause");

    let direct_kw_or: Vec<_> = where_clause
        .children_with_tokens()
        .filter_map(|c| c.as_token().filter(|t| t.kind() == SyntaxKind::KW_OR).cloned())
        .collect();
    assert_eq!(
        direct_kw_or.len(),
        0,
        "KW_OR must NOT be a direct token child of SdblWhereClause — \
         it sits inside the SdblLogicalOrExpr wrapper. Got direct KW_OR tokens: {:?}",
        direct_kw_or,
    );

    let recursive_count = count_kw_or_excluding_subqueries(&where_clause);
    assert_eq!(
        recursive_count, 1,
        "Recursive walk (mirroring \
         collect_or_tokens_excluding_subqueries) must find exactly one \
         KW_OR token reachable from SdblWhereClause through non-subquery \
         descendants. Got: {}",
        recursive_count,
    );
}

#[test]
fn test_slice11_where_recursive_walk_skips_subquery() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Т ГДЕ A В (ВЫБРАТЬ X ИЗ С ГДЕ X = 1 ИЛИ X = 2)";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();

    let where_clauses: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE).collect();
    assert_eq!(
        where_clauses.len(),
        2,
        "Expected outer + inner SdblWhereClause; got {}",
        where_clauses.len(),
    );

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

    let outer_count = count_kw_or_excluding_subqueries(outer);
    let inner_count = count_kw_or_excluding_subqueries(inner);
    assert_eq!(
        outer_count, 0,
        "Outer SdblWhereClause recursive walk (skipping subquery kinds) \
         must find zero KW_OR — the only OR is inside the subquery. Got: {}",
        outer_count,
    );
    assert_eq!(
        inner_count, 1,
        "Inner subquery's SdblWhereClause recursive walk must find \
         exactly one KW_OR (the inner OR). Got: {}",
        inner_count,
    );
}

fn assert_bare_keyword_clause(clause: &syntax::SyntaxNode, expected_keyword: &str) {
    use syntax::{NodeOrToken, SyntaxKind};
    assert_eq!(
        clause.children().count(),
        0,
        "Bare-keyword shape: `{}` clause must have zero direct child \
         nodes; got: {:?}",
        expected_keyword,
        clause.children().map(|c| c.kind()).collect::<Vec<_>>(),
    );
    let non_trivia_tokens: Vec<(SyntaxKind, String)> = clause
        .children_with_tokens()
        .filter_map(|c| match c {
            NodeOrToken::Token(t) if !t.kind().is_trivia() => {
                Some((t.kind(), t.text().to_string()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        non_trivia_tokens.len(),
        1,
        "Bare-keyword shape: `{}` clause must contain exactly one \
         non-trivia token (the leading keyword). Got: {:?}",
        expected_keyword,
        non_trivia_tokens,
    );
    let leading = &non_trivia_tokens[0].1.to_uppercase();
    assert_eq!(
        leading,
        &expected_keyword.to_uppercase(),
        "Bare-keyword shape: leading keyword text mismatch",
    );
    assert!(
        clause.descendants().all(|n| n == *clause || n.kind() != SyntaxKind::ERROR),
        "Bare-keyword shape: `{}` clause must contain no ERROR descendants",
        expected_keyword,
    );
}

#[test]
fn test_slice11_group_missing_by_recovery() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т СГРУППИРОВАТЬ A";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let group = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_GROUP_CLAUSE)
        .expect("Tree must contain SdblGroupClause even when BY is missing");

    assert_bare_keyword_clause(&group, "СГРУППИРОВАТЬ");
}

#[test]
fn test_slice11_order_missing_by_recovery() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т УПОРЯДОЧИТЬ A";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let order = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_ORDER_CLAUSE)
        .expect("Tree must contain SdblOrderClause even when BY is missing");
    assert_bare_keyword_clause(&order, "УПОРЯДОЧИТЬ");
}

#[test]
fn test_slice11_index_missing_by_recovery() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т ИНДЕКСИРОВАТЬ A";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let index = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_INDEX_BY)
        .expect("Tree must contain SdblIndexBy even when BY is missing");
    assert_bare_keyword_clause(&index, "ИНДЕКСИРОВАТЬ");
}

#[test]
fn test_slice11_totals_missing_by_recovery() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т ИТОГИ A";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let totals = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TOTALS_BY)
        .expect("Tree must contain SdblTotalsBy even when BY is missing");

    let has_by_token = totals.children_with_tokens().any(|c| {
        c.as_token().is_some_and(|t| {
            let s = t.text().to_uppercase();
            s == "BY" || s == "ПО"
        })
    });
    assert!(
        !has_by_token,
        "Missing-BY recovery: SdblTotalsBy must NOT contain a BY/ПО token \
         when BY is absent in the input",
    );
    let pre_by_expr_kinds: Vec<_> = totals
        .children()
        .map(|c| c.kind())
        .filter(|k| {
            matches!(
                k,
                SyntaxKind::SDBL_LOGICAL_OR_EXPR
                    | SyntaxKind::SDBL_COLUMN_REF
                    | SyntaxKind::SDBL_FUNCTION_CALL,
            )
        })
        .collect();
    assert!(
        !pre_by_expr_kinds.is_empty(),
        "Pre-BY aggregate-expression loop must consume `A` BEFORE the \
         missing-BY check; SdblTotalsBy direct children should include \
         at least one expression kind. Got direct child kinds: {:?}",
        totals.children().map(|c| c.kind()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_slice11_order_by_hierarchy_consumed() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т УПОРЯДОЧИТЬ ПО A ИЕРАРХИЯ";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let order = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_ORDER_CLAUSE)
        .expect("Tree must contain SdblOrderClause");

    let has_hierarchy_token = order.children_with_tokens().any(|c| {
        c.as_token().is_some_and(|t| {
            let s = t.text().to_uppercase();
            s == "HIERARCHY" || s == "ИЕРАРХИЯ"
        })
    });
    assert!(
        has_hierarchy_token,
        "C2 fix: ИЕРАРХИЯ must be consumed by order_by_item as a flat \
         sibling token of SdblOrderClause. Direct children/tokens: {:?}",
        order
            .children_with_tokens()
            .map(|c| match c {
                syntax::NodeOrToken::Node(n) => format!("Node({:?})", n.kind()),
                syntax::NodeOrToken::Token(t) => format!("Token({:?}: {:?})", t.kind(), t.text()),
            })
            .collect::<Vec<_>>(),
    );
}

#[test]
fn test_slice11_order_by_flat_children_no_wrapper() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A, B ИЗ Т УПОРЯДОЧИТЬ ПО A ВОЗР, B УБЫВ";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let order = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_ORDER_CLAUSE)
        .expect("Tree must contain SdblOrderClause");

    let direct_expr_kinds: Vec<_> = order
        .children()
        .map(|c| c.kind())
        .filter(|k| {
            matches!(
                k,
                SyntaxKind::SDBL_LOGICAL_OR_EXPR
                    | SyntaxKind::SDBL_COLUMN_REF
                    | SyntaxKind::SDBL_FUNCTION_CALL,
            )
        })
        .collect();
    assert_eq!(
        direct_expr_kinds.len(),
        2,
        "Two flat direct expression children expected (no per-item \
         wrapper). Got: {:?}",
        direct_expr_kinds,
    );

    let direct_ident_texts: Vec<String> = order
        .children_with_tokens()
        .filter_map(|c| c.as_token().cloned())
        .filter(|t| t.kind() == SyntaxKind::IDENT)
        .map(|t| t.text().to_uppercase())
        .collect();
    assert!(
        direct_ident_texts.iter().any(|s| s == "ВОЗР" || s == "ASC"),
        "Direction token ВОЗР must be a flat IDENT direct token of \
         SdblOrderClause. Got direct IDENT texts: {:?}",
        direct_ident_texts,
    );
    assert!(
        direct_ident_texts.iter().any(|s| s == "УБЫВ" || s == "DESC"),
        "Direction token УБЫВ must be a flat IDENT direct token of \
         SdblOrderClause. Got direct IDENT texts: {:?}",
        direct_ident_texts,
    );

    let unexpected_node_kinds: Vec<_> = order
        .children()
        .map(|c| c.kind())
        .filter(|k| {
            !matches!(
                k,
                SyntaxKind::SDBL_LOGICAL_OR_EXPR
                    | SyntaxKind::SDBL_COLUMN_REF
                    | SyntaxKind::SDBL_FUNCTION_CALL,
            )
        })
        .collect();
    assert!(
        unexpected_node_kinds.is_empty(),
        "Flat-children invariant: SdblOrderClause must have only \
         expression-node direct children — no per-item wrapper node. \
         Got unexpected direct node kinds: {:?}",
        unexpected_node_kinds,
    );
}

#[test]
fn test_slice11_having_logical_expression_wrapping() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т СГРУППИРОВАТЬ ПО A ИМЕЮЩИЕ A > 0";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let having = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_HAVING_CLAUSE)
        .expect("Tree must contain SdblHavingClause");

    let direct_expr_kinds: Vec<_> = having.children().map(|c| c.kind()).collect();
    assert!(
        direct_expr_kinds.contains(&SyntaxKind::SDBL_LOGICAL_OR_EXPR),
        "HAVING calls expression(p) but Slice 10a wraps the result in \
         SdblLogicalOrExpr — the consumer-side wrapper kind must still \
         appear as a direct child of SdblHavingClause. Got: {:?}",
        direct_expr_kinds,
    );
}

#[test]
fn test_slice11_for_without_update_recovery() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т ДЛЯ Справочник.X";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let for_update = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_FOR_UPDATE)
        .expect("Tree must contain SdblForUpdate even when UPDATE keyword is missing");

    let has_update_token = for_update.children_with_tokens().any(|c| {
        c.as_token().is_some_and(|t| {
            let s = t.text().to_uppercase();
            s == "UPDATE" || s == "ИЗМЕНЕНИЯ"
        })
    });
    assert!(
        !has_update_token,
        "Missing-UPDATE recovery: SdblForUpdate must NOT contain an \
         UPDATE/ИЗМЕНЕНИЯ token when input has just `ДЛЯ <MDO>`",
    );
}

#[test]
fn test_slice11_for_update_deep_mdo_chain() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т ДЛЯ ИЗМЕНЕНИЯ Справочник.X.Y.Z";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let for_update = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_FOR_UPDATE)
        .expect("Tree must contain SdblForUpdate");

    let dot_count = for_update
        .children_with_tokens()
        .filter(|c| c.as_token().is_some_and(|t| t.text() == "."))
        .count();
    assert_eq!(
        dot_count, 3,
        "Greedy MDO chain expected to flatten `Справочник.X.Y.Z` into \
         3 Dot tokens at SdblForUpdate's direct-child level. Got: {}",
        dot_count,
    );
}

#[test]
fn test_slice11_totals_overall_fallthrough_shape() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ СУММА(A) ИЗ Т ИТОГИ ПО ОБЩИЕ";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let totals = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TOTALS_BY)
        .expect("Tree must contain SdblTotalsBy");

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
        "OVERALL/ОБЩИЕ must fall through is_expression_start and be \
         consumed as a bare-expression direct child of SdblTotalsBy. \
         Direct child kinds: {:?}",
        totals.children().map(|c| c.kind()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_slice11_tail_any_order_no_cross_query_leak() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т1 ИТОГИ ПО A АВТОУПОРЯДОЧИВАНИЕ; \
                 SELECT B FROM T2 ORDER BY B; \
                 SELECT C FROM T3 АВТОУПОРЯДОЧИВАНИЕ";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();

    let all_select_queries: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY).collect();
    let top_level_queries: Vec<_> = all_select_queries
        .iter()
        .filter(|q| !q.ancestors().skip(1).any(|a| a.kind() == SyntaxKind::SDBL_SELECT_QUERY))
        .cloned()
        .collect();
    assert_eq!(
        top_level_queries.len(),
        3,
        "Three top-level SdblSelectQuery nodes expected (one per `;` \
         segment, not nested inside another query); got {} top-level \
         out of {} total SdblSelectQuery nodes",
        top_level_queries.len(),
        all_select_queries.len(),
    );
    for q in &all_select_queries {
        let nested_count =
            q.descendants().filter(|d| d != q && d.kind() == SyntaxKind::SDBL_SELECT_QUERY).count();
        assert_eq!(
            nested_count,
            0,
            "SdblSelectQuery at offset {:?} must NOT contain another \
             SdblSelectQuery as a descendant (semicolon boundary \
             violation). Nested count: {}",
            q.text_range().start(),
            nested_count,
        );
    }

    let q_t1 = top_level_queries
        .iter()
        .find(|q| {
            let s = q.text().to_string();
            s.contains("Т1") && !s.contains("T2") && !s.contains("T3")
        })
        .expect(
            "Query 1 (Т1) top-level SdblSelectQuery must exist and not span across `;` boundaries",
        );
    let q_t2 = top_level_queries
        .iter()
        .find(|q| {
            let s = q.text().to_string();
            s.contains("T2") && !s.contains("Т1") && !s.contains("T3")
        })
        .expect(
            "Query 2 (T2) top-level SdblSelectQuery must exist and not span across `;` boundaries",
        );
    let q_t3 = top_level_queries
        .iter()
        .find(|q| {
            let s = q.text().to_string();
            s.contains("T3") && !s.contains("T2") && !s.contains("Т1")
        })
        .expect(
            "Query 3 (T3) top-level SdblSelectQuery must exist and not span across `;` boundaries",
        );

    fn owners_or_fail(
        root: &syntax::SyntaxNode,
        kind: SyntaxKind,
        kind_label: &str,
    ) -> Vec<syntax::SyntaxNode> {
        let nodes: Vec<_> = root.descendants().filter(|n| n.kind() == kind).collect();
        nodes
            .iter()
            .map(|n| {
                let ancestor = n.ancestors().find(|a| a.kind() == SyntaxKind::SDBL_SELECT_QUERY);
                assert!(
                    ancestor.is_some(),
                    "{} node at offset {:?} has no SdblSelectQuery \
                     ancestor — that is a cross-query leak at the \
                     package/root level. Node text: `{}`",
                    kind_label,
                    n.text_range().start(),
                    n.text(),
                );
                ancestor.unwrap()
            })
            .collect()
    }

    fn ranges_eq(a: &syntax::SyntaxNode, b: &syntax::SyntaxNode) -> bool {
        a.text_range() == b.text_range()
    }

    let totals_owners = owners_or_fail(&root, SyntaxKind::SDBL_TOTALS_BY, "SdblTotalsBy");
    assert_eq!(
        totals_owners.len(),
        1,
        "Exactly one SdblTotalsBy node expected across the package; got {}",
        totals_owners.len(),
    );
    assert!(
        ranges_eq(&totals_owners[0], q_t1),
        "The single SdblTotalsBy must attach inside the Т1 query \
         (matched by text_range identity, not substring). Got \
         owner range {:?}, expected Т1 range {:?}",
        totals_owners[0].text_range(),
        q_t1.text_range(),
    );

    let order_owners = owners_or_fail(&root, SyntaxKind::SDBL_ORDER_CLAUSE, "SdblOrderClause");
    assert_eq!(
        order_owners.len(),
        1,
        "Exactly one SdblOrderClause node expected across the package; got {}",
        order_owners.len(),
    );
    assert!(
        ranges_eq(&order_owners[0], q_t2),
        "The single SdblOrderClause must attach inside the T2 query \
         (matched by text_range identity). Got owner range {:?}, \
         expected T2 range {:?}",
        order_owners[0].text_range(),
        q_t2.text_range(),
    );

    let autoorder_owners = owners_or_fail(&root, SyntaxKind::SDBL_AUTOORDER, "SdblAutoorder");
    assert_eq!(
        autoorder_owners.len(),
        2,
        "Exactly two SdblAutoorder nodes expected (Т1 + T3); got {}",
        autoorder_owners.len(),
    );
    let t1_count = autoorder_owners.iter().filter(|o| ranges_eq(o, q_t1)).count();
    let t3_count = autoorder_owners.iter().filter(|o| ranges_eq(o, q_t3)).count();
    assert_eq!(
        t1_count, 1,
        "Exactly one SdblAutoorder must attach inside the Т1 query \
         (any-order after TOTALS) — matched by text_range identity",
    );
    assert_eq!(
        t3_count, 1,
        "Exactly one SdblAutoorder must attach inside the T3 query \
         — matched by text_range identity",
    );
}

#[test]
fn test_slice11_is_clause_keyword_join_delegation() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ A ИЗ Т1 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Т2 ПО Т1.X = Т2.Y ГДЕ Т1.X > 0";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();

    let join_clauses: Vec<_> =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_JOIN_CLAUSE).collect();
    assert_eq!(
        join_clauses.len(),
        1,
        "is_join_keyword delegation must let alias scan terminate at \
         ВНУТРЕННЕЕ so the JOIN clause forms; expected exactly one \
         SdblJoinClause, got {}",
        join_clauses.len(),
    );

    let first_data_source = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_DATA_SOURCE)
        .expect("Tree must contain SdblDataSource for Т1");
    let has_alias = first_data_source.children().any(|c| c.kind() == SyntaxKind::SDBL_ALIAS);
    assert!(
        !has_alias,
        "is_join_keyword delegation must terminate Т1's alias scan \
         at ВНУТРЕННЕЕ — first SdblDataSource must have NO SdblAlias \
         direct child. Direct children: {:?}",
        first_data_source.children().map(|c| c.kind()).collect::<Vec<_>>(),
    );

    let has_where = root.descendants().any(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE);
    assert!(has_where, "SdblWhereClause must appear in the tree");
    let where_inside_join = root
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE)
        .any(|w| w.ancestors().skip(1).any(|a| a.kind() == SyntaxKind::SDBL_JOIN_CLAUSE));
    assert!(
        !where_inside_join,
        "is_clause_keyword must terminate JOIN parsing at ГДЕ — \
         SdblWhereClause must NOT be a descendant of SdblJoinClause",
    );
}

#[test]
fn test_slice7adn_distinct_canonical_ru() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ РАЗЛИЧНЫЕ A ИЗ Т";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let query = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_QUERY)
        .expect("Tree must contain SdblQuery");
    let limitations: Vec<_> =
        query.children().filter(|c| c.kind() == SyntaxKind::SDBL_LIMITATIONS).collect();
    assert_eq!(
        limitations.len(),
        1,
        "SdblQuery must have exactly one SdblLimitations direct child for \
         `ВЫБРАТЬ РАЗЛИЧНЫЕ A ИЗ Т`. Got: {:?}",
        query.children().map(|c| c.kind()).collect::<Vec<_>>(),
    );
    let kw_text: String = limitations[0]
        .children_with_tokens()
        .filter_map(|c| c.as_token().map(|t| t.text().to_string()))
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        kw_text.to_uppercase().contains("РАЗЛИЧНЫЕ"),
        "SdblLimitations must contain РАЗЛИЧНЫЕ token. Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_top_canonical_ru() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ ПЕРВЫЕ 3 A ИЗ Т";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let limitations = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_LIMITATIONS)
        .expect("Tree must contain SdblLimitations");
    let top_clauses: Vec<_> =
        limitations.children().filter(|c| c.kind() == SyntaxKind::SDBL_TOP_CLAUSE).collect();
    assert_eq!(
        top_clauses.len(),
        1,
        "SdblLimitations must have exactly one SdblTopClause direct child \
         for `ВЫБРАТЬ ПЕРВЫЕ 3 A ИЗ Т`. Got direct children: {:?}",
        limitations.children().map(|c| c.kind()).collect::<Vec<_>>(),
    );
    let token_text: Vec<(SyntaxKind, String)> = top_clauses[0]
        .children_with_tokens()
        .filter_map(|c| c.as_token().map(|t| (t.kind(), t.text().to_string())))
        .filter(|(k, _)| !k.is_trivia())
        .collect();
    let has_top_keyword = token_text
        .iter()
        .any(|(_, t)| t.to_uppercase().contains("ПЕРВЫЕ") || t.to_uppercase().contains("TOP"));
    let has_decimal = token_text.iter().any(|(k, t)| *k == SyntaxKind::DECIMAL && t == "3");
    assert!(
        has_top_keyword,
        "SdblTopClause must contain ПЕРВЫЕ/TOP keyword token. Got non-trivia tokens: {:?}",
        token_text,
    );
    assert!(
        has_decimal,
        "SdblTopClause must contain a Decimal `3` token. Got non-trivia tokens: {:?}",
        token_text,
    );
}

#[test]
fn test_slice7adn_allowed_canonical_ru() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ РАЗРЕШЕННЫЕ A ИЗ Т";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let query = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_QUERY)
        .expect("Tree must contain SdblQuery");
    let limitations: Vec<_> =
        query.children().filter(|c| c.kind() == SyntaxKind::SDBL_LIMITATIONS).collect();
    assert_eq!(
        limitations.len(),
        1,
        "SdblQuery must have exactly one SdblLimitations direct child for \
         `ВЫБРАТЬ РАЗРЕШЕННЫЕ A ИЗ Т`. Got: {:?}",
        query.children().map(|c| c.kind()).collect::<Vec<_>>(),
    );
    let kw_text: String = limitations[0]
        .children_with_tokens()
        .filter_map(|c| c.as_token().map(|t| t.text().to_string()))
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        kw_text.to_uppercase().contains("РАЗРЕШЕННЫЕ"),
        "SdblLimitations must contain РАЗРЕШЕННЫЕ token (canonical Tier A1 \
         per v8327doc Глава 8). Got tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_combined_any_order() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ ПЕРВЫЕ 5 РАЗРЕШЕННЫЕ РАЗЛИЧНЫЕ A ИЗ Т";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let query = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_QUERY)
        .expect("Tree must contain SdblQuery");
    let limitations: Vec<_> =
        query.children().filter(|c| c.kind() == SyntaxKind::SDBL_LIMITATIONS).collect();
    assert_eq!(
        limitations.len(),
        1,
        "SdblQuery must have exactly one SdblLimitations wrapper for the \
         any-order combined input. Got direct children: {:?}",
        query.children().map(|c| c.kind()).collect::<Vec<_>>(),
    );
    let top_count =
        limitations[0].children().filter(|c| c.kind() == SyntaxKind::SDBL_TOP_CLAUSE).count();
    assert_eq!(
        top_count, 1,
        "SdblLimitations must have exactly one SdblTopClause direct child \
         (the `ПЕРВЫЕ 5` qualifier). Got: {}",
        top_count,
    );
    let kw_text: String = limitations[0]
        .children_with_tokens()
        .filter_map(|c| c.as_token().map(|t| t.text().to_string()))
        .collect::<Vec<_>>()
        .join("|")
        .to_uppercase();
    assert!(
        kw_text.contains("РАЗРЕШЕННЫЕ"),
        "SdblLimitations must contain the РАЗРЕШЕННЫЕ keyword. Tokens: {}",
        kw_text,
    );
    assert!(
        kw_text.contains("РАЗЛИЧНЫЕ"),
        "SdblLimitations must contain the РАЗЛИЧНЫЕ keyword. Tokens: {}",
        kw_text,
    );
}

#[test]
fn test_slice7adn_top_missing_decimal_recovery() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ ПЕРВЫЕ A ИЗ Т";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let limitations = root.descendants().find(|n| n.kind() == SyntaxKind::SDBL_LIMITATIONS).expect(
        "SdblLimitations marker must still be completed when the \
             Decimal after ПЕРВЫЕ is missing — IDE-recovery allowance Q3",
    );
    let top_clauses: Vec<_> =
        limitations.children().filter(|c| c.kind() == SyntaxKind::SDBL_TOP_CLAUSE).collect();
    assert_eq!(
        top_clauses.len(),
        1,
        "SdblLimitations must still have exactly one SdblTopClause direct \
         child even when the Decimal is missing. Got: {}",
        top_clauses.len(),
    );
    let decimal_count = top_clauses[0]
        .children_with_tokens()
        .filter_map(|c| c.as_token().filter(|t| t.kind() == SyntaxKind::DECIMAL).cloned())
        .count();
    assert_eq!(
        decimal_count, 0,
        "SdblTopClause must have NO Decimal token child when the count is \
         missing — Q3 preserved-parser-support recovery shape (Parser::expect \
         calls Parser::error which bumps the next token into ERROR sub-node).",
    );
    let error_children: Vec<_> =
        top_clauses[0].children().filter(|c| c.kind() == SyntaxKind::ERROR).collect();
    assert_eq!(
        error_children.len(),
        1,
        "SdblTopClause must have exactly one ERROR sub-node child (the \
         bumped `A` Ident) per Parser::error() recovery contract. Got: {}",
        error_children.len(),
    );
    let error_text = error_children[0].text().to_string();
    assert!(
        error_text.contains('A'),
        "ERROR sub-node must contain the bumped `A` Ident. Got text: {:?}",
        error_text,
    );
    let from_clauses_count =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count();
    assert_eq!(
        from_clauses_count, 1,
        "The clause after a failed count must survive: an empty selection \
         list no longer takes the next clause's keyword as its only field. \
         Got: {} SdblFromClause nodes.",
        from_clauses_count,
    );
}

#[test]
fn test_top_without_any_count_leaves_the_next_clause_alone() {
    use syntax::SyntaxKind;
    // Nothing at all where the count belongs: the complaint is made
    // without moving, so `ИЗ` is still there to be recognised.
    let parse = parse_sdbl("ВЫБРАТЬ ПЕРВЫЕ ИЗ Т");
    assert!(parse.has_errors(), "a missing count must still be reported");
    assert_eq!(
        parse
            .syntax_node()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE)
            .count(),
        1,
    );
}

#[test]
fn test_slice8adn_gap_empty_paren_pair() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Регистр.Остатки() КАК Т";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let table_ref = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    let missing = table_ref.children().filter(|c| c.kind() == SyntaxKind::SDBL_MISSING_ARG).count();
    assert_eq!(
        missing, 0,
        "Empty `()` must NOT emit any SdblMissingArg under SdblTableRef \
         (mini-spec §IDE-recovery allowance #4 — outer `if !p.at(RParen)` \
         skip). Got: {}",
        missing,
    );
    let lparen = table_ref
        .children_with_tokens()
        .filter_map(|c| c.as_token().filter(|t| t.kind() == SyntaxKind::L_PAREN).cloned())
        .count();
    let rparen = table_ref
        .children_with_tokens()
        .filter_map(|c| c.as_token().filter(|t| t.kind() == SyntaxKind::R_PAREN).cloned())
        .count();
    assert_eq!(
        lparen, 1,
        "Expected exactly one L_PAREN token child of SdblTableRef. Got: {}",
        lparen
    );
    assert_eq!(
        rparen, 1,
        "Expected exactly one R_PAREN token child of SdblTableRef. Got: {}",
        rparen
    );
    let errors = table_ref.children().filter(|c| c.kind() == SyntaxKind::ERROR).count();
    assert_eq!(errors, 0, "Empty `()` must not emit any ERROR direct child. Got: {}", errors);
}

#[test]
fn test_slice8adn_gap_single_trailing_comma() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Регистр.Остатки(&Период,) КАК Т";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let table_ref = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    let missing = table_ref.children().filter(|c| c.kind() == SyntaxKind::SDBL_MISSING_ARG).count();
    assert_eq!(
        missing, 1,
        "`(&Период,)` must emit exactly one SdblMissingArg direct child of \
         SdblTableRef (mini-spec §IDE-recovery allowance #2 — \
         empty-trailing-arg after the last comma). Got: {}",
        missing,
    );
    let comma = table_ref
        .children_with_tokens()
        .filter_map(|c| c.as_token().filter(|t| t.kind() == SyntaxKind::COMMA).cloned())
        .count();
    assert_eq!(comma, 1, "Expected exactly one COMMA token child. Got: {}", comma);
    let errors = table_ref.children().filter(|c| c.kind() == SyntaxKind::ERROR).count();
    assert_eq!(errors, 0, "Clean trailing-comma form must not emit ERROR. Got: {}", errors);
}

#[test]
fn test_slice8adn_gap_canonical_v8327doc_5arg() {
    use syntax::SyntaxKind;
    let input =
        "ВЫБРАТЬ * ИЗ РегистрНакопления.УчетНоменклатуры.ОстаткиИОбороты(, , Авто, , ) КАК Т";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let table_ref = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    let missing = table_ref.children().filter(|c| c.kind() == SyntaxKind::SDBL_MISSING_ARG).count();
    assert_eq!(
        missing, 4,
        "Canonical v8327doc shape `(, , Авто, , )` must produce exactly \
         four SdblMissingArg direct children of SdblTableRef \
         (allowance #1 + #3: 2 empty-leading + 2 empty-trailing slots \
         around the single `Авто` Ident). Got: {}",
        missing,
    );
    let comma = table_ref
        .children_with_tokens()
        .filter_map(|c| c.as_token().filter(|t| t.kind() == SyntaxKind::COMMA).cloned())
        .count();
    assert_eq!(
        comma, 4,
        "Canonical 5-arg shape must have exactly 4 COMMA token children. Got: {}",
        comma
    );
    let errors = table_ref.children().filter(|c| c.kind() == SyntaxKind::ERROR).count();
    assert_eq!(
        errors, 0,
        "Canonical v8327doc form must parse cleanly with zero ERROR children. Got: {}",
        errors
    );
}

#[test]
fn test_slice8adn_gap_paren_balanced_subquery_arg() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Регистр.Обороты(, &Конец, , Поле В (ВЫБРАТЬ X ИЗ Y)) КАК Т";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let table_ref = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    let errors = table_ref.children().filter(|c| c.kind() == SyntaxKind::ERROR).count();
    assert_eq!(
        errors, 0,
        "Clean IN-subquery `Поле В (ВЫБРАТЬ X ИЗ Y)` as a VT arg must NOT \
         trigger `recover_to_delimiter_vt` — the subquery's `)` is consumed \
         inside `expression(p)` / `predicate_expr` (Slice 10b territory). \
         Got: {} ERROR direct children of SdblTableRef.",
        errors,
    );
}

#[test]
fn test_slice8adn_gap_mid_arg_recovery() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Регистр.Остатки(СУММА(A) Q, B) КАК Т";
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let table_ref = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    let errors: Vec<_> = table_ref.children().filter(|c| c.kind() == SyntaxKind::ERROR).collect();
    assert!(
        !errors.is_empty(),
        "Mid-arg spurious-token form must trigger `recover_to_delimiter_vt` \
         and emit an ERROR direct child of SdblTableRef containing the \
         spurious `Q` token (mini-spec §IDE-recovery allowance #5). Got: 0 \
         ERROR direct children. SdblTableRef text: {:?}",
        table_ref.text().to_string(),
    );
    let absorbed_q = errors.iter().any(|e| e.text().to_string().contains('Q'));
    assert!(
        absorbed_q,
        "ERROR direct child of SdblTableRef must contain the spurious `Q` \
         token consumed by `recover_to_delimiter_vt`. ERROR texts: {:?}",
        errors.iter().map(|e| e.text().to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_slice8adn_gap_nested_function_call_arg() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Регистр.Остатки(СУММА(A)) КАК Т";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let table_ref = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    let errors = table_ref.children().filter(|c| c.kind() == SyntaxKind::ERROR).count();
    assert_eq!(
        errors, 0,
        "Clean nested call `СУММА(A)` as a VT arg must NOT trigger \
         `recover_to_delimiter_vt` (mini-spec §IDE-recovery allowance #5 \
         — the helper is a safety net for malformed input only). Got: {} \
         ERROR direct children of SdblTableRef.",
        errors,
    );
    let func_calls =
        table_ref.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FUNCTION_CALL).count();
    assert!(
        func_calls >= 1,
        "Expected at least one SdblFunctionCall node somewhere under \
         SdblTableRef for `СУММА(A)` (typically wrapped in the Slice 10a \
         expression backbone — a direct child of SdblTableRef would be \
         e.g. SdblLogicalOrExpr that descends to SdblFunctionCall). Got: \
         {} SdblFunctionCall descendants. SdblTableRef text: {:?}",
        func_calls,
        table_ref.text().to_string(),
    );
}

#[test]
fn test_slice8adn_gap_vt_args_then_clause_keyword() {
    use syntax::SyntaxKind;
    let input = "ВЫБРАТЬ * ИЗ Регистр.Остатки(&Дата) ГДЕ X = 1";
    let parse = parse_sdbl(input);
    assert_clean_parse(&parse, input);
    let root = parse.syntax_node();
    let table_ref = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SDBL_TABLE_REF)
        .expect("Tree must contain SdblTableRef");
    let where_inside_table_ref =
        table_ref.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE).count();
    assert_eq!(
        where_inside_table_ref, 0,
        "SdblWhereClause must NOT be nested inside SdblTableRef — the outer \
         `expect(RParen)` exit hands off to Slice 8 source_alias / Slice 11 \
         query_body_clauses cleanly. Got: {} SdblWhereClause descendants \
         of SdblTableRef.",
        where_inside_table_ref,
    );
    let where_in_tree =
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE).count();
    assert_eq!(
        where_in_tree, 1,
        "Expected exactly one SdblWhereClause in the tree (attached \
         outside SdblTableRef). Got: {}",
        where_in_tree,
    );
}

#[test]
fn test_column_dot_ssylka_is_field_name() {
    check_no_errors("SELECT Т.Ссылка FROM Справочник.Номенклатура AS Т");
}

#[test]
fn test_column_dot_summa_is_field_name() {
    check_no_errors("SELECT Т.Сумма FROM РегистрНакопления.Продажи AS Т");
}

#[test]
fn test_column_dot_v_operator_token_is_field_name() {
    check_no_errors("SELECT Т.В FROM Справочник.Номенклатура AS Т");
}

#[test]
fn test_column_dot_istina_literal_token_is_field_name() {
    check_no_errors("SELECT Т.Истина FROM Справочник.Номенклатура AS Т");
}

// SDBL keywords are not reserved as alias or field names. These mirror the bsl-ls
// behaviour and the ERP queries that previously produced false QueryParseError.
mod keyword_names {
    use super::*;

    #[test]
    fn source_alias_named_after_clause_keyword_parses_clean() {
        check_no_errors("ВЫБРАТЬ * ИЗ Товары КАК Итоги");
    }

    #[test]
    fn selected_field_alias_named_after_clause_keyword_parses_clean() {
        check_no_errors("ВЫБРАТЬ Поле КАК Итоги ИЗ Товары");
    }

    #[test]
    fn join_keyword_alias_parses_clean() {
        // Join keywords collide with aliases by text, and only `INNER` is reserved.
        check_no_errors("SELECT * FROM (SELECT 1) AS Outer");
        check_no_errors("SELECT * FROM (SELECT 1) AS Left");
        check_no_errors("SELECT * FROM (SELECT 1) AS Join");
    }

    #[test]
    fn field_named_after_clause_keyword_same_line_parses_clean() {
        check_no_errors("ВЫБРАТЬ Т.Итоги ИЗ Справочник.Номенклатура КАК Т");
    }

    #[test]
    fn field_named_after_expression_keyword_same_line_parses_clean() {
        // `Конец` (END), `Выбор` (CASE) are common field names, not reserved.
        check_no_errors("ВЫБРАТЬ Т.Начало КАК Начало, Т.Конец КАК Конец ИЗ ОстаткиВТ КАК Т");
        check_no_errors("ВЫБРАТЬ Т.Выбор ИЗ Справочник.Номенклатура КАК Т");
    }

    #[test]
    fn totals_clause_still_parses_with_aggregate() {
        // `ИТОГИ` must still drive the TOTALS clause; the implicit-alias path must
        // not swallow it as the source alias of `Т`.
        check_no_errors("ВЫБРАТЬ Поле КАК Поле ИЗ Т ИТОГИ СУММА(Поле) ПО Поле");
    }

    #[test]
    fn as_followed_by_body_clause_keyword_still_recovers() {
        // A primary clause after AS is an omitted alias, not an alias named WHERE.
        let parse = parse_sdbl("SELECT * FROM Products AS WHERE Active = TRUE");
        let t = format!("{:#?}", parse.syntax_node());
        assert!(t.contains("SDBL_WHERE_CLAUSE"), "WHERE must still parse, tree:\n{t}");
    }

    #[test]
    fn dangling_dot_before_clause_keyword_across_newline_recovers() {
        // A trailing dot before a clause keyword on the next line must not be glued
        // to the keyword as a field access.
        let parse = parse_sdbl("ВЫБРАТЬ Т.\nИЗ Товары");
        let t = format!("{:#?}", parse.syntax_node());
        assert!(parse.has_errors(), "dangling dot must report recovery, tree:\n{t}");
        assert!(t.contains("SDBL_FROM_CLAUSE"), "FROM must still parse, tree:\n{t}");
    }
}

#[test]
fn test_column_dot_kak_does_not_swallow_alias() {
    let input = "SELECT Т. КАК Алиас FROM Справочник.Номенклатура AS Т";
    let parse = parse_sdbl(input);

    let has_alias_node =
        parse.syntax_node().descendants().any(|n| n.kind() == syntax::SyntaxKind::SDBL_ALIAS);
    assert!(
        has_alias_node,
        "trailing-dot before КАК must preserve the SDBL_ALIAS node; tree:\n{:#?}",
        parse.syntax_node()
    );

    let col_ref = parse
        .syntax_node()
        .descendants()
        .find(|n| n.kind() == syntax::SyntaxKind::SDBL_COLUMN_REF)
        .expect("SdblColumnRef must exist");
    let col_text = col_ref.text().to_string();
    assert!(
        !col_text.contains("КАК"),
        "SdblColumnRef must not contain the alias keyword; got: {col_text:?}"
    );
}

#[test]
fn test_column_dot_vybor_case_keyword_does_not_swallow_case_frame() {
    let input = "SELECT Т., ВЫБОР КОГДА 1 = 1 ТОГДА 1 КОНЕЦ FROM Справочник.Номенклатура AS Т";
    let parse = parse_sdbl(input);

    let case_text = parse
        .syntax_node()
        .descendants()
        .find(|n| n.kind() == syntax::SyntaxKind::SDBL_CASE_EXPR)
        .map(|n| n.text().to_string());
    assert!(
        case_text.is_some_and(|t| t.contains("КОГДА") && t.contains("КОНЕЦ")),
        "trailing-dot before ВЫБОР must preserve the SdblCaseExpr frame; \
         tree:\n{:#?}",
        parse.syntax_node()
    );
}

// Query-language extension blocks `{...}` (customizable sections for dynamic
// lists / DCS) must not break parsing of the surrounding query.
#[test]
fn test_query_extension_where_block_no_errors() {
    check_no_errors(
        "ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Товары КАК Т \
         ГДЕ Т.Цена > 0 \
         {ГДЕ (Т.Номенклатура В (&Номенклатура))} \
         УПОРЯДОЧИТЬ ПО Т.Ссылка",
    );
}

#[test]
fn test_query_extension_select_block_no_errors() {
    check_no_errors(
        "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Справочник.Товары КАК Т \
         {ВЫБРАТЬ Т.Ссылка, Т.Наименование}",
    );
}

#[test]
fn test_query_extension_where_inside_join_subquery_no_errors() {
    check_no_errors(
        "ВЫБРАТЬ Зак.Ссылка \
         ИЗ Документ.Заказ КАК Зак \
         ЛЕВОЕ СОЕДИНЕНИЕ (\
            ВЫБРАТЬ Ост.Регистратор КАК Регистратор \
            ИЗ РегистрНакопления.Остатки КАК Ост \
            ГДЕ Ост.Тип = ЗНАЧЕНИЕ(Перечисление.Типы.А) \
            {ГДЕ (Ост.Организация В (&Организация))} \
            СГРУППИРОВАТЬ ПО Ост.Регистратор\
         ) КАК Итоги \
         ПО Зак.Ссылка = Итоги.Регистратор",
    );
}

#[test]
fn test_query_extension_nested_braces_no_errors() {
    check_no_errors("ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Товары КАК Т {ГДЕ {Вложенный} Т.Поле}");
}

#[test]
fn test_query_extension_trailing_order_block_no_errors() {
    check_no_errors(
        "ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Товары КАК Т \
         УПОРЯДОЧИТЬ ПО Т.Ссылка \
         {УПОРЯДОЧИТЬ ПО Т.Наименование}",
    );
}

#[test]
fn test_query_extension_produces_extension_node() {
    let parse = parse_sdbl("ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Товары КАК Т {ГДЕ Т.Поле}");
    assert!(!parse.has_errors(), "unexpected errors: {:#?}", parse.errors());
    let tree = format!("{:#?}", parse.syntax_node());
    assert!(
        tree.contains("SDBL_QUERY_EXTENSION"),
        "expected SDBL_QUERY_EXTENSION node, tree:\n{tree}"
    );
}

#[test]
fn test_query_extension_after_field_list_before_from_no_errors() {
    check_no_errors(
        "ВЫБРАТЬ Т.Поле1, Т.Поле2 \
         {ВЫБРАТЬ Т.Поле3, Т.Поле4} \
         ИЗ Справочник.Таблица КАК Т \
         ГДЕ Т.Поле1 > 0 \
         {ГДЕ Т.Поле2}",
    );
}

#[test]
fn test_query_extension_between_source_and_join_no_errors() {
    check_no_errors(
        "ВЫБРАТЬ Т.Поле \
         ИЗ Справочник.Т1 КАК Т \
         {ГДЕ Т.Поле} \
         ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Т2 КАК У \
         ПО Т.Поле = У.Поле",
    );
}

#[test]
fn test_query_extension_unbalanced_brace_is_tolerated() {
    // Unbalanced `{` (typically a runtime-concatenated query fragment such as
    // `"… {ВЫБРАТЬ" + Поля`) is consumed rather than reported, so it does not
    // produce a false QueryParseError on the surrounding code.
    let parse = parse_sdbl("ВЫБРАТЬ Т.Поле ИЗ Справочник.Т КАК Т {ГДЕ Т.Поле");
    assert!(!parse.has_errors(), "unbalanced extension brace should be tolerated");
}

#[test]
fn test_query_extension_inside_virtual_table_args_no_errors() {
    check_no_errors(
        "ВЫБРАТЬ Т.Номенклатура \
         ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(\
            &Период, \
            Номенклатура В (ВЫБРАТЬ С.Ссылка ИЗ Справочник.Номенклатура КАК С) \
            И &Отбор \
            {(Номенклатура).* КАК Ном, (Склад).*}\
         ) КАК Т \
         ГДЕ Т.КоличествоОстаток > 0",
    );
}

#[test]
fn test_query_extension_as_first_virtual_table_arg_no_errors() {
    check_no_errors(
        "ВЫБРАТЬ Т.Номенклатура \
         ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки({(Номенклатура).*}) КАК Т",
    );
}

// =====================================================================
// Coverage of the input by the syntax tree
//
// A Rowan tree is full-fidelity: its text is the source text. The SDBL
// entry point maintains that by draining whatever its rules refuse into
// a trailing error node, so nothing can leave the parser unaccounted
// for. `uncovered_tail` therefore answers the empty string for every
// input, well-formed or not, and the interesting question moves to
// whether the leftover was reported and what it contains.
// =====================================================================

fn uncovered_tail(input: &str) -> &str {
    let parse = parse_sdbl(input);
    let covered = usize::from(parse.syntax_node().text_range().len());
    &input[covered..]
}

/// Asserts that `input` is fully covered and that the part the grammar
/// could not place was reported rather than discarded.
fn assert_leftover_reported(input: &str, expected_leftover: &str) {
    assert_eq!(uncovered_tail(input), "", "nothing may leave the tree for `{input}`");

    let parse = parse_sdbl(input);
    let reported: Vec<_> = parse
        .errors()
        .iter()
        .map(|e| {
            let r = e.range();
            input[usize::from(r.start())..usize::from(r.end())].trim().to_string()
        })
        .collect();

    assert!(
        reported.iter().any(|text| text == expected_leftover),
        "expected `{expected_leftover}` to be reported for `{input}`; got {reported:?}",
    );
}

/// Asserts that `input` is fully covered and drew no complaint.
fn assert_accepted(input: &str) {
    assert_eq!(uncovered_tail(input), "", "nothing may leave the tree for `{input}`");
    let parse = parse_sdbl(input);
    assert!(!parse.has_errors(), "`{input}` must parse cleanly: {:#?}", parse.errors());
}

#[test]
fn test_trailing_tokens_after_a_query_are_reported() {
    assert_leftover_reported("ВЫБРАТЬ А ИЗ Т ФУНК(Х)", "(Х)");
    assert_leftover_reported("ВЫБРАТЬ А ИЗ Т ЫЫЫ ЭЭЭ", "ЭЭЭ");
    assert_leftover_reported("ВЫБРАТЬ А ИЗ Т 42", "42");
    assert_leftover_reported("ВЫБРАТЬ А ИЗ Т \"хвост\"", "\"хвост\"");
}

#[test]
fn test_a_bad_query_no_longer_swallows_the_next_one_in_the_package() {
    use syntax::SyntaxKind;
    // The shape that made the loss worth fixing: a following query, its
    // FROM clause and all, used to vanish without a word. Reporting the
    // leftover is only half of it — the leftover has to end at the
    // separator, or the next query is still lost to every consumer that
    // walks the package.
    let input = "ВЫБРАТЬ А ИЗ Т ГДЕ А = 1 ФУНК(Х); ВЫБРАТЬ 2 ИЗ У";
    assert_leftover_reported(input, "ФУНК(Х)");

    let root = parse_sdbl(input).syntax_node();
    assert_eq!(
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_QUERY).count(),
        2,
        "both queries must be in the tree as queries, not as error text",
    );
    assert_eq!(root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).count(), 2,);

    assert_accepted("ВЫБРАТЬ А ИЗ Т; ВЫБРАТЬ 2 ИЗ У");
}

#[test]
fn test_totals_periods_modifier_is_parsed() {
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ)");
    assert_accepted(
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(МИНУТА, ДАТАВРЕМЯ(2006,6,28), ДАТАВРЕМЯ(2006,6,28))",
    );
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н ИЕРАРХИЯ, П ПЕРИОДАМИ(ДЕНЬ)");
    // A modifier whose paren is not closed yet is a query mid-typing, and
    // is treated like the other half-typed clauses: taken, and quiet.
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ");
}

#[test]
fn test_totals_control_point_alias_is_parsed() {
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н КАК Группа");
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н Группа");
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н ИЕРАРХИЯ КАК Г");
}

#[test]
fn test_totals_hierarchy_modifiers_still_parse() {
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н ИЕРАРХИЯ");
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н ТОЛЬКО ИЕРАРХИЯ");
}

#[test]
fn test_overall_keeps_its_shape_and_loses_nothing() {
    // The reference rule writes `ПО ОБЩИЕ <список>` with no separator and
    // the textbook writes it with a comma. The comma form is the one that
    // occurs in practice and the one whose tree shape is settled; the
    // other now merely stops losing text, its reading left open.
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО ОБЩИЕ");
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО ОБЩИЕ, Н");
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО ОБЩИЕ Н");
}

#[test]
fn test_order_by_takes_all_four_documented_orderings() {
    assert_accepted("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н ВОЗР");
    assert_accepted("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н УБЫВ");
    assert_accepted("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н ИЕРАРХИЯ");
    assert_accepted("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н ИЕРАРХИЯ УБЫВ");
    // Not among the four, tolerated as a plausible slip.
    assert_accepted("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н УБЫВ ИЕРАРХИЯ");
}

#[test]
fn test_a_clause_out_of_order_is_reported_not_dropped() {
    assert_leftover_reported("ВЫБРАТЬ А ГДЕ А=1 ИЗ Т", "ИЗ Т");
    assert_leftover_reported("ВЫБРАТЬ А ИЗ Т СГРУППИРОВАТЬ ПО Н ГДЕ А=1", "ГДЕ А=1");
    assert_leftover_reported("ВЫБРАТЬ А ИЗ Т ИМЕЮЩИЕ А>0 СГРУППИРОВАТЬ ПО Н", "СГРУППИРОВАТЬ ПО Н");
    assert_leftover_reported("ВЫБРАТЬ А ИЗ Т ИНДЕКСИРОВАТЬ ПО Н ГДЕ А=1", "ГДЕ А=1");
    assert_leftover_reported("ВЫБРАТЬ А ИЗ Т ПОМЕСТИТЬ ВТ", "ПОМЕСТИТЬ ВТ");
}

#[test]
fn test_the_three_trailing_clauses_are_genuinely_order_free() {
    assert_accepted("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н ИТОГИ СУММА(А) ПО Н");
    assert_accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н УПОРЯДОЧИТЬ ПО Н");
    assert_accepted("ВЫБРАТЬ А ИЗ Т АВТОУПОРЯДОЧИВАНИЕ УПОРЯДОЧИТЬ ПО Н");
}

#[test]
fn test_a_clause_keyword_typed_without_its_body_is_accepted_in_silence() {
    // Deliberate: the words are typed before what must follow them, and
    // an error on every keystroke in between would be noise. The drain
    // must not turn these into diagnostics.
    for input in [
        "ВЫБРАТЬ А ИЗ Т СГРУППИРОВАТЬ",
        "ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ",
        "ВЫБРАТЬ А ИЗ Т ИНДЕКСИРОВАТЬ",
        "ВЫБРАТЬ А ИЗ Т ИТОГИ",
        "ВЫБРАТЬ А ИЗ Т ДЛЯ",
    ] {
        assert_accepted(input);
    }
}

#[test]
fn test_comments_are_trivia_and_cost_no_coverage() {
    assert_accepted("ВЫБРАТЬ А // хвост\nИЗ Т");
    assert_accepted("ВЫБРАТЬ А ИЗ Т // хвост");
    assert_accepted("// пусто");
}

#[test]
fn test_well_formed_queries_are_covered_completely() {
    for input in [
        "ВЫБРАТЬ * ИЗ Справочник.Товары",
        "ВЫБРАТЬ РАЗЛИЧНЫЕ ПЕРВЫЕ 10 Т.Код КАК К ИЗ Справочник.Товары КАК Т",
        "ВЫБРАТЬ А ИЗ Т ГДЕ А=1 СГРУППИРОВАТЬ ПО Н ИМЕЮЩИЕ А>0",
        "ВЫБРАТЬ А ИЗ Т ЛЕВОЕ СОЕДИНЕНИЕ У ПО Т.А = У.А",
        "ВЫБРАТЬ А ИЗ Т ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Б ИЗ У",
        "ВЫБРАТЬ А ПОМЕСТИТЬ Врем ИЗ Т; УНИЧТОЖИТЬ Врем",
        "ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Товары КАК Т {ГДЕ Т.Поле}",
    ] {
        assert_accepted(input);
    }
}

/// Одинокий амперсанд никогда не соседствует с `Ident` вплотную.
///
/// На этом стоит правило параметра: имя приходит внутри самого токена, и
/// отдельного `Ident` за амперсандом грамматика не берёт. Допущение лексерное,
/// поэтому и проверяется на лексере: смена его правил обязана уронить этот
/// тест, а не тихо вернуть грамматике съеденное ключевое слово.
#[test]
fn a_lone_ampersand_is_never_adjacent_to_an_identifier() {
    use lexer::sdbl::{tokenize_sdbl, SdblTokenKind};

    let mut seen_lone = false;
    for source in [
        "ВЫБРАТЬ &П ИЗ Т",
        "ВЫБРАТЬ & П ИЗ Т",
        "ВЫБРАТЬ &\nП ИЗ Т",
        "ВЫБРАТЬ &&П ИЗ Т",
        "ВЫБРАТЬ &1 ИЗ Т",
        "ВЫБРАТЬ & ИЗ Т",
        "ВЫБРАТЬ &_П ИЗ Т",
    ] {
        let tokens = tokenize_sdbl(source);
        for (i, token) in tokens.iter().enumerate() {
            if token.kind != SdblTokenKind::Ampersand {
                continue;
            }
            seen_lone = true;
            assert_ne!(
                tokens.get(i + 1).map(|next| next.kind),
                Some(SdblTokenKind::Ident),
                "в {source:?} за одиноким амперсандом встал Ident вплотную"
            );
        }
    }

    assert!(seen_lone, "корпус не дал ни одного одинокого амперсанда: правило не проверено");
}

/// Узел параметра состоит ровно из одного токена, где бы он ни разобрался.
///
/// Имя приходит внутри самого токена, поэтому любое правило, дозволившее себе
/// взять `Ident` следом, отбирает у запроса следующее слово: у псевдонима —
/// `КАК`, у соединения — `ПО`. Правил, разбирающих параметр, несколько, и
/// проверять их поимённо значит пропустить следующее; свойство держит разряд
/// целиком.
#[test]
fn a_parameter_node_never_holds_more_than_its_own_token() {
    use syntax::SyntaxKind;

    let mut seen = 0;
    for source in [
        "ВЫБРАТЬ * ИЗ &ТЗ КАК Т",
        "SELECT * FROM &Tmp AS T",
        "ВЫБРАТЬ * ИЗ & Т",
        "ВЫБРАТЬ * ИЗ &\nГДЕ 1 = 1",
        "ВЫБРАТЬ А ИЗ Т ЛЕВОЕ СОЕДИНЕНИЕ &\nПО Т.А = 1",
        "ВЫБРАТЬ * ИЗ Т ГДЕ Т.А = &П И Т.Б = 1",
        "ВЫБРАТЬ * ИЗ Т ГДЕ Т.А = &\nИ Т.Б = 1",
        "ВЫБРАТЬ &П КАК Поле ИЗ Т",
    ] {
        for node in parse_sdbl(source)
            .syntax_node()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::SDBL_PARAMETER)
        {
            seen += 1;
            let tokens: Vec<_> = node
                .children_with_tokens()
                .filter_map(|c| c.into_token())
                .filter(|t| !t.kind().is_trivia())
                .map(|t| (t.kind(), t.text().to_string()))
                .collect();
            assert_eq!(tokens.len(), 1, "в {source:?} узел параметра забрал лишнее: {tokens:?}");
            assert_eq!(
                tokens[0].0,
                SyntaxKind::AMPERSAND,
                "в {source:?} параметр не из амперсанда"
            );
        }
    }

    assert!(seen >= 8, "корпус дал всего {seen} параметров: часть правил не пройдена");
}
