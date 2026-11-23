#[path = "platform_help_self/support.rs"]
mod support;

use bsl_platform::{install_platform_help, PlatformData, PlatformHelp, PlatformHelpRequest};
use support::*;

#[test]
fn bundled_form_members_resolve_but_unattested_absence_is_not_proven() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let _ = install_platform_help(help);
    let (db, file_id, root) = database();
    assert_managed_form(root.path(), &db, file_id);
    assert!(!PlatformData::instance().all_types().is_empty());
    let (calls, names) = unresolved(&db, file_id);
    assert!(!calls.iter().any(|name| name == "Закрыть"), "{calls:?}");
    assert!(
        !calls.iter().any(|name| name == "НеизвестнаяФормаФункция"),
        "the bundled corpus has no member-surface attestation for forms: {calls:?}"
    );
    assert!(!names.iter().any(|name| name == "Заголовок"), "{names:?}");
}
