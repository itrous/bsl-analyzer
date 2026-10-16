//! `textDocument/codeAction` offers refactorings (`refactor.rewrite`) next to the quick
//! fixes of diagnostics, and the `only` filter keeps the two apart: asking for quick fixes
//! or for `source.fixAll` returns no refactoring, asking for refactorings returns no fix.

mod common;

use std::path::PathBuf;

use common::*;
use serde_json::{json, Value};

/// Line 0 is the declaration (the refactoring); line 1 holds `ЭтаФорма`, whose diagnostic
/// carries a safe quick fix (`ЭтотОбъект`).
const SOURCE: &str = "Процедура Тест() Экспорт\r\n    ЭтаФорма.Закрыть();\r\nКонецПроцедуры\r\n";

fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/Main.bsl"), SOURCE).unwrap();
    std::fs::write(dir.path().join("bsl-analyzer.toml"), "[source]\nroot = \"src\"\n").unwrap();
    let path = dir.path().join("src/Main.bsl");
    (dir, path)
}

fn code_actions(only: Option<Value>) -> Vec<Value> {
    let (dir, path) = project();
    let mut lsp = Lsp::start(dir.path());
    lsp.open(&path, SOURCE);
    let uri = lsp_types::Url::from_file_path(&path).unwrap();
    let mut context = json!({"diagnostics": []});
    if let Some(only) = only {
        context["only"] = only;
    }
    let response = lsp.request(
        2,
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri},
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 10}},
            "context": context,
        }),
    );
    assert!(response["error"].is_null(), "codeAction failed: {response}");
    response["result"].as_array().cloned().unwrap_or_default()
}

fn of_kind<'a>(actions: &'a [Value], kind: &str) -> Vec<&'a Value> {
    actions.iter().filter(|action| action["kind"] == kind).collect()
}

#[test]
fn refactoring_is_offered_alongside_quick_fixes() {
    let actions = code_actions(None);

    let refactors = of_kind(&actions, "refactor.rewrite");
    assert_eq!(refactors.len(), 1, "one refactoring expected: {actions:#?}");
    let refactor = refactors[0];
    assert_eq!(refactor["title"], "Преобразовать в функцию");
    assert!(refactor.get("diagnostics").is_none(), "a refactoring fixes no diagnostic");
    let edits = refactor["edit"]["changes"]
        .as_object()
        .and_then(|changes| changes.values().next())
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("refactoring carries no edits: {refactor}"));
    assert_eq!(
        edits,
        &vec![
            json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 9}}, "newText": "Функция"}),
            json!({"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 14}}, "newText": "КонецФункции"}),
        ]
    );

    let quick_fixes = of_kind(&actions, "quickfix");
    assert!(
        quick_fixes.iter().any(|fix| fix["diagnostics"].is_array()),
        "the diagnostic's quick fix must still be offered: {actions:#?}"
    );
    assert_eq!(of_kind(&actions, "source.fixAll.bsl-analyzer").len(), 1, "{actions:#?}");
}

#[test]
fn only_quick_fixes_or_fix_all_returns_no_refactoring() {
    let quick_fixes = code_actions(Some(json!(["quickfix"])));
    assert!(of_kind(&quick_fixes, "refactor.rewrite").is_empty(), "{quick_fixes:#?}");
    assert!(!of_kind(&quick_fixes, "quickfix").is_empty(), "{quick_fixes:#?}");

    let fix_all = code_actions(Some(json!(["source.fixAll"])));
    assert!(of_kind(&fix_all, "refactor.rewrite").is_empty(), "{fix_all:#?}");
    assert_eq!(of_kind(&fix_all, "source.fixAll.bsl-analyzer").len(), 1, "{fix_all:#?}");
}

#[test]
fn only_refactor_returns_the_refactoring_and_no_fix() {
    let actions = code_actions(Some(json!(["refactor"])));
    assert_eq!(of_kind(&actions, "refactor.rewrite").len(), 1, "{actions:#?}");
    assert!(of_kind(&actions, "quickfix").is_empty(), "{actions:#?}");
    assert!(of_kind(&actions, "source.fixAll.bsl-analyzer").is_empty(), "{actions:#?}");
}
