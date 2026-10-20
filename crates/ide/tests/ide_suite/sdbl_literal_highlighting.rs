//! Highlighting of a query inside a BSL string literal.
//!
//! The query text lives in the literal in its BSL envelope: a quote inside the query is
//! doubled, and every continuation line starts with `|`. The ranges the IDE paints must
//! land on the query's own lexemes in the BSL file — whole identifiers, whole keywords —
//! whatever the envelope added in between. Inputs are written for this file.

use std::fmt::Write;

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

fn highlight_inside_literal(source: &str) -> String {
    let file_id = FileId(0);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    db.set_file_text(file_id, source);
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));

    let literal_start = source.find("\"ВЫБРАТЬ").expect("the module holds a query literal");
    let literal_end = source.rfind("\";").expect("the literal is closed");

    let mut out = String::new();
    for hl in ide::highlight(&db, file_id).highlights {
        let (start, end) = (usize::from(hl.range.start()), usize::from(hl.range.end()));
        if start > literal_start && end <= literal_end {
            let before = source[..start].chars().next_back();
            let after = source[end..].chars().next();
            let cut = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
            assert!(
                !(cut(before) || cut(after)) || &source[start..end] == "=",
                "the range {start}..{end} cuts a lexeme: {:?}",
                &source[start..end]
            );
            writeln!(out, "{:?} {:?}", &source[start..end], hl.tag).unwrap();
        }
    }
    out
}

/// Every painted range is one whole lexeme of the query, and the listed lexemes are
/// painted with the listed tags.
fn check(source: &str, expected: &[(&str, &str)]) {
    let painted = highlight_inside_literal(source);
    for line in painted.lines() {
        let text = line.split('"').nth(1).unwrap_or_default();
        assert!(
            !text.is_empty() && text.chars().all(|c| c.is_alphanumeric() || "=<>&".contains(c)),
            "a painted range is not a whole lexeme: {line}"
        );
    }
    for (text, tag) in expected {
        let line = format!("{text:?} {tag}");
        assert!(painted.lines().any(|l| l == line), "{line} is missing:\n{painted}");
    }
}

#[test]
fn single_line_query_with_doubled_quotes() {
    check(
        r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ ""Готово"" КАК Статус, Т.Наименование КАК Имя ИЗ Справочник.Номенклатура КАК Т";
КонецПроцедуры
"#,
        &[
            ("ВЫБРАТЬ", "Keyword"),
            ("Статус", "EnumMember"),
            ("Наименование", "Property"),
            ("Имя", "EnumMember"),
            ("Номенклатура", "Type"),
        ],
    );
}

#[test]
fn multi_line_query_with_pipe_prefixes() {
    check(
        "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ\n    |    Т.Наименование КАК Имя,\n    |    Т.Код КАК Код\n    |ИЗ\n    |    Справочник.Номенклатура КАК Т\n    |ГДЕ\n    |    Т.Код = &Код\";\nКонецПроцедуры\n",
        &[
            ("ВЫБРАТЬ", "Keyword"),
            ("Наименование", "Property"),
            ("Код", "Property"),
            ("ИЗ", "Keyword"),
            ("Номенклатура", "Type"),
            ("ГДЕ", "Keyword"),
        ],
    );
}

#[test]
fn an_identifier_after_a_doubled_quote_is_painted_whole() {
    let source = "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Номенклатура КАК Т ГДЕ Т.Наименование ПОДОБНО \"\"А%\"\" И Т.ЭтоГруппа = ЛОЖЬ\";\nКонецПроцедуры\n";
    let painted = highlight_inside_literal(source);
    for word in ["\"ЭтоГруппа\"", "\"Наименование\"", "\"ПОДОБНО\""] {
        assert!(
            painted.lines().any(|line| line.starts_with(word)),
            "{word} must be painted as a whole lexeme:\n{painted}"
        );
    }
}

// --- Second package: functions, CASE branches and join sources. The query forms follow
// ITS pubqlang «Как получить текстовое представление ссылочного поля» (ПРЕДСТАВЛЕНИЕ),
// «Примеры использования выражений в списке полей выборки запроса» (ВЫБОР), «Как
// получить данные из разных таблиц, связанных несколькими соединениями» and «Как
// получить данные из табличной части некоторого документа»; the BSL envelope (doubled
// quotes, `|` continuation) follows the BSL string syntax. ---

