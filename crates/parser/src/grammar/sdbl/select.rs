use crate::event::NodeKind;
use crate::parser::Parser;

use super::expressions;

pub(super) fn at_sdbl_keyword(p: &Parser, en: &str, ru: &str) -> bool {
    p.at_keyword(en) || p.at_keyword(ru)
}

pub(super) fn eat_sdbl_keyword(p: &mut Parser, en: &str, ru: &str) -> bool {
    p.eat_keyword(en) || p.eat_keyword(ru)
}

// =====================================================================
// CLEAN-ROOM Slice 12 — field-list recovery
//
// Skips a malformed selected field to the next boundary a reader would
// recognise — an alias, a comma, a clause keyword — while refusing to
// stop inside a `CASE`, a paren group or a nested query, where such a
// boundary would be a false one. Not the language; kept so that one bad
// field does not destroy the list around it.
//
// Provenance: `docs/legal/sdbl-clean-room-slice12.md`, entry A6.
// =====================================================================

fn recover_field_to_alias_or_delimiter(p: &mut Parser) {
    let err = p.start();
    let mut case_depth = 0i32;
    let mut paren_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut consumed_any = false;
    let mut nested_query_starts: Vec<i32> = Vec::new();

    loop {
        p.check_iteration_limit();

        // An extension region is opaque: the words inside it are not this
        // query's, and a boundary read from one is a boundary that is not
        // there.
        if p.at(T![LBrace]) {
            brace_depth += 1;
            p.bump();
            consumed_any = true;
            continue;
        }

        if p.at(T![RBrace]) {
            brace_depth = brace_depth.saturating_sub(1);
            p.bump();
            consumed_any = true;
            continue;
        }

        if brace_depth > 0 {
            if p.at_end() || p.at(T![Semicolon]) {
                break;
            }
            p.bump();
            consumed_any = true;
            continue;
        }

        if p.at_keyword("CASE") || p.at_keyword("ВЫБОР") {
            case_depth += 1;
            p.bump();
            consumed_any = true;
            continue;
        }

        if (p.at_keyword("END") || p.at_keyword("КОНЕЦ")) && case_depth > 0 {
            case_depth -= 1;
            p.bump();
            consumed_any = true;
            continue;
        }

        if p.at(T![LParen]) {
            paren_depth += 1;
            p.bump();
            consumed_any = true;
            if is_query_starter_or_combiner_keyword(p) {
                nested_query_starts.push(paren_depth);
            }
            continue;
        }

        if p.at(T![RParen]) && paren_depth > 0 {
            if let Some(&d) = nested_query_starts.last() {
                if d == paren_depth {
                    nested_query_starts.pop();
                }
            }
            paren_depth -= 1;
            p.bump();
            consumed_any = true;
            continue;
        }

        let at_top_level = case_depth == 0 && paren_depth == 0;
        let inside_nested_query = !nested_query_starts.is_empty();
        // Refusing to take the boundary elsewhere buys nothing if the skip
        // that follows takes it instead.
        if at_top_level && super::at_query_boundary(p) {
            break;
        }
        // A clause keyword standing as the field name after a dot belongs
        // to what is being skipped, and the drain reads it that way too.
        //
        // An open `ВЫБОР` does not earn the same treatment, though its
        // depth is counted here: an unclosed one would then swallow the
        // rest of the query, and half-written `ВЫБОР` is what an editor
        // buffer holds. Stopping early costs a closed `ВЫБОР` its skip;
        // stopping late costs every unclosed one its whole query.
        if is_clause_keyword(p) && p.prev_significant() != Some(T![Dot]) {
            let stop = if at_top_level {
                true
            } else if inside_nested_query {
                false
            } else {
                !is_query_starter_or_combiner_keyword(p)
            };
            if stop {
                break;
            }
        }
        if p.at(T![Semicolon]) {
            break;
        }
        if p.at_end() {
            break;
        }

        if case_depth == 0 && paren_depth == 0 {
            if at_sdbl_keyword(p, "AS", "КАК") {
                break;
            }
            if p.at(T![Comma]) {
                break;
            }
            if p.at(T![RParen]) {
                break;
            }
        }

        p.bump();
        consumed_any = true;
    }

    if consumed_any {
        p.error_custom_at_marker(err, "пропуск некорректного фрагмента");
    } else {
        err.abandon(p);
    }
}

pub fn select_query(p: &mut Parser) {
    let m = p.start();
    // The clauses after the body — `АВТОУПОРЯДОЧИВАНИЕ`, `УПОРЯДОЧИТЬ`,
    // `ИТОГИ` — may come in any order and are parsed here rather than in the
    // body, so stating the boundary in the body alone leaves them outside it.
    p.within_boundary(is_clause_keyword, |p| {
        subquery(p);
        select_tail_clauses(p);
    });
    m.complete(p, NodeKind::SdblSelectQuery);
}

pub(super) fn subquery(p: &mut Parser) {
    let m = p.start();

    query(p);

    loop {
        if p.at(T![Semicolon]) {
            break;
        }

        if !at_sdbl_keyword(p, "UNION", "ОБЪЕДИНИТЬ") {
            break;
        }

        union_clause(p);
    }

    m.complete(p, NodeKind::SdblSubquery);
}

fn union_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "UNION", "ОБЪЕДИНИТЬ");

    eat_sdbl_keyword(p, "ALL", "ВСЕ");

    query(p);

    m.complete(p, NodeKind::SdblUnionClause);
}

/// A query reads its own names.
///
/// Every query arrives here — the package's own members, a subquery in a
/// filter or a comparison, a parenthesised source, each member of a `UNION` —
/// and whatever field-name scope the expression holding it was in says nothing
/// about how its own clauses read theirs: its `УПОРЯДОЧИТЬ ПО` may name an
/// alias its own selection declared, and an alias may spell a keyword.
///
/// Stated by a call owning the scope rather than by hand, because the body
/// below returns early in several places and each of those would otherwise
/// leave the holder's scope behind.
fn query(p: &mut Parser) {
    p.outside_field_names(query_content);
}

