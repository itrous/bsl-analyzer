//! The built-in interface facts (`source = "bundled"`, and what `auto` degrades
//! to): hover and completion answer with names, signatures and parameters,
//! while descriptions stay empty because the facts carry no 1C text.

#[path = "platform_help/common.rs"]
mod common;

use bsl_platform::{
    install_platform_help, PlatformData, PlatformHelp, PlatformHelpRequest, PlatformHelpSourceKind,
    PlatformHelpStatus,
};
use common::*;
use ide::Analysis;

fn install_bundled() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let _ = install_platform_help(help);
    let data = PlatformData::instance();
    assert_eq!(data.help_request(), &PlatformHelpRequest::Bundled);
    assert_eq!(data.help_origin().map(|o| o.source), Some(PlatformHelpSourceKind::Bundled));
}

#[test]
fn hover_and_completion_work_on_facts_without_texts() {
    install_bundled();
    assert_eq!(
        PlatformData::instance().help_status_for_target(Some("8.3.27")),
        PlatformHelpStatus::Available
    );
    let (db, file_id, _fixture) = database(None);
    let analysis = Analysis::from_database(db);

    let method = hover(&analysis, file_id, "Добавить").expect("method hover on facts");
    assert!(method.contains("Добавить") && method.contains("Значение"), "{method}");

    let position = offset_of("Список.Добавить") + "Список.".len() as u32;
    let completions = analysis.completions(file_id, position, None, ide::Locale::Ru);
    let add = completions.iter().find(|item| item.label == "Добавить").expect("method completion");
    assert!(
        completions.iter().any(|item| item.label == "Количество"),
        "the type's members come from the facts: {completions:?}"
    );
    assert!(
        add.documentation.as_deref().is_none_or(|docs| !docs.contains("appends one element")),
        "no corpus text may appear: {add:?}"
    );

    let function = hover(&analysis, file_id, "СтрДлина(").expect("global function hover");
    assert!(function.contains("СтрДлина") && function.contains("Строка"), "{function}");
    let keyword = hover(&analysis, file_id, "Если").expect("keyword hover");
    assert!(keyword.contains("Условный оператор"), "{keyword}");
}

#[test]
fn facts_resolve_members_and_edt_absence_rules_stay() {
    install_bundled();
    let data = PlatformData::instance();
    let add = data.get_method("Массив", "Добавить").expect("facts know the method");
    assert!(data.get_method_docs(add.id).is_some_and(|docs| docs.description.is_empty()));

    let (db, file_id, _fixture) = database(Some("8.3.27"));
    let unresolved = unresolved_bare_calls(&db, file_id);
    assert!(unresolved.iter().any(|n| n == "СтрДлинаНеСуществует"), "{unresolved:?}");
    assert!(!unresolved.iter().any(|n| n == "СтрДлина" || n == "Сообщить"), "{unresolved:?}");
}
