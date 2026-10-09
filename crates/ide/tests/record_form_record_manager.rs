//! An information register record form: its main attribute `Запись` has the type
//! `cfg:InformationRegisterRecordManager.<Имя>`, and the form module sees it as a
//! form-data structure over the register's columns. A name the register lacks is
//! as absent there as on the record manager itself.

use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

/// One information register `Цены` (dimension `Товар`, resource `Цена`) and its
/// record form `ФормаЗаписи` with the main attribute `Запись`.
fn fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/record_form"))
}

fn record_form_module_path() -> PathBuf {
    fixture_path().join("InformationRegisters/Цены/Forms/ФормаЗаписи/Ext/Form/Module.bsl")
}

fn setup(bsl: &str) -> (RootDatabaseImpl, FileId) {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(record_form_module_path().to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, bsl);
    db.set_all_config_paths(vec![(None, fixture_path())]);
    (db, file_id)
}

/// Every unresolved-member finding of the module, by name.
fn unresolved_members(bsl: &str) -> Vec<String> {
    let (db, file_id) = setup(bsl);
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(format!("field {}", field_name.as_str()))
            }
            InferenceDiagnostic::UnresolvedMethodCall { method_name, .. } => {
                Some(format!("method {}", method_name.as_str()))
            }
            _ => None,
        })
        .collect()
}

/// A dimension, a resource, the form data's own method and its own property are
/// all members of `Запись`; only the column the register does not have is reported.
#[test]
fn a_record_form_reports_only_the_column_its_register_lacks() {
    let bsl =
        "&НаСервере\nПроцедура ПередЗаписьюНаСервере(Отказ, ТекущийОбъект, ПараметрыЗаписи)\n    \
               Товар = Запись.Товар;\n    \
               Цена = Запись.Цена;\n    \
               ЕстьЦена = Запись.Свойство(\"Цена\");\n    \
               Ключ = Запись.ИсходныйКлючЗаписи;\n    \
               Проценты = Запись.Проценты;\nКонецПроцедуры\n";
    assert_eq!(unresolved_members(bsl), vec!["field Проценты".to_string()]);
}
