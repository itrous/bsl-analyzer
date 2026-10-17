use crate::Sig;

use crate::event::NodeKind;
use crate::parser::token_set::TokenSet;
use crate::parser::Parser;

use super::expressions;

pub fn stmt_list(p: &mut Parser, terminator: Sig) -> bool {
    !stmt_list_of(p, &[terminator], &[terminator]).is_empty()
}

pub fn statement(p: &mut Parser) -> bool {
    if p.at(T![Ident]) {
        return assignment_or_call(p);
    }

    match p.current() {
        Some(T![Semicolon]) => {
            let m = p.start();
            p.bump();
            m.complete(p, NodeKind::EmptyStmt);
        }
        Some(T![KwReturn]) => return_stmt(p),
        Some(T![KwIf]) => if_stmt(p),
        Some(T![KwWhile]) => while_stmt(p),
        Some(T![KwFor]) => for_stmt(p),
        Some(T![KwTry]) => try_stmt(p),
        Some(T![KwRaise]) => raise_stmt(p),
        Some(T![KwBreak]) => {
            let m = p.start();
            p.bump();
            p.eat(T![Semicolon]);
            m.complete(p, NodeKind::BreakStmt);
        }
        Some(T![KwContinue]) => {
            let m = p.start();
            p.bump();
            p.eat(T![Semicolon]);
            m.complete(p, NodeKind::ContinueStmt);
        }
        Some(T![KwGoto]) => goto_stmt(p),
        Some(T![Tilde]) => label_stmt(p),
        Some(T![KwExecute]) => execute_stmt(p),
        Some(T![KwAddHandler]) => add_handler_stmt(p),
        Some(T![KwRemoveHandler]) => remove_handler_stmt(p),
        Some(T![KwVar]) => super::items::var_declaration(p),
        Some(T![PreRegion]) => super::preprocessor_region(p),
        Some(T![PreEndRegion]) => super::preprocessor_end_region(p),
        Some(T![PreIf]) => super::preprocessor_if(p),
        Some(T![PreDelete]) => super::preprocessor_delete(p),
        Some(T![PreInsert]) => super::preprocessor_insert(p),
        // The closer of an insertion that tore apart the statement before this
        // one, when the statement ended inside it — a literal whose closing
        // line was inserted takes its `;` along.
        Some(T![PreEndInsert]) if p.inline_insert_open() => p.bump_inline_insert_marker(),
        _ => {
            if p.current().is_some() {
                let m = p.start();
                expressions::expression(p);
                m.complete(p, NodeKind::CallStmt);
                p.eat(T![Semicolon]);
            }
        }
    }

    false
}

/// Закрыватель после списка, восстановление в котором съело похожее на него слово.
///
/// Опечатка в закрывателе (`КонецЕслли`) разбирается как неизвестный оператор, и требовать
/// после него ещё и пропущенный `КонецЕсли` значит сообщить об одной ошибке дважды.
/// Посторонний оператор такой поблажки не даёт: конструкция после него по-прежнему не
/// закрыта, и сказать об этом больше некому.
pub(super) fn expect_stmt_list_terminator(p: &mut Parser, terminator: Sig, closer_typo: bool) {
    if p.at(terminator) || !closer_typo {
        p.expect(terminator);
    }
}

fn return_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    // `Возврат` may carry a value or not, and what says it does not is the
    // end of the statement or the end of something enclosing it. The blocks
    // that are open have already stated their closers, so asking them beats
    // keeping a list here — a list has no way to know that `КонецЦикла` with
    // no loop open closes nothing.
    if !at_end_of_statement(p) {
        expressions::expression(p);
    }

    p.eat(T![Semicolon]);

    m.complete(p, NodeKind::ReturnStmt);
}

// A block states the words that close it while it parses what it encloses, so
// that a rule tripping over one of them reports the trip and leaves the word
// for the block. Without that, the count of false "closer is missing" reports
// is the nesting depth: one unclosed call inside `Попытка` inside `Процедура`
// costs `Исключение`, `КонецПопытки` and `КонецПроцедуры` all three.

