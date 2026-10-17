//! A bare call whose name is both a method of the implicit receiver and a platform
//! global function. In an object or manager module the platform binds it to the
//! receiver: `ПолучитьФорму()` in a report object module and
//! `ПолучитьДанныеВыбора(Параметры)` in a catalog manager module compile, although
//! the same-named globals require one and two arguments; four arguments to
//! `ПолучитьФорму` in an object module are refused at the fourth, where the global
//! accepts six (checked on 8.3.17 and 8.3.27). The documented exception is
//! `ПолучитьДанныеВыбора` with two arguments in a manager module, which calls the
//! global function.

use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup_at(relative: &str, text: &str) -> (RootDatabaseImpl, FileId) {
    let path = designer_fixture_path().join(relative);
    let file_id = FileId::from_raw(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    file_set.insert(file_id, VfsPath::new(path.to_string_lossy().to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    (db, file_id)
}

/// `(required, total, found)` of every argument-count finding, plus the number of
/// argument type findings.
fn argument_findings(
    db: &RootDatabaseImpl,
    file_id: FileId,
) -> (Vec<(usize, usize, usize)>, usize) {
    let diagnostics = db.arg_diagnostics(file_id);
    let counts = diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::MismatchedArgCount {
                required_count, total_count, found, ..
            } => Some((*required_count, *total_count, *found)),
            _ => None,
        })
        .collect();
    let types = diagnostics
        .iter()
        .filter(|(_, diag)| matches!(diag, InferenceDiagnostic::TypeMismatch { .. }))
        .count();
    (counts, types)
}

const REPORT_OBJECT_MODULE: &str = "Reports/ТестовыйОтчёт/Ext/ObjectModule.bsl";
const CATALOG_MANAGER_MODULE: &str = "Catalogs/Справочник1/Ext/ManagerModule.bsl";
const COMMON_MODULE: &str = "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl";

#[test]
fn a_bare_receiver_method_wins_over_the_same_named_global() {
    let report = "Процедура Тест()\n    Форма = ПолучитьФорму();\n    \
                  Форма = ПолучитьФорму(\"Форма\");\n    \
                  Форма = ПолучитьФорму(\"Форма\", Неопределено, \"Ключ\");\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(REPORT_OBJECT_MODULE, report);
    assert_eq!(
        argument_findings(&db, file_id),
        (vec![], 0),
        "ПолучитьФорму in a report object module is the report's own method"
    );

    // With two arguments the manager module calls the global function — the
    // manager method's help says so, and the probe returned the data of the type
    // passed first.
    let manager = "Функция Тест(Параметры)\n    \
                   Данные = ПолучитьДанныеВыбора(Тип(\"СправочникСсылка.Справочник1\"), Параметры);\n    \
                   Возврат ПолучитьДанныеВыбора(Параметры);\nКонецФункции\n";
    let (db, file_id) = setup_at(CATALOG_MANAGER_MODULE, manager);
    assert_eq!(
        argument_findings(&db, file_id),
        (vec![], 0),
        "ПолучитьДанныеВыбора in a catalog manager module is the manager's own method"
    );
}

/// Control: without an implicit receiver the same spellings are the global
/// functions, which require one and two arguments.
#[test]
fn a_common_module_still_judges_the_global_function() {
    let text = "Процедура Тест(Параметры)\n    Форма = ПолучитьФорму();\n    \
                Данные = ПолучитьДанныеВыбора(Параметры);\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(COMMON_MODULE, text);
    let (counts, _) = argument_findings(&db, file_id);
    assert_eq!(counts, vec![(1, 6, 0), (2, 2, 1)], "the globals keep their contract");
}

/// Control: the receiver's own signature is judged — one argument past it fires.
#[test]
fn an_arity_error_on_the_receiver_method_still_fires() {
    let report = "Процедура Тест()\n    \
                  Форма = ПолучитьФорму(\"Форма\", Неопределено, Неопределено, Неопределено);\n\
                  КонецПроцедуры\n";
    let (db, file_id) = setup_at(REPORT_OBJECT_MODULE, report);
    let (counts, _) = argument_findings(&db, file_id);
    assert_eq!(counts, vec![(0, 3, 4)], "the report's ПолучитьФорму takes at most three");

    let manager = "Функция Тест(Параметры)\n    \
                   Возврат ПолучитьДанныеВыбора(Параметры, 2, 3);\nКонецФункции\n";
    let (db, file_id) = setup_at(CATALOG_MANAGER_MODULE, manager);
    let (counts, _) = argument_findings(&db, file_id);
    assert_eq!(counts.len(), 1, "three arguments exceed the manager's method: {counts:?}");
    assert_eq!(counts[0].2, 3);
}
