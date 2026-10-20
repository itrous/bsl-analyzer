//! A method's fold starts on the line of `Процедура`/`Функция` (or `Асинх`), not on the
//! compilation directive or annotation above it.
//!
//! The editor keeps the first line of a fold visible. Started on `&НаКлиенте`, a folded
//! module shows a column of identical directive lines and hides every method name; started
//! on the declaration, the folded line names the method and the directive stays above it.
//! A method without annotations, the other constructs and the comment folds keep their lines.

use ide::{Analysis, FoldingRangeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

fn ranges_by_lines(code: &str) -> Vec<(u32, u32, Option<FoldingRangeKind>)> {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, code);
    Analysis::from_database(db)
        .folding_ranges(file_id)
        .into_iter()
        .map(|range| (range.start_line, range.end_line, range.kind))
        .collect()
}

#[test]
fn procedure_fold_starts_below_the_compilation_directive() {
    let code = "&НаКлиенте\n\
                Процедура ОбработатьНажатие(Команда)\n\
                \tСообщить(1);\n\
                КонецПроцедуры";

    assert_eq!(ranges_by_lines(code), vec![(1, 3, None)]);
}

#[test]
fn function_fold_starts_below_several_annotations() {
    let code = "&НаСервере\n\
                &После(\"Функция1\")\n\
                Функция Расш1_Функция1()\n\
                \tВозврат 1;\n\
                КонецФункции";

    assert_eq!(ranges_by_lines(code), vec![(2, 4, None)]);
}

#[test]
fn change_and_control_method_keeps_its_insert_fold() {
    let code = "&ИзменениеИКонтроль(\"Процедура1\")\n\
                Процедура Расш1_Процедура1()\n\
                \t#Вставка\n\
                \tСообщить(1);\n\
                \t#КонецВставки\n\
                КонецПроцедуры";

    assert_eq!(ranges_by_lines(code), vec![(1, 5, None), (2, 4, None)]);
}

#[test]
fn async_method_fold_starts_on_the_async_keyword() {
    let code = "&НаКлиенте\n\
                Асинх Процедура Процедура1()\n\
                \tСообщить(1);\n\
                КонецПроцедуры\n\
                \n\
                &НаКлиенте\n\
                Асинх Функция Функция1()\n\
                \tВозврат 1;\n\
                КонецФункции";

    assert_eq!(ranges_by_lines(code), vec![(1, 3, None), (6, 8, None)]);
}

#[test]
fn english_keywords_fold_below_the_directive() {
    let code = "&AtClient\n\
                Procedure Procedure1()\n\
                \tMessage(1);\n\
                EndProcedure\n\
                \n\
                &AtServer\n\
                Async Function Function1()\n\
                \tReturn 1;\n\
                EndFunction";

    assert_eq!(ranges_by_lines(code), vec![(1, 3, None), (6, 8, None)]);
}

#[test]
fn method_without_annotations_still_folds_from_its_first_line() {
    let code = "Процедура Процедура1()\n\
                \tСообщить(1);\n\
                КонецПроцедуры\n\
                \n\
                Асинх Функция Функция1()\n\
                \tВозврат 1;\n\
                КонецФункции";

    assert_eq!(ranges_by_lines(code), vec![(0, 2, None), (4, 6, None)]);
}

#[test]
fn region_and_nested_block_folds_are_unchanged() {
    let code = "#Область ОбработчикиКомандФормы\n\
                \n\
                &НаКлиенте\n\
                Процедура ОбработатьНажатие(Команда)\n\
                \tЕсли Истина Тогда\n\
                \t\tСообщить(1);\n\
                \tКонецЕсли;\n\
                КонецПроцедуры\n\
                \n\
                #КонецОбласти";

    assert_eq!(
        ranges_by_lines(code),
        vec![(0, 9, Some(FoldingRangeKind::Region)), (3, 7, None), (4, 6, None)]
    );
}

#[test]
fn multiline_literal_inside_annotated_method_keeps_its_fold() {
    let code = "&НаСервере\n\
                Процедура Процедура1()\n\
                \tТекст = \"ВЫБРАТЬ\n\
                \t|\t1\";\n\
                КонецПроцедуры";

    assert_eq!(ranges_by_lines(code), vec![(1, 4, None), (2, 3, None)]);
}

#[test]
fn doc_comment_above_the_directive_keeps_its_own_fold() {
    let code = "// Обрабатывает нажатие.\n\
                // Параметры: Команда - команда формы.\n\
                &НаКлиенте\n\
                Процедура ОбработатьНажатие(Команда)\n\
                \tСообщить(1);\n\
                КонецПроцедуры";

    assert_eq!(ranges_by_lines(code), vec![(0, 1, Some(FoldingRangeKind::Comment)), (3, 5, None)]);
}
