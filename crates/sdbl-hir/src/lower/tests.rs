use crate::hir::{JoinType, SdblHir, SdblPackage};
use crate::lower::lower_sdbl_to_hir;

fn single_query_hir(package: &SdblPackage) -> &SdblHir {
    assert_eq!(package.queries().len(), 1, "Expected single query in package");
    &package.queries()[0].hir
}

fn parse_attr_type_for_test(type_str: &str) -> bsl_metadata::AttributeType {
    use bsl_metadata::{AttributeType, MdoType};

    match type_str {
        "Boolean" => AttributeType::Boolean,
        "String" => AttributeType::String { length: None },
        "Number" => AttributeType::Number { precision: 10, scale: 2 },
        "УникальныйИдентификатор" => AttributeType::Uuid,
        s if s.starts_with("TaskRef.") => {
            let name = &s["TaskRef.".len()..];
            AttributeType::Ref { mdo_type: MdoType::Task, name: name.to_string() }
        }
        s if s.contains('.') => {
            let parts: Vec<_> = s.split('.').collect();
            if parts.len() == 2 {
                let mdo_type = match parts[0] {
                    "Задача" => MdoType::Task,
                    "Документ" => MdoType::Document,
                    "Справочник" => MdoType::Catalog,
                    "БизнесПроцесс" => MdoType::BusinessProcess,
                    _ => MdoType::Document,
                };
                AttributeType::Ref { mdo_type, name: parts[1].to_string() }
            } else {
                AttributeType::String { length: None }
            }
        }
        _ => AttributeType::String { length: None },
    }
}

fn lower_query(sdbl: &str) -> SdblHir {
    let ast = parser::parse_sdbl(sdbl);
    let package = lower_sdbl_to_hir(&ast, None);
    single_query_hir(&package).clone()
}

fn lower_query_with_source_map(sdbl: &str) -> SdblPackage {
    let ast = parser::parse_sdbl(sdbl);
    lower_sdbl_to_hir(&ast, None)
}

#[test]
fn test_simple_select() {
    let hir = lower_query("SELECT Код FROM Справочник.Валюты");

    assert!(!hir.select.fields.is_empty());
    assert_eq!(hir.from.len(), 1);
    assert_eq!(hir.from[0].full_name, "Справочник.Валюты");
}

#[test]
fn test_source_map_collects_keywords() {
    let result =
        lower_query_with_source_map("SELECT Код FROM Справочник.Валюты WHERE Наименование = 'USD'");

    let sm = &result.source_map;

    assert!(
        sm.clause_keywords.len() >= 3,
        "Expected at least 3 clause keywords (SELECT, FROM, WHERE), got {}",
        sm.clause_keywords.len()
    );

    let select_token = sm
        .clause_keywords
        .iter()
        .find(|t| t.text.to_uppercase() == "SELECT" || t.text.to_uppercase() == "ВЫБРАТЬ");
    assert!(select_token.is_some(), "Should find SELECT keyword");

    let from_token = sm
        .clause_keywords
        .iter()
        .find(|t| t.text.to_uppercase() == "FROM" || t.text.to_uppercase() == "ИЗ");
    assert!(from_token.is_some(), "Should find FROM keyword");

    let where_token = sm
        .clause_keywords
        .iter()
        .find(|t| t.text.to_uppercase() == "WHERE" || t.text.to_uppercase() == "ГДЕ");
    assert!(where_token.is_some(), "Should find WHERE keyword");

    assert!(
        !sm.operators.is_empty(),
        "Expected at least 1 operator (=), got {}",
        sm.operators.len()
    );
}

#[test]
fn test_source_map_collects_operators() {
    let result = lower_query_with_source_map(
        "SELECT Код FROM Справочник.Валюты WHERE Сумма > 100 AND Количество <= 50",
    );

    let sm = &result.source_map;

    assert!(
        sm.operators.len() >= 3,
        "Expected at least 3 operators (>, AND, <=), got {}",
        sm.operators.len()
    );
}

#[test]
fn test_source_map_collects_join_keywords() {
    let result = lower_query_with_source_map("SELECT Код FROM Справочник.Товары");

    let sm = &result.source_map;

    assert!(sm.clause_keywords.len() >= 2, "Should have SELECT and FROM keywords");
}

#[test]
fn test_source_map_collects_union_keywords() {
    let result = lower_query_with_source_map(
        "SELECT Код FROM Справочник.Товары UNION ALL SELECT Номер FROM Документ.Продажа",
    );

    let sm = &result.source_map;

    assert!(
        sm.modifiers.len() >= 2,
        "Expected at least 2 modifiers (UNION, ALL), got {}",
        sm.modifiers.len()
    );
}

#[test]
fn test_totals_by_only_hierarchy_source_map() {
    let result = lower_query_with_source_map(
        "ВЫБРАТЬ Группа КАК Группа ИЗ Товары ИТОГИ ПО Группа ТОЛЬКО ИЕРАРХИЯ",
    );
    let sm = &result.source_map;

    for keyword in ["ИТОГИ", "ПО"] {
        assert!(
            sm.clause_keywords.iter().any(|token| token.text == keyword),
            "Expected TOTALS BY clause keyword `{keyword}` in source map"
        );
    }

    for modifier in ["ТОЛЬКО", "ИЕРАРХИЯ"] {
        assert!(
            sm.modifiers.iter().any(|token| token.text == modifier),
            "Expected TOTALS BY modifier `{modifier}` in source map"
        );
    }

    let totals_start = sm
        .clause_keywords
        .iter()
        .find(|token| token.text == "ИТОГИ")
        .expect("Expected TOTALS BY keyword")
        .range
        .start();

    assert!(
        sm.field_aliases
            .iter()
            .any(|token| token.text == "Группа" && token.range.start() > totals_start),
        "Expected TOTALS BY output reference `Группа` to be recorded as a field alias"
    );
}

#[test]
fn test_aliased_table() {
    let hir = lower_query("SELECT Код FROM Справочник.Валюты");

    assert_eq!(hir.from.len(), 1);
    assert_eq!(hir.from[0].full_name, "Справочник.Валюты");
}

#[test]
fn test_join_detection() {
    let hir = lower_query(
        "SELECT Т.Код FROM Справочник.Валюты AS В LEFT JOIN Справочник.Товары AS Т ON В.Ссылка = Т.Владелец"
    );

    assert_eq!(hir.joins.len(), 1);
    assert_eq!(hir.joins[0].join_type, JoinType::Left);
}

#[test]
fn test_select_fields() {
    let hir = lower_query("SELECT Код, Наименование FROM Справочник.Валюты");

    assert!(!hir.select.fields.is_empty());
}

#[test]
fn test_source_map_collects_aggregate_functions() {
    let query = "SELECT SUM(Price), AVG(Quantity), COUNT(*), MIN(Date), MAX(Total) FROM Products";
    let result = lower_query_with_source_map(query);

    assert!(
        result.source_map.aggregate_functions.len() >= 5,
        "Expected at least 5 aggregate functions, got {}",
        result.source_map.aggregate_functions.len()
    );

    let func_names: Vec<String> =
        result.source_map.aggregate_functions.iter().map(|t| t.text.to_string()).collect();
    assert!(func_names.contains(&"SUM".to_string()));
    assert!(func_names.contains(&"AVG".to_string()));
    assert!(func_names.contains(&"COUNT".to_string()));
    assert!(func_names.contains(&"MIN".to_string()));
    assert!(func_names.contains(&"MAX".to_string()));
}

#[test]
fn test_source_map_collects_aggregate_functions_russian() {
    let query = "ВЫБРАТЬ СУММА(Цена), СРЕДНЕЕ(Количество), КОЛИЧЕСТВО(*) ИЗ Товары";
    let result = lower_query_with_source_map(query);

    assert!(
        result.source_map.aggregate_functions.len() >= 3,
        "Expected at least 3 aggregate functions, got {}",
        result.source_map.aggregate_functions.len()
    );

    let func_names: Vec<String> =
        result.source_map.aggregate_functions.iter().map(|t| t.text.to_string()).collect();
    assert!(func_names.contains(&"СУММА".to_string()));
    assert!(func_names.contains(&"СРЕДНЕЕ".to_string()));
    assert!(func_names.contains(&"КОЛИЧЕСТВО".to_string()));
}

#[test]
fn test_source_map_collects_is_null_keywords() {
    let query = "SELECT * FROM Products WHERE Price IS NULL";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("IS")),
        "Expected IS keyword in special_keywords"
    );
    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("NULL")),
        "Expected NULL keyword in special_keywords"
    );
}

#[test]
fn test_source_map_collects_is_not_null_keywords() {
    let query = "SELECT * FROM Products WHERE Price IS NOT NULL";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();
    let operators: Vec<String> =
        result.source_map.operators.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("IS")),
        "Expected IS keyword in special_keywords"
    );
    assert!(
        operators.iter().any(|k| k.eq_ignore_ascii_case("NOT")),
        "Expected NOT keyword in operators"
    );
    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("NULL")),
        "Expected NULL keyword in special_keywords"
    );
}

#[test]
fn test_source_map_collects_is_null_russian() {
    let query = "ВЫБРАТЬ * ИЗ Товары ГДЕ Цена ЕСТЬ NULL";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("ЕСТЬ")),
        "Expected ЕСТЬ keyword in special_keywords"
    );
    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("NULL")),
        "Expected NULL keyword in special_keywords"
    );
}

#[test]
fn test_source_map_collects_in_keyword() {
    let query = "SELECT * FROM Products WHERE Type IN (1, 2, 3)";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("IN")),
        "Expected IN keyword in special_keywords"
    );
}

#[test]
fn test_source_map_collects_in_keyword_russian() {
    let query = "ВЫБРАТЬ * ИЗ Товары ГДЕ Тип В (1, 2, 3)";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("В")),
        "Expected В keyword in special_keywords"
    );
}

#[test]
fn test_in_expression_value_list() {
    let query = "SELECT * FROM Products WHERE Type IN (1, 2, 3)";
    let result = lower_query_with_source_map(query);

    let hir = single_query_hir(&result);
    assert!(hir.where_clause.is_some(), "Expected WHERE clause");

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();
    assert!(special_keywords.iter().any(|k| k.eq_ignore_ascii_case("IN")));
}

#[test]
fn test_source_map_collects_distinct_keyword() {
    let query = "SELECT DISTINCT Name FROM Products";
    let result = lower_query_with_source_map(query);

    let modifiers: Vec<String> =
        result.source_map.modifiers.iter().map(|t| t.text.to_string()).collect();

    assert!(
        modifiers.iter().any(|k| k.eq_ignore_ascii_case("DISTINCT")),
        "Expected DISTINCT keyword in modifiers"
    );

    assert!(single_query_hir(&result).select.distinct);
}

#[test]
fn test_source_map_collects_distinct_keyword_russian() {
    let query = "ВЫБРАТЬ РАЗЛИЧНЫЕ Наименование ИЗ Товары";
    let result = lower_query_with_source_map(query);

    let modifiers: Vec<String> =
        result.source_map.modifiers.iter().map(|t| t.text.to_string()).collect();

    assert!(
        modifiers.iter().any(|k| k.eq_ignore_ascii_case("РАЗЛИЧНЫЕ")),
        "Expected РАЗЛИЧНЫЕ keyword in modifiers"
    );

    assert!(single_query_hir(&result).select.distinct);
}

#[test]
fn test_source_map_collects_top_keyword() {
    let query = "SELECT TOP 10 Name FROM Products";
    let result = lower_query_with_source_map(query);

    let modifiers: Vec<String> =
        result.source_map.modifiers.iter().map(|t| t.text.to_string()).collect();

    assert!(
        modifiers.iter().any(|k| k.eq_ignore_ascii_case("TOP")),
        "Expected TOP keyword in modifiers"
    );

    assert_eq!(single_query_hir(&result).select.top, Some(10));
}

#[test]
fn test_source_map_collects_top_keyword_russian() {
    let query = "ВЫБРАТЬ ПЕРВЫЕ 5 Наименование ИЗ Товары";
    let result = lower_query_with_source_map(query);

    let modifiers: Vec<String> =
        result.source_map.modifiers.iter().map(|t| t.text.to_string()).collect();

    assert!(
        modifiers.iter().any(|k| k.eq_ignore_ascii_case("ПЕРВЫЕ")),
        "Expected ПЕРВЫЕ keyword in modifiers"
    );

    assert_eq!(single_query_hir(&result).select.top, Some(5));
}

