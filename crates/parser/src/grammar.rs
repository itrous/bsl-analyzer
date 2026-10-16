pub mod expressions;
pub mod items;
pub mod sdbl;
pub mod statements;

use crate::event::NodeKind;
use crate::parser::Parser;

fn annotated_item(p: &mut Parser) {
    let outer = p.start();

    // An error inside an annotation must not take the word the
    // declaration begins with: that word is what the annotation was
    // attached to, and consuming it costs the whole declaration.
    p.within_boundary(at_declaration_start, |p| {
        while matches!(
            p.current(),
            Some(T![AnnAtClient])
                | Some(T![AnnAtServer])
                | Some(T![AnnAtServerNoContext])
                | Some(T![AnnAtClientAtServer])
                | Some(T![AnnAtClientAtServerNoContext])
                | Some(T![AnnBefore])
                | Some(T![AnnAfter])
                | Some(T![AnnAround])
                | Some(T![AnnChangeAndValidate])
                | Some(T![AnnCustom])
        ) {
            p.check_iteration_limit();
            match p.current() {
                Some(T![AnnAtClient])
                | Some(T![AnnAtServer])
                | Some(T![AnnAtServerNoContext])
                | Some(T![AnnAtClientAtServer])
                | Some(T![AnnAtClientAtServerNoContext]) => {
                    items::compiler_directive(p);
                }
                _ => {
                    items::annotation(p);
                }
            }
        }

        // Region directives are flat folding markers and may sit between an
        // annotation and the declaration it applies to (e.g. `&НаКлиенте #Область X
        // <newline> Перем Y;`). Consume them here so the annotation still binds to
        // the following Procedure/Function/Var instead of derailing the parse.
        // So may the end of a torn group whose branches give the declaration
        // different annotations (`#Если Сервер Тогда &НаСервере #Иначе
        // &НаКлиенте #КонецЕсли Процедура …`).
        while matches!(p.current(), Some(T![PreRegion]) | Some(T![PreEndRegion]))
            || p.at_torn_if_end()
        {
            p.check_iteration_limit();
            match p.current() {
                Some(T![PreRegion]) => preprocessor_region(p),
                Some(T![PreEndRegion]) => preprocessor_end_region(p),
                _ => torn_preprocessor_if_end(p),
            }
        }
    });

    match p.current() {
        Some(T![KwAsync]) => match p.nth(1) {
            Some(T![KwProcedure]) => {
                items::procedure_def_content(p);
                outer.complete(p, NodeKind::ProcedureDef);
            }
            Some(T![KwFunction]) => {
                items::function_def_content(p);
                outer.complete(p, NodeKind::FunctionDef);
            }
            _ => {
                outer.abandon(p);
                p.error_unexpected();
            }
        },
        Some(T![KwProcedure]) => {
            items::procedure_def_content(p);
            outer.complete(p, NodeKind::ProcedureDef);
        }
        Some(T![KwFunction]) => {
            items::function_def_content(p);
            outer.complete(p, NodeKind::FunctionDef);
        }
        Some(T![KwVar]) => {
            items::var_declaration_content(p);
            outer.complete(p, NodeKind::VarDef);
        }
        _ => {
            outer.abandon(p);
            p.error_unexpected();
        }
    }
}

pub fn source_file(p: &mut Parser) {
    let m = p.start();

    while !p.at_end() {
        p.check_iteration_limit();

        if p.at_end() {
            break;
        }

        match p.current() {
            Some(T![KwAsync]) => match p.nth(1) {
                Some(T![KwProcedure]) => items::procedure_def(p),
                Some(T![KwFunction]) => items::function_def(p),
                _ => p.error_unexpected(),
            },
            Some(T![KwProcedure]) => items::procedure_def(p),
            Some(T![KwFunction]) => items::function_def(p),
            Some(T![KwVar]) => items::var_declaration(p),
            Some(T![PreRegion]) => preprocessor_region(p),
            Some(T![PreEndRegion]) => preprocessor_end_region(p),
            Some(T![PreIf]) => preprocessor_if(p),
            Some(T![PreDelete]) => preprocessor_delete(p),
            Some(T![PreInsert]) => preprocessor_insert(p),
            Some(T![AnnAtClient])
            | Some(T![AnnAtServer])
            | Some(T![AnnAtServerNoContext])
            | Some(T![AnnAtClientAtServer])
            | Some(T![AnnAtClientAtServerNoContext]) => {
                annotated_item(p);
            }
            Some(T![AnnBefore])
            | Some(T![AnnAfter])
            | Some(T![AnnAround])
            | Some(T![AnnChangeAndValidate])
            | Some(T![AnnCustom]) => {
                annotated_item(p);
            }
            _ => {
                statements::statement(p);
            }
        }
    }

    m.complete(p, NodeKind::SourceFile);
}

