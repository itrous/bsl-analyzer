//! A stop of the lower alias work budget must reach the file's diagnostics result as
//! incompleteness: the semantic projection finishes normally, but its uncertainty
//! marker suppresses every unused-parameter finding of the module, so the result may
//! not be announced as a complete empty answer.

use hir::DefDatabase;
use ide_db::base_db::{FileIdInput, SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{DiagnosticCode, DiagnosticsCompleteness, DiagnosticsConfig};
use vfs::{FileId, FileSet, VfsPath};

fn install_bundled_facts() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let facts =
            bsl_platform::PlatformHelp::without_io(&bsl_platform::PlatformHelpRequest::Bundled)
                .expect("bundled facts need no I/O");
        assert_eq!(
            bsl_platform::install_platform_help(facts),
            Ok(bsl_platform::InstallOutcome::Installed)
        );
    });
}

/// One body whose alias-index pass alone runs past its work budget (the semantic
/// projection's budget is far larger and finishes), plus an ordinary uncalled method
/// whose unused parameter the uncertainty marker must suppress.
fn module() -> String {
    let mut code =
        String::from("Процедура Регистрация()\nПерем Параметры;\nПараметры = Новый Массив;\n");
    for _ in 0..1_400 {
        code.push_str("Параметры.Добавить(\"ПредполагаемыйОбработчик\");\n");
    }
    code.push_str(
        "Параметры.Добавить(ЭтотОбъект);\nО = Новый(\"CallbackDescription\", Параметры);\nКонецПроцедуры\n\nПроцедура Посторонняя(Неиспользуемый)\nСообщить(1);\nКонецПроцедуры\n",
    );
    code
}

#[test]
fn lower_alias_budget_stop_reports_incomplete_result_with_suppressed_findings() {
    install_bundled_facts();
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test/Module.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, &module());
    // The lower stop landed in the semantic summary, so every consumer sees it.
    let summary = db.module_call_summary(hir::ModuleId::new(file_id));
    let file = FileIdInput::new(&db, file_id);
    let generous = hir::semantic_notify_facts_with_work_limit(&db, file, 2_000_000);
    assert_eq!(*summary, *generous.summary, "the default semantic budget must finish");
    assert_eq!(
        *hir::semantic_notify_candidates_query(&db, file),
        *generous.notify_candidates,
        "the default semantic projection must match a much larger budget"
    );
    assert!(
        summary.notify_projection_incomplete,
        "the alias budget stop must set the projection incompleteness flag"
    );
    assert!(
        !summary.notify_dynamic_signature_analysis_incomplete.is_empty(),
        "the stop must keep its module-level uncertainty gap"
    );

    let result =
        ide_diagnostics::file_diagnostics_result(&db, file_id, &DiagnosticsConfig::all_enabled());
    let unused: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::UnusedParameters)
        .collect();
    assert!(
        unused.is_empty(),
        "the uncertainty marker suppresses every unused-parameter finding: {unused:#?}"
    );
    assert_eq!(
        result.completeness,
        DiagnosticsCompleteness::Incomplete {
            reason: ide_diagnostics::DiagnosticsIncompleteReason::NotifyProjection,
        },
        "suppressed findings behind a budget stop must not be announced as complete"
    );
}

#[test]
fn ordinary_module_without_alias_stops_reports_complete() {
    install_bundled_facts();
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test/Module.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    let code = "Процедура Посторонняя(Неиспользуемый)\nСообщить(1);\nКонецПроцедуры\n";
    db.set_file_text(file_id, code);

    let result =
        ide_diagnostics::file_diagnostics_result(&db, file_id, &DiagnosticsConfig::all_enabled());
    assert_eq!(result.completeness, DiagnosticsCompleteness::Complete);
    assert_eq!(
        result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::UnusedParameters)
            .count(),
        1,
        "without a budget stop the unused parameter is reported as usual"
    );
}
