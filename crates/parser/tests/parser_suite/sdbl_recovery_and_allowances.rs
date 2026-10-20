//! What the SDBL parser does with input the query language does not
//! describe, and with the parts of it that the language describes but
//! the parser used to ignore.
//!
//! Two properties are pinned here. The first is that the syntax tree
//! covers the whole input: nothing the parser refuses may leave without
//! a node and a word. The second is that each tolerance is deliberate —
//! either the language requires the parser to accept it, or it is kept
//! for editor behaviour and named as such.
//!
//! Provenance: `docs/legal/sdbl-clean-room-slice12.md`.

use parser::parse_sdbl;
use syntax::SyntaxKind;

/// The whole input reached the tree and nothing was reported.
fn accepted(input: &str) -> syntax::SyntaxNode {
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    assert_eq!(
        usize::from(root.text_range().len()),
        input.len(),
        "the tree must cover `{input}` completely",
    );
    assert!(!parse.has_errors(), "`{input}` must parse cleanly: {:#?}", parse.errors());
    root
}

/// The whole input reached the tree, and the part the grammar could not
/// place was named in an error.
fn leftover_reported(input: &str, expected: &str) {
    let parse = parse_sdbl(input);
    assert_eq!(
        usize::from(parse.syntax_node().text_range().len()),
        input.len(),
        "the tree must cover `{input}` completely even when it cannot parse it",
    );

    let reported: Vec<_> = parse
        .errors()
        .iter()
        .map(|e| {
            let r = e.range();
            input[usize::from(r.start())..usize::from(r.end())].trim().to_string()
        })
        .collect();
    assert!(
        reported.iter().any(|t| t == expected),
        "expected `{expected}` to be reported for `{input}`; got {reported:?}",
    );
}

fn has_kind(root: &syntax::SyntaxNode, kind: SyntaxKind) -> bool {
    root.descendants().any(|n| n.kind() == kind)
}

// --- The forms the Developer's Reference works through ----------------

#[test]
fn totals_by_hierarchy_worked_example() {
    let root = accepted(
        "ВЫБРАТЬ\n\
         \tДокумент.Номенклатура КАК Номенклатура,\n\
         \tДокумент.Количество КАК Количество\n\
         ИЗ\n\
         \tДокумент.РасходнаяНакладная.Состав КАК Документ\n\
         УПОРЯДОЧИТЬ ПО\n\
         \tДокумент.Номенклатура\n\
         ИТОГИ\n\
         \tСУММА(Количество)\n\
         ПО\n\
         \tНоменклатура ИЕРАРХИЯ",
    );
    assert!(has_kind(&root, SyntaxKind::SDBL_TOTALS_BY));
    assert!(has_kind(&root, SyntaxKind::SDBL_ORDER_CLAUSE));
}

#[test]
fn totals_by_only_hierarchy_worked_example() {
    let root = accepted(
        "ВЫБРАТЬ\n\
         \tДокумент.Номенклатура КАК Номенклатура\n\
         ИЗ\n\
         \tДокумент.РасходнаяНакладная.Состав КАК Документ\n\
         ИТОГИ\n\
         \tСУММА(Количество)\n\
         ПО\n\
         \tНоменклатура ТОЛЬКО ИЕРАРХИЯ",
    );
    assert!(has_kind(&root, SyntaxKind::SDBL_TOTALS_BY));
}

#[test]
fn totals_by_periods_worked_example() {
    let root = accepted(
        "ВЫБРАТЬ\n\
         \tПриходнаяНакладная.Контрагент,\n\
         \tНАЧАЛОПЕРИОДА(ПриходнаяНакладная.Дата, ЧАС) КАК Период,\n\
         \tКОЛИЧЕСТВО(ПриходнаяНакладная.Ссылка) КАК КоличествоПокупок\n\
         ИЗ\n\
         \tДокумент.ПриходнаяНакладная КАК ПриходнаяНакладная\n\
         СГРУППИРОВАТЬ ПО\n\
         \tПриходнаяНакладная.Контрагент,\n\
         \tНАЧАЛОПЕРИОДА(ПриходнаяНакладная.Дата, ЧАС)\n\
         ИТОГИ\n\
         \tСУММА(КоличествоПокупок)\n\
         ПО\n\
         \tПериод ПЕРИОДАМИ(МИНУТА, ДАТАВРЕМЯ(2006,6,28), ДАТАВРЕМЯ(2006,6,28))",
    );
    assert!(has_kind(&root, SyntaxKind::SDBL_TOTALS_BY));
}

#[test]
fn totals_by_overall_worked_example() {
    accepted(
        "ВЫБРАТЬ\n\
         \tДокумент.Номенклатура,\n\
         \tДокумент.Количество КАК Количество\n\
         ИЗ\n\
         \tДокумент.РасходнаяНакладная.Состав КАК Документ\n\
         ИТОГИ\n\
         \tСУММА(Количество)\n\
         ПО\n\
         \tОБЩИЕ",
    );
}

#[test]
fn totals_by_without_an_aggregate_list() {
    // The list may be omitted, in which case it is derived from the
    // selection's aggregate fields.
    accepted(
        "ВЫБРАТЬ\n\
         \tДокумент.Номенклатура КАК Номенклатура,\n\
         \tСУММА(Документ.Количество) КАК Количество\n\
         ИЗ\n\
         \tДокумент.РасходнаяНакладная.Состав КАК Документ\n\
         СГРУППИРОВАТЬ ПО\n\
         \tДокумент.Номенклатура\n\
         ИТОГИ ПО\n\
         \tОБЩИЕ,\n\
         \tНоменклатура",
    );
}

