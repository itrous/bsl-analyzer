#[path = "support/command_module_export_methods.rs"]
mod command_module_export_methods;

use bsl_platform::{install_platform_help, InstallOutcome, PlatformHelp, PlatformHelpRequest};

#[test]
fn platform_facts_exempt_the_registered_handler_but_report_ordinary_exports() {
    assert_eq!(
        install_platform_help(
            PlatformHelp::without_io(&PlatformHelpRequest::Bundled)
                .expect("bundled facts need no I/O")
        ),
        Ok(InstallOutcome::Installed)
    );

    let (diagnostics, source) = command_module_export_methods::command_module_export_diagnostics();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let range = diagnostics[0].range;
    let start: usize = range.start().into();
    let end: usize = range.end().into();
    assert_eq!(&source[start..end], "ОбычныйЭкспорт");
}