fn query_content(p: &mut Parser) {
    let m = p.start();

    if !eat_sdbl_keyword(p, "SELECT", "ВЫБРАТЬ") {
        // The word is missing, not wrong: what stands here belongs to the
        // rest of the query. Taking it would cost the query the clause it
        // starts with, and would put the complaint on that clause instead
        // of on the gap in front of it.
        //
        // The rule still stops here. Carrying on would parse a body for a
        // query that has no keyword, and this rule is also reached
        // speculatively — from a parenthesised source, where Slice 8
        // requires `ИЗ (&Tmp)` to yield no field of its own.
        p.error_custom_no_bump("ожидалось 'ВЫБРАТЬ' / 'SELECT'");
        m.complete(p, NodeKind::SdblQuery);
        return;
    }

    // The words that open the clauses of this query are what the query is
    // waiting for, so no rule inside it may report an error by taking one:
    // an unfinished `ВЫБОР` used to report its missing `КОНЕЦ` by consuming
    // the `ИЗ` that followed, and the source clause then existed nowhere.
    //
    // A clause keyword is still an ordinary name where a name is what
    // belongs — `КАК Итоги` names a source — and that is why this is stated
    // here rather than as a word the whole parse treats as a boundary.
    p.within_boundary(is_clause_keyword, |p| {
        if is_limitation_keyword(p) {
            limitations(p);
        }

        selected_fields(p);

        eat_query_extensions(p);
        if at_sdbl_keyword(p, "INTO", "ПОМЕСТИТЬ") {
            into_clause(p);
        }

        query_body_clauses(p);
    });

    m.complete(p, NodeKind::SdblQuery);
}

pub(super) fn selected_fields(p: &mut Parser) {
    let m = p.start();

    // The list parser takes its first item unconditionally, and an
    // expression asked to start on a clause keyword takes it anyway. That
    // turns a selection list the user has not typed yet — or one whose
    // qualifier failed to parse — into a field made of the next clause's
    // keyword, and the clause it belonged to is then never recognised.
    // Only a clause keyword, a separator or the end of input means the list
    // is absent. Anything else — a stray comma, a token no rule accepts — is
    // a list with something wrong inside it, and the list parser's own
    // recovery handles that better than giving up here would.
    let list_is_absent = is_clause_keyword(p)
        || p.at(T![Semicolon])
        || p.at(T![RParen])
        || p.at(T![LBrace])
        || p.at_end();

    if !list_is_absent {
        // A bare name in the selection is a field of a source, so a clause
        // keyword here is not a name at all.
        p.within_field_names(|p| {
            super::expressions::parse_delimited_list(
                p,
                T![Comma],
                &super::LIST_RECOVERY,
                is_field_start,
                selected_field,
            )
        });
    } else if !p.at_end() {
        // The list is mandatory, so its absence is worth saying — but say
        // it without moving, or the clause keyword that revealed the
        // absence is the thing that gets consumed. At end of input this
        // stays quiet: that is a query being typed, not one missing a
        // selection.
        p.error_custom_no_bump("ожидался список полей выборки после 'ВЫБРАТЬ' / 'SELECT'");
    }

    m.complete(p, NodeKind::SdblFieldList);
}

fn selected_field(p: &mut Parser) {
    let m = p.start();

    if is_asterisk_start(p) {
        asterisk_field(p);
    } else {
        expressions::expression(p);

        let at_expected_position = at_sdbl_keyword(p, "AS", "КАК")
            || at_an_implicit_alias(p)
            || p.at(T![Comma])
            || p.at(T![Semicolon])
            || p.at(T![LBrace])
            || is_clause_keyword(p)
            || p.at_end();

        if !at_expected_position {
            recover_field_to_alias_or_delimiter(p);
        }
    }

    if at_sdbl_keyword(p, "AS", "КАК") || at_an_implicit_alias(p) {
        selected_field_alias(p);
    }

    m.complete(p, NodeKind::SdblSelectedField);
}

fn is_field_start(p: &Parser) -> bool {
    if is_asterisk_start(p) {
        return true;
    }
    super::expressions::is_expression_start(p)
}

fn is_asterisk_start(p: &Parser) -> bool {
    if p.at(T![Star]) {
        return true;
    }

    if p.at(T![Ident]) {
        if let Some(T![Dot]) = p.nth(1) {
            if let Some(T![Star]) = p.nth(2) {
                return true;
            }
        }
    }

    false
}

fn asterisk_field(p: &mut Parser) {
    let m = p.start();

    while p.at(T![Ident]) {
        if let Some(T![Dot]) = p.nth(1) {
            p.bump();
            p.bump();
        } else {
            break;
        }
    }

    p.expect(T![Star]);

    m.complete(p, NodeKind::SdblAsteriskField);
}

fn selected_field_alias(p: &mut Parser) {
    let m = p.start();
    alias_name(p, "ожидался псевдоним после 'КАК' / 'AS'", Aliased::Field);
    m.complete(p, NodeKind::SdblAlias);
}

/// Reads an alias after its optional `КАК`, refusing the words the language
/// reserves.
///
/// Most keywords are not reserved as alias names — `КАК Итоги`, `КАК Выбрать`,
/// `КАК Упорядочить` are accepted by the platform — so the word alone decides,
/// not whether it opens a clause. A refused word is reported at itself. After
/// an explicit `КАК` one that opens a clause is left to that clause: the alias
/// was more likely omitted than misspelt, and taking the word would cost the
/// clause behind it. Any other refused word — a clause word too, when a list
/// separator or the end follows it and no clause can begin — is taken as the
/// alias it was meant to be, so the source or field it names keeps its place in
/// the query.
///
/// Without `КАК` the word is reported only behind a complete name: one refused
/// as the table, or as the part of a name after a dot, has been reported there
/// already.
fn alias_name(p: &mut Parser, missing: &'static str, aliased: Aliased) {
    let explicit = eat_sdbl_keyword(p, "AS", "КАК");
    if explicit && matches!(aliased, Aliased::Totals) && at_sdbl_keyword(p, "BY", "ПО") {
        p.error_custom_no_bump(missing);
        return;
    }

    // Behind a dot the word is the part of a name that the dot promised and
    // the name's own rule has refused it already.
    let after_a_name = !matches!(aliased, Aliased::Source { named: false });
    let reported_elsewhere = !after_a_name || p.prev_significant() == Some(T![Dot]);
    if at_word_refused_as_alias(p) && (explicit || !reported_elsewhere) {
        let message = if explicit {
            "ожидается имя: ключевое слово языка запросов нельзя использовать как псевдоним"
        } else {
            "синтаксическая ошибка: ключевое слово языка запросов нельзя использовать как псевдоним"
        };
        if explicit && at_a_clause_that_goes_on(p, aliased) {
            p.error_custom_no_bump(message);
        } else if is_clause_keyword(p) {
            // A clause word is a boundary the generic recovery will not consume,
            // so it is taken explicitly; only a separator or the end follows it.
            let err = p.start();
            p.bump();
            p.error_custom_at_marker(err, message);
        } else {
            p.error_custom(message);
        }
        return;
    }

    super::eat_name_here(p, missing);
}

/// What an alias names: a selected field, or a source together with whether
/// the source's name was taken.
#[derive(Clone, Copy)]
enum Aliased {
    Field,
    Totals,
    Source { named: bool },
}