#[test]
fn order_by_descending_worked_example() {
    accepted(
        "ВЫБРАТЬ ПЕРВЫЕ 5\n\
         \tНоменклатура.Наименование,\n\
         \tНоменклатура.ЗакупочнаяЦена\n\
         ИЗ\n\
         \tСправочник.Номенклатура КАК Номенклатура\n\
         УПОРЯДОЧИТЬ ПО\n\
         \tНоменклатура.ЗакупочнаяЦена УБЫВ",
    );
}

// --- The closed lists the rules give ---------------------------------

#[test]
fn every_period_name_is_taken() {
    for period in [
        "СЕКУНДА",
        "МИНУТА",
        "ЧАС",
        "ДЕНЬ",
        "НЕДЕЛЯ",
        "МЕСЯЦ",
        "КВАРТАЛ",
        "ГОД",
        "ДЕКАДА",
        "ПОЛУГОДИЕ",
    ] {
        accepted(&format!("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ({period})"));
    }
}

#[test]
fn periods_takes_none_one_or_two_boundaries() {
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ)");
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ, &Начало)");
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ, &Начало, &Конец)");
    accepted(
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ, ДАТАВРЕМЯ(2006,1,1), ДАТАВРЕМЯ(2006,12,31))",
    );
}

#[test]
fn an_ordering_field_takes_its_four_orderings() {
    for order in ["ВОЗР", "УБЫВ", "ИЕРАРХИЯ", "ИЕРАРХИЯ УБЫВ"] {
        accepted(&format!("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н {order}"));
    }
}

#[test]
fn a_control_point_takes_its_alias() {
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н КАК Группа");
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н Группа");
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н ИЕРАРХИЯ КАК Группа");
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ) КАК Группа");
}

#[test]
fn modifiers_survive_across_a_list_of_control_points() {
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н ИЕРАРХИЯ, П ПЕРИОДАМИ(ДЕНЬ), К КАК Группа");
}

// --- Tolerances kept on purpose ---------------------------------------

#[test]
fn a_word_order_the_rules_do_not_list_is_still_taken() {
    // Not among the four orderings; a plausible slip, and rejecting it
    // would buy nothing.
    accepted("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н УБЫВ ИЕРАРХИЯ");
}

#[test]
fn a_clause_keyword_without_its_body_stays_quiet() {
    // Mid-typing. The mandatory BY has not been reached yet, and saying
    // so on every keystroke would be noise.
    for input in [
        "ВЫБРАТЬ А ИЗ Т СГРУППИРОВАТЬ",
        "ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ",
        "ВЫБРАТЬ А ИЗ Т ИНДЕКСИРОВАТЬ",
        "ВЫБРАТЬ А ИЗ Т ИТОГИ",
        "ВЫБРАТЬ А ИЗ Т ДЛЯ",
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ",
    ] {
        accepted(input);
    }
}

#[test]
fn an_unbalanced_extension_brace_stays_quiet() {
    // A query assembled at runtime can carry half a brace region.
    accepted("ВЫБРАТЬ Т.Поле ИЗ Справочник.Т КАК Т {ГДЕ Т.Поле");
}

#[test]
fn comments_are_trivia() {
    accepted("// только комментарий");
    accepted("ВЫБРАТЬ А // поле\nИЗ Т");
    accepted("ВЫБРАТЬ А ИЗ Т // хвост");
}

#[test]
fn a_bad_field_does_not_cost_the_rest_of_the_list() {
    let parse = parse_sdbl("ВЫБРАТЬ А, Б В Г Д, Е ИЗ Т");
    let root = parse.syntax_node();
    assert_eq!(usize::from(root.text_range().len()), "ВЫБРАТЬ А, Б В Г Д, Е ИЗ Т".len());
    assert!(has_kind(&root, SyntaxKind::SDBL_FROM_CLAUSE), "the FROM clause is still found");
}

// --- Nothing leaves without a word ------------------------------------

#[test]
fn a_misplaced_clause_is_named() {
    leftover_reported("ВЫБРАТЬ А ГДЕ А=1 ИЗ Т", "ИЗ Т");
    leftover_reported("ВЫБРАТЬ А ИЗ Т ПОМЕСТИТЬ ВТ", "ПОМЕСТИТЬ ВТ");
    leftover_reported("ВЫБРАТЬ А ИЗ Т СГРУППИРОВАТЬ ПО Н ГДЕ А=1", "ГДЕ А=1");
}

#[test]
fn a_following_query_is_never_swallowed() {
    // The leftover ends at the separator, so a bad query costs its own
    // parse and nothing else in the package.
    let input = "ВЫБРАТЬ А ИЗ Т ГДЕ А = 1 ФУНК(Х); ВЫБРАТЬ 2 ИЗ У; ВЫБРАТЬ 3 ИЗ В";
    leftover_reported(input, "ФУНК(Х)");
    let root = parse_sdbl(input).syntax_node();
    assert_eq!(
        root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_QUERY).count(),
        3,
        "one bad member of a package must not cost the others their parse",
    );
}

#[test]
fn the_mandatory_parts_of_a_periods_modifier_are_reported() {
    // Every part the rule marks mandatory, and the alternative it is not
    // allowed to combine with. All are reported; none costs the text.
    for (input, what) in [
        ("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ КАК Г", "no opening paren"),
        ("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ()", "no period name"),
        ("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(СМЕНА)", "not one of the ten"),
        ("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ,)", "boundary missing"),
        ("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н ИЕРАРХИЯ ПЕРИОДАМИ(ДЕНЬ)", "exclusive with hierarchy"),
        ("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н ТОЛЬКО", "ONLY without HIERARCHY"),
    ] {
        let parse = parse_sdbl(input);
        assert!(parse.has_errors(), "{what}: `{input}` must be reported");
        assert_eq!(
            usize::from(parse.syntax_node().text_range().len()),
            input.len(),
            "{what}: reporting must not cost the text",
        );
    }
}

