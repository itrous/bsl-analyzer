use hir_def::extension_merge::{extract_change_and_validate, strip_directives, Origin};
use syntax::SyntaxKind;

#[test]
fn annotated_inline_directives_keep_existing_effective_text_and_segments() {
    let source = r#"&ИзменениеИКонтроль("Цель")
Функция Расш1_Цель()
    Возврат "ВЫБРАТЬ
    |Т.Ссылка КАК Ссылка,
    #Удаление
    |Т.Старое КАК Старое,
    #КонецУдаления
    #Вставка
    |Т.Новое КАК Новое,
    #КонецВставки
    |Т.Дата КАК Дата
    |ИЗ Таблица КАК Т";
КонецФункции"#;
    let parsed = parser::parse(source);
    assert!(!parsed.has_errors(), "parse errors: {:?}", parsed.errors());
    let method = parsed
        .syntax_node()
        .descendants()
        .find(|node| node.kind() == SyntaxKind::FUNCTION_DEF)
        .expect("function");
    assert_eq!(
        extract_change_and_validate(&method).expect("balanced change-and-validate").target,
        "Цель"
    );
    let body = method.children().find(|node| node.kind() == SyntaxKind::STMT_LIST).expect("body");
    let (effective, segments) = strip_directives(&body);

    assert!(effective.contains("Т.Новое КАК Новое"));
    assert!(!effective.contains("Т.Старое КАК Старое"));
    assert!(!effective.contains("#Вставка") && !effective.contains("#Удаление"));

    let inserted = "Т.Новое КАК Новое";
    let effective_at = effective.find(inserted).expect("inserted effective field") as u32;
    let segment = segments
        .iter()
        .find(|segment| {
            segment.origin == Origin::Inserted
                && u32::from(segment.effective.start()) <= effective_at
                && effective_at < u32::from(segment.effective.end())
        })
        .expect("inserted text keeps an Inserted segment");
    let source_at = source.find(inserted).expect("source field") as u32;
    let mapped =
        u32::from(segment.ext.start()) + effective_at - u32::from(segment.effective.start());
    assert_eq!(mapped, source_at, "segment maps active text to the extension source");
    assert!(
        segments.iter().any(|segment| segment.origin == Origin::Copied),
        "unchanged text keeps Copied origin"
    );
}

#[test]
fn malformed_inline_markers_are_not_accepted_for_merge() {
    for source in [
        "&ИзменениеИКонтроль(\"Цель\")\nФункция X()\nВозврат 1 + #Вставка 2;\nКонецФункции",
        "&ИзменениеИКонтроль(\"Цель\")\nФункция X()\nВозврат 1 + #Удаление 2;\nКонецФункции",
    ] {
        let method = parser::parse(source)
            .syntax_node()
            .descendants()
            .find(|node| node.kind() == SyntaxKind::FUNCTION_DEF)
            .expect("function");
        assert!(
            extract_change_and_validate(&method).is_none(),
            "unbalanced markers must not produce an effective merge"
        );
    }
}
