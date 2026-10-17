//! What error recovery does to the token an enclosing rule was waiting for.
//!
//! A rule deep inside a construct reports an error by consuming the token it
//! tripped over. When that token is the one closing an enclosing block, the
//! enclosing rule never sees it, runs to the end of the file, and reports the
//! closer as missing — while the closer is right there in the text.
//!
//! A block therefore states the words that close it while it parses what it
//! encloses, and a rule that trips over one of them reports the trip and
//! leaves the word alone. What these tests hold to is that the closer stays
//! in the tree where the text put it, and that the count of messages does not
//! grow with the depth of the nesting.

use syntax::SyntaxKind;

fn error_ranges(input: &str) -> Vec<syntax::TextRange> {
    parser::parse(input).errors().iter().map(|error| error.range()).collect()
}

/// The texts of every `ERROR` node, trimmed.
fn error_node_texts(input: &str, bsl: bool) -> Vec<String> {
    let parse = if bsl { parser::parse(input) } else { parser::parse_sdbl(input) };
    parse
        .syntax_node()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::ERROR)
        .map(|n| n.text().to_string().trim().to_string())
        .collect()
}

fn messages(input: &str, bsl: bool) -> Vec<String> {
    let parse = if bsl { parser::parse(input) } else { parser::parse_sdbl(input) };
    parse.errors().iter().map(|e| e.message().to_string()).collect()
}

fn covers(input: &str, bsl: bool) -> bool {
    let parse = if bsl { parser::parse(input) } else { parser::parse_sdbl(input) };
    usize::from(parse.syntax_node().text_range().len()) == input.len()
}

fn swallowed(input: &str, bsl: bool, closer: &str) -> bool {
    error_node_texts(input, bsl).iter().any(|t| t.contains(closer))
}

fn claims_missing(input: &str, bsl: bool, closer: &str) -> bool {
    messages(input, bsl).iter().any(|m| m.contains(closer) && m.contains("конец файла"))
}

#[test]
fn an_unknown_end_if_is_the_only_error_and_owns_its_token() {
    let input = "Процедура П()\nЕсли Истина Тогда\nКонецЕслли;\nКонецПроцедуры";
    let typo = "КонецЕслли";
    let start = input.find(typo).expect("typo") as u32;

    assert_eq!(
        error_ranges(input),
        vec![syntax::TextRange::new(start.into(), (start + typo.len() as u32).into())]
    );
}

#[test]
fn unknown_statement_recovery_closes_only_the_nearest_block() {
    let input = "Процедура П()\nЕсли Истина Тогда\nПока Истина Цикл\nКонецЦиклаа;\nКонецПроцедуры";
    let errors = parser::parse(input);
    let messages: Vec<_> = errors.errors().iter().map(|error| error.message()).collect();

    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages.iter().any(|message| message.contains("вызов или присваивание")));
    assert!(
        messages.iter().any(|message| message.contains("КонецЕсли")),
        "the loop recovery must not close its enclosing `Если`: {messages:?}"
    );
    assert!(
        !messages.iter().any(|message| message.contains("КонецЦикла")),
        "the nearest marked loop is implicitly closed: {messages:?}"
    );
}

#[test]
fn unknown_statement_recovery_preserves_an_earlier_error() {
    let input = "Процедура П()\nЕсли Истина Тогда\nА = ;\nКонецЕслли;\nКонецПроцедуры";
    let parse = parser::parse(input);
    let messages: Vec<_> = parse.errors().iter().map(|error| error.message()).collect();

    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        messages.iter().any(|message| message.contains("Неожиданный токен ';'")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|message| message.contains("вызов или присваивание")),
        "{messages:?}"
    );
}

/// Посторонний оператор — не попытка закрыть блок, и блок после него остаётся незакрытым.
#[test]
fn a_stray_statement_does_not_close_the_block_around_it() {
    for (input, closer) in [
        ("Процедура П()\nЕсли Истина Тогда\nHHH;\nКонецПроцедуры", "КонецЕсли"),
        ("Процедура П()\nПока Истина Цикл\nHHH;\nКонецПроцедуры", "КонецЦикла"),
        ("Процедура П()\nПопытка\nHHH;\nКонецПроцедуры", "Исключение"),
    ] {
        let messages = messages(input, true);
        assert!(
            messages.iter().any(|message| message.contains(closer)),
            "{closer} потерян: {messages:?}"
        );
    }
}

/// Опечатка в разделителе ветки конструкцию не закрывает: `Иначее` — не `КонецЕсли`.
#[test]
fn a_typo_of_a_branch_separator_does_not_close_the_if() {
    let input = "Процедура П()\nЕсли Истина Тогда\nИначее;\nКонецПроцедуры";
    let messages = messages(input, true);

    assert!(
        messages.iter().any(|message| message.contains("КонецЕсли")),
        "опечатка в `Иначе` скрыла пропажу закрывателя: {messages:?}"
    );
}

/// Английское написание закрывателя портится опечаткой так же, как русское.
#[test]
fn an_english_closer_typo_is_the_only_error() {
    let input = "Procedure P()\nIf True Then\nEndIff;\nEndProcedure";
    let messages = messages(input, true);

    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("вызов или присваивание"), "{messages:?}");
}

/// Опечатка в `КонецПопытки` до ветки `Исключение`: список тела `Попытки`
/// кончается на `Исключение`, и опечатка закрывателя попадает в него. О пропаже
/// `КонецПопытки` он обязан помнить сам — иначе финальное ожидание повторяет
/// уже сказанное (github#88).
#[test]
fn a_typo_of_the_try_closer_before_the_except_clause_is_reported_once() {
    let input = "Процедура П()\nПопытка\nКонецПопыткии;\nКонецПроцедуры";
    let messages = messages(input, true);

    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        messages.iter().any(|message| message.contains("Исключение")),
        "пропавшая ветка обязана быть названа: {messages:?}"
    );
    assert!(
        !messages.iter().any(|message| message.contains("КонецПопытки")),
        "об опечатке сообщили дважды: {messages:?}"
    );
}