#[test]
fn an_explicit_alias_commits_to_a_name() {
    // `КАК` written out means a name follows; its absence is reported.
    let parse = parse_sdbl("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н КАК, П");
    assert!(parse.has_errors());

    // But a name that happens to spell a keyword is still a name, as it is
    // everywhere else an explicit alias is parsed.
    accepted("SELECT A FROM T TOTALS SUM(A) BY N AS Inner");
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н КАК Итоги");
}

#[test]
fn a_stray_comma_costs_only_its_own_field() {
    // A separator with nothing before it is a list with something wrong in
    // it, not an absent list; the clauses after it still parse.
    let input = "ВЫБРАТЬ , А ИЗ Т";
    let parse = parse_sdbl(input);
    assert!(parse.has_errors());
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
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
fn the_leftover_is_a_node_not_just_a_message() {
    let parse = parse_sdbl("ВЫБРАТЬ А ГДЕ А=1 ИЗ Т");
    assert!(
        has_kind(&parse.syntax_node(), SyntaxKind::ERROR),
        "the unparsed remainder must be reachable in the tree, not only in the error list",
    );
}

#[test]
fn coverage_holds_across_the_shapes_the_parser_owns() {
    for input in [
        "ВЫБРАТЬ * ИЗ Справочник.Товары",
        "ВЫБРАТЬ РАЗЛИЧНЫЕ ПЕРВЫЕ 10 Т.Код КАК К ИЗ Справочник.Товары КАК Т",
        "ВЫБРАТЬ А ПОМЕСТИТЬ Врем ИЗ Т; УНИЧТОЖИТЬ Врем",
        "ВЫБРАТЬ А ИЗ Т ЛЕВОЕ СОЕДИНЕНИЕ У ПО Т.А = У.А",
        "ВЫБРАТЬ А ИЗ Т ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Б ИЗ У",
        "ВЫБРАТЬ Т.Номенклатура ИЗ РегистрНакопления.Т.Остатки(&Период, ) КАК Т",
        "ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Товары КАК Т {ГДЕ Т.Поле}",
        "ВЫБРАТЬ А ИЗ Т ГДЕ А В (ВЫБРАТЬ Б ИЗ У)",
        "ВЫБРАТЬ А ИЗ Т АВТОУПОРЯДОЧИВАНИЕ",
        "ВЫБРАТЬ А ИЗ Т ДЛЯ ИЗМЕНЕНИЯ Т",
        "ВЫБРАТЬ А ПОМЕСТИТЬ Врем ИЗ Т ИНДЕКСИРОВАТЬ ПО А",
    ] {
        accepted(input);
    }
}

#[test]
fn a_bad_member_keeps_the_next_query_even_when_it_ate_the_separator() {
    // Several recovery paths report by bumping a token, and the token can be
    // the separator. The package loop must not depend on the separator
    // surviving, or "one bad member costs only itself" holds by luck.
    // `SDBL_QUERY` counts every `ВЫБРАТЬ`, nested ones included; the DROP
    // case is counted by its own node because it has no `ВЫБРАТЬ` of its own.
    for (input, kind, wanted) in [
        ("УНИЧТОЖИТЬ ; ВЫБРАТЬ A ИЗ T", SyntaxKind::SDBL_QUERY, 1),
        ("УНИЧТОЖИТЬ ; ВЫБРАТЬ A ИЗ T", SyntaxKind::SDBL_DROP_QUERY, 1),
        ("ВЫБРАТЬ ПЕРВЫЕ ; ВЫБРАТЬ Б ИЗ У", SyntaxKind::SDBL_QUERY, 2),
        ("ВЫБРАТЬ А ИЗ ; ВЫБРАТЬ Б ИЗ У", SyntaxKind::SDBL_QUERY, 2),
        ("ВЫБРАТЬ (ВЫБРАТЬ ); ВЫБРАТЬ А ИЗ Т", SyntaxKind::SDBL_QUERY, 3),
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(
            usize::from(parse.syntax_node().text_range().len()),
            input.len(),
            "`{input}` must be covered completely",
        );
        let found = parse.syntax_node().descendants().filter(|n| n.kind() == kind).count();
        assert_eq!(found, wanted, "`{input}`: {kind:?} count");
    }
}

#[test]
fn a_period_boundary_is_a_date_or_a_parameter() {
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ, &Н, &К)");
    accepted(
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(МИНУТА, ДАТАВРЕМЯ(2006,6,28), ДАТАВРЕМЯ(2006,6,28))",
    );
    for bad in [
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ, 42)",
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ, \"текст\")",
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ, Поле.Реквизит)",
    ] {
        let parse = parse_sdbl(bad);
        assert!(parse.has_errors(), "`{bad}`: the rule allows only a date or a parameter");
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), bad.len());
    }
}

#[test]
fn only_the_descending_direction_pairs_with_hierarchy() {
    accepted("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н УБЫВ ИЕРАРХИЯ");
    accepted("ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н ИЕРАРХИЯ УБЫВ");
    for bad in [
        "ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н ВОЗР ИЕРАРХИЯ",
        "ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО Н ИЕРАРХИЯ ВОЗР",
    ] {
        assert!(parse_sdbl(bad).has_errors(), "`{bad}` is not one of the four orderings");
        assert_eq!(uncovered_len(bad), 0);
    }
}

#[test]
fn a_missing_hierarchy_does_not_also_report_a_conflict_with_it() {
    // `ТОЛЬКО ПЕРИОДАМИ(…)` is one mistake, not two: the alternative was
    // never taken, so nothing can conflict with it.
    let parse = parse_sdbl("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н ТОЛЬКО ПЕРИОДАМИ(ДЕНЬ)");
    assert_eq!(parse.errors().len(), 1, "got: {:#?}", parse.errors());
}

