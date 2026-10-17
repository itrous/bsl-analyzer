use crate::event::NodeKind;
use crate::parser::Parser;

pub(super) fn is_expression_start(p: &Parser) -> bool {
    if at_a_keyword_qualifier(p) {
        return true;
    }

    match p.current() {
        Some(T![Decimal])
        | Some(T![Float])
        | Some(T![String])
        | Some(T![Date])
        | Some(T![KwTrue])
        | Some(T![KwFalse])
        | Some(T![KwUndefined]) => true,

        // The word that begins the next query begins no expression. Saying
        // otherwise sends a rule that will refuse it a token it must not
        // take, and a loop that expects an expression to be consumed then
        // has nothing to consume and no reason to stop.
        Some(T![Ident]) => !super::select::is_clause_keyword(p) && !super::at_query_boundary(p),

        Some(T![Plus]) | Some(T![Minus]) | Some(T![KwNot]) | Some(T![Star]) => true,

        Some(T![LParen]) => true,

        Some(T![Ampersand]) => true,

        _ => p.at_keyword("CASE") || p.at_keyword("ВЫБОР") || p.at_keyword("NULL"),
    }
}

/// `И` / `ИЛИ` set apart from the dot before it is the connective that follows an
/// unfinished reference, not a member name: the condition goes on after it. A member
/// that happens to be spelled like a connective is written against its dot (`Т.И`).
fn at_connective_after_dangling_dot(p: &Parser) -> bool {
    (p.at(T![KwAnd]) || p.at(T![KwOr])) && p.a_gap_precedes()
}

pub(super) fn at_property_name(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(
            T![Ident]
                | T![KwIn]
                | T![KwAnd]
                | T![KwOr]
                | T![KwNot]
                | T![KwTrue]
                | T![KwFalse]
                | T![KwUndefined]
        )
    )
}

// =====================================================================
// CLEAN-ROOM Slice 12 — expression-level recovery
//
// None of this is the query language: the official grammar has no
// opinion on malformed input. These helpers exist so that one bad
// expression does not cost the editor the rest of the query, and each is
// justified on that ground alone.
//
// Provenance: `docs/legal/sdbl-clean-room-slice12.md`, entries A8–A9.
// =====================================================================

fn is_recovery_point(p: &Parser, recovery_set: &crate::parser::token_set::TokenSet) -> bool {
    if let Some(kind) = p.current() {
        if recovery_set.contains(kind) {
            return true;
        }
    }

    if super::select::is_clause_keyword(p) {
        return true;
    }

    p.at_end()
}

pub(super) fn at_closing_paren(p: &Parser) -> bool {
    p.at(T![RParen])
}

pub(super) fn at_list_delimiter(p: &Parser) -> bool {
    p.at(T![Comma]) || p.at(T![RParen])
}

pub(super) fn at_comma(p: &Parser) -> bool {
    p.at(T![Comma])
}

fn closing_paren_ahead(p: &Parser) -> bool {
    let mut parens = 0;
    let mut braces = 0;
    let mut offset = 0;
    while let Some(kind) = p.nth(offset) {
        match kind {
            T![Semicolon] => return false,
            T![LBrace] => braces += 1,
            T![RBrace] => braces = (braces - 1).max(0),
            T![LParen] if braces == 0 => parens += 1,
            T![RParen] if braces == 0 => {
                if parens == 0 {
                    return true;
                }
                parens -= 1;
            }
            _ => {}
        }
        offset += 1;
    }
    false
}

