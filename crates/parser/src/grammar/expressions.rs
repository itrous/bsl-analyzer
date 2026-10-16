use crate::Sig;

use crate::event::NodeKind;
use crate::parser::token_set::TokenSet;
use crate::parser::{CompletedMarker, Parser};

// Keywords accepted as a member name when the name is on a *new* line after the
// dot. There the construct is ambiguous with a dangling dot before a fresh
// statement, so we stay conservative and never swallow block-structuring
// keywords (КонецФункции, Функция, …) — that would destroy error recovery.
const CROSS_LINE_PROPERTY_NAME_TOKENS: TokenSet = TokenSet::new(&[
    T![Ident],
    T![KwProcedure],
    T![KwFunction],
    T![KwExecute],
    T![KwGoto],
    T![KwBreak],
    T![KwContinue],
    T![KwTo],
    T![KwTrue],
    T![KwFalse],
    T![KwUndefined],
    T![KwNull],
    T![KwNew],
]);

pub fn expression(p: &mut Parser) {
    or_expr(p);
}

pub fn postfix_expression_for_assignment(p: &mut Parser) -> bool {
    postfix_expr_with_call_info(p)
}

fn or_expr(p: &mut Parser) {
    let mut lhs = p.start();
    and_expr(p);

    while p.at(T![KwOr]) {
        p.check_iteration_limit();
        let m = lhs.complete(p, NodeKind::Expr).precede(p);
        p.bump();
        let rhs = p.start();
        and_expr(p);
        rhs.complete(p, NodeKind::Expr);
        lhs = m.complete(p, NodeKind::BinaryExpr).precede(p);
    }

    lhs.complete(p, NodeKind::Expr);
}

fn and_expr(p: &mut Parser) {
    let mut lhs = p.start();
    not_expr(p);

    while p.at(T![KwAnd]) {
        p.check_iteration_limit();
        let m = lhs.complete(p, NodeKind::Expr).precede(p);
        p.bump();
        let rhs = p.start();
        not_expr(p);
        rhs.complete(p, NodeKind::Expr);
        lhs = m.complete(p, NodeKind::BinaryExpr).precede(p);
    }

    lhs.complete(p, NodeKind::Expr);
}

fn not_expr(p: &mut Parser) {
    inline_directives(p);
    if p.at(T![KwNot]) {
        let m = p.start();
        p.bump();
        not_expr(p);
        m.complete(p, NodeKind::UnaryExpr);
    } else {
        comparison_expr(p);
    }
}

fn comparison_expr(p: &mut Parser) {
    let mut lhs = p.start();
    additive_expr(p);
    let mut saw_comparison = false;

    while matches!(p.current(), Some(T![Eq] | T![Neq] | T![Lt] | T![Le] | T![Gt] | T![Ge])) {
        saw_comparison = true;
        let m = lhs.complete(p, NodeKind::Expr).precede(p);
        p.bump();
        let rhs = p.start();
        additive_expr(p);
        rhs.complete(p, NodeKind::Expr);
        lhs = m.complete(p, NodeKind::BinaryExpr).precede(p);
    }

    if saw_comparison {
        lhs.complete(p, NodeKind::Expr);
    } else {
        lhs.abandon(p);
    }
}

fn additive_expr(p: &mut Parser) {
    let mut lhs = p.start();
    multiplicative_expr(p);

    while matches!(p.current(), Some(T![Plus]) | Some(T![Minus])) {
        p.check_iteration_limit();
        let m = lhs.complete(p, NodeKind::Expr).precede(p);
        p.bump();
        let rhs = p.start();
        multiplicative_expr(p);
        rhs.complete(p, NodeKind::Expr);
        lhs = m.complete(p, NodeKind::BinaryExpr).precede(p);
    }

    lhs.complete(p, NodeKind::Expr);
}

fn multiplicative_expr(p: &mut Parser) {
    let mut lhs = p.start();
    unary_expr(p);

    while matches!(p.current(), Some(T![Star]) | Some(T![Slash]) | Some(T![Percent])) {
        p.check_iteration_limit();
        let m = lhs.complete(p, NodeKind::Expr).precede(p);
        p.bump();
        let rhs = p.start();
        unary_expr(p);
        rhs.complete(p, NodeKind::Expr);
        lhs = m.complete(p, NodeKind::BinaryExpr).precede(p);
    }

    lhs.complete(p, NodeKind::Expr);
}