fn uncovered_len(input: &str) -> usize {
    input.len() - usize::from(parse_sdbl(input).syntax_node().text_range().len())
}

#[test]
fn a_package_says_when_its_members_are_not_separated() {
    // Recognising a query where a separator should be is what keeps the rest
    // of the package parseable; saying nothing about it would turn one bad
    // package into two good queries.
    let run_on = "ВЫБРАТЬ А ИЗ Т ВЫБРАТЬ Б ИЗ У";
    let parse = parse_sdbl(run_on);
    assert!(parse.has_errors(), "a missing separator must be reported");
    assert_eq!(
        parse.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::SDBL_QUERY).count(),
        2,
        "and both queries must still be parsed",
    );

    // A member missing between two separators is reported too.
    assert!(parse_sdbl("ВЫБРАТЬ А ИЗ Т;;ВЫБРАТЬ Б ИЗ У").has_errors());

    // A separator at the end is not a missing member.
    accepted("ВЫБРАТЬ А ИЗ Т;");
    accepted("ВЫБРАТЬ А ИЗ Т ОБЪЕДИНИТЬ ВЫБРАТЬ Б ИЗ У");
}

#[test]
fn a_closing_paren_ends_a_missing_top_count() {
    // Inside a subquery the paren is structure, not a count. Consuming it
    // would cost the subquery its closing paren and the outer query its
    // shape.
    let input = "ВЫБРАТЬ (ВЫБРАТЬ ПЕРВЫЕ)";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    assert!(
        !parse.errors().iter().any(|e| e.message().contains("Ожидалось ')'")),
        "the paren is still there, so nothing may ask for another: {:#?}",
        parse.errors(),
    );
}

#[test]
fn a_separator_survives_the_recovery_that_reports_next_to_it() {
    // Reporting is implemented as taking the offending token, and for a
    // while the offending token could be the separator. Then the package
    // loop either blamed the next query for a separator that was there, or
    // let a recovery keep parsing straight through it.
    let after_drop = parse_sdbl("УНИЧТОЖИТЬ ; ВЫБРАТЬ A ИЗ T");
    assert!(
        !after_drop.errors().iter().any(|e| e.message().contains("разделитель")),
        "the separator is present, so nothing may report it missing: {:#?}",
        after_drop.errors(),
    );
    assert!(parse_sdbl("УНИЧТОЖИТЬ ;; ВЫБРАТЬ A ИЗ T")
        .errors()
        .iter()
        .any(|e| e.message().contains("между разделителями")));

    let after_join = parse_sdbl("ВЫБРАТЬ A ИЗ T СОЕДИНЕНИЕ U ; ВЫБРАТЬ B ИЗ V");
    assert_eq!(
        after_join
            .syntax_node()
            .children()
            .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
            .count(),
        2,
        "a recovery that reports mid-clause must not swallow the next member",
    );
}

#[test]
fn the_drain_does_not_promote_a_nested_query() {
    // A query start inside a paren group belongs to that group. Promoting it
    // would hand the lowerer a subquery dressed as a package member.
    let input = "ВЫБРАТЬ А ИЗ Т 42 (ВЫБРАТЬ Б ИЗ У)";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    assert_eq!(
        parse
            .syntax_node()
            .children()
            .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
            .count(),
        1,
    );
}

#[test]
fn a_separator_ends_the_leftover_at_any_depth() {
    // A separator cannot belong to a paren group in this language, so an
    // unclosed group in the leftover must not hide it.
    let input = "SELECT A FROM T 42 (; SELECT B FROM U";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    assert_eq!(
        parse
            .syntax_node()
            .children()
            .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
            .count(),
        2,
    );
}

#[test]
fn a_query_keyword_inside_a_group_is_not_a_package_member() {
    // Inside parens it is a subquery; inside braces it is extension text.
    // Either way the lowerer must not be handed it as a member of its own.
    for input in [
        "ВЫБРАТЬ Ф(А X ВЫБРАТЬ Б ИЗ У)",
        "SELECT A FROM T 42 {SELECT B FROM U}",
        "ВЫБРАТЬ А ИЗ Т 42 (ВЫБРАТЬ Б ИЗ У)",
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
                .count(),
            1,
            "`{input}`",
        );
    }
}

#[test]
fn an_error_at_a_separator_points_at_the_separator() {
    // Not bumping the separator is only half of it: an error still marked
    // as "the token was taken" is placed on the previous token, so the
    // complaint lands on the word before the gap instead of in it.
    let input = "УНИЧТОЖИТЬ;";
    let parse = parse_sdbl(input);
    let at_semicolon = syntax::TextSize::from(input.find(';').unwrap() as u32);
    assert_eq!(parse.errors()[0].range(), syntax::TextRange::empty(at_semicolon));
}

#[test]
fn nesting_is_counted_per_kind_and_from_the_whole_member() {
    // A closer of the wrong kind does not close the group that is open, and
    // a group closed by the drain counts as closed for what follows. Both
    // fail if nesting is one arithmetic balance accumulated in two places.
    let cases: [(&str, usize); 4] = [
        // The `}` must not cancel the `(`, so the second query is inside it.
        ("SELECT A FROM T 42 ( } SELECT B FROM U", 1),
        ("SELECT F(A X } SELECT B FROM U)", 1),
        // Here the drain closes the call, so what follows is a member again.
        ("ВЫБРАТЬ Ф(А X ВЫБРАТЬ Б ИЗ У) ВЫБРАТЬ В ИЗ W", 2),
        // And a separator still ends the member whatever is open.
        ("SELECT A FROM T 42 (; SELECT B FROM U)", 2),
    ];

    for (input, wanted) in cases {
        let parse = parse_sdbl(input);
        assert_eq!(
            usize::from(parse.syntax_node().text_range().len()),
            input.len(),
            "`{input}` must be covered completely",
        );
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
                .count(),
            wanted,
            "`{input}`",
        );
    }
}