pub(super) fn recover_to_delimiter(
    p: &mut Parser,
    report: bool,
    at_delimiter: fn(&Parser) -> bool,
) {
    let has_closer = closing_paren_ahead(p);
    let err = p.start();
    let mut consumed_any = false;
    let mut paren_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut nested_query_starts: Vec<i32> = Vec::new();

    loop {
        p.check_iteration_limit();

        if p.at(T![LBrace]) {
            brace_depth += 1;
            p.bump();
            consumed_any = true;
            continue;
        }

        if p.at(T![RBrace]) {
            if brace_depth > 0 {
                brace_depth -= 1;
            }
            p.bump();
            consumed_any = true;
            continue;
        }

        // An extension region is opaque text, so the parser's own group
        // count ignores parens inside one. This scan has to agree, or its
        // idea of which `)` closes the caller's `(` drifts from the
        // parser's and it walks out through a boundary that is still open.
        if brace_depth > 0 {
            if p.at_end() || p.at(T![Semicolon]) {
                break;
            }
            p.bump();
            consumed_any = true;
            continue;
        }

        if p.at(T![LParen]) {
            paren_depth += 1;
            p.bump();
            consumed_any = true;
            if super::select::is_query_starter_or_combiner_keyword(p) {
                nested_query_starts.push(paren_depth);
            }
            continue;
        }

        if p.at(T![RParen]) {
            if paren_depth > 0 {
                if let Some(&d) = nested_query_starts.last() {
                    if d == paren_depth {
                        nested_query_starts.pop();
                    }
                }
                paren_depth -= 1;
                p.bump();
                consumed_any = true;
                continue;
            } else {
                break;
            }
        }

        let inside_nested_query = !nested_query_starts.is_empty();

        if paren_depth == 0 && brace_depth == 0 && super::at_query_boundary(p) {
            break;
        }

        // After a qualifying dot the word is a field name, not this query's
        // clause; the drain reads it that way and a skip that does not
        // hands the enclosing query a clause it never had.
        if brace_depth == 0
            && super::select::is_clause_keyword(p)
            && p.prev_significant() != Some(T![Dot])
        {
            let stop = if paren_depth == 0 {
                // A lone keyword before a delimiter is the bad operand.
                // With a body after it, the clause still belongs to the query.
                !has_closer || !matches!(p.nth(1), Some(T![Comma] | T![RParen]))
            } else if inside_nested_query {
                false
            } else {
                !super::select::is_query_starter_or_combiner_keyword(p)
            };
            if stop {
                break;
            }
        }

        // A separator ends the skipped fragment at any depth: it is the
        // boundary between package members, and no depth of ours outranks it.
        if p.at(T![Semicolon]) {
            break;
        }

        if paren_depth == 0
            && brace_depth == 0
            && p.at(T![Comma])
            && (at_delimiter(p) || !has_closer)
        {
            break;
        }

        if p.at_end() {
            break;
        }

        p.bump();
        consumed_any = true;
    }

    if consumed_any {
        if report {
            p.error_custom_at_marker(err, "пропуск некорректного фрагмента");
        } else {
            err.complete(p, NodeKind::SdblError);
        }
    } else {
        err.abandon(p);
    }
}

use crate::Sig;

pub(super) fn parse_delimited_list<F>(
    p: &mut Parser,
    delimiter: Sig,
    recovery_set: &crate::parser::token_set::TokenSet,
    is_item_start: fn(&Parser) -> bool,
    mut parse_item: F,
) where
    F: FnMut(&mut Parser),
{
    // A list starting on its delimiter is missing its first item, not
    // holding one that happens to be a comma. Handing the delimiter to the
    // item rule costs the list the item that follows it, which the rule
    // reads as that one's alias.
    if p.at(delimiter) {
        let err = p.start();
        p.error_custom_at_marker(err, "пропущен элемент списка");
    } else {
        p.within_boundary(at_comma, |p| parse_item(p));
    }

    loop {
        if is_recovery_point(p, recovery_set) {
            break;
        }

        if !p.eat(delimiter) {
            break;
        }

        p.check_iteration_limit();

        if p.at(delimiter) || is_recovery_point(p, recovery_set) || !is_item_start(p) {
            let err = p.start();
            p.error_custom_at_marker(err, "пропущен элемент списка");

            if !p.at(delimiter) {
                break;
            }
            continue;
        }

        p.within_boundary(at_comma, |p| parse_item(p));
    }
}

pub fn logical_expression(p: &mut Parser) {
    logical_or_expr(p);
}

pub fn expression(p: &mut Parser) {
    logical_or_expr(p);
}

fn logical_or_expr(p: &mut Parser) {
    let m = p.start();

    logical_and_expr(p);

    loop {
        if p.at(T![KwOr]) {
            p.check_iteration_limit();
            p.bump();
            logical_and_expr(p);
        } else {
            break;
        }
    }

    m.complete(p, NodeKind::SdblLogicalOrExpr);
}

