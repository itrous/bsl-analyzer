//! Ведущие комментарии объявлений над настоящими сериями комментариев.
//!
//! Сборщики берут комментарии из серий дерева, а не из текста, поэтому текст
//! разбирается полностью, и тест интеграционный.

use syntax::{
    ast_utils::has_variable_leading_description, comment_runs,
    extract_leading_comment_lines_at_offset, extract_leading_comments,
    extract_leading_comments_at_offset, extract_variable_comments_at_offset, CommentRun,
    SyntaxKind,
};

fn runs(text: &str) -> Vec<CommentRun> {
    comment_runs(&parser::parse(text).syntax_node())
}

fn off(text: &str, marker: &str) -> usize {
    text.find(marker).unwrap_or_else(|| panic!("marker {marker:?} not found in {text:?}"))
}

mod variable_comment_extractor_tests {
    use super::*;

    #[test]
    fn no_comments_returns_none() {
        let text = "Перем X;";
        let var_kw = off(text, "Перем");
        let var_end = text.len();
        assert_eq!(
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)),
            None
        );
    }

    #[test]
    fn leading_single_line() {
        let text = "// purpose\nПерем X;";
        let var_kw = off(text, "Перем");
        let var_end = text.len();
        let got =
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)).unwrap();
        assert_eq!(got, vec!["purpose".to_string()]);
    }

    #[test]
    fn leading_content_strips_exactly_one_comment_marker() {
        let text = "//// literal\nПерем X;";
        let got = extract_variable_comments_at_offset(
            text,
            off(text, "Перем"),
            text.len(),
            None,
            &runs(text),
        )
        .unwrap();
        assert_eq!(got, vec!["// literal".to_string()]);
    }

    #[test]
    fn indentation_before_variable_anchor_is_not_a_barrier() {
        for (text, keyword) in [("// docs\n  Перем X;", "Перем"), ("// docs\n\tVar X;", "Var")]
        {
            assert_eq!(
                extract_variable_comments_at_offset(
                    text,
                    off(text, keyword),
                    text.len(),
                    None,
                    &runs(text)
                ),
                Some(vec!["docs".to_string()])
            );
        }
    }

    #[test]
    fn leading_multiline_block() {
        let text = "// first\n// second\nПерем X;";
        let var_kw = off(text, "Перем");
        let var_end = text.len();
        let got =
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)).unwrap();
        assert_eq!(got, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn blank_line_breaks_leading() {
        let text = "// far away\n\nПерем X;";
        let var_kw = off(text, "Перем");
        let var_end = text.len();
        assert_eq!(
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)),
            None
        );
    }

    #[test]
    fn trailing_only() {
        let text = "Перем X; // trailing";
        let var_kw = off(text, "Перем");
        let var_end = off(text, ";") + 1;
        let got =
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)).unwrap();
        assert_eq!(got, vec!["trailing".to_string()]);
    }

    #[test]
    fn empty_trailing_marker_filtered() {
        let text = "Перем X; //";
        let var_kw = off(text, "Перем");
        let var_end = off(text, ";") + 1;
        assert_eq!(
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)),
            None
        );
    }

    #[test]
    fn empty_leading_marker_filtered() {
        let text = "//\nПерем X;";
        let var_kw = off(text, "Перем");
        let var_end = text.len();
        assert_eq!(
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)),
            None
        );
    }

    #[test]
    fn leading_then_trailing_combined() {
        let text = "// purpose\nПерем X; // remark";
        let var_kw = off(text, "Перем");
        let var_end = off(text, ";") + 1;
        let got =
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)).unwrap();
        assert_eq!(got, vec!["purpose".to_string(), "remark".to_string()]);
    }

    #[test]
    fn inter_annotation_capture() {
        let text = "&Идентификатор\n// inter\n&Колонка\nПерем X;";
        let var_kw = off(text, "Перем");
        let first_ann = off(text, "&Идентификатор");
        let var_end = text.len();
        let got = extract_variable_comments_at_offset(
            text,
            var_kw,
            var_end,
            Some(first_ann),
            &runs(text),
        )
        .unwrap();
        assert_eq!(got, vec!["inter".to_string()]);
    }

    #[test]
    fn leading_above_first_annotation() {
        let text = "// header\n&Идентификатор\nПерем X;";
        let var_kw = off(text, "Перем");
        let first_ann = off(text, "&Идентификатор");
        let var_end = text.len();
        let got = extract_variable_comments_at_offset(
            text,
            var_kw,
            var_end,
            Some(first_ann),
            &runs(text),
        )
        .unwrap();
        assert_eq!(got, vec!["header".to_string()]);
    }

    #[test]
    fn trailing_with_annotations() {
        let text = "&Идентификатор\nПерем X; // tail";
        let var_kw = off(text, "Перем");
        let first_ann = off(text, "&Идентификатор");
        let var_end = off(text, ";") + 1;
        let got = extract_variable_comments_at_offset(
            text,
            var_kw,
            var_end,
            Some(first_ann),
            &runs(text),
        )
        .unwrap();
        assert_eq!(got, vec!["tail".to_string()]);
    }

    #[test]
    fn leading_blank_above_annotation_breaks_connection() {
        let text = "// orphan\n\n&Идентификатор\nПерем X;";
        let var_kw = off(text, "Перем");
        let first_ann = off(text, "&Идентификатор");
        let var_end = text.len();
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                var_kw,
                var_end,
                Some(first_ann),
                &runs(text)
            ),
            None
        );
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let text = "// purpose\r\nПерем X;\r\n";
        let var_kw = off(text, "Перем");
        let var_end = off(text, ";") + 1;
        let got =
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)).unwrap();
        assert_eq!(got, vec!["purpose".to_string()]);
    }

    #[test]
    fn cyrillic_variable_name_offsets() {
        let text = "// заголовок\nПерем СчётчикВызовов; // примечание";
        let var_kw = off(text, "Перем");
        let var_end = off(text, ";") + 1;
        let got =
            extract_variable_comments_at_offset(text, var_kw, var_end, None, &runs(text)).unwrap();
        assert_eq!(got, vec!["заголовок".to_string(), "примечание".to_string()]);
    }

    #[test]
    fn all_three_regions_combined() {
        let text = "// header\n&Идентификатор\n// inter\n&Колонка\nПерем X; // tail";
        let var_kw = off(text, "Перем");
        let first_ann = off(text, "&Идентификатор");
        let var_end = off(text, ";") + 1;
        let got = extract_variable_comments_at_offset(
            text,
            var_kw,
            var_end,
            Some(first_ann),
            &runs(text),
        )
        .unwrap();
        assert_eq!(got, vec!["header".to_string(), "inter".to_string(), "tail".to_string()]);
    }

    #[test]
    fn var_anchor_without_first_annotation_crosses_annotation_runs() {
        let text = "// first\n&AtClient\n// second\nVar X;";
        let got = extract_variable_comments_at_offset(
            text,
            off(text, "Var"),
            text.len(),
            None,
            &runs(text),
        )
        .unwrap();
        assert_eq!(got, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn annotation_tail_is_skipped_but_not_collected() {
        let text = "// first\n&AtClient // ignored\n// second\nVar X;";
        let got = extract_variable_comments_at_offset(
            text,
            off(text, "Var"),
            text.len(),
            None,
            &runs(text),
        )
        .unwrap();
        assert_eq!(got, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn annotation_immediately_before_variable_is_skipped_before_any_comment_is_accepted() {
        for text in ["// first\n&AtClient\nVar X;", "// first\n&AtClient // ignored\nVar X;"] {
            assert_eq!(
                extract_variable_comments_at_offset(
                    text,
                    off(text, "Var"),
                    text.len(),
                    None,
                    &runs(text)
                ),
                Some(vec!["first".to_string()])
            );
        }
    }

    #[test]
    fn blank_line_next_to_annotation_stops_leading_comments() {
        let text = "// first\n\n&AtClient // ignored\n// second\nVar X;";
        let got = extract_variable_comments_at_offset(
            text,
            off(text, "Var"),
            text.len(),
            None,
            &runs(text),
        )
        .unwrap();
        assert_eq!(got, vec!["second".to_string()]);
    }

    #[test]
    fn partial_comment_fragment_before_variable_anchor_is_collected() {
        let text = "// before\n// fragment after\nVar X;";
        let anchor = off(text, " after");
        let got = extract_variable_comments_at_offset(text, anchor, text.len(), None, &runs(text))
            .unwrap();
        assert_eq!(got, vec!["before".to_string(), "fragment".to_string()]);
    }

    #[test]
    fn variable_anchor_between_comment_slashes_is_a_barrier() {
        let text = "// before\n// fragment\nVar X;";
        let anchor = off(text, "// fragment") + 1;
        assert_eq!(
            extract_variable_comments_at_offset(text, anchor, text.len(), None, &runs(text)),
            None
        );
    }

    #[test]
    fn code_before_partial_variable_comment_anchor_is_a_barrier() {
        let text = "// before\nX = 1; // fragment after\nVar X;";
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                off(text, " after"),
                text.len(),
                None,
                &runs(text)
            ),
            None
        );
    }

    #[test]
    fn comments_absent_from_runs_are_not_discovered_in_raw_text() {
        let text = "// raw\nПерем X;";
        assert_eq!(
            extract_variable_comments_at_offset(text, off(text, "Перем"), text.len(), None, &[]),
            None
        );
    }

    #[test]
    fn comments_absent_from_a_nonempty_run_slice_are_not_discovered() {
        let text = "// decoy\n\n// raw\nПерем X;";
        let decoy_runs = runs("// decoy\n\n");
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                off(text, "Перем"),
                text.len(),
                None,
                &decoy_runs
            ),
            None
        );
    }

    #[test]
    fn empty_markers_connect_variable_comments_but_have_no_content() {
        let text = "// first\n//\n// second\nПерем X;";
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                off(text, "Перем"),
                text.len(),
                None,
                &runs(text)
            ),
            Some(vec!["first".to_string(), "second".to_string()])
        );

        let only_empty = "//\n//\nПерем X;";
        assert_eq!(
            extract_variable_comments_at_offset(
                only_empty,
                off(only_empty, "Перем"),
                only_empty.len(),
                None,
                &runs(only_empty)
            ),
            None
        );
    }

    #[test]
    fn code_between_comment_and_variable_is_a_barrier() {
        let text = "// far\nX = 1;\nПерем Y;";
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                off(text, "Перем"),
                text.len(),
                None,
                &runs(text)
            ),
            None
        );
    }

    #[test]
    fn code_on_variable_anchor_line_is_a_barrier() {
        let text = "// far\nX = 1; Перем Y;";
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                off(text, "Перем"),
                text.len(),
                None,
                &runs(text)
            ),
            None
        );
    }

    #[test]
    fn code_tail_in_a_variable_run_blocks_earlier_comments() {
        let with_near = "// far\nX = 1; // tail\n// near\nVar Y;";
        assert_eq!(
            extract_variable_comments_at_offset(
                with_near,
                off(with_near, "Var"),
                with_near.len(),
                None,
                &runs(with_near)
            ),
            Some(vec!["near".to_string()])
        );

        let without_near = "// far\nX = 1; // tail\nVar Y;";
        assert_eq!(
            extract_variable_comments_at_offset(
                without_near,
                off(without_near, "Var"),
                without_near.len(),
                None,
                &runs(without_near)
            ),
            None
        );
    }

    #[test]
    fn variable_bom_is_a_barrier_but_unicode_space_is_indentation() {
        for bom in [
            "\u{feff}// blocked\n// near\nПерем X;",
            "// far\n\u{feff}// blocked\n// near\nПерем X;",
        ] {
            assert_eq!(
                extract_variable_comments_at_offset(
                    bom,
                    off(bom, "Перем"),
                    bom.len(),
                    None,
                    &runs(bom)
                ),
                Some(vec!["near".to_string()])
            );
        }

        let unicode_space = "\u{2003}// near\nПерем X;";
        assert_eq!(
            extract_variable_comments_at_offset(
                unicode_space,
                off(unicode_space, "Перем"),
                unicode_space.len(),
                None,
                &runs(unicode_space)
            ),
            Some(vec!["near".to_string()])
        );
    }

    #[test]
    fn blank_line_between_annotation_and_variable_is_a_barrier() {
        let text = "// first\n&AtClient\n\nVar X;";
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                off(text, "Var"),
                text.len(),
                None,
                &runs(text)
            ),
            None
        );
    }

    #[test]
    fn whitespace_only_line_next_to_annotation_is_a_barrier() {
        for text in ["// first\n \t \n&AtClient\nVar X;", "// first\n&AtClient\n \t \nVar X;"] {
            assert_eq!(
                extract_variable_comments_at_offset(
                    text,
                    off(text, "Var"),
                    text.len(),
                    None,
                    &runs(text)
                ),
                None
            );
        }
    }

    #[test]
    fn indented_annotation_and_its_tail_are_skipped() {
        let text = "// first\n  &AtClient // ignored\n// second\nVar X;";
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                off(text, "Var"),
                text.len(),
                None,
                &runs(text)
            ),
            Some(vec!["first".to_string(), "second".to_string()])
        );
    }

    #[test]
    fn var_anchor_crosses_multiple_annotation_runs() {
        let text = "// first\n&AtClient\n// second\n&AtServer\n// third\nVar X;";
        let got = extract_variable_comments_at_offset(
            text,
            off(text, "Var"),
            text.len(),
            None,
            &runs(text),
        )
        .unwrap();
        assert_eq!(got, vec!["first".to_string(), "second".to_string(), "third".to_string()]);
    }

    #[test]
    fn comment_below_partial_variable_anchor_is_not_collected() {
        let text = "// fragment after\n// below\nVar X;";
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                off(text, " after"),
                text.len(),
                None,
                &runs(text)
            ),
            Some(vec!["fragment".to_string()])
        );
    }

    #[test]
    fn zero_variable_anchor_does_not_clamp_to_real_comments() {
        let text = "// raw\nПерем X;";
        assert_eq!(
            extract_variable_comments_at_offset(text, 0, text.len(), None, &runs(text)),
            None
        );
    }

    #[test]
    fn crlf_blank_line_is_a_variable_barrier() {
        let text = "// far\r\n\r\nПерем X;";
        assert_eq!(
            extract_variable_comments_at_offset(
                text,
                off(text, "Перем"),
                text.len(),
                None,
                &runs(text)
            ),
            None
        );
    }
}

