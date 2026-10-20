//! Independent SDBL parser corpus.
//!
//! Every input here is written for this file from the query-language chapters of ITS
//! (`pubqlang`: query package, temporary tables, unions, joins, grouping, selection
//! conditions, expressions) and from the local SELECT/expressions mini-specs. The
//! expectations pin the syntax nodes each language form must produce, not just the
//! absence of errors.

use std::fmt::Write;

use expect_test::{expect, Expect};
use parser::parse_sdbl;
use syntax::{NodeOrToken, SyntaxKind, SyntaxNode};

/// Node kinds with their significant tokens, one per line, followed by parse errors.
fn outline(input: &str) -> String {
    let parse = parse_sdbl(input);
    let mut out = String::new();
    render(&parse.syntax_node(), 0, &mut out);
    for error in parse.errors() {
        writeln!(out, "error {:?}: {}", error.range(), error.message()).unwrap();
    }
    out
}

fn render(node: &SyntaxNode, depth: usize, out: &mut String) {
    writeln!(out, "{}{:?}", "  ".repeat(depth), node.kind()).unwrap();
    for child in node.children_with_tokens() {
        match child {
            NodeOrToken::Node(child) => render(&child, depth + 1, out),
            NodeOrToken::Token(token) if !token.kind().is_trivia() => {
                writeln!(out, "{}{:?} {:?}", "  ".repeat(depth + 1), token.kind(), token.text())
                    .unwrap();
            }
            NodeOrToken::Token(_) => {}
        }
    }
}

fn check(input: &str, expect: Expect) {
    expect.assert_eq(&outline(input));
}

fn kinds_of_package_items(input: &str) -> Vec<SyntaxKind> {
    parse_sdbl(input)
        .syntax_node()
        .children_with_tokens()
        .map(|element| element.kind())
        .filter(|kind| !kind.is_trivia())
        .collect()
}

fn count(input: &str, kind: SyntaxKind) -> usize {
    parse_sdbl(input).syntax_node().descendants().filter(|node| node.kind() == kind).count()
}

// --- Query package (pubqlang «Временные таблицы и пакетные запросы») ---

#[test]
fn package_keeps_each_query_and_the_drop_statement_as_separate_items() {
    let input = "ВЫБРАТЬ 1 КАК Код ПОМЕСТИТЬ Врем;\nВЫБРАТЬ Вр.Код КАК Код ИЗ Врем КАК Вр;\nУНИЧТОЖИТЬ Врем";
    assert_eq!(
        kinds_of_package_items(input),
        vec![
            SyntaxKind::SDBL_SELECT_QUERY,
            SyntaxKind::SEMICOLON,
            SyntaxKind::SDBL_SELECT_QUERY,
            SyntaxKind::SEMICOLON,
            SyntaxKind::SDBL_DROP_QUERY,
        ]
    );
}

#[test]
fn into_clause_names_the_temporary_table() {
    check(
        "ВЫБРАТЬ Склады.Ссылка КАК Склад ПОМЕСТИТЬ ВыбранныеСклады ИЗ Справочник.Склады КАК Склады",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Склады"
                                  DOT "."
                                  IDENT "Ссылка"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Склад"
                    SDBL_INTO_CLAUSE
                      IDENT "ПОМЕСТИТЬ"
                      SDBL_TEMP_TABLE_NAME
                        IDENT "ВыбранныеСклады"
                    SDBL_FROM_CLAUSE
                      IDENT "ИЗ"
                      SDBL_DATA_SOURCE
                        SDBL_TABLE_REF
                          IDENT "Справочник"
                          DOT "."
                          IDENT "Склады"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Склады"
        "#]],
    );
}

// --- Unions (pubqlang «Как получить данные из разных таблиц, не связывая, а дополняя их») ---

#[test]
fn union_all_attaches_the_second_query_to_the_same_subquery() {
    check(
        "ВЫБРАТЬ План.Сумма КАК Сумма ИЗ РегистрНакопления.План КАК План\nОБЪЕДИНИТЬ ВСЕ\nВЫБРАТЬ Факт.Сумма ИЗ РегистрНакопления.Факт КАК Факт",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "План"
                                  DOT "."
                                  IDENT "Сумма"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Сумма"
                    SDBL_FROM_CLAUSE
                      IDENT "ИЗ"
                      SDBL_DATA_SOURCE
                        SDBL_TABLE_REF
                          IDENT "РегистрНакопления"
                          DOT "."
                          IDENT "План"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "План"
                  SDBL_UNION_CLAUSE
                    IDENT "ОБЪЕДИНИТЬ"
                    IDENT "ВСЕ"
                    SDBL_QUERY
                      IDENT "ВЫБРАТЬ"
                      SDBL_FIELD_LIST
                        SDBL_SELECTED_FIELD
                          SDBL_LOGICAL_OR_EXPR
                            SDBL_LOGICAL_AND_EXPR
                              SDBL_ADDITIVE_EXPR
                                SDBL_MULTIPLICATIVE_EXPR
                                  SDBL_COLUMN_REF
                                    IDENT "Факт"
                                    DOT "."
                                    IDENT "Сумма"
                      SDBL_FROM_CLAUSE
                        IDENT "ИЗ"
                        SDBL_DATA_SOURCE
                          SDBL_TABLE_REF
                            IDENT "РегистрНакопления"
                            DOT "."
                            IDENT "Факт"
                          SDBL_ALIAS
                            IDENT "КАК"
                            IDENT "Факт"
        "#]],
    );
}

#[test]
fn union_without_all_is_a_union_clause_too() {
    let input = "ВЫБРАТЬ 1 КАК Н ОБЪЕДИНИТЬ ВЫБРАТЬ 2";
    assert_eq!(count(input, SyntaxKind::SDBL_SELECT_QUERY), 1);
    assert_eq!(count(input, SyntaxKind::SDBL_QUERY), 2);
}

// --- Selected fields (pubqlang «Как получить только определенные поля…», v8std #437) ---

#[test]
fn selected_fields_keep_expression_and_alias_apart() {
    check(
        "ВЫБРАТЬ Товары.Наименование КАК Имя, Товары.Цена * 2 КАК ДвойнаяЦена, Товары.Код ИЗ Справочник.Товары КАК Товары",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Товары"
                                  DOT "."
                                  IDENT "Наименование"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Имя"
                      COMMA ","
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Товары"
                                  DOT "."
                                  IDENT "Цена"
                                STAR "*"
                                SDBL_LITERAL
                                  DECIMAL "2"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "ДвойнаяЦена"
                      COMMA ","
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Товары"
                                  DOT "."
                                  IDENT "Код"
                    SDBL_FROM_CLAUSE
                      IDENT "ИЗ"
                      SDBL_DATA_SOURCE
                        SDBL_TABLE_REF
                          IDENT "Справочник"
                          DOT "."
                          IDENT "Товары"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Товары"
        "#]],
    );
}

