use crate::event::NodeKind;
use crate::parser::Parser;

use super::statements;

pub fn compiler_directive(p: &mut Parser) {
    let m = p.start();
    p.bump();
    m.complete(p, NodeKind::CompilerDirective);
}

pub fn annotation(p: &mut Parser) {
    let m = p.start();
    p.bump();

    if p.at(T![LParen]) {
        annotation_params(p);
    }

    m.complete(p, NodeKind::Annotation);
}

fn annotation_params(p: &mut Parser) {
    let m = p.start();
    p.bump();

    p.within_boundary(super::at_paren_list_punctuation, |p| {
        if !p.at(T![RParen]) {
            annotation_param(p);
            while p.eat(T![Comma]) {
                p.check_iteration_limit();
                annotation_param(p);
            }
        }
    });

    p.expect(T![RParen]);

    m.complete(p, NodeKind::AnnotationParams);
}

fn annotation_param(p: &mut Parser) {
    let m = p.start();

    if p.at(T![Ident]) {
        p.bump();
        if p.eat(T![Eq]) {
            annotation_param_value(p);
        }
    } else {
        annotation_param_value(p);
    }

    m.complete(p, NodeKind::AnnotationParam);
}

fn annotation_param_value(p: &mut Parser) {
    match p.current() {
        Some(T![Decimal])
        | Some(T![Float])
        | Some(T![String])
        | Some(T![Date])
        | Some(T![KwTrue])
        | Some(T![KwFalse])
        | Some(T![KwUndefined])
        | Some(T![KwNull]) => {
            p.bump();
        }
        Some(T![Minus]) | Some(T![Plus]) => {
            p.bump();
            if p.at(T![Decimal]) || p.at(T![Float]) {
                p.bump();
            }
        }
        _ => {
            p.error_unexpected();
        }
    }
}

pub fn procedure_def(p: &mut Parser) {
    let m = p.start();
    procedure_def_content(p);
    m.complete(p, NodeKind::ProcedureDef);
}

// A definition states the word that closes it while it parses its header and
// its body, so that a rule tripping over that word leaves it for the
// definition instead of consuming it and reporting it missing at end of file.

fn at_end_procedure(p: &Parser) -> bool {
    p.at(T![KwEndProcedure])
}

fn at_end_function(p: &Parser) -> bool {
    p.at(T![KwEndFunction])
}

/// A keyword is taken as the declared name, and the diagnostic says why not.
///
/// Section 4.2.4.6 reserves keywords against use as the names of declared
/// procedures and functions, so `Процедура Если()` is not the language. The rule
/// takes it regardless: refusing the token here costs the whole body, while
/// naming the violation costs nothing and belongs to a diagnostic —
/// `ReservedWordAsMethodName` reports it at blocker severity. Same for
/// [`function_def_content`].
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D5.
pub fn procedure_def_content(p: &mut Parser) {
    p.forget_inline_inserts();
    p.eat(T![KwAsync]);

    p.expect(T![KwProcedure]);

    let recovered = p.within_boundary(at_end_procedure, |p| {
        // A keyword standing here is a name: the rule takes it and the
        // `ReservedWordAsMethodName` diagnostic says why it is not one (D5).
        // The word closing THIS declaration is the exception — taking it as
        // the name loses the closer, and the declaration then ends at end of
        // file with a complaint about end of file. A closer of something else
        // (a `)` of a group still open) is not reported on: that recovery's
        // count is pinned by `a_word_inside_a_group_is_not_the_separator_the_header_awaits`.
        if p.at(T![Ident])
            || (p.current().is_some_and(|k| k.is_keyword()) && !p.at(T![KwEndProcedure]))
        {
            p.bump();
        } else if p.at(T![KwEndProcedure]) {
            report_missing_name(p, "ожидалось имя процедуры");
        }

        if p.at(T![LParen]) {
            param_list(p);
        }

        p.eat(T![KwExport]);

        statements::stmt_list(p, T![KwEndProcedure])
    });

    statements::expect_stmt_list_terminator(p, T![KwEndProcedure], recovered);
}

pub fn function_def(p: &mut Parser) {
    let m = p.start();
    function_def_content(p);
    m.complete(p, NodeKind::FunctionDef);
}