fn logical_and_expr(p: &mut Parser) {
    let m = p.start();

    not_expr(p);

    loop {
        if p.at(T![KwAnd]) {
            p.check_iteration_limit();
            p.bump();
            not_expr(p);
        } else {
            break;
        }
    }

    m.complete(p, NodeKind::SdblLogicalAndExpr);
}

fn not_expr(p: &mut Parser) {
    if p.at(T![KwNot]) && !at_a_keyword_qualifier(p) {
        let m = p.start();
        p.bump();
        not_expr(p);
        m.complete(p, NodeKind::SdblNotExpr);
    } else {
        comparison_expr(p);
    }
}

fn additive_expr(p: &mut Parser) {
    let m = p.start();

    multiplicative_expr(p);

    loop {
        if !matches!(p.current(), Some(T![Plus]) | Some(T![Minus])) {
            break;
        }
        p.check_iteration_limit();
        p.bump();
        multiplicative_expr(p);
    }

    m.complete(p, NodeKind::SdblAdditiveExpr);
}

fn multiplicative_expr(p: &mut Parser) {
    let m = p.start();

    unary_expr(p);

    loop {
        if !matches!(p.current(), Some(T![Star]) | Some(T![Slash]) | Some(T![Percent])) {
            break;
        }
        p.check_iteration_limit();
        p.bump();
        unary_expr(p);
    }

    m.complete(p, NodeKind::SdblMultiplicativeExpr);
}

fn unary_expr(p: &mut Parser) {
    if matches!(p.current(), Some(T![Plus]) | Some(T![Minus]) | Some(T![KwNot]))
        && !at_a_keyword_qualifier(p)
    {
        let m = p.start();
        p.bump();
        unary_expr(p);
        m.complete(p, NodeKind::SdblUnaryExpr);
    } else {
        primary_expr(p);
    }
}

fn primary_expr(p: &mut Parser) {
    if super::select::at_sdbl_keyword(p, "AS", "КАК") && !next_is_a_qualifying_dot(p) {
        p.error_custom_no_bump("ожидался операнд перед 'КАК' / 'AS'");
        return;
    }
    if at_a_keyword_qualifier(p) {
        column_or_function(p);
        return;
    }

    if p.at_keyword("CASE") || p.at_keyword("ВЫБОР") {
        case_expr(p);
        return;
    }

    if p.at_keyword("NULL") {
        let m = p.start();
        p.bump();
        m.complete(p, NodeKind::SdblLiteral);
        return;
    }

    match p.current() {
        Some(T![LParen]) => paren_or_subquery_expr(p),
        Some(T![Decimal]) | Some(T![Float]) => literal_expr(p),
        Some(T![String]) => literal_expr(p),
        // The lexer has taken `'ГГГГММДД[ЧЧММСС]'` as one token since the
        // token set was re-derived; nothing here ever accepted it, so a
        // whole class of literal was an error wherever it stood.
        Some(T![Date]) => literal_expr(p),
        Some(T![KwTrue]) | Some(T![KwFalse]) => literal_expr(p),
        Some(T![KwUndefined]) => literal_expr(p),
        Some(T![Ampersand]) => parameter_expr(p),

        Some(T![Star]) => {
            let m = p.start();
            p.bump();
            m.complete(p, NodeKind::SdblLiteral);
        }

        // A query keyword is never a column name. Where nothing is open to
        // make it a subquery, it begins the package's next member, and an
        // operand missing in front of it has to be reported at the gap: the
        // boundary check in `error` then leaves the keyword where it is.
        // Taking it instead would cost the package that whole member — and
        // an operand left unwritten is exactly what an editor buffer holds.
        Some(T![Ident])
            if (p.open_group_count() > 0 || !super::at_query_boundary(p))
                && !at_a_keyword_that_cannot_be_a_field(p) =>
        {
            column_or_function(p)
        }

        // A keyword reaching here is an operand the text never wrote. What is
        // missing is an operand and not a field name: a literal, a parameter,
        // a call or a parenthesised expression would all have done, and naming
        // one of them would name the wrong thing. The recovery choice stays
        // the generic one, so a word no rule awaits is still consumed.
        Some(T![Ident]) => {
            let m = p.start();
            p.error_custom("ожидался операнд, встречено ключевое слово");
            m.complete(p, NodeKind::SdblError);
        }

        _ => {
            let m = p.start();
            p.error_unexpected();
            m.complete(p, NodeKind::SdblError);
        }
    }
}