/// `#Область Имя` is a flat folding marker, not a container.
///
/// 1C region directives are preprocessor markers stripped before compilation;
/// they may interleave with control flow without nesting (e.g. `#КонецОбласти`
/// inside an `Если` body before `КонецЕсли`). A container node cannot represent
/// such overlapping ranges in a full-fidelity tree, so each `#Область` and each
/// `#КонецОбласти` is its own leaf node. Region nesting is reconstructed
/// post-hoc by pairing start/end markers (see `hir-def::region_tree`).
pub(super) fn preprocessor_region(p: &mut Parser) {
    let m = p.start();
    p.bump();
    // The name sits on the directive's own line; reaching past the newline would
    // steal the next statement's identifier and leave that statement headless.
    if !p.a_line_break_precedes() && p.at(T![Ident]) {
        p.bump();
    }

    m.complete(p, NodeKind::PreRegionDir);
}

/// `#КонецОбласти` is a flat folding marker. See [`preprocessor_region`].
pub(super) fn preprocessor_end_region(p: &mut Parser) {
    let m = p.start();
    p.bump();
    m.complete(p, NodeKind::PreRegionDir);
}

pub(super) fn preprocessor_if(p: &mut Parser) {
    if p.at_torn_if_statement() {
        torn_preprocessor_if(p);
        return;
    }

    let m = p.start();
    p.bump();
    p.enter_preproc_if();

    p.within_boundary(at_preproc_closer, |p| {
        p.within_boundary(at_then, preproc_expression);

        p.expect(T![KwThen]);

        preproc_content(p);

        while p.at(T![PreElsIf]) {
            p.check_iteration_limit();
            let elsif_m = p.start();
            p.bump();

            p.within_boundary(at_then, preproc_expression);

            p.expect(T![KwThen]);

            preproc_content(p);

            elsif_m.complete(p, NodeKind::PreElsIfClause);
        }

        if p.at(T![PreElse]) {
            let else_m = p.start();
            p.bump();

            preproc_content(p);

            else_m.complete(p, NodeKind::PreElseClause);
        }
    });

    p.leave_preproc_if();
    // Content that stopped at a statement closer (an `ИначеЕсли` of the `Если`
    // around the block) left the block open: its own directives follow.
    let crossed = !p.at_end() && !p.at_preproc_if_directive() && p.at_enclosing_boundary();
    p.expect(T![PreEndIf]);
    if crossed {
        p.cross_preproc_if();
    }
    m.complete(p, NodeKind::PreIfDir);
}

/// A conditional group that tears a construct apart, read as one variant.
///
/// The platform chooses lines before it parses, so a group may stand inside
/// an expression, between `Если` and `Тогда`, or give each branch its own
/// header for one shared body; every environment compiles a whole construct.
/// A tree holds one reading of the text, so the branch whose condition holds
/// with every symbol defined is read as code, and the header, the directives
/// and the other branches become opaque `PreTornIfPart` nodes around it —
/// text this variant does not compile, as a removal is. The part opened here
/// ends at the directive closing the branch, taken by
/// [`torn_preprocessor_if_end`] wherever the parse stands then.
pub(super) fn torn_preprocessor_if(p: &mut Parser) {
    let Some(group) = p.torn_if_here() else {
        return;
    };
    let m = p.start();
    p.bump();

    p.within_boundary(at_then, preproc_expression);
    p.expect(T![KwThen]);

    match group.active {
        Some((start, end)) => {
            p.bump_removed_until(start);
            p.open_torn_if(end, group.close);
        }
        None => p.bump_removed_until(group.close + 1),
    }

    m.complete(p, NodeKind::PreTornIfPart);
}