#[test]
fn distinct_and_top_precede_the_field_list() {
    check(
        "ВЫБРАТЬ РАЗЛИЧНЫЕ ПЕРВЫЕ 10 Товары.Производитель КАК Производитель ИЗ Справочник.Товары КАК Товары",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_LIMITATIONS
                      IDENT "РАЗЛИЧНЫЕ"
                      SDBL_TOP_CLAUSE
                        IDENT "ПЕРВЫЕ"
                        DECIMAL "10"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Товары"
                                  DOT "."
                                  IDENT "Производитель"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Производитель"
                    SDBL_FROM_CLAUSE
                      IDENT "ИЗ"
                      SDBL_DATA_SOURCE
                        SDBL_TABLE_REF
                          IDENT "Справочник"
                          DOT "."
                          IDENT "Товары"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Товары"
        "#]],
    );
}

// --- Sources and joins (pubqlang «Внутреннее/Левое/Правое/Полное соединение», #48) ---

#[test]
fn virtual_table_parameters_stay_under_the_table_reference() {
    check(
        "ВЫБРАТЬ Ост.КоличествоОстаток КАК Остаток ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(&Дата, Склад = &Склад) КАК Ост",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Ост"
                                  DOT "."
                                  IDENT "КоличествоОстаток"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Остаток"
                    SDBL_FROM_CLAUSE
                      IDENT "ИЗ"
                      SDBL_DATA_SOURCE
                        SDBL_TABLE_REF
                          IDENT "РегистрНакопления"
                          DOT "."
                          IDENT "ТоварыНаСкладах"
                          DOT "."
                          IDENT "Остатки"
                          L_PAREN "("
                          SDBL_LOGICAL_OR_EXPR
                            SDBL_LOGICAL_AND_EXPR
                              SDBL_ADDITIVE_EXPR
                                SDBL_MULTIPLICATIVE_EXPR
                                  SDBL_PARAMETER
                                    AMPERSAND "&Дата"
                          COMMA ","
                          SDBL_LOGICAL_OR_EXPR
                            SDBL_LOGICAL_AND_EXPR
                              SDBL_COMPARISON_EXPR
                                SDBL_ADDITIVE_EXPR
                                  SDBL_MULTIPLICATIVE_EXPR
                                    SDBL_COLUMN_REF
                                      IDENT "Склад"
                                EQ "="
                                SDBL_ADDITIVE_EXPR
                                  SDBL_MULTIPLICATIVE_EXPR
                                    SDBL_PARAMETER
                                      AMPERSAND "&Склад"
                          R_PAREN ")"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Ост"
        "#]],
    );
}

#[test]
fn every_join_kind_produces_one_join_clause_with_its_condition() {
    for (keyword, english) in
        [("ВНУТРЕННЕЕ", "INNER"), ("ЛЕВОЕ", "LEFT"), ("ПРАВОЕ", "RIGHT"), ("ПОЛНОЕ", "FULL")]
    {
        for kind in [keyword, english] {
            let input = format!(
                "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т {kind} СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Т.Ссылка = Ц.Товар"
            );
            let parse = parse_sdbl(&input);
            assert!(parse.errors().is_empty(), "{input}: {:?}", parse.errors());
            assert_eq!(count(&input, SyntaxKind::SDBL_JOIN_CLAUSE), 1, "{input}");
            assert_eq!(count(&input, SyntaxKind::SDBL_COMPARISON_EXPR), 1, "{input}");
        }
    }
}

#[test]
fn a_join_nested_in_a_join_belongs_to_the_inner_data_source() {
    check(
        "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Валюты КАК В1 ПО Ц.Валюта = В1.Ссылка ПО Т.Ссылка = Ц.Товар",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Т"
                                  DOT "."
                                  IDENT "Код"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Код"
                    SDBL_FROM_CLAUSE
                      IDENT "ИЗ"
                      SDBL_DATA_SOURCE
                        SDBL_TABLE_REF
                          IDENT "Справочник"
                          DOT "."
                          IDENT "Товары"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Т"
                        SDBL_JOIN_CLAUSE
                          IDENT "ЛЕВОЕ"
                          IDENT "СОЕДИНЕНИЕ"
                          SDBL_DATA_SOURCE
                            SDBL_TABLE_REF
                              IDENT "РегистрСведений"
                              DOT "."
                              IDENT "Цены"
                            SDBL_ALIAS
                              IDENT "КАК"
                              IDENT "Ц"
                            SDBL_JOIN_CLAUSE
                              IDENT "ВНУТРЕННЕЕ"
                              IDENT "СОЕДИНЕНИЕ"
                              SDBL_DATA_SOURCE
                                SDBL_TABLE_REF
                                  IDENT "Справочник"
                                  DOT "."
                                  IDENT "Валюты"
                                SDBL_ALIAS
                                  IDENT "КАК"
                                  IDENT "В1"
                              IDENT "ПО"
                              SDBL_LOGICAL_OR_EXPR
                                SDBL_LOGICAL_AND_EXPR
                                  SDBL_COMPARISON_EXPR
                                    SDBL_ADDITIVE_EXPR
                                      SDBL_MULTIPLICATIVE_EXPR
                                        SDBL_COLUMN_REF
                                          IDENT "Ц"
                                          DOT "."
                                          IDENT "Валюта"
                                    EQ "="
                                    SDBL_ADDITIVE_EXPR
                                      SDBL_MULTIPLICATIVE_EXPR
                                        SDBL_COLUMN_REF
                                          IDENT "В1"
                                          DOT "."
                                          IDENT "Ссылка"
                          IDENT "ПО"
                          SDBL_LOGICAL_OR_EXPR
                            SDBL_LOGICAL_AND_EXPR
                              SDBL_COMPARISON_EXPR
                                SDBL_ADDITIVE_EXPR
                                  SDBL_MULTIPLICATIVE_EXPR
                                    SDBL_COLUMN_REF
                                      IDENT "Т"
                                      DOT "."
                                      IDENT "Ссылка"
                                EQ "="
                                SDBL_ADDITIVE_EXPR
                                  SDBL_MULTIPLICATIVE_EXPR
                                    SDBL_COLUMN_REF
                                      IDENT "Ц"
                                      DOT "."
                                      IDENT "Товар"
        "#]],
    );
}

#[test]
fn nested_query_as_a_source_keeps_its_own_query_node() {
    check(
        "ВЫБРАТЬ Вл.Код КАК Код ИЗ (ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т) КАК Вл",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Вл"
                                  DOT "."
                                  IDENT "Код"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Код"
                    SDBL_FROM_CLAUSE
                      IDENT "ИЗ"
                      SDBL_DATA_SOURCE
                        L_PAREN "("
                        SDBL_SUBQUERY
                          SDBL_QUERY
                            IDENT "ВЫБРАТЬ"
                            SDBL_FIELD_LIST
                              SDBL_SELECTED_FIELD
                                SDBL_LOGICAL_OR_EXPR
                                  SDBL_LOGICAL_AND_EXPR
                                    SDBL_ADDITIVE_EXPR
                                      SDBL_MULTIPLICATIVE_EXPR
                                        SDBL_COLUMN_REF
                                          IDENT "Т"
                                          DOT "."
                                          IDENT "Код"
                                SDBL_ALIAS
                                  IDENT "КАК"
                                  IDENT "Код"
                            SDBL_FROM_CLAUSE
                              IDENT "ИЗ"
                              SDBL_DATA_SOURCE
                                SDBL_TABLE_REF
                                  IDENT "Справочник"
                                  DOT "."
                                  IDENT "Товары"
                                SDBL_ALIAS
                                  IDENT "КАК"
                                  IDENT "Т"
                        R_PAREN ")"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Вл"
        "#]],
    );
}