/// A clause word that really opens a clause here. A join and its `ПО` follow
/// only a source, never a field.
fn at_a_clause_that_goes_on(p: &Parser, aliased: Aliased) -> bool {
    let opens_here = match aliased {
        Aliased::Field | Aliased::Totals => !is_join_keyword(p) && !at_sdbl_keyword(p, "ON", "ПО"),
        Aliased::Source { .. } => true,
    };
    is_clause_keyword(p)
        && opens_here
        && !matches!(p.nth(1), None | Some(T![Comma]) | Some(T![RParen]) | Some(T![Semicolon]))
}

/// A word standing where an alias without `КАК` would: a name that opens no
/// clause, or a reserved word of a kind of its own, which is reported there.
fn at_an_implicit_alias(p: &Parser) -> bool {
    (is_identifier_token(p) && !is_clause_keyword(p)) || at_a_keyword_kind_refused_as_alias(p)
}

/// A word the platform refuses as an alias: after `КАК` it answers «Ожидается
/// имя», without `КАК` it reports a syntax error at the word.
///
/// The list is exactly the set checked word by word after `КАК` on 8.3.17 and
/// 8.3.27, as a field alias and as a source alias, and refused by both; the
/// form without `КАК` was checked on 8.3.27. Counterparts that
/// were not checked (`ESCAPE`, `AUTOORDER`, `PERIODS`, `ON`) are left out rather
/// than guessed. `В`, `И`, `ИЛИ`, `НЕ`, `ИСТИНА`, `ЛОЖЬ` and `НЕОПРЕДЕЛЕНО`
/// reach the parser with kinds of their own; the rest arrive as identifiers.
fn at_word_refused_as_alias(p: &Parser) -> bool {
    const REFUSED: &[&str] = &[
        "ИЗ",
        "ГДЕ",
        "КАК",
        "ПО",
        "ВЫБОР",
        "КОГДА",
        "ТОГДА",
        "ИНАЧЕ",
        "ЕСТЬ",
        "NULL",
        "ПОДОБНО",
        "СПЕЦСИМВОЛ",
        "МЕЖДУ",
        "ПЕРВЫЕ",
        "РАЗЛИЧНЫЕ",
        "РАЗРЕШЕННЫЕ",
        "ВНУТРЕННЕЕ",
        "ДЛЯ",
        "ПОМЕСТИТЬ",
        "ВОЗР",
        "УБЫВ",
        "АВТОУПОРЯДОЧИВАНИЕ",
        "ОБЩИЕ",
        "ТОЛЬКО",
        "ПЕРИОДАМИ",
        "ВЫРАЗИТЬ",
        "FROM",
        "WHERE",
        "AS",
        "BY",
        "CASE",
        "WHEN",
        "THEN",
        "ELSE",
        "IS",
        "LIKE",
        "BETWEEN",
        "TOP",
        "DISTINCT",
        "ALLOWED",
        "INNER",
        "FOR",
        "INTO",
        "ASC",
        "DESC",
        "ONLY",
        "OVERALL",
        "CAST",
    ];

    at_a_keyword_kind_refused_as_alias(p) || REFUSED.iter().any(|word| p.at_keyword(word))
}

fn at_a_keyword_kind_refused_as_alias(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(
            T![KwIn]
                | T![KwAnd]
                | T![KwOr]
                | T![KwNot]
                | T![KwTrue]
                | T![KwFalse]
                | T![KwUndefined]
        )
    )
}

fn into_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "INTO", "ПОМЕСТИТЬ");

    if super::at_field_name(p) {
        let table_m = p.start();
        p.bump();
        table_m.complete(p, NodeKind::SdblTempTableName);
    } else {
        p.error_custom("ожидалось имя временной таблицы после 'ПОМЕСТИТЬ' / 'INTO'");
    }

    m.complete(p, NodeKind::SdblIntoClause);
}

fn is_data_source_start(p: &Parser) -> bool {
    match p.current() {
        Some(T![LParen]) => true,
        Some(T![Ampersand]) => true,
        Some(T![Ident]) => !is_clause_keyword(p),
        _ => false,
    }
}

fn from_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "FROM", "ИЗ");

    super::expressions::parse_delimited_list(
        p,
        T![Comma],
        &super::LIST_RECOVERY,
        is_data_source_start,
        data_source,
    );

    m.complete(p, NodeKind::SdblFromClause);
}

fn data_source(p: &mut Parser) {
    let m = p.start();

    let named = if p.at(T![LParen]) {
        p.bump();
        let checkpoint = p.error_checkpoint();
        p.within_boundary(expressions::at_closing_paren, |p| {
            subquery(p);
            expressions::finish_parenthesized(p, checkpoint);
        });
        true
    } else {
        table_ref(p)
    };

    if at_sdbl_keyword(p, "AS", "КАК") || at_an_implicit_alias(p) {
        source_alias(p, named);
    }

    eat_query_extensions(p);
    while is_join_keyword(p) {
        join_clause(p);
        eat_query_extensions(p);
    }

    m.complete(p, NodeKind::SdblDataSource);
}

/// Returns whether a name was taken: a word refused as the table has already
/// been reported, and saying it again as a bad alias would report it twice.
fn table_ref(p: &mut Parser) -> bool {
    let m = p.start();

    if p.at(T![Ampersand]) {
        let pm = p.start();
        // Имя параметра приходит внутри самого токена, отдельного `Ident` за
        // амперсандом не бывает. Взять его значило бы отобрать у запроса
        // следующее слово — `AS` у псевдонима, `ПО` у соединения.
        p.bump();
        pm.complete(p, NodeKind::SdblParameter);
        m.complete(p, NodeKind::SdblTableRef);
        return true;
    }

    // `expect` on a matching kind is refused only by a hard boundary, and a
    // clause keyword is not one — so asking it here took `ГДЕ` for the table
    // and the filter behind it existed nowhere. The first source of a list is
    // reached without the list's own start check, which is why this belongs at
    // the reference itself.
    if p.at(T![Ident]) && !super::at_field_name(p) {
        p.error_custom_no_bump("ожидалось имя таблицы");
        m.complete(p, NodeKind::SdblTableRef);
        return false;
    }

    if !p.expect(T![Ident]) {
        m.complete(p, NodeKind::SdblTableRef);
        return false;
    }

    // The dot reaches across the space before it as well as the space after
    // it: `Справочник . Товары` is one name, and the expression layer already
    // reads it as one.
    while p.nth(1) == Some(T![Dot]) || p.at(T![Dot]) {
        if !p.eat(T![Dot]) {
            break;
        }
        p.check_iteration_limit();

        if !super::at_table_name_component(p) {
            report_the_component_the_dot_promised(p);
            break;
        }

        p.bump();
    }

    virtual_table_args(p);

    m.complete(p, NodeKind::SdblTableRef);
    true
}

