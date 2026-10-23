//! The quick fix of `UnusedParameters`: drop the parameter from the declaration and
//! the argument at its position from every call of the method in the module.
//!
//! The diagnostic already leaves out the methods whose signature the platform or the
//! metadata dictates (event and command handlers, subscriptions, notification
//! callbacks, interceptors, attachable methods, handlers bound by string). The fix
//! narrows that further to the methods whose every caller is in this file and can be
//! rewritten without changing what the program does:
//!
//! - the method is not `Экспорт`: an exported method has callers in other modules;
//! - no body of the module (neither this one nor an extension's) runs `Выполнить` or
//!   `Вычислить` or binds a handler (`ОписаниеОповещения`, `УстановитьДействие`) by a
//!   name that is not one string literal, no string literal of them names the method,
//!   and no extension body mentions it at all: those callers are out of reach of a
//!   file-local edit;
//! - every mention of the method in the file is a call, bare (`Имя(…)`) or through
//!   `ЭтотОбъект.`/`ЭтаФорма.`; a name read in any other way (a handler handed to
//!   `ДобавитьОбработчик`, a same-named variable) leaves the method as it is;
//! - the name of the parameter occurs nowhere else in the method, in any
//!   preprocessor branch, string or opaque block included;
//! - the argument a call passes at that position, and the parameter's default value,
//!   is a literal, a bare name or a unary operator over one: dropping a call or a field
//!   read would drop what evaluating it does (side effects, the exception it raises).
//!
//! The fix is manual: it changes a signature, which a reader decides on.

use crate::handlers::unused_local_method::{mentions_identifier, string_literal_texts};
use crate::{Diagnostic, DiagnosticsContext, Fix, TextEdit};
use ide_db::TextRange;
use rustc_hash::FxHashMap;
use stdx::case::CaseExt;
use syntax::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken, TextSize};

/// What every fix of the file checks against, gathered once.
///
/// A file reports an unused parameter per method, and a large module reports
/// hundreds; walking the whole file and its extensions' bodies again for each of them
/// made the fixes cost the size of the module times the number of reports.
struct ModuleFacts {
    root: SyntaxNode,
    /// Every identifier token of this body by its lower-cased text, in file order.
    identifiers: FxHashMap<String, Vec<SyntaxToken>>,
    /// Lower-cased string literals of this body and of the module's other bodies.
    string_literals: Vec<String>,
    /// Lower-cased texts of the tokens other than comments of the module's other
    /// bodies (an extension's), which this fix cannot edit.
    sibling_texts: Vec<String>,
    /// Some body of the module runs code known only at run time.
    runs_dynamic_code: bool,
    /// Some body of the module hands the platform a callback whose name is known only at
    /// run time.
    binds_computed_callback: bool,
}

/// Why a reported parameter gets no fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Declined {
    /// The diagnostic's range is not the name of a parameter of a method's own list.
    NotAParameter,
    Exported,
    DynamicCode,
    ComputedCallback,
    /// The parameter's name occurs in the method besides its declaration.
    ParameterNamed,
    /// A string literal of the module, or an extension body, names the method.
    MethodNamedOutsideCalls,
    /// The method's name is read in the file other than as a call this fix rewrites.
    NotACall,
    /// A call passes an argument at that position, or the parameter's default value is
    /// an expression, whose evaluation does more than produce a value.
    ArgumentWithEffect,
    /// A list holds an extension marker, a preprocessor directive or a parse error.
    UnsupportedList,
}

pub(super) fn attach_fixes(ctx: &DiagnosticsContext, diagnostics: &mut [Diagnostic]) {
    if diagnostics.is_empty() {
        return;
    }
    let root = ctx.parse().syntax_node();
    let sibling_roots = ctx.sibling_body_roots();
    let runs_dynamic_code = runs_dynamic_code(&root) || sibling_roots.iter().any(runs_dynamic_code);
    let binds_computed_callback =
        binds_computed_callback(&root) || sibling_roots.iter().any(binds_computed_callback);
    let mut string_literals = string_literal_texts(&root);
    for sibling in &sibling_roots {
        string_literals.extend(string_literal_texts(sibling));
    }
    let sibling_texts = sibling_roots.iter().flat_map(lower_cased_texts).collect();
    let facts = ModuleFacts {
        identifiers: identifiers_of(&root),
        root,
        string_literals,
        sibling_texts,
        runs_dynamic_code,
        binds_computed_callback,
    };
    for diagnostic in diagnostics.iter_mut() {
        if let Ok(fix) = remove_parameter_fix(&facts, diagnostic.range) {
            diagnostic.fixes.push(fix);
        }
    }
}

