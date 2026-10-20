//! Document highlight on a structural keyword lights the keywords of that one construct —
//! `Если`/`Тогда`/`ИначеЕсли`/`Иначе`/`КонецЕсли`, the loop header and `КонецЦикла`,
//! `Попытка`/`Исключение`/`КонецПопытки`, the method header and its end, `#Если` with its
//! branches — and a region directive lights its balanced partner. Answered from any keyword of
//! the construct, the closing one included, and for English spellings alike.
//!
//! Every input is cut so that a nested construct stands next to the one under the cursor: an
//! answer that walked into nested blocks, or paired regions by nearest instead of balanced
//! marker, would light the wrong words.

use ide::{Analysis, DocumentHighlightKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

const FILE: FileId = FileId(0);
const CURSOR: &str = "$0";

fn analysis(text: &str) -> Analysis {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(FILE, VfsPath::new("/Module.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(FILE, SourceRootId(0));
    db.set_file_text(FILE, text);
    Analysis::from_database(db)
}

/// The highlights at the `$0` marker of `fixture`, as `(line, text)` in source order.
fn highlights(fixture: &str) -> Vec<(usize, String)> {
    let offset = fixture.find(CURSOR).expect("fixture carries a cursor");
    let text = fixture.replacen(CURSOR, "", 1);
    let mut found = analysis(&text).document_highlights(FILE, offset as u32);
    found.sort_by_key(|highlight| highlight.range.start());
    found
        .into_iter()
        .map(|highlight| {
            let start: usize = highlight.range.start().into();
            let end: usize = highlight.range.end().into();
            (text[..start].matches('\n').count(), text[start..end].to_string())
        })
        .collect()
}

fn kinds(fixture: &str) -> Vec<DocumentHighlightKind> {
    let offset = fixture.find(CURSOR).expect("fixture carries a cursor");
    let text = fixture.replacen(CURSOR, "", 1);
    let mut found = analysis(&text).document_highlights(FILE, offset as u32);
    found.sort_by_key(|highlight| highlight.range.start());
    found.into_iter().map(|highlight| highlight.kind).collect()
}

fn lines(expected: &[(usize, &str)]) -> Vec<(usize, String)> {
    expected.iter().map(|(line, text)| (*line, text.to_string())).collect()
}

const IF_BLOCK: &str = "\
Процедура Проверить(А, Б)
    Если А Тогда
        Если Б Тогда
            В = 1;
        КонецЕсли;
    ИначеЕсли Б Тогда
        В = 2;
    Иначе
        В = 3;
    КонецЕсли;
КонецПроцедуры
";

fn with_cursor(text: &str, line: usize, needle: &str) -> String {
    let line_start: usize = text.split_inclusive('\n').take(line).map(str::len).sum();
    let at = line_start + text[line_start..].find(needle).expect("needle on the line");
    format!("{}{CURSOR}{}", &text[..at], &text[at..])
}

#[test]
fn if_keywords_light_every_branch_but_not_the_nested_if() {
    let outer = lines(&[
        (1, "Если"),
        (1, "Тогда"),
        (5, "ИначеЕсли"),
        (5, "Тогда"),
        (7, "Иначе"),
        (9, "КонецЕсли"),
    ]);
    for (line, needle) in
        [(1, "Если"), (1, "Тогда"), (5, "ИначеЕсли"), (5, "Тогда"), (7, "Иначе"), (9, "КонецЕсли")]
    {
        assert_eq!(
            highlights(&with_cursor(IF_BLOCK, line, needle)),
            outer,
            "cursor on `{needle}` at line {line}"
        );
    }

    let inner = lines(&[(2, "Если"), (2, "Тогда"), (4, "КонецЕсли")]);
    assert_eq!(highlights(&with_cursor(IF_BLOCK, 2, "Если")), inner);
    assert_eq!(highlights(&with_cursor(IF_BLOCK, 4, "КонецЕсли")), inner);
}

#[test]
fn keyword_highlights_are_text_kind() {
    let fixture = with_cursor(IF_BLOCK, 9, "КонецЕсли");
    let found = kinds(&fixture);
    assert_eq!(found.len(), 6, "{found:?}");
    assert!(found.iter().all(|kind| *kind == DocumentHighlightKind::Text), "{found:?}");
}

#[test]
fn loop_keywords_pair_with_their_end_do() {
    let text = "\
Процедура Обойти(Массив)
    Для Каждого Элемент Из Массив Цикл
        Для Сч = 1 По 3 Цикл
            Пока Сч < 2 Цикл
                Прервать;
            КонецЦикла;
        КонецЦикла;
    КонецЦикла;
КонецПроцедуры
";
    let for_each = lines(&[(1, "Для"), (1, "Каждого"), (1, "Цикл"), (7, "КонецЦикла")]);
    assert_eq!(highlights(&with_cursor(text, 1, "Для")), for_each);
    assert_eq!(highlights(&with_cursor(text, 7, "КонецЦикла")), for_each);

    let for_to = lines(&[(2, "Для"), (2, "Цикл"), (6, "КонецЦикла")]);
    assert_eq!(highlights(&with_cursor(text, 2, "Цикл")), for_to);
    assert_eq!(highlights(&with_cursor(text, 6, "КонецЦикла")), for_to);

    let while_loop = lines(&[(3, "Пока"), (3, "Цикл"), (5, "КонецЦикла")]);
    assert_eq!(highlights(&with_cursor(text, 3, "Пока")), while_loop);
    assert_eq!(highlights(&with_cursor(text, 5, "КонецЦикла")), while_loop);
}

#[test]
fn try_keywords_pair_and_skip_the_nested_try() {
    let text = "\
Процедура Выполнить()
    Попытка
        Попытка
            А = 1;
        Исключение
            А = 2;
        КонецПопытки;
    Исключение
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры
";
    let outer = lines(&[(1, "Попытка"), (7, "Исключение"), (9, "КонецПопытки")]);
    assert_eq!(highlights(&with_cursor(text, 1, "Попытка")), outer);
    assert_eq!(highlights(&with_cursor(text, 7, "Исключение")), outer);
    assert_eq!(highlights(&with_cursor(text, 9, "КонецПопытки")), outer);

    let inner = lines(&[(2, "Попытка"), (4, "Исключение"), (6, "КонецПопытки")]);
    assert_eq!(highlights(&with_cursor(text, 4, "Исключение")), inner);
}

#[test]
fn method_keywords_pair_under_an_annotation() {
    let text = "\
&НаСервере
Функция Вычислить(А) Экспорт
    Если А Тогда
        Возврат 1;
    КонецЕсли;
    Возврат 0;
КонецФункции

Процедура Пустая()
КонецПроцедуры
";
    let function = lines(&[(1, "Функция"), (6, "КонецФункции")]);
    assert_eq!(highlights(&with_cursor(text, 1, "Функция")), function);
    assert_eq!(highlights(&with_cursor(text, 6, "КонецФункции")), function);

    let procedure = lines(&[(8, "Процедура"), (9, "КонецПроцедуры")]);
    assert_eq!(highlights(&with_cursor(text, 9, "КонецПроцедуры")), procedure);
}

#[test]
fn nested_regions_pair_with_the_balanced_partner() {
    let text = "\
#Область Внешняя
#Область Первая
Процедура А()
КонецПроцедуры
#КонецОбласти
#Область Вторая
#КонецОбласти
#КонецОбласти
";
    let outer = lines(&[(0, "#Область"), (7, "#КонецОбласти")]);
    assert_eq!(highlights(&with_cursor(text, 0, "#Область")), outer);
    assert_eq!(highlights(&with_cursor(text, 7, "#КонецОбласти")), outer);

    let first = lines(&[(1, "#Область"), (4, "#КонецОбласти")]);
    assert_eq!(highlights(&with_cursor(text, 1, "#Область")), first);
    assert_eq!(highlights(&with_cursor(text, 4, "#КонецОбласти")), first);

    let second = lines(&[(5, "#Область"), (6, "#КонецОбласти")]);
    assert_eq!(highlights(&with_cursor(text, 5, "#Область")), second);
    assert_eq!(highlights(&with_cursor(text, 6, "#КонецОбласти")), second);
}

#[test]
fn preprocessor_if_lights_its_branches_not_the_code_inside() {
    let text = "\
#Если Сервер Тогда
Процедура НаСервере(А)
    Если А Тогда
    КонецЕсли;
КонецПроцедуры
#ИначеЕсли Клиент Тогда
#Иначе
#КонецЕсли
";
    let directive = lines(&[
        (0, "#Если"),
        (0, "Тогда"),
        (5, "#ИначеЕсли"),
        (5, "Тогда"),
        (6, "#Иначе"),
        (7, "#КонецЕсли"),
    ]);
    for (line, needle) in
        [(0, "#Если"), (0, "Тогда"), (5, "#ИначеЕсли"), (6, "#Иначе"), (7, "#КонецЕсли")]
    {
        assert_eq!(
            highlights(&with_cursor(text, line, needle)),
            directive,
            "cursor on `{needle}` at line {line}"
        );
    }

    let statement = lines(&[(2, "Если"), (2, "Тогда"), (3, "КонецЕсли")]);
    assert_eq!(highlights(&with_cursor(text, 2, "Тогда")), statement);
}

#[test]
fn english_keywords_pair_like_russian_ones() {
    let text = "\
#Region Public
#If Server Then
Procedure Run(A) Export
    If A Then
        While A Do
        EndDo;
    ElsIf Not A Then
    Else
    EndIf;
    Try
    Except
    EndTry;
EndProcedure
#EndIf
#EndRegion
";
    assert_eq!(
        highlights(&with_cursor(text, 8, "EndIf")),
        lines(&[(3, "If"), (3, "Then"), (6, "ElsIf"), (6, "Then"), (7, "Else"), (8, "EndIf")])
    );
    assert_eq!(
        highlights(&with_cursor(text, 4, "Do")),
        lines(&[(4, "While"), (4, "Do"), (5, "EndDo")])
    );
    assert_eq!(
        highlights(&with_cursor(text, 10, "Except")),
        lines(&[(9, "Try"), (10, "Except"), (11, "EndTry")])
    );
    assert_eq!(
        highlights(&with_cursor(text, 2, "Procedure")),
        lines(&[(2, "Procedure"), (12, "EndProcedure")])
    );
    assert_eq!(
        highlights(&with_cursor(text, 13, "#EndIf")),
        lines(&[(1, "#If"), (1, "Then"), (13, "#EndIf")])
    );
    assert_eq!(
        highlights(&with_cursor(text, 14, "#EndRegion")),
        lines(&[(0, "#Region"), (14, "#EndRegion")])
    );
}

/// Control: an identifier inside a block keeps the symbol highlight it had before.
#[test]
fn identifier_inside_a_block_keeps_its_symbol_highlight() {
    let text = "\
Процедура Проверить()
    Перем Счетчик;
    Если Истина Тогда
        Счетчик = 1;
    КонецЕсли;
КонецПроцедуры
";
    let fixture = with_cursor(text, 3, "Счетчик");
    assert_eq!(highlights(&fixture), lines(&[(1, "Счетчик"), (3, "Счетчик")]));
    assert_eq!(kinds(&fixture), vec![DocumentHighlightKind::Text, DocumentHighlightKind::Write]);
}

/// Control: a construct the parser left open, and a region marker without a partner, answer
/// nothing for that pair — half a pair would point at an end that is not there.
#[test]
fn unfinished_pairs_answer_nothing() {
    let unclosed_if = "\
Процедура Проверить(А)
    Если А Тогда
        Б = 1;
КонецПроцедуры
";
    assert_eq!(highlights(&with_cursor(unclosed_if, 1, "Если")), Vec::new());
    assert_eq!(highlights(&with_cursor(unclosed_if, 1, "Тогда")), Vec::new());

    let unclosed_region = "\
#Область Открытая
Процедура А()
КонецПроцедуры
";
    assert_eq!(highlights(&with_cursor(unclosed_region, 0, "#Область")), Vec::new());

    let stray_end = "\
Процедура А()
КонецПроцедуры
#КонецОбласти
";
    assert_eq!(highlights(&with_cursor(stray_end, 2, "#КонецОбласти")), Vec::new());

    let unfinished_method = "Процедура А()\n    Если Истина Тогда\n    КонецЕсли;\n";
    assert_eq!(highlights(&with_cursor(unfinished_method, 0, "Процедура")), Vec::new());
}

/// A method named with its own declaration keyword — not the language, but the parser takes the
/// word as the name — keeps the name on the symbol path: the name is not part of the pair.
#[test]
fn method_named_with_its_own_keyword_keeps_the_name_out_of_the_pair() {
    for (text, opener, closer) in [
        ("Процедура Процедура()\nКонецПроцедуры\n", "Процедура", "КонецПроцедуры"),
        ("Функция Функция()\nКонецФункции\n", "Функция", "КонецФункции"),
        ("Procedure Procedure()\nEndProcedure\n", "Procedure", "EndProcedure"),
    ] {
        let name_at = opener.len() + 1;
        let on_name = format!("{}{CURSOR}{}", &text[..name_at], &text[name_at..]);
        let found = highlights(&on_name);
        assert!(!found.iter().any(|(_, word)| word == closer), "cursor on the name: {found:?}");

        assert_eq!(
            highlights(&with_cursor(text, 1, closer)),
            lines(&[(0, opener), (1, closer)]),
            "cursor on `{closer}`"
        );
    }
}

/// A region marker the parser swallowed into an error node is not a region directive: it neither
/// pairs nor shifts the pairing of the directives around it.
#[test]
fn region_marker_inside_an_error_node_does_not_pair() {
    let text = "\
#Область Внешняя
Процедура А()
    Б =
#Область Внутренняя
    1;
КонецПроцедуры
#КонецОбласти
";
    let outer = lines(&[(0, "#Область"), (6, "#КонецОбласти")]);
    assert_eq!(highlights(&with_cursor(text, 0, "#Область")), outer);
    assert_eq!(highlights(&with_cursor(text, 6, "#КонецОбласти")), outer);
    assert_eq!(highlights(&with_cursor(text, 3, "#Область")), Vec::new());
}
