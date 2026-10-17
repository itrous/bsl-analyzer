//! Independent HIR contracts for SDBL lowering.
//!
//! Inputs are written for this file from ITS `pubqlang` (query package and temporary
//! tables, nested queries, unions, joins) and checked against the local `sdbl-hir` API:
//! query ranges, nested-query HIRs, alias and field identity, and the semantic token
//! categories the IDE paints.

use sdbl_hir::{lower_sdbl_to_hir, ExprHir, FunctionKind, JoinType, SdblPackage, TokenCategory};

fn lower(input: &str) -> SdblPackage {
    lower_sdbl_to_hir(&parser::parse_sdbl(input), None)
}

fn slice(input: &str, range: text_size::TextRange) -> &str {
    &input[usize::from(range.start())..usize::from(range.end())]
}

#[test]
fn each_package_query_gets_its_own_hir_and_range() {
    let input = "ВЫБРАТЬ Т.Код КАК Код ПОМЕСТИТЬ Врем ИЗ Справочник.Товары КАК Т;\nВЫБРАТЬ Вр.Код КАК Код ИЗ Врем КАК Вр";
    let package = lower(input);
    let ranges: Vec<&str> = package.queries().iter().map(|q| slice(input, q.range)).collect();
    assert_eq!(
        ranges,
        vec![
            "ВЫБРАТЬ Т.Код КАК Код ПОМЕСТИТЬ Врем ИЗ Справочник.Товары КАК Т",
            "ВЫБРАТЬ Вр.Код КАК Код ИЗ Врем КАК Вр",
        ]
    );
    assert_eq!(package.queries()[0].hir.into_table.as_ref().map(|n| n.as_str()), Some("Врем"));
}

#[test]
fn a_temporary_table_exposes_the_selected_names_to_the_next_query() {
    let input = "ВЫБРАТЬ Т.Код КАК Артикул ПОМЕСТИТЬ Врем ИЗ Справочник.Товары КАК Т;\nВЫБРАТЬ Вр.Артикул КАК А ИЗ Врем КАК Вр";
    let package = lower(input);
    let source = &package.queries()[1].hir.from[0];
    let Some(sdbl_hir::ResolvedTable::TempTable { fields, .. }) = &source.metadata else {
        panic!("the second query must see the temporary table: {:?}", source.metadata);
    };
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["Артикул"]);
}

#[test]
fn union_members_are_separate_queries_of_the_package() {
    let input = "ВЫБРАТЬ А.Сумма КАК Сумма ИЗ РегистрНакопления.План КАК А\nОБЪЕДИНИТЬ ВСЕ\nВЫБРАТЬ Б.Сумма ИЗ РегистрНакопления.Факт КАК Б";
    let package = lower(input);
    assert_eq!(package.queries().len(), 2);
    assert_eq!(
        slice(input, package.queries()[1].range),
        "ВЫБРАТЬ Б.Сумма ИЗ РегистрНакопления.Факт КАК Б"
    );
}

#[test]
fn a_nested_source_carries_its_own_hir_and_the_outer_alias() {
    let input =
        "ВЫБРАТЬ Вл.Код КАК Код ИЗ (ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т) КАК Вл";
    let package = lower(input);
    let source = &package.queries()[0].hir.from[0];
    assert_eq!(source.alias.as_ref().map(|a| a.as_str()), Some("Вл"));
    assert_eq!(source.subquery.len(), 1);
    let nested = &source.subquery[0];
    assert_eq!(nested.from[0].alias.as_ref().map(|a| a.as_str()), Some("Т"));
    assert_eq!(slice(input, nested.range), "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т");
}

#[test]
fn a_membership_test_against_a_query_lowers_the_query() {
    let input = "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ГДЕ Т.Ссылка В (ВЫБРАТЬ Ц.Товар ИЗ РегистрСведений.Цены КАК Ц)";
    let package = lower(input);
    let Some(ExprHir::In { values: sdbl_hir::InValues::Subquery(nested), .. }) =
        &package.queries()[0].hir.where_clause
    else {
        panic!("expected a membership test against a query");
    };
    assert_eq!(nested.from[0].alias.as_ref().map(|a| a.as_str()), Some("Ц"));
}

