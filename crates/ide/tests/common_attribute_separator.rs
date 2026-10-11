//! A data separator is a common attribute (`ОбщийРеквизит`) that the platform adds as a field to
//! every object in its composition, although no object's own XML declares it. A configuration
//! built on the standard subsystems library separates its auxiliary data this way, and module
//! code both queries a register by the separator and reads it from a record manager.
//!
//! Fixture: the separator `ОбластьДанныхВспомогательныеДанные` has `AutoUse = Use`, so it reaches
//! the unlisted `РазделенныеНастройки`, while its `Content` lists `НеразделенныеНастройки` with
//! `Use = DontUse`, which keeps that register out of the composition. The second register is the
//! control: the same field read there is a real miss and must still be reported.

use ide::{Analysis, DiagnosticCode, DiagnosticsConfig};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

const SEPARATOR: &str = "ОбластьДанныхВспомогательныеДанные";

fn findings(register: &str) -> (Vec<String>, Vec<String>) {
    let root = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/common_attribute_separator"
    ));
    let code = format!(
        r#"Процедура Тест(Область) Экспорт
    Запрос = Новый Запрос(
    "ВЫБРАТЬ
    |    Т.Ключ КАК Ключ,
    |    Т.{SEPARATOR} КАК Область
    |ИЗ
    |    РегистрСведений.{register} КАК Т
    |ГДЕ
    |    Т.{SEPARATOR} = &Область");
    Запрос.УстановитьПараметр("Область", Область);
    Выборка = Запрос.Выполнить().Выбрать();

    Запись = РегистрыСведений.{register}.СоздатьМенеджерЗаписи();
    Запись.Ключ = "";
    Запись.{SEPARATOR} = Область;
    Значение = Запись.{SEPARATOR};
КонецПроцедуры
"#
    );

    let file_id = FileId(0);
    let path = root.join("CommonModules/Настройки/Ext/Module.bsl");
    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root)]);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, &code);

    let diagnostics =
        Analysis::from_database(db).diagnostics(file_id, &DiagnosticsConfig::all_enabled());
    let messages = |code: DiagnosticCode| -> Vec<String> {
        diagnostics.iter().filter(|d| d.code == code).map(|d| d.message.clone()).collect()
    };
    (messages(DiagnosticCode::UnknownFieldInQuery), messages(DiagnosticCode::UnresolvedField))
}

fn mentions_separator(messages: &[String]) -> usize {
    messages.iter().filter(|message| message.contains(SEPARATOR)).count()
}

#[test]
fn separator_is_a_field_of_a_register_in_its_composition() {
    let (query, unresolved) = findings("РазделенныеНастройки");
    assert_eq!(
        (mentions_separator(&query), mentions_separator(&unresolved)),
        (0, 0),
        "query findings: {query:?}, unresolved fields: {unresolved:?}"
    );
}

#[test]
fn separator_is_still_missing_on_a_register_outside_its_composition() {
    let (query, unresolved) = findings("НеразделенныеНастройки");
    // Two misses each: the selected column and the filter column of the query, the write and
    // the read through the record manager.
    assert_eq!(
        (mentions_separator(&query), mentions_separator(&unresolved)),
        (2, 2),
        "query findings: {query:?}, unresolved fields: {unresolved:?}"
    );
}
