//! `глПеременная.Метод()`, where `глПеременная` is an exported variable of the
//! managed application module.
//!
//! Such a variable is a global of every client module of the configuration, and its
//! value is untyped, so a method called through it is not something inference can
//! judge. Server code does not see it: there the read itself is reported as unavailable.

use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

const APPLICATION_MODULE: &str = "Ext/ManagedApplicationModule.bsl";
const APPLICATION_MODULE_TEXT: &str = "Перем глМояПеременная Экспорт;\n\
                                       Перем глНастройки Экспорт;\n\n\
                                       Процедура ПроцедураПриложения() Экспорт\n\
                                       КонецПроцедуры\n";

/// Every body that enters the global context of the designer fixture, so the user
/// global surface is complete and a miss is a proven absence. The application
/// module is given the exported variables under test.
const GLOBAL_SURFACE: &[&str] = &[
    "CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl",
    APPLICATION_MODULE,
    "Ext/ExternalConnectionModule.bsl",
    "Ext/SessionModule.bsl",
];

const CLIENT_COMMON_MODULE: &str = "CommonModules/КлиентскийОбщийМодуль/Ext/Module.bsl";
const SERVER_COMMON_MODULE: &str = "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl";
const FORM_MODULE: &str = "DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl";

fn setup_at(relative: &str, text: &str) -> (RootDatabaseImpl, FileId) {
    let path = designer_fixture_path().join(relative);
    let file_id = FileId::from_raw(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    file_set.insert(file_id, VfsPath::new(path.to_string_lossy().to_string()));

    let mut extra = Vec::new();
    for (index, global) in GLOBAL_SURFACE.iter().enumerate() {
        let global_path = designer_fixture_path().join(global);
        let text = if *global == APPLICATION_MODULE {
            APPLICATION_MODULE_TEXT.to_string()
        } else {
            std::fs::read_to_string(&global_path).expect("read global-surface fixture body")
        };
        let global_id = FileId::from_raw(100 + index as u32);
        file_set.insert(global_id, VfsPath::new(global_path.to_string_lossy().to_string()));
        extra.push((global_id, text));
    }

    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    for (global_id, global_text) in extra {
        db.set_file_source_root(global_id, SourceRootId(0));
        db.set_file_text(global_id, &global_text);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    (db, file_id)
}

/// `receiver.method` of every unresolved method call, in source order.
fn unresolved_method_calls(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedMethodCall { receiver_name, method_name, .. } => {
                Some(format!("{}.{}", receiver_name.as_str(), method_name.as_str()))
            }
            _ => None,
        })
        .collect()
}

fn unavailable_names(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnavailableInEnvironment { name, .. } => {
                Some(name.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

fn absent_bare_calls(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedBareCall { name, .. } => Some(name.as_str().to_string()),
            _ => None,
        })
        .collect()
}

const CALLS_THROUGH_VARIABLES: &str = "Процедура Тест() Экспорт\n    \
                                       глМояПеременная.Метод();\n    \
                                       глНастройки.Вставить(\"Ключ\", 1);\n    \
                                       Значение = глНастройки.Получить(\"Ключ\");\n\
                                       КонецПроцедуры\n";

#[test]
fn a_call_through_an_application_variable_is_silent_in_a_client_common_module() {
    let (db, file_id) = setup_at(CLIENT_COMMON_MODULE, CALLS_THROUGH_VARIABLES);
    assert!(
        unresolved_method_calls(&db, file_id).is_empty(),
        "an exported application-module variable is a client global: {:?}",
        unresolved_method_calls(&db, file_id)
    );
    assert!(unavailable_names(&db, file_id).is_empty(), "{:?}", unavailable_names(&db, file_id));
}

#[test]
fn a_call_through_an_application_variable_is_silent_in_a_client_form_procedure() {
    let text = "&НаКлиенте\nПроцедура Тест()\n    глМояПеременная.Метод();\n    \
                глНастройки.Вставить(\"Ключ\", 1);\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(FORM_MODULE, text);
    assert!(
        unresolved_method_calls(&db, file_id).is_empty(),
        "an exported application-module variable is a client global: {:?}",
        unresolved_method_calls(&db, file_id)
    );
}

/// Server code does not see the variable. The call is not reported as an unresolved
/// receiver; the read of the variable is reported as unavailable on the server.
#[test]
fn an_application_variable_is_unavailable_in_a_server_common_module() {
    let (db, file_id) = setup_at(SERVER_COMMON_MODULE, CALLS_THROUGH_VARIABLES);
    assert!(
        unresolved_method_calls(&db, file_id).is_empty(),
        "{:?}",
        unresolved_method_calls(&db, file_id)
    );
    assert_eq!(
        unavailable_names(&db, file_id),
        vec!["глМояПеременная".to_string(), "глНастройки".to_string(), "глНастройки".to_string()]
    );
}

/// A misspelled variable is not an application export and is still reported, and
/// so is a bare call to a name the application module does not export.
#[test]
fn a_misspelled_receiver_and_an_undeclared_bare_call_are_still_reported() {
    let text = "Процедура Тест() Экспорт\n    глМояПеремення.Метод();\n    \
                ПроцедураПриложения();\n    ПроцедураПрилжения();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(CLIENT_COMMON_MODULE, text);
    assert_eq!(unresolved_method_calls(&db, file_id), vec!["глМояПеремення.Метод".to_string()]);
    assert_eq!(absent_bare_calls(&db, file_id), vec!["ПроцедураПрилжения".to_string()]);
}