fn at_end_do(p: &Parser) -> bool {
    p.at(T![KwEndDo])
}

// A header waits for the word that ends it as surely as the block waits for
// the word that closes it, and a condition never written leaves the parser
// standing on that word. `Если Тогда` used to consume the `Тогда` in front of
// it and then report it missing.

/// The `Тогда` the `Если` header ends at.
///
/// Byte-identical to the predicate of the same name in `grammar.rs`, and kept
/// apart on purpose — that one is awaited by `#Если`. See the banner there for
/// why one predicate answering both questions would stop telling the two
/// positions apart.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, findings D1 and D2.
fn at_then(p: &Parser) -> bool {
    p.at(T![KwThen])
}

/// The comma between the two operands of a handler statement. It stands at
/// the statement's own level, where no group is open to speak for it.
fn at_handler_comma(p: &Parser) -> bool {
    p.at(T![Comma])
}

fn at_do(p: &Parser) -> bool {
    p.at(T![KwDo])
}

fn at_to_or_do(p: &Parser) -> bool {
    matches!(p.current(), Some(T![KwTo] | T![KwDo]))
}

fn at_if_closer(p: &Parser) -> bool {
    matches!(p.current(), Some(T![KwElsIf] | T![KwElse] | T![KwEndIf]))
}

fn at_try_closer(p: &Parser) -> bool {
    matches!(p.current(), Some(T![KwExcept] | T![KwEndTry]))
}

fn if_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    let recovered = p.within_boundary(at_if_closer, |p| {
        p.within_boundary(at_then, expressions::expression);

        p.expect(T![KwThen]);

        let mut recovered =
            stmt_list_of(p, &[T![KwElsIf], T![KwElse], T![KwEndIf]], &[T![KwEndIf]]);

        while p.at(T![KwElsIf]) {
            p.check_iteration_limit();
            let em = p.start();
            p.bump();
            p.within_boundary(at_then, expressions::expression);
            p.expect(T![KwThen]);
            recovered = recovered.union(stmt_list_of(
                p,
                &[T![KwElsIf], T![KwElse], T![KwEndIf]],
                &[T![KwEndIf]],
            ));
            em.complete(p, NodeKind::ElseIfClause);
        }

        if p.at(T![KwElse]) {
            let em = p.start();
            p.bump();
            recovered = recovered.union(stmt_list_of(p, &[T![KwEndIf]], &[T![KwEndIf]]));
            em.complete(p, NodeKind::ElseClause);
        }
        recovered
    });

    expect_stmt_list_terminator(p, T![KwEndIf], recovered.contains(T![KwEndIf]));

    p.eat(T![Semicolon]);

    m.complete(p, NodeKind::IfStmt);
}

fn while_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    let recovered = p.within_boundary(at_end_do, |p| {
        p.within_boundary(at_do, expressions::expression);

        p.expect(T![KwDo]);

        stmt_list(p, T![KwEndDo])
    });

    expect_stmt_list_terminator(p, T![KwEndDo], recovered);

    p.eat(T![Semicolon]);

    m.complete(p, NodeKind::WhileStmt);
}

fn for_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    if p.at(T![KwEach]) {
        let recovered = p.within_boundary(at_end_do, |p| {
            p.bump();

            if p.at(T![Ident]) {
                p.bump();
            }

            p.within_boundary(at_do, |p| {
                p.expect(T![KwIn]);

                expressions::expression(p);
            });

            p.expect(T![KwDo]);

            stmt_list(p, T![KwEndDo])
        });

        expect_stmt_list_terminator(p, T![KwEndDo], recovered);

        p.eat(T![Semicolon]);

        m.complete(p, NodeKind::ForEachStmt);
    } else {
        let recovered = p.within_boundary(at_end_do, |p| {
            if p.at(T![Ident]) {
                p.bump();
            }

            p.expect(T![Eq]);

            // The scope has to outlive the `По` it protects: an expect run
            // outside it would take the `Цикл` the header is still waiting
            // for, and then report that same `Цикл` missing.
            // `По` stops being awaited the moment it is consumed, so the
            // second bound is parsed under `Цикл` alone. Keeping `По` a
            // boundary past its own position leaves a repeated one standing,
            // and the expect for `Цикл` then takes the real `Цикл` instead.
            p.within_boundary(at_to_or_do, |p| {
                expressions::expression(p);

                p.expect(T![KwTo]);
            });

            p.within_boundary(at_do, |p| {
                expressions::expression(p);
            });

            p.expect(T![KwDo]);

            stmt_list(p, T![KwEndDo])
        });

        expect_stmt_list_terminator(p, T![KwEndDo], recovered);

        p.eat(T![Semicolon]);

        m.complete(p, NodeKind::ForStmt);
    }
}