mod leading_comment_scan_tests {
    use super::*;

    #[test]
    fn comment_block_above_method() {
        let text = "// Описание.\n// Вторая строка.\nПроцедура П()";
        let got =
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)).unwrap();
        assert_eq!(got, vec!["Описание.".to_string(), "Вторая строка.".to_string()]);
    }

    #[test]
    fn blank_line_above_method_detaches_the_block() {
        let text = "// первый\n\n// второй\n\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            None
        );
    }

    #[test]
    fn blank_line_inside_block_keeps_only_adjacent_part() {
        let text = "// далёкий\n\n// ближний\nПроцедура П()";
        let got =
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)).unwrap();
        assert_eq!(got, vec!["ближний".to_string()]);
    }

    #[test]
    fn whitespace_only_line_is_a_method_barrier() {
        let text = "// far\n \t \nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            None
        );
    }

    #[test]
    fn code_line_stops_the_scan() {
        let text = "// далёкий\nКонецПроцедуры\n\n// ближний\nПроцедура П()";
        let got =
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)).unwrap();
        assert_eq!(got, vec!["ближний".to_string()]);
    }

    #[test]
    fn code_between_comment_and_method_is_a_barrier() {
        let text = "// far\nX = 1;\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            None
        );
    }

    #[test]
    fn crlf_comments_are_trimmed() {
        let text = "// заметка\r\nПроцедура П()";
        let got =
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)).unwrap();
        assert_eq!(got, vec!["заметка".to_string()]);
    }

    #[test]
    fn comment_at_file_start() {
        let text = "// шапка\nПроцедура П()";
        let got =
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)).unwrap();
        assert_eq!(got, vec!["шапка".to_string()]);
    }

    #[test]
    fn content_strips_exactly_one_comment_marker() {
        let text = "//// literal\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            Some(vec!["// literal".to_string()])
        );
    }

    #[test]
    fn indentation_before_method_anchor_is_not_a_barrier() {
        for (text, keyword) in
            [("// docs\n  Процедура П()", "Процедура"), ("// docs\n\tProcedure P()", "Procedure")]
        {
            assert_eq!(
                extract_leading_comments_at_offset(off(text, keyword), text, &runs(text)),
                Some(vec!["docs".to_string()])
            );
        }
    }

    #[test]
    fn no_comments_returns_none() {
        let text = "КонецПроцедуры\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура П"), text, &runs(text)),
            None
        );
        assert_eq!(extract_leading_comments_at_offset(0, text, &runs(text)), None);
    }

    #[test]
    fn comments_above_annotation_anchor_are_attached() {
        // Callers anchor the offset at the annotation when a method has one,
        // so the doc block right above the annotation is found.
        let text = "// Описание.\n&НаСервере\nПроцедура П()";
        let got =
            extract_leading_comments_at_offset(off(text, "&НаСервере"), text, &runs(text)).unwrap();
        assert_eq!(got, vec!["Описание.".to_string()]);
    }

    #[test]
    fn node_wrapper_uses_runs_from_the_whole_tree() {
        let text = "// docs\nПроцедура П()\nКонецПроцедуры";
        let parse = parser::parse(text);
        let procedure = parse
            .syntax_node()
            .descendants()
            .find(|node| node.kind() == SyntaxKind::PROCEDURE_DEF)
            .expect("procedure node");
        assert_eq!(extract_leading_comments(&procedure, text), Some(vec!["docs".to_string()]));
    }

    #[test]
    fn code_before_offset_on_same_line_returns_none() {
        let text = "// Описание.\nПерем А; Процедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            None
        );
    }

    #[test]
    fn empty_marker_comments_are_dropped() {
        let text = "//\n// текст\n//\nПроцедура П()";
        let got =
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)).unwrap();
        assert_eq!(got, vec!["текст".to_string()]);
    }

    #[test]
    fn only_empty_markers_return_none() {
        let text = "//\n//\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            None
        );
    }

    #[test]
    fn comments_absent_from_runs_are_not_discovered_in_raw_text() {
        let text = "// raw\nПроцедура П()";
        assert_eq!(extract_leading_comments_at_offset(off(text, "Процедура"), text, &[]), None);
    }

    #[test]
    fn comments_absent_from_a_nonempty_run_slice_are_not_discovered() {
        let text = "// decoy\n\n// raw\nПроцедура П()";
        let decoy_runs = runs("// decoy\n\n");
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &decoy_runs),
            None
        );
    }

    #[test]
    fn trailing_comment_in_same_run_blocks_earlier_comment() {
        let text = "// far\nX = 1; // tail\n// near\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            Some(vec!["near".to_string()])
        );
    }

    #[test]
    fn tail_before_near_comment_is_not_documentation() {
        let text = "X = 1; // tail\n// near\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            Some(vec!["near".to_string()])
        );
    }

    #[test]
    fn tail_alone_is_not_method_documentation() {
        let text = "X = 1; // tail\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            None
        );
    }

    #[test]
    fn bom_opens_the_file_but_is_a_barrier_inside_it() {
        let leading = "\u{feff}// first\n// near\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(leading, "Процедура"), leading, &runs(leading)),
            Some(vec!["first".to_string(), "near".to_string()])
        );

        let inner = "// far\n\u{feff}// blocked\n// near\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(inner, "Процедура"), inner, &runs(inner)),
            Some(vec!["near".to_string()])
        );
    }

    #[test]
    fn unicode_space_is_indentation() {
        let unicode_space = "\u{2003}// near\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(
                off(unicode_space, "Процедура"),
                unicode_space,
                &runs(unicode_space)
            ),
            Some(vec!["near".to_string()])
        );
    }

    #[test]
    fn comments_below_anchor_in_same_run_are_not_collected() {
        let text = "// above\nПроцедура П() // below";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            Some(vec!["above".to_string()])
        );
    }

    #[test]
    fn method_anchor_inside_comment_ignores_its_own_fragment() {
        let text = "// before\n// fragment after\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, " after"), text, &runs(text)),
            Some(vec!["before".to_string()])
        );
    }

    #[test]
    fn method_anchor_between_comment_slashes_is_a_barrier() {
        let text = "// before\n// fragment\nПроцедура П()";
        let anchor = off(text, "// fragment") + 1;
        assert_eq!(extract_leading_comments_at_offset(anchor, text, &runs(text)), None);
    }

    #[test]
    fn code_before_partial_method_comment_anchor_is_a_barrier() {
        let text = "// before\nX = 1; // fragment after\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, " after"), text, &runs(text)),
            None
        );
    }

    #[test]
    fn crlf_blank_line_is_a_method_barrier() {
        let text = "// far\r\n\r\nПроцедура П()";
        assert_eq!(
            extract_leading_comments_at_offset(off(text, "Процедура"), text, &runs(text)),
            None
        );
    }

    #[test]
    fn invalid_method_offsets_do_not_clamp_to_real_comments() {
        let text = "// raw\nПроцедура П()";
        assert_eq!(extract_leading_comments_at_offset(0, text, &runs(text)), None);
        assert_eq!(extract_leading_comments_at_offset(text.len() + 1, text, &runs(text)), None);
    }

    #[test]
    fn variable_description_above_annotation() {
        let text = "// назначение\n&НаКлиенте\nПерем X;";
        assert!(has_variable_leading_description(off(text, "Перем"), text, Some(off(text, "&"))));
    }

    #[test]
    fn variable_without_description() {
        let text = "КонецПроцедуры\n&НаКлиенте\nПерем X;";
        assert!(!has_variable_leading_description(off(text, "Перем"), text, Some(off(text, "&"))));
    }

    #[test]
    fn annotation_on_first_line_only() {
        let text = "&НаКлиенте\nПерем X;";
        assert!(!has_variable_leading_description(off(text, "Перем"), text, Some(0)));
    }

    #[test]
    fn anchor_at_zero_returns_false() {
        assert!(!has_variable_leading_description(0, "Перем X;", None));
    }
}