/// Unary `+` and `-` bind at the level 4.5.4 gives them and build no node.
///
/// The precedence is the source's: the seventh row of the table binds tighter
/// than multiplication and looser than dereference. The absent node is not —
/// `Не а` completes a `UnaryExpr` while `-а` leaves the sign a sibling of its
/// operand. Kept as it stands: the shape of the tree is not the language, both
/// tokens are present, and wrapping them would change the tree for every unary
/// sign in every module and everything that lowers one.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D9.
fn unary_expr(p: &mut Parser) {
    inline_directives(p);
    match p.current() {
        Some(T![Plus]) | Some(T![Minus]) => {
            p.bump();
            unary_expr(p);
        }
        _ => postfix_expr(p),
    }
}

fn postfix_expr(p: &mut Parser) {
    postfix_expr_with_call_info(p);
}

/// `[]` is placed with `.` and `()`, which the precedence table does not do.
///
/// Section 4.5.4 puts dereference and call at the top of the ladder and never
/// mentions the bracket. The form is given three times — 4.2.5, 4.7.1
/// (`<Объект>[<Имя свойства>]`) and 4.7.5 (`[<Аргумент>]`) — and its precedence
/// nowhere. Grouping it here is a decision, not a reading: it is the only
/// placement under which `а.б[0].в` binds left to right as written.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D10.
fn postfix_expr_with_call_info(p: &mut Parser) -> bool {
    let mut postfix_after_constructor = p.at(T![KwNew]);
    let Some(mut lhs) = primary_expr(p) else {
        return false;
    };

    let mut is_valid_statement = false;

    loop {
        p.check_iteration_limit();
        match p.current() {
            Some(T![Dot]) => {
                let m = lhs.precede(p);
                if postfix_after_constructor {
                    let dot = p.start();
                    p.bump();
                    p.error_custom_at_marker(
                        dot,
                        "после конструктора нельзя обращаться к свойству или методу напрямую",
                    );
                } else {
                    p.bump();
                }
                // Whether the word behind the markers is a name is decided just
                // below, by the same rule as without them; here it only has to
                // be a word.
                inline_directives_before(p, |kind| kind == T![Ident] || kind.is_keyword());
                let crossed_newline = p.a_line_break_precedes();
                let is_orphaned_declaration = crossed_newline && p.at_declaration_start();
                // On the same line `expr.keyword` is unambiguously a member access —
                // BSL keywords are not reserved as property/field/enum-value names
                // (ВСД.for, XDTO.return, Условие.Иначе, Перечисления.X.ИЛИ). Across a
                // newline the construct is ambiguous with a dangling dot before a new
                // statement, so we keep the conservative whitelist to preserve in-method
                // error recovery (do not swallow КонецФункции/Функция/… as a member).
                let at_property_name = if crossed_newline {
                    p.at_ts(CROSS_LINE_PROPERTY_NAME_TOKENS)
                } else {
                    p.at(T![Ident]) || p.current().is_some_and(Sig::is_keyword)
                };
                if at_property_name && !is_orphaned_declaration {
                    p.bump();
                    lhs = m.complete(p, NodeKind::FieldExpr);
                    is_valid_statement = false;
                } else {
                    p.error_custom_no_bump("ожидалось имя свойства после '.'");
                    m.complete(p, NodeKind::FieldExpr);
                    break;
                }
                postfix_after_constructor = false;
            }
            Some(T![LBracket]) => {
                let m = lhs.precede(p);
                postfix_after_constructor = false;
                p.bump();
                p.within_boundary(super::at_closing_bracket, |p| {
                    expression(p);
                });
                p.expect(T![RBracket]);
                lhs = m.complete(p, NodeKind::IndexExpr);
                is_valid_statement = true;
            }
            Some(T![LParen]) => {
                let m = lhs.precede(p);
                postfix_after_constructor = false;
                arg_list(p);
                lhs = m.complete(p, NodeKind::CallExpr);
                is_valid_statement = true;
            }
            Some(
                T![PreInsert]
                | T![PreEndInsert]
                | T![PreDelete]
                | T![PreIf]
                | T![PreElsIf]
                | T![PreElse]
                | T![PreEndIf],
            ) if continue_across_inline_directives(p) => {}
            _ => break,
        }
    }

    is_valid_statement
}

