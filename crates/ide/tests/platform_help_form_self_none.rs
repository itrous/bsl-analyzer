#[path = "platform_help_self/support.rs"]
mod support;

use bsl_platform::{PlatformData, PlatformHelp, PlatformHelpRequest, install_platform_help};
use ide_db::base_db::SourceDatabase;
use support::*;

#[test]
fn absent_form_surface_does_not_prove_bare_form_names_absent() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap();
    let _ = install_platform_help(help);
    assert_eq!(PlatformData::instance().help_request(), &PlatformHelpRequest::None);

    let (mut db, file_id, root) = database();
    assert_managed_form(root.path(), &db, file_id);
    assert!(PlatformData::instance().all_types().is_empty());
    db.set_file_text(
        file_id,
        "Процедура Тест()\n    Результат = НеизвестноеИмяФормы;\nКонецПроцедуры\n",
    );
    let (calls, names) = unresolved(&db, file_id);
    assert!(!calls.iter().any(|name| name == "Закрыть"), "{calls:?}");
    assert!(
        !names.iter().any(|name| name == "НеизвестноеИмяФормы"),
        "an unreadable implicit receiver makes an ordinary read indeterminate: {names:?}"
    );
}