fn source_alias(p: &mut Parser, named: bool) {
    let m = p.start();
    alias_name(p, "ожидался псевдоним источника после 'КАК' / 'AS'", Aliased::Source { named });
    m.complete(p, NodeKind::SdblAlias);
}

fn is_join_keyword(p: &Parser) -> bool {
    p.at_keyword("LEFT")
        || p.at_keyword("ЛЕВОЕ")
        || p.at_keyword("RIGHT")
        || p.at_keyword("ПРАВОЕ")
        || p.at_keyword("FULL")
        || p.at_keyword("ПОЛНОЕ")
        || p.at_keyword("INNER")
        || p.at_keyword("ВНУТРЕННЕЕ")
        || p.at_keyword("JOIN")
        || p.at_keyword("СОЕДИНЕНИЕ")
}

fn join_clause(p: &mut Parser) {
    let m = p.start();

    let has_type = p.at_keyword("LEFT")
        || p.at_keyword("ЛЕВОЕ")
        || p.at_keyword("RIGHT")
        || p.at_keyword("ПРАВОЕ")
        || p.at_keyword("FULL")
        || p.at_keyword("ПОЛНОЕ")
        || p.at_keyword("INNER")
        || p.at_keyword("ВНУТРЕННЕЕ");
    if has_type {
        p.bump();
    }

    if p.at_keyword("OUTER") || p.at_keyword("ВНЕШНЕЕ") {
        p.bump();
    }

    if !p.at_keyword("JOIN") && !p.at_keyword("СОЕДИНЕНИЕ") {
        p.error_custom("ожидалось 'СОЕДИНЕНИЕ' / 'JOIN'");
        m.complete(p, NodeKind::SdblJoinClause);
        return;
    }
    p.bump();

    data_source(p);

    if !eat_sdbl_keyword(p, "ON", "ПО") {
        p.error_custom("ожидалось 'ПО' / 'ON' в соединении");
    }

    p.within_field_names(expressions::logical_expression);

    m.complete(p, NodeKind::SdblJoinClause);
}

fn select_tail_clauses(p: &mut Parser) {
    let mut parsed_autoorder = false;
    let mut parsed_order_by = false;
    let mut parsed_totals_by = false;

    loop {
        if p.at(T![LBrace]) {
            query_extension(p);
            continue;
        }

        if !parsed_autoorder && at_sdbl_keyword(p, "AUTOORDER", "АВТОУПОРЯДОЧИВАНИЕ")
        {
            autoorder_clause(p);
            parsed_autoorder = true;
            continue;
        }

        if !parsed_order_by && at_sdbl_keyword(p, "ORDER", "УПОРЯДОЧИТЬ") {
            order_by_clause(p);
            parsed_order_by = true;
            continue;
        }

        if !parsed_totals_by && at_sdbl_keyword(p, "TOTALS", "ИТОГИ") {
            totals_by_clause(p);
            parsed_totals_by = true;
            continue;
        }

        break;
    }
}

fn query_body_clauses(p: &mut Parser) {
    eat_query_extensions(p);
    if at_sdbl_keyword(p, "FROM", "ИЗ") {
        from_clause(p);
    }

    eat_query_extensions(p);
    if at_sdbl_keyword(p, "WHERE", "ГДЕ") {
        where_clause(p);
    }

    eat_query_extensions(p);
    if at_sdbl_keyword(p, "GROUP", "СГРУППИРОВАТЬ") {
        group_by_clause(p);
    }

    eat_query_extensions(p);
    if at_sdbl_keyword(p, "HAVING", "ИМЕЮЩИЕ") {
        having_clause(p);
    }

    eat_query_extensions(p);
    if at_sdbl_keyword(p, "FOR", "ДЛЯ") {
        for_update_clause(p);
    }

    eat_query_extensions(p);
    if at_sdbl_keyword(p, "INDEX", "ИНДЕКСИРОВАТЬ") {
        index_by_clause(p);
    }

    eat_query_extensions(p);
    if at_sdbl_keyword(p, "ORDER", "УПОРЯДОЧИТЬ") {
        order_by_clause(p);
    }

    eat_query_extensions(p);
}

/// Consumes any run of `{…}` query-language extension blocks at the current
/// position. These braces mark sections a user may customize at runtime: the
/// data-composition documentation defines `{ВЫБРАТЬ …}`, `{ГДЕ …}` and
/// `{ХАРАКТЕРИСТИКИ …}`, and configurations also contain undocumented forms
/// such as `{УПОРЯДОЧИТЬ ПО …}`. Their inner text is taken verbatim, so the
/// surrounding query parses and keeps its diagnostics whichever element
/// appears.
pub(super) fn eat_query_extensions(p: &mut Parser) {
    while p.at(T![LBrace]) {
        query_extension(p);
    }
}

fn query_extension(p: &mut Parser) {
    let m = p.start();
    p.bump(); // consume '{'

    let mut depth = 1i32;
    loop {
        p.check_iteration_limit();

        if p.at_end() {
            break;
        }

        // An unclosed `{` is tolerated, but tolerating it must not cost the
        // rest of the package: a separator ends the region whether or not
        // its brace was ever closed.
        if p.at(T![Semicolon]) {
            break;
        }

        if p.at(T![LBrace]) {
            depth += 1;
            p.bump();
            continue;
        }

        if p.at(T![RBrace]) {
            depth -= 1;
            p.bump();
            if depth == 0 {
                break;
            }
            continue;
        }

        p.bump();
    }

    // An unbalanced `{` is tolerated rather than reported: the dominant source
    // of such fragments is queries assembled at runtime by string
    // concatenation (`"… {ВЫБРАТЬ" + Поля + "} …"`), where the static literal
    // legitimately ends mid-extension. Flagging those would be a false positive.
    m.complete(p, NodeKind::SdblQueryExtension);
}

fn where_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "WHERE", "ГДЕ");

    p.within_field_names(expressions::logical_expression);

    m.complete(p, NodeKind::SdblWhereClause);
}

pub(super) fn is_clause_keyword(p: &Parser) -> bool {
    at_sdbl_keyword(p, "SELECT", "ВЫБРАТЬ")
        || at_sdbl_keyword(p, "FROM", "ИЗ")
        || at_sdbl_keyword(p, "WHERE", "ГДЕ")
        || at_sdbl_keyword(p, "GROUP", "СГРУППИРОВАТЬ")
        || at_sdbl_keyword(p, "HAVING", "ИМЕЮЩИЕ")
        || at_sdbl_keyword(p, "ORDER", "УПОРЯДОЧИТЬ")
        || at_sdbl_keyword(p, "UNION", "ОБЪЕДИНИТЬ")
        || at_sdbl_keyword(p, "INTO", "ПОМЕСТИТЬ")
        || at_sdbl_keyword(p, "ON", "ПО")
        || at_sdbl_keyword(p, "FOR", "ДЛЯ")
        || at_sdbl_keyword(p, "INDEX", "ИНДЕКСИРОВАТЬ")
        || at_sdbl_keyword(p, "AUTOORDER", "АВТОУПОРЯДОЧИВАНИЕ")
        || at_sdbl_keyword(p, "TOTALS", "ИТОГИ")
        || is_join_keyword(p)
}