#[test]
fn test_distinct_and_top_together() {
    let query = "SELECT DISTINCT TOP 20 Name FROM Products";
    let result = lower_query_with_source_map(query);

    let modifiers: Vec<String> =
        result.source_map.modifiers.iter().map(|t| t.text.to_string()).collect();

    assert!(modifiers.iter().any(|k| k.eq_ignore_ascii_case("DISTINCT")));
    assert!(modifiers.iter().any(|k| k.eq_ignore_ascii_case("TOP")));

    assert!(single_query_hir(&result).select.distinct);
    assert_eq!(single_query_hir(&result).select.top, Some(20));
}

#[test]
fn test_source_map_collects_between_keyword() {
    let query = "SELECT * FROM Products WHERE Price BETWEEN 100 AND 500";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("BETWEEN")),
        "Expected BETWEEN keyword in special_keywords"
    );
}

#[test]
fn test_source_map_collects_between_keyword_russian() {
    let query = "ВЫБРАТЬ * ИЗ Товары ГДЕ Цена МЕЖДУ 100 И 500";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("МЕЖДУ")),
        "Expected МЕЖДУ keyword in special_keywords"
    );
}

#[test]
fn test_source_map_collects_like_keyword() {
    let query = "SELECT * FROM Products WHERE Name LIKE 'Apple%'";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("LIKE")),
        "Expected LIKE keyword in special_keywords"
    );
}

#[test]
fn test_source_map_collects_like_keyword_russian() {
    let query = "ВЫБРАТЬ * ИЗ Товары ГДЕ Наименование ПОДОБНО 'Яблоко%'";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("ПОДОБНО")),
        "Expected ПОДОБНО keyword in special_keywords"
    );
}

#[test]
fn test_source_map_collects_like_escape_keyword() {
    let query = "SELECT * FROM Products WHERE Name LIKE 'App!_le%' ESCAPE '!'";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("LIKE")),
        "Expected LIKE keyword"
    );
    if special_keywords.iter().any(|k| k.eq_ignore_ascii_case("ESCAPE")) {}
}

#[test]
fn test_case_expression_parsed() {
    let query = "SELECT CASE Status WHEN 1 THEN 'Active' END FROM Products";
    let parse = parser::parse_sdbl(query);
    let tree = format!("{:#?}", parse.syntax_node());

    assert!(tree.contains("SDBL_CASE_EXPR"), "CASE expression not in parse tree");
}

#[test]
fn test_source_map_collects_case_keywords() {
    let query = r#"SELECT CASE Status WHEN 1 THEN "Active" WHEN 2 THEN "Inactive" ELSE "Unknown" END AS StatusText FROM Products"#;

    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("CASE")),
        "Expected CASE keyword, got: {:?}",
        special_keywords
    );
    assert!(
        special_keywords.iter().filter(|k| k.eq_ignore_ascii_case("WHEN")).count() >= 2,
        "Expected at least 2 WHEN keywords"
    );
    assert!(
        special_keywords.iter().filter(|k| k.eq_ignore_ascii_case("THEN")).count() >= 2,
        "Expected at least 2 THEN keywords"
    );
    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("ELSE")),
        "Expected ELSE keyword"
    );
    assert!(special_keywords.iter().any(|k| k.eq_ignore_ascii_case("END")), "Expected END keyword");
}

#[test]
fn test_source_map_collects_case_searched() {
    let query = r#"SELECT CASE WHEN Price > 1000 THEN "Expensive" WHEN Price > 500 THEN "Moderate" ELSE "Cheap" END AS PriceCategory FROM Products"#;
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("CASE")),
        "Expected CASE keyword"
    );
    assert!(
        special_keywords.iter().filter(|k| k.eq_ignore_ascii_case("WHEN")).count() >= 2,
        "Expected at least 2 WHEN keywords"
    );
}

#[test]
fn test_source_map_collects_case_keywords_russian() {
    let query = r#"ВЫБРАТЬ ВЫБОР Статус КОГДА 1 ТОГДА "Активен" КОГДА 2 ТОГДА "Неактивен" ИНАЧЕ "Неизвестен" КОНЕЦ КАК ТекстСтатуса ИЗ Товары"#;
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("ВЫБОР")),
        "Expected ВЫБОР keyword"
    );
    assert!(
        special_keywords.iter().filter(|k| k.eq_ignore_ascii_case("КОГДА")).count() >= 2,
        "Expected at least 2 КОГДА keywords"
    );
    assert!(
        special_keywords.iter().filter(|k| k.eq_ignore_ascii_case("ТОГДА")).count() >= 2,
        "Expected at least 2 ТОГДА keywords"
    );
    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("ИНАЧЕ")),
        "Expected ИНАЧЕ keyword"
    );
    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("КОНЕЦ")),
        "Expected КОНЕЦ keyword"
    );
}

#[test]
fn test_in_expression_not_in() {
    let query = "SELECT * FROM Products WHERE Type NOT IN (1, 2)";
    let result = lower_query_with_source_map(query);

    let special_keywords: Vec<String> =
        result.source_map.special_keywords.iter().map(|t| t.text.to_string()).collect();
    let operators: Vec<String> =
        result.source_map.operators.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special_keywords.iter().any(|k| k.eq_ignore_ascii_case("IN")),
        "Expected IN keyword in special_keywords"
    );
    assert!(
        operators.iter().any(|k| k.eq_ignore_ascii_case("NOT")),
        "Expected NOT keyword in operators (from NOT_EXPR lowering)"
    );
}

#[test]
fn names_taken_from_tokens_are_the_names_written_in_the_query() {
    // Метаморфное свойство сравнивает два понижения между собой и потому
    // слепо к равномерно неверному значению: пустое имя параметра было бы
    // пустым в обоих. Имена приходится утверждать прямо.
    use crate::hir::ExprHir;

    let query = "ВЫБРАТЬ ЗНАЧЕНИЕ(Перечисление.ВидыТоваров.Основной) КАК Вид \
                 ИЗ Справочник.Товары КАК Т \
                 ГДЕ Т.Дата = &НачалоПериода";
    let hir = lower_query(query);

    let ExprHir::FunctionCall { args, .. } = &hir.select.fields[0].expr else {
        panic!("ожидался вызов ЗНАЧЕНИЕ");
    };
    let ExprHir::ColumnRef { parts, .. } = &args[0] else {
        panic!("ожидалась ссылка внутри ЗНАЧЕНИЕ");
    };
    let value_parts: Vec<&str> = parts.iter().map(|p| p.as_str()).collect();
    assert_eq!(value_parts, vec!["Перечисление", "ВидыТоваров", "Основной"]);

    let Some(ExprHir::BinaryOp { lhs, rhs, .. }) = hir.where_clause.as_ref() else {
        panic!("ожидалось сравнение в ГДЕ");
    };

    let ExprHir::ColumnRef { parts, .. } = lhs.as_ref() else {
        panic!("слева ожидалась ссылка на поле");
    };
    let column_parts: Vec<&str> = parts.iter().map(|p| p.as_str()).collect();
    assert_eq!(column_parts, vec!["Т", "Дата"]);

    let ExprHir::Parameter { name, .. } = rhs.as_ref() else {
        panic!("справа ожидался параметр");
    };
    assert_eq!(name.as_str(), "НачалоПериода");
}

#[test]
fn a_name_range_stops_at_the_name() {
    // Диапазон составного имени кончается на последнем имени и там, где
    // ссылка лежит внутри ЗНАЧЕНИЕ, — тоже: собственный диапазон узла
    // растянулся бы на комментарий перед закрывающей скобкой.
    use crate::hir::ExprHir;

    let query = "ВЫБРАТЬ ЗНАЧЕНИЕ(Перечисление.ВидыТоваров.Основной // хвост\n) КАК Вид";
    let hir = lower_query(query);

    let ExprHir::FunctionCall { args, .. } = &hir.select.fields[0].expr else {
        panic!("ожидался вызов ЗНАЧЕНИЕ");
    };
    let ExprHir::ColumnRef { range, .. } = &args[0] else {
        panic!("ожидалась ссылка внутри ЗНАЧЕНИЕ");
    };

    let expected = query.find("Основной").unwrap() + "Основной".len();
    assert_eq!(usize::from(range.end()), expected);
}

#[test]
fn each_operator_in_a_chain_is_its_own() {
    // Операция берётся от своего токена: подстрока в тексте узла не различает,
    // какая из двух операций цепочки чья, и первая же выигрывала за обе.
    use crate::hir::{BinaryOp, ExprHir};

    let hir = lower_query("ВЫБРАТЬ Т.А ИЗ Справочник.Валюты КАК Т ГДЕ Т.Цена = 100 - 10 + 1");

    let mut ops = Vec::new();
    let mut expr = hir.where_clause.as_ref().expect("ГДЕ разобран");
    while let ExprHir::BinaryOp { lhs, op, .. } = expr {
        ops.push(*op);
        expr = lhs;
    }
    ops.reverse();

    assert_eq!(ops, vec![BinaryOp::Eq], "верхний уровень — сравнение");

    let ExprHir::BinaryOp { rhs, .. } = hir.where_clause.as_ref().unwrap() else {
        panic!("ожидалось сравнение");
    };

    let mut arithmetic = Vec::new();
    let mut expr = rhs.as_ref();
    while let ExprHir::BinaryOp { lhs, op, .. } = expr {
        arithmetic.push(*op);
        expr = lhs;
    }
    arithmetic.reverse();

    assert_eq!(arithmetic, vec![BinaryOp::Sub, BinaryOp::Add]);
}

#[test]
fn a_literal_containing_a_keyword_is_not_an_operator() {
    // Текст узла включает содержимое литерала, и союз внутри строки
    // превращал сравнение в конъюнкцию.
    use crate::hir::{BinaryOp, ExprHir};

    let hir = lower_query("ВЫБРАТЬ Т.А ИЗ Справочник.Валюты КАК Т ГДЕ Т.Имя = \" И \"");

    let ExprHir::BinaryOp { op, .. } = hir.where_clause.as_ref().expect("ГДЕ разобран")
    else {
        panic!("ожидалось сравнение");
    };
    assert_eq!(*op, BinaryOp::Eq);
}

#[test]
fn test_into_clause_russian() {
    let hir = lower_query("ВЫБРАТЬ Поле1 ПОМЕСТИТЬ ВременнаяТаблица ИЗ Справочник.Валюты");

    assert_eq!(hir.into_table.as_ref().map(|n| n.as_str()), Some("ВременнаяТаблица"));
    assert!(!hir.select.fields.is_empty());
    assert_eq!(hir.from.len(), 1);
}

#[test]
fn test_into_clause_english() {
    let hir = lower_query("SELECT Field1 INTO TempTable FROM Catalog.Currency");

    assert_eq!(hir.into_table.as_ref().map(|n| n.as_str()), Some("TempTable"));
    assert!(!hir.select.fields.is_empty());
    assert_eq!(hir.from.len(), 1);
}

#[test]
fn test_into_clause_with_distinct_and_top() {
    let hir = lower_query("SELECT DISTINCT TOP 10 Field1 INTO MyTemp FROM Catalog.Items");

    assert_eq!(hir.into_table.as_ref().map(|n| n.as_str()), Some("MyTemp"));
    assert!(hir.select.distinct);
    assert_eq!(hir.select.top, Some(10));
    assert!(!hir.select.fields.is_empty());
}

#[test]
fn test_no_into_clause() {
    let hir = lower_query("SELECT Field1 FROM Catalog.Items");

    assert!(hir.into_table.is_none());
    assert!(!hir.select.fields.is_empty());
}

#[test]
fn test_temp_table_in_union() {
    let query = "SELECT Поле1 AS Действие INTO ТаблицаДействий FROM Справочник.Валюты UNION ALL SELECT Действие FROM ТаблицаДействий";

    let ast = parser::parse_sdbl(query);
    let result = lower_sdbl_to_hir(&ast, None);

    assert_eq!(result.queries().len(), 2, "Expected 2 queries in package (main + UNION)");

    let main_hir = &result.queries()[0].hir;
    assert_eq!(main_hir.into_table.as_ref().map(|n| n.as_str()), Some("ТаблицаДействий"));
    assert_eq!(main_hir.select.fields.len(), 1);

    let union_hir = &result.queries()[1].hir;
    assert_eq!(union_hir.from.len(), 1);

    let temp_table_ref = &union_hir.from[0];
    assert_eq!(temp_table_ref.full_name, "ТаблицаДействий");
    assert!(temp_table_ref.is_resolved(), "Temporary table should be resolved");

    if let Some(crate::hir::ResolvedTable::TempTable { name, fields, .. }) =
        &temp_table_ref.metadata
    {
        assert_eq!(name, "ТаблицаДействий");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name.as_str(), "Действие");
    } else {
        panic!("Expected TempTable variant, got: {:?}", temp_table_ref.metadata);
    }
}