#[test]
fn joins_keep_their_kind_alias_and_condition() {
    let input = "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Т.Ссылка = Ц.Товар ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Склады КАК С ПО С.Ссылка = Ц.Склад";
    let package = lower(input);
    let joins = &package.queries()[0].hir.joins;
    let summary: Vec<(JoinType, &str, &str)> = joins
        .iter()
        .map(|j| {
            (
                j.join_type,
                j.table.alias.as_ref().map(|a| a.as_str()).unwrap_or(""),
                slice(input, j.condition.as_ref().map(|c| c.range()).unwrap()),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (JoinType::Left, "Ц", "Т.Ссылка = Ц.Товар"),
            (JoinType::Inner, "С", "С.Ссылка = Ц.Склад"),
        ]
    );
}

#[test]
fn selected_fields_keep_alias_and_derived_name() {
    let input = "ВЫБРАТЬ Т.Наименование КАК Имя, Т.Код ИЗ Справочник.Товары КАК Т";
    let package = lower(input);
    let fields = &package.queries()[0].hir.select.fields;
    let names: Vec<(Option<&str>, Option<&str>, &str)> = fields
        .iter()
        .map(|f| {
            (
                f.alias.as_ref().map(|a| a.as_str()),
                f.alias_or_name().map(|n| n.as_str()),
                slice(input, f.range),
            )
        })
        .collect();
    assert_eq!(
        names,
        vec![(Some("Имя"), Some("Имя"), "Т.Наименование КАК Имя"), (None, Some("Код"), "Т.Код"),]
    );
}

fn painted(input: &str) -> Vec<(String, TokenCategory)> {
    let package = lower(input);
    package
        .source_map
        .all_tokens()
        .map(|(info, category)| (slice(input, info.range).to_string(), category))
        .collect()
}

#[test]
fn semantic_tokens_classify_keywords_sources_aliases_and_fields() {
    let input = "ВЫБРАТЬ Т.Наименование КАК Имя ИЗ Справочник.Товары КАК Т ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Т.Ссылка = Ц.Товар ГДЕ Т.Код > 0";
    let tokens = painted(input);
    for (text, category) in [
        ("ВЫБРАТЬ", TokenCategory::ClauseKeyword),
        ("ИЗ", TokenCategory::ClauseKeyword),
        ("ГДЕ", TokenCategory::ClauseKeyword),
        ("ЛЕВОЕ", TokenCategory::JoinKeyword),
        ("СОЕДИНЕНИЕ", TokenCategory::JoinKeyword),
        ("Товары", TokenCategory::TableName),
        ("Т", TokenCategory::TableAlias),
        ("Ц", TokenCategory::TableAlias),
        ("Наименование", TokenCategory::FieldName),
        ("Товар", TokenCategory::FieldName),
        ("Имя", TokenCategory::FieldAlias),
    ] {
        assert!(
            tokens.iter().any(|(t, c)| t == text && *c == category),
            "{text} must be painted as {category:?}: {tokens:?}"
        );
    }
}

#[test]
fn every_token_range_of_the_source_map_lies_on_its_own_text() {
    let input = "ВЫБРАТЬ РАЗЛИЧНЫЕ Т.Код КАК Код, СУММА(Т.Цена) КАК Цена ИЗ Справочник.Товары КАК Т СГРУППИРОВАТЬ ПО Т.Код";
    let package = lower(input);
    for (info, category) in package.source_map.all_tokens() {
        assert_eq!(slice(input, info.range), info.text.as_str(), "{category:?}");
    }
    assert!(!package.source_map.tokens_by_category(TokenCategory::AggregateFunction).is_empty());
}

#[test]
fn operator_source_map_has_one_entry_per_source_occurrence() {
    let input = "ВЫБРАТЬ Т.Код ИЗ Справочник.Товары КАК Т ГДЕ Т.Код = 1 И Т.Код > 0";
    let package = lower(input);
    let operators = package.source_map.tokens_by_category(TokenCategory::Operator);
    let ranges: Vec<_> = operators.iter().map(|token| token.range).collect();
    let expected: Vec<_> = [" = ", " И ", " > "]
        .into_iter()
        .map(|spaced| {
            let start = input.find(spaced).unwrap() + 1;
            let end = start + spaced.trim().len();
            text_size::TextRange::new((start as u32).into(), (end as u32).into())
        })
        .collect();
    assert_eq!(ranges, expected);
    for range in expected {
        let (info, category) = package.source_map.token_at_range(range).unwrap();
        assert_eq!(category, TokenCategory::Operator);
        assert_eq!(
            info.text.as_str(),
            &input[usize::from(range.start())..usize::from(range.end())]
        );
    }
    assert_eq!(
        package
            .source_map
            .all_tokens()
            .filter(|(_, category)| *category == TokenCategory::Operator)
            .count(),
        3
    );
}

fn expected_token_at(
    input: &str,
    context: &str,
    token: &str,
    category: TokenCategory,
) -> (text_size::TextRange, String, TokenCategory) {
    let context_start =
        input.find(context).unwrap_or_else(|| panic!("missing context {context:?}"));
    let token_start =
        context.find(token).unwrap_or_else(|| panic!("missing token {token:?} in {context:?}"));
    let start = context_start + token_start;
    let end = start + token.len();
    (
        text_size::TextRange::new((start as u32).into(), (end as u32).into()),
        token.to_string(),
        category,
    )
}

fn predicate_token_mismatch(
    input: &str,
    expected: &[(&str, &str, TokenCategory)],
) -> Option<String> {
    let package = lower(input);
    let mut actual: Vec<_> = package
        .source_map
        .all_tokens()
        .filter(|(_, category)| {
            matches!(category, TokenCategory::Operator | TokenCategory::SpecialKeyword)
        })
        .map(|(token, category)| (token.range, token.text.to_string(), category))
        .collect();
    let mut expected: Vec<_> = expected
        .iter()
        .map(|(context, token, category)| expected_token_at(input, context, token, *category))
        .collect();
    actual.sort_by_key(|(range, _, _)| range.start());
    expected.sort_by_key(|(range, _, _)| range.start());
    for (range, text, category) in &actual {
        if slice(input, *range) != text
            || package.source_map.token_at_range(*range).unwrap().1 != *category
        {
            return Some(format!(
                "invalid source-map entry {text:?} {range:?} {category:?} in {input:?}"
            ));
        }
    }
    (actual != expected).then(|| format!("{input:?}: actual={actual:?}, expected={expected:?}"))
}

#[test]
fn predicate_operators_are_owned_by_their_source_nodes() {
    let prefix = "ВЫБРАТЬ Т.Код ИЗ Т ГДЕ ";
    let cases = [
        (
            "Т.Код НЕ В (1, 2)",
            vec![
                ("Т.Код НЕ В", "НЕ", TokenCategory::Operator),
                ("Т.Код НЕ В", "В", TokenCategory::SpecialKeyword),
            ],
        ),
        (
            "Т.Код НЕ МЕЖДУ 1 И 2",
            vec![
                ("Т.Код НЕ МЕЖДУ", "НЕ", TokenCategory::Operator),
                ("Т.Код НЕ МЕЖДУ", "МЕЖДУ", TokenCategory::SpecialKeyword),
                ("1 И 2", "И", TokenCategory::Operator),
            ],
        ),
        (
            "Т.Код НЕ ПОДОБНО \"x\"",
            vec![
                ("Т.Код НЕ ПОДОБНО", "НЕ", TokenCategory::Operator),
                ("Т.Код НЕ ПОДОБНО", "ПОДОБНО", TokenCategory::SpecialKeyword),
            ],
        ),
        (
            "Т.Код ЕСТЬ НЕ NULL",
            vec![
                ("Т.Код ЕСТЬ НЕ NULL", "ЕСТЬ", TokenCategory::SpecialKeyword),
                ("Т.Код ЕСТЬ НЕ NULL", "НЕ", TokenCategory::Operator),
                ("Т.Код ЕСТЬ НЕ NULL", "NULL", TokenCategory::SpecialKeyword),
            ],
        ),
        (
            "Т.Код НЕ В ИЕРАРХИИ (&Корень)",
            vec![
                ("Т.Код НЕ В ИЕРАРХИИ", "НЕ", TokenCategory::Operator),
                ("Т.Код НЕ В ИЕРАРХИИ", "В", TokenCategory::SpecialKeyword),
                ("Т.Код НЕ В ИЕРАРХИИ", "ИЕРАРХИИ", TokenCategory::SpecialKeyword),
            ],
        ),
        (
            "НЕ (Т.Код = 1)",
            vec![
                ("НЕ (Т.Код", "НЕ", TokenCategory::Operator),
                ("Т.Код = 1", "=", TokenCategory::Operator),
            ],
        ),
        (
            "НЕ (Т.Код ССЫЛКА Справочник.Номенклатура)",
            vec![
                ("НЕ (Т.Код", "НЕ", TokenCategory::Operator),
                ("Т.Код ССЫЛКА", "ССЫЛКА", TokenCategory::SpecialKeyword),
            ],
        ),
        (
            "Т.Код В (ВЫБРАТЬ В.Код ИЗ Справочник.В ГДЕ В.Код НЕ МЕЖДУ 1 И 2)",
            vec![
                ("Т.Код В (", "В", TokenCategory::SpecialKeyword),
                ("В.Код НЕ МЕЖДУ", "НЕ", TokenCategory::Operator),
                ("В.Код НЕ МЕЖДУ", "МЕЖДУ", TokenCategory::SpecialKeyword),
                ("1 И 2", "И", TokenCategory::Operator),
            ],
        ),
    ];

    let mut mismatches = Vec::new();
    for (expression, expected) in &cases {
        let input = format!("{prefix}{expression}");
        if let Some(mismatch) = predicate_token_mismatch(&input, expected) {
            mismatches.push(mismatch);
        }
    }
    assert!(mismatches.is_empty(), "predicate token gaps:\n{}", mismatches.join("\n"));

    let nested =
        format!("{prefix}Т.Код В (ВЫБРАТЬ В.Код ИЗ Справочник.В ГДЕ В.Код НЕ МЕЖДУ 1 И 2)");
    let package = lower(&nested);
    assert!(matches!(
        package.queries()[0].hir.where_clause,
        Some(ExprHir::In { negated: true, .. })
    ));
    assert_eq!(
        package
            .source_map
            .tokens_by_category(TokenCategory::Operator)
            .iter()
            .filter(|token| token.text == "НЕ")
            .count(),
        1,
        "nested NOT belongs only to the inner predicate"
    );
}

#[test]
fn equal_operators_keep_distinct_join_having_and_package_ranges() {
    let input = "ВЫБРАТЬ Т.Код, СУММА(Ц.Цена) ИЗ Справочник.Товары КАК Т СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Т.Ссылка = Ц.Товар СГРУППИРОВАТЬ ПО Т.Код ИМЕЮЩИЕ СУММА(Ц.Цена) = 1; ВЫБРАТЬ Т.Код ИЗ Справочник.Товары КАК Т ГДЕ Т.Код = 2";
    let expected: Vec<_> = input
        .match_indices('=')
        .map(|(start, _)| {
            (
                text_size::TextRange::new((start as u32).into(), ((start + 1) as u32).into()),
                "=".to_string(),
            )
        })
        .collect();
    let package = lower(input);
    let mut actual: Vec<_> = package
        .source_map
        .tokens_by_category(TokenCategory::Operator)
        .iter()
        .filter(|token| token.text == "=")
        .map(|token| (token.range, token.text.to_string()))
        .collect();
    actual.sort_by_key(|(range, _)| range.start());
    assert_eq!(actual, expected);
    assert_eq!(package.queries().len(), 2);
    for (range, text) in actual {
        assert_eq!(slice(input, range), text);
        assert_eq!(package.source_map.token_at_range(range).unwrap().0.text, text);
    }
}

#[test]
fn long_nested_operator_chain_has_exact_source_ranges() {
    let terms: Vec<_> = (0..64).map(|index| format!("((Т.Код = {index}))")).collect();
    let expression = terms.join(" И ");
    let input = format!("ВЫБРАТЬ Т.Код ИЗ Справочник.Товары КАК Т ГДЕ {expression}");
    let mut expected: Vec<_> = input
        .match_indices(" = ")
        .map(|(start, _)| {
            let offset = start + 1;
            (
                text_size::TextRange::new((offset as u32).into(), ((offset + 1) as u32).into()),
                "=".to_string(),
            )
        })
        .chain(input.match_indices(" И ").map(|(start, _)| {
            let offset = start + 1;
            (
                text_size::TextRange::new((offset as u32).into(), ((offset + 2) as u32).into()),
                "И".to_string(),
            )
        }))
        .collect();
    expected.sort_by_key(|(range, _)| range.start());
    let package = lower(&input);
    let mut actual: Vec<_> = package
        .source_map
        .tokens_by_category(TokenCategory::Operator)
        .iter()
        .map(|token| (token.range, token.text.to_string()))
        .collect();
    actual.sort_by_key(|(range, _)| range.start());
    assert_eq!(actual.len(), 127);
    assert_eq!(actual, expected);
    for (range, text) in actual {
        assert_eq!(slice(&input, range), text);
    }
}

#[test]
fn unary_predicate_count_and_nested_query_operators_keep_their_owners() {
    let input = "ВЫБРАТЬ COUNT(*), -Т.Код + +1 ИЗ Справочник.Товары КАК Т ГДЕ Т.Код = 1 И Т.Код > 0 И Т.Код НЕ В (ВЫБРАТЬ В.Код ИЗ Справочник.В КАК В ГДЕ В.Код = 2)";
    let package = lower(input);
    assert!(matches!(
        &package.queries()[0].hir.select.fields[0].expr,
        ExprHir::FunctionCall { function: FunctionKind::Count, .. }
    ));
    let operators = package.source_map.tokens_by_category(TokenCategory::Operator);
    let mut actual_texts: Vec<_> = operators.iter().map(|token| token.text.as_str()).collect();
    actual_texts.sort();
    let mut expected_texts = vec!["*", "-", "+", "+", "=", "И", ">", "И", "НЕ", "="];
    expected_texts.sort();
    assert_eq!(actual_texts, expected_texts);
    let ranges: Vec<_> = operators.iter().map(|token| token.range).collect();
    assert_eq!(ranges.iter().collect::<std::collections::HashSet<_>>().len(), ranges.len());
    for token in operators {
        assert_eq!(slice(input, token.range), token.text.as_str());
    }

    for token in operators {
        assert_eq!(package.source_map.token_at_range(token.range).unwrap().0, token);
    }
    assert_eq!(
        package
            .source_map
            .all_tokens()
            .filter(|(_, category)| *category == TokenCategory::Operator)
            .count(),
        operators.len()
    );

    let select_star = lower("ВЫБРАТЬ * ИЗ Справочник.Товары");
    assert!(select_star.source_map.tokens_by_category(TokenCategory::Operator).is_empty());

    let ignored_text = lower("ВЫБРАТЬ Т.Код ИЗ Т КАК Т ГДЕ Т.Код = 1 // = >\nИ Т.Имя = \"= >\"");
    let painted: Vec<_> = ignored_text
        .source_map
        .tokens_by_category(TokenCategory::Operator)
        .iter()
        .map(|token| token.text.as_str())
        .collect();
    assert_eq!(painted, ["=", "И", "="]);
}

// --- Second package: nested unions, tabular parts, package ranges, recovery of the
// source map, cursor context. Sources: ITS pubqlang «Как использовать данные одного
// запроса внутри другого запроса», «Как получить данные из разных таблиц, не связывая,
// а дополняя их», «Как получить данные из табличной части некоторого документа»,
// «Как получить данные из разных таблиц, связанных несколькими соединениями»,
// «Временные таблицы и пакетные запросы»; local API `SdblHir`, `TableRef`, `JoinHir`,
// `SdblSourceMap`, `detect_context`. Recovery and context are local editor contracts. ---

fn alias_of(table: &sdbl_hir::TableRef) -> &str {
    table.alias.as_ref().map(|a| a.as_str()).unwrap_or("")
}

fn selected(hir: &sdbl_hir::SdblHir, input: &str) -> Vec<String> {
    hir.select.fields.iter().map(|f| slice(input, f.range).to_string()).collect()
}

#[test]
fn each_union_branch_of_a_nested_source_is_its_own_hir() {
    let input = "ВЫБРАТЬ Вл.Код КАК Код ИЗ (ВЫБРАТЬ А.Код КАК Код ИЗ Справочник.А КАК А ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Б.Код ИЗ Справочник.Б КАК Б) КАК Вл";
    let package = lower(input);
    let outer = &package.queries()[0].hir;
    assert_eq!(alias_of(&outer.from[0]), "Вл");
    let branches = &outer.from[0].subquery;
    assert_eq!(branches.len(), 2);
    assert_eq!(alias_of(&branches[0].from[0]), "А");
    assert_eq!(alias_of(&branches[1].from[0]), "Б");
    assert_eq!(selected(&branches[0], input), vec!["А.Код КАК Код"]);
    assert_eq!(selected(&branches[1], input), vec!["Б.Код"]);
}

#[test]
fn unions_on_two_levels_keep_their_sources_on_their_own_level() {
    let input = "ВЫБРАТЬ Внеш.Код КАК Код ИЗ (ВЫБРАТЬ Сред.Код КАК Код ИЗ (ВЫБРАТЬ А.Код КАК Код ИЗ Справочник.А КАК А ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Б.Код ИЗ Справочник.Б КАК Б) КАК Сред ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Ц.Код ИЗ Справочник.Ц КАК Ц) КАК Внеш\nОБЪЕДИНИТЬ ВСЕ\nВЫБРАТЬ Д.Код ИЗ Справочник.Д КАК Д";
    let package = lower(input);
    assert_eq!(package.queries().len(), 2);
    assert_eq!(alias_of(&package.queries()[1].hir.from[0]), "Д");
    let middle = &package.queries()[0].hir.from[0].subquery;
    assert_eq!(middle.len(), 2);
    assert_eq!(alias_of(&middle[0].from[0]), "Сред");
    assert_eq!(alias_of(&middle[1].from[0]), "Ц");
    let inner = &middle[0].from[0].subquery;
    let inner_aliases: Vec<&str> = inner.iter().map(|h| alias_of(&h.from[0])).collect();
    assert_eq!(inner_aliases, vec!["А", "Б"]);
    assert!(middle[1].from[0].subquery.is_empty());
}

#[test]
fn a_tabular_part_joins_as_a_source_with_its_full_name() {
    let input = "ВЫБРАТЬ Заказ.Номер КАК Номер, Состав.Товар КАК Товар ИЗ Документ.Заказ КАК Заказ ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ.Состав КАК Состав ПО Состав.Ссылка = Заказ.Ссылка";
    let package = lower(input);
    let hir = &package.queries()[0].hir;
    let join = &hir.joins[0];
    assert_eq!(join.join_type, JoinType::Inner);
    let parts: Vec<&str> = join.table.parts.iter().map(|p| p.as_str()).collect();
    assert_eq!(parts, vec!["Документ", "Заказ", "Состав"]);
    assert_eq!(alias_of(&join.table), "Состав");
    let aliases: Vec<&str> =
        hir.select.fields.iter().filter_map(|f| f.alias.as_ref().map(|a| a.as_str())).collect();
    assert_eq!(aliases, vec!["Номер", "Товар"]);
}

#[test]
fn dotted_paths_in_the_projection_stay_bound_to_their_sources() {
    let input = "ВЫБРАТЬ Состав.Ссылка.Номер КАК Номер, Цены.Цена КАК Цена ИЗ Документ.Заказ.Состав КАК Состав ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Состав.Товар";
    let package = lower(input);
    let hir = &package.queries()[0].hir;
    let heads: Vec<(String, usize)> = hir
        .select
        .fields
        .iter()
        .map(|f| match &f.expr {
            ExprHir::ColumnRef { parts, .. } => (parts[0].to_string(), parts.len()),
            other => panic!("expected a column reference, got {other:?}"),
        })
        .collect();
    assert_eq!(heads, vec![("Состав".to_string(), 3), ("Цены".to_string(), 2)]);
    assert_eq!(alias_of(&hir.from[0]), "Состав");
    assert_eq!(alias_of(&hir.joins[0].table), "Цены");
}

#[test]
fn each_join_kind_is_attached_to_its_own_table_inside_a_nested_query() {
    let input = "ВЫБРАТЬ Вл.Номер КАК Номер ИЗ (ВЫБРАТЬ Заказ.Номер КАК Номер, Состав.Товар КАК Товар, Цены.Цена КАК Цена ИЗ Документ.Заказ КАК Заказ ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ.Состав КАК Состав ПО Состав.Ссылка = Заказ.Ссылка ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Состав.Товар) КАК Вл";
    let package = lower(input);
    let inner = &package.queries()[0].hir.from[0].subquery[0];
    let joins: Vec<(JoinType, &str)> =
        inner.joins.iter().map(|j| (j.join_type, alias_of(&j.table))).collect();
    assert_eq!(joins, vec![(JoinType::Inner, "Состав"), (JoinType::Left, "Цены")]);
    let aliases: Vec<&str> =
        inner.select.fields.iter().filter_map(|f| f.alias.as_ref().map(|a| a.as_str())).collect();
    assert_eq!(aliases, vec!["Номер", "Товар", "Цена"]);
}

#[test]
fn union_branches_with_tabular_sources_keep_projection_source_and_join() {
    let input = "ВЫБРАТЬ Вл.Товар КАК Товар ИЗ (ВЫБРАТЬ Состав.Товар КАК Товар ИЗ Документ.Заказ.Состав КАК Состав ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ КАК Заказ ПО Заказ.Ссылка = Состав.Ссылка ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Возврат.Товар ИЗ Документ.Возврат.Товары КАК Возврат ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Склады КАК Склад ПО Склад.Ссылка = Возврат.Склад) КАК Вл";
    let package = lower(input);
    let branches = &package.queries()[0].hir.from[0].subquery;
    let summary: Vec<(Vec<String>, String, (JoinType, String))> = branches
        .iter()
        .map(|b| {
            (
                selected(b, input),
                b.from[0].parts.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("."),
                (b.joins[0].join_type, alias_of(&b.joins[0].table).to_string()),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                vec!["Состав.Товар КАК Товар".to_string()],
                "Документ.Заказ.Состав".to_string(),
                (JoinType::Inner, "Заказ".to_string())
            ),
            (
                vec!["Возврат.Товар".to_string()],
                "Документ.Возврат.Товары".to_string(),
                (JoinType::Left, "Склад".to_string())
            ),
        ]
    );
}

#[test]
fn a_later_package_query_range_covers_all_its_fields_and_nothing_else() {
    let first = "ВЫБРАТЬ 1 КАК Н";
    let second = "ВЫБРАТЬ ВЫБОР КОГДА Т.Код > 0 ТОГДА \"Плюс\" ИНАЧЕ \"Минус\" КОНЕЦ КАК Знак, ВЫРАЗИТЬ(Т.Имя КАК СТРОКА(10)) КАК Кратко ИЗ Справочник.Т КАК Т";
    let input = format!("{first};\n{second}");
    let package = lower(&input);
    assert_eq!(slice(&input, package.queries()[0].range), first);
    assert_eq!(slice(&input, package.queries()[1].range), second);
    assert_eq!(
        selected(&package.queries()[1].hir, &input),
        vec![
            "ВЫБОР КОГДА Т.Код > 0 ТОГДА \"Плюс\" ИНАЧЕ \"Минус\" КОНЕЦ КАК Знак",
            "ВЫРАЗИТЬ(Т.Имя КАК СТРОКА(10)) КАК Кратко",
        ]
    );
}

fn has_token(input: &str, text: &str, category: TokenCategory) -> bool {
    lower(input)
        .source_map
        .all_tokens()
        .any(|(info, c)| c == category && slice(input, info.range) == text)
}

fn assert_tokens_after_unfinished_fields(input: &str) {
    for (text, category) in [
        ("Артикул", TokenCategory::FieldAlias),
        ("Товары", TokenCategory::TableName),
        ("ГДЕ", TokenCategory::ClauseKeyword),
    ] {
        assert!(has_token(input, text, category), "{text} {category:?} after an unfinished field");
    }
    let code = input.rfind("Код").unwrap();
    let package = lower(input);
    assert!(package
        .source_map
        .all_tokens()
        .any(|(info, c)| c == TokenCategory::FieldName && usize::from(info.range.start()) == code));
}

#[test]
fn an_unfinished_field_does_not_stop_the_tokens_that_follow() {
    assert_tokens_after_unfinished_fields(
        "ВЫБРАТЬ Т., Т.Код КАК Артикул ИЗ Справочник.Товары КАК Т ГДЕ Т.Код > 0",
    );
    assert_tokens_after_unfinished_fields(
        "ВЫБРАТЬ Т.Код КАК Артикул, Т.Цена * ИЗ Справочник.Товары КАК Т ГДЕ Т.Код > 0",
    );
}

#[test]
fn unfinished_fields_with_operators_do_not_stop_the_tokens_that_follow() {
    assert_tokens_after_unfinished_fields(
        "ВЫБРАТЬ Т.Цена +, Т.Количество * , Т.Код КАК Артикул ИЗ Справочник.Товары КАК Т ГДЕ Т.Код > 0",
    );
}

#[test]
fn totals_aliases_have_one_source_entry_and_are_not_column_references() {
    for (input, aliases) in [
        ("ВЫБРАТЬ А ИЗ Т ИТОГИ КОЛИЧЕСТВО(А) КАК К ПО А", vec!["К"]),
        (
            "SELECT A FROM T TOTALS SUM(A) + 1 Result, COUNT(A) AS Order BY A",
            vec!["Result", "Order"],
        ),
    ] {
        assert!(parser::parse_sdbl(input).errors().is_empty());
        let package = lower(input);
        for alias in aliases {
            let expected = input.rfind(alias).unwrap();
            let entries: Vec<_> =
                package.source_map.all_tokens().filter(|(info, _)| info.text == alias).collect();
            assert_eq!(entries.len(), 1, "{alias}: {entries:?}");
            let (info, category) = entries[0];
            assert_eq!(category, TokenCategory::FieldAlias);
            assert_eq!(usize::from(info.range.start()), expected);
            assert_eq!(slice(input, info.range), alias);
            assert_eq!(package.source_map.token_at_range(info.range).unwrap().1, category);
        }
        assert!(!package
            .source_map
            .tokens_by_category(TokenCategory::FieldName)
            .iter()
            .any(|info| matches!(info.text.as_str(), "AS" | "КАК" | "Result" | "Order" | "К")));
        let ranges: Vec<_> = package.source_map.all_tokens().map(|(info, _)| info.range).collect();
        assert_eq!(ranges.iter().collect::<std::collections::HashSet<_>>().len(), ranges.len());
        assert!(package.all_diagnostics().all(|diagnostic| !matches!(diagnostic,
            sdbl_hir::SdblDiagnostic::UnknownField { field_name, .. }
                if matches!(field_name.as_str(), "К" | "Result" | "Order" | "AS" | "КАК")
        )));
    }
}

#[test]
fn an_unfinished_alias_keeps_the_query_and_the_next_clause() {
    let input = "ВЫБРАТЬ Т.Наименование КАК , Т.Код КАК Код ИЗ Справочник.Товары КАК Т";
    let package = lower(input);
    assert_eq!(package.queries().len(), 1);
    assert!(has_token(input, "Код", TokenCategory::FieldAlias));
    assert!(has_token(input, "ИЗ", TokenCategory::ClauseKeyword));
    assert!(has_token(input, "Т", TokenCategory::TableAlias));
}

#[test]
fn an_unfinished_source_in_a_union_branch_keeps_the_next_branch() {
    let input = "ВЫБРАТЬ А.Код КАК Код ИЗ Справочник. ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Б.Код ИЗ Справочник.Склады КАК Б";
    assert!(has_token(input, "Склады", TokenCategory::TableName));
    let alias = input.rfind("Б").unwrap();
    let package = lower(input);
    assert!(package.source_map.all_tokens().any(
        |(info, c)| c == TokenCategory::TableAlias && usize::from(info.range.start()) == alias
    ));
}

#[test]
fn an_unfinished_join_condition_keeps_sources_and_the_next_query() {
    let input = "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Т.Ссылка = Ц.;\nВЫБРАТЬ С.Код КАК Код ИЗ Справочник.Склады КАК С";
    let package = lower(input);
    assert_eq!(package.queries().len(), 2);
    let first = &package.queries()[0].hir;
    assert_eq!(alias_of(&first.from[0]), "Т");
    assert_eq!(alias_of(&first.joins[0].table), "Ц");
    assert_eq!(alias_of(&package.queries()[1].hir.from[0]), "С");
    assert!(has_token(input, "Склады", TokenCategory::TableName));
}

fn context_at_end(text: &str) -> sdbl_hir::SdblCompletionContext {
    sdbl_hir::detect_context(text, text_size::TextSize::of(text))
}

#[test]
fn a_qualified_name_right_after_a_parenthesis_has_an_empty_prefix() {
    let context = context_at_end("ВЫБРАТЬ ЕСТЬNULL(Т.");
    assert_eq!(
        context,
        sdbl_hir::SdblCompletionContext::AfterTableAlias {
            alias: "Т".to_string(),
            prefix: String::new()
        }
    );
}

#[test]
fn a_typed_part_after_a_parenthesis_is_the_prefix_not_the_alias() {
    let context = context_at_end("ВЫБРАТЬ ЕСТЬNULL(Т.Наим");
    assert_eq!(
        context,
        sdbl_hir::SdblCompletionContext::AfterTableAlias {
            alias: "Т".to_string(),
            prefix: "Наим".to_string()
        }
    );
}

#[test]
fn a_field_of_a_cast_result_inside_a_call_keeps_the_cast_target_and_chain() {
    let context = context_at_end(
        "ВЫБРАТЬ ПРЕДСТАВЛЕНИЕ(ВЫРАЗИТЬ(Т.Регистратор КАК Документ.Заказ).Контрагент.",
    );
    assert_eq!(
        context,
        sdbl_hir::SdblCompletionContext::AfterCastExpression {
            mdo_type: bsl_metadata::MdoType::Document,
            object_name: "Заказ".to_string(),
            field_chain: vec!["Контрагент".to_string()],
            prefix: String::new(),
        }
    );
}

// --- Third package: nested joins (a join written inside the joined source, before the
// outer ПО). Sources: ITS pubqlang «Как получить данные из разных таблиц, связанных
// несколькими соединениями», «Внутреннее соединение», «Левое внешнее соединение»,
// «Как получить данные из табличной части некоторого документа», «Как использовать
// данные одного запроса внутри другого запроса»; SELECT mini-spec «Table references»,
// «JOIN clauses»; expressions mini-spec «Recovery contract». Join order in `hir.joins`,
// ranges and token categories are local API contracts. ---

fn table_identity(table: &sdbl_hir::TableRef) -> (String, Vec<String>, String) {
    (
        table.full_name.clone(),
        table.parts.iter().map(|p| p.to_string()).collect(),
        alias_of(table).to_string(),
    )
}

fn identity(full: &str, alias: &str) -> (String, Vec<String>, String) {
    (full.to_string(), full.split('.').map(str::to_string).collect(), alias.to_string())
}

/// Every byte offset at which `text` occurs in `input`, so a token range can be checked
/// against an independently located occurrence.
fn positions(input: &str, text: &str) -> Vec<usize> {
    input.match_indices(text).map(|(i, _)| i).collect()
}

fn token_at(input: &str, start: usize, text: &str, category: TokenCategory) -> bool {
    lower(input).source_map.all_tokens().any(|(info, c)| {
        c == category
            && usize::from(info.range.start()) == start
            && slice(input, info.range) == text
    })
}

#[test]
fn unfinished_nested_join_conditions_keep_every_source_with_its_own_alias() {
    let input = "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Склады КАК Ск ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Валюты КАК Вал ПО Вал. = Ск. ПО Т. = Ск.Ссылка";
    let package = lower(input);
    assert_eq!(package.queries().len(), 1);
    let hir = &package.queries()[0].hir;
    assert_eq!(table_identity(&hir.from[0]), identity("Справочник.Товары", "Т"));
    let mut joined: Vec<_> = hir.joins.iter().map(|j| table_identity(&j.table)).collect();
    joined.sort();
    assert_eq!(
        joined,
        vec![identity("Справочник.Валюты", "Вал"), identity("Справочник.Склады", "Ск")]
    );
}

#[test]
fn tokens_after_an_unfinished_reference_in_the_same_condition_reach_the_source_map() {
    let input = "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Склады КАК Ск ПО Т. = Ск.Ссылка И Ск.Активен = &Флаг";
    let condition_start = input.find("ПО Т.").unwrap();
    let after = |text: &str| -> usize {
        *positions(input, text).iter().find(|&&p| p > condition_start + "ПО Т.".len()).unwrap()
    };
    assert!(token_at(input, after("Ск.Ссылка"), "Ск", TokenCategory::TableAlias));
    assert!(token_at(input, after("Ссылка"), "Ссылка", TokenCategory::FieldName));
    assert!(token_at(input, after("Ск.Активен"), "Ск", TokenCategory::TableAlias));
    assert!(token_at(input, after("Активен"), "Активен", TokenCategory::FieldName));
    let package = lower(input);
    let hir = &package.queries()[0].hir;
    assert_eq!(table_identity(&hir.from[0]), identity("Справочник.Товары", "Т"));
    assert_eq!(table_identity(&hir.joins[0].table), identity("Справочник.Склады", "Ск"));
    // The parameter has no token category; it is kept in the lowered condition with its
    // exact range.
    let condition = hir.joins[0].condition.as_ref().expect("the condition is lowered");
    let mut parameters = Vec::new();
    collect_parameters(condition, &mut parameters);
    assert_eq!(parameters.iter().map(|r| slice(input, *r)).collect::<Vec<_>>(), vec!["&Флаг"]);
}

/// Complete column references of an expression, as `Алиас.Поле`.
fn collect_columns(expr: &ExprHir, out: &mut Vec<String>) {
    match expr {
        ExprHir::ColumnRef { parts, .. } => {
            out.push(parts.iter().map(|p| p.as_str()).collect::<Vec<_>>().join("."))
        }
        ExprHir::BinaryOp { lhs, rhs, .. } => {
            collect_columns(lhs, out);
            collect_columns(rhs, out);
        }
        ExprHir::UnaryOp { expr, .. } => collect_columns(expr, out),
        _ => {}
    }
}

fn collect_parameters(expr: &ExprHir, out: &mut Vec<text_size::TextRange>) {
    match expr {
        ExprHir::Parameter { range, .. } => out.push(*range),
        ExprHir::BinaryOp { lhs, rhs, .. } => {
            collect_parameters(lhs, out);
            collect_parameters(rhs, out);
        }
        ExprHir::UnaryOp { expr, .. } => collect_parameters(expr, out),
        _ => {}
    }
}

/// Checks a package whose second query has nested joins with several unfinished
/// qualified references in both conditions, followed by complete fields and parameters.
fn assert_compound_recovery(second: &str, inner_rest: (&str, &str), outer_rest: (&str, &str)) {
    let first = "ВЫБРАТЬ 1 КАК Номер";
    let input = format!("{first};\n{second}");
    let package = lower(&input);
    assert_eq!(package.queries().len(), 2);
    assert_eq!(slice(&input, package.queries()[0].range), first);
    assert_eq!(slice(&input, package.queries()[1].range), second);
    let hir = &package.queries()[1].hir;
    assert_eq!(table_identity(&hir.from[0]), identity("Справочник.Товары", "Т"));
    let joins: Vec<_> = hir.joins.iter().map(|j| (j.join_type, table_identity(&j.table))).collect();
    assert_eq!(
        joins,
        vec![
            (JoinType::Left, identity("Справочник.Валюты", "Вал")),
            (JoinType::Inner, identity("Справочник.Склады", "Ск")),
        ]
    );
    // The surviving field and parameter of each condition stay with their own join and
    // do not appear in the other one.
    let rests = [inner_rest, outer_rest];
    for (index, join) in hir.joins.iter().enumerate() {
        let condition = join.condition.as_ref().unwrap();
        let mut params = Vec::new();
        collect_parameters(condition, &mut params);
        assert_eq!(
            params.iter().map(|r| slice(&input, *r)).collect::<Vec<_>>(),
            vec![rests[index].1]
        );
        let mut columns = Vec::new();
        collect_columns(condition, &mut columns);
        assert!(
            columns.contains(&rests[index].0.to_string()),
            "{} in join {index}: {columns:?}",
            rests[index].0
        );
        assert!(
            !columns.contains(&rests[1 - index].0.to_string()),
            "{} leaked into join {index}",
            rests[1 - index].0
        );
    }
    // The complete field after the unfinished references reaches the source map at its
    // own place, in both conditions.
    let base = input.find(second).unwrap();
    for (field, _) in [inner_rest, outer_rest] {
        let (alias, name) = field.split_once('.').unwrap();
        let at = base + second.find(field).unwrap();
        assert!(token_at(&input, at, alias, TokenCategory::TableAlias), "{field}");
        assert!(token_at(&input, at + alias.len() + 1, name, TokenCategory::FieldName), "{field}");
    }
}

#[test]
fn several_unfinished_references_in_nested_conditions_keep_the_rest_of_the_package() {
    assert_compound_recovery(
        "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Склады КАК Ск ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Валюты КАК Вал ПО Вал. = Ск.Валюта И Вал. <> &Код ПО Т. <> Ск.Ссылка ИЛИ Т. = &Флаг",
        ("Ск.Валюта", "&Код"),
        ("Ск.Ссылка", "&Флаг"),
    );
}

/// An unfinished reference directly followed by `И` / `ИЛИ` leaves the connective to the
/// condition, which goes on after it.
#[test]
fn unfinished_references_before_a_logical_connective_keep_the_rest_of_the_package() {
    assert_compound_recovery(
        "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Склады КАК Ск ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Валюты КАК Вал ПО Вал. = Ск. И Вал.Код = &Код ПО Т. <> Ск. ИЛИ Ск.Активен = &Флаг",
        ("Вал.Код", "&Код"),
        ("Ск.Активен", "&Флаг"),
    );
}

/// The same with the connective on the next line, as a query is usually laid out.
#[test]
fn unfinished_references_before_a_connective_on_the_next_line_keep_the_rest_of_the_package() {
    assert_compound_recovery(
        "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Склады КАК Ск ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Валюты КАК Вал ПО Вал. = Ск.\n\t\tИ Вал.Код = &Код ПО Т. <> Ск.\n\tИЛИ Ск.Активен = &Флаг",
        ("Вал.Код", "&Код"),
        ("Ск.Активен", "&Флаг"),
    );
}

/// A member spelled like a connective and written against its dot stays a member.
#[test]
fn a_member_spelled_like_a_connective_stays_a_member() {
    let input = "ВЫБРАТЬ Т.И КАК Первое, Т.ИЛИ КАК Второе ИЗ Справочник.Т КАК Т";
    let package = lower(input);
    let fields: Vec<Vec<String>> = package.queries()[0]
        .hir
        .select
        .fields
        .iter()
        .map(|f| match &f.expr {
            ExprHir::ColumnRef { parts, .. } => parts.iter().map(|p| p.to_string()).collect(),
            other => panic!("expected a column reference, got {other:?}"),
        })
        .collect();
    assert_eq!(
        fields,
        vec![vec!["Т".to_string(), "И".to_string()], vec!["Т".to_string(), "ИЛИ".to_string()]]
    );
    assert!(parser::parse_sdbl(input).errors().is_empty());
}

#[test]
fn a_tabular_source_inside_a_nested_query_keeps_its_full_identity() {
    let input = "ВЫБРАТЬ Вл.Номер КАК Номер ИЗ (ВЫБРАТЬ З.Номер КАК Номер, С.Товар КАК Товар, Ц.Цена КАК Цена ИЗ Документ.Заказ КАК З ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ.Состав КАК С ПО С.Ссылка = З.Ссылка ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Ц.Товар = С.Товар) КАК Вл";
    let package = lower(input);
    let inner = &package.queries()[0].hir.from[0].subquery[0];
    assert_eq!(table_identity(&inner.from[0]), identity("Документ.Заказ", "З"));
    let joins: Vec<_> =
        inner.joins.iter().map(|j| (j.join_type, table_identity(&j.table))).collect();
    assert_eq!(
        joins,
        vec![
            (JoinType::Inner, identity("Документ.Заказ.Состав", "С")),
            (JoinType::Left, identity("РегистрСведений.Цены", "Ц")),
        ]
    );
    assert_eq!(
        selected(inner, input),
        vec!["З.Номер КАК Номер", "С.Товар КАК Товар", "Ц.Цена КАК Цена"]
    );
    let lowered: Vec<(Option<String>, Vec<String>)> = inner
        .select
        .fields
        .iter()
        .map(|f| {
            let parts = match &f.expr {
                ExprHir::ColumnRef { parts, .. } => parts.iter().map(|p| p.to_string()).collect(),
                other => panic!("expected a column reference, got {other:?}"),
            };
            (f.alias.as_ref().map(|a| a.to_string()), parts)
        })
        .collect();
    let field = |alias: &str, source: &str, name: &str| {
        (Some(alias.to_string()), vec![source.to_string(), name.to_string()])
    };
    assert_eq!(
        lowered,
        vec![
            field("Номер", "З", "Номер"),
            field("Товар", "С", "Товар"),
            field("Цена", "Ц", "Цена")
        ]
    );
}

#[test]
fn a_nested_join_precedes_its_enclosing_join_with_its_own_condition() {
    let input = "ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Товары КАК Т ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ.Состав КАК С ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Ц.Товар = С.Товар ПО С.Номенклатура = Т.Ссылка";
    let package = lower(input);
    let hir = &package.queries()[0].hir;
    let joins: Vec<_> = hir
        .joins
        .iter()
        .map(|j| {
            (
                j.join_type,
                table_identity(&j.table),
                slice(input, j.condition.as_ref().unwrap().range()),
            )
        })
        .collect();
    assert_eq!(
        joins,
        vec![
            (JoinType::Left, identity("РегистрСведений.Цены", "Ц"), "Ц.Товар = С.Товар"),
            (JoinType::Inner, identity("Документ.Заказ.Состав", "С"), "С.Номенклатура = Т.Ссылка"),
        ]
    );
}