/// Тот же вход, но с веткой `Исключение`: опечатка попадает во второй список и
/// гасится там. Контроль ради — у этого входа ответ не меняется.
#[test]
fn a_typo_of_the_try_closer_after_the_except_clause_names_no_closer() {
    let input = "Процедура П()\nПопытка\nИсключение\nКонецПопыткии;\nКонецПроцедуры";
    let messages = messages(input, true);

    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("вызов или присваивание"), "{messages:?}");
}

/// Обратное направление: опечатка в теле глушит финальное ожидание только того
/// закрывателя, на который она похожа. Честно пропавший `КонецПопытки` и
/// опечатка `Исключение` ничего лишнего не глушат.
#[test]
fn only_the_closer_the_typo_resembled_is_suppressed() {
    // `Исключение` на месте, `КонецПопытки` честно пропал: молчать нельзя.
    let missing = messages("Процедура П()\nПопытка\nИсключение\nКонецПроцедуры", true);
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert!(missing[0].contains("КонецПопытки"), "{missing:?}");

    // Опечатка `Исключение`: о ней сообщают как о неизвестном операторе, а
    // пропажу `КонецПопытки` финальное ожидание обязано назвать.
    let except_typo = messages("Процедура П()\nПопытка\nИсключениее;\nКонецПроцедуры", true);
    assert_eq!(except_typo.len(), 2, "{except_typo:?}");
    assert!(
        except_typo.iter().any(|message| message.contains("вызов или присваивание")),
        "{except_typo:?}"
    );
    assert!(
        except_typo.iter().any(|message| message.contains("КонецПопытки")),
        "опечатка `Исключение` не отменяет пропажу `КонецПопытки`: {except_typo:?}"
    );
    assert!(
        !except_typo.iter().any(|message| message.contains("Исключение")),
        "о пропаже `Исключение` сообщили бы второй раз: {except_typo:?}"
    );
}

/// Обрыв файла — не вторая ошибка, а единственная: подавлять её нечем.
#[test]
fn a_truncated_block_is_reported_even_after_a_recovery() {
    for (input, closer) in [
        ("Процедура П()\nА;\n", "КонецПроцедуры"),
        ("Процедура П()\nПока Истина Цикл\nА;\n", "КонецЦикла"),
    ] {
        let messages = messages(input, true);
        assert!(
            messages.iter().any(|message| message.contains(closer)),
            "обрыв на {closer} потерян: {messages:?}"
        );
    }
}

#[test]
fn a_genuinely_missing_end_if_is_still_reported() {
    let input = "Процедура П()\nЕсли Истина Тогда\nА = 1;\nКонецПроцедуры";
    let messages = messages(input, true);

    assert!(messages.iter().any(|message| message.contains("КонецЕсли")), "{messages:?}");
}

#[test]
fn parsing_continues_after_an_unknown_statement() {
    let input = "Процедура П()\nКонецЕслли;\nА = 1;\nКонецПроцедуры";
    let parse = parser::parse(input);

    assert_eq!(parse.errors().len(), 1, "{:?}", parse.errors());
    assert_eq!(
        parse
            .syntax_node()
            .descendants()
            .filter(|node| node.kind() == SyntaxKind::ASSIGN_STMT)
            .count(),
        1
    );
}

#[test]
fn valid_identifier_statements_do_not_enter_unknown_statement_recovery() {
    let input = "Процедура П()\nА = Б + В;\nФ();\nОбъект.Метод();\nКонецПроцедуры";
    let parse = parser::parse(input);

    assert!(!parse.has_errors(), "{:?}", parse.errors());
}

// --- BSL: a block closer is not a recovery's to take ---------------------

/// Nothing consumed the closer, and nothing claimed it was missing.
fn closer_survives(input: &str, closer: &str) {
    assert!(covers(input, true), "`{input}`");
    assert!(
        !swallowed(input, true, closer),
        "`{closer}` was consumed: {:?}",
        error_node_texts(input, true)
    );
    assert!(
        !claims_missing(input, true, closer),
        "`{closer}` was reported missing while present: {:?}",
        messages(input, true)
    );
}

#[test]
fn an_unclosed_call_leaves_a_procedure_its_closer() {
    let input = "Процедура П()\nФ(\nКонецПроцедуры";

    closer_survives(input, "КонецПроцедуры");

    // Two messages about the one gap, both standing at it. One is the
    // argument that was never written, the other the paren that never closed.
    assert_eq!(messages(input, true).len(), 2, "{:?}", messages(input, true));
}

#[test]
fn an_unfinished_assignment_leaves_both_closers() {
    let input = "Процедура П()\nЕсли Истина Тогда\nА =\nКонецЕсли;\nКонецПроцедуры";

    for closer in ["КонецЕсли", "КонецПроцедуры"] {
        closer_survives(input, closer);
    }

    // The missing right-hand side, and nothing else.
    assert_eq!(messages(input, true).len(), 1, "{:?}", messages(input, true));
}

#[test]
fn the_count_of_messages_does_not_grow_with_the_nesting() {
    // Three blocks stand between the typo and the end of the file. Each used
    // to lose its closer and be reported unclosed, so the false reports came
    // one per level; the depth now costs nothing.
    let input =
        "Процедура П()\n\tПопытка\n\t\tА = Ф(\n\tИсключение\n\tКонецПопытки;\nКонецПроцедуры";

    for closer in ["Исключение", "КонецПопытки", "КонецПроцедуры"]
    {
        closer_survives(input, closer);
    }

    assert_eq!(messages(input, true).len(), 2, "{:?}", messages(input, true));
}

#[test]
fn a_loop_closer_stays_the_loop_closer() {
    let input = "Процедура П()\n\tПока Истина Цикл\n\t\tА = Ф(\n\tКонецЦикла;\nКонецПроцедуры";

    for closer in ["КонецЦикла", "КонецПроцедуры"] {
        closer_survives(input, closer);
    }

    assert_eq!(messages(input, true).len(), 2, "{:?}", messages(input, true));
}