/// Byte offsets of every occurrence of `word` in `source` that is a whole word.
fn occurrences(source: &str, word: &str) -> Vec<usize> {
    source
        .match_indices(word)
        .map(|(i, _)| i)
        .filter(|&i| {
            let before = source[..i].chars().next_back();
            let after = source[i + word.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
        .collect()
}

/// The painted ranges with their tags, as byte offsets into the BSL source.
fn painted(source: &str) -> Vec<(usize, usize, String)> {
    let file_id = FileId(0);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    db.set_file_text(file_id, source);
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    ide::highlight(&db, file_id)
        .highlights
        .into_iter()
        .map(|hl| {
            (usize::from(hl.range.start()), usize::from(hl.range.end()), format!("{:?}", hl.tag))
        })
        .collect()
}

/// Every whole-word occurrence of `word` inside the query literal is painted with `tag`
/// over exactly its own bytes.
fn assert_every_occurrence(source: &str, word: &str, tag: &str) {
    let literal = source.find("\"ВЫБРАТЬ").unwrap()..source.rfind("\";").unwrap();
    let wanted: Vec<usize> =
        occurrences(source, word).into_iter().filter(|i| literal.contains(i)).collect();
    assert!(!wanted.is_empty(), "{word} must occur in the query");
    let ranges = painted(source);
    for start in wanted {
        assert!(
            ranges.iter().any(|(s, e, t)| *s == start && *e == start + word.len() && t == tag),
            "{word} at {start} is not painted as {tag}"
        );
    }
}

#[test]
fn a_function_and_the_next_qualified_field_with_doubled_quotes() {
    let source = "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ ПРЕДСТАВЛЕНИЕ(Т.Ссылка) КАК Имя, Т.Артикул КАК Код ИЗ Справочник.Номенклатура КАК Т ГДЕ Т.Имя = \"\"А\"\"\";\nКонецПроцедуры\n";
    check(source, &[("ВЫБРАТЬ", "Keyword"), ("ПРЕДСТАВЛЕНИЕ", "Function"), ("Ссылка", "Property")]);
    assert_every_occurrence(source, "ПРЕДСТАВЛЕНИЕ", "Function");
    assert_every_occurrence(source, "Артикул", "Property");
}

#[test]
fn a_function_and_the_next_qualified_field_across_continuation_lines() {
    let source = "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ\n    |    ПРЕДСТАВЛЕНИЕ(Т.Ссылка) КАК Имя,\n    |    Т.Код КАК Код\n    |ИЗ\n    |    Справочник.Номенклатура КАК Т\";\nКонецПроцедуры\n";
    check(source, &[("ПРЕДСТАВЛЕНИЕ", "Function"), ("Код", "Property"), ("Номенклатура", "Type")]);
    assert_every_occurrence(source, "ПРЕДСТАВЛЕНИЕ", "Function");
    assert_every_occurrence(source, "Ссылка", "Property");
}

#[test]
fn crlf_continuations_keep_exact_unicode_and_operator_ranges() {
    let source = "Процедура Тест()\r\n    Запрос = Новый Запрос;\r\n    Запрос.Текст = \"ВЫБРАТЬ\r\n    |    Т.Наименование КАК Имя\r\n    |ИЗ Справочник.Товары КАК Т\r\n    |ГДЕ Т.Код = 1 И Т.Наименование = \"\"Готово\"\" И Т.Код = 1\";\r\nКонецПроцедуры\r\n";
    assert_every_occurrence(source, "Наименование", "Property");
    assert_every_occurrence(source, "Код", "Property");
    assert_every_occurrence(source, "=", "Operator");
    assert_every_occurrence(source, "ИЗ", "Keyword");
}

#[test]
fn a_function_in_every_case_branch_is_painted_at_each_occurrence() {
    let source = "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ ВЫБОР КОГДА Т.Вид = 1 ТОГДА ПРЕДСТАВЛЕНИЕ(Т.Ссылка) КОГДА Т.Вид = 2 ТОГДА ПРЕДСТАВЛЕНИЕ(Т.Родитель) ИНАЧЕ ПРЕДСТАВЛЕНИЕ(Т.Владелец) КОНЕЦ КАК Имя ИЗ Справочник.Номенклатура КАК Т\";\nКонецПроцедуры\n";
    assert_eq!(occurrences(source, "ПРЕДСТАВЛЕНИЕ").len(), 3);
    assert_every_occurrence(source, "ПРЕДСТАВЛЕНИЕ", "Function");
    assert_every_occurrence(source, "КОГДА", "Keyword");
}

#[test]
fn tables_of_nested_joins_and_a_tabular_part_are_types() {
    let source = "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Номенклатура КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Склады КАК Ск ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Валюты КАК Вал ПО Вал.Ссылка = Ск.Валюта ПО Ск.Ссылка = Т.Склад ЛЕВОЕ СОЕДИНЕНИЕ Документ.Заказ.Состав КАК Сост ПО Сост.Товар = Т.Ссылка\";\nКонецПроцедуры\n";
    for table in ["Склады", "Валюты", "Заказ", "Состав"] {
        assert_every_occurrence(source, table, "Type");
    }
}