// --- Grouping and conditions (pubqlang «…отобранные по некоторому условию», #35, #39) ---

#[test]
fn where_group_by_having_and_order_by_follow_in_order() {
    check(
        "ВЫБРАТЬ П.Покупатель КАК Покупатель, СУММА(П.Сумма) КАК Сумма ИЗ РегистрНакопления.Продажи КАК П ГДЕ П.Период МЕЖДУ &Начало И &Конец СГРУППИРОВАТЬ ПО П.Покупатель ИМЕЮЩИЕ СУММА(П.Сумма) > 100 УПОРЯДОЧИТЬ ПО Сумма УБЫВ",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "П"
                                  DOT "."
                                  IDENT "Покупатель"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Покупатель"
                      COMMA ","
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_FUNCTION_CALL
                                  IDENT "СУММА"
                                  L_PAREN "("
                                  SDBL_LOGICAL_OR_EXPR
                                    SDBL_LOGICAL_AND_EXPR
                                      SDBL_ADDITIVE_EXPR
                                        SDBL_MULTIPLICATIVE_EXPR
                                          SDBL_COLUMN_REF
                                            IDENT "П"
                                            DOT "."
                                            IDENT "Сумма"
                                  R_PAREN ")"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Сумма"
                    SDBL_FROM_CLAUSE
                      IDENT "ИЗ"
                      SDBL_DATA_SOURCE
                        SDBL_TABLE_REF
                          IDENT "РегистрНакопления"
                          DOT "."
                          IDENT "Продажи"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "П"
                    SDBL_WHERE_CLAUSE
                      IDENT "ГДЕ"
                      SDBL_LOGICAL_OR_EXPR
                        SDBL_LOGICAL_AND_EXPR
                          SDBL_BETWEEN_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "П"
                                  DOT "."
                                  IDENT "Период"
                            IDENT "МЕЖДУ"
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_PARAMETER
                                  AMPERSAND "&Начало"
                            KW_AND "И"
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_PARAMETER
                                  AMPERSAND "&Конец"
                    SDBL_GROUP_CLAUSE
                      IDENT "СГРУППИРОВАТЬ"
                      IDENT "ПО"
                      SDBL_LOGICAL_OR_EXPR
                        SDBL_LOGICAL_AND_EXPR
                          SDBL_ADDITIVE_EXPR
                            SDBL_MULTIPLICATIVE_EXPR
                              SDBL_COLUMN_REF
                                IDENT "П"
                                DOT "."
                                IDENT "Покупатель"
                    SDBL_HAVING_CLAUSE
                      IDENT "ИМЕЮЩИЕ"
                      SDBL_LOGICAL_OR_EXPR
                        SDBL_LOGICAL_AND_EXPR
                          SDBL_COMPARISON_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_FUNCTION_CALL
                                  IDENT "СУММА"
                                  L_PAREN "("
                                  SDBL_LOGICAL_OR_EXPR
                                    SDBL_LOGICAL_AND_EXPR
                                      SDBL_ADDITIVE_EXPR
                                        SDBL_MULTIPLICATIVE_EXPR
                                          SDBL_COLUMN_REF
                                            IDENT "П"
                                            DOT "."
                                            IDENT "Сумма"
                                  R_PAREN ")"
                            GT ">"
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_LITERAL
                                  DECIMAL "100"
                    SDBL_ORDER_CLAUSE
                      IDENT "УПОРЯДОЧИТЬ"
                      IDENT "ПО"
                      SDBL_LOGICAL_OR_EXPR
                        SDBL_LOGICAL_AND_EXPR
                          SDBL_ADDITIVE_EXPR
                            SDBL_MULTIPLICATIVE_EXPR
                              SDBL_COLUMN_REF
                                IDENT "Сумма"
                      IDENT "УБЫВ"
        "#]],
    );
}

#[test]
fn logical_operators_bind_not_before_and_before_or() {
    check(
        "ВЫБРАТЬ 1 КАК Н ГДЕ НЕ Т.А = 1 И Т.Б = 2 ИЛИ Т.Г = 3",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_LITERAL
                                  DECIMAL "1"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Н"
                    SDBL_WHERE_CLAUSE
                      IDENT "ГДЕ"
                      SDBL_LOGICAL_OR_EXPR
                        SDBL_LOGICAL_AND_EXPR
                          SDBL_NOT_EXPR
                            KW_NOT "НЕ"
                            SDBL_COMPARISON_EXPR
                              SDBL_ADDITIVE_EXPR
                                SDBL_MULTIPLICATIVE_EXPR
                                  SDBL_COLUMN_REF
                                    IDENT "Т"
                                    DOT "."
                                    IDENT "А"
                              EQ "="
                              SDBL_ADDITIVE_EXPR
                                SDBL_MULTIPLICATIVE_EXPR
                                  SDBL_LITERAL
                                    DECIMAL "1"
                          KW_AND "И"
                          SDBL_COMPARISON_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Т"
                                  DOT "."
                                  IDENT "Б"
                            EQ "="
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_LITERAL
                                  DECIMAL "2"
                        KW_OR "ИЛИ"
                        SDBL_LOGICAL_AND_EXPR
                          SDBL_COMPARISON_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_COLUMN_REF
                                  IDENT "Т"
                                  DOT "."
                                  IDENT "Г"
                            EQ "="
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_LITERAL
                                  DECIMAL "3"
        "#]],
    );
}

#[test]
fn predicates_produce_their_own_nodes() {
    let input = "ВЫБРАТЬ 1 КАК Н ИЗ Справочник.Т КАК Т ГДЕ Т.А В (1, 2) И Т.Б ПОДОБНО \"А%\" И Т.Д ЕСТЬ NULL И Т.Г МЕЖДУ 1 И 2";
    let parse = parse_sdbl(input);
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());
    for kind in [
        SyntaxKind::SDBL_IN_EXPR,
        SyntaxKind::SDBL_LIKE_EXPR,
        SyntaxKind::SDBL_IS_NULL_EXPR,
        SyntaxKind::SDBL_BETWEEN_EXPR,
    ] {
        assert_eq!(count(input, kind), 1, "{kind:?}");
    }
}