#[test]
fn a_statement_list_ends_at_a_closer_further_out() {
    // A boundary is a promise that some rule will not advance, and a list
    // waiting only for its own terminator waits for a token nothing will
    // reach. These inputs each end a block without its own closer, and the
    // parse has to finish rather than spin.
    for input in [
        "Процедура П()\n\tПока Истина Цикл\n\t\tА = 1;\nКонецПроцедуры",
        "Процедура П()\n\tЕсли Истина Тогда\n\t\tА = 1;\nКонецПроцедуры",
        "Процедура П()\n\tПопытка\n\t\tА = 1;\nКонецПроцедуры",
        "Процедура П()\n\tДля Каждого Э Из К Цикл\n\t\tА = 1;\nКонецПроцедуры",
    ] {
        assert!(covers(input, true), "`{input}`");
        closer_survives(input, "КонецПроцедуры");
    }
}

#[test]
fn what_the_boundary_costs_when_the_closer_is_not_stranded() {
    // The same rule cuts the other way. Here `Иначе` is a stray word, not a
    // closer anyone is waiting for in the text — the loop's `КонецЦикла` is
    // right there — but an enclosing `Если` is open, so the list ends and the
    // loop reports a closer that is present. Two messages where one would do,
    // and the first of them false.
    //
    // Telling the two cases apart needs to know whether the block's own
    // closer appears later, which is the whole rest of the input. The trade
    // was made the other way on measurement: this shape costs one message on
    // one module in a production configuration of 21 114, and the shape it
    // replaces costs one false message per level of nesting on every
    // half-written block.
    let input =
        "Процедура П()\n\tЕсли А Тогда\n\t\tДля Каждого Э Из К Цикл\n\t\t\tИначе\n\t\tКонецЦикла;\n\tКонецЕсли;\nКонецПроцедуры";

    assert!(covers(input, true));
    assert_eq!(messages(input, true).len(), 2, "{:?}", messages(input, true));
    assert!(
        messages(input, true).iter().any(|m| m.contains("КонецЦикла")),
        "{:?}",
        messages(input, true)
    );
}

#[test]
fn a_region_crossing_a_block_does_not_hang() {
    // A `#Если` may open inside `Если` and the `ИначеЕсли` closing that `Если`
    // may stand inside the region. The region's content must end there: rules
    // inside it will not consume an enclosing closer, so a region waiting only
    // for `#КонецЕсли` waits for a token nothing reaches.
    let input = "Процедура П()\n\tЕсли А Тогда\n#Если Клиент Тогда\n\tИначеЕсли Б Тогда\n#КонецЕсли\n\tКонецЕсли;\nКонецПроцедуры";

    assert!(covers(input, true));
    closer_survives(input, "КонецПроцедуры");
}

#[test]
fn a_header_leaves_the_word_that_ends_it() {
    // A header waits for the word ending it as surely as a block waits for
    // the word closing it, and a condition never written leaves the parser
    // standing on that word. Consuming it produced a second, false report
    // that the word was missing.
    for (input, separator) in [
        ("Процедура П()\nЕсли Тогда\nКонецЕсли;\nКонецПроцедуры", "Тогда"),
        ("Процедура П()\nЕсли А Тогда\nИначеЕсли Тогда\nКонецЕсли;\nКонецПроцедуры", "Тогда"),
        ("Процедура П()\nПока Цикл\nКонецЦикла;\nКонецПроцедуры", "Цикл"),
        ("Процедура П()\nДля Каждого Э Из Цикл\nКонецЦикла;\nКонецПроцедуры", "Цикл"),
        ("Процедура П()\nДля А = По 10 Цикл\nКонецЦикла;\nКонецПроцедуры", "По"),
    ] {
        assert!(covers(input, true), "`{input}`");
        assert!(
            !swallowed(input, true, separator),
            "`{separator}` was consumed: {:?}",
            error_node_texts(input, true)
        );
        assert_eq!(messages(input, true).len(), 1, "`{input}`: {:?}", messages(input, true));
    }
}

#[test]
fn a_bracketed_list_keeps_its_punctuation() {
    // A comma is how a list reaches its next item and a paren is how it ends,
    // so a rule tripping over one inside an item must leave it. A parameter
    // with no default value used to consume the `)` and the list then reported
    // it missing at the closer of the procedure.
    let input = "Процедура П(А = )\nКонецПроцедуры";
    assert!(covers(input, true));
    assert!(!swallowed(input, true, ")"), "{:?}", error_node_texts(input, true));
    assert_eq!(messages(input, true).len(), 1, "{:?}", messages(input, true));
}

#[test]
fn an_annotation_keeps_the_declaration_it_precedes() {
    // The declaration is what the annotation was attached to, and an
    // unclosed parameter list inside the annotation used to consume the whole
    // of it: four messages and no definition in the tree at all.
    let input = "&Перед(1\nПроцедура П()\nКонецПроцедуры";
    let parse = parser::parse(input);

    assert!(covers(input, true));
    assert!(!swallowed(input, true, "Процедура"), "{:?}", error_node_texts(input, true));
    assert!(
        parse.syntax_node().descendants().any(|n| n.kind() == SyntaxKind::PROCEDURE_DEF),
        "the definition survives: {:#?}",
        parse.errors()
    );
    assert_eq!(messages(input, true).len(), 1, "{:?}", messages(input, true));
}

#[test]
fn a_region_header_keeps_its_then() {
    // The same header rule as `Если`, in the conditional region.
    let input = "#Если Тогда\n#КонецЕсли";
    assert!(covers(input, true));
    assert!(!swallowed(input, true, "Тогда"), "{:?}", error_node_texts(input, true));
    assert_eq!(messages(input, true).len(), 1, "{:?}", messages(input, true));
}

#[test]
fn a_header_keeps_its_last_word_through_the_separator_before_it() {
    // A scope has to outlive the word it protects. `Для А = 1 Цикл` has no
    // `По`, and the expect that says so ran outside the scope keeping `Цикл`,
    // so it took the `Цикл` and the header then reported that same word
    // missing.
    for input in [
        "Процедура П()\nДля А = 1 Цикл\nКонецЦикла;\nКонецПроцедуры",
        "Процедура П()\nДля Каждого Э Цикл\nКонецЦикла;\nКонецПроцедуры",
    ] {
        assert!(covers(input, true), "`{input}`");
        assert!(!swallowed(input, true, "Цикл"), "{:?}", error_node_texts(input, true));
        closer_survives(input, "КонецЦикла");
    }
}