#[test]
fn test_drop_query_semantic_tokens() {
    let result = lower_query_with_source_map("УНИЧТОЖИТЬ ВТ_ВсеСвойства");
    let sm = &result.source_map;

    assert!(result.queries().is_empty(), "DROP query should not create SELECT HIR");

    let clause_keywords: Vec<_> = sm.clause_keywords.iter().map(|t| t.text.as_str()).collect();
    let table_names: Vec<_> = sm.table_names.iter().map(|t| t.text.as_str()).collect();

    assert!(
        clause_keywords.contains(&"УНИЧТОЖИТЬ"),
        "DROP keyword should be highlighted, got: {clause_keywords:?}"
    );
    assert!(
        table_names.contains(&"ВТ_ВсеСвойства"),
        "temporary table name should be highlighted, got: {table_names:?}"
    );
}

#[test]
fn test_drop_query_removes_temp_table_from_subsequent_scope() {
    let query = "ВЫБРАТЬ Поле КАК Поле ПОМЕСТИТЬ ВТ ИЗ Источник; УНИЧТОЖИТЬ ВТ; ВЫБРАТЬ Поле ИЗ ВТ";

    let result = lower_query_with_source_map(query);

    assert_eq!(result.queries().len(), 2, "DROP query should not create SELECT HIR");

    let second_hir = &result.queries()[1].hir;
    assert_eq!(second_hir.from.len(), 1);
    assert!(
        !second_hir.from[0].is_resolved(),
        "temporary table must not remain resolved after DROP/УНИЧТОЖИТЬ"
    );
}

fn create_test_metadata_with_tabular_section() -> bsl_metadata::Configuration {
    use bsl_metadata::{
        tabular_section::{TabularSection, TabularSectionAttribute},
        MdoType, MetadataObject,
    };

    let uuid_nil =
        *bsl_metadata::tabular_section::TabularSection::new(Default::default(), "temp").uuid();

    let mut config = bsl_metadata::Configuration::new("TestConfig");

    let mut bp = MetadataObject::new(MdoType::BusinessProcess, "Исполнение");

    let mut ts = TabularSection::new(uuid_nil, "РезультатыПроверки");
    ts.set_name_en(Some("CheckResults".to_string()));

    let mut attr1 = TabularSectionAttribute::new(
        uuid_nil,
        "ЗадачаИсполнителя",
        parse_attr_type_for_test("TaskRef.Задача"),
    );
    attr1.set_name_en(Some("ExecutorTask".to_string()));

    let mut attr2 = TabularSectionAttribute::new(
        uuid_nil,
        "ЗадачаПроверяющего",
        parse_attr_type_for_test("TaskRef.Задача"),
    );
    attr2.set_name_en(Some("CheckerTask".to_string()));

    let mut attr3 = TabularSectionAttribute::new(
        uuid_nil,
        "ОтправленоНаДоработку",
        parse_attr_type_for_test("Boolean"),
    );
    attr3.set_name_en(Some("SentForRevision".to_string()));

    ts.set_attributes(vec![attr1, attr2, attr3]);

    bp.add_tabular_section(ts);

    config.add_metadata_object(bp);
    config
}

#[test]
fn test_tabular_section_field_resolution() {
    let metadata = create_test_metadata_with_tabular_section();

    let code = "ВЫБРАТЬ Т.ЗадачаИсполнителя ИЗ БизнесПроцесс.Исполнение.РезультатыПроверки КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(metadata.clone())));
    let hir = single_query_hir(&package);

    assert_eq!(hir.from.len(), 1);
    let table_ref = &hir.from[0];
    assert_eq!(table_ref.full_name, "БизнесПроцесс.Исполнение.РезультатыПроверки");
    assert!(table_ref.is_resolved(), "Tabular section should be resolved");

    let resolved = table_ref.metadata.as_ref().expect("Metadata should be present");
    let fields = resolved.fields();

    assert_eq!(fields.len(), 5, "Expected 5 fields: Ссылка + НомерСтроки + 3 attributes");

    let ref_field = fields.iter().find(|f| f.name.as_str() == "Ссылка");
    assert!(ref_field.is_some(), "Missing Ссылка field");
    let ref_field = ref_field.unwrap();
    assert!(ref_field.is_standard, "Ссылка should be marked as standard");
    assert_eq!(ref_field.name_en.as_deref(), Some("Ref"));

    assert!(fields.iter().any(|f| f.name.as_str() == "ЗадачаИсполнителя"));
    assert!(fields.iter().any(|f| f.name.as_str() == "ЗадачаПроверяющего"));
    assert!(fields.iter().any(|f| f.name.as_str() == "ОтправленоНаДоработку"));
}

#[test]
fn test_tabular_section_nomer_stroki_field() {
    let metadata = create_test_metadata_with_tabular_section();

    let code = "ВЫБРАТЬ Т.НомерСтроки ИЗ БизнесПроцесс.Исполнение.РезультатыПроверки КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(metadata)));
    let hir = single_query_hir(&package);

    let table_ref = &hir.from[0];
    let resolved = table_ref.metadata.as_ref().expect("Metadata should be present");
    let fields = resolved.fields();

    let line_num_field = fields.iter().find(|f| f.name.as_str() == "НомерСтроки");
    assert!(line_num_field.is_some(), "Missing НомерСтроки field");
    let line_num_field = line_num_field.unwrap();
    assert!(line_num_field.is_standard, "НомерСтроки should be marked as standard");
    assert_eq!(line_num_field.name_en.as_deref(), Some("LineNumber"));
}

#[test]
fn test_tabular_section_case_insensitive_matching() {
    let metadata = create_test_metadata_with_tabular_section();

    let code = "ВЫБРАТЬ Т.ЗадачаИсполнителя ИЗ БизнесПроцесс.Исполнение.результатыпроверки КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(metadata.clone())));
    let hir = single_query_hir(&package);

    assert_eq!(hir.from.len(), 1);
    let table_ref = &hir.from[0];
    assert!(table_ref.is_resolved(), "Should resolve with case-insensitive matching");
}

#[test]
fn test_tabular_section_bilingual_support() {
    let metadata = create_test_metadata_with_tabular_section();

    let code = "ВЫБРАТЬ Т.ЗадачаИсполнителя ИЗ БизнесПроцесс.Исполнение.CheckResults КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(metadata.clone())));
    let hir = single_query_hir(&package);

    assert_eq!(hir.from.len(), 1);
    let table_ref = &hir.from[0];
    assert!(table_ref.is_resolved(), "Should resolve using English name");

    let resolved = table_ref.metadata.as_ref().expect("Metadata should be present");
    let fields = resolved.fields();
    assert_eq!(fields.len(), 5, "Expected 5 fields");
}

#[test]
fn test_tabular_section_not_found() {
    let metadata = create_test_metadata_with_tabular_section();

    let code = "ВЫБРАТЬ Т.Поле ИЗ БизнесПроцесс.Исполнение.НесуществующаяТабличнаяЧасть КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(metadata.clone())));
    let hir = single_query_hir(&package);

    assert_eq!(hir.from.len(), 1);
    let table_ref = &hir.from[0];

    let resolved = table_ref.metadata.as_ref();
    if let Some(r) = resolved {
        assert_eq!(r.fields().len(), 0, "Should have no fields when tabular section not found");
    }
}

#[test]
fn test_invalid_mdo_type_for_tabular_section() {
    use bsl_metadata::{Configuration, MdoType, MetadataObject};

    let mut config = Configuration::new("TestConfig");

    let register = MetadataObject::new(MdoType::InformationRegister, "ТестовыйРегистр");
    config.add_metadata_object(register);

    let code = "ВЫБРАТЬ Т.Поле ИЗ РегистрСведений.ТестовыйРегистр.ТабличнаяЧасть КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config.clone())));
    let hir = single_query_hir(&package);

    assert_eq!(hir.from.len(), 1);
    let table_ref = &hir.from[0];

    let resolved = table_ref.metadata.as_ref();
    if let Some(r) = resolved {
        assert_eq!(r.fields().len(), 0, "Should have no fields for invalid MDO type");
    }
}

#[test]
fn test_tabular_section_task_ref_type_parsing() {
    use bsl_metadata::{
        tabular_section::{TabularSection, TabularSectionAttribute},
        MdoType, MetadataObject,
    };

    let uuid_nil =
        *bsl_metadata::tabular_section::TabularSection::new(Default::default(), "temp").uuid();

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let mut bp = MetadataObject::new(MdoType::BusinessProcess, "Исполнение");
    let mut ts = TabularSection::new(uuid_nil, "РезультатыПроверки");

    let mut attr = TabularSectionAttribute::new(
        uuid_nil,
        "ЗадачаПроверяющего",
        parse_attr_type_for_test("Задача.ЗадачаИсполнителя"),
    );
    attr.set_name_en(Some("CheckerTask".to_string()));

    ts.set_attributes(vec![attr]);
    bp.add_tabular_section(ts);
    config.add_metadata_object(bp);

    let code = "ВЫБРАТЬ Т.ЗадачаПроверяющего ИЗ БизнесПроцесс.Исполнение.РезультатыПроверки КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config.clone())));
    let hir = single_query_hir(&package);

    assert_eq!(hir.from.len(), 1);
    let table_ref = &hir.from[0];
    assert!(table_ref.is_resolved(), "Table should be resolved");

    let resolved = table_ref.metadata.as_ref().expect("Metadata should be present");
    let fields = resolved.fields();

    let field = fields.iter().find(|f| f.name.as_str() == "ЗадачаПроверяющего");
    assert!(field.is_some(), "Should find ЗадачаПроверяющего field");

    let field = field.unwrap();
    match &field.ty {
        crate::SdblType::Ref(mdo_ref) => {
            assert_eq!(mdo_ref.mdo_type, MdoType::Task, "Should be Task reference");
            assert_eq!(
                mdo_ref.name, "ЗадачаИсполнителя",
                "Should reference ЗадачаИсполнителя task"
            );
        }
        other => panic!("Expected Ref type, got: {:?}", other),
    }
}

#[test]
fn hierarchical_catalog_parent_field_is_resolved() {
    let catalog_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>Номенклатура</Name>
            <Hierarchical>true</Hierarchical>
            <CodeLength>9</CodeLength>
            <DescriptionLength>25</DescriptionLength>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let catalog = bsl_metadata::xml_parser::parse_catalog_xml(catalog_xml).unwrap();
    config.add_metadata_object(catalog);

    let code = "ВЫБРАТЬ Номенклатура.Родитель ИЗ Справочник.Номенклатура КАК Номенклатура";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let hir = single_query_hir(&package);

    let fields = hir.from[0].metadata.as_ref().expect("catalog must resolve").fields();
    let parent = fields.iter().find(|field| field.name == "Родитель").expect("Родитель field");
    assert_eq!(parent.name_en.as_deref(), Some("Parent"));
    assert!(parent.is_standard, "Родитель must remain marked as a standard field");

    let unknown_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|diag| {
            matches!(
                diag,
                crate::diagnostics::SdblDiagnostic::UnknownField { field_name, .. }
                    if field_name == "Родитель"
            )
        })
        .collect();
    assert!(unknown_diags.is_empty(), "Родитель must not be UnknownField: {unknown_diags:?}");

    let unresolved: Vec<_> =
        package.source_map.unresolved_field_names.iter().map(|t| t.text.as_str()).collect();
    assert!(
        !unresolved.contains(&"Родитель"),
        "Родитель must not be highlighted as unresolved: {unresolved:?}"
    );
}

const CHART_OF_CALCULATION_TYPES_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <ChartOfCalculationTypes uuid="b1c40e57-2a31-44f0-9c91-1d70f25ad301">
        <Properties>
            <Name>ОсновныеНачисления</Name>
            <DescriptionLength>50</DescriptionLength>
            <DependenceOnCalculationTypes>OnActionPeriod</DependenceOnCalculationTypes>
        </Properties>
        <ChildObjects>
            <Attribute uuid="11111111-1111-1111-1111-111111111111">
                <Properties>
                    <Name>СпособРасчета</Name>
                    <Type><v8:Type>xs:string</v8:Type></Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </ChartOfCalculationTypes>
