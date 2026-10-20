//! A nested query in a select list, through both routes a query reaches the rules by: a string
//! literal in a module and a bare query text.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig};
use text_size::{TextRange, TextSize};
use vfs::{FileId, FileSet, VfsPath};

const MESSAGE: &str = "Вложенный запрос в списке полей выборки недопустим: он возможен только \
                       как источник в ИЗ и справа от В (...)";

fn module_query_errors(source: &str) -> Vec<Diagnostic> {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test/Module.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::QueryParseError)
        .collect()
}

fn bare_query_errors(query: &str) -> Vec<Diagnostic> {
    ide_diagnostics::validate_query_text(&DiagnosticsConfig::all_enabled(), None, query)
        .into_iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::QueryParseError)
        .collect()
}

/// The range of `needle`, which must occur in `text` exactly once.
fn range_of(text: &str, needle: &str) -> TextRange {
    let start = text.find(needle).expect("needle in text");
    assert_eq!(text.rfind(needle), Some(start), "needle {needle:?} is ambiguous");
    TextRange::at(TextSize::try_from(start).unwrap(), TextSize::of(needle))
}

fn findings(diagnostics: &[Diagnostic]) -> Vec<(TextRange, &str)> {
    diagnostics.iter().map(|d| (d.range, d.message.as_str())).collect()
}

#[test]
fn a_module_literal_reports_the_subquery_where_it_stands_in_the_module() {
    let source = "Функция Тест() Экспорт\n\
                  \tЗапрос = Новый Запрос(\"ВЫБРАТЬ (ВЫБРАТЬ 1) КАК А\");\n\
                  \tВозврат Запрос.Выполнить();\n\
                  КонецФункции\n";
    let errors = module_query_errors(source);
    assert_eq!(findings(&errors), vec![(range_of(source, "(ВЫБРАТЬ 1)"), MESSAGE)]);
}

/// A query spread over continuation lines is mapped back through the `|` prefixes: the range
/// starts at the opening parenthesis and ends at the closing one, several lines below.
#[test]
fn a_multiline_module_literal_maps_the_subquery_across_its_lines() {
    let source = "Функция Тест() Экспорт\n\
                  \tТекст = \"ВЫБРАТЬ\n\
                  \t|\tТ.Ссылка КАК Ссылка,\n\
                  \t|\t(ВЫБРАТЬ\n\
                  \t|\t\tКОЛИЧЕСТВО(*) КАК Количество\n\
                  \t|\tИЗ\n\
                  \t|\t\tСправочник.Т КАК Т2) КАК Всего\n\
                  \t|ИЗ\n\
                  \t|\tСправочник.Т КАК Т\";\n\
                  \tВозврат Текст;\n\
                  КонецФункции\n";
    let start = range_of(source, "(ВЫБРАТЬ").start();
    let end = range_of(source, "Т2)").end();
    let errors = module_query_errors(source);
    assert_eq!(findings(&errors), vec![(TextRange::new(start, end), MESSAGE)]);
}

#[test]
fn a_bare_query_reports_the_subquery_in_query_coordinates() {
    let query = "ВЫБРАТЬ (ВЫБРАТЬ 1) КАК А";
    let errors = bare_query_errors(query);
    assert_eq!(findings(&errors), vec![(range_of(query, "(ВЫБРАТЬ 1)"), MESSAGE)]);
}

/// The silent controls of both routes: the same nested query as a source and on the right of
/// `В` / `НЕ В` raises no query parse error at all.
#[test]
fn allowed_positions_stay_silent_on_both_routes() {
    for query in [
        "ВЫБРАТЬ Вл.А КАК А ИЗ (ВЫБРАТЬ 1 КАК А) КАК Вл",
        "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т \
         ГДЕ Т.Ссылка В (ВЫБРАТЬ Т2.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т2)",
        "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т \
         ГДЕ НЕ Т.Ссылка В (ВЫБРАТЬ Т2.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т2)",
    ] {
        let source = format!(
            "Функция Тест() Экспорт\n\tЗапрос = Новый Запрос(\"{query}\");\n\
             \tВозврат Запрос.Выполнить();\nКонецФункции\n"
        );
        assert_eq!(findings(&module_query_errors(&source)), Vec::new(), "{query}");
        assert_eq!(findings(&bare_query_errors(query)), Vec::new(), "{query}");
    }
}