#[test]
fn a_region_that_crosses_a_block_keeps_its_own_closer_only_by_luck() {
    // The scope stack holds constructs that nest. A conditional region and a
    // statement block may cross instead, and then the region returns on a word
    // that is not its own, its scope is dropped, and the `#КонецЕсли` that
    // does belong to it is no longer protected from anything.
    //
    // Pinned as it behaves, which is as it behaved before any of this: the
    // mechanism does not make it worse and cannot make it better. Expressing
    // a scope that outlives the rule that opened it is what would fix it.
    let input = "Процедура П()\nЕсли А Тогда\n#Если Клиент Тогда\n\tИначеЕсли Б Тогда\n#КонецЕсли\n\tКонецЕсли;\nКонецПроцедуры";

    assert!(covers(input, true));
    assert!(swallowed(input, true, "#КонецЕсли"), "{:?}", error_node_texts(input, true));
    assert_eq!(messages(input, true).len(), 2, "{:?}", messages(input, true));
}

#[test]
fn a_call_keeps_its_closing_paren() {
    // The same rule as a parameter list, in the constructs that had not been
    // given it: an argument list and a parenthesised expression.
    for input in
        ["Процедура П()\nФ(А = )\nКонецПроцедуры", "Процедура П()\nА = (Б = )\nКонецПроцедуры"]
    {
        assert!(covers(input, true), "`{input}`");
        assert!(!swallowed(input, true, ")"), "{:?}", error_node_texts(input, true));
    }
}

#[test]
fn an_annotation_keeps_the_next_link_of_its_chain() {
    // A chain may hold more than one annotation, and a folding marker may sit
    // between them, so the next link is awaited exactly as the declaration is.
    let input = "&Перед(1\n&НаКлиенте\nПроцедура П()\nКонецПроцедуры";
    let parse = parser::parse(input);

    assert!(covers(input, true));
    assert!(!swallowed(input, true, "&НаКлиенте"), "{:?}", error_node_texts(input, true));
    assert!(
        parse.syntax_node().descendants().any(|n| n.kind() == SyntaxKind::COMPILER_DIRECTIVE),
        "the directive survives: {:#?}",
        parse.errors()
    );
}

#[test]
fn a_separator_stops_being_awaited_once_it_is_taken() {
    // `По` is awaited until it is consumed and not after. Keeping it a
    // boundary past its own position leaves a repeated one standing, and the
    // expect for `Цикл` then takes the real `Цикл` instead of it.
    let input = "Процедура П()\nДля А = 1 По По Цикл\nКонецЦикла;\nКонецПроцедуры";

    assert!(covers(input, true));
    assert!(!swallowed(input, true, "Цикл"), "{:?}", error_node_texts(input, true));
    assert_eq!(messages(input, true).len(), 1, "{:?}", messages(input, true));
}

#[test]
fn a_word_inside_a_group_is_not_the_separator_the_header_awaits() {
    // Pinned as it behaves, and as it behaved before any of this. A boundary
    // holds at every depth inside its scope, so a `Тогда` inside parens counts
    // as the one the header is waiting for, and the real one after the group
    // is then taken as an error. Limiting a boundary to the depth it was
    // declared at would fix these two and break the case the whole mechanism
    // exists for — `КонецПроцедуры` reached with a paren still open.
    for input in
        ["#Если (Тогда) Тогда\n#КонецЕсли", "&Перед(Процедура)\nПроцедура П()\nКонецПроцедуры"]
    {
        assert!(covers(input, true), "`{input}`");
        assert_eq!(messages(input, true).len(), 4, "`{input}`: {:?}", messages(input, true));
    }
}

#[test]
fn every_bracketed_construct_keeps_its_punctuation() {
    // Enumerated from the grammar rather than from what a review happened to
    // find: every construct that owns a bracket or a comma says so. The
    // ternary was the worst of them — a missing first operand consumed the
    // comma, both remaining operands and the closing paren, six messages for
    // one gap.
    for input in [
        "Процедура П(А = )\nКонецПроцедуры",
        "Процедура П()\n\tФ(А = );\nКонецПроцедуры",
        "Процедура П()\n\tА = Б[ ];\nКонецПроцедуры",
        "Процедура П()\n\tА = ?(, 1, 2);\nКонецПроцедуры",
        "Процедура П()\n\tВызватьИсключение(, 1);\nКонецПроцедуры",
        "Процедура П()\n\tДобавитьОбработчик , П;\nКонецПроцедуры",
        "Процедура П()\n\tУдалитьОбработчик , П;\nКонецПроцедуры",
        "&Перед(1\nПроцедура П()\nКонецПроцедуры",
    ] {
        assert!(covers(input, true), "`{input}`");

        let eaten: Vec<String> =
            error_node_texts(input, true).into_iter().filter(|t| !t.is_empty()).collect();
        assert!(eaten.is_empty(), "`{input}` consumed {eaten:?}");
        assert!(messages(input, true).len() <= 1, "`{input}`: {:?}", messages(input, true));
    }
}

#[test]
fn a_construct_claims_only_the_punctuation_it_consumes() {
    // The mirror of the test above, and the price of stating the class with
    // one shared predicate: a construct that also claimed its neighbour's
    // separator left behind a token no rule would take, and then spent its
    // own closer on it — three messages for one stray bracket.
    for input in [
        "Процедура П()\n\tА = Б[,];\nКонецПроцедуры",
        "Процедура П()\n\tА = Б[)];\nКонецПроцедуры",
        "Процедура П()\n\tА = (]);\nКонецПроцедуры",
        "Процедура П()\n\tФ(]);\nКонецПроцедуры",
        "Процедура П()\n\tА = ?(],1,2);\nКонецПроцедуры",
        "Процедура П()\n\tВызватьИсключение(]);\nКонецПроцедуры",
    ] {
        assert!(covers(input, true), "`{input}`");
        assert_eq!(messages(input, true).len(), 1, "`{input}`: {:?}", messages(input, true));
    }
}