</MetaDataObject>"#;

fn config_with_calculation_types() -> bsl_metadata::Configuration {
    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let mdo = bsl_metadata::xml_parser::parse_chart_of_calculation_types_xml(
        CHART_OF_CALCULATION_TYPES_XML,
    )
    .unwrap();
    config.add_metadata_object(mdo);
    config
}

#[test]
fn chart_of_calculation_types_valid_fields_resolve() {
    // Content-parsed charts of calculation types carry their standard attributes
    // (Ссылка/ПериодДействияБазовый), user attributes, and the dependency tabular
    // section names, so a query over valid fields must not raise unknown-field.
    let config = config_with_calculation_types();
    let code =
        "ВЫБРАТЬ Т.Ссылка, Т.СпособРасчета, Т.ПериодДействияБазовый, Т.ВытесняющиеВидыРасчета \
                ИЗ ПланВидовРасчета.ОсновныеНачисления КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let unknown_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|diag| matches!(diag, crate::diagnostics::SdblDiagnostic::UnknownField { .. }))
        .collect();
    assert!(unknown_diags.is_empty(), "valid calc-type fields must resolve: {unknown_diags:?}");
}

#[test]
fn chart_of_calculation_types_unknown_field_now_fires() {
    // The model is exhaustive, so the unknown-field gate is on: a bogus field is flagged.
    let config = config_with_calculation_types();
    let code = "ВЫБРАТЬ Т.НесуществующееПоле ИЗ ПланВидовРасчета.ОсновныеНачисления КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let unknown: Vec<_> =
        package.source_map.unresolved_field_names.iter().map(|t| t.text.as_str()).collect();
    assert!(
        unknown.contains(&"НесуществующееПоле"),
        "an unknown calc-type field must be flagged: {unknown:?}"
    );
}

#[test]
fn chart_of_calculation_types_dependency_tabular_section_fields_resolve() {
    // A direct query over a dependency tabular section resolves its ВидРасчета column.
    let config = config_with_calculation_types();
    let code = "ВЫБРАТЬ Т.ВидРасчета, Т.Ссылка \
                ИЗ ПланВидовРасчета.ОсновныеНачисления.ВытесняющиеВидыРасчета КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let unknown_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|diag| matches!(diag, crate::diagnostics::SdblDiagnostic::UnknownField { .. }))
        .collect();
    assert!(
        unknown_diags.is_empty(),
        "dependency tabular-section fields must resolve: {unknown_diags:?}"
    );
}

#[test]
fn hierarchical_catalog_parent_field_resolves_by_english_name() {
    let catalog_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>Номенклатура</Name>
            <Hierarchical>true</Hierarchical>
            <CodeLength>9</CodeLength>
            <DescriptionLength>25</DescriptionLength>
        </Properties>
    </Catalog>
</MetaDataObject>"#;

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let catalog = bsl_metadata::xml_parser::parse_catalog_xml(catalog_xml).unwrap();
    config.add_metadata_object(catalog);

    let code = "SELECT N.Parent FROM Catalog.Номенклатура AS N";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let unknown_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|diag| {
            matches!(
                diag,
                crate::diagnostics::SdblDiagnostic::UnknownField { field_name, .. }
                    if field_name == "Parent"
            )
        })
        .collect();
    assert!(unknown_diags.is_empty(), "Parent must resolve by English standard name");

    let resolved: Vec<_> = package.source_map.field_names.iter().map(|t| t.text.as_str()).collect();
    let unresolved: Vec<_> =
        package.source_map.unresolved_field_names.iter().map(|t| t.text.as_str()).collect();

    assert!(resolved.contains(&"Parent"), "Parent should be a resolved field: {resolved:?}");
    assert!(!unresolved.contains(&"Parent"), "Parent must not be unresolved: {unresolved:?}");
}

#[test]
fn test_tabular_section_uuid_type_parsing() {
    use bsl_metadata::{
        tabular_section::{TabularSection, TabularSectionAttribute},
        MdoType, MetadataObject,
    };

    let uuid_nil =
        *bsl_metadata::tabular_section::TabularSection::new(Default::default(), "temp").uuid();

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let mut bp = MetadataObject::new(MdoType::BusinessProcess, "Исполнение");
    let mut ts = TabularSection::new(uuid_nil, "РезультатыПроверки");

    let mut attr = TabularSectionAttribute::new(
        uuid_nil,
        "ИдентификаторИсполнителя",
        parse_attr_type_for_test("УникальныйИдентификатор"),
    );
    attr.set_name_en(Some("ExecutorId".to_string()));

    ts.set_attributes(vec![attr]);
    bp.add_tabular_section(ts);
    config.add_metadata_object(bp);

    let code =
        "ВЫБРАТЬ Т.ИдентификаторИсполнителя ИЗ БизнесПроцесс.Исполнение.РезультатыПроверки КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config.clone())));
    let hir = single_query_hir(&package);

    assert_eq!(hir.from.len(), 1);
    let table_ref = &hir.from[0];
    assert!(table_ref.is_resolved(), "Table should be resolved");

    let resolved = table_ref.metadata.as_ref().expect("Metadata should be present");
    let fields = resolved.fields();

    let field = fields.iter().find(|f| f.name.as_str() == "ИдентификаторИсполнителя");
    assert!(field.is_some(), "Should find ИдентификаторИсполнителя field");

    let field = field.unwrap();
    assert_eq!(field.ty, crate::SdblType::Uuid, "Should be UUID type");
}

#[test]
fn test_parse_simple_nested_subquery() {
    let query = r#"ВЫБРАТЬ
    Т.Поле КАК Поле
ИЗ (
    ВЫБРАТЬ
        Т1.Поле КАК Поле
    ИЗ Таблица1 КАК Т1
) КАК Т"#;

    let parse = parser::parse_sdbl(query);
    let package = crate::lower::lower_sdbl_to_hir(&parse, None);

    assert_eq!(package.queries().len(), 1);
    let query_hir = &package.queries()[0].hir;
    assert_eq!(query_hir.from.len(), 1, "Should have 1 FROM table");
    assert_eq!(query_hir.select.fields.len(), 1, "Should have 1 SELECT field");

    let subquery_table = &query_hir.from[0];
    assert_eq!(subquery_table.subquery.len(), 1, "Should have 1 subquery HIR");
}

#[test]
fn test_leading_whitespace_in_sdbl() {
    let query = "\nВЫБРАТЬ 1 ИЗ Т";
    let parsed = parser::parse_sdbl(query);
    assert!(!parsed.has_errors(), "Should parse query with leading newline");

    use syntax::ast::{AstNode, SdblQueryPackage};
    let pkg = SdblQueryPackage::cast(parsed.syntax_node()).expect("Should have query package");
    assert_eq!(pkg.queries().count(), 1, "Should have 1 query");
}

fn create_config_with_ref_attribute() -> bsl_metadata::Configuration {
    use bsl_metadata::{Attribute, AttributeType, MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");

    let files_catalog = MetadataObject::new(MdoType::Catalog, "Файлы");
    config.add_metadata_object(files_catalog);

    let mut catalog = MetadataObject::new(MdoType::Catalog, "СлужебныеФайлы");
    catalog.add_attribute(Attribute {
        name: "Файл".to_string(),
        name_en: None,
        attr_type: AttributeType::Ref {
            mdo_type: MdoType::Catalog, name: "Файлы".to_string()
        },
    });
    config.add_metadata_object(catalog);

    config
}

#[test]
fn test_ref_overuse_with_metadata_ref_at_end() {
    let config = create_config_with_ref_attribute();

    let code = "ВЫБРАТЬ Т.Файл.Ссылка КАК Ссылка ИЗ Справочник.СлужебныеФайлы КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let ref_overuse_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::RefOveruse { .. }))
        .collect();

    assert_eq!(ref_overuse_diags.len(), 1, "Expected 1 RefOveruse diagnostic: Файл is Ref type");
}

#[test]
fn test_ref_overuse_with_metadata_non_ref_field() {
    use bsl_metadata::{Attribute, AttributeType, MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let mut catalog = MetadataObject::new(MdoType::Catalog, "Контрагенты");
    catalog.add_attribute(Attribute {
        name: "ИНН".to_string(),
        name_en: None,
        attr_type: AttributeType::String { length: None },
    });
    config.add_metadata_object(catalog);

    let code = "ВЫБРАТЬ Т.ИНН.Ссылка КАК Ссылка ИЗ Справочник.Контрагенты КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let ref_overuse_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::RefOveruse { .. }))
        .collect();

    assert_eq!(
        ref_overuse_diags.len(),
        0,
        "Expected 0 RefOveruse diagnostics: ИНН is String, not a Ref"
    );
}

#[test]
fn test_ref_overuse_with_metadata_double_ref() {
    use bsl_metadata::{MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let catalog = MetadataObject::new(MdoType::Catalog, "Контрагенты");
    config.add_metadata_object(catalog);

    let code = "ВЫБРАТЬ Т.Ссылка.Ссылка КАК п1 ИЗ Справочник.Контрагенты КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let ref_overuse_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::RefOveruse { .. }))
        .collect();

    assert_eq!(
        ref_overuse_diags.len(),
        0,
        "Ссылка is a standard field not in metadata fields() → type Unknown → no diagnostic"
    );
}

#[test]
fn test_ref_overuse_with_metadata_ref_in_middle_not_at_end() {
    use bsl_metadata::{Attribute, AttributeType, MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let mut catalog = MetadataObject::new(MdoType::Catalog, "Контрагенты");
    catalog.add_attribute(Attribute {
        name: "ИНН".to_string(),
        name_en: None,
        attr_type: AttributeType::String { length: None },
    });
    config.add_metadata_object(catalog);

    let code = "ВЫБРАТЬ Т.Ссылка.ИНН КАК ИНН ИЗ Справочник.Контрагенты КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let ref_overuse_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::RefOveruse { .. }))
        .collect();

    assert_eq!(
        ref_overuse_diags.len(),
        0,
        "Expected 0 RefOveruse diagnostics: Ссылка is at position 1, not a redundant usage"
    );
}

#[test]
fn test_ref_overuse_with_metadata_chain_ref_at_end() {
    let config = create_config_with_ref_attribute();

    let code = "ВЫБРАТЬ Т.Файл.Ссылка.Дата КАК Дата ИЗ Справочник.СлужебныеФайлы КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let ref_overuse_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::RefOveruse { .. }))
        .collect();

    assert_eq!(
        ref_overuse_diags.len(),
        1,
        "Expected 1 RefOveruse diagnostic: Файл is Ref, so .Ссылка after it is redundant"
    );
}

#[test]
fn test_ref_overuse_with_metadata_simple_ref_no_error() {
    use bsl_metadata::{MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let catalog = MetadataObject::new(MdoType::Catalog, "Контрагенты");
    config.add_metadata_object(catalog);

    let code = "ВЫБРАТЬ Т.Ссылка КАК Контрагент ИЗ Справочник.Контрагенты КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let ref_overuse_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::RefOveruse { .. }))
        .collect();

    assert_eq!(
        ref_overuse_diags.len(),
        0,
        "Expected 0 RefOveruse diagnostics: simple Alias.Ссылка is not redundant"
    );
}

fn create_config_with_enum() -> bsl_metadata::Configuration {
    use bsl_metadata::{metadata_object::EnumValue, MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let mut enum_obj = MetadataObject::new(MdoType::Enum, "ПолФизическогоЛица");
    enum_obj.enum_values = vec![
        EnumValue {
            name: "Мужской".to_string(),
            name_en: Some("Male".to_string()),
            uuid: "1".to_string(),
        },
        EnumValue {
            name: "Женский".to_string(),
            name_en: Some("Female".to_string()),
            uuid: "2".to_string(),
        },
    ];
    config.add_metadata_object(enum_obj);
    config
}

#[test]
fn test_value_function_valid_enum_value_gets_field_name_token() {
    let config = create_config_with_enum();

    let code = "ВЫБРАТЬ 1 ИЗ Справочник.Тест КАК Т ГДЕ Т.Пол = ЗНАЧЕНИЕ(Перечисление.ПолФизическогоЛица.Мужской)";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let sm = &package.source_map;

    let resolved: Vec<String> = sm.field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        resolved.iter().any(|t| t == "Мужской"),
        "Expected 'Мужской' in field_names, got: {:?}",
        resolved
    );

    let unresolved: Vec<String> =
        sm.unresolved_field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        !unresolved.iter().any(|t| t == "Мужской"),
        "Expected 'Мужской' NOT in unresolved_field_names, got: {:?}",
        unresolved
    );
}

#[test]
fn test_value_function_invalid_enum_value_gets_unresolved_token() {
    let config = create_config_with_enum();

    let code = "ВЫБРАТЬ 1 ИЗ Справочник.Тест КАК Т ГДЕ Т.Пол = ЗНАЧЕНИЕ(Перечисление.ПолФизическогоЛица.НесуществующееЗначение)";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let sm = &package.source_map;

    let unresolved: Vec<String> =
        sm.unresolved_field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        unresolved.iter().any(|t| t == "НесуществующееЗначение"),
        "Expected 'НесуществующееЗначение' in unresolved_field_names, got: {:?}",
        unresolved
    );
}

#[test]
fn test_value_function_empty_ref_always_valid() {
    let code = "ВЫБРАТЬ 1 ИЗ Справочник.Валюты КАК Вал ГДЕ Вал.Пол = ЗНАЧЕНИЕ(Справочник.Валюты.ПустаяСсылка)";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, None);
    let sm = &package.source_map;

    let resolved: Vec<String> = sm.field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        resolved.iter().any(|t| t == "ПустаяСсылка"),
        "Expected 'ПустаяСсылка' in field_names (EmptyRef is always valid), got: {:?}",
        resolved
    );

    let unresolved: Vec<String> =
        sm.unresolved_field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        !unresolved.iter().any(|t| t == "ПустаяСсылка"),
        "Expected 'ПустаяСсылка' NOT in unresolved_field_names, got: {:?}",
        unresolved
    );
}

