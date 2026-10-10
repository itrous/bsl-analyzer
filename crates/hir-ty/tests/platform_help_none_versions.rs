//! Version checks preserve their no-help behavior independently of corpus tests.

use bsl_platform::{
    install_platform_help, InstallOutcome, PlatformGlobalCatalog, PlatformHelp,
    PlatformHelpRequest, PlatformVersion,
};

#[test]
fn missing_help_does_not_invent_member_versions_or_remove_own_registries() {
    assert_eq!(
        install_platform_help(PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap()),
        Ok(InstallOutcome::Installed)
    );
    assert_eq!(hir_ty::min_platform::constructed_type("Массив"), None);
    assert_eq!(hir_ty::min_platform::type_member("Массив", "Добавить", false), None);
    assert_eq!(
        hir_ty::compat_mode::hidden_type_member("Запрос", "ТребуемаяАктуальностьДанных"),
        None
    );
    assert_eq!(
        hir_ty::compat_mode::hidden_global("СтрНайти"),
        Some(PlatformVersion { major: 8, minor: 3, patch: 6, build: None })
    );
    assert!(PlatformGlobalCatalog::instance().contains("СтрНайти"));
}
