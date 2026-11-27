//! A reaching-definitions solver that stops before converging must reach the notify
//! projection's completeness. The array pass then has no possible writes to prove
//! against, so its empty answer may not be published as a complete projection: the
//! registration would silently disappear and every consumer of the projection would
//! read a clean module.
//!
//! The solve itself is the production one; `dataflow`'s `test-seams` feature only lets a
//! test install a visit bound no production body reaches, because a monotone reaching
//! definitions solution converges long before the proportional production bound.

use hir::{DefDatabase, ModuleId};
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

/// A dynamic `CallbackDescription` built from a local array: the handler is only
/// proven through the reaching definitions of the constructor's point.
const MODULE: &str = r#"
&НаКлиенте
Процедура Зарегистрировать()
    Перем Параметры;
    Параметры = Новый Массив;
    Параметры.Добавить("Провал");
    Параметры.Добавить(ЭтотОбъект);
    О = Новый("CallbackDescription", Параметры);
КонецПроцедуры
"#;

fn database(source: &str) -> (RootDatabaseImpl, FileId) {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test/Module.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    (db, file_id)
}

#[test]
fn a_converging_solver_publishes_the_dynamic_array_registration() {
    install_bundled_facts();
    let (db, file_id) = database(MODULE);

    let summary = db.module_call_summary(ModuleId::new(file_id));
    assert!(!summary.notify_projection_incomplete, "the production bound converges");
    assert_eq!(summary.notify_regs.len(), 1, "{:#?}", summary.notify_regs);
    assert_eq!(summary.notify_regs[0].callback_name.as_str(), "Провал");
    assert_eq!(
        summary
            .notify_dynamic_signature_handlers
            .iter()
            .map(|handler| handler.as_str())
            .collect::<Vec<_>>(),
        ["Провал"]
    );

    let candidates = hir::semantic_notify_candidates_query(&db, FileIdInput::new(&db, file_id));
    assert!(!candidates.is_empty(), "the array registration is a candidate");
    assert!(candidates.iter().all(|candidate| !candidate.analysis_incomplete));

    let result =
        ide_diagnostics::file_diagnostics_result(&db, file_id, &DiagnosticsConfig::all_enabled());
    assert_eq!(result.completeness, DiagnosticsCompleteness::Complete);
}

#[test]
fn a_reaching_definitions_stop_withholds_the_projection_as_incomplete() {
    install_bundled_facts();
    // A bound no production body reaches makes the executable solver stop on this
    // ordinary body. The notify work budget stays far from exhaustion, so anything
    // incomplete below is caused by the stop, not by the projection spending its work.
    let (db, file_id) = database(MODULE);
    let (summary, candidates, generous, result) =
        dataflow::reaching_defs::with_visit_budget(1, || {
            let summary = db.module_call_summary(ModuleId::new(file_id));
            let candidates =
                hir::semantic_notify_candidates_query(&db, FileIdInput::new(&db, file_id));
            let generous = hir::semantic_notify_facts_with_work_limit(
                &db,
                FileIdInput::new(&db, file_id),
                2_000_000,
            );
            let result = ide_diagnostics::file_diagnostics_result(
                &db,
                file_id,
                &DiagnosticsConfig::all_enabled(),
            );
            (summary, candidates, generous, result)
        });

    assert!(
        summary.notify_projection_incomplete,
        "a stopped possible-write analysis must mark the projection incomplete"
    );
    assert!(
        summary.notify_regs.is_empty(),
        "no registration is proven without the reaching definitions: {:#?}",
        summary.notify_regs
    );
    assert!(
        generous.summary.notify_projection_incomplete,
        "a much larger notify work budget must not hide the reaching-definitions stop"
    );

    // The projection keeps an explicit uncertainty marker, so a consumer reads a
    // protected signature instead of a module without callbacks.
    assert!(!candidates.is_empty(), "the stop must not look like a module without candidates");
    assert!(candidates.iter().all(|candidate| candidate.analysis_incomplete), "{candidates:#?}");
    assert!(candidates.iter().all(|candidate| candidate.signature_protection()
        == hir::NotifySignatureProtection::Indeterminate));

    // The file's diagnostics are not announced as a complete answer.
    assert_eq!(
        result.completeness,
        DiagnosticsCompleteness::Incomplete {
            reason: ide_diagnostics::DiagnosticsIncompleteReason::NotifyProjection,
        }
    );
    assert_eq!(
        result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::MissingNotifyHandler)
            .count(),
        0,
        "an unknown handler is not judged: {:#?}",
        result.diagnostics
    );
}
