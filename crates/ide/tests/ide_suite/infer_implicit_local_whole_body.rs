//! A handler reads what the protected statements assigned before the exception, so such a
//! read is a read of the implicit local (possibly uninitialized), not an unknown name; a
//! name never assigned in the body stays unknown.

use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

const COMMON_MODULE: &str = "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl";

/// Every body that enters the global context of the fixture; unread, the miss would be
/// indeterminate and the rule would say nothing.
const GLOBAL_SURFACE: &[&str] = &[
    "CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl",
    "Ext/ManagedApplicationModule.bsl",
    "Ext/ExternalConnectionModule.bsl",
    "Ext/SessionModule.bsl",
];

fn setup(text: &str) -> (RootDatabaseImpl, FileId) {
    let path = designer_fixture_path().join(COMMON_MODULE);
    let file_id = FileId::from_raw(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    file_set.insert(file_id, VfsPath::new(path.to_string_lossy().to_string()));
    let mut extra = Vec::new();
    for (index, global) in GLOBAL_SURFACE.iter().enumerate() {
        let global_path = designer_fixture_path().join(global);
        let Ok(global_text) = std::fs::read_to_string(&global_path) else { continue };
        let global_id = FileId::from_raw(100 + index as u32);
        file_set.insert(global_id, VfsPath::new(global_path.to_string_lossy().to_string()));
        extra.push((global_id, global_text));
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

fn absent_names(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedName { name, .. } => Some(name.as_str().to_string()),
            _ => None,
        })
        .collect()
}

fn uninitialized_names(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UninitializedRead { name, .. } => Some(name.as_str().to_string()),
            _ => None,
        })
        .collect()
}

/// The read sits on the right-hand side of the assignment that declares the variable. No
/// write has executed when it runs, and the model reports that as an unknown name by
/// design (see `absent_name_reported_only_as_unresolved` in ide-diagnostics): this pins
/// the model, it is not a defect.
#[test]
fn a_read_before_any_write_on_every_path_stays_an_unknown_name() {
    let text = "Процедура Шаблон()\n    ТекстЗапроса = СтрЗаменить(ТекстЗапроса, \"СтатьяА\", \"СтатьяБ\");\nКонецПроцедуры\n";
    let (db, file_id) = setup(text);
    assert_eq!(absent_names(&db, file_id), vec!["ТекстЗапроса".to_string()]);
}

/// The assignment does not dominate the read: it is in a loop, the read in the handler.
const NOT_DOMINATING: &str = "Процедура Шаблон(Коллекция)
    Попытка
        Для Каждого Строка Из Коллекция Цикл
            КодСчета = Строка.Код;
        КонецЦикла;
    Исключение
        Сообщить(Строка(КодСчета));
    КонецПопытки;
КонецПроцедуры
";

#[test]
fn an_assignment_that_does_not_dominate_the_read_still_declares_the_name() {
    let (db, file_id) = setup(NOT_DOMINATING);
    assert_eq!(absent_names(&db, file_id), Vec::<String>::new());
}

/// The control of the two tests above: the same text, but the loop assigns another name,
/// so `КодСчета` is never assigned anywhere in the body and stays an unknown name.
#[test]
fn a_name_never_assigned_in_the_body_is_still_an_unknown_name() {
    let text = NOT_DOMINATING.replace("КодСчета = Строка.Код", "ДругоеИмя = Строка.Код");
    let (db, file_id) = setup(&text);
    assert_eq!(absent_names(&db, file_id), vec!["КодСчета".to_string()]);
}

/// The control of the first test: the right-hand read of a name that is NOT the target.
#[test]
fn a_right_hand_read_of_a_name_other_than_the_target_is_still_unknown() {
    let text = "Процедура Шаблон()\n    ТекстЗапроса = СтрЗаменить(НигдеНеПрисвоено, \"СтатьяА\", \"СтатьяБ\");\nКонецПроцедуры\n";
    let (db, file_id) = setup(text);
    assert_eq!(absent_names(&db, file_id), vec!["НигдеНеПрисвоено".to_string()]);
}

/// What the read becomes instead: an uninitialized read, reported by its own rule. The
/// read is a plain right-hand side here; as an argument of a call it is exempt, because
/// the callee may take it by reference and assign it.
#[test]
fn the_read_no_assignment_dominates_is_an_uninitialized_read() {
    let text = NOT_DOMINATING.replace("Сообщить(Строка(КодСчета))", "Результат = КодСчета");
    let (db, file_id) = setup(&text);
    assert_eq!(absent_names(&db, file_id), Vec::<String>::new());
    assert_eq!(uninitialized_names(&db, file_id), vec!["КодСчета".to_string()]);
}

/// The call-argument form of the same read stays silent in both rules: it is not an unknown
/// name, and an argument may be initialized by the callee.
#[test]
fn a_call_argument_read_is_not_reported_by_either_rule() {
    let (db, file_id) = setup(NOT_DOMINATING);
    assert_eq!(absent_names(&db, file_id), Vec::<String>::new());
    assert_eq!(uninitialized_names(&db, file_id), Vec::<String>::new());
}