/// A clause keyword standing where only a field name can stand.
///
/// A bare name here is a field, and the source closes that position to
/// keywords: «Имена таблиц и полей не могут совпадать с ключевыми словами
/// языка запросов». A name carrying a dot is a different thing — the
/// qualifier of a chain, which may be the alias of a source, and an alias is
/// allowed to spell a keyword. `ПО Зак.Ссылка = Итоги.Регистратор` reads a
/// source aliased `КАК Итоги`, and refusing the word would cost that join its
/// condition.
/// A clause keyword standing where only a field name can stand.
///
fn at_a_keyword_that_cannot_be_a_field(p: &Parser) -> bool {
    p.names_are_fields()
        && (super::select::is_clause_keyword(p) || super::at_a_list_separator(p))
        && !next_is_a_qualifying_dot(p)
}

/// An operator or literal word standing in front of a dot: `В.Ссылка`,
/// `НЕ.Ссылка`, `ВЫБОР.Ссылка`.
///
/// None of these words takes a dot as an operator or a literal, so the dot makes
/// the word the qualifier of a chain — the alias of a source spelled with a
/// reserved word. The alias is refused where it is declared; reading its uses
/// as chains keeps that one finding from being followed by a second one at
/// every reference.
fn at_a_keyword_qualifier(p: &Parser) -> bool {
    let is_keyword = matches!(
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
    ) || p.at_keyword("CASE")
        || p.at_keyword("ВЫБОР")
        || p.at_keyword("NULL");

    is_keyword && next_is_a_qualifying_dot(p)
}

fn next_is_a_qualifying_dot(p: &Parser) -> bool {
    matches!(p.nth(1), Some(T![Dot]))
}

fn literal_expr(p: &mut Parser) {
    if p.at(T![String]) {
        string_literal_or_multi(p);
    } else {
        let m = p.start();
        p.bump();
        m.complete(p, NodeKind::SdblLiteral);
    }
}

fn string_literal_or_multi(p: &mut Parser) {
    let m = p.start();

    p.bump();

    // Лексер режет одну строку языка на открывающую кавычку, содержимое и
    // закрывающую — три токена вплотную. Склеиваются именно они, поэтому
    // соседство здесь спрашивается прямо: строка, отделённая хоть пробелом, —
    // уже другая строка, и своего узла лишаться не должна.
    let mut count = 1;
    while p.at(T![String]) && !p.a_gap_precedes() {
        p.bump();
        count += 1;
    }

    if count > 1 {
        m.complete(p, NodeKind::SdblMultiString);
    } else {
        m.complete(p, NodeKind::SdblLiteral);
    }
}

/// Имя параметра приходит внутри самого токена: лексер берёт `&Имя` целиком,
/// а одинокий `&` остаётся только там, где за ним не буква. Отдельного `Ident`
/// после амперсанда не бывает, и брать его нельзя — на `&\nПО` это забрало бы
/// у соединения его же ключевое слово.
fn parameter_expr(p: &mut Parser) {
    let m = p.start();
    p.bump();
    m.complete(p, NodeKind::SdblParameter);
}

fn paren_or_subquery_expr(p: &mut Parser) {
    let m = p.start();

    p.bump();
    let mut checkpoint = p.error_checkpoint();

    let kind = p.within_boundary(at_closing_paren, |p| {
        if p.at_keyword("SELECT") || p.at_keyword("ВЫБРАТЬ") {
            super::select::subquery(p);
            finish_parenthesized(p, checkpoint);
            NodeKind::SdblSubqueryExpr
        } else {
            p.within_boundary(at_list_delimiter, expression);

            if p.at(T![Comma]) {
                while p.eat(T![Comma]) {
                    p.check_iteration_limit();
                    checkpoint = p.error_checkpoint();

                    if p.at(T![RParen]) {
                        break;
                    }
                    if !is_expression_start(p) {
                        p.error_custom_no_bump("ожидался элемент кортежа после запятой");
                        break;
                    }

                    p.within_boundary(at_list_delimiter, expression);
                }

                finish_parenthesized(p, checkpoint);
                NodeKind::SdblTupleExpr
            } else {
                finish_parenthesized(p, checkpoint);
                NodeKind::SdblParenExpr
            }
        }
    });
    m.complete(p, kind);
}