fn clause_count_bsl(input: &str, kind: SyntaxKind) -> usize {
    parser::parse(input).syntax_node().descendants().filter(|n| n.kind() == kind).count()
}

/// Every malformed shape a ternary can take, what each costs, and whether the
/// call enclosing it still keeps the argument written after it.
///
/// The ternary is the only bracketed rule whose opening paren is an `expect`
/// rather than a `bump`, so it is the only one that could be parsing operands
/// with no group open. Three attempts to say which punctuation it awaits in
/// that state each fixed one column here and broke another, and the last of
/// them could not be stated truthfully at all: a comma after `?` is not the
/// ternary's evidence of anything, because an enclosing list writes the same
/// comma and a successful `expect` never asks whose it is.
///
/// So the rule does not read a group it does not have. The table is here whole
/// because a later shape must not be handed over one at a time.
const TERNARY_SHAPES: &[(&str, usize, bool)] = &[
    ("?(Истина, 1, 2)", 0, true),
    ("?(, 1, 2)", 1, true),
    ("?(],1,2)", 1, true),
    ("?(Истина, , 2)", 1, true),
    ("?(Истина, 1, )", 1, true),
    ("?(Истина 1, 2)", 2, true),
    // With no paren of its own the ternary is just the `?`, and every shape
    // below hands the rest of its text to whoever encloses it — which is why
    // the call no longer sees `5` as its own argument: the shape's punctuation
    // became the call's first.
    ("?, 1, 2)", 3, false),
    ("?, 1, )", 3, false),
    ("?, , )", 3, false),
    ("?, ), 2)", 4, false),
    ("?), 1, 2)", 4, false),
    ("?)(1, 2, 3)", 1, false),
    ("?)", 1, false),
    ("?", 1, true),
    // Giving up must not spend the token either, where something else will
    // use it: an operator carries on the expression around the `?`, and no
    // rule declares an operator as a boundary because a rule that reaches one
    // consumes it and loops.
    ("? + 9", 1, true),
    ("? . А", 1, true),
];

#[test]
fn a_ternary_does_not_read_a_group_it_does_not_have() {
    for (shape, expected, _) in TERNARY_SHAPES {
        let input = format!("Процедура П()\n\tА = {shape};\nКонецПроцедуры");
        assert!(covers(&input, true), "`{shape}`");
        assert_eq!(clause_count_bsl(&input, SyntaxKind::TERNARY_EXPR), 1, "`{shape}`");
        assert_eq!(
            messages(&input, true).len(),
            *expected,
            "`{shape}`: {:?}",
            messages(&input, true)
        );
    }
}

#[test]
fn a_ternary_never_takes_the_argument_after_it() {
    // The assertion that matters is the negative one. Asking only whether `5`
    // still has an `ARG_LIST` ancestor proves nothing: a ternary that ate the
    // call's comma and made `5` its own operand sits inside that same argument
    // list, so the check passes while the tree is wrong.
    for (shape, _, call_keeps_last_arg) in TERNARY_SHAPES {
        let input = format!("Процедура П()\n\tФ({shape}, 5);\nКонецПроцедуры");
        assert!(covers(&input, true), "`{shape}`");

        let root = parser::parse(&input).syntax_node();
        let last_arg = root
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.text() == "5")
            .unwrap_or_else(|| panic!("`{shape}`: the last argument is not in the tree"));

        assert!(
            !last_arg.parent_ancestors().any(|n| n.kind() == SyntaxKind::TERNARY_EXPR),
            "`{shape}`: the ternary took the argument after it"
        );
        assert_eq!(
            last_arg.parent_ancestors().any(|n| n.kind() == SyntaxKind::ARG_LIST),
            *call_keeps_last_arg,
            "`{shape}`: the call kept its last argument"
        );
    }
}

#[test]
fn a_ternary_that_gives_up_leaves_the_bracket_holding_it() {
    // The index owns `]` and declares it, but an operator standing where the
    // ternary's paren should be is declared by nobody: consuming it as the
    // report cost the index its bracket two tokens later.
    for (shape, _, _) in TERNARY_SHAPES {
        if !shape.contains('+') && !shape.contains('.') {
            continue;
        }
        let input = format!("Процедура П()\n\tА = Б[{shape}];\nКонецПроцедуры");
        assert!(covers(&input, true), "`{shape}`");
        assert!(!swallowed(&input, true, "]"), "`{shape}`: {:?}", error_node_texts(&input, true));
        assert_eq!(messages(&input, true).len(), 1, "`{shape}`: {:?}", messages(&input, true));
    }
}

#[test]
fn a_broken_name_is_not_blamed_on_a_keyword_that_is_not_there() {
    // Two reasons the name broke off, and naming the wrong one is its own
    // defect: a word that cannot be part of it is a keyword, while nothing at
    // all is a name the text does not hold.
    for (input, keyword) in [
        ("SELECT A FROM Catalog.BY", true),
        ("SELECT A FROM Catalog.", false),
        ("SELECT A FROM Catalog.1", false),
    ] {
        let said = messages(input, false).iter().any(|m| m.contains("ключевое слово"));
        assert_eq!(said, keyword, "`{input}`: {:?}", messages(input, false));
    }
}

// --- SDBL: a clause keyword taken by a recovery inside the clause ---------

fn clause_count(input: &str, kind: SyntaxKind) -> usize {
    parser::parse_sdbl(input).syntax_node().descendants().filter(|n| n.kind() == kind).count()
}