fn remove_parameter_fix(facts: &ModuleFacts, name_range: TextRange) -> Result<Fix, Declined> {
    use Declined::*;
    let name_token = facts
        .root
        .token_at_offset(name_range.start())
        .right_biased()
        .filter(|token| token.kind() == SyntaxKind::IDENT && token.text_range() == name_range)
        .ok_or(NotAParameter)?;
    let param =
        name_token.parent().filter(|node| node.kind() == SyntaxKind::PARAM).ok_or(NotAParameter)?;
    let param_list =
        param.parent().filter(|node| node.kind() == SyntaxKind::PARAM_LIST).ok_or(NotAParameter)?;
    let method = param_list
        .parent()
        .filter(|node| matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
        .ok_or(NotAParameter)?;
    let method_name = direct_token(&method, SyntaxKind::IDENT).ok_or(NotAParameter)?;
    if direct_token(&method, SyntaxKind::KW_EXPORT).is_some() {
        return Err(Exported);
    }
    if facts.runs_dynamic_code {
        return Err(DynamicCode);
    }
    if facts.binds_computed_callback {
        return Err(ComputedCallback);
    }

    let param_name = name_token.text().fold_lower();
    if mentioned_elsewhere(&method, &name_token, &param_name) {
        return Err(ParameterNamed);
    }
    // A caller omitting the argument evaluates the default value; it goes with the
    // declaration.
    if !param.children().all(|default| is_inert(&default)) {
        return Err(ArgumentWithEffect);
    }

    let method_lower = method_name.text().fold_lower();
    if facts.string_literals.iter().any(|literal| mentions_identifier(literal, &method_lower))
        || facts.sibling_texts.iter().any(|text| mentions_identifier(text, &method_lower))
    {
        return Err(MethodNamedOutsideCalls);
    }

    let params = list_slots(&param_list).ok_or(UnsupportedList)?;
    let index = params
        .iter()
        .position(|slot| slot.items.iter().any(|item| item.as_node() == Some(&param)))
        .ok_or(UnsupportedList)?;
    let mut edits: Vec<TextEdit> = removal_ranges(&params, index)
        .ok_or(UnsupportedList)?
        .into_iter()
        .map(|range| TextEdit { range, new_text: String::new() })
        .collect();

    let mut call_edits = 0usize;
    for call in calls_of(facts, &method_name, &method_lower).ok_or(NotACall)? {
        let arg_list =
            call.children().find(|node| node.kind() == SyntaxKind::ARG_LIST).ok_or(NotACall)?;
        let args = list_slots(&arg_list).ok_or(UnsupportedList)?;
        if index >= args.len() {
            continue;
        }
        if !args[index].items.iter().all(|item| item.as_node().is_some_and(is_inert)) {
            return Err(ArgumentWithEffect);
        }
        let ranges = removal_ranges(&args, index).ok_or(UnsupportedList)?;
        if ranges.iter().any(|range| !range.is_empty()) {
            edits.extend(
                ranges.into_iter().map(|range| TextEdit { range, new_text: String::new() }),
            );
            call_edits += 1;
        }
    }

    let name = name_token.text();
    let label = if call_edits == 0 {
        format!("Удалить неиспользуемый параметр \"{name}\"")
    } else {
        format!("Удалить неиспользуемый параметр \"{name}\" и его аргумент в вызовах")
    };
    Ok(Fix::manual(label, edits))
}

fn direct_token(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    node.children_with_tokens().filter_map(|e| e.into_token()).find(|t| t.kind() == kind)
}

fn is_trivia(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE | SyntaxKind::COMMENT)
}

/// The parameter's name occurs in the method beyond its own declaration — in code of any
/// preprocessor branch, in a string, in an opaque block. Comments do not count, nor does
/// the member name of a field access (`Строка.Имя`), which is never the parameter.
fn mentioned_elsewhere(method: &SyntaxNode, declaration: &SyntaxToken, name: &str) -> bool {
    method
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token != declaration && token.kind() != SyntaxKind::COMMENT)
        .filter(|token| token.parent().is_none_or(|parent| parent.kind() != SyntaxKind::FIELD_EXPR))
        .any(|token| mentions_identifier(&token.text().fold_lower(), name))
}

