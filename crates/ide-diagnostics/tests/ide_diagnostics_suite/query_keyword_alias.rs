//! A query keyword used as an alias, through both routes a query reaches the rules by: a string
//! literal in a module and a bare query text.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig};
use text_size::{TextRange, TextSize};
use vfs::{FileId, FileSet, VfsPath};

const MESSAGE: &str =
    "Ожидается имя: ключевое слово языка запросов нельзя использовать как псевдоним";

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

fn module_with(query: &str) -> String {
    format!(
        "Функция Тест() Экспорт\n\tЗапрос = Новый Запрос(\"{query}\");\n\
         \tВозврат Запрос.Выполнить();\nКонецФункции\n"
    )
}

/// The range of the alias `word` declared by `КАК word` in `text`.
fn alias_range(text: &str, word: &str) -> TextRange {
    let declaration = format!("КАК {word}");
    let start = text.find(&declaration).expect("alias in text") + "КАК ".len();
    TextRange::at(TextSize::try_from(start).unwrap(), TextSize::of(word))
}

fn findings(diagnostics: &[Diagnostic]) -> Vec<(TextRange, &str)> {
    diagnostics.iter().map(|d| (d.range, d.message.as_str())).collect()
}

#[test]
fn an_unreferenced_keyword_alias_is_reported_on_both_routes() {
    let query = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Справочник.Валюты КАК В ГДЕ ИСТИНА";
    let source = module_with(query);
    assert_eq!(findings(&module_query_errors(&source)), vec![(alias_range(&source, "В"), MESSAGE)]);
    assert_eq!(findings(&bare_query_errors(query)), vec![(alias_range(query, "В"), MESSAGE)]);
}

/// The reference used to be the only finding, at the wrong place and in the wrong words.
#[test]
fn a_referenced_keyword_alias_is_the_one_finding_on_both_routes() {
    let query = "ВЫБРАТЬ В.Ссылка КАК Ссылка ИЗ Справочник.Валюты КАК В";
    let source = module_with(query);
    assert_eq!(findings(&module_query_errors(&source)), vec![(alias_range(&source, "В"), MESSAGE)]);
    assert_eq!(findings(&bare_query_errors(query)), vec![(alias_range(query, "В"), MESSAGE)]);
}

#[test]
fn a_keyword_field_alias_is_reported_on_both_routes() {
    let query = "ВЫБРАТЬ 1 КАК Когда, 2 КАК Б";
    let source = module_with(query);
    assert_eq!(
        findings(&module_query_errors(&source)),
        vec![(alias_range(&source, "Когда"), MESSAGE)]
    );
    assert_eq!(findings(&bare_query_errors(query)), vec![(alias_range(query, "Когда"), MESSAGE)]);
}

/// Keywords the platform accepts as aliases stay silent, the clause ones included.
#[test]
fn accepted_keyword_aliases_stay_silent_on_both_routes() {
    for query in [
        "ВЫБРАТЬ Т.Ссылка КАК Упорядочить ИЗ Справочник.Валюты КАК Т УПОРЯДОЧИТЬ ПО Упорядочить",
        "ВЫБРАТЬ Т.Ссылка КАК Выбрать ИЗ Справочник.Валюты КАК Т",
        "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ (ВЫБРАТЬ 1 КАК Ссылка) КАК Объединить, \
         Справочник.Валюты КАК Т",
    ] {
        assert_eq!(findings(&module_query_errors(&module_with(query))), Vec::new(), "{query}");
        assert_eq!(findings(&bare_query_errors(query)), Vec::new(), "{query}");
    }
}
