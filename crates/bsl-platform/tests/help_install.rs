//! The process serves one snapshot: the first selection wins, repeating it is
//! idempotent, and a different one is refused with a restart request. Runs in its
//! own test binary because the snapshot is process-wide.

use bsl_platform::{
    active_platform_help_request, install_platform_help, InstallOutcome, PlatformData,
    PlatformGlobalCatalog, PlatformHelp, PlatformHelpRequest,
};

#[test]
fn first_selection_is_fixed_for_the_process() {
    assert!(active_platform_help_request().is_none(), "nothing may fix the snapshot early");

    let none = PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap();
    assert_eq!(install_platform_help(none.clone()), Ok(InstallOutcome::Installed));
    assert_eq!(active_platform_help_request(), Some(&PlatformHelpRequest::None));
    assert_eq!(install_platform_help(none), Ok(InstallOutcome::AlreadyActive));

    let conflict =
        install_platform_help(PlatformHelp::missing(PlatformHelpRequest::Auto, "x")).unwrap_err();
    assert_eq!(conflict.active, PlatformHelpRequest::None);
    assert_eq!(conflict.requested, PlatformHelpRequest::Auto);

    // Every consumer, the EDT catalog included, sees the installed selection.
    assert!(PlatformData::instance().all_methods().is_empty());
    assert_eq!(PlatformData::instance().help_request(), &PlatformHelpRequest::None);
    assert!(!PlatformGlobalCatalog::instance().symbols().is_empty(), "EDT catalog stays");
}