fn lower_cased_texts(root: &SyntaxNode) -> Vec<String> {
    root.descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() != SyntaxKind::COMMENT)
        .map(|token| token.text().fold_lower())
        .collect()
}

fn identifiers_of(root: &SyntaxNode) -> FxHashMap<String, Vec<SyntaxToken>> {
    #[cfg(test)]
    tests::IDENTIFIER_INDEXES.with(|count| count.set(count.get() + 1));
    let mut identifiers: FxHashMap<String, Vec<SyntaxToken>> = FxHashMap::default();
    for token in root.descendants_with_tokens().filter_map(|element| element.into_token()) {
        if token.kind() == SyntaxKind::IDENT {
            identifiers.entry(token.text().fold_lower()).or_default().push(token);
        }
    }
    identifiers
}

/// The calls of the method in the module, or `None` when the name is read in a way that
/// is not a call of it this fix can rewrite.
fn calls_of(facts: &ModuleFacts, declaration: &SyntaxToken, name: &str) -> Option<Vec<SyntaxNode>> {
    let mut calls = Vec::new();
    for token in facts.identifiers.get(name).into_iter().flatten() {
        if token == declaration {
            continue;
        }
        let parent = token.parent()?;
        let callee = match parent.kind() {
            // `Имя(…)`: the name is an expression of its own.
            SyntaxKind::IDENT => parent,
            // `Получатель.Имя`: another object's member is not this module's local
            // method, unless the receiver is the module's own object.
            SyntaxKind::FIELD_EXPR => {
                let receiver = parent.first_child()?;
                if !is_this_object(&receiver) {
                    continue;
                }
                parent
            }
            _ => return None,
        };
        let call = callee.parent().filter(|node| node.kind() == SyntaxKind::CALL_EXPR)?;
        if call.first_child().as_ref() != Some(&callee) {
            return None;
        }
        calls.push(call);
    }
    Some(calls)
}

fn is_this_object(receiver: &SyntaxNode) -> bool {
    receiver.kind() == SyntaxKind::IDENT
        && matches!(
            receiver.text().to_string().fold_lower().as_str(),
            "этотобъект" | "этаформа" | "thisobject" | "thisform"
        )
}

/// `Выполнить` and `Вычислить` run text known only at run time, which may call the
/// method with any number of arguments.
fn runs_dynamic_code(root: &SyntaxNode) -> bool {
    root.descendants().any(|node| match node.kind() {
        SyntaxKind::EXECUTE_STMT => true,
        SyntaxKind::CALL_EXPR => node.first_child().is_some_and(|callee| {
            callee.kind() == SyntaxKind::IDENT
                && matches!(callee.text().to_string().fold_lower().as_str(), "вычислить" | "eval")
        }),
        _ => false,
    })
}

/// `Новый ОписаниеОповещения(Имя, …)` (or `Новый("ОписаниеОповещения", Имя, …)`) or
/// `Элемент.УстановитьДействие("Событие", Имя)` whose handler name is not one string
/// literal: the platform calls whatever method the value names, with the arguments the
/// platform passes.
fn binds_computed_callback(root: &SyntaxNode) -> bool {
    root.descendants().any(|node| match node.kind() {
        SyntaxKind::NEW_EXPR => notify_description_with_computed_name(&node),
        SyntaxKind::CALL_EXPR => set_action_with_computed_name(&node),
        _ => false,
    })
}

fn set_action_with_computed_name(call: &SyntaxNode) -> bool {
    let is_set_action = call
        .first_child()
        .and_then(|callee| syntax::ast_utils::field_tail_name_token(&callee))
        .is_some_and(|name| {
            matches!(name.text().fold_lower().as_str(), "установитьдействие" | "setaction")
        });
    if !is_set_action {
        return false;
    }
    let Some(args) = call.children().find(|node| node.kind() == SyntaxKind::ARG_LIST) else {
        return false;
    };
    match list_slots(&args) {
        Some(slots) => slots
            .get(1)
            .is_some_and(|slot| !slot.items.is_empty() && string_literal(slot).is_none()),
        None => true,
    }
}

