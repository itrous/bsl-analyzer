use hir::Semantics;
use ide_assists::{Assist, AssistId, FileEdit, SourceChange, TextEdit};
use ide_db::RootDatabase;
use syntax::ast::{AstNode, FunctionDef};
use syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};
use vfs::FileId;

use crate::references::find_references_in_file;

/// `Процедура` ⇄ `Функция` on the declaration line of a method.
///
/// Only the opening and closing keywords are rewritten, in the script the source already
/// uses (`Процедура`/`Procedure`); annotations, `Асинх`, the parameter list, `Экспорт` and
/// the body are left as they are. A procedure becomes a function without a `Возврат` being
/// added: a function that never returns a value is valid and returns `Неопределено`.
///
/// The reverse is offered only when nothing depends on the function returning a value:
/// it has no `Возврат <выражение>` (a procedure does not compile one), no call in the
/// module uses its result (a procedure call is not an expression), and it is not
/// `Экспорт` — callers in other modules are not searched here, so an exported function
/// is never converted.
pub fn convert_method_assists<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    range: TextRange,
) -> Vec<Assist> {
    let text = db.file_text(file_id);
    let root = db.parse(file_id).syntax_node();

    let Some(method) = enclosing_method(&root, range.start()) else {
        return Vec::new();
    };
    if !on_declaration_line(&text, &method, range.start()) {
        return Vec::new();
    }

    let assist = match method.kind() {
        SyntaxKind::PROCEDURE_DEF => convert(
            file_id,
            &method,
            (SyntaxKind::KW_PROCEDURE, SyntaxKind::KW_END_PROCEDURE),
            (("Функция", "КонецФункции"), ("Function", "EndFunction")),
            AssistId("convert_procedure_to_function"),
            "Преобразовать в функцию",
        ),
        SyntaxKind::FUNCTION_DEF if may_become_procedure(db, file_id, &method) => convert(
            file_id,
            &method,
            (SyntaxKind::KW_FUNCTION, SyntaxKind::KW_END_FUNCTION),
            (("Процедура", "КонецПроцедуры"), ("Procedure", "EndProcedure")),
            AssistId("convert_function_to_procedure"),
            "Преобразовать в процедуру",
        ),
        _ => None,
    };
    assist.into_iter().collect()
}

fn enclosing_method(root: &SyntaxNode, offset: TextSize) -> Option<SyntaxNode> {
    let token = root.token_at_offset(offset).right_biased()?;
    token
        .parent_ancestors()
        .find(|n| matches!(n.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
}

fn keyword(method: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    method.children_with_tokens().filter_map(|e| e.into_token()).find(|t| t.kind() == kind)
}

/// The line that starts with `Асинх`/`Процедура`/`Функция`, up to its end. The method node
/// also spans the annotations above it (`&НаСервере`), which are not the declaration.
fn on_declaration_line(text: &str, method: &SyntaxNode, offset: TextSize) -> bool {
    let Some(start) = keyword(method, SyntaxKind::KW_ASYNC)
        .or_else(|| keyword(method, SyntaxKind::KW_PROCEDURE))
        .or_else(|| keyword(method, SyntaxKind::KW_FUNCTION))
        .map(|kw| usize::from(kw.text_range().start()))
    else {
        return false;
    };
    let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    (start..=end).contains(&usize::from(offset))
}

fn may_become_procedure<DB: RootDatabase>(db: &DB, file_id: FileId, method: &SyntaxNode) -> bool {
    let Some(function) = FunctionDef::cast(method.clone()) else {
        return false;
    };
    if function.export_keyword().is_some() || returns_value(method) {
        return false;
    }
    let Some(name) = function.name() else {
        return false;
    };
    let Some(symbol) = Semantics::new(db).symbol_for_token(file_id, &name) else {
        return false;
    };
    let root = db.parse(file_id).syntax_node();
    find_references_in_file(db, file_id, &symbol)
        .into_iter()
        .filter(|reference| reference.range != name.text_range())
        .all(|reference| {
            root.token_at_offset(reference.range.start())
                .right_biased()
                .is_some_and(|token| is_call_statement(&token))
        })
}

/// A bare `Возврат;` holds only tokens; `Возврат <выражение>;` holds the expression node.
fn returns_value(method: &SyntaxNode) -> bool {
    method
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::RETURN_STMT)
        .any(|ret| ret.children().next().is_some())
}

/// The reference is the callee of a call standing alone as a statement (`Имя(...);`), whose
/// result nobody reads. Anything else — an assignment, an argument, a condition, an operand,
/// `Возврат Имя()`, the name without parentheses — reads the result, or is not understood
/// and counts as reading it.
fn is_call_statement(name: &SyntaxToken) -> bool {
    let Some(callee) = name.parent() else {
        return false;
    };
    let Some(call) = callee.parent().filter(|call| call.kind() == SyntaxKind::CALL_EXPR) else {
        return false;
    };
    call.first_child().as_ref() == Some(&callee)
        && call.parent().is_some_and(|stmt| stmt.kind() == SyntaxKind::CALL_STMT)
}

type KeywordPair = (&'static str, &'static str);

fn convert(
    file_id: FileId,
    method: &SyntaxNode,
    (open_kind, end_kind): (SyntaxKind, SyntaxKind),
    (russian, english): (KeywordPair, KeywordPair),
    id: AssistId,
    label: &str,
) -> Option<Assist> {
    let open = keyword(method, open_kind)?;
    let end = keyword(method, end_kind)?;
    let spelled = |token: &SyntaxToken, (ru, en): (&str, &str)| {
        if token.text().is_ascii() {
            en.to_string()
        } else {
            ru.to_string()
        }
    };
    let edits = vec![
        TextEdit { range: open.text_range(), new_text: spelled(&open, (russian.0, english.0)) },
        TextEdit { range: end.text_range(), new_text: spelled(&end, (russian.1, english.1)) },
    ];
    Some(Assist {
        id,
        label: label.to_string(),
        group: None,
        source_change: SourceChange { edits: vec![FileEdit { file_id, edits }] },
    })
}
