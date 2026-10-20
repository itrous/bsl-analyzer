use sdbl_hir::detect_sdbl_at_position;
use syntax::TextSize;

fn at(source: &str, needle: &str) -> TextSize {
    TextSize::from(source.find(needle).unwrap_or_else(|| panic!("missing {needle:?}")) as u32)
}

#[test]
fn cursor_uses_active_query_projection_and_rejects_deleted_text() {
    let source = r#"Функция Тест()
    Текст = "ВЫБРАТЬ
    #Удаление
    |СтароеПоле КАК Старое,
    #КонецУдаления
    #Вставка
    |НовоеПоле КАК Новое,
    #КонецВставки
    |Дата КАК Дата
    |ИЗ Таблица";
КонецФункции"#;
    let root = parser::parse(source).syntax_node();

    let active = detect_sdbl_at_position(&root, at(source, "НовоеПоле"))
        .expect("inserted query text is active under the cursor");
    assert_eq!(active.query_text, "ВЫБРАТЬ\nНовоеПоле КАК Новое,\nДата КАК Дата\nИЗ Таблица");
    assert_eq!(usize::from(active.offset_in_query), active.query_text.find("НовоеПоле").unwrap());

    assert!(
        detect_sdbl_at_position(&root, at(source, "СтароеПоле")).is_none(),
        "deleted query text must not activate SDBL features"
    );
    assert!(
        detect_sdbl_at_position(&root, at(source, "#Вставка")).is_none(),
        "a marker is not query text"
    );
}

#[test]
fn cursor_mapping_handles_crlf_and_escaped_quotes() {
    let source = concat!(
        "Функция Тест()\r\n",
        "    Текст = \"ВЫБРАТЬ \"\"ёж\"\" КАК Имя,\r\n",
        "    #Вставка\r\n",
        "    |Поле КАК Поле\r\n",
        "    #КонецВставки\r\n",
        "    |ИЗ Таблица\";\r\n",
        "КонецФункции"
    );
    let root = parser::parse(source).syntax_node();
    let info = detect_sdbl_at_position(&root, at(source, "Поле КАК"))
        .expect("inserted CRLF line is query text");
    assert_eq!(info.query_text, "ВЫБРАТЬ \"ёж\" КАК Имя,\nПоле КАК Поле\nИЗ Таблица");
    assert_eq!(usize::from(info.offset_in_query), info.query_text.find("Поле КАК").unwrap());

    let quote = source.find("ёж").unwrap() as u32 - 2;
    let quoted = detect_sdbl_at_position(&root, TextSize::from(quote))
        .expect("escaped quote remains in query text");
    assert_eq!(usize::from(quoted.offset_in_query), quoted.query_text.find('"').unwrap());
}
