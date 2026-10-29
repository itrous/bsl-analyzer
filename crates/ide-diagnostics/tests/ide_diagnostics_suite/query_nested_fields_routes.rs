//! `QueryNestedFieldsByDot` needs the type of the dereferenced field, so it speaks only when
//! the query is checked against metadata — and then says the same thing whether the query sits
//! in a module or arrives as bare text.
//!
//! `ЦеныОбъектов.Объект` is a composite reference, `ЦеныОбъектов.Номенклатура` a single one.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{
    Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext, METADATA_DEPENDENT_CODES,
};
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/query_nested_fields");

const COMPOSITE: &str = "ВЫБРАТЬ Цены.Объект.Код КАК Код ИЗ РегистрСведений.ЦеныОбъектов КАК Цены";
const SINGLE: &str =
    "ВЫБРАТЬ Цены.Номенклатура.Код КАК Код ИЗ РегистрСведений.ЦеныОбъектов КАК Цены";
const MIXED: &str = "ВЫБРАТЬ Цены.Номенклатура.Код КАК К1, Цены.Объект.Код КАК К2 \
                     ИЗ РегистрСведений.ЦеныОбъектов КАК Цены";

fn claims(diagnostics: Vec<Diagnostic>) -> Vec<(String, String)> {
    diagnostics
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::QueryNestedFieldsByDot)
        .map(|d| (d.code.as_str().to_string(), d.message))
        .collect()
}

fn in_module(query_body: &str) -> Vec<(String, String)> {
    let code = format!(
        "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{query_body}\";\nКонецПроцедуры"
    );
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    let file_id = FileId(0);
    file_set.insert(file_id, VfsPath::new(format!("{FIXTURE}/Ext/SessionModule.bsl")));
    let source_root_id = SourceRootId(0);
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, source_root_id);
    db.set_file_text(file_id, &code);

    let configuration_path_input =
        ide_db::metadata::ConfigurationPathInput::new(&db, FIXTURE.to_string(), 0);
    let provider = ide_db::SalsaProvider::new(&db, Some(configuration_path_input));
    let config = DiagnosticsConfig::all_enabled();
    claims(ide_diagnostics::diagnostics(&DiagnosticsContext::new(&config, file_id, &provider)))
}

fn as_text(query: &str, with_metadata: bool) -> Vec<(String, String)> {
    let configuration = bsl_metadata::load_from_directory(PathBuf::from(FIXTURE))
        .expect("the fixture configuration loads");
    claims(ide_diagnostics::validate_query_text(
        &DiagnosticsConfig::all_enabled(),
        with_metadata.then_some(&configuration),
        query,
    ))
}

fn reported() -> Vec<(String, String)> {
    vec![(
        "QueryNestedFieldsByDot".to_string(),
        "Обнаружено разыменование ссылочного поля".to_string(),
    )]
}

#[test]
fn query_nested_fields_is_listed_as_a_code_that_needs_metadata() {
    assert!(METADATA_DEPENDENT_CODES.contains(&DiagnosticCode::QueryNestedFieldsByDot));
}

#[test]
fn query_nested_fields_is_absent_from_a_check_that_attached_no_metadata() {
    assert_eq!(as_text(COMPOSITE, true), reported(), "the control: with metadata it is there");
    assert_eq!(as_text(COMPOSITE, false), vec![]);
}

#[test]
fn query_nested_fields_a_module_and_bare_text_report_the_same_code_and_message() {
    for (label, query, expected) in [
        ("composite", COMPOSITE, reported()),
        ("single reference", SINGLE, vec![]),
        ("both in one query", MIXED, reported()),
    ] {
        assert_eq!(in_module(query), expected, "[{label}] module");
        assert_eq!(as_text(query, true), expected, "[{label}] bare text");
    }
}