pub(super) fn finish_parenthesized(p: &mut Parser, checkpoint: usize) {
    if !p.at(T![RParen]) {
        recover_expression_tail(p, checkpoint);
    }
    p.expect(T![RParen]);
}

pub(super) fn recover_expression_tail(p: &mut Parser, checkpoint: usize) {
    let report = !p.has_error_since(checkpoint);
    recover_to_delimiter(p, report, at_list_delimiter);
}

fn comparison_expr(p: &mut Parser) {
    predicate_expr(p);
}

fn predicate_expr(p: &mut Parser) {
    let m = p.start();

    additive_expr(p);

    if p.at(T![KwNot]) {
        p.bump();
    }

    if p.at(T![KwIn]) {
        p.bump();

        if p.at_keyword("HIERARCHY") || p.at_keyword("ИЕРАРХИИ") {
            p.bump();

            if !p.expect(T![LParen]) {
                m.complete(p, NodeKind::SdblInHierarchyExpr);
                return;
            }
            let checkpoint = p.error_checkpoint();

            p.within_boundary(at_closing_paren, |p| {
                if p.at_keyword("SELECT") || p.at_keyword("ВЫБРАТЬ") {
                    super::select::subquery(p);
                } else {
                    expression(p);
                }

                finish_parenthesized(p, checkpoint);
            });
            m.complete(p, NodeKind::SdblInHierarchyExpr);
        } else {
            if !p.expect(T![LParen]) {
                m.complete(p, NodeKind::SdblInExpr);
                return;
            }
            let mut checkpoint = p.error_checkpoint();

            p.within_boundary(at_closing_paren, |p| {
                if p.at_keyword("SELECT") || p.at_keyword("ВЫБРАТЬ") {
                    super::select::subquery(p);
                } else {
                    parse_delimited_list(
                        p,
                        T![Comma],
                        &super::LIST_RECOVERY,
                        is_expression_start,
                        |p| {
                            checkpoint = p.error_checkpoint();
                            expression(p);
                        },
                    );
                }

                finish_parenthesized(p, checkpoint);
            });
            m.complete(p, NodeKind::SdblInExpr);
        }
    } else if p.at_keyword("IS") || p.at_keyword("ЕСТЬ") {
        p.bump();

        if p.at(T![KwNot]) {
            p.bump();
        }

        if !p.at_keyword("NULL") {
            m.abandon(p);
            return;
        }
        p.bump();

        m.complete(p, NodeKind::SdblIsNullExpr);
    } else if p.at_keyword("BETWEEN") || p.at_keyword("МЕЖДУ") {
        p.bump();

        additive_expr(p);

        if !p.at(T![KwAnd]) {
            m.complete(p, NodeKind::SdblBetweenExpr);
            return;
        }
        p.bump();

        additive_expr(p);

        m.complete(p, NodeKind::SdblBetweenExpr);
    } else if p.at_keyword("LIKE") || p.at_keyword("ПОДОБНО") {
        p.bump();

        additive_expr(p);

        if p.at_keyword("ESCAPE") || p.at_keyword("СПЕЦСИМВОЛ") {
            p.bump();
            additive_expr(p);
        }

        m.complete(p, NodeKind::SdblLikeExpr);
    } else if p.at_keyword("REFS") || p.at_keyword("ССЫЛКА") {
        p.bump();

        if super::at_field_name(p) {
            p.bump();

            while super::eat_qualifying_dot(p) {
                p.check_iteration_limit();
                if !super::at_table_name_component(p) {
                    // A word stands here and cannot be part of the name: say
                    // so, or it is taken in silence. Nothing standing here at
                    // all is the unfinished path of a query built by
                    // concatenation, which `QueryParseError` already reports —
                    // and reporting it twice, from a layer that does not know
                    // it is a fragment, is worse than not reporting it here.
                    if at_property_name(p) {
                        report_missing_name(p, "ожидалось имя объекта после '.'");
                    }
                    break;
                }
                p.bump();
            }
        } else {
            // The source is explicit that what stands here is a table:
            // «проверяется, является ли значение выражения, указанного слева
            // от него, ссылкой на таблицу, указанную справа».
            report_missing_name(p, "ожидалось имя таблицы после 'ССЫЛКА' / 'REFS'");
        }

        m.complete(p, NodeKind::SdblRefsExpr);
    } else if matches!(
        p.current(),
        Some(T![Eq]) | Some(T![Neq]) | Some(T![Lt]) | Some(T![Le]) | Some(T![Gt]) | Some(T![Ge])
    ) {
        p.bump();
        additive_expr(p);
        m.complete(p, NodeKind::SdblComparisonExpr);
    } else {
        m.abandon(p);
    }
}