mod leading_comment_line_tests {
    use super::*;

    /// Documentation needs indentation and empty comment lines even when source uses CRLF.
    #[test]
    fn documentation_layout_and_legacy_trimmed_comments() {
        let source = "// Returns:\r\n//\r\n//   Structure:\r\n//     * Field - String\r\nFunction Test()\r\nEndFunction";
        let offset = off(source, "Function");
        assert_eq!(
            extract_leading_comment_lines_at_offset(offset, source, &runs(source)).unwrap(),
            ["Returns:", "", "  Structure:", "    * Field - String"]
        );
        assert_eq!(
            extract_leading_comments_at_offset(offset, source, &runs(source)).unwrap(),
            ["Returns:", "Structure:", "* Field - String"]
        );
    }

    /// An actual blank source line still breaks the association with a method.
    #[test]
    fn blank_source_line_breaks_documentation() {
        let source = "// Returns:\n\nFunction Test()\nEndFunction";
        assert!(extract_leading_comment_lines_at_offset(
            off(source, "Function"),
            source,
            &runs(source)
        )
        .is_none());
    }

    #[test]
    fn only_empty_markers_give_none() {
        let source = "//\n//\nFunction Test()\nEndFunction";
        assert!(extract_leading_comment_lines_at_offset(
            off(source, "Function"),
            source,
            &runs(source)
        )
        .is_none());
    }

    /// A UTF-8 BOM must not hide the first documentation line from hover.
    #[test]
    fn bom_keeps_the_first_documentation_line() {
        let source =
            "\u{feff}// Parameters:\r\n// Value - String\r\nProcedure Test(Value)\r\nEndProcedure";
        assert_eq!(
            extract_leading_comment_lines_at_offset(
                off(source, "Procedure"),
                source,
                &runs(source)
            )
            .unwrap(),
            ["Parameters:", "Value - String"]
        );
    }
}