fn primary_expr(p: &mut Parser) -> Option<CompletedMarker> {
    match p.current() {
        Some(T![Decimal]) | Some(T![Float]) | Some(T![Date]) => {
            let m = p.start();
            p.bump();
            Some(m.complete(p, NodeKind::Literal))
        }
        Some(T![String]) | Some(T![StringStart]) => Some(string_literal(p)),
        Some(T![StringPart]) | Some(T![StringTail]) => {
            p.error_custom("неожиданный фрагмент строки");
            None
        }
        Some(T![KwTrue]) | Some(T![KwFalse]) => {
            let m = p.start();
            p.bump();
            Some(m.complete(p, NodeKind::Literal))
        }
        Some(T![KwUndefined]) | Some(T![KwNull]) => {
            let m = p.start();
            p.bump();
            Some(m.complete(p, NodeKind::Literal))
        }
        Some(T![KwAwait]) => Some(await_expr(p)),
        Some(T![Ident]) => {
            let m = p.start();
            p.bump();
            Some(m.complete(p, NodeKind::Ident))
        }
        Some(T![LParen]) => {
            let m = p.start();
            p.bump();
            p.within_boundary(super::at_closing_paren, |p| {
                expression(p);
            });
            p.expect(T![RParen]);
            Some(m.complete(p, NodeKind::ParenExpr))
        }
        Some(T![KwNew]) => Some(new_expr(p)),
        Some(T![Question]) => Some(ternary_expr(p)),
        _ => {
            p.error_unexpected();
            None
        }
    }
}

fn string_literal(p: &mut Parser) -> CompletedMarker {
    let m = p.start();

    loop {
        match p.current() {
            Some(T![String]) => {
                p.bump();
            }
            Some(T![StringStart]) => {
                p.bump();
                string_continuation_tail(p);
            }
            _ => break,
        }

        if !at_adjacent_string_literal(p) {
            break;
        }
    }

    m.complete(p, NodeKind::Literal)
}

fn at_adjacent_string_literal(p: &Parser) -> bool {
    matches!(p.current(), Some(T![String] | T![StringStart]))
}

fn string_continuation_tail(p: &mut Parser) {
    loop {
        p.check_iteration_limit();
        match p.current() {
            Some(T![StringTail]) | Some(T![String]) => {
                p.bump();
                break;
            }
            Some(T![StringPart]) => {
                p.bump();
            }
            // The literal is still open, so no statement can begin here: the
            // marker can only be tearing the literal apart.
            _ if at_inline_directive(p) => inline_directives(p),
            None => {
                p.error_custom("незакрытая многострочная строка");
                break;
            }
            _ => {
                p.error_custom("неожиданный токен в многострочной строке");
                break;
            }
        }
    }
}

fn await_expr(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    p.bump();
    expression(p);
    m.complete(p, NodeKind::AwaitExpr)
}

/// `Новый` alone is taken, and 4.6.2 has no such variant.
///
/// The section gives two: `Новый <Имя типа>[(…)]` and the functional
/// `Новый(<Тип>[, <Параметры>])` — hence both the optional type name and the
/// optional argument list. Bare `Новый` with neither is what stands in the
/// editor before the type name is typed, and refusing it would cost the
/// assignment around it.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D10.
fn new_expr(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    p.bump();

    inline_directives_before(p, |kind| kind == T![Ident] || kind == T![LParen]);
    if p.at(T![Ident]) {
        p.bump();
    }

    inline_directives_before(p, |kind| kind == T![LParen]);
    if p.at(T![LParen]) {
        arg_list(p);
    }
    m.complete(p, NodeKind::NewExpr)
}

/// Whether the token here carries on the expression this one sits inside —
/// an operator of any precedence level, or the dot and bracket of a postfix
/// chain. No rule declares these as a boundary, because a rule that reaches
/// one consumes it and loops; a rule giving up in front of one has to leave
/// it for that loop.
fn continues_the_surrounding_expression(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(
            T![KwOr]
                | T![KwAnd]
                | T![Eq]
                | T![Neq]
                | T![Lt]
                | T![Le]
                | T![Gt]
                | T![Ge]
                | T![Plus]
                | T![Minus]
                | T![Star]
                | T![Slash]
                | T![Percent]
                | T![Dot]
                | T![LBracket]
        )
    )
}