#[test]
fn an_expected_token_missing_at_a_separator_leaves_it_alone() {
    // `expect` is a third path to reporting, next to `error` and
    // `error_custom`, and it has to follow the same rule about separators.
    let parse = parse_sdbl("SELECT A FROM ; SELECT B FROM U");
    assert!(
        !parse.errors().iter().any(|e| e.message().contains("разделитель")),
        "the separator is present: {:#?}",
        parse.errors(),
    );
    assert_eq!(
        parse
            .syntax_node()
            .children()
            .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
            .count(),
        2,
    );
}

#[test]
fn brackets_inside_an_extension_region_say_nothing_about_the_structure() {
    // A brace region is taken verbatim, so whatever brackets it contains
    // belong to its text and not to the package around it.
    for (input, wanted) in [
        ("SELECT A { ) ( } SELECT B", 2usize),
        ("SELECT A FROM T { ( } SELECT B FROM U", 2),
        // The region itself still nests: an unclosed brace holds the rest.
        ("SELECT A FROM T { ( SELECT B FROM U", 1),
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
                .count(),
            wanted,
            "`{input}`",
        );
    }
}

#[test]
fn recovery_stays_linear_on_a_long_malformed_package() {
    // Deciding whether a query keyword is a package member must not cost a
    // rescan of everything before it: a run-on package is exactly the input
    // an editor sees while a query is being written, and quadratic work here
    // is a freeze rather than a wrong answer. The iteration limiter does not
    // catch it, because the scanning happens inside one iteration.
    let run_on = format!("SELECT A FROM T 42 ({}", " SELECT".repeat(20_000));
    let parse = parse_sdbl(&run_on);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), run_on.len());

    let no_separators = "SELECT A ".repeat(20_000);
    let parse = parse_sdbl(&no_separators);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), no_separators.len());
}

#[test]
fn a_group_left_open_by_one_member_does_not_reach_the_next() {
    // A group cannot span a separator. An unclosed brace that outlived its
    // member would go on suppressing the paren count in every member after
    // it, and a depth left standing would let the next member close it and
    // open its own without the total ever moving.
    for (input, wanted) in [
        ("SELECT A FROM T 42 {; SELECT A FROM T 42 ( SELECT B FROM U", 2usize),
        ("SELECT A FROM T 42 (; SELECT A FROM T ) 42 ( SELECT B FROM U", 2),
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
                .count(),
            wanted,
            "`{input}`",
        );
    }
}

#[test]
fn nothing_may_run_past_a_separator() {
    // Reporting is not the only way a rule can swallow the boundary. An
    // extension region reads to its closing brace, and both skip-ahead
    // recoveries used to stop at a separator only while their own local
    // depth was zero. A separator outranks every depth any of them keeps.
    for (input, wanted) in [
        ("SELECT A FROM T {; SELECT B FROM U", 2usize),
        ("SELECT A FROM T {foo; SELECT B FROM U", 2),
        ("SELECT F(A X (; ) SELECT B FROM U", 2),
        ("SELECT A FROM T.V(A X (; ) SELECT B FROM U", 2),
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
                .count(),
            wanted,
            "`{input}`",
        );
    }
}

#[test]
fn a_member_is_not_minted_for_a_token_that_begins_nothing() {
    // After a separator a query is due, but forcing a query rule onto a `)`
    // produces an empty member node, and the lowerer walks those. A clause
    // keyword is different: `ИЗ Т` with no `ВЫБРАТЬ` yet is a query being
    // written, and the field-list slice guarantees it a node.
    for (input, queries) in [
        ("ВЫБРАТЬ А ИЗ Т; ) ВЫБРАТЬ Б ИЗ У", 2usize),
        ("ВЫБРАТЬ А ИЗ Т; 42 ВЫБРАТЬ Б ИЗ У", 2),
        ("FROM Products", 1),
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .descendants()
                .filter(|n| n.kind() == SyntaxKind::SDBL_QUERY)
                .count(),
            queries,
            "`{input}`",
        );
    }

    // A string that is not a query at all gets no query node and does get a
    // complaint.
    let parse = parse_sdbl("ЫЫЫ");
    assert!(parse.has_errors());
    assert_eq!(
        parse.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::SDBL_QUERY).count(),
        0,
    );
}

#[test]
fn dropped_junk_does_not_cancel_the_member_it_precedes() {
    // A token the package cannot use is dropped, not counted as the member
    // that was owed. The incomplete query after it still gets the node the
    // field-list slice promises it, and the complaint is made once per
    // member rather than once per dropped token.
    for (input, queries) in [
        ("SELECT A FROM T; ) FROM Products", 2usize),
        ("SELECT A FROM T; ) ГДЕ А = 1", 2),
        (") FROM Products", 1),
        // Junk with nothing after it owes nothing more.
        ("ВЫБРАТЬ А ИЗ Т; ) 42", 1),
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .descendants()
                .filter(|n| n.kind() == SyntaxKind::SDBL_QUERY)
                .count(),
            queries,
            "`{input}`",
        );
    }
}

