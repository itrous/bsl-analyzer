//! A corpus loaded at runtime from the project's own fixture serves hover with
//! its texts; the help status never changes what the EDT catalog decides.

#[path = "platform_help/common.rs"]
mod common;

use bsl_platform::{
    install_platform_help, PlatformData, PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest,
    PlatformHelpSourceKind, PlatformHelpStatus, PlatformSnapshot,
};
use common::*;
use ide::Analysis;

const FIXTURE: &str = include_str!("../../bsl-platform/tests/fixtures/help/corpus.json");

fn install_fixture() {
    let snapshot = PlatformSnapshot::from_corpus_json(FIXTURE.as_bytes()).unwrap();
    let _ = install_platform_help(PlatformHelp::loaded(
        PlatformHelpRequest::ExternalPath("corpus.json".into()),
        snapshot,
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: Some("corpus.json".to_owned()),
            platform_version: Some("8.3.27.2214".to_owned()),
            digest: None,
        },
    ));
}

#[test]
fn hover_shows_texts_of_the_loaded_corpus() {
    install_fixture();
    let (db, file_id, _fixture) = database(None);
    let analysis = Analysis::from_database(db);

    let method = hover(&analysis, file_id, "Добавить").expect("method hover");
    assert!(method.contains("appends one element"), "{method}");
    let position = offset_of("Список.Добавить") + "Список.".len() as u32;
    let completions = analysis.completions(file_id, position, None, ide::Locale::Ru);
    let add = completions.iter().find(|item| item.label == "Добавить").expect("method completion");
    assert!(
        add.documentation.as_deref().is_some_and(|docs| docs.contains("appends one element")),
        "completion serves the loaded docs: {add:?}"
    );

    let function = hover(&analysis, file_id, "СтрДлина(").expect("global function hover");
    assert!(function.contains("length of a string"), "{function}");
    let keyword = hover(&analysis, file_id, "Если").expect("keyword hover");
    assert!(keyword.contains("Условный оператор"), "{keyword}");
}

#[test]
fn help_status_matrix_leaves_edt_absence_rules_alone() {
    install_fixture();
    let data = PlatformData::instance();
    assert_eq!(data.help_status_for_target(Some("8.3.27")), PlatformHelpStatus::Available);
    assert_eq!(data.help_status_for_target(Some("8.3.28")), PlatformHelpStatus::UnsupportedTarget);

    let (db, file_id, _fixture) = database(Some("8.3.27"));
    let unresolved = unresolved_bare_calls(&db, file_id);
    assert!(unresolved.iter().any(|n| n == "СтрДлинаНеСуществует"), "{unresolved:?}");
    assert!(
        !unresolved.iter().any(|n| n == "Сообщить"),
        "EDT knows it without help: {unresolved:?}"
    );

    let (db, file_id, _fixture) = database(Some("8.3.28"));
    assert!(unresolved_bare_calls(&db, file_id).is_empty(), "EDT UnsupportedTarget suppresses");
    // Positive lookups stay available whatever the status.
    assert!(data.get_method("Массив", "Добавить").is_some());
}
