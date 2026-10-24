//! Cost of the source-root fold behind `workspace_foreign_handler_bindings`: it reads
//! the text of every file of the root to find the few that bind a handler on a
//! receiver. Through the memoised `file_text_query` each scanned text would stay
//! resident until the next LRU trim, so in a batch the scan alone held the whole
//! configuration's source in memory. Only a candidate - a file the fold goes on to
//! lower - may leave its text memoised.

use hir::DefDatabase;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::DiagnosticsConfig;
use vfs::{FileId, FileSet, VfsPath};

const FORM_MODULE: &str = "/test/Documents/Заказ/Forms/ФормаДокумента/Ext/Form/Module.bsl";

const FORM: &str = r#"
Процедура ОбработчикОжидания1()
    Сообщить(1);
КонецПроцедуры

Процедура ДействительноНеИспользуется(Параметр)
    Сообщить(2);
КонецПроцедуры

Процедура Подготовить() Экспорт
    ОбщийМодуль1.Подключить(ЭтотОбъект);
КонецПроцедуры
"#;

const HELPER: &str = r#"
Процедура Подключить(Форма) Экспорт
    Форма.ПодключитьОбработчикОжидания("ОбработчикОжидания1", 0.1, Истина);
КонецПроцедуры
"#;

/// Modules the scan reads and rejects: plain code, a bare (own-module) binding call,
/// and the method name in a comment and a string, none of them after a dot.
const FILLERS: usize = 24;

fn filler(index: usize) -> String {
    format!(
        "Процедура Метод{index}() Экспорт\n    // ПодключитьОбработчикОжидания\n    \
         ПодключитьОбработчикОжидания(\"Метод{index}\", 1);\n    \
         Сообщить(\"УстановитьДействие\");\nКонецПроцедуры\n"
    )
}

/// The form module, the helper that binds one of its procedures, and [`FILLERS`]
/// common modules, in one source root of a database that counts salsa executions.
fn stand() -> (RootDatabaseImpl, usize) {
    let mut db = RootDatabaseImpl::new_with_salsa_events();
    let mut sources = vec![
        (FORM_MODULE.to_string(), FORM.to_string()),
        ("/test/CommonModules/ОбщийМодуль1/Ext/Module.bsl".to_string(), HELPER.to_string()),
    ];
    sources.extend(
        (0..FILLERS).map(|i| (format!("/test/CommonModules/Модуль{i}/Ext/Module.bsl"), filler(i))),
    );
    let mut files = FileSet::default();
    for (index, (path, source)) in sources.iter().enumerate() {
        let file_id = FileId(index as u32);
        files.insert(file_id, VfsPath::new(path.clone()));
        db.set_file_text(file_id, source);
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    for index in 0..sources.len() {
        db.set_file_source_root(FileId(index as u32), SourceRootId(0));
    }
    (db, sources.len())
}

fn executions(db: &RootDatabaseImpl, family: &str) -> u64 {
    db.salsa_family_churn(family).expect("the stand counts salsa events").0
}

#[test]
fn binding_scan_memoises_only_the_candidates_text() {
    let (db, files) = stand();
    assert!(db.salsa_events_reset());

    let bindings = db.workspace_foreign_handler_bindings(SourceRootId(0));

    assert!(bindings.is_bound("ОбработчикОжидания1"), "the scan must find the helper's binding");
    assert_eq!(bindings.len(), 1, "no filler is a binding");
    assert_eq!(
        executions(&db, "file_mentions_handler_binding_query"),
        files as u64,
        "the scan reads every file of the root"
    );
    assert_eq!(
        executions(&db, "file_text_query"),
        1,
        "only the helper, the one candidate, may keep its text memoised"
    );
    assert_eq!(
        executions(&db, "file_name_offsets_query"),
        0,
        "a candidate's tree is searched for the binding names, not indexed by every name"
    );
}

/// The batch warm-up computes the same fold the form module would ask for, so the
/// module's findings do not depend on whether it ran.
#[test]
fn primed_bindings_give_the_same_findings() {
    let findings = |db: &RootDatabaseImpl| {
        ide_diagnostics::file_diagnostics(db, FileId(0), &DiagnosticsConfig::all_enabled())
            .into_iter()
            .map(|d| format!("{} {:?} {}", d.code.as_str(), d.range, d.message))
            .collect::<Vec<_>>()
    };

    let (cold, files) = stand();
    let (primed, _) = stand();
    assert!(primed.salsa_events_reset());
    primed.prime_foreign_handler_bindings(SourceRootId(0));
    assert_eq!(executions(&primed, "file_mentions_handler_binding_query"), files as u64);
    assert_eq!(executions(&primed, "workspace_foreign_handler_bindings_query"), 1);

    assert!(primed.salsa_events_reset());
    let primed_findings = findings(&primed);
    assert_eq!(
        executions(&primed, "workspace_foreign_handler_bindings_query"),
        0,
        "the form module reads the warmed fold"
    );

    let cold_findings = findings(&cold);
    assert_eq!(primed_findings, cold_findings);
    assert!(
        cold_findings.iter().any(|f| f.contains("\"ДействительноНеИспользуется\"")),
        "{cold_findings:#?}"
    );
    assert!(
        !cold_findings.iter().any(|f| f.contains("\"ОбработчикОжидания1\"")),
        "{cold_findings:#?}"
    );
}