#[test]
fn case_and_cast_are_expressions() {
    check(
        "ВЫБРАТЬ ВЫБОР КОГДА Т.Количество > 0 ТОГДА \"Есть\" ИНАЧЕ \"Нет\" КОНЕЦ КАК Наличие, ВЫРАЗИТЬ(Т.Описание КАК СТРОКА(100)) КАК Кратко ИЗ Справочник.Т КАК Т",
        expect![[r#"
            SDBL_QUERY_PACKAGE
              SDBL_SELECT_QUERY
                SDBL_SUBQUERY
                  SDBL_QUERY
                    IDENT "ВЫБРАТЬ"
                    SDBL_FIELD_LIST
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_CASE_EXPR
                                  IDENT "ВЫБОР"
                                  SDBL_WHEN_CLAUSE
                                    IDENT "КОГДА"
                                    SDBL_LOGICAL_OR_EXPR
                                      SDBL_LOGICAL_AND_EXPR
                                        SDBL_COMPARISON_EXPR
                                          SDBL_ADDITIVE_EXPR
                                            SDBL_MULTIPLICATIVE_EXPR
                                              SDBL_COLUMN_REF
                                                IDENT "Т"
                                                DOT "."
                                                IDENT "Количество"
                                          GT ">"
                                          SDBL_ADDITIVE_EXPR
                                            SDBL_MULTIPLICATIVE_EXPR
                                              SDBL_LITERAL
                                                DECIMAL "0"
                                    IDENT "ТОГДА"
                                    SDBL_LOGICAL_OR_EXPR
                                      SDBL_LOGICAL_AND_EXPR
                                        SDBL_ADDITIVE_EXPR
                                          SDBL_MULTIPLICATIVE_EXPR
                                            SDBL_MULTI_STRING
                                              STRING "\""
                                              STRING "Есть"
                                              STRING "\""
                                  IDENT "ИНАЧЕ"
                                  SDBL_LOGICAL_OR_EXPR
                                    SDBL_LOGICAL_AND_EXPR
                                      SDBL_ADDITIVE_EXPR
                                        SDBL_MULTIPLICATIVE_EXPR
                                          SDBL_MULTI_STRING
                                            STRING "\""
                                            STRING "Нет"
                                            STRING "\""
                                  IDENT "КОНЕЦ"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Наличие"
                      COMMA ","
                      SDBL_SELECTED_FIELD
                        SDBL_LOGICAL_OR_EXPR
                          SDBL_LOGICAL_AND_EXPR
                            SDBL_ADDITIVE_EXPR
                              SDBL_MULTIPLICATIVE_EXPR
                                SDBL_FUNCTION_CALL
                                  IDENT "ВЫРАЗИТЬ"
                                  L_PAREN "("
                                  SDBL_LOGICAL_OR_EXPR
                                    SDBL_LOGICAL_AND_EXPR
                                      SDBL_ADDITIVE_EXPR
                                        SDBL_MULTIPLICATIVE_EXPR
                                          SDBL_COLUMN_REF
                                            IDENT "Т"
                                            DOT "."
                                            IDENT "Описание"
                                  IDENT "КАК"
                                  SDBL_TYPE
                                    IDENT "СТРОКА"
                                    L_PAREN "("
                                    DECIMAL "100"
                                    R_PAREN ")"
                                  R_PAREN ")"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Кратко"
                    SDBL_FROM_CLAUSE
                      IDENT "ИЗ"
                      SDBL_DATA_SOURCE
                        SDBL_TABLE_REF
                          IDENT "Справочник"
                          DOT "."
                          IDENT "Т"
                        SDBL_ALIAS
                          IDENT "КАК"
                          IDENT "Т"
        "#]],
    );
}

// --- Recovery: an incomplete query keeps what follows (local IDE contract) ---

#[test]
fn an_empty_field_slot_does_not_swallow_the_following_clauses() {
    let input = "ВЫБРАТЬ Т.А, ИЗ Справочник.Т КАК Т ГДЕ Т.А = 1";
    assert!(!parse_sdbl(input).errors().is_empty());
    assert_eq!(count(input, SyntaxKind::SDBL_FROM_CLAUSE), 1);
    assert_eq!(count(input, SyntaxKind::SDBL_WHERE_CLAUSE), 1);
}

#[test]
fn an_unfinished_query_does_not_absorb_the_next_package_item() {
    let input = "ВЫБРАТЬ Т.А ИЗ;\nВЫБРАТЬ 2 КАК Б";
    assert_eq!(
        kinds_of_package_items(input),
        vec![SyntaxKind::SDBL_SELECT_QUERY, SyntaxKind::SEMICOLON, SyntaxKind::SDBL_SELECT_QUERY]
    );
}

#[test]
fn an_unclosed_nested_query_still_reports_and_keeps_its_tokens() {
    let input = "ВЫБРАТЬ Вл.А КАК А ИЗ (ВЫБРАТЬ Т.А КАК А ИЗ Справочник.Т КАК Т КАК Вл";
    let parse = parse_sdbl(input);
    assert!(!parse.errors().is_empty());
    assert_eq!(parse.syntax_node().text().to_string(), input);
}

// --- Parser event contract: lossless text, each lexeme once ---

#[test]
fn the_tree_reproduces_the_input_and_holds_each_lexeme_once() {
    let inputs = [
        "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т // комментарий\nГДЕ Т.Код = 1",
        "ВЫБРАТЬ\n\tПЕРВЫЕ 1 Т.Код КАК Код\nИЗ\n\tСправочник.Товары КАК Т\nУПОРЯДОЧИТЬ ПО Код",
        "SELECT T.Code AS Code FROM Catalog.Goods AS T WHERE T.Code IN (1, 2)",
    ];
    for input in inputs {
        let root = parse_sdbl(input).syntax_node();
        assert_eq!(root.text().to_string(), input);
        let significant: String = root
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
            .filter(|token| !token.kind().is_trivia())
            .map(|token| token.text().to_string())
            .collect();
        let expected: String = input
            .split("//")
            .enumerate()
            .map(|(i, part)| if i == 0 { part } else { part.split_once('\n').map_or("", |p| p.1) })
            .collect::<String>()
            .split_whitespace()
            .collect();
        assert_eq!(significant, expected, "{input}");
    }
}

#[test]
fn an_error_range_points_at_the_offending_token() {
    // The alias name is missing, so the error belongs where the keyword ИЗ stands.
    let input = "ВЫБРАТЬ Т.А КАК ИЗ Справочник.Т КАК Т";
    let parse = parse_sdbl(input);
    let error = parse.errors().first().expect("a missing alias name is an error");
    assert_eq!(usize::from(error.range().start()), input.find("ИЗ").unwrap());

    // A missing list element is reported at the token that stands in its place.
    let input = "ВЫБРАТЬ Т.А, ИЗ Справочник.Т КАК Т";
    let parse = parse_sdbl(input);
    let error = parse.errors().first().expect("an empty list slot is an error");
    assert_eq!(usize::from(error.range().start()), input.find("ИЗ").unwrap());
}

// --- Second package: package/union ownership, virtual-table arguments, select-list
// expressions, recovery. Sources: ITS pubqlang «Временные таблицы и пакетные запросы»,
// «Как получить данные из разных таблиц, не связывая, а дополняя их», «Как получить
// данные из разных таблиц, связанных несколькими соединениями», «Как получить данные
// из табличной части некоторого документа», «Использовать параметры виртуальных
// таблиц», «Примеры использования выражений в списке полей выборки запроса»;
// SELECT mini-spec «Query package», «Subquery and UNION», «Virtual table argument
// behavior»; expressions mini-spec «CAST type specification», «CASE expressions»,
// «Predicates», «VT-arg children direct under SdblTableRef». ---

fn root(input: &str) -> SyntaxNode {
    parse_sdbl(input).syntax_node()
}

fn package_items(input: &str) -> Vec<SyntaxNode> {
    root(input).children().filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY).collect()
}

fn descendants_of(node: &SyntaxNode, kind: SyntaxKind) -> Vec<SyntaxNode> {
    node.descendants().filter(|n| n.kind() == kind).collect()
}

fn significant_text(node: &SyntaxNode) -> String {
    node.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
        .map(|t| t.text().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Alias names of the selected fields of one query, in order; `-` for a field without one.
fn field_aliases(query: &SyntaxNode) -> Vec<String> {
    query
        .children()
        .filter(|n| n.kind() == SyntaxKind::SDBL_FIELD_LIST)
        .flat_map(|list| list.children().filter(|n| n.kind() == SyntaxKind::SDBL_SELECTED_FIELD))
        .map(|field| {
            field
                .children()
                .find(|n| n.kind() == SyntaxKind::SDBL_ALIAS)
                .and_then(|alias| {
                    alias
                        .children_with_tokens()
                        .filter_map(|e| e.into_token())
                        .filter(|t| t.kind() == SyntaxKind::IDENT)
                        .last()
                        .map(|t| t.text().to_string())
                })
                .unwrap_or_else(|| "-".to_string())
        })
        .collect()
}

#[test]
fn a_comment_and_line_breaks_around_the_separator_keep_items_apart() {
    let input = "ВЫБРАТЬ 1 КАК Первое // первый запрос пакета\n;\n// второй\nВЫБРАТЬ 2 КАК Второе";
    assert_eq!(root(input).text().to_string(), input);
    assert_eq!(
        kinds_of_package_items(input),
        vec![SyntaxKind::SDBL_SELECT_QUERY, SyntaxKind::SEMICOLON, SyntaxKind::SDBL_SELECT_QUERY]
    );
    let items = package_items(input);
    assert_eq!(significant_text(&items[0]), "ВЫБРАТЬ 1 КАК Первое");
    assert_eq!(significant_text(&items[1]), "ВЫБРАТЬ 2 КАК Второе");
}

#[test]
fn union_branches_belong_to_their_own_package_item() {
    let input = "ВЫБРАТЬ А.Код КАК Код, А.Имя ИЗ Справочник.А КАК А\nОБЪЕДИНИТЬ ВСЕ\nВЫБРАТЬ Б.Код, Б.Имя КАК Имя ИЗ Справочник.Б КАК Б;\nВЫБРАТЬ Ц.Сумма КАК Сумма ИЗ РегистрНакопления.Ц КАК Ц\nОБЪЕДИНИТЬ\nВЫБРАТЬ Д.Сумма ИЗ РегистрНакопления.Д КАК Д";
    let items = package_items(input);
    assert_eq!(items.len(), 2);
    let queries: Vec<Vec<Vec<String>>> = items
        .iter()
        .map(|item| {
            descendants_of(item, SyntaxKind::SDBL_QUERY).iter().map(field_aliases).collect()
        })
        .collect();
    assert_eq!(
        queries,
        vec![
            vec![
                vec!["Код".to_string(), "-".to_string()],
                vec!["-".to_string(), "Имя".to_string()]
            ],
            vec![vec!["Сумма".to_string()], vec!["-".to_string()]],
        ]
    );
    for (item, sources) in items.iter().zip([["А", "Б"], ["Ц", "Д"]]) {
        let aliases: Vec<String> = descendants_of(item, SyntaxKind::SDBL_DATA_SOURCE)
            .iter()
            .map(|ds| significant_text(ds).rsplit(' ').next().unwrap().to_string())
            .collect();
        assert_eq!(aliases, sources);
    }
}

#[test]
fn into_union_and_the_next_items_join_and_grouping_stay_in_their_items() {
    let input = "ВЫБРАТЬ А.Код КАК Код ПОМЕСТИТЬ Врем ИЗ Справочник.А КАК А\nОБЪЕДИНИТЬ ВСЕ\nВЫБРАТЬ Б.Код ИЗ Справочник.Б КАК Б;\nВЫБРАТЬ Вр.Код КАК Код, КОЛИЧЕСТВО(Ц.Цена) КАК Цен ИЗ Врем КАК Вр ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Ц.Код = Вр.Код СГРУППИРОВАТЬ ПО Вр.Код";
    let items = package_items(input);
    assert_eq!(items.len(), 2);
    let count_in = |i: usize, kind| descendants_of(&items[i], kind).len();
    assert_eq!(
        (count_in(0, SyntaxKind::SDBL_INTO_CLAUSE), count_in(1, SyntaxKind::SDBL_INTO_CLAUSE)),
        (1, 0)
    );
    assert_eq!(
        (count_in(0, SyntaxKind::SDBL_UNION_CLAUSE), count_in(1, SyntaxKind::SDBL_UNION_CLAUSE)),
        (1, 0)
    );
    assert_eq!(
        (count_in(0, SyntaxKind::SDBL_JOIN_CLAUSE), count_in(1, SyntaxKind::SDBL_JOIN_CLAUSE)),
        (0, 1)
    );
    assert_eq!(
        (count_in(0, SyntaxKind::SDBL_GROUP_CLAUSE), count_in(1, SyntaxKind::SDBL_GROUP_CLAUSE)),
        (0, 1)
    );
    let into = &descendants_of(&items[0], SyntaxKind::SDBL_INTO_CLAUSE)[0];
    assert_eq!(significant_text(into), "ПОМЕСТИТЬ Врем");
}

#[test]
fn a_nested_join_split_over_lines_keeps_its_condition_and_parameter() {
    let input = "ВЫБРАТЬ Т.Код КАК Код\nИЗ Справочник.Товары КАК Т\n  ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц\n    ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Валюты КАК Вал\n    ПО Ц.Валюта = Вал.Ссылка И Вал.Дата < &Дата\n  ПО Т.Ссылка = Ц.Товар;\nВЫБРАТЬ 2 КАК Второе";
    let items = package_items(input);
    assert_eq!(items.len(), 2);
    assert_eq!(significant_text(&items[1]), "ВЫБРАТЬ 2 КАК Второе");
    let joins = descendants_of(&items[0], SyntaxKind::SDBL_JOIN_CLAUSE);
    assert_eq!(joins.len(), 2);
    let (outer, inner) = (&joins[0], &joins[1]);
    assert!(
        inner.ancestors().skip(1).any(|a| &a == outer),
        "the inner join nests in the outer one"
    );
    let inner_params = descendants_of(inner, SyntaxKind::SDBL_PARAMETER);
    assert_eq!(inner_params.len(), 1);
    assert_eq!(significant_text(&inner_params[0]), "&Дата");
    let outer_condition: Vec<String> = outer
        .children()
        .filter(|n| {
            n.kind() == SyntaxKind::SDBL_COMPARISON_EXPR
                || n.kind() == SyntaxKind::SDBL_LOGICAL_OR_EXPR
        })
        .map(|n| significant_text(&n))
        .collect();
    assert_eq!(outer_condition, vec!["Т . Ссылка = Ц . Товар"]);
}

fn table_ref_children(input: &str) -> Vec<SyntaxKind> {
    let table_ref = descendants_of(&root(input), SyntaxKind::SDBL_TABLE_REF).remove(0);
    table_ref
        .children_with_tokens()
        .map(|e| e.kind())
        .filter(|k| !k.is_trivia() && *k != SyntaxKind::IDENT && *k != SyntaxKind::DOT)
        .collect()
}

#[test]
fn empty_virtual_table_arguments_do_not_absorb_their_neighbours() {
    use SyntaxKind::*;
    let input = "ВЫБРАТЬ О.Остаток КАК Остаток ИЗ РегистрНакопления.Товары.ОстаткиИОбороты(, , &Конец, ) КАК О";
    assert!(parse_sdbl(input).errors().is_empty());
    assert_eq!(
        table_ref_children(input),
        vec![
            L_PAREN,
            SDBL_MISSING_ARG,
            COMMA,
            SDBL_MISSING_ARG,
            COMMA,
            SDBL_LOGICAL_OR_EXPR,
            COMMA,
            SDBL_MISSING_ARG,
            R_PAREN
        ]
    );
    let table_ref = descendants_of(&root(input), SDBL_TABLE_REF).remove(0);
    let filled: Vec<String> = table_ref
        .children()
        .filter(|n| n.kind() == SDBL_LOGICAL_OR_EXPR)
        .map(|n| significant_text(&n))
        .collect();
    assert_eq!(filled, vec!["&Конец"]);
    let source = descendants_of(&root(input), SDBL_DATA_SOURCE).remove(0);
    let alias =
        source.children().find(|n| n.kind() == SDBL_ALIAS).expect("the alias stays on the source");
    assert_eq!(significant_text(&alias), "КАК О");
}

#[test]
fn a_membership_query_stays_inside_its_virtual_table_argument() {
    use SyntaxKind::*;
    let input = "ВЫБРАТЬ О.Товар КАК Товар, О.Остаток * 2 КАК Двойной ИЗ РегистрНакопления.Товары.Остатки(&Дата, Товар В (ВЫБРАТЬ С.Ссылка ИЗ Справочник.Номенклатура КАК С)) КАК О ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Номенклатура КАК Н ПО Н.Ссылка = О.Товар";
    assert!(parse_sdbl(input).errors().is_empty(), "{:?}", parse_sdbl(input).errors());
    assert_eq!(package_items(input).len(), 1);
    assert_eq!(
        table_ref_children(input),
        vec![L_PAREN, SDBL_LOGICAL_OR_EXPR, COMMA, SDBL_LOGICAL_OR_EXPR, R_PAREN]
    );
    let table_ref = descendants_of(&root(input), SDBL_TABLE_REF).remove(0);
    let args: Vec<SyntaxNode> =
        table_ref.children().filter(|n| n.kind() == SDBL_LOGICAL_OR_EXPR).collect();
    assert_eq!(significant_text(&args[0]), "&Дата");
    let membership = descendants_of(&args[1], SDBL_IN_EXPR).remove(0);
    assert!(significant_text(&membership).starts_with("Товар В ( ВЫБРАТЬ"));
    assert_eq!(descendants_of(&membership, SDBL_SUBQUERY).len(), 1);
    // The outer query owns exactly one FROM; the nested one lives inside the argument.
    let outer = descendants_of(&root(input), SDBL_QUERY).remove(0);
    let from = outer.children().find(|n| n.kind() == SDBL_FROM_CLAUSE).unwrap();
    let sources: Vec<_> = from.children().filter(|n| n.kind() == SDBL_DATA_SOURCE).collect();
    assert_eq!(sources.len(), 1);
    assert!(
        descendants_of(&table_ref, SDBL_JOIN_CLAUSE).is_empty(),
        "the join is outside the call"
    );
    assert_eq!(field_aliases(&outer), vec!["Товар", "Двойной"]);
}

fn fields(input: &str) -> Vec<SyntaxNode> {
    let query = descendants_of(&root(input), SyntaxKind::SDBL_QUERY).remove(0);
    query
        .children()
        .filter(|n| n.kind() == SyntaxKind::SDBL_FIELD_LIST)
        .flat_map(|list| list.children().filter(|n| n.kind() == SyntaxKind::SDBL_SELECTED_FIELD))
        .collect()
}

fn call_args(call: &SyntaxNode) -> Vec<String> {
    call.children()
        .filter(|n| n.kind() != SyntaxKind::SDBL_TYPE)
        .map(|n| significant_text(&n))
        .collect()
}

#[test]
fn several_multi_argument_calls_keep_their_arguments_and_fields() {
    let input = "ВЫБРАТЬ ПОДСТРОКА(Т.Имя, 1, 3) КАК Начало, РАЗНОСТЬДАТ(Т.Начало, Т.Конец, ДЕНЬ) КАК Дней, Т.Код КАК Код ИЗ Справочник.Т КАК Т";
    assert!(parse_sdbl(input).errors().is_empty());
    let fields = fields(input);
    assert_eq!(fields.len(), 3);
    let calls: Vec<Vec<String>> = fields[..2]
        .iter()
        .map(|f| call_args(&descendants_of(f, SyntaxKind::SDBL_FUNCTION_CALL)[0]))
        .collect();
    assert_eq!(calls[0], vec!["Т . Имя", "1", "3"]);
    assert_eq!(calls[1], vec!["Т . Начало", "Т . Конец", "ДЕНЬ"]);
    assert_eq!(significant_text(&fields[2]), "Т . Код КАК Код");
    assert_eq!(descendants_of(&root(input), SyntaxKind::SDBL_FROM_CLAUSE).len(), 1);
}

#[test]
fn a_cast_type_and_its_parameters_belong_to_the_cast() {
    let input = "ВЫБРАТЬ ВЫРАЗИТЬ(Т.Сумма * 2 КАК ЧИСЛО(15, 2)) КАК Удвоено, ВЫБОР КОГДА Т.Сумма > 0 ТОГДА Т.Сумма ИНАЧЕ 0 КОНЕЦ + 1 КАК Плюс, Т.Код КАК Код ИЗ Справочник.Т КАК Т";
    assert!(parse_sdbl(input).errors().is_empty());
    let fields = fields(input);
    assert_eq!(fields.len(), 3);
    let cast = descendants_of(&fields[0], SyntaxKind::SDBL_FUNCTION_CALL).remove(0);
    let ty = cast
        .children()
        .find(|n| n.kind() == SyntaxKind::SDBL_TYPE)
        .expect("the type is a child of the cast");
    assert_eq!(significant_text(&ty), "ЧИСЛО ( 15 , 2 )");
    assert_eq!(call_args(&cast), vec!["Т . Сумма * 2"]);
    assert_eq!(
        field_aliases(&descendants_of(&root(input), SyntaxKind::SDBL_QUERY)[0]),
        vec!["Удвоено", "Плюс", "Код"]
    );
    assert_eq!(descendants_of(&fields[1], SyntaxKind::SDBL_CASE_EXPR).len(), 1);
    assert!(descendants_of(&fields[1], SyntaxKind::SDBL_TYPE).is_empty());
}

#[test]
fn case_concatenation_and_cast_leave_from_and_where_intact() {
    let input = "ВЫБРАТЬ ВЫБОР КОГДА Т.Вид = 1 ТОГДА \"А\" ИНАЧЕ \"Б\" КОНЕЦ + \"-\" + ВЫРАЗИТЬ(Т.Имя КАК СТРОКА(20)) КАК Метка ИЗ Справочник.Т КАК Т ГДЕ Т.Код > 0";
    assert!(parse_sdbl(input).errors().is_empty());
    let query = descendants_of(&root(input), SyntaxKind::SDBL_QUERY).remove(0);
    assert_eq!(field_aliases(&query), vec!["Метка"]);
    let from = query.children().find(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE).unwrap();
    assert_eq!(significant_text(&from), "ИЗ Справочник . Т КАК Т");
    let filter = query.children().find(|n| n.kind() == SyntaxKind::SDBL_WHERE_CLAUSE).unwrap();
    assert_eq!(significant_text(&filter), "ГДЕ Т . Код > 0");
}

#[test]
fn null_tests_inside_case_conditions_are_predicates_even_on_one_line() {
    let input = "ВЫБРАТЬ ВЫБОР КОГДА Т.А ЕСТЬ NULL ТОГДА 0 КОГДА Т.Б ЕСТЬ НЕ NULL ТОГДА 1 ИНАЧЕ 2 КОНЕЦ КАК Флаг ИЗ Справочник.Т КАК Т";
    assert!(parse_sdbl(input).errors().is_empty());
    let whens = descendants_of(&root(input), SyntaxKind::SDBL_WHEN_CLAUSE);
    assert_eq!(whens.len(), 2);
    let tests: Vec<String> = whens
        .iter()
        .map(|w| significant_text(&descendants_of(w, SyntaxKind::SDBL_IS_NULL_EXPR)[0]))
        .collect();
    assert_eq!(tests, vec!["Т . А ЕСТЬ NULL", "Т . Б ЕСТЬ НЕ NULL"]);
}

#[test]
fn a_reference_type_check_and_a_null_test_coexist_in_case() {
    let input = "ВЫБРАТЬ ВЫБОР КОГДА Т.Документ ССЫЛКА Документ.Заказ ТОГДА 1 КОГДА Т.Документ ЕСТЬ NULL ТОГДА 0 КОНЕЦ КАК Вид ИЗ Справочник.Т КАК Т";
    assert!(parse_sdbl(input).errors().is_empty());
    let refs = descendants_of(&root(input), SyntaxKind::SDBL_REFS_EXPR);
    assert_eq!(refs.len(), 1);
    assert_eq!(significant_text(&refs[0]), "Т . Документ ССЫЛКА Документ . Заказ");
    assert_eq!(descendants_of(&root(input), SyntaxKind::SDBL_IS_NULL_EXPR).len(), 1);
}

#[test]
fn the_null_replacement_function_and_the_null_predicate_are_different_forms() {
    let input = "ВЫБРАТЬ ЕСТЬNULL(Т.Имя, \"\") КАК Имя ИЗ Справочник.Т КАК Т ГДЕ Т.Код ЕСТЬ NULL";
    assert!(parse_sdbl(input).errors().is_empty());
    let fields = fields(input);
    let call = descendants_of(&fields[0], SyntaxKind::SDBL_FUNCTION_CALL).remove(0);
    assert_eq!(call_args(&call), vec!["Т . Имя", "\" \""]);
    assert!(descendants_of(&fields[0], SyntaxKind::SDBL_IS_NULL_EXPR).is_empty());
    let filter = descendants_of(&root(input), SyntaxKind::SDBL_WHERE_CLAUSE).remove(0);
    assert_eq!(descendants_of(&filter, SyntaxKind::SDBL_IS_NULL_EXPR).len(), 1);
    assert!(descendants_of(&filter, SyntaxKind::SDBL_FUNCTION_CALL).is_empty());
}

#[test]
fn an_unfinished_join_condition_keeps_sources_and_the_next_item() {
    let input = "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Т.Ссылка = Ц.;\nВЫБРАТЬ 2 КАК Второе";
    let parse = parse_sdbl(input);
    assert!(!parse.errors().is_empty());
    let items = package_items(input);
    assert_eq!(items.len(), 2);
    assert_eq!(significant_text(&items[1]), "ВЫБРАТЬ 2 КАК Второе");
    let aliases: Vec<String> =
        descendants_of(&items[0], SyntaxKind::SDBL_ALIAS).iter().map(significant_text).collect();
    assert_eq!(aliases, vec!["КАК Код", "КАК Т", "КАК Ц"]);
    let first_error = usize::from(parse.errors()[0].range().start());
    assert!(first_error >= input.find("Ц.;").unwrap(), "the error is at the unfinished name");
    assert!(first_error <= input.find(';').unwrap());
}

// --- Independent shared fixture for parser invariants. Written for this corpus from
// ITS pubqlang «Временные таблицы и пакетные запросы» (package, ПОМЕСТИТЬ, УНИЧТОЖИТЬ),
// «Как получить данные из разных таблиц, не связывая, а дополняя их» (ОБЪЕДИНИТЬ ВСЕ),
// «Использовать параметры виртуальных таблиц» (empty argument positions, a condition
// with a membership query) and the SELECT mini-spec «Virtual table argument behavior». ---

const PACKAGE_FIXTURE: &str = include_str!("../fixtures/sdbl_independent_package.sdbl");

fn kind_outline(input: &str) -> Vec<(usize, SyntaxKind)> {
    fn walk(node: &SyntaxNode, depth: usize, out: &mut Vec<(usize, SyntaxKind)>) {
        out.push((depth, node.kind()));
        for child in node.children() {
            walk(&child, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(&root(input), 0, &mut out);
    out
}

#[test]
fn fixture_has_the_expected_package_structure() {
    use SyntaxKind::*;
    let parse = parse_sdbl(PACKAGE_FIXTURE);
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());
    assert_eq!(
        kinds_of_package_items(PACKAGE_FIXTURE),
        vec![SDBL_SELECT_QUERY, SEMICOLON, SDBL_SELECT_QUERY, SEMICOLON, SDBL_DROP_QUERY]
    );
    let items = package_items(PACKAGE_FIXTURE);
    // First item: a union of two queries; the INTO and the virtual table belong to the
    // first branch, the membership query to the virtual table's last argument.
    let branches = items[0]
        .children()
        .find(|n| n.kind() == SDBL_SUBQUERY)
        .map(|sq| {
            descendants_of(&sq, SDBL_QUERY)
                .into_iter()
                .filter(|q| {
                    q.ancestors().skip(1).find(|a| a.kind() == SDBL_SUBQUERY).map(|a| a.parent())
                        == Some(Some(items[0].clone()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(branches.len(), 2);
    assert_eq!(field_aliases(&branches[0]), vec!["Номенклатура", "Количество"]);
    assert_eq!(field_aliases(&branches[1]), vec!["-", "-"]);
    assert_eq!(descendants_of(&branches[0], SDBL_INTO_CLAUSE).len(), 1);
    assert!(descendants_of(&branches[1], SDBL_INTO_CLAUSE).is_empty());
    let vt = descendants_of(&branches[0], SDBL_TABLE_REF).remove(0);
    let args: Vec<SyntaxKind> = vt
        .children_with_tokens()
        .map(|e| e.kind())
        .filter(|k| matches!(k, SDBL_MISSING_ARG | SDBL_LOGICAL_OR_EXPR))
        .collect();
    assert_eq!(
        args,
        vec![
            SDBL_MISSING_ARG,
            SDBL_MISSING_ARG,
            SDBL_LOGICAL_OR_EXPR,
            SDBL_MISSING_ARG,
            SDBL_LOGICAL_OR_EXPR
        ]
    );
    let membership = vt.children().filter(|n| n.kind() == SDBL_LOGICAL_OR_EXPR).last().unwrap();
    assert_eq!(descendants_of(&membership, SDBL_IN_EXPR).len(), 1);
    assert_eq!(descendants_of(&membership, SDBL_SUBQUERY).len(), 1);
    // Second item: one query with a join to a virtual table and a grouping.
    assert_eq!(descendants_of(&items[1], SDBL_JOIN_CLAUSE).len(), 1);
    assert_eq!(descendants_of(&items[1], SDBL_GROUP_CLAUSE).len(), 1);
    assert!(descendants_of(&items[1], SDBL_UNION_CLAUSE).is_empty());
}

#[test]
fn fixture_text_is_reproduced_and_each_lexeme_is_held_once() {
    let root = root(PACKAGE_FIXTURE);
    assert_eq!(root.text().to_string(), PACKAGE_FIXTURE);
    let mut tokens = root.descendants_with_tokens().filter_map(|e| e.into_token()).peekable();
    let mut rebuilt = String::new();
    let mut significant = 0;
    for token in tokens.by_ref() {
        rebuilt.push_str(token.text());
        if !token.kind().is_trivia() {
            significant += 1;
            // A significant token never starts or ends with whitespace: trivia is separate.
            assert_eq!(token.text().trim(), token.text(), "{token:?}");
        }
    }
    assert_eq!(rebuilt, PACKAGE_FIXTURE);
    // Every node's text starts and ends on a significant token or on trivia it owns,
    // and no two tokens overlap: the token ranges tile the input exactly.
    let mut expected_start = 0;
    for token in root.descendants_with_tokens().filter_map(|e| e.into_token()) {
        assert_eq!(usize::from(token.text_range().start()), expected_start);
        expected_start = usize::from(token.text_range().end());
    }
    assert_eq!(expected_start, PACKAGE_FIXTURE.len());
    assert!(significant > 100, "the fixture is meant to exercise many lexemes");
}

#[test]
fn fixture_structure_does_not_depend_on_line_breaks() {
    let reference = kind_outline(PACKAGE_FIXTURE);
    let crlf = PACKAGE_FIXTURE.replace('\n', "\r\n");
    assert_eq!(kind_outline(&crlf), reference);
    // Joining lines with a space is legal everywhere except after a line comment.
    let without_comments: String = PACKAGE_FIXTURE
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let one_line = without_comments.replace('\n', " ");
    assert_eq!(kind_outline(&one_line), kind_outline(&without_comments));
    assert!(parse_sdbl(&one_line).errors().is_empty());
}

#[test]
fn damage_to_the_fixture_is_reported_where_it_happens() {
    // An unclosed argument list: the error lies inside the first package item.
    let unclosed = PACKAGE_FIXTURE.replacen("Склады.Активен))", "Склады.Активен)", 1);
    let first_separator = unclosed.find("\n;").unwrap();
    let errors = parse_sdbl(&unclosed).errors().to_vec();
    assert!(!errors.is_empty());
    let start = usize::from(errors[0].range().start());
    assert!(
        start >= unclosed.find("Склады.Активен").unwrap() && start <= first_separator + 1,
        "{errors:?}"
    );

    // A missing alias name in the second item: the error points at the following keyword
    // and the first item stays error-free.
    let no_alias = PACKAGE_FIXTURE.replacen("КАК Цена\nИЗ", "КАК\nИЗ", 1);
    let errors = parse_sdbl(&no_alias).errors().to_vec();
    assert_eq!(errors.len(), 1, "{errors:?}");
    let second_from = no_alias.find("КАК\nИЗ").unwrap() + "КАК\n".len();
    assert_eq!(usize::from(errors[0].range().start()), second_from);
}

#[test]
fn an_unfinished_member_after_a_call_leaves_the_connective_to_the_condition() {
    for connective in ["И", "ИЛИ", "AND", "OR"] {
        for gap in [" ", "\n\t"] {
            let input = format!(
                "ВЫБРАТЬ 1 КАК Н ГДЕ ВЫРАЗИТЬ(Т.Ссылка КАК Справочник.Т).{gap}{connective} Т.Код = &Код; ВЫБРАТЬ 2 КАК М"
            );
            let parse = parse_sdbl(&input);
            assert!(!parse.errors().is_empty(), "an unfinished member must be reported");
            let items = package_items(&input);
            assert_eq!(items.len(), 2);
            assert_eq!(significant_text(&items[1]), "ВЫБРАТЬ 2 КАК М");
            let filter = descendants_of(&items[0], SyntaxKind::SDBL_WHERE_CLAUSE).remove(0);
            let operators: Vec<_> = filter
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| matches!(t.kind(), SyntaxKind::KW_AND | SyntaxKind::KW_OR))
                .collect();
            assert_eq!(operators.len(), 1, "{input}");
            let operator = &operators[0];
            assert!(matches!(
                operator.parent().unwrap().kind(),
                SyntaxKind::SDBL_LOGICAL_AND_EXPR | SyntaxKind::SDBL_LOGICAL_OR_EXPR
            ));
            let columns: Vec<_> = descendants_of(&filter, SyntaxKind::SDBL_COLUMN_REF)
                .iter()
                .map(significant_text)
                .collect();
            expect![[r#"
                ["Т . Ссылка", "Т . Код"]
            "#]]
            .assert_eq(&format!("{columns:?}\n"));
            let parameters = descendants_of(&filter, SyntaxKind::SDBL_PARAMETER);
            assert_eq!(parameters.len(), 1);
            assert_eq!(significant_text(&parameters[0]), "&Код");
        }
    }

    for member in ["И", "ИЛИ", "AND", "OR"] {
        let input = format!("ВЫБРАТЬ ВЫРАЗИТЬ(Т.Ссылка КАК Справочник.Т).{member} КАК Поле");
        assert!(parse_sdbl(&input).errors().is_empty(), "{input}");
        let call = descendants_of(&root(&input), SyntaxKind::SDBL_FUNCTION_CALL).remove(0);
        assert!(significant_text(&call).ends_with(&format!(". {member}")), "{input}");
    }
}