#[test]
fn a_member_is_complained_about_once_and_by_one_voice() {
    // Two voices can report a member with no query in it: the query rule,
    // which says so when it runs, and the loop, which speaks for members no
    // rule ever saw. Only one of them may speak per member.
    let counted = |input: &str| {
        parse_sdbl(input)
            .errors()
            .iter()
            .filter(|e| {
                let m = e.message().to_lowercase();
                m.contains("'выбрать' / 'select'") || m.contains("между разделителями")
            })
            .count()
    };

    // Junk, then an incomplete query the rule reports itself.
    assert_eq!(counted("SELECT A FROM T; ) FROM Products"), 1);
    // Junk that reaches a separator: nobody else will speak for it.
    assert_eq!(counted("SELECT A FROM T; ); SELECT B FROM U"), 1);
    // Two members short, two complaints — the state does not leak across
    // the separator between them.
    assert_eq!(counted("SELECT A FROM T; ); 42"), 2);
    // A separator at the end owes nothing.
    assert_eq!(counted("ВЫБРАТЬ А ИЗ Т;"), 0);
    assert_eq!(counted("ВЫБРАТЬ А ИЗ Т;;ВЫБРАТЬ Б ИЗ У"), 1);
}

#[test]
fn a_clause_keyword_after_a_dot_is_a_property_name() {
    // `T.ИЗ` is one fragment being skipped. Treating the word after the dot
    // as the start of something mints a query node where there is no query.
    for input in ["T.FROM", "Т.ИЗ", "Т.ГДЕ", "T.TOTALS"] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .descendants()
                .filter(|n| n.kind() == SyntaxKind::SDBL_QUERY)
                .count(),
            0,
            "`{input}` contains no query",
        );
    }
}

#[test]
fn a_line_break_ends_the_reach_of_a_dot() {
    // `Т.\nИЗ` is a name and then a clause, which is how the expression
    // layer reads it: a space after a dot keeps the qualified name together,
    // a newline does not.
    let broken = parse_sdbl("ВЫБРАТЬ Т.\nИЗ ИЗ Т");
    assert!(broken.has_errors(), "the grammar itself breaks the name at a newline");

    let input = "SELECT A; ) T.\nFROM Products";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    assert_eq!(
        parse.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::SDBL_QUERY).count(),
        2,
        "the incomplete query after the line break still gets its node",
    );
}

#[test]
fn a_dropped_table_name_is_not_the_next_query() {
    // Keywords reach the parser as identifiers, so `УНИЧТОЖИТЬ ВЫБРАТЬ …`
    // would otherwise name a table `ВЫБРАТЬ` and swallow the query.
    let input = "DROP SELECT A FROM T";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    let root = parse.syntax_node();
    assert_eq!(root.children().filter(|n| n.kind() == SyntaxKind::SDBL_DROP_QUERY).count(), 1);
    assert_eq!(root.children().filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY).count(), 1);
    assert!(parse.errors().iter().any(|e| e.message().contains("разделител")));

    // A real name still works.
    accepted("УНИЧТОЖИТЬ ВТ");
}

#[test]
fn a_dot_reaches_across_the_space_after_it() {
    // `Т . ИЗ` is one property access being skipped. Whitespace between the
    // dot and the name does not make the name a beginning, and the
    // expression layer reads it the same way.
    // A line break is where the reach ends, so those inputs belong to the
    // opposite test below.
    for input in ["T . ИЗ", "T. FROM", "Т.\tИЗ"] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .descendants()
                .filter(|n| n.kind() == SyntaxKind::SDBL_QUERY)
                .count(),
            0,
            "`{input}` contains no query",
        );
    }
}

#[test]
fn no_rule_reports_by_taking_the_next_query_away() {
    use syntax::{TextRange, TextSize};

    // A rule deep inside a construct cannot know what encloses it, so the
    // grammar names the boundary once and no rule may take it. Without that,
    // an unfinished CASE reports its missing END by eating the `SELECT` of
    // the next member, and the package loses that member entirely.
    let input = "SELECT CASE WHEN 1 THEN 2 SELECT B FROM U";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    assert_eq!(
        parse.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::SDBL_QUERY).count(),
        2,
    );

    // A missing `ВЫБРАТЬ` is a gap, so it is reported at the gap rather than
    // on the word that follows it.
    let parse = parse_sdbl("FROM Products");
    let gap = parse.errors().iter().find(|e| e.message().contains("SELECT")).expect("reported");
    assert_eq!(gap.range(), TextRange::empty(TextSize::new(0)));
}

#[test]
fn a_dot_with_nothing_before_it_shields_nothing() {
    // The dot rule exists so that `T.ИЗ` is skipped as one name. A dot needs
    // a name in front of it to qualify; without one — at the start of the
    // leftover, or after another dot — it is junk of its own and must not
    // hide what follows.
    // Two of the receivers cannot be told apart by token kind: a parameter
    // arrives whole, so a lone sigil shares its kind, and a paren that
    // closes nothing closes no cast.
    for input in
        [". SELECT A", ".. SELECT A", ". . SELECT A", "+ . SELECT A", "). SELECT A", "&. SELECT A"]
    {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
                .count(),
            1,
            "`{input}` still contains a query",
        );
    }
}

#[test]
fn an_operand_left_unwritten_costs_only_its_own_query() {
    // A query keyword is never a column name. An operand missing in front of
    // one is the ordinary state of a buffer being typed, and the parser used
    // to answer it by reading the next member's `ВЫБРАТЬ` as that operand —
    // which cost the package the whole member behind it.
    for input in [
        "ВЫБРАТЬ А ИЗ Т ГДЕ\nВЫБРАТЬ Б ИЗ У",
        "ВЫБРАТЬ А ИЗ Т ГДЕ А =\nВЫБРАТЬ Б ИЗ У",
        "ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО\nВЫБРАТЬ Б ИЗ У",
        "SELECT A + SELECT B FROM U",
        "SELECT A FROM T WHERE SELECT B FROM U",
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| n.kind() == SyntaxKind::SDBL_SELECT_QUERY)
                .count(),
            2,
            "`{input}` holds two queries: {:#?}",
            parse.errors(),
        );
        assert!(
            parse.errors().iter().any(|e| e.message().contains("разделитель")),
            "`{input}` is missing its separator: {:#?}",
            parse.errors(),
        );
    }

    // Inside a group the same keyword is a subquery, and stays one.
    accepted("ВЫБРАТЬ А ИЗ (ВЫБРАТЬ Б ИЗ У) КАК Вложенный");
    accepted("ВЫБРАТЬ А ИЗ Т ГДЕ А В (ВЫБРАТЬ Б ИЗ У)");
}