fn ternary_expr(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    p.bump();

    // Every other bracketed rule is entered with its opener already in hand.
    // The ternary's opener is an `expect`, so it can be absent — and with no
    // group open, nothing standing here is the ternary's to take. Not the
    // commas: an enclosing list writes those after a `?` too, and a successful
    // `expect` does not ask whose separator it is. Not the closer either.
    // Reading on would spend another rule's punctuation on operands that were
    // never written.
    if !p.eat(T![LParen]) {
        // Giving up must not spend a token either — but only where something
        // else will use it. `expect` already leaves an enclosing boundary
        // alone; what nobody declares is the operator that carries on the
        // expression around the `?`. Anything past those two is stray, and
        // leaving that behind means no rule ever takes it.
        if continues_the_surrounding_expression(p) {
            p.expect_no_bump(T![LParen]);
        } else {
            p.expect(T![LParen]);
        }
        return m.complete(p, NodeKind::TernaryExpr);
    }

    p.within_boundary(super::at_paren_list_punctuation, |p| {
        expression(p);
        p.expect(T![Comma]);
        expression(p);
        p.expect(T![Comma]);
        expression(p);
    });
    p.expect(T![RParen]);
    m.complete(p, NodeKind::TernaryExpr)
}

fn arg_list(p: &mut Parser) {
    let m = p.start();
    p.bump();

    p.within_boundary(super::at_paren_list_punctuation, |p| {
        // Before an argument a marker is read first: an argument the extension
        // removed entirely leaves the position empty, and only past the marker
        // is it visible whether the next token is an argument or the comma.
        inline_directives(p);
        if !p.at(T![RParen]) {
            if !p.at(T![Comma]) && !p.at(T![RParen]) {
                expression(p);
            }

            while p.eat(T![Comma]) {
                p.check_iteration_limit();
                inline_directives(p);
                if !p.at(T![Comma]) && !p.at(T![RParen]) {
                    expression(p);
                }
            }
        }
    });

    p.expect(T![RParen]);

    m.complete(p, NodeKind::ArgList);
}

// Extension markers (`#Вставка`, `#Удаление` and their closers) are text-level
// in the platform: the configurator applies them to the module text, so they may
// tear any construct apart, a string literal included. Between statements the
// statement rules take them as opaque blocks; the rules below accept them inside
// an expression, where what is inserted is parsed as part of the expression and
// what is removed stays an opaque `PreDeleteDir` the expression does not see.
//
// A conditional group (`#Если … #КонецЕсли`) is text-level in the platform as
// well, and inside an expression the same rules take it: the branch read is
// parsed as part of the expression, the directives and the other branches stay
// opaque `PreTornIfPart` nodes (see `torn_preprocessor_if`).

/// Whether a marker an expression may take stands here.
///
/// A closer counts only while an insertion opened inside an expression waits
/// for it; any other closer stays the stray token it always was.
pub(super) fn at_inline_directive(p: &Parser) -> bool {
    match p.current() {
        Some(T![PreInsert] | T![PreDelete]) => true,
        Some(T![PreEndInsert]) => p.inline_insert_open(),
        Some(T![PreIf]) => p.at_torn_if_inline(),
        Some(T![PreElsIf] | T![PreElse] | T![PreEndIf]) => p.at_torn_if_end(),
        _ => false,
    }
}

/// Consumes the markers standing where the construct cannot end — an operand
/// position, a list item, the inside of an open literal. No statement can
/// begin there, so a marker in that place tears the construct apart.
pub(super) fn inline_directives(p: &mut Parser) {
    while at_inline_directive(p) {
        p.check_iteration_limit();
        if p.at(T![PreIf]) {
            super::torn_preprocessor_if(p);
        } else if p.at_torn_if_end() {
            super::torn_preprocessor_if_end(p);
        } else if p.at(T![PreDelete]) {
            inline_delete(p);
        } else if p.at(T![PreInsert]) && !at_closed_inline_insert(p) {
            // Nothing will close it, so it opens nothing either: counted, it
            // would make a stray closer further on acceptable.
            let m = p.start();
            p.bump();
            p.error_custom_at_marker(m, "директива #Вставка без #КонецВставки");
        } else {
            p.bump_inline_insert_marker();
        }
    }
}