/// A keyword is taken as the declared name, and the diagnostic says why not.
///
/// The same division as [`procedure_def_content`]: section 4.2.4.6 reserves
/// keywords against the names of declared functions, the rule takes one anyway
/// so that a bad name costs no body, and `ReservedWordAsMethodName` reports it.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D6.
pub fn function_def_content(p: &mut Parser) {
    p.forget_inline_inserts();
    p.eat(T![KwAsync]);

    p.expect(T![KwFunction]);

    let recovered = p.within_boundary(at_end_function, |p| {
        // The same division as [`procedure_def_content`], closer included.
        if p.at(T![Ident])
            || (p.current().is_some_and(|k| k.is_keyword()) && !p.at(T![KwEndFunction]))
        {
            p.bump();
        } else if p.at(T![KwEndFunction]) {
            report_missing_name(p, "ожидалось имя функции");
        }

        if p.at(T![LParen]) {
            param_list(p);
        }

        p.eat(T![KwExport]);

        statements::stmt_list(p, T![KwEndFunction])
    });

    statements::expect_stmt_list_terminator(p, T![KwEndFunction], recovered);
}

fn param_list(p: &mut Parser) {
    let m = p.start();
    p.bump();

    p.within_boundary(super::at_paren_list_punctuation, |p| {
        if !p.at(T![RParen]) {
            param(p);
            while p.eat(T![Comma]) {
                p.check_iteration_limit();
                param(p);
            }
        }
    });

    p.expect(T![RParen]);

    m.complete(p, NodeKind::ParamList);
}

/// The parameter name is optional, which 4.6.3 does not make it.
///
/// `[Знач] <Парам> [=<ДефЗнач>]` is the source's form, and the name is not in
/// brackets there. It is optional here for the line still being typed:
/// refusing would return an error on every keystroke between the paren and the
/// name.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D10.
fn param(p: &mut Parser) {
    let m = p.start();

    p.eat(T![KwVal]);

    if p.at(T![Ident]) {
        p.bump();
    } else if !p.at_end() && !p.at_enclosing_boundary() && !p.at_error() && !p.at(T![Eq]) {
        // A word of the wrong kind where the name belongs is reported and
        // taken by the ordinary recovery — leaving it behind would let the
        // list's own `expect(RParen)` spend it, and the closing paren with it
        // (github#259). The comma and the closing paren are a different state:
        // a typed list and its already-typed end, with the name still to be
        // written (finding D10); so is `=`, the parameter's own default value
        // ahead of the name, which taking would hand the value to
        // `expect(RParen)`. Text the lexer already rejected is not complained
        // about twice (the norm of `at_error`).
        p.error_custom("ожидалось имя параметра");
    }

    if p.eat(T![Eq]) {
        super::expressions::expression(p);
    }

    m.complete(p, NodeKind::Param);
}

pub fn var_declaration(p: &mut Parser) {
    let m = p.start();
    var_declaration_content(p);
    m.complete(p, NodeKind::VarDef);
}

/// Reports a name the position requires, without moving the cursor.
///
/// The token the position tripped over is what holds the rest of the parse
/// together: `;` closes the declaration, `,` ends the item, `Экспорт` is still
/// read by its own `eat`. Taking it as recovery costs the statement, so the
/// complaint stands where the name should have been and the token stays
/// (github#209). At end of input the position stays silent: the line is still
/// being typed, and the name may yet be written. On text the lexer already
/// rejected it stays silent too (the norm of `at_error`).
///
/// The SDBL half keeps a `report_missing_name` of its own
/// (`grammar/sdbl/expressions.rs`): there the report goes at a marker span and
/// no punctuation is ever taken, while here a word of the wrong kind is taken
/// by the ordinary recovery when the position is the parameter's.
fn report_missing_name(p: &mut Parser, expected: &'static str) {
    if !p.at_end() && !p.at_error() {
        p.error_custom_no_bump(expected);
    }
}

/// `Экспорт` is taken after any name of the list.
///
/// Section 4.6.1 states this three ways that do not agree: the production puts
/// `[Экспорт]` after the first name only, the prose requires it on every
/// declared variable separately, and the example writes `Перем А, Б Экспорт;`.
/// The platform settles it with the prose: `Перем А Экспорт, Б Экспорт;` and
/// `Перем А Экспорт, Б;` both build on 8.3.17 and 8.3.27, and each `Экспорт`
/// exports only the name it follows. Every form is read here, so none of them
/// costs the rest of the module a parse error.
///
/// Provenance: `docs/legal/bsl-clean-room-slice-b3.md`, finding D7.
pub fn var_declaration_content(p: &mut Parser) {
    p.bump();

    if p.at(T![Ident]) {
        p.bump();
    } else {
        report_missing_name(p, "ожидалось имя переменной");
    }
    p.eat(T![KwExport]);

    while p.eat(T![Comma]) {
        p.check_iteration_limit();
        if p.at(T![Ident]) {
            p.bump();
        } else {
            report_missing_name(p, "ожидалось имя переменной");
        }
        p.eat(T![KwExport]);
    }

    p.eat(T![Semicolon]);
}