fn is_cast_function(p: &Parser) -> bool {
    p.at_keyword("CAST") || p.at_keyword("ВЫРАЗИТЬ")
}

fn parse_cast_type(p: &mut Parser) {
    let m = p.start();

    if super::at_field_name(p) {
        let is_number_type = p.at_keyword("NUMBER") || p.at_keyword("ЧИСЛО");

        p.bump();

        while super::eat_qualifying_dot(p) {
            p.check_iteration_limit();

            if !super::at_table_name_component(p) {
                // Saying «expected an identifier» of a word that IS one reads
                // as nonsense; what is wrong with it is that it is a keyword.
                if at_property_name(p) {
                    report_missing_name(p, "ожидалось имя объекта после '.'");
                } else {
                    p.error_expected(T![Ident]);
                }
                break;
            }

            p.bump();
        }

        if p.at(T![LParen]) {
            p.bump();
            let checkpoint = p.error_checkpoint();
            p.within_boundary(
                if is_number_type { at_list_delimiter } else { at_closing_paren },
                |p| {
                    if p.at(T![Decimal]) {
                        p.bump();

                        if is_number_type && p.eat(T![Comma]) && p.at(T![Decimal]) {
                            p.bump();
                        }
                    }

                    if !is_number_type && !p.at(T![RParen]) {
                        recover_to_delimiter(p, true, at_closing_paren);
                    }
                    finish_parenthesized(p, checkpoint);
                },
            );
        }
    } else {
        report_missing_name(p, "ожидался тип после 'КАК' / 'AS'");
        recover_to_delimiter(p, false, at_list_delimiter);
    }

    m.complete(p, NodeKind::SdblType);
}

/// Reports a name the text does not hold, at the gap where it belongs.
///
/// A position that requires a name and finds none used to leave the node
/// empty and say nothing, so a query meaning something else parsed clean.
///
/// The message names what the position wants rather than what stands there:
/// a clause keyword is one thing that cannot be a name here, a parameter is
/// another, and a message that names only the first is wrong for the second.
/// The report goes in the gap, because whatever stands here is not this
/// rule's to take.
fn report_missing_name(p: &mut Parser, message: &'static str) {
    let m = p.start();
    p.error_custom_at_marker(m, message);
}

