use ide_db::RootDatabase;
use line_index::LineIndex;
use syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};
use vfs::FileId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldingRange {
    pub start_line: u32,
    pub end_line: u32,
    pub kind: Option<FoldingRangeKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldingRangeKind {
    Region,
    Comment,
}

pub fn folding_ranges<DB: RootDatabase>(db: &DB, file_id: FileId) -> Vec<FoldingRange> {
    let _span = tracing::info_span!("folding_ranges", ?file_id).entered();

    let text = db.file_text(file_id);
    let line_index = LineIndex::new(&text);
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let mut ranges = Vec::new();
    collect_region_ranges(db, file_id, &line_index, &mut ranges);
    collect_syntax_ranges(&root, &line_index, &mut ranges);
    collect_comment_ranges(&root, &line_index, &mut ranges);

    ranges.sort_by_key(|range| (range.start_line, range.end_line));
    ranges.dedup_by_key(|range| (range.start_line, range.end_line, range.kind));
    ranges
}

fn collect_region_ranges<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    line_index: &LineIndex,
    ranges: &mut Vec<FoldingRange>,
) {
    let region_tree = db.region_tree(file_id);
    for (_, region) in region_tree.regions() {
        push_multiline_range(ranges, line_index, region.range, Some(FoldingRangeKind::Region));
    }
}

fn collect_syntax_ranges(
    root: &SyntaxNode,
    line_index: &LineIndex,
    ranges: &mut Vec<FoldingRange>,
) {
    for node in root.descendants() {
        if node.kind() == SyntaxKind::LITERAL {
            if let Some(range) = string_literal_range(&node) {
                push_multiline_range(ranges, line_index, range, None);
            }
        } else if is_foldable_syntax_node(node.kind()) {
            push_multiline_range(ranges, line_index, node.text_range(), None);
        }
    }
}

/// Текст запроса — многострочный литерал: складка идёт от первой кавычки
/// узла до последней закрывающей. Соседние строки без оператора парсер
/// склеивает в один узел, и сворачивается он только целиком закрытый:
/// границу оборванной части задаёт восстановление парсера, которое
/// закрывает её чужой строкой или отдаёт литералу следующий за ним код.
fn string_literal_range(literal: &SyntaxNode) -> Option<TextRange> {
    let mut open = false;
    let mut end = None;
    for token in literal.children_with_tokens().filter_map(|element| element.into_token()) {
        match token.kind() {
            SyntaxKind::STRING_START if !open => open = true,
            SyntaxKind::STRING_PART if open => {}
            SyntaxKind::STRING_TAIL if open => {
                open = false;
                end = Some(token.text_range().end());
            }
            SyntaxKind::STRING if !open => end = Some(token.text_range().end()),
            SyntaxKind::STRING_START
            | SyntaxKind::STRING_PART
            | SyntaxKind::STRING_TAIL
            | SyntaxKind::STRING => return None,
            _ => {}
        }
    }
    if open {
        return None;
    }
    Some(TextRange::new(literal.text_range().start(), end?))
}

/// Серия комментариев сворачивается по частям, владеющим своей строкой:
/// комментарий, стоящий за кодом, свернуть нечем — его строка всё равно
/// останется видимой, а приклеенный к соседней складке он удлинил бы её на
/// строку с кодом.
fn collect_comment_ranges(
    root: &SyntaxNode,
    line_index: &LineIndex,
    ranges: &mut Vec<FoldingRange>,
) {
    for run in syntax::comment_runs(root) {
        let mut owned: Option<TextRange> = None;
        for line in run.lines() {
            if line.owns_line {
                let range = line.range;
                owned = Some(owned.map_or(range, |open| open.cover(range)));
            } else if let Some(open) = owned.take() {
                push_multiline_range(ranges, line_index, open, Some(FoldingRangeKind::Comment));
            }
        }
        if let Some(open) = owned {
            push_multiline_range(ranges, line_index, open, Some(FoldingRangeKind::Comment));
        }
    }
}

