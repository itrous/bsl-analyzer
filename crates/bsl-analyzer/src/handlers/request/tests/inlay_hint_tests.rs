use super::*;

/// Numbered parameter hints must survive UTF-16 conversion at every multiline argument.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn inlay_hints_expand_variadic_parameters_at_lsp_positions() {
    let mut state = create_test_state();
    state.init_empty_source_root();
    let uri = lsp_types::Url::parse("file:///variadic-inlay-hints.bsl").unwrap();
    let source = "Процедура Тест(Текст1, Текст2, Текст3)\n    Текст = СтрШаблон(\n        \"%1, %2, %3\",\n        Текст1,\n        Текст2,\n        Текст3);\nКонецПроцедуры\n";
    open_source(&mut state, &uri, source);
    let params = InlayHintParams {
        work_done_progress_params: Default::default(),
        text_document: TextDocumentIdentifier { uri },
        range: lsp_types::Range {
            start: Position { line: 1, character: 0 },
            end: Position { line: 6, character: 0 },
        },
    };
    let hints = handle_inlay_hint(&latency_ctx(&state), params).unwrap().unwrap();
    let rendered: Vec<_> = hints
        .into_iter()
        .map(|hint| {
            assert_eq!(hint.kind, Some(LspInlayHintKind::PARAMETER));
            assert_eq!(hint.padding_right, Some(true));
            let InlayHintLabel::String(label) = hint.label else {
                panic!("expected string label");
            };
            (label, hint.position)
        })
        .collect();
    assert_eq!(
        rendered,
        vec![
            ("Шаблон:".to_string(), Position { line: 2, character: 8 }),
            ("Значение1:".to_string(), Position { line: 3, character: 8 }),
            ("Значение2:".to_string(), Position { line: 4, character: 8 }),
            ("Значение3:".to_string(), Position { line: 5, character: 8 }),
        ],
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn inlay_hints_preserve_repeated_nested_receiver_labels_at_lsp_positions() {
    let mut state = create_test_state();
    state.init_empty_source_root();

    let uri = crate::test_uri::file_uri("repeated-inlay-hints.bsl");
    let source = "Процедура Тест()\n    Массив = Новый Массив;\n    Список = Новый СписокЗначений;\n    Массив.Добавить(1);\n    Список.Добавить(2);\n    Массив.Добавить(Массив.Добавить(3));\n    Массив.Добавить(,\nКонецПроцедуры\n";
    open_source(&mut state, &uri, source);

    let params = InlayHintParams {
        work_done_progress_params: Default::default(),
        text_document: TextDocumentIdentifier { uri },
        range: lsp_types::Range {
            start: Position { line: 3, character: 0 },
            end: Position { line: 7, character: 0 },
        },
    };

    let hints = handle_inlay_hint(&latency_ctx(&state), params).unwrap().unwrap();
    let rendered: Vec<(String, Position)> = hints
        .into_iter()
        .map(|hint| {
            let InlayHintLabel::String(label) = hint.label else {
                panic!("expected string label");
            };
            (label, hint.position)
        })
        .collect();

    assert_eq!(
        rendered,
        vec![
            ("Значение:".to_string(), Position { line: 3, character: 20 }),
            ("Значение:".to_string(), Position { line: 4, character: 20 }),
            ("Значение:".to_string(), Position { line: 5, character: 20 }),
            ("Значение:".to_string(), Position { line: 5, character: 36 }),
        ],
    );
}