pub(super) fn is_likely_clause_start_after_dot(p: &Parser) -> bool {
    is_clause_keyword(p)
        || at_sdbl_keyword(p, "AS", "КАК")
        || at_sdbl_keyword(p, "ASC", "ВОЗР")
        || at_sdbl_keyword(p, "DESC", "УБЫВ")
        || at_sdbl_keyword(p, "CASE", "ВЫБОР")
        || at_sdbl_keyword(p, "WHEN", "КОГДА")
        || at_sdbl_keyword(p, "THEN", "ТОГДА")
        || at_sdbl_keyword(p, "ELSE", "ИНАЧЕ")
        || at_sdbl_keyword(p, "END", "КОНЕЦ")
        || at_sdbl_keyword(p, "BETWEEN", "МЕЖДУ")
        || at_sdbl_keyword(p, "LIKE", "ПОДОБНО")
        || at_sdbl_keyword(p, "ESCAPE", "СПЕЦСИМВОЛ")
        || at_sdbl_keyword(p, "ALL", "ВСЕ")
        || at_sdbl_keyword(p, "DISTINCT", "РАЗЛИЧНЫЕ")
        || at_sdbl_keyword(p, "TOP", "ПЕРВЫЕ")
        || at_sdbl_keyword(p, "HIERARCHY", "ИЕРАРХИИ")
        || at_sdbl_keyword(p, "DROP", "УНИЧТОЖИТЬ")
        || at_sdbl_keyword(p, "BY", "ПО")
}

pub(super) fn is_query_starter_or_combiner_keyword(p: &Parser) -> bool {
    at_sdbl_keyword(p, "SELECT", "ВЫБРАТЬ") || at_sdbl_keyword(p, "UNION", "ОБЪЕДИНИТЬ")
}

// Primary clause keywords that begin a new query section. When one of these
// appears where a totals alias is expected the alias was almost certainly
// omitted, so the alias parser leaves the keyword for its clause instead of
// swallowing it.
pub(super) fn is_body_clause_keyword(p: &Parser) -> bool {
    at_sdbl_keyword(p, "SELECT", "ВЫБРАТЬ")
        || at_sdbl_keyword(p, "FROM", "ИЗ")
        || at_sdbl_keyword(p, "WHERE", "ГДЕ")
        || at_sdbl_keyword(p, "GROUP", "СГРУППИРОВАТЬ")
        || at_sdbl_keyword(p, "HAVING", "ИМЕЮЩИЕ")
        || at_sdbl_keyword(p, "ORDER", "УПОРЯДОЧИТЬ")
        || at_sdbl_keyword(p, "UNION", "ОБЪЕДИНИТЬ")
        || at_sdbl_keyword(p, "INTO", "ПОМЕСТИТЬ")
}

fn group_by_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "GROUP", "СГРУППИРОВАТЬ");

    if !at_sdbl_keyword(p, "BY", "ПО") {
        m.complete(p, NodeKind::SdblGroupClause);
        return;
    }
    eat_sdbl_keyword(p, "BY", "ПО");

    super::expressions::expression(p);

    while p.eat(T![Comma]) {
        p.check_iteration_limit();
        super::expressions::expression(p);
    }

    m.complete(p, NodeKind::SdblGroupClause);
}

fn order_by_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "ORDER", "УПОРЯДОЧИТЬ");

    if !at_sdbl_keyword(p, "BY", "ПО") {
        m.complete(p, NodeKind::SdblOrderClause);
        return;
    }
    eat_sdbl_keyword(p, "BY", "ПО");

    order_by_item(p);

    while p.eat(T![Comma]) {
        p.check_iteration_limit();
        order_by_item(p);
    }

    m.complete(p, NodeKind::SdblOrderClause);
}

// =====================================================================
// CLEAN-ROOM Slice 12 — ordering-field modifiers
//
// The Developer's Reference gives an ordering field exactly four
// alternatives for its order: `ВОЗР`, `УБЫВ`, `ИЕРАРХИЯ` and
// `ИЕРАРХИЯ УБЫВ`. The reverse word order `УБЫВ ИЕРАРХИЯ` is not among
// them and is accepted anyway, as a plausible slip that costs nothing to
// tolerate.
//
// Provenance: `docs/legal/sdbl-clean-room-slice12.md`, entries A13 and
// D4.
// =====================================================================

fn order_by_item(p: &mut Parser) {
    super::expressions::expression(p);

    // Of the four documented orderings, `ИЕРАРХИЯ` may be preceded by
    // nothing and followed by `УБЫВ`. The reverse, `УБЫВ ИЕРАРХИЯ`, is not
    // among them and is tolerated; `ВОЗР ИЕРАРХИЯ` is not among them either
    // and is not — one stated tolerance, not a general permission.
    let descending_came_first = at_sdbl_keyword(p, "DESC", "УБЫВ");
    let direction_came_first = eat_order_direction(p);

    if at_sdbl_keyword(p, "HIERARCHY", "ИЕРАРХИЯ") {
        if direction_came_first && !descending_came_first {
            p.error_custom_no_bump("'ИЕРАРХИЯ' / 'HIERARCHY' сочетается только с 'УБЫВ' / 'DESC'");
        }

        p.bump();

        if !direction_came_first {
            if at_sdbl_keyword(p, "DESC", "УБЫВ") {
                p.bump();
            } else if at_sdbl_keyword(p, "ASC", "ВОЗР") {
                p.error_custom_no_bump(
                    "'ИЕРАРХИЯ' / 'HIERARCHY' сочетается только с 'УБЫВ' / 'DESC'",
                );
                p.bump();
            }
        }
    }
}

fn eat_order_direction(p: &mut Parser) -> bool {
    if at_sdbl_keyword(p, "ASC", "ВОЗР") || at_sdbl_keyword(p, "DESC", "УБЫВ") {
        p.bump();
        true
    } else {
        false
    }
}

fn having_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "HAVING", "ИМЕЮЩИЕ");

    p.within_field_names(super::expressions::expression);

    m.complete(p, NodeKind::SdblHavingClause);
}