#[test]
fn a_name_left_unwritten_costs_only_its_own_query() {
    // Every keyword of this language arrives as an identifier, so a rule
    // that asks only for the kind takes the word beginning the next query
    // for a table, an alias or a type — and the package loses that query.
    for input in [
        "ВЫБРАТЬ А ПОМЕСТИТЬ ВЫБРАТЬ Б ИЗ У",
        "ВЫБРАТЬ А ИЗ Т ДЛЯ ИЗМЕНЕНИЯ ВЫБРАТЬ Б ИЗ У",
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н КАК ВЫБРАТЬ Б ИЗ У",
        "ВЫБРАТЬ А ССЫЛКА ВЫБРАТЬ Б ИЗ У",
        "УНИЧТОЖИТЬ ВЫБРАТЬ Б ИЗ У",
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| matches!(
                    n.kind(),
                    SyntaxKind::SDBL_SELECT_QUERY | SyntaxKind::SDBL_DROP_QUERY
                ))
                .count(),
            2,
            "`{input}` holds two queries: {:#?}",
            parse.errors(),
        );
    }
}

#[test]
fn no_rule_requires_the_word_that_begins_the_next_query() {
    // Kinds are coarser than words: every keyword arrives as an identifier,
    // so a rule asking for one by kind takes the next query's first word and
    // costs the package that query. Naming the sites one at a time missed
    // some twice, so the check lives in `expect` and covers every caller.
    for input in [
        "ВЫБРАТЬ А ИЗ ВЫБРАТЬ Б ИЗ У",
        "ВЫБРАТЬ А УНИЧТОЖИТЬ Т",
        "ВЫБРАТЬ А ИЗ Т ВНУТРЕННЕЕ СОЕДИНЕНИЕ ВЫБРАТЬ Б ИЗ У",
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| matches!(
                    n.kind(),
                    SyntaxKind::SDBL_SELECT_QUERY | SyntaxKind::SDBL_DROP_QUERY
                ))
                .count(),
            2,
            "`{input}` holds two queries: {:#?}",
            parse.errors(),
        );
    }
}

#[test]
fn an_explicit_alias_may_spell_anything() {
    // The boundary is read from the word alone, and a word is not enough:
    // after `КАК` nothing but a name can stand, so the rule standing there
    // says so and the guess steps aside. Field and source aliases were
    // already allowed to spell a keyword before this slice.
    accepted("ВЫБРАТЬ А КАК УНИЧТОЖИТЬ ИЗ Т");
    accepted("ВЫБРАТЬ А ИЗ Т КАК УНИЧТОЖИТЬ");
    accepted("ВЫБРАТЬ А КАК Итоги ИЗ Т");
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н КАК УНИЧТОЖИТЬ");
}

#[test]
fn every_rule_reading_a_qualified_name_reads_it_the_same_way() {
    // Seven rules walk a dotted chain, and each one that spelled the walk
    // out for itself got some of it wrong — the space before the dot, the
    // space after it, or the word beyond it. Naming the sites one at a time
    // left one more every round, so they share the step now.
    for input in [
        "ВЫБРАТЬ А ИЗ Справочник . Товары",
        "ВЫБРАТЬ Т . Поле ИЗ Справочник.Товары КАК Т",
        "ВЫБРАТЬ ВЫРАЗИТЬ(А КАК Справочник . Товары) ИЗ Т",
        "ВЫБРАТЬ А ИЗ Т ГДЕ А ССЫЛКА Справочник . Товары",
        "ВЫБРАТЬ А ИЗ Т ДЛЯ ИЗМЕНЕНИЯ Справочник . Контрагенты",
    ] {
        accepted(input);
    }

    // And none of them may walk into the next query.
    for input in
        ["ВЫБРАТЬ А ИЗ Т.\nУНИЧТОЖИТЬ У", "ВЫБРАТЬ А ИЗ Т ДЛЯ ИЗМЕНЕНИЯ Справочник.УНИЧТОЖИТЬ У"]
    {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| matches!(
                    n.kind(),
                    SyntaxKind::SDBL_SELECT_QUERY | SyntaxKind::SDBL_DROP_QUERY
                ))
                .count(),
            2,
            "`{input}` holds two queries: {:#?}",
            parse.errors(),
        );
    }
}

#[test]
fn a_date_literal_is_a_literal_everywhere_it_may_stand() {
    // The lexer has taken `'ГГГГММДД[ЧЧММСС]'` as one token since the token
    // set was re-derived. Teaching one rule about it and not the rule beside
    // it leaves the same date valid in one position and wrong in the next.
    accepted("ВЫБРАТЬ '20240101'");
    accepted("ВЫБРАТЬ А ИЗ Т ГДЕ А > '20240101235959'");
    accepted("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО П ПЕРИОДАМИ(ДЕНЬ, '20240101')");
}

#[test]
fn an_extension_region_is_opaque_to_every_skip() {
    // `{…}` is text for the platform, not for this query. The parser's own
    // group count already treats it so; a skip that reads the words inside
    // one ends where no boundary is, and takes the rest of the list with it.
    for input in [
        "ВЫБРАТЬ А # {ВЫБРАТЬ Я}, Б ИЗ Т",
        "ВЫБРАТЬ Ф(А { ( }, Б), Ц ИЗ Т",
        "ВЫБРАТЬ * ИЗ Т.В(А Х {ВЫБРАТЬ Я}, Б)",
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert!(
            parse.syntax_node().descendants().any(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE),
            "`{input}` keeps its FROM: {:#?}",
            parse.errors(),
        );
    }
}