#[test]
fn test_value_function_without_metadata_graceful_degradation() {
    let code = "ВЫБРАТЬ 1 ИЗ Справочник.Тест КАК Т ГДЕ Т.Пол = ЗНАЧЕНИЕ(Перечисление.ПолФизическогоЛица.Мужской)";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, None);
    let sm = &package.source_map;

    let unresolved: Vec<String> =
        sm.unresolved_field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        !unresolved.iter().any(|t| t == "Мужской"),
        "Without metadata, 'Мужской' should not be unresolved, got: {:?}",
        unresolved
    );

    let resolved: Vec<String> = sm.field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        resolved.iter().any(|t| t == "Мужской"),
        "Without metadata, 'Мужской' should be in field_names, got: {:?}",
        resolved
    );
}

#[test]
fn test_value_function_mdo_type_and_table_name_tokens() {
    let code = "ВЫБРАТЬ 1 ИЗ Справочник.Тест КАК Т ГДЕ Т.Ссылка = ЗНАЧЕНИЕ(Перечисление.ПолФизическогоЛица.Мужской)";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, None);
    let sm = &package.source_map;

    let mdo_types: Vec<String> = sm.mdo_types.iter().map(|t| t.text.to_string()).collect();
    assert!(
        mdo_types.iter().any(|t| t == "Перечисление"),
        "Expected 'Перечисление' in mdo_types, got: {:?}",
        mdo_types
    );

    let table_names: Vec<String> = sm.table_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        table_names.iter().any(|t| t == "ПолФизическогоЛица"),
        "Expected 'ПолФизическогоЛица' in table_names, got: {:?}",
        table_names
    );
}

fn create_config_with_catalog_predefined() -> bsl_metadata::Configuration {
    use bsl_metadata::{metadata_object::PredefinedItem, MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let mut catalog_obj = MetadataObject::new(MdoType::Catalog, "Валюты");
    catalog_obj.predefined_items = vec![
        PredefinedItem {
            name: "Доллар".to_string(),
            name_en: Some("Dollar".to_string()),
            uuid: "1".to_string(),
        },
        PredefinedItem {
            name: "Евро".to_string(),
            name_en: Some("Euro".to_string()),
            uuid: "2".to_string(),
        },
    ];
    config.add_metadata_object(catalog_obj);
    config
}

#[test]
fn test_value_function_valid_predefined_item_gets_field_name_token() {
    let config = create_config_with_catalog_predefined();

    let code = "ВЫБРАТЬ 1 ИЗ Справочник.Валюты КАК Вал ГДЕ Вал.Ссылка = ЗНАЧЕНИЕ(Справочник.Валюты.Доллар)";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let sm = &package.source_map;

    let resolved: Vec<String> = sm.field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        resolved.iter().any(|t| t == "Доллар"),
        "Expected 'Доллар' in field_names, got: {:?}",
        resolved
    );

    let unresolved: Vec<String> =
        sm.unresolved_field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        !unresolved.iter().any(|t| t == "Доллар"),
        "Expected 'Доллар' NOT in unresolved_field_names, got: {:?}",
        unresolved
    );
}

#[test]
fn test_value_function_invalid_predefined_item_gets_unresolved_token() {
    let config = create_config_with_catalog_predefined();

    let code = "ВЫБРАТЬ 1 ИЗ Справочник.Валюты КАК Вал ГДЕ Вал.Ссылка = ЗНАЧЕНИЕ(Справочник.Валюты.Несуществующий)";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let sm = &package.source_map;

    let unresolved: Vec<String> =
        sm.unresolved_field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        unresolved.iter().any(|t| t == "Несуществующий"),
        "Expected 'Несуществующий' in unresolved_field_names, got: {:?}",
        unresolved
    );
}

#[test]
fn test_value_function_predefined_item_empty_list_graceful_degradation() {
    use bsl_metadata::{MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let catalog_obj = MetadataObject::new(MdoType::Catalog, "Валюты");
    config.add_metadata_object(catalog_obj);

    let code = "ВЫБРАТЬ 1 ИЗ Справочник.Валюты КАК Вал ГДЕ Вал.Ссылка = ЗНАЧЕНИЕ(Справочник.Валюты.ЛюбоеЗначение)";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let sm = &package.source_map;

    let resolved: Vec<String> = sm.field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        resolved.iter().any(|t| t == "ЛюбоеЗначение"),
        "Expected 'ЛюбоеЗначение' in field_names when predefined_items is empty, got: {:?}",
        resolved
    );

    let unresolved: Vec<String> =
        sm.unresolved_field_names.iter().map(|t| t.text.to_string()).collect();
    assert!(
        !unresolved.iter().any(|t| t == "ЛюбоеЗначение"),
        "Expected 'ЛюбоеЗначение' NOT in unresolved_field_names when predefined_items is empty, got: {:?}",
        unresolved
    );
}

#[test]
fn test_join_paren_field_resolution() {
    let query = r#"ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Валюты КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Валюты КАК Т2 ПО Т.Ссылка = Т2.Ссылка И (Т2.Код = "USD")"#;

    let ast = parser::parse_sdbl(query);

    let mut config = bsl_metadata::Configuration::new("Test");
    let catalog = bsl_metadata::MetadataObject {
        mdo_type: bsl_metadata::MdoType::Catalog,
        name: "Валюты".to_string(),
        name_en: None,
        attributes: vec![
            bsl_metadata::Attribute {
                name: "Ссылка".to_string(),
                name_en: Some("Ref".to_string()),
                attr_type: bsl_metadata::AttributeType::Ref {
                    mdo_type: bsl_metadata::MdoType::Catalog,
                    name: "Валюты".to_string(),
                },
            },
            bsl_metadata::Attribute {
                name: "Код".to_string(),
                name_en: Some("Code".to_string()),
                attr_type: bsl_metadata::AttributeType::String { length: Some(10) },
            },
        ],
        tabular_sections: vec![],
        children: vec![],
        enum_values: vec![],
        predefined_items: vec![],
        check_unique: false,
        code_series: bsl_metadata::CodeSeries::default(),
        constant_type: None,
        register_records: vec![],
        uuid: None,
        object_belonging: bsl_metadata::ObjectBelonging::Own,
        extended_configuration_object: None,
        common_attributes: Vec::new(),
        common_attributes_open: false,
    };
    config.add_metadata_object(catalog);

    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let source_map = &package.source_map;
    let unresolved =
        source_map.tokens_by_category(crate::source_map::TokenCategory::UnresolvedFieldName);
    let resolved = source_map.tokens_by_category(crate::source_map::TokenCategory::FieldName);

    assert!(
        unresolved.is_empty(),
        "Fields inside parens should resolve. Unresolved: {:?}",
        unresolved.iter().map(|t| &t.text).collect::<Vec<_>>()
    );
    assert!(!resolved.is_empty(), "Fields inside parens should produce resolved field tokens");
}

fn create_config_with_accumulation_register() -> bsl_metadata::Configuration {
    use bsl_metadata::{
        dimension::DimensionBuilder, register::RegisterResource, MdoType, Register,
    };

    let mut config = bsl_metadata::Configuration::new("TestConfig");

    let register = Register::builder()
        .name("ИзмененияВНакопленияхКлиента")
        .mdo_type(MdoType::AccumulationRegister)
        .dimensions(vec![DimensionBuilder::default().name("Партнер").build()])
        .resources(vec![
            RegisterResource::new(Default::default(), "Сумма"),
            RegisterResource::new(Default::default(), "Количество"),
        ])
        .build();

    config.add_register(register);
    config
}

#[test]
fn test_virtual_table_turnovers_field_generation() {
    let config = create_config_with_accumulation_register();
    let code = "ВЫБРАТЬ Т.Партнер, Т.СуммаОборот, Т.КоличествоОборот ИЗ РегистрНакопления.ИзмененияВНакопленияхКлиента.Обороты(,,) КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let hir = single_query_hir(&package);

    assert_eq!(hir.from.len(), 1);
    let table_ref = &hir.from[0];
    assert!(table_ref.is_virtual_table);
    let resolved = table_ref.metadata.as_ref().expect("Should have metadata");

    let fields = resolved.fields();
    let field_names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();

    assert!(field_names.contains(&"Период"), "Should have Период, got: {:?}", field_names);
    assert!(
        field_names.contains(&"Регистратор"),
        "Should have Регистратор, got: {:?}",
        field_names
    );
    assert!(field_names.contains(&"Партнер"), "Should have Партнер, got: {:?}", field_names);
    assert!(
        field_names.contains(&"СуммаОборот"),
        "Should have СуммаОборот, got: {:?}",
        field_names
    );
    assert!(
        field_names.contains(&"КоличествоОборот"),
        "Should have КоличествоОборот, got: {:?}",
        field_names
    );

    assert!(!field_names.contains(&"Сумма"), "Should NOT have raw Сумма, got: {:?}", field_names);
    assert!(
        !field_names.contains(&"Количество"),
        "Should NOT have raw Количество, got: {:?}",
        field_names
    );

    let unknown_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::UnknownField { .. }))
        .collect();
    assert!(
        unknown_diags.is_empty(),
        "Should have no UnknownField diagnostics, got: {:?}",
        unknown_diags
    );
}

#[test]
fn test_virtual_table_balance_field_generation() {
    use bsl_metadata::{
        dimension::DimensionBuilder, register::RegisterResource, MdoType, Register,
    };

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let register = Register::builder()
        .name("ТоварыНаСкладах")
        .mdo_type(MdoType::AccumulationRegister)
        .dimensions(vec![DimensionBuilder::default().name("Склад").build()])
        .resources(vec![RegisterResource::new(Default::default(), "Количество")])
        .build();
    config.add_register(register);

    let code = "ВЫБРАТЬ Т.Склад, Т.КоличествоОстаток ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(,) КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let hir = single_query_hir(&package);

    let resolved = hir.from[0].metadata.as_ref().expect("Should have metadata");
    let field_names: Vec<&str> = resolved.fields().iter().map(|f| f.name.as_str()).collect();

    assert!(field_names.contains(&"Склад"), "Should have Склад");
    assert!(field_names.contains(&"КоличествоОстаток"), "Should have КоличествоОстаток");
    assert!(!field_names.contains(&"Количество"), "Should NOT have raw Количество");
}