fn for_update_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "FOR", "ДЛЯ");

    eat_sdbl_keyword(p, "UPDATE", "ИЗМЕНЕНИЯ");

    if super::at_field_name(p) {
        p.bump();
        while super::eat_qualifying_dot(p) {
            p.check_iteration_limit();
            if !super::at_table_name_component(p) {
                // The list of targets is optional, so this rule said nothing
                // here before. It says something now — the dot proves a name
                // was being written — but only where a token actually stands
                // and the lexer could read it. Nothing there is the unfinished
                // path of a text built by concatenation, and a token the lexer
                // rejected belongs to a text already reported as unreadable:
                // the inputs that reach here that way are Russian prose whose
                // `для` reads as `ДЛЯ` and whose full stop reads as a dot.
                if !p.at_end() && !p.at_error() {
                    report_the_component_the_dot_promised(p);
                }
                break;
            }
            p.bump();
        }
    }

    m.complete(p, NodeKind::SdblForUpdate);
}

/// Says that the name broke off, once the dot has proved it was being written.
///
/// Which of the two reasons it was depends on what stands there, and naming
/// the wrong one is its own defect: a word that cannot be part of the name is
/// a keyword, while nothing standing there at all is a name the text does not
/// hold. The message said "keyword" for both, which is false at the end of
/// input, after a comma, after a paren.
///
/// Leaving without saying anything is worse than either: the word left behind
/// opens a clause of its own, so `ДЛЯ ИЗМЕНЕНИЯ Catalog.УПОРЯДОЧИТЬ ПО А`
/// parsed as a whole clean query with a broken name inside it.
fn report_the_component_the_dot_promised(p: &mut Parser) {
    if super::expressions::at_property_name(p) {
        let err = p.start();
        p.error_custom_at_marker(err, "ожидалось имя объекта, встречено ключевое слово");
        return;
    }

    p.error_expected(T![Ident]);
}

fn index_by_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "INDEX", "ИНДЕКСИРОВАТЬ");

    if !at_sdbl_keyword(p, "BY", "ПО") {
        m.complete(p, NodeKind::SdblIndexBy);
        return;
    }
    eat_sdbl_keyword(p, "BY", "ПО");

    super::expressions::expression(p);

    while p.eat(T![Comma]) {
        p.check_iteration_limit();
        super::expressions::expression(p);
    }

    m.complete(p, NodeKind::SdblIndexBy);
}

fn autoorder_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "AUTOORDER", "АВТОУПОРЯДОЧИВАНИЕ");

    m.complete(p, NodeKind::SdblAutoorder);
}

fn totals_by_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "TOTALS", "ИТОГИ");

    while !p.at_end() {
        p.check_iteration_limit();

        if at_sdbl_keyword(p, "BY", "ПО") {
            break;
        }

        if is_clause_keyword(p) {
            break;
        }

        if super::expressions::is_expression_start(p) {
            p.within_boundary(expressions::at_comma, expressions::expression);
            if at_sdbl_keyword(p, "AS", "КАК")
                || (at_an_implicit_alias(p) && !at_sdbl_keyword(p, "BY", "ПО"))
            {
                let alias = p.start();
                alias_name(p, "ожидался псевдоним после 'КАК' / 'AS'", Aliased::Totals);
                alias.complete(p, NodeKind::SdblAlias);
            }

            if !p.at(T![Comma]) {
                continue;
            }
            p.bump();
        } else {
            break;
        }
    }

    if !at_sdbl_keyword(p, "BY", "ПО") {
        m.complete(p, NodeKind::SdblTotalsBy);
        return;
    }
    eat_sdbl_keyword(p, "BY", "ПО");

    totals_group_item(p);

    while p.eat(T![Comma]) {
        p.check_iteration_limit();
        totals_group_item(p);
    }

    m.complete(p, NodeKind::SdblTotalsBy);
}

// =====================================================================
// CLEAN-ROOM Slice 12 — totals control-point modifiers
//
// A control point is an expression, then an optional `[ТОЛЬКО]
// ИЕРАРХИЯ` or `ПЕРИОДАМИ(<вид периода> [, <дата>] [, <дата>])`, then an
// optional alias. Modifiers are consumed as tokens of the enclosing
// `SdblTotalsBy` rather than promoted into nodes of their own, so the
// shape `crates/sdbl-hir` reads is unchanged; interpreting them is
// Slice 13's.
//
// Provenance: `docs/legal/sdbl-clean-room-slice12.md`, entries D2–D3.
// =====================================================================

/// The ten period names, in both spellings, as the rule enumerates them.
const PERIOD_NAMES: &[(&str, &str)] = &[
    ("SECOND", "СЕКУНДА"),
    ("MINUTE", "МИНУТА"),
    ("HOUR", "ЧАС"),
    ("DAY", "ДЕНЬ"),
    ("WEEK", "НЕДЕЛЯ"),
    ("MONTH", "МЕСЯЦ"),
    ("QUARTER", "КВАРТАЛ"),
    ("YEAR", "ГОД"),
    ("TENDAYS", "ДЕКАДА"),
    ("HALFYEAR", "ПОЛУГОДИЕ"),
];

fn at_period_name(p: &Parser) -> bool {
    PERIOD_NAMES.iter().any(|(en, ru)| at_sdbl_keyword(p, en, ru))
}

/// `<Литерал типа DATE> | <Идентификатор параметра>` — what the rule allows
/// a period boundary to be.
fn at_period_boundary(p: &Parser) -> bool {
    // A parameter arrives as one token, name included, so `Ampersand` here
    // means a whole `&Имя` rather than the sigil alone.
    p.at(T![Ampersand])
        || p.at(T![Date])
        || (at_sdbl_keyword(p, "DATETIME", "ДАТАВРЕМЯ") && p.nth(1) == Some(T![LParen]))
}

fn at_periods_keyword(p: &Parser) -> bool {
    at_sdbl_keyword(p, "PERIODS", "ПЕРИОДАМИ")
}

fn totals_group_item(p: &mut Parser) {
    super::expressions::expression(p);

    totals_group_modifier(p);
    totals_group_alias(p);
}

/// `[[ТОЛЬКО] ИЕРАРХИЯ] | [ПЕРИОДАМИ(…)]`.
///
/// The rule offers these as alternatives, not as a sequence, so taking
/// one rules out the other. A control point carrying both is reported and
/// then consumed anyway: the reading is wrong, but dropping the text
/// would be worse.
fn totals_group_modifier(p: &mut Parser) {
    let took_hierarchy = if at_sdbl_keyword(p, "ONLY", "ТОЛЬКО") {
        p.bump();

        if at_sdbl_keyword(p, "HIERARCHY", "ИЕРАРХИЯ") {
            p.bump();
            true
        } else {
            p.error_custom_no_bump("ожидалось 'ИЕРАРХИЯ' / 'HIERARCHY' после 'ТОЛЬКО' / 'ONLY'");
            // The alternative was never taken, so what follows cannot
            // conflict with it. Saying otherwise would be a second message
            // about a word that is not there.
            false
        }
    } else if at_sdbl_keyword(p, "HIERARCHY", "ИЕРАРХИЯ") {
        p.bump();
        true
    } else {
        false
    };

    if at_periods_keyword(p) {
        if took_hierarchy {
            p.error_custom_no_bump(
                "'ИЕРАРХИЯ' / 'HIERARCHY' и 'ПЕРИОДАМИ' / 'PERIODS' исключают друг друга",
            );
        }
        totals_periods_modifier(p);
    }
}