#[test]
fn a_recovery_inside_a_query_leaves_the_clause_that_follows_it() {
    // Each of these has one defect inside one part of the query, and the
    // clause after it used to be consumed by the recovery and produce no
    // node — so a consumer could not see a source clause that is plainly
    // written.
    let cases: &[(&str, SyntaxKind)] = &[
        ("ВЫБРАТЬ ВЫБОР КОГДА А ТОГДА Б ИЗ Т", SyntaxKind::SDBL_FROM_CLAUSE),
        ("ВЫБРАТЬ () ИЗ Т", SyntaxKind::SDBL_FROM_CLAUSE),
        ("ВЫБРАТЬ А В (1 ИЗ Т", SyntaxKind::SDBL_FROM_CLAUSE),
        ("ВЫБРАТЬ * ИЗ Т ГДЕ А В () УПОРЯДОЧИТЬ ПО Б", SyntaxKind::SDBL_ORDER_CLAUSE),
        ("ВЫБРАТЬ А В (1 ИЗ Т ГДЕ Б", SyntaxKind::SDBL_WHERE_CLAUSE),
    ];

    for (input, kind) in cases {
        assert!(covers(input, false), "`{input}`");
        assert_eq!(clause_count(input, *kind), 1, "`{input}` keeps its {kind:?}");
    }
}

/// Every position where a bare name must be a field or a table, and what a
/// clause keyword standing there costs.
///
/// The source closes these positions to keywords: «Имена таблиц и полей не
/// могут совпадать с ключевыми словами языка запросов». Before this, each of
/// these parsed clean and meant something else — the keyword became the
/// operand, the source clause behind it became an alias, and nothing was
/// reported at all.
///
/// What the position is matters twice over. A bare name is a field only where
/// the query says what to read; where it says how to arrange what was read,
/// the same word may be an alias the select list declared. And a name
/// carrying a dot is a qualifier, which may be the alias of a source. Both
/// exceptions are held by `a_keyword_standing_where_a_name_belongs_is_a_name`.
const NAME_POSITIONS: &[(&str, usize, usize)] = &[
    ("ВЫБРАТЬ А + ИЗ Т", 1, 1),
    ("ВЫБРАТЬ А ПОДОБНО ИЗ Т", 1, 1),
    ("ВЫБРАТЬ А ССЫЛКА ИЗ Т", 1, 1),
    ("ВЫБРАТЬ А ПОМЕСТИТЬ ИЗ Т", 1, 1),
    ("ВЫБРАТЬ А ИЗ Т ГДЕ Б = ИЗ", 2, 1),
    ("ВЫБРАТЬ А ИЗ Т1 ЛЕВОЕ СОЕДИНЕНИЕ Т2 ПО Т1.А = ИЗ", 2, 1),
    ("ВЫБРАТЬ СУММА(А) ИЗ Т СГРУППИРОВАТЬ ПО Б ИМЕЮЩИЕ СУММА(А) > ИЗ", 2, 1),
    // The name of a source, which `expect(Ident)` used to take: a matching
    // kind is refused only by a hard boundary, and the first source of the
    // list is reached without the list's own start check.
    ("ВЫБРАТЬ А ИЗ ГДЕ Б", 1, 1),
    ("УНИЧТОЖИТЬ ГДЕ", 2, 0),
    // The alias separator is a word no name position admits either.
    ("ВЫБРАТЬ А ССЫЛКА КАК Б ИЗ Т", 1, 1),
    ("ВЫБРАТЬ А ПОМЕСТИТЬ КАК ИЗ Т", 1, 1),
    // A rejected name inside a group leaves its closer and outer source intact.
    ("ВЫБРАТЬ (А + ИЗ) ИЗ Т", 1, 1),
    ("ВЫБРАТЬ ВЫРАЗИТЬ(А КАК ИЗ) ИЗ Т", 1, 1),
    ("ВЫБРАТЬ А ИЗ Т ГДЕ Б В (ИЗ)", 1, 1),
    ("ВЫБРАТЬ СУММА(ИЗ) ИЗ Т", 1, 1),
    ("ВЫБРАТЬ ВЫРАЗИТЬ(А КАК КАК) ИЗ Т", 1, 1),
    ("ВЫБРАТЬ А ИЗ Т ДЛЯ ИЗМЕНЕНИЯ КАК", 1, 1),
    // `ПО` covers the Russian half of both `ON` and `BY`, so without its
    // English form the same word was a boundary in one language and a table
    // name in the other.
    ("SELECT A FROM BY", 1, 1),
    // The name predicate has to be asked wherever a name stands. These three
    // asked the clause keywords directly, which do not carry `BY`.
    ("SELECT BY FROM T", 1, 1),
    ("SELECT A FROM Catalog.BY", 1, 1),
    ("SELECT A FROM T WHERE B = BY", 1, 1),
    // A name after the dot is still part of the table's name. Four rules walk
    // such a chain and each checked only its first component, so each let a
    // different set of words through the rest of it.
    ("SELECT A REFS Catalog.BY FROM T", 1, 1),
    ("SELECT A FROM T FOR UPDATE Catalog.BY", 2, 1),
    // The word left behind opens a clause of its own, so leaving without
    // saying anything made a broken name into a whole clean query.
    ("SELECT A FROM T FOR UPDATE Catalog.ORDER BY A", 1, 1),
];

#[test]
fn a_keyword_is_never_a_field_or_a_table_name() {
    for (input, expected, sources) in NAME_POSITIONS {
        assert!(covers(input, false), "`{input}`");
        assert_eq!(clause_count(input, SyntaxKind::SDBL_FROM_CLAUSE), *sources, "`{input}`");
        assert_eq!(
            messages(input, false).len(),
            *expected,
            "`{input}`: {:?}",
            messages(input, false)
        );
    }
}

#[test]
fn the_clause_a_taken_name_used_to_cost_is_parsed() {
    // The point of refusing the word rather than consuming it: `ГДЕ` was the
    // table of the source clause and `Б` was its alias, so the filter existed
    // nowhere in the tree.
    let input = "ВЫБРАТЬ А ИЗ ГДЕ Б";
    assert_eq!(clause_count(input, SyntaxKind::SDBL_WHERE_CLAUSE), 1);
}

#[test]
fn a_name_the_text_does_not_hold_is_never_accepted_in_silence() {
    // The half of this defect that is not about losing a clause: the position
    // reported nothing, so a consumer could not tell these from a query that
    // says what its author meant.
    for (input, _, _) in NAME_POSITIONS {
        assert!(!messages(input, false).is_empty(), "`{input}` says nothing");
    }
}

