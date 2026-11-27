//! Partial callback facts with a readable constructor signature: the served snapshot
//! cannot project the callback slots from parameter names, but the resolver still proves
//! arity verdicts from the five-parameter signature. A constructor call the resolver
//! rejects by arity must publish no registration — the platform never executes it —
//! while a well-argued call without projected slots keeps the bundled-slot protection.

#[path = "support/partial_platform_help.rs"]
mod partial_platform_help;

use bsl_platform::{install_platform_help, InstallOutcome, PlatformData};
use ide_db::base_db::{FileIdInput, SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

const HANDLER: &str = "\n&НаКлиенте\nПроцедура ПослеКоманды(П) Экспорт\nКонецПроцедуры\n";

fn source(constructor_call: &str) -> String {
    format!(
        "Процедура Зарегистрировать()\n    Описание = Новый ОписаниеОповещения(\"ПослеКоманды\", ЭтотОбъект{constructor_call});\nКонецПроцедуры{HANDLER}"
    )
}

fn notify_facts(source: &str) -> (Vec<String>, Vec<hir::NotifyCandidate>) {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);

    let facts =
        hir::semantic_notify_facts_with_work_limit(&db, FileIdInput::new(&db, file_id), usize::MAX);
    let summary = &facts.summary;
    assert!(
        !summary.notify_projection_incomplete,
        "the generous budget must finish the projection: {summary:#?}"
    );
    let reg_names =
        summary.notify_regs.iter().map(|reg| reg.callback_name.as_str().to_string()).collect();
    (reg_names, facts.notify_candidates.as_ref().clone())
}

#[test]
fn arity_rejected_constructor_publishes_no_registration_while_well_argued_stays_protected() {
    // Both tests of this binary serve the same snapshot; whichever thread runs first
    // installs it, the other observes the same source already active.
    let outcome =
        install_platform_help(partial_platform_help::unreadable_callback_constructor_snapshot());
    assert!(
        matches!(outcome, Ok(InstallOutcome::Installed | InstallOutcome::AlreadyActive)),
        "the partial snapshot must serve this binary: {outcome:?}"
    );
    let platform = PlatformData::instance();
    assert!(platform.help_origin().is_some(), "the partial snapshot must be served");
    assert!(
        !bsl_platform::has_callback_description_constructor_facts(platform),
        "the snapshot must keep the callback slots unprojectable"
    );

    // The resolver proves the call invalid by the served signature (six arguments
    // against five parameters), so no registration may be projected for it: the same
    // call under complete facts publishes nothing either. A registration-free
    // compatibility candidate may stay — partial-facts protection by name predates
    // the semantic normalization — but it must carry no registration.
    let (rejected_regs, rejected_candidates) = notify_facts(&source(", 1, 2, 3, 4"));
    assert!(
        rejected_regs.is_empty(),
        "an arity-rejected constructor call must publish no registration: {rejected_regs:#?}"
    );
    assert!(
        rejected_candidates.iter().all(|candidate| candidate.registration.is_none()),
        "an arity-rejected constructor call must classify no registration: {rejected_candidates:#?}"
    );

    // The bundled-slot protection is for calls the resolver could not reject: the
    // same snapshot without projected slots must still register the handler and
    // keep its local signature protected.
    let (protected_regs, protected_candidates) = notify_facts(&source(""));
    assert_eq!(
        protected_regs,
        vec!["ПослеКоманды".to_string()],
        "the well-argued call must keep its bundled-slot registration: {protected_regs:#?}"
    );
    assert!(
        protected_candidates.iter().any(|candidate| {
            candidate.callback_name.as_str() == "ПослеКоманды"
                && candidate.protects_local_method
                && candidate.registration.is_some()
        }),
        "the well-argued call must keep its protected classified candidate: {protected_candidates:#?}"
    );
}
