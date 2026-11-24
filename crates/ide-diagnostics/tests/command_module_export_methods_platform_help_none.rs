#[path = "support/command_module_export_methods.rs"]
mod command_module_export_methods;

use bsl_platform::{install_platform_help, InstallOutcome, PlatformHelp, PlatformHelpRequest};

#[test]
fn platform_help_none_silences_registered_and_ordinary_command_exports() {
    assert_eq!(
        install_platform_help(
            PlatformHelp::without_io(&PlatformHelpRequest::None).expect("none needs no I/O")
        ),
        Ok(InstallOutcome::Installed)
    );

    let (diagnostics, _) = command_module_export_methods::command_module_export_diagnostics();
    assert!(
        diagnostics.is_empty(),
        "without platform facts the command export diagnostic stays silent: {diagnostics:#?}"
    );

    let (chain_diagnostics, _) =
        command_module_export_methods::dynamic_alias_chain_export_diagnostics();
    assert!(
        chain_diagnostics.is_empty(),
        "without callback facts the A→B→C candidate API and diagnostic stay silent: {chain_diagnostics:#?}"
    );
}