fn try_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    let (body_typos, except_typos) = p.within_boundary(at_try_closer, |p| {
        // Тело идёт до `Исключение`, но опечатка `КонецПопытки` попадает в него
        // раньше: список обязан запомнить оба закрывателя, о пропаже которых
        // спросят после него.
        let body_typos = stmt_list_of(p, &[T![KwExcept]], &[T![KwExcept], T![KwEndTry]]);

        expect_stmt_list_terminator(p, T![KwExcept], body_typos.contains(T![KwExcept]));

        let em = p.start();
        let except_typos = stmt_list_of(p, &[T![KwEndTry]], &[T![KwEndTry]]);
        em.complete(p, NodeKind::ExceptClause);
        (body_typos, except_typos)
    });

    expect_stmt_list_terminator(
        p,
        T![KwEndTry],
        body_typos.contains(T![KwEndTry]) || except_typos.contains(T![KwEndTry]),
    );

    p.eat(T![Semicolon]);

    m.complete(p, NodeKind::TryStmt);
}

fn raise_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    if p.at(T![LParen]) {
        parse_raise_call_args(p);
    } else if !at_end_of_statement(p) {
        expressions::expression(p);
    }

    p.eat(T![Semicolon]);

    m.complete(p, NodeKind::RaiseStmt);
}

/// Whether nothing more of this statement can follow: its own separator, the
/// end of the input, or a word an enclosing construct is waiting for.
fn at_end_of_statement(p: &Parser) -> bool {
    p.at(T![Semicolon]) || p.at_end() || p.at_enclosing_boundary()
}

fn parse_raise_call_args(p: &mut Parser) {
    assert!(p.at(T![LParen]));
    p.bump();

    p.within_boundary(super::at_paren_list_punctuation, |p| {
        while !p.at(T![RParen]) && !p.at_end() {
            p.check_iteration_limit();

            if p.at(T![Comma]) {
            } else if !p.at(T![RParen]) {
                expressions::expression(p);
            }

            if p.at(T![Comma]) {
                p.bump();
            } else if !p.at(T![RParen]) {
                break;
            }
        }
    });

    p.expect(T![RParen]);
}

/// The tilde is optional, though 4.2.4.2 makes it part of the label.
///
/// 4.6.7 writes `Перейти <Метка>;` and its example writes `Перейти ~Метка1;`.
/// Taking the label without its tilde leaves the statement its shape while the
/// tilde is still being typed, instead of losing the label name with it.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D10.
fn goto_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    let tilde = p.eat(T![Tilde]);

    // A word spelled as a keyword is a label name when written flush against
    // its tilde: `Перейти ~КонецЦикла;` builds on the platform. A tilde left
    // alone while typing does not take the `КонецЦикла` of the next line.
    let keyword_name =
        tilde && p.current().is_some_and(|kind| kind.is_keyword()) && !p.a_gap_precedes();
    if p.at(T![Ident]) || keyword_name {
        p.bump();
    }

    p.eat(T![Semicolon]);

    m.complete(p, NodeKind::GotoStmt);
}

