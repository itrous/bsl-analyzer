use super::*;

use lsp_types::{
    CallHierarchyPrepareParams, DocumentHighlightParams, DocumentOnTypeFormattingParams,
    FormattingOptions, HoverParams, ReferenceParams, RenameParams,
};

/// A request computed on text the server has not received yet arrives with a position
/// past the end of the buffer the server holds. "Nothing here" is the only sensible
/// answer: an error is logged by the editor as an LSP failure for every such request.
const SOURCE: &str =
    "Процедура Цель()\nКонецПроцедуры\n\nПроцедура Вызов()\n    Цель();\nКонецПроцедуры\n";

/// On the call site `Цель();`, inside the identifier.
const VALID: Position = Position { line: 4, character: 5 };
/// Line past the end of the file, as after a deletion shortened the buffer. A column past
/// the end of an existing line is clamped by the position conversion and never fails.
const PAST_LAST_LINE: Position = Position { line: 999, character: 0 };

fn opened_state() -> (GlobalState, lsp_types::Url, tempfile::TempDir) {
    let mut state = create_test_state();
    state.init_empty_source_root();
    let dir = tempfile::tempdir().unwrap();
    let uri = lsp_types::Url::from_file_path(dir.path().join("PositionRace.bsl")).unwrap();
    open_source(&mut state, &uri, SOURCE);
    (state, uri, dir)
}

fn position_params(uri: &lsp_types::Url, position: Position) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position,
    }
}

fn goto_params(uri: &lsp_types::Url, position: Position) -> GotoDefinitionParams {
    GotoDefinitionParams {
        text_document_position_params: position_params(uri, position),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn hover_params(uri: &lsp_types::Url, position: Position) -> HoverParams {
    HoverParams {
        text_document_position_params: position_params(uri, position),
        work_done_progress_params: Default::default(),
    }
}

fn references_params(uri: &lsp_types::Url, position: Position) -> ReferenceParams {
    ReferenceParams {
        text_document_position: position_params(uri, position),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: lsp_types::ReferenceContext { include_declaration: true },
    }
}

fn rename_params(uri: &lsp_types::Url, position: Position) -> RenameParams {
    RenameParams {
        text_document_position: position_params(uri, position),
        new_name: "Другая".to_string(),
        work_done_progress_params: Default::default(),
    }
}

fn highlight_params(uri: &lsp_types::Url, position: Position) -> DocumentHighlightParams {
    DocumentHighlightParams {
        text_document_position_params: position_params(uri, position),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

fn call_hierarchy_params(uri: &lsp_types::Url, position: Position) -> CallHierarchyPrepareParams {
    CallHierarchyPrepareParams {
        text_document_position_params: position_params(uri, position),
        work_done_progress_params: Default::default(),
    }
}

fn on_type_params(uri: &lsp_types::Url, position: Position) -> DocumentOnTypeFormattingParams {
    DocumentOnTypeFormattingParams {
        text_document_position: position_params(uri, position),
        ch: "\n".to_string(),
        options: FormattingOptions { tab_size: 4, insert_spaces: false, ..Default::default() },
    }
}

/// Every position request must answer "nothing here" (`Ok(None)`), not an error, for a
/// position the buffer no longer has.
#[test]
fn position_requests_past_the_buffer_answer_nothing_instead_of_an_error() {
    let (state, uri, _dir) = opened_state();
    let ctx = latency_ctx(&state);

    let position = PAST_LAST_LINE;
    let mut wrong = Vec::new();
    let mut check = |name: &str, nothing: bool, got: String| {
        if !nothing {
            wrong.push(format!("{name}: {got}"));
        }
    };

    let r = handle_goto_definition(&ctx, goto_params(&uri, position));
    check("goto_definition", matches!(r, Ok(None)), format!("{r:?}"));

    let r = handle_type_definition(&ctx, goto_params(&uri, position));
    check("type_definition", matches!(r, Ok(None)), format!("{r:?}"));

    let r = handle_find_references(&ctx, references_params(&uri, position));
    check("references", matches!(r, Ok(None)), format!("{r:?}"));

    let r = handle_prepare_rename(&ctx, position_params(&uri, position));
    check("prepare_rename", matches!(r, Ok(None)), format!("{r:?}"));

    let r = handle_rename(&ctx, rename_params(&uri, position));
    check("rename", matches!(r, Ok(None)), format!("{r:?}"));

    let r = handle_document_highlight(&ctx, highlight_params(&uri, position));
    check("document_highlight", matches!(r, Ok(None)), format!("{r:?}"));

    let r = handle_hover(&ctx, hover_params(&uri, position));
    check("hover", matches!(r, Ok(None)), format!("{r:?}"));

    let r = handle_prepare_call_hierarchy(&ctx, call_hierarchy_params(&uri, position));
    check("prepare_call_hierarchy", matches!(r, Ok(None)), format!("{r:?}"));

    let r = handle_on_type_formatting(state.snapshot(), on_type_params(&uri, position));
    check("on_type_formatting", matches!(r, Ok(None)), format!("{r:?}"));

    assert!(wrong.is_empty(), "handlers that did not answer Ok(None):\n{}", wrong.join("\n"));
}

/// Control: the guard must not turn real answers into "nothing here". The same requests
/// at a position the buffer does have still return a result.
#[test]
fn position_requests_inside_the_buffer_still_answer() {
    let (state, uri, _dir) = opened_state();
    let ctx = latency_ctx(&state);

    let r = handle_goto_definition(&ctx, goto_params(&uri, VALID)).unwrap();
    assert!(r.is_some(), "goto_definition found nothing at a valid position");

    let r = handle_find_references(&ctx, references_params(&uri, VALID)).unwrap();
    assert!(r.is_some(), "references found nothing at a valid position");

    let r = handle_prepare_rename(&ctx, position_params(&uri, VALID)).unwrap();
    assert!(r.is_some(), "prepare_rename found nothing at a valid position");

    let r = handle_rename(&ctx, rename_params(&uri, VALID)).unwrap();
    assert!(r.is_some(), "rename found nothing at a valid position");

    let r = handle_document_highlight(&ctx, highlight_params(&uri, VALID)).unwrap();
    assert!(r.is_some(), "document_highlight found nothing at a valid position");

    let r = handle_hover(&ctx, hover_params(&uri, VALID)).unwrap();
    assert!(r.is_some(), "hover found nothing at a valid position");

    let r = handle_prepare_call_hierarchy(&ctx, call_hierarchy_params(&uri, VALID)).unwrap();
    assert!(r.is_some(), "prepare_call_hierarchy found nothing at a valid position");

    // These two may legitimately have nothing to say here; what matters is that a valid
    // position is not an error.
    handle_type_definition(&ctx, goto_params(&uri, VALID)).expect("type_definition");
    handle_on_type_formatting(state.snapshot(), on_type_params(&uri, VALID))
        .expect("on_type_formatting");
}