fn notify_description_with_computed_name(new_expr: &SyntaxNode) -> bool {
    let Some(args) = new_expr.children().find(|node| node.kind() == SyntaxKind::ARG_LIST) else {
        return false;
    };
    let slots = list_slots(&args);
    let base = match syntax::ast_utils::new_expr_type_name_token(new_expr) {
        Some(type_name) if is_notify_description(type_name.text()) => 0,
        Some(_) => return false,
        None => match slots.as_ref().and_then(|slots| slots.first()) {
            Some(first)
                if string_literal(first).is_some_and(|name| is_notify_description(&name)) =>
            {
                1
            }
            _ => return false,
        },
    };
    let Some(slots) = slots else {
        return true;
    };
    [base, base + 3]
        .into_iter()
        .filter_map(|index| slots.get(index))
        .any(|slot| !slot.items.is_empty() && string_literal(slot).is_none())
}

fn is_notify_description(name: &str) -> bool {
    matches!(name.fold_lower().as_str(), "описаниеоповещения" | "notifydescription")
}

/// The text of a position holding exactly one single-line string literal, unquoted.
fn string_literal(slot: &Slot) -> Option<String> {
    let [SyntaxElement::Node(node)] = slot.items.as_slice() else {
        return None;
    };
    let mut node = node.clone();
    while matches!(node.kind(), SyntaxKind::EXPR | SyntaxKind::PAREN_EXPR) {
        let mut children = node.children();
        let (Some(inner), None) = (children.next(), children.next()) else {
            return None;
        };
        node = inner;
    }
    if node.kind() != SyntaxKind::LITERAL {
        return None;
    }
    let mut tokens = node
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| !is_trivia(token.kind()));
    let (Some(token), None) = (tokens.next(), tokens.next()) else {
        return None;
    };
    if token.kind() != SyntaxKind::STRING {
        return None;
    }
    let text = token.text();
    text.strip_prefix('"').and_then(|text| text.strip_suffix('"')).map(str::to_string)
}

/// An argument whose evaluation does nothing but produce its value: a literal, a bare
/// name, an operator over those. A call, a `Новый`, a field or an index read may change
/// state or raise, which dropping the argument would silently undo.
fn is_inert(node: &SyntaxNode) -> bool {
    match node.kind() {
        SyntaxKind::LITERAL | SyntaxKind::IDENT => true,
        SyntaxKind::EXPR | SyntaxKind::PAREN_EXPR | SyntaxKind::UNARY_EXPR => {
            let mut operands = node.children();
            operands.next().is_some_and(|operand| is_inert(&operand)) && operands.next().is_none()
        }
        _ => false,
    }
}

/// One comma-separated position of a parenthesised list, with the separators around it.
struct Slot {
    /// What stands at the position besides whitespace and comments; empty for an
    /// omitted argument.
    items: Vec<SyntaxElement>,
    /// The `(` or `,` before the position.
    before: TextRange,
    /// The `,` or `)` after the position.
    after: TextRange,
    /// Where the line of the position starts when a comment ends the line before it,
    /// between `before` and the position.
    line_after_comment: Option<TextSize>,
}

impl Slot {
    fn start(&self) -> TextSize {
        self.items.first().map_or(self.after.start(), |item| trimmed(item).start())
    }

    fn end(&self) -> TextSize {
        self.items.last().map_or(self.after.start(), |item| trimmed(item).end())
    }
}

/// The positions of a `PARAM_LIST` or an `ARG_LIST`; `None` for a list torn by an
/// extension marker or a preprocessor directive, or one that did not parse.
fn list_slots(list: &SyntaxNode) -> Option<Vec<Slot>> {
    let elements: Vec<SyntaxElement> = list.children_with_tokens().collect();
    let open = elements.first().filter(|e| e.kind() == SyntaxKind::L_PAREN)?;
    let close = elements.last().filter(|e| e.kind() == SyntaxKind::R_PAREN)?;
    if elements.len() < 2 {
        return None;
    }
    let mut slots = Vec::new();
    let mut items = Vec::new();
    let mut before = open.text_range();
    for element in &elements[1..elements.len() - 1] {
        match element.kind() {
            SyntaxKind::COMMA => {
                slots.push(Slot {
                    items: std::mem::take(&mut items),
                    before,
                    after: element.text_range(),
                    line_after_comment: None,
                });
                before = element.text_range();
            }
            kind if is_trivia(kind) => {}
            SyntaxKind::ERROR => return None,
            kind if is_directive(kind) => return None,
            _ => items.push(element.clone()),
        }
    }
    let last = Slot { items, before, after: close.text_range(), line_after_comment: None };
    // `()` holds no position at all.
    if !(slots.is_empty() && last.items.is_empty()) {
        slots.push(last);
    }
    let tokens: Vec<SyntaxToken> =
        list.descendants_with_tokens().filter_map(|element| element.into_token()).collect();
    for slot in &mut slots {
        let (from, to) = (slot.before.end(), slot.start());
        let mut after_comment = false;
        for token in tokens
            .iter()
            .filter(|token| token.text_range().start() >= from && token.text_range().end() <= to)
        {
            match token.kind() {
                SyntaxKind::COMMENT => after_comment = true,
                SyntaxKind::NEWLINE if after_comment => {
                    slot.line_after_comment = Some(token.text_range().end());
                    after_comment = false;
                }
                _ => {}
            }
        }
    }
    Some(slots)
}