fn label_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();
    // `~КонецЦикла:` builds on the platform: the word behind the tilde names the
    // label even when spelled as a keyword. It is taken only flush against the
    // tilde and before its colon, so a construct word is never read as a label.
    let keyword_name = p.current().is_some_and(|kind| kind.is_keyword())
        && !p.a_gap_precedes()
        && p.nth(1) == Some(T![Colon]);
    if keyword_name {
        p.bump();
    } else {
        p.expect(T![Ident]);
    }
    p.expect(T![Colon]);
    m.complete(p, NodeKind::LabelStmt);
}

/// The parentheses of 4.6.8 are read as optional.
///
/// The section writes `Выполнить(<Строка>)` with the parentheses inside the
/// form rather than in brackets of optionality. They are optional here because
/// the bare form occurs 41 times across a corpus of 75 438 files, against 1833
/// parenthesised ones, and refusing it would be 41 false errors on code that
/// builds.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D8.
fn execute_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    expressions::expression(p);

    p.eat(T![Semicolon]);

    m.complete(p, NodeKind::ExecuteStmt);
}

fn add_handler_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    p.within_boundary(at_handler_comma, |p| {
        expressions::expression(p);

        p.expect(T![Comma]);
    });

    expressions::expression(p);

    p.eat(T![Semicolon]);

    m.complete(p, NodeKind::AddHandlerStmt);
}

fn remove_handler_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();

    p.within_boundary(at_handler_comma, |p| {
        expressions::expression(p);

        p.expect(T![Comma]);
    });

    expressions::expression(p);

    p.eat(T![Semicolon]);

    m.complete(p, NodeKind::RemoveHandlerStmt);
}

fn assignment_or_call(p: &mut Parser) -> bool {
    let m = p.start();

    let is_valid_stmt = expressions::postfix_expression_for_assignment(p);

    if p.eat(T![Eq]) {
        expressions::expression(p);
        m.complete(p, NodeKind::AssignStmt);
        p.eat(T![Semicolon]);
        false
    } else if is_valid_stmt {
        m.complete(p, NodeKind::CallStmt);
        p.eat(T![Semicolon]);
        false
    } else {
        p.error_custom_at_marker(m, "ожидался вызов или присваивание");
        p.eat(T![Semicolon]);
        true
    }
}

/// Список операторов до одного из `terminators`.
///
/// Возвращает не «было восстановление», а множество закрывателей из `closers`,
/// опечатки которых оно съело: только такое слово — испорченная попытка закрыть
/// конструкцию, и только после него вторая жалоба о пропаже закрывателя была бы
/// дублем. Посторонний оператор такой поблажки не даёт: конструкция после него
/// по-прежнему не закрыта, и сказать об этом больше некому. Список ветки `Если`
/// кончается ещё на `ИначеЕсли` и `Иначе`, но опечатка в них конструкцию не
/// закрывает и структурную ошибку не отменяет.
///
/// Закрывателей несколько там, где список кончается раньше, чем следующий
/// успевает стать ожидаемым: тело `Попытки` останавливается на `Исключение`, но
/// опечатка `КонецПопытки` попадает в него до этого.
fn stmt_list_of(p: &mut Parser, terminators: &[Sig], closers: &[Sig]) -> TokenSet {
    let m = p.start();
    let mut recovered_closer_typos = TokenSet::empty();

    // The list ends at its own terminator and at every closer further out.
    // Rules inside refuse to consume an enclosing closer, so a list waiting
    // only for its own would wait for a token nothing will reach.
    while !p.at_end() && !terminators.iter().any(|t| p.at(*t)) && !p.at_enclosing_boundary() {
        p.check_iteration_limit();

        if p.at_end() || terminators.iter().any(|t| p.at(*t)) || p.at_enclosing_boundary() {
            break;
        }

        let head = p.token_pos();
        if statement(p) {
            for closer in closers {
                if p.resembles_closer(head, *closer) {
                    recovered_closer_typos =
                        recovered_closer_typos.union(TokenSet::new(&[*closer]));
                }
            }
        }
    }

    m.complete(p, NodeKind::StmtList);
    recovered_closer_typos
}