/// The directive ending the branch read of a torn group, together with the
/// branches after it and the `#КонецЕсли`. See [`torn_preprocessor_if`].
pub(super) fn torn_preprocessor_if_end(p: &mut Parser) {
    let Some(close) = p.close_torn_if() else {
        return;
    };
    let m = p.start();
    p.bump_removed_until(close + 1);
    m.complete(p, NodeKind::PreTornIfPart);
}

/// The `Тогда` a preprocessor condition ends at.
///
/// Byte-identical to the predicate of the same name in `statements.rs`, and
/// kept apart on purpose: that one is awaited by the `Если` statement, this one
/// by `#Если`. A single predicate asked by both would answer without knowing
/// which rule is asking, and a boundary that cannot tell its positions apart is
/// how a shared look-ahead stops distinguishing them.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, findings D1 and D2.
fn at_then(p: &Parser) -> bool {
    p.at(T![KwThen])
}

/// The punctuation a parenthesised list owns: the comma it reaches its next
/// part with, and the paren it ends with.
///
/// Declared by each construct rather than derived from the parser's count of
/// open groups. The count outlives its owner — once the rule that opened the
/// paren has returned, nothing will ever consume it — and a boundary nobody
/// is waiting behind is a parse that cannot move.
///
/// Each construct declares only the punctuation it will itself consume. A
/// construct that also claims a neighbour's separator makes recovery leave
/// behind a token nobody will take, and its own `expect` then spends the
/// closer on it.
pub(super) fn at_paren_list_punctuation(p: &Parser) -> bool {
    matches!(p.current(), Some(T![RParen] | T![Comma]))
}

/// The paren a group ends with. A group holds a single expression, so a comma
/// inside it belongs to no rule waiting here.
pub(super) fn at_closing_paren(p: &Parser) -> bool {
    p.at(T![RParen])
}

/// The bracket an index ends with.
pub(super) fn at_closing_bracket(p: &Parser) -> bool {
    p.at(T![RBracket])
}

/// The words a declaration begins with. An annotation is followed by one, and
/// an error inside the annotation must not take it: the declaration is what
/// the annotation was attached to.
fn at_declaration_start(p: &Parser) -> bool {
    matches!(
        p.current(),
        Some(
            T![KwProcedure]
                | T![KwFunction]
                | T![KwVar]
                | T![KwAsync]
                // The chain may hold more than one annotation, with a folding
                // marker allowed between them, so the next link of the chain
                // is awaited here exactly as the declaration is.
                | T![AnnAtClient]
                | T![AnnAtServer]
                | T![AnnAtServerNoContext]
                | T![AnnAtClientAtServer]
                | T![AnnAtClientAtServerNoContext]
                | T![AnnBefore]
                | T![AnnAfter]
                | T![AnnAround]
                | T![AnnChangeAndValidate]
                | T![AnnCustom]
                | T![PreRegion]
                | T![PreEndRegion]
        )
    )
}

/// The directive ending a branch of a torn group inside the region is not the
/// region's: the torn group takes it.
fn at_preproc_closer(p: &Parser) -> bool {
    p.at_preproc_if_directive() && !p.at_crossed_preproc_if_directive() && !p.at_torn_if_end()
}