#[test]
fn a_position_that_admits_only_a_name_takes_any_word() {
    // Not every word of this language arrives as an identifier — a handful
    // keep a kind of their own — so a rule that says "a name belongs here"
    // and then asks for one kind loses the word. Those words are reserved as
    // aliases, so they are reported, but at themselves and with the source
    // clause behind them intact; a word the platform accepts stays clean.
    accepted("ВЫБРАТЬ А КАК УНИЧТОЖИТЬ ИЗ Т");
    for word in ["НЕ", "И", "ИЛИ", "В", "ИСТИНА"] {
        let input = format!("ВЫБРАТЬ А КАК {word} ИЗ Т");
        let parse = parse_sdbl(&input);
        let reported: Vec<_> = parse
            .errors()
            .iter()
            .map(|e| &input[usize::from(e.range().start())..usize::from(e.range().end())])
            .collect();
        assert_eq!(reported, [word], "`{input}`: {:#?}", parse.errors());
        assert!(has_kind(&parse.syntax_node(), SyntaxKind::SDBL_FROM_CLAUSE), "`{input}`");
    }
}

#[test]
fn a_missing_alias_leaves_the_list_its_delimiter() {
    // A name that is not there is a gap. Consuming the comma that revealed
    // the gap costs the list every item after it, and the clauses too.
    let input = "ВЫБРАТЬ * ИЗ Т КАК, У ГДЕ У.А = 1";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    assert_eq!(
        parse
            .syntax_node()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::SDBL_DATA_SOURCE)
            .count(),
        2,
        "{:#?}",
        parse.errors(),
    );
    assert!(has_kind(&parse.syntax_node(), SyntaxKind::SDBL_WHERE_CLAUSE));
}

#[test]
fn a_clause_keyword_after_a_dot_is_not_a_boundary_for_a_skip() {
    // The leftover reads `Т.ИЗ` as one name; a recovery skip that reads the
    // same two tokens as a name and then a clause hands the enclosing query
    // a clause it never had, and loses everything after it.
    let input = "ВЫБРАТЬ Ф(А Х Т.ИЗ, Б), Ц ИЗ У";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    assert_eq!(
        parse
            .syntax_node()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::SDBL_SELECTED_FIELD)
            .count(),
        2,
        "{:#?}",
        parse.errors(),
    );
}

#[test]
fn an_extension_region_where_the_field_list_should_be() {
    // `ВЫБРАТЬ {Х} ИЗ Т` has no ordinary selection, which is worth saying —
    // but the region and the clause after it are both still there, and
    // handing the brace to the rule that parses a field loses both.
    let input = "ВЫБРАТЬ {Х} ИЗ Т";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    assert!(has_kind(&parse.syntax_node(), SyntaxKind::SDBL_QUERY_EXTENSION));
    assert!(has_kind(&parse.syntax_node(), SyntaxKind::SDBL_FROM_CLAUSE));
}

#[test]
fn a_list_starting_on_its_delimiter_keeps_what_follows() {
    // A leading comma means the first item is missing, not that the comma is
    // the first item. Handing it to the item rule costs the list the field
    // after it, which that rule then reads as the comma's alias.
    let input = "ВЫБРАТЬ , А ИЗ Т";
    let parse = parse_sdbl(input);
    assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    assert!(parse.syntax_node().descendants().any(|n| n.kind() == SyntaxKind::SDBL_COLUMN_REF));
    assert!(parse.syntax_node().descendants().any(|n| n.kind() == SyntaxKind::SDBL_FROM_CLAUSE));
}

#[test]
fn a_dot_reaches_across_the_space_before_it_too() {
    // Whitespace around a dot changes nothing about the name it builds. The
    // parser read the space after the dot and not the space before it, so a
    // valid query drew an error the moment the leftover started reporting.
    accepted("ВЫБРАТЬ А ИЗ Справочник . Товары");
    accepted("ВЫБРАТЬ Т . * ИЗ Справочник.Товары КАК Т");
    accepted("ВЫБРАТЬ А ИЗ Справочник\n\t.Товары");
}

#[test]
fn a_keyword_after_a_dot_is_still_a_name_to_the_leftover() {
    // `Т.В.ВЫБРАТЬ` is one chain of names. A few words carry a kind of their
    // own, and the leftover has to read them as the expression layer does or
    // it cuts the chain in half and invents a query.
    for input in ["Т.В.ВЫБРАТЬ", "Т.И.SELECT", "Т.НЕ.УНИЧТОЖИТЬ"] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
        assert_eq!(
            parse
                .syntax_node()
                .children()
                .filter(|n| matches!(
                    n.kind(),
                    SyntaxKind::SDBL_SELECT_QUERY | SyntaxKind::SDBL_DROP_QUERY
                ))
                .count(),
            0,
            "`{input}` is one name, not a query: {:#?}",
            parse.errors(),
        );
    }
}

#[test]
fn a_rule_that_takes_nothing_still_ends_its_loop() {
    // A list of aggregates asks whether an expression starts here and then
    // parses one. Answering yes for a word no rule may take leaves the loop
    // with nothing consumed and no reason to stop, and the parse never
    // returns — the one failure an editor cannot recover from.
    for input in [
        "ВЫБРАТЬ А ИЗ Т ИТОГИ УНИЧТОЖИТЬ У",
        "ВЫБРАТЬ А ИЗ Т ИТОГИ ВЫБРАТЬ Б ИЗ У",
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А), ВЫБРАТЬ Б ИЗ У",
    ] {
        let parse = parse_sdbl(input);
        assert_eq!(usize::from(parse.syntax_node().text_range().len()), input.len());
    }
}