fn is_directive(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::PRE_IF
            | SyntaxKind::PRE_ELSIF
            | SyntaxKind::PRE_ELSE
            | SyntaxKind::PRE_END_IF
            | SyntaxKind::PRE_REGION
            | SyntaxKind::PRE_END_REGION
            | SyntaxKind::PRE_INSERT
            | SyntaxKind::PRE_END_INSERT
            | SyntaxKind::PRE_DELETE
            | SyntaxKind::PRE_END_DELETE
            | SyntaxKind::PRE_IF_DIR
            | SyntaxKind::PRE_ELSIF_CLAUSE
            | SyntaxKind::PRE_ELSE_CLAUSE
            | SyntaxKind::PRE_REGION_DIR
            | SyntaxKind::PRE_DELETE_DIR
            | SyntaxKind::PRE_INSERT_DIR
    )
}

/// The element's range without whitespace or comments at its edges.
fn trimmed(element: &SyntaxElement) -> TextRange {
    match element {
        SyntaxElement::Token(token) => token.text_range(),
        SyntaxElement::Node(node) => {
            let tokens: Vec<SyntaxToken> = node
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| !is_trivia(t.kind()))
                .collect();
            match (tokens.first(), tokens.last()) {
                (Some(first), Some(last)) => {
                    TextRange::new(first.text_range().start(), last.text_range().end())
                }
                _ => node.text_range(),
            }
        }
    }
}

/// What to delete to take position `index` out of the list. A position before the last
/// goes with the comma after it and the spacing up to the next one, so a comment on the
/// line of the previous position stays; the last goes with the comma before it, and a
/// comment between that comma and the position stays on its line.
fn removal_ranges(slots: &[Slot], index: usize) -> Option<Vec<TextRange>> {
    let slot = slots.get(index)?;
    if slots.len() == 1 {
        return Some(vec![TextRange::new(slot.start(), slot.end())]);
    }
    if let Some(next) = slots.get(index + 1) {
        return Some(vec![TextRange::new(slot.start(), next.start())]);
    }
    Some(match slot.line_after_comment {
        Some(line_start) => vec![slot.before, TextRange::new(line_start, slot.end())],
        None => vec![TextRange::new(slot.before.start(), slot.end())],
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use crate::DiagnosticCode;

    thread_local! {
        pub(super) static IDENTIFIER_INDEXES: Cell<usize> = const { Cell::new(0) };
    }

    /// The fixes of a file index its identifiers once, whatever the number of parameters
    /// it reports: a walk of the file per report made a large module cost its size times
    /// its reports. Every report of the input gets a fix, so the index serves them all.
    #[test]
    fn the_fixes_of_a_file_index_its_identifiers_once() {
        let methods = 8;
        let code: String = (0..methods)
            .map(|i| {
                format!(
                    "Процедура Метод{i}(Нужный, Лишний)\n    Сообщить(Нужный);\nКонецПроцедуры\n\n\
                     Процедура Вызов{i}()\n    Метод{i}(1, 2);\nКонецПроцедуры\n\n"
                )
            })
            .collect();
        let before = IDENTIFIER_INDEXES.with(Cell::get);

        let diagnostics = crate::test_utils::check_metadata_diagnostic(
            hir::ModuleMetadata {
                module_type: bsl_metadata::ModuleType::CommonModule,
                execution_context: None,
                common_module: None,
                mdo: None,
                register: None,
                command: None,
                http_service: None,
                web_service: None,
                integration_service: None,
                form: None,
            },
            &code,
            |_metadata, ctx| super::super::check(ctx),
        );

        let reported: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::UnusedParameters)
            .collect();
        assert_eq!(reported.len(), methods);
        assert!(reported.iter().all(|diagnostic| !diagnostic.fixes.is_empty()));
        assert_eq!(IDENTIFIER_INDEXES.with(Cell::get) - before, 1);
    }
}