fn column_or_function(p: &mut Parser) {
    let m = p.start();

    let is_cast = is_cast_function(p);

    p.bump();

    if p.at(T![Dot]) || super::at_a_qualifying_dot(p) {
        while super::eat_qualifying_dot(p) {
            let crossed_newline = p.a_line_break_precedes();

            if p.at(T![LParen]) {
                inline_table_fields(p);
                break;
            }

            if at_connective_after_dangling_dot(p) {
                let err = p.start();
                p.error_custom_at_marker(err, "ожидалось имя поля, встречена логическая связка");
                break;
            }

            if !at_property_name(p) {
                p.error_expected(T![Ident]);
                break;
            }

            // A keyword spelled right after a dot is a member name — SDBL keywords are
            // not reserved as field names (`Объект.Конец`, `Объект.Выбор`, `Объект.Итоги`).
            // Dangling-dot recovery is kept narrowly: across a newline any clause start
            // is recovered, and on the same line only the alias separator AS/КАК — which
            // is never a field name — breaks out instead of being swallowed.
            let dangling_dot_recovery = if crossed_newline {
                super::select::is_likely_clause_start_after_dot(p) || super::at_query_boundary(p)
            } else {
                super::select::at_sdbl_keyword(p, "AS", "КАК")
            };
            if dangling_dot_recovery {
                let err = p.start();
                p.error_custom_at_marker(err, "ожидалось имя поля, встречено ключевое слово");
                break;
            }

            p.bump();
        }
        m.complete(p, NodeKind::SdblColumnRef);
    } else if p.at(T![LParen]) {
        p.bump();
        let mut checkpoint = p.error_checkpoint();
        p.within_boundary(at_list_delimiter, |p| {
            if !p.at(T![RParen]) {
                if p.at_keyword("DISTINCT") || p.at_keyword("РАЗЛИЧНЫЕ") {
                    p.bump();
                }

                if is_expression_start(p)
                    && !p.at(T![Comma])
                    && !super::select::is_clause_keyword(p)
                {
                    expression(p);

                    if is_cast && (p.at_keyword("AS") || p.at_keyword("КАК")) {
                        p.bump();
                        parse_cast_type(p);
                    } else {
                        if !p.at(T![Comma]) && !p.at(T![RParen]) {
                            recover_expression_tail(p, checkpoint);
                        }
                    }
                } else if p.at(T![Comma]) {
                    let err = p.start();
                    p.error_custom_at_marker(err, "пропущен первый аргумент");
                } else {
                    p.error_custom_no_bump("ожидался аргумент функции");
                    recover_to_delimiter(p, false, at_list_delimiter);
                }

                while p.eat(T![Comma]) {
                    p.check_iteration_limit();
                    checkpoint = p.error_checkpoint();

                    if p.at(T![Comma])
                        || p.at(T![RParen])
                        || !is_expression_start(p)
                        || super::select::is_clause_keyword(p)
                    {
                        let err = p.start();
                        p.error_custom_at_marker(err, "пропущен аргумент функции");

                        if !p.at(T![Comma]) {
                            break;
                        }
                        continue;
                    }

                    expression(p);

                    if !p.at(T![Comma]) && !p.at(T![RParen]) {
                        recover_expression_tail(p, checkpoint);
                    }
                }
            }

            finish_parenthesized(p, checkpoint);
        });

        while super::eat_qualifying_dot(p) {
            p.check_iteration_limit();
            let crossed_newline = p.a_line_break_precedes();

            if at_connective_after_dangling_dot(p) {
                let err = p.start();
                p.error_custom_at_marker(err, "ожидалось имя поля, встречена логическая связка");
                break;
            }

            if at_property_name(p) {
                // Same-line keyword after a dot is a member name; across a newline a
                // clause keyword is a dangling-dot recovery point. See the column-ref
                // dot loop above.
                if crossed_newline
                    && (super::select::is_clause_keyword(p) || super::at_query_boundary(p))
                {
                    let err = p.start();
                    p.error_custom_at_marker(err, "ожидалось имя поля, встречено ключевое слово");
                    break;
                }

                p.bump();
            } else {
                p.error_expected(T![Ident]);
                break;
            }
        }

        m.complete(p, NodeKind::SdblFunctionCall);
    } else {
        m.complete(p, NodeKind::SdblColumnRef);
    }
}

fn inline_table_fields(p: &mut Parser) {
    let m = p.start();

    p.bump();
    let checkpoint = p.error_checkpoint();

    p.within_boundary(at_closing_paren, |p| {
        super::select::selected_fields(p);
        finish_parenthesized(p, checkpoint);
    });

    m.complete(p, NodeKind::SdblInlineTableFields);
}

fn case_expr(p: &mut Parser) {
    let m = p.start();

    p.bump();

    let is_searched_case = p.at_keyword("WHEN") || p.at_keyword("КОГДА");

    if !is_searched_case {
        expression(p);
    }

    let mut has_when = false;
    while p.at_keyword("WHEN") || p.at_keyword("КОГДА") {
        has_when = true;
        when_clause(p);
    }

    if !has_when {
        p.error_custom("в выражении CASE отсутствует 'КОГДА' / 'WHEN'");
    }

    if p.at_keyword("ELSE") || p.at_keyword("ИНАЧЕ") {
        p.bump();
        expression(p);
    }

    if !p.at_keyword("END") && !p.at_keyword("КОНЕЦ") {
        p.error_custom("ожидалось 'КОНЕЦ' / 'END' в выражении CASE");
    } else {
        p.bump();
    }

    m.complete(p, NodeKind::SdblCaseExpr);
}

fn when_clause(p: &mut Parser) {
    let m = p.start();

    p.bump();

    expression(p);

    if !p.at_keyword("THEN") && !p.at_keyword("ТОГДА") {
        m.complete(p, NodeKind::SdblWhenClause);
        return;
    }
    p.bump();

    expression(p);

    m.complete(p, NodeKind::SdblWhenClause);
}