/// Whether the insertion opened here is closed before its method ends.
///
/// Before this marker could be read as tearing the expression, a missing
/// closer was an error; taking the marker must not make it silent. Only the
/// method is searched, since an insertion does not outlive it, and a marker
/// inside removed text closes nothing.
fn at_closed_inline_insert(p: &Parser) -> bool {
    p.insert_here_is_closed()
}

/// Consumes the markers here only when the token behind them satisfies
/// `continues`, so a construct that may end here is not extended by a block
/// that belongs to the statements after it.
fn inline_directives_before(p: &mut Parser, continues: impl Fn(Sig) -> bool) {
    if at_inline_directives_before(p, continues) {
        inline_directives(p);
    }
}

/// Takes the markers after a complete operand when they tear the expression
/// apart, and reports whether it took any.
///
/// After an operand the expression may just as well have ended — a statement
/// with its `;` omitted is followed by the next statement, and a marker there
/// opens a block of statements. Only the closer of an insertion this
/// expression opened, or a token behind the markers that no statement starts
/// with and the expression is waiting for, says the markers are the
/// expression's.
fn continue_across_inline_directives(p: &mut Parser) -> bool {
    if p.at(T![PreEndInsert]) && p.inline_insert_open() {
        p.bump_inline_insert_marker();
        return true;
    }
    // A conditional group in front of `;` stands inside the statement the `;`
    // ends: no statement of its own is left between them. So does the end of
    // a torn group's branch, which was opened inside that statement.
    let torn = p.at_torn_if_end() || p.at_torn_if_inline();
    if at_inline_directives_before(p, |kind| {
        TORN_EXPRESSION_CONTINUATION.contains(kind) || (torn && kind == T![Semicolon])
    }) {
        inline_directives(p);
        return true;
    }
    false
}

/// Whether a run of markers an expression may take starts here, is complete,
/// and is followed by a token satisfying `continues`.
///
/// An unclosed removal gives no answer: nothing stands behind it, and the
/// markers stay with the rule for statement boundaries.
fn at_inline_directives_before(p: &Parser, continues: impl Fn(Sig) -> bool) -> bool {
    let mut n = 0;
    let mut open = p.open_inline_inserts();
    let mut torn = Vec::new();
    loop {
        if let Some(resume) = p.torn_if_skip(n, &mut torn) {
            n = resume;
            continue;
        }
        match p.nth(n) {
            Some(T![PreInsert]) => {
                open += 1;
                n += 1;
            }
            Some(T![PreEndInsert]) if open > 0 => {
                open -= 1;
                n += 1;
            }
            Some(T![PreDelete]) => loop {
                n += 1;
                match p.nth(n) {
                    Some(T![PreEndDelete]) => {
                        n += 1;
                        break;
                    }
                    Some(_) => {}
                    None => return false,
                }
            },
            Some(next) if n > 0 => return continues(next),
            _ => return false,
        }
    }
}

/// Tokens that carry on an expression from after its operand and that no
/// statement begins with: operators, postfix openers, the punctuation of an
/// enclosing list or group, and the words ending a statement header. The words
/// ending a block are not here: a statement ends in front of them, and a
/// marker there stands between statements.
const TORN_EXPRESSION_CONTINUATION: TokenSet = TokenSet::new(&[
    T![KwOr],
    T![KwAnd],
    T![Eq],
    T![Neq],
    T![Lt],
    T![Le],
    T![Gt],
    T![Ge],
    T![Plus],
    T![Minus],
    T![Star],
    T![Slash],
    T![Percent],
    T![Dot],
    T![LBracket],
    T![LParen],
    T![RParen],
    T![RBracket],
    T![Comma],
    T![KwThen],
    T![KwDo],
    T![KwTo],
]);

/// A removed block inside an expression: kept whole and verbatim, and taken
/// without group tracking because its brackets no longer exist in the module.
fn inline_delete(p: &mut Parser) {
    let m = p.start();
    p.bump_removed();
    while !p.at_end() && !p.at(T![PreEndDelete]) {
        p.check_iteration_limit();
        p.bump_removed();
    }
    if p.at(T![PreEndDelete]) {
        p.bump_removed();
    }
    m.complete(p, NodeKind::PreDeleteDir);
}