#[test]
fn what_closing_the_name_positions_costs() {
    // The measured price, and the source is what makes it a price rather than
    // a defect: a field may not be named `Итоги`, so this query is not legal
    // either way — but recovery no longer reaches the source clause, where
    // before the keyword was quietly taken as the operand and the clause was
    // parsed.
    let input = "ВЫБРАТЬ А + Итоги ИЗ Т";
    assert!(covers(input, false));
    assert_eq!(clause_count(input, SyntaxKind::SDBL_FROM_CLAUSE), 0);
    assert_eq!(messages(input, false).len(), 2, "{:?}", messages(input, false));
}

#[test]
fn the_clauses_after_the_body_are_inside_the_boundary_too() {
    // `АВТОУПОРЯДОЧИВАНИЕ`, `УПОРЯДОЧИТЬ` and `ИТОГИ` may come in any order
    // and are parsed after the body rather than in it, so a boundary stated
    // only for the body leaves them outside it.
    let cases: &[(&str, SyntaxKind)] = &[
        ("ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО () АВТОУПОРЯДОЧИВАНИЕ", SyntaxKind::SDBL_AUTOORDER),
        (
            "ВЫБРАТЬ А ИЗ Т АВТОУПОРЯДОЧИВАНИЕ УПОРЯДОЧИТЬ ПО () ИТОГИ ПО А",
            SyntaxKind::SDBL_TOTALS_BY,
        ),
    ];

    for (input, kind) in cases {
        assert!(covers(input, false), "`{input}`");
        assert_eq!(clause_count(input, *kind), 1, "`{input}` keeps its {kind:?}");
    }
}

// --- What a boundary must never do ---------------------------------------

#[test]
fn a_keyword_standing_where_a_name_belongs_is_a_name() {
    // These are legal queries. Whatever protects a clause keyword from being
    // consumed must not reach the position after an explicit `КАК`, where a
    // keyword is an ordinary alias.
    for input in [
        "ВЫБРАТЬ А КАК Итоги ИЗ Т",
        "ВЫБРАТЬ А КАК УНИЧТОЖИТЬ ИЗ Т",
        "ВЫБРАТЬ А ИЗ Т КАК Итоги",
        "ВЫБРАТЬ А ИЗ Т ИТОГИ СУММА(А) ПО Н КАК УНИЧТОЖИТЬ",
        // An alias may spell a keyword, so a reference to one may too. Where
        // the query arranges what it read, a bare name may be an alias the
        // select list declared; and a name carrying a dot is the qualifier of
        // a chain, which may be the alias of a source. A join condition
        // reading `Итоги.Регистратор` is a whole join's worth of tree.
        "ВЫБРАТЬ А КАК Итоги ИЗ Т УПОРЯДОЧИТЬ ПО Итоги",
        "ВЫБРАТЬ А ИЗ Т СГРУППИРОВАТЬ ПО ИЗ",
        "ВЫБРАТЬ А ИЗ Т УПОРЯДОЧИТЬ ПО ИЗ",
        "ВЫБРАТЬ Зак.Ссылка ИЗ Т КАК Зак ЛЕВОЕ СОЕДИНЕНИЕ Т2 КАК Итоги \
         ПО Зак.Ссылка = Итоги.Регистратор",
        // Legal queries whose shape the field-name rule must not disturb.
        "ВЫБРАТЬ А ИЗ Т ГДЕ Б В (ВЫБРАТЬ Ц ИЗ Т2)",
        "ВЫБРАТЬ А ИЗ Т ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Б ИЗ Т2",
        "ВЫБРАТЬ ВЫБОР КОГДА А ТОГДА 1 ИНАЧЕ 2 КОНЕЦ ИЗ Т",
        "ВЫБРАТЬ ВЫРАЗИТЬ(А КАК ЧИСЛО(10, 2)) ИЗ Т",
        "ВЫБРАТЬ А ССЫЛКА Справочник.Т ИЗ Т",
        "ВЫБРАТЬ А ПОМЕСТИТЬ Врем ИЗ Т",
        "ВЫБРАТЬ А ИЗ Т ГДЕ А МЕЖДУ 1 И 2",
        // A query reads its own names. A subquery inside a filter, and each
        // member of a `UNION`, order themselves by an alias their own
        // selection declared — the scope of the clause holding them says
        // nothing about it.
        "ВЫБРАТЬ А ИЗ Т ГДЕ А В (ВЫБРАТЬ Б КАК Итоги ИЗ У УПОРЯДОЧИТЬ ПО Итоги)",
        "ВЫБРАТЬ А ИЗ Т ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Б КАК Итоги ИЗ У УПОРЯДОЧИТЬ ПО Итоги",
        // `КАК` is refused where a name is required, and this rule is also
        // reached where an expression has ended and its alias follows. Eight
        // production queries in a corpus of 3 142 814 literals hold this shape.
        "ВЫБРАТЬ А ИЗ Т ИТОГИ КОЛИЧЕСТВО(А) КАК А ПО Б",
        "УНИЧТОЖИТЬ Врем",
        // Every English clause that spells its second word `BY`, which is a
        // boundary and still has to be consumed by the clause that owns it.
        "SELECT A FROM T GROUP BY A",
        "SELECT A FROM T ORDER BY A",
        "SELECT SUM(A) FROM T TOTALS SUM(A) BY B",
        "SELECT A FROM T INDEX BY A",
        "SELECT A FROM T1 LEFT JOIN T2 ON T1.A = T2.B",
        "ВЫБРАТЬ А ИЗ Т ДЛЯ ИЗМЕНЕНИЯ Т",
        // `BY` ends a list, so no rule inside one may consume it — and it is
        // still an ordinary name where a name may stand. Saying both with one
        // predicate cost these. As a field or source alias it is reserved.
        "SELECT BY.A FROM T",
        "SELECT A FROM T TOTALS SUM(A) BY N BY",
        // A field chain is open to keywords where a table's name is not, and
        // closing the one must not close the other.
        "ВЫБРАТЬ Т.Итоги ИЗ Справочник.Товары КАК Т",
        "ВЫБРАТЬ Т.Конец ИЗ Т",
        "ВЫБРАТЬ Т.И ИЗ Т",
        // Attested by slice 10a: a configuration may name an object `В`, so a
        // word carrying a kind of its own is a name after a dot. Refusing it
        // here broke three of that slice's tests.
        "ВЫБРАТЬ * ИЗ Справочник.В",
        "ВЫБРАТЬ * ИЗ Т КАК Т ДЛЯ ИЗМЕНЕНИЯ Т.В",
        "ВЫБРАТЬ * ИЗ Т КАК Т ГДЕ Т.Поле ССЫЛКА Справочник.В",
        "ВЫБРАТЬ А ССЫЛКА Справочник.Товары ИЗ Т",
        "ВЫБРАТЬ А ИЗ РегистрСведений.Курсы.СрезПоследних",
        "SELECT CAST(A AS Catalog.Goods) FROM T",
        "SELECT A FROM T FOR UPDATE Catalog.Goods",
    ] {
        let parse = parser::parse_sdbl(input);
        assert!(!parse.has_errors(), "`{input}`: {:#?}", parse.errors());
    }
}

