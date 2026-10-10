//! No platform help (`source = "none"`): platform answers are empty, while local
//! resolution, completion, keyword docs and the EDT-backed absence diagnostics
//! keep working.

#[path = "platform_help/common.rs"]
mod common;

use bsl_platform::{
    install_platform_help, PlatformData, PlatformHelp, PlatformHelpRequest, PlatformHelpStatus,
};
use common::*;
use ide::Analysis;

fn install_none() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap();
    let _ = install_platform_help(help);
    assert_eq!(PlatformData::instance().help_request(), &PlatformHelpRequest::None);
}

#[test]
fn platform_answers_are_empty_but_independent_features_remain() {
    install_none();
    assert_eq!(
        PlatformData::instance().help_status_for_target(Some("8.3.27")),
        PlatformHelpStatus::Missing
    );

    let (db, file_id, _fixture) = database(None);
    let analysis = Analysis::from_database(db);

    let method_hover = hover(&analysis, file_id, "Добавить").unwrap_or_default();
    assert!(
        !method_hover.contains("appends one element"),
        "no help, no method docs: {method_hover}"
    );
    let data = PlatformData::instance();
    assert!(data.all_types().is_empty() && data.all_methods().is_empty());
    assert!(data.get_method("Массив", "Добавить").is_none());
    let position = offset_of("Список.Добавить") + "Список.".len() as u32;
    let items = analysis.completions(file_id, position, None, ide::Locale::Ru);
    assert!(!items.iter().any(|item| item.label == "Добавить"), "no help completion: {items:?}");

    let keyword = hover(&analysis, file_id, "Если").expect("keyword hover stays");
    assert!(keyword.contains("Условный оператор"), "{keyword}");

    let empty_line = common::MODULE.find("    \nКонецПроцедуры").unwrap() as u32 + 4;
    let items = analysis.completions(file_id, empty_line, None, ide::Locale::Ru);
    assert!(items.iter().any(|i| i.label == "Локальная"), "local completion stays");

    let local = analysis.goto_definition(file_id, offset_of("Локальная = 1;") + 1);
    assert!(local.is_some(), "local name resolution stays");
}

#[test]
fn absence_diagnostics_follow_the_edt_catalog_not_help_status() {
    install_none();

    // Complete EDT catalog for the target: a misspelt global is still reported
    // although no help is served, and a known global is not.
    let (db, file_id, _fixture) = database(Some("8.3.27"));
    let unresolved = unresolved_bare_calls(&db, file_id);
    assert!(unresolved.iter().any(|n| n == "СтрДлинаНеСуществует"), "{unresolved:?}");
    assert!(!unresolved.iter().any(|n| n == "Сообщить" || n == "СтрДлина"), "{unresolved:?}");

    // Another release line: the EDT catalog no longer proves absence.
    let (db, file_id, _fixture) = database(Some("8.3.28"));
    assert!(unresolved_bare_calls(&db, file_id).is_empty());
}