fn is_foldable_syntax_node(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::PROCEDURE_DEF
            | SyntaxKind::FUNCTION_DEF
            | SyntaxKind::IF_STMT
            | SyntaxKind::WHILE_STMT
            | SyntaxKind::FOR_STMT
            | SyntaxKind::FOR_EACH_STMT
            | SyntaxKind::TRY_STMT
            | SyntaxKind::PRE_IF_DIR
            | SyntaxKind::PRE_DELETE_DIR
            | SyntaxKind::PRE_INSERT_DIR
    )
}

fn push_multiline_range(
    ranges: &mut Vec<FoldingRange>,
    line_index: &LineIndex,
    range: TextRange,
    kind: Option<FoldingRangeKind>,
) {
    if let Some((start_line, end_line)) = folding_lines(line_index, range) {
        ranges.push(FoldingRange { start_line, end_line, kind });
    }
}

/// Единственная проекция диапазона в пару строк: наружу уезжают уже готовые
/// номера, чтобы второй расчёт по другому тексту не мог с ними разойтись.
fn folding_lines(line_index: &LineIndex, range: TextRange) -> Option<(u32, u32)> {
    if range.is_empty() {
        return None;
    }

    let start_line = line_index.try_line_col(range.start())?.line;
    let end_offset = range.end() - TextSize::from(1);
    let end_line = line_index.try_line_col(end_offset)?.line;
    (end_line > start_line).then_some((start_line, end_line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::vfs::{file_set::FileSet, VfsPath};
    use ide_db::RootDatabaseImpl;

    fn setup_db(code: &str) -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, code);
        (db, file_id)
    }

    fn ranges_by_lines(code: &str) -> Vec<(u32, u32, Option<FoldingRangeKind>)> {
        let (db, file_id) = setup_db(code);
        folding_ranges(&db, file_id)
            .into_iter()
            .map(|range| (range.start_line, range.end_line, range.kind))
            .collect()
    }

    #[test]
    fn folds_procedure_and_function() {
        let code = "Процедура Тест()\n    Сообщить(1);\nКонецПроцедуры\n\nФункция Ф()\n    Возврат 1;\nКонецФункции";

        let ranges = ranges_by_lines(code);

        assert_eq!(ranges, vec![(0, 2, None), (4, 6, None)]);
    }

    #[test]
    fn folds_regions_with_kind() {
        let code = "#Область Public\nПроцедура Тест()\nКонецПроцедуры\n#КонецОбласти";

        let ranges = ranges_by_lines(code);

        assert_eq!(ranges, vec![(0, 3, Some(FoldingRangeKind::Region)), (1, 2, None)]);
    }

    #[test]
    fn folds_control_flow_blocks() {
        let code = "Процедура Тест()\nЕсли Истина Тогда\n    Сообщить(1);\nКонецЕсли;\nДля Сч = 1 По 2 Цикл\n    Сообщить(Сч);\nКонецЦикла;\nПопытка\n    Сообщить(1);\nИсключение\n    Сообщить(2);\nКонецПопытки;\nКонецПроцедуры";

        let ranges = ranges_by_lines(code);

        assert!(ranges.contains(&(0, 12, None)));
        assert!(ranges.contains(&(1, 3, None)));
        assert!(ranges.contains(&(4, 6, None)));
        assert!(ranges.contains(&(7, 11, None)));
    }

    #[test]
    fn folds_preprocessor_blocks() {
        let code = "#Если Сервер Тогда\nПроцедура Тест()\nКонецПроцедуры\n#КонецЕсли\n#Удаление\nСообщить(1);\n#КонецУдаления\n#Вставка\nСообщить(2);\n#КонецВставки";

        let ranges = ranges_by_lines(code);

        assert!(ranges.contains(&(0, 3, None)));
        assert!(ranges.contains(&(1, 2, None)));
        assert!(ranges.contains(&(4, 6, None)));
        assert!(ranges.contains(&(7, 9, None)));
    }

    #[test]
    fn folds_only_line_owning_comments() {
        let code = "А = 1; // хвост\n// своя\n// своя2\n";

        let ranges = ranges_by_lines(code);

        assert_eq!(ranges, vec![(1, 2, Some(FoldingRangeKind::Comment))]);
    }

    #[test]
    fn single_comment_line_is_not_folded() {
        assert!(ranges_by_lines("// один\nА = 1;\n").is_empty());
        // Контрольный вход: без него пустой ответ выше означал бы и «правило
        // работает», и «комментарные складки не собираются вовсе».
        assert_eq!(
            ranges_by_lines("// один\n// два\nА = 1;\n"),
            vec![(0, 1, Some(FoldingRangeKind::Comment))]
        );
    }

    /// Шапка метода по стандарту 1С: пустые `//` внутри шапки её не разрывают,
    /// пустая строка — разрывает, а диапазон кончается на последней строке
    /// комментария и не захватывает объявление под ней, иначе он пересёкся бы
    /// с диапазоном функции в неразрешимую сторону.
    #[test]
    fn doc_header_folds_as_one_block_and_stops_above_the_declaration() {
        let code = "#Область ПрограммныйИнтерфейс\n\
                    \n\
                    // Возвращает описание физического лица.\n\
                    //\n\
                    // Параметры:\n\
                    //  ФизическоеЛицо - СправочникСсылка.ФизическиеЛица - лицо.\n\
                    //  Дата           - Дата - на какой момент брать историю.\n\
                    //\n\
                    // Возвращаемое значение:\n\
                    //  Структура - состав полей описан в документации.\n\
                    //\n\
                    Функция ОписаниеФизическогоЛица(ФизическоеЛицо, Дата) Экспорт\n\
                    \n\
                        // Пустая ссылка сюда доходить не должна: вызывающий код\n\
                        // проверяет её сам, но контракт дешевле продублировать,\n\
                        // чем потом искать пустую структуру в выгрузке.\n\
                        Если НЕ ЗначениеЗаполнено(ФизическоеЛицо) Тогда\n\
                        ВызватьИсключение \"Не задано физическое лицо\";\n\
                    КонецЕсли;\n\
                    \n\
                        Возврат Новый Структура;\n\
                    \n\
                    КонецФункции\n\
                    \n\
                    #КонецОбласти";

        let mut ranges = ranges_by_lines(code);
        ranges.sort_by_key(|&(start, end, _)| (start, end));

        assert_eq!(
            ranges,
            vec![
                (0, 24, Some(FoldingRangeKind::Region)),
                (2, 10, Some(FoldingRangeKind::Comment)),
                (11, 22, None),
                (13, 15, Some(FoldingRangeKind::Comment)),
                (16, 18, None),
            ]
        );
    }

    #[test]
    fn double_slash_inside_string_literal_is_not_folded() {
        assert!(ranges_by_lines("А = \"http://a\";\nБ = \"http://b\";\n").is_empty());
        // Тот же текст комментариями даёт складку, поэтому пустой ответ выше
        // нельзя спутать с неработающей сборкой складок. Наивный текстовый скан
        // на `//` свернул бы и первый вход.
        assert_eq!(
            ranges_by_lines("// http://a\n// http://b\n"),
            vec![(0, 1, Some(FoldingRangeKind::Comment))]
        );
    }

    /// Оборванный многострочный литерал, дотянувшийся до конца тела, — текст
    /// строки, а не серия комментариев: ложная складка на нём не собирается,
    /// а тот же хвост без оборванной строки сворачивается.
    #[test]
    fn unclosed_multiline_literal_reaching_the_body_end_is_not_folded() {
        let with_literal =
            "Процедура П()\n    Т = \"ВЫБРАТЬ *\n    // Сообщить(1);\n    // Возврат;\nКонецПроцедуры";
        assert_eq!(ranges_by_lines(with_literal), vec![(0, 4, None)]);

        let without_literal = "Процедура П()\n    // Сообщить(1);\n    // Возврат;\nКонецПроцедуры";
        let mut ranges = ranges_by_lines(without_literal);
        ranges.sort_by_key(|&(start, end, _)| (start, end));
        assert_eq!(ranges, vec![(0, 3, None), (1, 2, Some(FoldingRangeKind::Comment))]);
    }

    /// Текст запроса — многострочный литерал: складка идёт от строки с
    /// открывающей кавычкой до строки с закрывающей, как у остальных
    /// конструкций — первая строка остаётся видимой.
    #[test]
    fn folds_multiline_query_literal() {
        let code = "Процедура Тест()\n\
                    \tЗапрос = Новый Запрос;\n\
                    \tЗапрос.Текст =\n\
                    \t\"ВЫБРАТЬ\n\
                    \t|\tТовары.Ссылка КАК Ссылка\n\
                    \t|ИЗ\n\
                    \t|\tСправочник.Товары КАК Товары\";\n\
                    \tВыборка = Запрос.Выполнить().Выбрать();\n\
                    КонецПроцедуры";

        let ranges = ranges_by_lines(code);

        assert_eq!(ranges, vec![(0, 8, None), (3, 6, None)]);
    }

    #[test]
    fn single_line_literal_is_not_folded() {
        let code = "Процедура Тест()\n\tА = \"ВЫБРАТЬ 1\";\nКонецПроцедуры";

        assert_eq!(ranges_by_lines(code), vec![(0, 2, None)]);
        // Контрольный вход: тот же литерал, разбитый на две строки, складку
        // даёт — пустой ответ выше не означает, что литералы не сворачиваются
        // вовсе.
        let code = "Процедура Тест()\n\tА = \"ВЫБРАТЬ\n\t| 1\";\nКонецПроцедуры";
        assert_eq!(ranges_by_lines(code), vec![(0, 3, None), (1, 2, None)]);
    }

    /// Литерал во вложенной конструкции сворачивается сам по себе, а
    /// складки самой конструкции и метода остаются прежними.
    #[test]
    fn multiline_literal_inside_nested_block_is_folded() {
        let code = "Процедура Тест()\n\
                    \tЕсли Истина Тогда\n\
                    \t\tДля Каждого Стр Из Список Цикл\n\
                    \t\t\tЗапрос.Текст = \"ВЫБРАТЬ\n\
                    \t\t\t|\t1 КАК Поле\";\n\
                    \t\tКонецЦикла;\n\
                    \tКонецЕсли;\n\
                    КонецПроцедуры";

        let ranges = ranges_by_lines(code);

        assert_eq!(ranges, vec![(0, 7, None), (1, 6, None), (2, 5, None), (3, 4, None)]);
    }

    /// Комментарий между строками литерала его не разрывает: литерал — одна
    /// складка, и комментарной складки внутри нет.
    #[test]
    fn comment_between_literal_lines_stays_inside_the_literal_fold() {
        let code = "Процедура Тест()\n\
                    \tТекст = \"ВЫБРАТЬ\n\
                    \t|\t1 КАК Поле\n\
                    \t// пояснение\n\
                    \t// ещё пояснение\n\
                    \t|ГДЕ ИСТИНА\";\n\
                    КонецПроцедуры";

        let ranges = ranges_by_lines(code);

        assert_eq!(ranges, vec![(0, 6, None), (1, 5, None)]);
    }

    /// Оборванный литерал складки не даёт ни со строками `|`, ни с кодом
    /// после них, а складка метода вокруг него остаётся. Тот же литерал,
    /// закрытый кавычкой, сворачивается — пустой ответ не означает, что
    /// литералы не сворачиваются вовсе.
    #[test]
    fn unclosed_literal_with_continuation_lines_is_not_folded() {
        let continued = "Процедура Тест()\n\
                         \tТекст = \"ВЫБРАТЬ\n\
                         \t|\t1 КАК Поле\n\
                         \t|ГДЕ ИСТИНА\n\
                         КонецПроцедуры";
        assert_eq!(ranges_by_lines(continued), vec![(0, 4, None)]);

        let with_code_after = "Процедура Тест()\n\
                               \tТекст = \"ВЫБРАТЬ\n\
                               \t|\t1 КАК Поле\n\
                               \t|ГДЕ ИСТИНА\n\
                               \tСообщить(Текст);\n\
                               КонецПроцедуры";
        assert_eq!(ranges_by_lines(with_code_after), vec![(0, 5, None)]);

        let without_continuation =
            "Процедура Тест()\n\tТекст = \"ВЫБРАТЬ\n\tСообщить(Текст);\nКонецПроцедуры";
        assert_eq!(ranges_by_lines(without_continuation), vec![(0, 3, None)]);

        let closed = "Процедура Тест()\n\
                      \tТекст = \"ВЫБРАТЬ\n\
                      \t|\t1 КАК Поле\n\
                      \t|ГДЕ ИСТИНА\";\n\
                      \tСообщить(Текст);\n\
                      КонецПроцедуры";
        assert_eq!(ranges_by_lines(closed), vec![(0, 5, None), (1, 3, None)]);
    }

    /// Оборванная строка, за которой стоит закрытая, парсер склеивает с ней
    /// в один литерал; закрывающая кавычка чужой строки не делает литерал
    /// закрытым, и складки нет — ни через комментарий, ни вплотную.
    #[test]
    fn unclosed_literal_followed_by_a_closed_string_is_not_folded() {
        let through_comment = "Процедура Тест()\n\
                               \tТекст = \"ВЫБРАТЬ\n\
                               \t// пояснение\n\
                               \t\"ИЗ\n\
                               \t|Т\";\n\
                               КонецПроцедуры";
        assert_eq!(ranges_by_lines(through_comment), vec![(0, 5, None)]);

        let adjacent = "Процедура Тест()\n\
                        \tТекст = \"ВЫБРАТЬ\n\
                        \t\"ИЗ\n\
                        \t|Т\";\n\
                        КонецПроцедуры";
        assert_eq!(ranges_by_lines(adjacent), vec![(0, 4, None)]);

        let closed_by_a_single_line_string = "Процедура Тест()\n\
                                              \tТекст = \"ВЫБРАТЬ\n\
                                              \t// пояснение\n\
                                              \t\"ИЗ Т\";\n\
                                              КонецПроцедуры";
        assert_eq!(ranges_by_lines(closed_by_a_single_line_string), vec![(0, 4, None)]);
    }

    /// Соседние строки без оператора — одно значение, склеенное из частей:
    /// складка идёт от первой кавычки до последней закрывающей, какая бы из
    /// частей ни была многострочной.
    #[test]
    fn adjacent_string_literals_fold_as_one_literal() {
        let two_multiline = "Процедура Тест()\n\
                             \tТекст = \"ВЫБРАТЬ\n\
                             \t|1\" \"ИЗ\n\
                             \t|Т\";\n\
                             КонецПроцедуры";
        assert_eq!(ranges_by_lines(two_multiline), vec![(0, 4, None), (1, 3, None)]);

        let multiline_then_single_line = "Процедура Тест()\n\
                                          \tТекст = \"ВЫБРАТЬ\n\
                                          \t|1\"\n\
                                          \t\" ГДЕ ИСТИНА\";\n\
                                          КонецПроцедуры";
        assert_eq!(ranges_by_lines(multiline_then_single_line), vec![(0, 4, None), (1, 3, None)]);

        let single_line_then_multiline = "Процедура Тест()\n\
                                          \tТекст = \"ВЫБРАТЬ 1\"\n\
                                          \t\"ГДЕ\n\
                                          \t|ИСТИНА\";\n\
                                          КонецПроцедуры";
        assert_eq!(ranges_by_lines(single_line_then_multiline), vec![(0, 4, None), (1, 3, None)]);
    }

    #[test]
    fn ignores_single_line_ranges() {
        let code = "Процедура Тест() КонецПроцедуры";

        let ranges = ranges_by_lines(code);

        assert!(ranges.is_empty());
    }
}