/// A conditional region and a statement block can cross each other: a
/// `#Если` may open inside `Если` and the `ИначеЕсли` closing that `Если` may
/// stand inside the region. The region's content therefore ends at the
/// closers of whatever encloses it as well as at its own — rules inside it
/// will not consume an enclosing closer, so a region waiting only for
/// `#КонецЕсли` would wait for a token nothing reaches.
fn preproc_content(p: &mut Parser) {
    while !p.at_end() && !at_preproc_closer(p) && !p.at_enclosing_boundary() {
        p.check_iteration_limit();
        if p.at_end() || at_preproc_closer(p) || p.at_enclosing_boundary() {
            break;
        }

        match p.current() {
            Some(T![KwAsync]) => match p.nth(1) {
                Some(T![KwProcedure]) => items::procedure_def(p),
                Some(T![KwFunction]) => items::function_def(p),
                _ => p.error_unexpected(),
            },
            Some(T![KwProcedure]) => items::procedure_def(p),
            Some(T![KwFunction]) => items::function_def(p),
            Some(T![KwVar]) => items::var_declaration(p),
            Some(T![PreRegion]) => preprocessor_region(p),
            Some(T![PreEndRegion]) => preprocessor_end_region(p),
            Some(T![PreIf]) => preprocessor_if(p),
            Some(T![PreDelete]) => preprocessor_delete(p),
            Some(T![PreInsert]) => preprocessor_insert(p),
            Some(T![AnnAtClient])
            | Some(T![AnnAtServer])
            | Some(T![AnnAtServerNoContext])
            | Some(T![AnnAtClientAtServer])
            | Some(T![AnnAtClientAtServerNoContext]) => {
                annotated_item(p);
            }
            Some(T![AnnBefore])
            | Some(T![AnnAfter])
            | Some(T![AnnAround])
            | Some(T![AnnChangeAndValidate])
            | Some(T![AnnCustom]) => {
                annotated_item(p);
            }
            _ => {
                statements::statement(p);
            }
        }
    }
}

fn preproc_expression(p: &mut Parser) {
    let m = p.start();
    preproc_logical_expression(p);
    m.complete(p, NodeKind::PreExpr);
}

fn preproc_logical_expression(p: &mut Parser) {
    let m = p.start();

    preproc_logical_operand(p);

    while matches!(p.current(), Some(T![KwAnd]) | Some(T![KwOr])) {
        p.check_iteration_limit();
        let op_m = p.start();
        p.bump();
        op_m.complete(p, NodeKind::PreBoolOp);
        preproc_logical_operand(p);
    }

    m.complete(p, NodeKind::PreLogicalExpr);
}

/// Parentheses are read here although the source does not write them.
///
/// The `<Логическое выражение>` production of section 4.8.1.2 is a flat chain of
/// `[НЕ] <Символ>` joined by `И`/`Или`, with no parentheses and no precedence —
/// which is why the chain above is flat too. Parentheses are accepted anyway:
/// they occur 29 times across a corpus of 75 438 files, and refusing them would
/// be 29 false errors on code the platform builds.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D3.
fn preproc_logical_operand(p: &mut Parser) {
    let m = p.start();

    if p.at(T![LParen]) {
        p.bump();

        p.within_boundary(at_closing_paren, |p| {
            if p.at(T![KwNot]) {
                p.bump();
                preproc_logical_operand(p);
            } else {
                preproc_logical_expression(p);
            }
        });

        p.expect(T![RParen]);
    } else if p.at(T![KwNot]) {
        p.bump();
        preproc_logical_operand(p);
    } else {
        preproc_symbol(p);
    }

    m.complete(p, NodeKind::PreLogicalOperand);
}

pub(super) fn preprocessor_delete(p: &mut Parser) {
    let m = p.start();
    p.bump();

    while !p.at_end() && !p.at(T![PreEndDelete]) {
        p.check_iteration_limit();
        p.bump();
    }

    p.eat(T![PreEndDelete]);
    m.complete(p, NodeKind::PreDeleteDir);
}

pub(super) fn preprocessor_insert(p: &mut Parser) {
    let m = p.start();
    p.bump();

    while !p.at_end() && !p.at(T![PreEndInsert]) {
        p.check_iteration_limit();
        p.bump();
    }

    p.eat(T![PreEndInsert]);
    m.complete(p, NodeKind::PreInsertDir);
}

fn preproc_symbol(p: &mut Parser) {
    let m = p.start();

    match p.current() {
        Some(T![Ident]) => {
            p.bump();
        }
        _ => {
            p.error_unexpected();
        }
    }

    m.complete(p, NodeKind::PreSymbol);
}