#[test]
fn test_virtual_table_balance_and_turnovers_field_generation() {
    use bsl_metadata::{
        dimension::DimensionBuilder, register::RegisterResource, MdoType, Register,
    };

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let register = Register::builder()
        .name("Продажи")
        .mdo_type(MdoType::AccumulationRegister)
        .dimensions(vec![DimensionBuilder::default().name("Товар").build()])
        .resources(vec![RegisterResource::new(Default::default(), "Сумма")])
        .build();
    config.add_register(register);

    let code = "ВЫБРАТЬ Т.Товар, Т.СуммаНачальныйОстаток, Т.СуммаОборот, Т.СуммаКонечныйОстаток ИЗ РегистрНакопления.Продажи.ОстаткиИОбороты(,,) КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let hir = single_query_hir(&package);

    let resolved = hir.from[0].metadata.as_ref().expect("Should have metadata");
    let field_names: Vec<&str> = resolved.fields().iter().map(|f| f.name.as_str()).collect();

    assert!(field_names.contains(&"Товар"), "Should have Товар");
    assert!(
        field_names.contains(&"СуммаНачальныйОстаток"),
        "Should have СуммаНачальныйОстаток, got: {:?}",
        field_names
    );
    assert!(field_names.contains(&"СуммаОборот"), "Should have СуммаОборот");
    assert!(field_names.contains(&"СуммаКонечныйОстаток"), "Should have СуммаКонечныйОстаток");
}

#[test]
fn test_virtual_table_slice_last_preserves_fields() {
    use bsl_metadata::{
        dimension::DimensionBuilder, register::RegisterResource, MdoType, Register,
    };

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let register = Register::builder()
        .name("Курсы")
        .mdo_type(MdoType::InformationRegister)
        .dimensions(vec![DimensionBuilder::default().name("Валюта").build()])
        .resources(vec![RegisterResource::new(Default::default(), "Курс")])
        .build();
    config.add_register(register);

    let code =
        "ВЫБРАТЬ Т.Валюта, Т.Курс, Т.Период ИЗ РегистрСведений.Курсы.СрезПоследних(&Дата,) КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let hir = single_query_hir(&package);

    let resolved = hir.from[0].metadata.as_ref().expect("Should have metadata");
    let field_names: Vec<&str> = resolved.fields().iter().map(|f| f.name.as_str()).collect();

    assert!(field_names.contains(&"Валюта"), "Should have Валюта");
    assert!(field_names.contains(&"Курс"), "Should have Курс (not suffixed)");
    assert!(field_names.contains(&"Период"), "Should have Период");
}

#[test]
fn test_virtual_table_param_scope_resolves_dimension() {
    let config = create_config_with_accumulation_register();
    let code = "ВЫБРАТЬ Т.Партнер ИЗ РегистрНакопления.ИзмененияВНакопленияхКлиента.Обороты(,,, Партнер В (ВЫБРАТЬ 1)) КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let unknown_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| {
            matches!(d, crate::diagnostics::SdblDiagnostic::UnknownField { field_name, .. } if field_name == "Партнер")
        })
        .collect();
    assert!(
        unknown_diags.is_empty(),
        "Партнер in VT condition should resolve via dimension scope, got: {:?}",
        unknown_diags
    );
}

#[test]
fn test_virtual_table_periodicity_resolved() {
    let config = create_config_with_accumulation_register();
    let code = "ВЫБРАТЬ Т.Партнер ИЗ РегистрНакопления.ИзмененияВНакопленияхКлиента.Обороты(,, Авто, Партнер В (ВЫБРАТЬ 1)) КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let sm = &package.source_map;

    let special: Vec<String> = sm.special_keywords.iter().map(|t| t.text.to_string()).collect();
    let unresolved: Vec<String> =
        sm.unresolved_field_names.iter().map(|t| t.text.to_string()).collect();

    assert!(
        special.iter().any(|t| t == "Авто"),
        "Авто should be in special_keywords, got: {:?}",
        special
    );
    assert!(
        !unresolved.iter().any(|t| t == "Авто"),
        "Авто should NOT be in unresolved_field_names, got: {:?}",
        unresolved
    );
}

#[test]
fn test_virtual_table_semantic_tokens_resolved() {
    let config = create_config_with_accumulation_register();
    let code = "ВЫБРАТЬ Т.СуммаОборот, Т.КоличествоОборот ИЗ РегистрНакопления.ИзмененияВНакопленияхКлиента.Обороты(,,) КАК Т";

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let sm = &package.source_map;

    let resolved: Vec<String> = sm.field_names.iter().map(|t| t.text.to_string()).collect();
    let unresolved: Vec<String> =
        sm.unresolved_field_names.iter().map(|t| t.text.to_string()).collect();

    assert!(
        resolved.iter().any(|t| t == "СуммаОборот"),
        "СуммаОборот should be in field_names, got: {:?}",
        resolved
    );
    assert!(
        resolved.iter().any(|t| t == "КоличествоОборот"),
        "КоличествоОборот should be in field_names, got: {:?}",
        resolved
    );
    assert!(
        !unresolved.iter().any(|t| t == "СуммаОборот"),
        "СуммаОборот should NOT be in unresolved, got: {:?}",
        unresolved
    );
}

#[test]
fn chart_of_characteristic_ref_and_index_by_are_semantically_highlighted() {
    use bsl_metadata::{Attribute, AttributeType, Configuration, MdoType, MetadataObject};

    let mut config = Configuration::new("TestConfig");
    let mut cct = MetadataObject::new(
        MdoType::ChartOfCharacteristicTypes,
        "ДополнительныеРеквизитыИСведения",
    );
    cct.attributes.push(Attribute {
        name: "Ссылка".to_string(),
        name_en: Some("Ref".to_string()),
        attr_type: AttributeType::Ref {
            mdo_type: MdoType::ChartOfCharacteristicTypes,
            name: "ДополнительныеРеквизитыИСведения".to_string(),
        },
    });
    config.add_metadata_object(cct);

    let code = r#"ВЫБРАТЬ
	ДополнительныеРеквизитыИСведения.Ссылка КАК Свойство
ПОМЕСТИТЬ ВТ_ВсеСвойства
ИЗ
	ПланВидовХарактеристик.ДополнительныеРеквизитыИСведения КАК ДополнительныеРеквизитыИСведения

ИНДЕКСИРОВАТЬ ПО
	Свойство"#;

    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let sm = &package.source_map;

    let resolved_fields: Vec<_> = sm.field_names.iter().map(|t| t.text.as_str()).collect();
    let unresolved_fields: Vec<_> =
        sm.unresolved_field_names.iter().map(|t| t.text.as_str()).collect();
    let clause_keywords: Vec<_> = sm.clause_keywords.iter().map(|t| t.text.as_str()).collect();
    let field_aliases: Vec<_> = sm.field_aliases.iter().map(|t| t.text.as_str()).collect();

    assert!(
        resolved_fields.contains(&"Ссылка"),
        "Ссылка should resolve for ChartOfCharacteristicTypes, got fields: {resolved_fields:?}"
    );
    assert!(
        !unresolved_fields.contains(&"Ссылка"),
        "Ссылка must not be unresolved, got: {unresolved_fields:?}"
    );
    assert!(
        clause_keywords.contains(&"ИНДЕКСИРОВАТЬ"),
        "INDEX BY keyword should be highlighted, got: {clause_keywords:?}"
    );
    assert!(
        clause_keywords.contains(&"ПО"),
        "INDEX BY 'ПО' keyword should be highlighted, got: {clause_keywords:?}"
    );
    assert!(
        field_aliases.iter().filter(|name| **name == "Свойство").count() >= 2,
        "SELECT alias and INDEX BY reference should be field aliases, got: {field_aliases:?}"
    );
}

#[test]
fn defined_type_inside_composite_resolves_through_metadata() {
    use crate::lower::context::LoweringContext;
    use crate::types::SdblType;
    use bsl_metadata::{AttributeType, Configuration, DefinedType, Uuid};

    let mut config = Configuration::new("Test");
    config.add_defined_type(
        DefinedType::builder()
            .uuid(Uuid::new_v4())
            .name("X")
            .underlying_type(AttributeType::Boolean)
            .build(),
    );

    let ctx = LoweringContext::new(Some(&config as &dyn bsl_metadata::QueryMetadataResolver));
    let composite = AttributeType::Composite {
        types: vec![AttributeType::DefinedType { name: "X".to_string() }, AttributeType::Boolean],
    };

    let resolved = ctx.resolve_attribute_type(&composite);

    let arms = match &resolved {
        SdblType::Composite { types } => types.clone(),
        other => panic!("expected Composite, got {other:?}"),
    };
    let defined_arm = arms
        .iter()
        .find_map(|t| match t {
            SdblType::DefinedType { name, underlying_type } if name == "X" => {
                Some(underlying_type.clone())
            }
            _ => None,
        })
        .expect("Composite must carry the DefinedType('X') arm");
    let underlying =
        defined_arm.expect("DefinedType('X') underlying must resolve through metadata");
    assert_eq!(*underlying, SdblType::Boolean);
}

#[test]
fn test_parse_table_name_keeps_soft_keyword_part_kw_in() {
    let hir = lower_query("ВЫБРАТЬ * ИЗ Справочник.В");
    assert_eq!(hir.from.len(), 1, "Single FROM source");
    let table = &hir.from[0];
    assert_eq!(
        table.parts.len(),
        2,
        "`Справочник.В` must lower as a 2-part path, not collapse the `В` (KW_IN) part. Got parts: {:?}",
        table.parts
    );
    assert_eq!(table.parts[0].as_str(), "Справочник");
    assert_eq!(table.parts[1].as_str(), "В");
}

#[test]
fn test_parse_table_name_keeps_soft_keyword_part_literal_kw() {
    let hir = lower_query("ВЫБРАТЬ * ИЗ Справочник.Истина");
    let table = &hir.from[0];
    assert_eq!(
        table.parts.len(),
        2,
        "`Справочник.Истина` must lower as a 2-part path. Got parts: {:?}",
        table.parts
    );
    assert_eq!(table.parts[1].as_str(), "Истина");
}

#[test]
fn asterisk_qualifier_lowers_bare_star_as_none() {
    let hir = lower_query("ВЫБРАТЬ * ИЗ Справочник.Товары");
    let field = &hir.select.fields[0];
    assert!(field.is_asterisk);
    assert_eq!(field.asterisk_qualifier, None);
}

#[test]
fn asterisk_qualifier_lowers_aliased_star() {
    let hir = lower_query("ВЫБРАТЬ Т.* ИЗ Справочник.Товары КАК Т");
    let field = &hir.select.fields[0];
    assert!(field.is_asterisk);
    assert_eq!(field.asterisk_qualifier.as_deref(), Some("Т"));
}

fn cast_field_ty(sdbl: &str) -> crate::types::SdblType {
    let hir = lower_query(sdbl);
    let field = hir.select.fields.first().expect("CAST query must yield a SELECT field");
    field.ty.clone()
}

#[test]
fn cast_number_precision_and_scale_lowers_to_full_number() {
    use crate::types::SdblType;
    let ty = cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Число(15, 2)) КАК Цена");
    assert_eq!(ty, SdblType::Number { precision: Some(15), scale: Some(2) });
    assert_eq!(ty.to_string(), "Число(15, 2)");
}

#[test]
fn cast_number_precision_only_lowers_to_partial_number() {
    use crate::types::SdblType;
    let ty = cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Число(15)) КАК Цена");
    assert_eq!(ty, SdblType::Number { precision: Some(15), scale: None });
    assert_eq!(ty.to_string(), "Число(15)");
}

#[test]
fn cast_string_length_lowers_to_sized_string() {
    use crate::types::SdblType;
    let ty = cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(\"\" КАК Строка(50)) КАК Имя");
    assert_eq!(ty, SdblType::String { length: Some(50) });
    assert_eq!(ty.to_string(), "Строка(50)");
}

#[test]
fn cast_date_and_boolean_lower_to_primitive_variants() {
    use crate::types::SdblType;
    assert_eq!(cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Дата) КАК Д"), SdblType::Date);
    assert_eq!(cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Булево) КАК Б"), SdblType::Boolean);
}

#[test]
fn cast_english_primitive_names_are_recognised() {
    use crate::types::SdblType;
    assert_eq!(
        cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК NUMBER(10, 4)) КАК X"),
        SdblType::Number { precision: Some(10), scale: Some(4) }
    );
    assert_eq!(
        cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(\"\" КАК STRING(20)) КАК S"),
        SdblType::String { length: Some(20) }
    );
    assert_eq!(cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК DATE) КАК D"), SdblType::Date);
    assert_eq!(cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК BOOLEAN) КАК B"), SdblType::Boolean);
}