/// `ПЕРИОДАМИ(<вид периода> [, <граница>] [, <граница>])`.
///
/// Every part the rule marks mandatory is reported when it is missing —
/// but reported without moving, so the clause that follows still gets
/// parsed. The period name comes from a closed list of ten words; the
/// boundaries are a date literal or a parameter, taken as expressions so
/// that their structure survives for the semantic layer.
fn totals_periods_modifier(p: &mut Parser) {
    p.bump();

    if !p.at(T![LParen]) {
        if !p.at_end() {
            p.error_custom_no_bump("ожидалась '(' после 'ПЕРИОДАМИ' / 'PERIODS'");
        }
        return;
    }
    p.bump();
    p.within_boundary(expressions::at_list_delimiter, |p| {
        if at_period_name(p) {
            p.bump();
        } else if super::at_name(p) && !is_clause_keyword(p) {
            // A word in the right place that is not one of the ten. Take it,
            // so the modifier still has a shape, and say what is wrong with it.
            p.error_custom_no_bump("неизвестный вид периода");
            p.bump();
        } else {
            p.error_custom_no_bump("ожидался вид периода");
            expressions::recover_to_delimiter(p, false, expressions::at_list_delimiter);
        }

        // The rule allows two boundaries. A third is a mistake the user can
        // see; counting them here would only cost the rest of the clause.
        while p.at(T![Comma]) {
            p.check_iteration_limit();
            p.bump();

            if super::expressions::is_expression_start(p) {
                if !at_period_boundary(p) {
                    p.error_custom_no_bump("границей периода может быть литерал даты или параметр");
                }
                super::expressions::expression(p);
            } else {
                p.error_custom_no_bump("ожидалась граница периода после запятой");
                expressions::recover_to_delimiter(p, false, expressions::at_list_delimiter);
            }
        }

        if p.at(T![RParen]) {
            p.bump();
        } else if !p.at_end() {
            // At end of input this is a query being typed and says nothing;
            // with a clause still to come the paren is simply unclosed.
            p.error_custom_no_bump("не закрыта скобка после 'ПЕРИОДАМИ' / 'PERIODS'");
        }
    });
}

/// The trailing `[[КАК] <Псевдоним поля>]` of a control point.
///
/// Written out, `КАК` commits to a name and its absence is reported. A
/// bare alias cannot be told from a following clause by anything but the
/// spelling, so that form keeps the wide guard; an explicit one uses the
/// same narrow guard as the other explicit aliases in this grammar,
/// because a name that spells a keyword is still a name.
fn totals_group_alias(p: &mut Parser) {
    if at_sdbl_keyword(p, "AS", "КАК") {
        eat_sdbl_keyword(p, "AS", "КАК");

        // After an explicit `КАК` nothing but a name can stand, so the
        // boundary's guess from the word alone does not apply here — the
        // same as for a field alias and a source alias.
        if is_body_clause_keyword(p) {
            p.error_custom_no_bump("ожидался псевдоним после 'КАК' / 'AS'");
        } else {
            super::eat_name_here(p, "ожидался псевдоним после 'КАК' / 'AS'");
        }
        return;
    }

    if super::at_name(p) && !is_clause_keyword(p) {
        p.bump();
    }
}

fn is_identifier_token(p: &Parser) -> bool {
    super::at_name(p)
}

fn is_limitation_keyword(p: &Parser) -> bool {
    at_sdbl_keyword(p, "DISTINCT", "РАЗЛИЧНЫЕ")
        || at_sdbl_keyword(p, "TOP", "ПЕРВЫЕ")
        || at_sdbl_keyword(p, "ALLOWED", "РАЗРЕШЕННЫЕ")
}

fn limitations(p: &mut Parser) {
    let m = p.start();

    while is_limitation_keyword(p) {
        if at_sdbl_keyword(p, "DISTINCT", "РАЗЛИЧНЫЕ") {
            eat_sdbl_keyword(p, "DISTINCT", "РАЗЛИЧНЫЕ");
        } else if at_sdbl_keyword(p, "TOP", "ПЕРВЫЕ") {
            top_clause(p);
        } else if at_sdbl_keyword(p, "ALLOWED", "РАЗРЕШЕННЫЕ") {
            eat_sdbl_keyword(p, "ALLOWED", "РАЗРЕШЕННЫЕ");
        }
    }

    m.complete(p, NodeKind::SdblLimitations);
}

fn top_clause(p: &mut Parser) {
    let m = p.start();

    eat_sdbl_keyword(p, "TOP", "ПЕРВЫЕ");

    // `expect` reports by bumping the offending token, which would eat the
    // next clause's keyword and cost that clause its parse. Say the same
    // thing without moving when the count is simply not there.
    if is_clause_keyword(p) || p.at(T![Semicolon]) || p.at(T![RParen]) || p.at_end() {
        p.error_custom_no_bump("ожидалось количество после 'ПЕРВЫЕ' / 'TOP'");
    } else {
        p.expect(T![Decimal]);
    }

    m.complete(p, NodeKind::SdblTopClause);
}

fn virtual_table_args(p: &mut Parser) {
    if !p.at(T![LParen]) {
        return;
    }
    p.bump();
    p.within_boundary(expressions::at_list_delimiter, virtual_table_args_body);
}