// --- BSL: имя там, где оно обязательно ------------------------------------

/// #259: пробел в имени параметра не забирает закрыватель списка.
///
/// `]` стоит там, где список имён не может стоять ни при какой ошибке в нём:
/// имя — единственное, что позиция вправе взять, а `)` — единственное, что она
/// обязана оставить списку. Восстановление на `]` — это сообщение о пропущенном
/// имени; восстановление на `)` — потерянный список и лишнее сообщение.
#[test]
fn a_missing_parameter_name_does_not_swallow_the_list_closer() {
    for input in [
        "Процедура П(])\nКонецПроцедуры",
        "Процедура П(А, ])\nКонецПроцедуры",
        "Процедура П(Знач ])\nКонецПроцедуры",
    ] {
        let messages = messages(input, true);
        assert_eq!(messages.len(), 1, "{input}: {messages:?}");
        assert!(messages[0].contains("имя параметра"), "{input}: {messages:?}");
        assert!(
            !swallowed(input, true, ")"),
            "{input}: закрыватель списка поглощён: {:?}",
            error_node_texts(input, true)
        );
        assert!(covers(input, true), "{input}");
    }

    // Набор имени между запятой и уже набранным закрывателем остаётся молчанием:
    // это состояние правки, а не ошибка (находка D10).
    let typing = "Процедура П(А, )\nКонецПроцедуры";
    assert_eq!(messages(typing, true), Vec::<String>::new(), "{:?}", messages(typing, true));

    // Текст, отвергнутый лексером, второй жалобы не получает: она говорит о нём
    // не больше первой (норма `at_error` в `parser.rs`).
    let lexer_error = "Процедура П(`)\nКонецПроцедуры";
    assert!(
        !messages(lexer_error, true).iter().any(|message| message.contains("имя параметра")),
        "{:?}",
        messages(lexer_error, true)
    );

    // `=` в позиции имени — продолжение самого параметра: значение по умолчанию
    // при ещё не набранном имени. Взятый восстановлением, он отдал бы значение
    // `expect(RParen)`, а закрыватель списка — телу.
    for typing in [
        "Процедура П(= 5)\nКонецПроцедуры",
        "Процедура П(Знач = 5)\nКонецПроцедуры",
        "Процедура П(А, = А + 1)\nКонецПроцедуры",
    ] {
        assert_eq!(messages(typing, true), Vec::<String>::new(), "{typing}");
    }
}

/// #209: `Перем` без имени сообщает о пропаже, не сдвигая позицию.
///
/// Токен, на котором позиция споткнулась, держит продолжение разбора: `;`
/// закрывает объявление, `,` — список, `Экспорт` ещё будет прочитан своим
/// `eat`. Сообщение стоит там, где имени не хватает, а токен остаётся тексту.
#[test]
fn a_var_declaration_without_a_name_reports_and_keeps_the_statement() {
    for input in
        ["Перем ;", "Перем;", "Перем А, ;", "Перем , А;", "Перем А, , Б;", "Перем Экспорт;"]
    {
        let messages = messages(input, true);
        assert_eq!(messages.len(), 1, "{input}: {messages:?}");
        assert!(messages[0].contains("имя переменной"), "{input}: {messages:?}");
        assert!(covers(input, true), "{input}: текст потерян");
        for kept in [";", ",", "Экспорт"] {
            assert!(
                !swallowed(input, true, kept),
                "{input}: `{kept}` уехал в восстановление: {:?}",
                error_node_texts(input, true)
            );
        }
    }

    // Текст, отвергнутый лексером, в позиции имени второй жалобы не получает.
    for input in ["Перем &;", "Перем А, !;"] {
        let messages = messages(input, true);
        assert_eq!(messages.len(), 1, "{input}: {messages:?}");
        assert!(!messages[0].contains("имя переменной"), "{input}: {messages:?}");
    }

    // Незавершённый набор на конце ввода — не ошибка: после `Перем` имя ещё
    // может быть набрано.
    assert!(messages("Перем", true).is_empty(), "{:?}", messages("Перем", true));
}

/// Имя процедуры или функции не берёт закрыватель собственного объявления.
///
/// Ключевое слово в позиции имени — это имя (находки D5/D6: `Процедура Если()`
/// разбирается, а диагностика называет нарушение), но слово, закрывающее ЭТО
/// объявление, именем быть не может: взяв его, правило теряет закрыватель и
/// жалуется на конец файла.
#[test]
fn a_missing_declaration_name_does_not_take_the_closer() {
    for (input, closer) in
        [("Процедура\nКонецПроцедуры", "КонецПроцедуры"), ("Функция\nКонецФункции", "КонецФункции")]
    {
        let messages = messages(input, true);
        assert_eq!(messages.len(), 1, "{input}: {messages:?}");
        assert!(
            messages[0].contains("имя процедуры") || messages[0].contains("имя функции"),
            "{input}: {messages:?}"
        );
        closer_survives(input, closer);
    }
}