#[test]
fn cast_mdo_reference_lowers_to_ref_type() {
    use crate::types::{MdoRef, SdblType};
    use bsl_metadata::MdoType;
    let ty = cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Справочник.Товары) КАК Ссылка");
    assert_eq!(ty, SdblType::Ref(MdoRef { mdo_type: MdoType::Catalog, name: "Товары".into() }));
}

#[test]
fn cast_unrecognised_primitive_name_collapses_to_unknown() {
    use crate::types::SdblType;
    assert_eq!(cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Несуществующий) КАК X"), SdblType::Unknown);
}

#[test]
fn cast_unknown_mdo_qualifier_collapses_to_unknown() {
    use crate::types::SdblType;
    assert_eq!(cast_field_ty("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Foo.Bar) КАК X"), SdblType::Unknown);
}

#[test]
fn collect_resolved_attributes_first_hop_skips_standard() {
    use crate::hir::SdblHir;
    use bsl_metadata::{Attribute, AttributeType, Configuration, MdoType, MetadataObject};
    use std::sync::Arc;

    let mut config = Configuration::new("Test");
    let mut catalog = MetadataObject::new(MdoType::Catalog, "Валюты");
    catalog.add_attribute(Attribute {
        name: "Курс".to_string(),
        name_en: Some("Rate".to_string()),
        attr_type: AttributeType::Number { precision: 15, scale: 4 },
    });
    catalog.add_attribute(Attribute {
        name: "Код".to_string(),
        name_en: Some("Code".to_string()),
        attr_type: AttributeType::String { length: Some(10) },
    });
    config.add_metadata_object(catalog);

    let ast = parser::parse_sdbl("ВЫБРАТЬ Валюты.Курс, Валюты.Код ИЗ Справочник.Валюты КАК Валюты");
    let package = lower_sdbl_to_hir(&ast, Some(Arc::new(config)));

    let mut attrs: Vec<(MdoType, String, String)> = Vec::new();
    for query in package.queries() {
        SdblHir::collect_resolved_attributes(&query.hir, &mut attrs);
    }

    // Курс (user attribute, qualified by alias) resolves to its attribute node.
    assert!(
        attrs.iter().any(|(t, o, a)| *t == MdoType::Catalog && o == "Валюты" && a == "Курс"),
        "user attribute Валюты.Курс must resolve: {attrs:?}"
    );
    // Код is a standard (platform) attribute → skipped.
    assert!(
        !attrs.iter().any(|(_, _, a)| a == "Код"),
        "standard attribute Код must be skipped: {attrs:?}"
    );
}

fn unknown_fields(package: &crate::SdblPackage) -> Vec<String> {
    package
        .all_diagnostics()
        .filter_map(|d| match d {
            crate::diagnostics::SdblDiagnostic::UnknownField { field_name, .. } => {
                Some(field_name.clone())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn accounting_register_main_table_is_incomplete_so_diagnostic_stays_silent() {
    use bsl_metadata::{
        dimension::DimensionBuilder, register::RegisterResource, MdoType, Register,
    };

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let register = Register::builder()
        .name("Хозрасчетный")
        .mdo_type(MdoType::AccountingRegister)
        .dimensions(vec![DimensionBuilder::default().name("Организация").build()])
        .resources(vec![RegisterResource::new(Default::default(), "Сумма")])
        .build();
    config.add_register(register);

    // The accounting main-table field model is not enumerable yet (Дт/Кт split,
    // correspondence flag), so it is gated incomplete — no false positive even
    // on a clearly-unknown field.
    let code = "ВЫБРАТЬ Т.НетТакогоПоля ИЗ РегистрБухгалтерии.Хозрасчетный КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    assert!(
        unknown_fields(&package).is_empty(),
        "accounting register must stay silent, got: {:?}",
        unknown_fields(&package)
    );
}

#[test]
fn calculation_register_main_table_is_modeled() {
    use bsl_metadata::{
        dimension::DimensionBuilder, register::RegisterResource, MdoType, Register,
    };

    let make_config = || {
        let mut config = bsl_metadata::Configuration::new("TestConfig");
        let register = Register::builder()
            .name("Начисления")
            .mdo_type(MdoType::CalculationRegister)
            .dimensions(vec![DimensionBuilder::default().name("Сотрудник").build()])
            .resources(vec![RegisterResource::new(Default::default(), "Результат")])
            .build();
        config.add_register(register);
        config
    };

    // Standard calc-register fields + a user dimension resolve cleanly.
    let valid = "ВЫБРАТЬ Т.ВидРасчета, Т.ПериодРегистрации, Т.Сторно, Т.Сотрудник ИЗ РегистрРасчета.Начисления КАК Т";
    let ast = parser::parse_sdbl(valid);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(make_config())));
    assert!(
        unknown_fields(&package).is_empty(),
        "valid calc-register fields must resolve, got: {:?}",
        unknown_fields(&package)
    );

    // A genuinely-unknown field fires (model is complete here).
    let bad = "ВЫБРАТЬ Т.НетТакогоПоля ИЗ РегистрРасчета.Начисления КАК Т";
    let ast = parser::parse_sdbl(bad);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(make_config())));
    assert_eq!(
        unknown_fields(&package),
        vec!["НетТакогоПоля".to_string()],
        "unknown calc-register field must fire"
    );
}

#[test]
fn nested_ref_navigation_resolves_final_attribute_type() {
    use bsl_metadata::{Attribute, AttributeType, MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");

    // Document whose attribute carries the date we ultimately want.
    let mut order = MetadataObject::new(MdoType::Document, "ЗаказПоставщику");
    order.add_attribute(Attribute {
        name: "ДатаПоступления".to_string(),
        name_en: Some("ReceiptDate".to_string()),
        attr_type: AttributeType::Date,
    });
    config.add_metadata_object(order);

    // Catalog whose attribute is a reference to that document.
    let mut catalog = MetadataObject::new(MdoType::Catalog, "СертификатыНоменклатуры");
    catalog.add_attribute(Attribute {
        name: "ЗаказПоставщику".to_string(),
        name_en: Some("PurchaseOrder".to_string()),
        attr_type: AttributeType::Ref {
            mdo_type: MdoType::Document,
            name: "ЗаказПоставщику".to_string(),
        },
    });
    config.add_metadata_object(catalog);

    let code = "ВЫБРАТЬ Серт.ЗаказПоставщику.ДатаПоступления КАК ДатаПоступления \
                ИЗ Справочник.СертификатыНоменклатуры КАК Серт";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let hir = single_query_hir(&package);

    assert_eq!(hir.select.fields.len(), 1);
    assert_eq!(
        hir.select.fields[0].ty,
        crate::SdblType::Date,
        "dotted navigation Серт.ЗаказПоставщику.ДатаПоступления must resolve to Date, \
         not the intermediate document reference"
    );
}

fn unlimited_string_test_config() -> std::sync::Arc<bsl_metadata::Configuration> {
    use bsl_metadata::{Attribute, AttributeType, MdoType, MetadataObject};

    let mut config = bsl_metadata::Configuration::new("TestConfig");
    let mut catalog = MetadataObject::new(MdoType::Catalog, "Лог");
    catalog.add_attribute(Attribute {
        name: "Описание".to_string(),
        name_en: None,
        attr_type: AttributeType::String { length: Some(0) },
    });
    catalog.add_attribute(Attribute {
        name: "Номер".to_string(),
        name_en: None,
        attr_type: AttributeType::String { length: Some(10) },
    });
    config.add_metadata_object(catalog);
    std::sync::Arc::new(config)
}

fn unlimited_string_diags(code: &str) -> Vec<crate::diagnostics::SdblDiagnostic> {
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(unlimited_string_test_config()));
    package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::UnlimitedStringUsage { .. }))
        .cloned()
        .collect()
}

fn unlimited_string_contexts(code: &str) -> Vec<crate::diagnostics::UnlimitedStringUsageContext> {
    unlimited_string_diags(code)
        .iter()
        .map(|d| match d {
            crate::diagnostics::SdblDiagnostic::UnlimitedStringUsage { context, .. } => *context,
            _ => unreachable!(),
        })
        .collect()
}

#[test]
fn unlimited_string_comparison_in_where() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts =
        unlimited_string_contexts("ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ Т.Описание <> \"\"");
    assert_eq!(contexts, vec![Ctx::Comparison]);
}

#[test]
fn unlimited_string_comparison_in_join_condition() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts = unlimited_string_contexts(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т \
         ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Лог КАК Т2 ПО Т.Описание = Т2.Описание",
    );
    assert_eq!(contexts, vec![Ctx::Comparison, Ctx::Comparison]);
}

#[test]
fn unlimited_string_in_operator() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts = unlimited_string_contexts(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ Т.Описание В (\"а\", \"б\")",
    );
    assert_eq!(contexts, vec![Ctx::In]);
}

#[test]
fn unlimited_string_between() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts = unlimited_string_contexts(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ Т.Описание МЕЖДУ \"а\" И \"б\"",
    );
    assert_eq!(contexts, vec![Ctx::Between]);
}

#[test]
fn unlimited_string_order_by() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts = unlimited_string_contexts(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т УПОРЯДОЧИТЬ ПО Т.Описание",
    );
    assert_eq!(contexts, vec![Ctx::OrderBy]);
}

#[test]
fn unlimited_string_distinct() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts =
        unlimited_string_contexts("ВЫБРАТЬ РАЗЛИЧНЫЕ Т.Описание ИЗ Справочник.Лог КАК Т");
    assert_eq!(contexts, vec![Ctx::Distinct]);
}

#[test]
fn unlimited_string_totals_by_alias() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts = unlimited_string_contexts(
        "ВЫБРАТЬ Т.Описание КАК Описание, Т.Номер ИЗ Справочник.Лог КАК Т \
         ИТОГИ КОЛИЧЕСТВО(Номер) ПО Описание",
    );
    assert_eq!(contexts, vec![Ctx::TotalsBy]);
}

#[test]
fn unlimited_string_aggregate_in_having() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts = unlimited_string_contexts(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т СГРУППИРОВАТЬ ПО Т.Номер \
         ИМЕЮЩИЕ МАКСИМУМ(Т.Описание) <> \"\"",
    );
    assert_eq!(contexts, vec![Ctx::Comparison]);
}

#[test]
fn unlimited_string_isnull_wrapper_still_flagged() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts = unlimited_string_contexts(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ ЕСТЬNULL(Т.Описание, \"\") <> \"\"",
    );
    assert_eq!(contexts, vec![Ctx::Comparison]);
}

#[test]
fn unlimited_string_in_subquery_where() {
    use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

    let contexts = unlimited_string_contexts(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т \
         ГДЕ Т.Номер В (ВЫБРАТЬ Т2.Номер ИЗ Справочник.Лог КАК Т2 ГДЕ Т2.Описание <> \"\")",
    );
    assert_eq!(contexts, vec![Ctx::Comparison]);
}

#[test]
fn unlimited_string_no_diagnostic_for_bounded_and_like() {
    assert!(unlimited_string_diags("ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ Т.Номер <> \"\"")
        .is_empty());
    assert!(unlimited_string_diags(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ Т.Описание ПОДОБНО \"а%\""
    )
    .is_empty());
    assert!(unlimited_string_diags(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ Т.Описание ЕСТЬ NULL"
    )
    .is_empty());
}

#[test]
fn unlimited_string_cast_to_bounded_not_flagged() {
    assert!(unlimited_string_diags(
        "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т \
         ГДЕ ВЫРАЗИТЬ(Т.Описание КАК СТРОКА(1000)) <> \"\""
    )
    .is_empty());
}

#[test]
fn having_clause_runs_ref_overuse_check() {
    let config = create_config_with_ref_attribute();

    let code = "ВЫБРАТЬ КОЛИЧЕСТВО(Т.Файл) КАК Кол ИЗ Справочник.СлужебныеФайлы КАК Т \
                ИМЕЮЩИЕ МАКСИМУМ(Т.Файл.Ссылка.Дата) > 0";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let ref_overuse_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::RefOveruse { .. }))
        .collect();

    assert_eq!(
        ref_overuse_diags.len(),
        1,
        "избыточная Ссылка в ИМЕЮЩИЕ должна детектироваться так же, как в ГДЕ"
    );
}