fn virtual_table_args_body(p: &mut Parser) {
    eat_query_extensions(p);

    if p.at(T![RParen]) {
        p.expect(T![RParen]);
        return;
    }

    if super::expressions::is_expression_start(p) && !p.at(T![Comma]) {
        let checkpoint = p.error_checkpoint();
        super::expressions::expression(p);
        eat_query_extensions(p);
        if !p.at(T![Comma]) && !p.at(T![RParen]) {
            expressions::recover_expression_tail(p, checkpoint);
        }
    } else {
        let m = p.start();
        m.complete(p, NodeKind::SdblMissingArg);
    }

    while p.eat(T![Comma]) {
        p.check_iteration_limit();
        eat_query_extensions(p);

        let empty_slot =
            p.at(T![Comma]) || p.at(T![RParen]) || !super::expressions::is_expression_start(p);
        if empty_slot {
            let m = p.start();
            m.complete(p, NodeKind::SdblMissingArg);
            if !p.at(T![Comma]) {
                break;
            }
            continue;
        }

        let checkpoint = p.error_checkpoint();
        super::expressions::expression(p);
        eat_query_extensions(p);
        if !p.at(T![Comma]) && !p.at(T![RParen]) {
            expressions::recover_expression_tail(p, checkpoint);
        }
    }

    if p.at(T![RParen]) {
        p.bump();
    } else if is_clause_keyword(p) {
        p.error_expected(T![RParen]);
    } else {
        p.expect(T![RParen]);
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_sdbl;

    #[test]
    fn test_error_recovery_incomplete_field_list() {
        let input = r#"ВЫБРАТЬ
    Очередь.
ИЗ
    РегистрСведений.ОчередьОбновленияКэширующихДанных КАК Очередь"#;

        let parse = parse_sdbl(input);
        let tree_text = format!("{:#?}", parse.syntax_node());

        assert!(
            tree_text.contains("ERROR"),
            "Expected ERROR node for incomplete field.\nTree: {}",
            tree_text
        );

        assert!(
            tree_text.contains("SDBL_FROM_CLAUSE"),
            "FROM clause should be parsed despite incomplete field list.\nTree: {}",
            tree_text
        );

        assert!(
            tree_text.contains("SDBL_DATA_SOURCE"),
            "Data source should be in FROM clause.\nTree: {}",
            tree_text
        );
    }

    #[test]
    fn test_error_recovery_complete_query_after_incomplete_field() {
        let input = r#"ВЫБРАТЬ
    Очередь.
ИЗ
    РегистрСведений.Тест КАК Очередь
ГДЕ
    Очередь.Попыток < 3"#;

        let parse = parse_sdbl(input);
        let text = format!("{:#?}", parse.syntax_node());

        assert!(
            text.contains("ERROR"),
            "Expected ERROR node for incomplete field.\nTree: {}",
            text
        );

        assert!(text.contains("SDBL_FROM_CLAUSE"), "FROM clause should be parsed.\nTree: {}", text);

        assert!(
            text.contains("SDBL_WHERE_CLAUSE"),
            "WHERE clause should be parsed.\nTree: {}",
            text
        );
    }

    #[test]
    fn test_error_recovery_incomplete_field_in_middle_of_list() {
        let input = r#"ВЫБРАТЬ ПЕРВЫЕ 500
    Очередь.,
    Очередь.ЗависимыйОбъектМетаданных КАК ЗависимыйОбъектМетаданных
ИЗ
    РегистрСведений.ОчередьОбновленияКэширующихДанных КАК Очередь
ГДЕ
    Очередь.Попыток < 3"#;

        let parse = parse_sdbl(input);
        let text = format!("{:#?}", parse.syntax_node());

        assert!(
            text.contains("ERROR"),
            "Expected ERROR node for incomplete field.\nTree: {}",
            text
        );

        assert!(
            text.contains("SDBL_FROM_CLAUSE"),
            "FROM clause should be parsed despite incomplete field in middle of list.\nTree: {}",
            text
        );

        assert!(
            text.contains("SDBL_DATA_SOURCE"),
            "Data source should be in FROM clause.\nTree: {}",
            text
        );

        assert!(
            text.contains("SDBL_WHERE_CLAUSE"),
            "WHERE clause should be parsed.\nTree: {}",
            text
        );

        let field_count = text.matches("SDBL_SELECTED_FIELD").count();
        assert!(
            field_count >= 2,
            "Should have at least 2 selected fields (incomplete + complete). Got: {}",
            field_count
        );
    }

    #[test]
    fn test_tuple_in_in_predicate() {
        let input = r#"ВЫБРАТЬ *
ИЗ Документ.Заказ КАК Заказ
ГДЕ (Заказ.Партнер, Заказ.Контрагент, Заказ.Организация) В
    (ВЫБРАТЬ Т.Партнер, Т.Контрагент, Т.Организация ИЗ ВТ_Данные КАК Т)"#;

        let parse = parse_sdbl(input);
        let text = format!("{:#?}", parse.syntax_node());

        assert!(
            text.contains("SDBL_TUPLE_EXPR"),
            "Expected SDBL_TUPLE_EXPR for tuple in IN predicate.\nTree: {}",
            text
        );

        assert!(text.contains("SDBL_IN_EXPR"), "Expected SDBL_IN_EXPR.\nTree: {}", text);

        assert!(
            text.contains("SDBL_SUBQUERY"),
            "Expected subquery inside IN predicate.\nTree: {}",
            text
        );

        assert!(!parse.has_errors(), "Should parse without errors: {:?}", parse.errors());
    }

    #[test]
    fn test_simple_tuple() {
        let input = r#"ВЫБРАТЬ * ИЗ Т ГДЕ (А, Б, Г) = (1, 2, 3)"#;

        let parse = parse_sdbl(input);
        let text = format!("{:#?}", parse.syntax_node());

        let tuple_count = text.matches("SDBL_TUPLE_EXPR").count();
        assert!(
            tuple_count >= 2,
            "Expected at least 2 SDBL_TUPLE_EXPR nodes. Got: {}\nTree: {}",
            tuple_count,
            text
        );
    }

    #[test]
    fn test_paren_expr_not_tuple() {
        let input = r#"ВЫБРАТЬ (А + Б) КАК Сумма ИЗ Т"#;

        let parse = parse_sdbl(input);
        let text = format!("{:#?}", parse.syntax_node());

        assert!(
            !text.contains("SDBL_TUPLE_EXPR"),
            "Single parenthesized expression should not create SDBL_TUPLE_EXPR.\nTree: {}",
            text
        );

        assert!(
            text.contains("SDBL_PAREN_EXPR"),
            "Expected SDBL_PAREN_EXPR for single parenthesized expression.\nTree: {}",
            text
        );
    }

    #[test]
    fn test_tuple_in_virtual_table_params() {
        let input = r#"ВЫБРАТЬ *
ИЗ РегистрНакопления.Расчеты.Обороты(
    &Начало,
    &Конец,
    ,
    (Аналитика.Партнер, Аналитика.Контрагент) В
        (ВЫБРАТЬ Т.Партнер, Т.Контрагент ИЗ ВТ_Данные КАК Т)
) КАК Обороты"#;

        let parse = parse_sdbl(input);
        let text = format!("{:#?}", parse.syntax_node());

        assert!(
            text.contains("SDBL_TUPLE_EXPR"),
            "Expected SDBL_TUPLE_EXPR in virtual table params.\nTree: {}",
            text
        );

        assert!(text.contains("SDBL_TABLE_REF"), "Expected SDBL_TABLE_REF.\nTree: {}", text);
    }
}
