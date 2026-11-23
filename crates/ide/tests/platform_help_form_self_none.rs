#[path = "platform_help_self/support.rs"]
mod support;

use bsl_platform::{install_platform_help, PlatformData, PlatformHelp, PlatformHelpRequest};
use support::*;

#[test]
fn absent_form_surface_does_not_prove_bare_form_names_absent() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap();
    let _ = install_platform_help(help);
    assert_eq!(PlatformData::instance().help_request(), &PlatformHelpRequest::None);

    let (db, file_id, root) = database();
    assert_managed_form(root.path(), &db, file_id);
    assert!(PlatformData::instance().all_types().is_empty());
    let (calls, names) = unresolved(&db, file_id);
    assert!(!calls.iter().any(|name| name == "Закрыть"), "{calls:?}");
    assert!(!names.iter().any(|name| name == "Заголовок"), "{names:?}");
}