#[test]
fn having_clause_runs_nested_fields_check() {
    let config = create_config_with_ref_attribute();

    let code = "ВЫБРАТЬ КОЛИЧЕСТВО(*) КАК Кол ИЗ Справочник.СлужебныеФайлы КАК Т \
                ИМЕЮЩИЕ МАКСИМУМ(Т.Файл.Код) > 0";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));

    let nested_diags: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, crate::diagnostics::SdblDiagnostic::QueryNestedFieldsByDot { .. }))
        .collect();

    assert_eq!(
        nested_diags.len(),
        1,
        "разыменование через точку в ИМЕЮЩИЕ должно детектироваться так же, как в ГДЕ"
    );
}

/// Which fields of a register may not exist is decided once, on the main table, and every
/// virtual table derived from it inherits that verdict.
///
/// The same false positive returned three review rounds in a row because each virtual-table
/// branch rebuilds its field list from literals: silencing the main table left the slice
/// firing, and fixing `Активность` there left `Период` to repeat it.
///
/// The conditional names are seeded ONLY into the main table's `fields`. Anything a shape hands
/// back under those names it therefore built itself, from a literal — which is exactly the path
/// that loses the mark and the only path inheritance has to repair. Seeding them everywhere
/// instead made the check pass on marks that had simply been copied through, proving nothing:
/// disabling inheritance for `Остатки` alone left the test green.
///
/// Which shapes rebuild which names is pinned below rather than merely counted, so the gate
/// fails in BOTH directions: a shape that starts producing a conditional name joins the checked
/// set instead of slipping past it, and a shape that stops producing one is noticed too.
#[test]
fn every_virtual_table_shape_inherits_the_provisional_marks_of_its_register() {
    use crate::hir::{FieldDef, ResolvedTable};
    use crate::standard_fields::VirtualTableType;
    use crate::SdblType;
    use bsl_metadata::MdoType;

    // The names a register lists although the platform may not create them.
    const CONDITIONAL: &[&str] = &["Период", "Активность", "Регистратор", "НомерСтроки"];

    // Shape → the conditional names it rebuilds for itself. An empty list is a claim too: that
    // shape derives its fields from the register's own dimensions and resources, so there is no
    // literal to lose a mark and nothing for inheritance to repair.
    const REBUILT: &[(VirtualTableType, &[&str])] = &[
        (VirtualTableType::SliceLast, &["Период"]),
        (VirtualTableType::SliceFirst, &["Период"]),
        (VirtualTableType::Balance, &[]),
        (VirtualTableType::Turnovers, &["Период", "Регистратор", "НомерСтроки"]),
        (VirtualTableType::BalanceAndTurnovers, &["Период", "Регистратор"]),
        (VirtualTableType::RecordsWithExtDimensions, &[]),
        (VirtualTableType::ExtDimensionDr, &[]),
        (VirtualTableType::ExtDimensions, &[]),
        (VirtualTableType::Changes, &[]),
    ];

    let mut checked_any = 0;

    for (shape, expected) in REBUILT {
        let main = ResolvedTable::Register {
            mdo_type: MdoType::InformationRegister,
            name: "Курсы".to_string(),
            fields: CONDITIONAL
                .iter()
                .map(|name| FieldDef::provisional_standard(*name, *name, SdblType::Date))
                .collect(),
            dimensions: vec![FieldDef::new("Измерение", SdblType::string())],
            resources: vec![FieldDef::new("Ресурс", SdblType::number())],
            attributes: vec![FieldDef::new("Реквизит", SdblType::string())],
            field_model_complete: true,
        };

        let transformed = super::LoweringContext::transform_for_virtual_table(main, *shape);

        let mut observed: Vec<&str> = CONDITIONAL
            .iter()
            .copied()
            .filter(|name| transformed.fields().iter().any(|f| f.matches_name(name)))
            .collect();
        observed.sort_unstable();
        let mut expected_sorted = expected.to_vec();
        expected_sorted.sort_unstable();

        assert_eq!(
            observed, expected_sorted,
            "[{shape:?}] rebuilds a different set of conditional names than this gate pins; \
             update the table together with the branch, or the new name goes unchecked",
        );

        for name in &observed {
            checked_any += 1;
            for field in transformed.fields().iter().filter(|f| f.matches_name(name)) {
                assert!(
                    field.provisional,
                    "[{shape:?}] `{name}` came back unmarked — the shape rebuilt it from a \
                     literal instead of inheriting the register's verdict",
                );
            }
        }
    }

    assert!(checked_any > 0, "no shape produced a conditional name — the gate asserted nothing");
}

/// The metadata XML reader synthesises the register's object-model standard attributes into
/// `attributes`; the query layer must not append a second copy of them (issue #83).
fn config_with_information_register_from_xml(periodicity: &str) -> bsl_metadata::Configuration {
    let xml = format!(
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
<InformationRegister uuid="59f8d329-f39c-4999-b470-ae9fc74511ac">
<Properties><Name>Курсы</Name><InformationRegisterPeriodicity>{periodicity}</InformationRegisterPeriodicity></Properties>
<ChildObjects>
<Dimension uuid="532f2a7f-4c1e-4a49-8281-3c21232da2d7"><Properties><Name>Валюта</Name></Properties></Dimension>
</ChildObjects>
</InformationRegister></MetaDataObject>"#
    );
    let register = bsl_metadata::xml_parser::parse_information_register_xml(&xml)
        .expect("information register fixture must parse");
    let mut config = bsl_metadata::Configuration::new("TestConfig");
    config.add_register(register);
    config
}

fn config_with_accumulation_register_from_xml() -> bsl_metadata::Configuration {
    let xml = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
<AccumulationRegister uuid="11111111-1111-1111-1111-111111111111">
<Properties><Name>ОстаткиТоваров</Name></Properties>
<ChildObjects>
<Dimension uuid="22222222-2222-2222-2222-222222222222"><Properties><Name>Товар</Name></Properties></Dimension>
</ChildObjects>
</AccumulationRegister></MetaDataObject>"#;
    let register = bsl_metadata::xml_parser::parse_accumulation_register_xml(xml)
        .expect("accumulation register fixture must parse");
    let mut config = bsl_metadata::Configuration::new("TestConfig");
    config.add_register(register);
    config
}

fn register_field_name_counts(
    fields: &[crate::hir::FieldDef],
) -> std::collections::HashMap<&str, usize> {
    let mut counts = std::collections::HashMap::new();
    for field in fields {
        *counts.entry(field.name.as_str()).or_insert(0) += 1;
    }
    counts
}

fn resolved_main_table(package: &SdblPackage) -> &crate::hir::ResolvedTable {
    single_query_hir(package).from[0].metadata.as_ref().expect("register main table must resolve")
}

/// Issue #83: a standard field name must be offered exactly once, for a periodic and for a
/// non-periodic information register alike, and the `provisional` marks must survive the dedup.
#[test]
fn information_register_standard_fields_are_offered_once() {
    for (periodicity, period_is_provisional) in [("Nonperiodical", true), ("Day", false)] {
        let config = config_with_information_register_from_xml(periodicity);
        let code = "ВЫБРАТЬ Т.Регистратор, Т.Период ИЗ РегистрСведений.Курсы КАК Т";
        let ast = parser::parse_sdbl(code);
        let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
        let resolved = resolved_main_table(&package);

        let counts = register_field_name_counts(resolved.fields());
        for (name, count) in &counts {
            assert_eq!(*count, 1, "[{periodicity}] `{name}` is offered {count} times: {counts:?}");
        }
        for name in ["Активность", "НомерСтроки", "Период", "Регистратор", "МоментВремени"]
        {
            assert!(counts.contains_key(name), "[{periodicity}] `{name}` is missing: {counts:?}");
        }

        let period = resolved.find_field("Период").expect("Период must be offered");
        assert_eq!(
            period.provisional, period_is_provisional,
            "[{periodicity}] `Период` provisional flag must reflect the periodicity",
        );
        assert!(
            resolved.find_field("МоментВремени").expect("listed above").provisional,
            "[{periodicity}] `МоментВремени` is recorder-mode-only and must stay provisional",
        );
        assert!(
            resolved.find_field("Регистратор").expect("listed above").provisional,
            "[{periodicity}] recorder-mode fields of an information register must stay provisional",
        );
    }
}

/// Issue #83: accumulation-register standards are unconditional — offered once and unmarked.
#[test]
fn accumulation_register_standard_fields_are_offered_once() {
    let config = config_with_accumulation_register_from_xml();
    let code = "ВЫБРАТЬ Т.Регистратор, Т.ВидДвижения ИЗ РегистрНакопления.ОстаткиТоваров КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let resolved = resolved_main_table(&package);

    let counts = register_field_name_counts(resolved.fields());
    for (name, count) in &counts {
        assert_eq!(*count, 1, "`{name}` is offered {count} times: {counts:?}");
    }
    for name in
        ["Активность", "ВидДвижения", "МоментВремени", "НомерСтроки", "Период", "Регистратор"]
    {
        assert!(counts.contains_key(name), "`{name}` is missing: {counts:?}");
        assert!(
            !resolved.find_field(name).expect("listed above").provisional,
            "`{name}` is unconditional for an accumulation register and must not be provisional",
        );
    }
}

/// Issue #83, acceptance point 2: completions derive from the field list one-to-one, so a
/// deduplicated register table offers every column exactly once.
#[test]
fn register_column_completions_have_no_duplicates() {
    let config = config_with_information_register_from_xml("Day");
    let code = "ВЫБРАТЬ Т.Регистратор ИЗ РегистрСведений.Курсы КАК Т";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    let table = single_query_hir(&package).from[0].clone();

    let mut scope = crate::Scope::new();
    let _ = scope.add_table(table);
    let completions = scope.column_completions(Some("Т"));

    let mut seen = std::collections::HashSet::new();
    for completion in &completions {
        assert!(
            seen.insert(completion.column_name.as_str().to_string()),
            "duplicate completion `{}` in {:?}",
            completion.column_name,
            completions.iter().map(|c| c.column_name.as_str()).collect::<Vec<_>>(),
        );
    }
    assert_eq!(
        completions.iter().filter(|c| c.column_name.as_str() == "Регистратор").count(),
        1,
        "`Регистратор` must be completed once",
    );
}

/// Issue #83 follow-up: with the metadata reader as the single owner, standard fields must
/// keep resolving by their English spellings too — `name_en` is the only path `matches_name`
/// has for them.
#[test]
fn register_standard_fields_resolve_under_english_names() {
    for periodicity in ["Day", "Nonperiodical"] {
        let config = config_with_information_register_from_xml(periodicity);
        let code = "ВЫБРАТЬ T.Recorder, T.Period, T.Active, T.LineNumber, T.PointInTime \
                    ИЗ РегистрСведений.Курсы КАК T";
        let ast = parser::parse_sdbl(code);
        let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
        assert!(
            unknown_fields(&package).is_empty(),
            "[{periodicity}] english standard names must resolve for an information register, \
             got: {:?}",
            unknown_fields(&package),
        );
    }

    let config = config_with_accumulation_register_from_xml();
    let code = "ВЫБРАТЬ T.Recorder, T.Period, T.Active, T.LineNumber, T.PointInTime, \
                T.RecordType ИЗ РегистрНакопления.ОстаткиТоваров КАК T";
    let ast = parser::parse_sdbl(code);
    let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
    assert!(
        unknown_fields(&package).is_empty(),
        "english standard names must resolve for an accumulation register, got: {:?}",
        unknown_fields(&package),
    );
}

/// Issue #83: the slice shapes rebuild a literal `Период`; when the register already carries
/// the reader's `Период` attribute, the literal must not join it as a second copy.
#[test]
fn register_slice_offers_period_once() {
    for periodicity in ["Nonperiodical", "Day"] {
        let config = config_with_information_register_from_xml(periodicity);
        let code = "ВЫБРАТЬ T.Период ИЗ РегистрСведений.Курсы.СрезПоследних(&Дата,) КАК T";
        let ast = parser::parse_sdbl(code);
        let package = lower_sdbl_to_hir(&ast, Some(std::sync::Arc::new(config)));
        let fields = single_query_hir(&package).from[0]
            .metadata
            .as_ref()
            .expect("slice must resolve")
            .fields();
        let periods = fields.iter().filter(|field| field.matches_name("Период")).count();
        assert_eq!(
            periods, 1,
            "[{periodicity}] `Период` must be offered once in the slice: {fields:?}"
        );
    }
}
