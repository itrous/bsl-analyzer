#[path = "support/command_module_export_methods.rs"]
mod command_module_export_methods;
#[path = "support/partial_platform_help.rs"]
mod partial_platform_help;

use bsl_platform::{install_platform_help, InstallOutcome};
use ide_diagnostics::DiagnosticCode;

#[test]
fn partial_array_surface_without_callback_description_keeps_callback_protections() {
    assert_eq!(
        install_platform_help(partial_platform_help::array_only_snapshot()),
        Ok(InstallOutcome::Installed)
    );

    let (diagnostics, _) =
        command_module_export_methods::command_module_callback_guard_diagnostics();
    assert!(
        diagnostics.iter().all(|diagnostic| diagnostic.code != DiagnosticCode::UnusedParameters),
        "the callback parameter must remain protected: {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().all(|diagnostic| {
            diagnostic.code != DiagnosticCode::UnusedLocalMethod
                || !diagnostic.message.contains("ПослеКоманды")
        }),
        "the callback method must remain protected: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != DiagnosticCode::CommandModuleExportMethods),
        "incomplete constructor facts cannot prove an export is not a callback: {diagnostics:#?}"
    );

    let (dynamic_diagnostics, _) =
        command_module_export_methods::command_module_dynamic_callback_guard_diagnostics();
    assert!(
        dynamic_diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != DiagnosticCode::UnusedParameters),
        "dynamic callback parameters remain protected: {dynamic_diagnostics:#?}"
    );
    assert!(
        dynamic_diagnostics.iter().all(|diagnostic| {
            diagnostic.code != DiagnosticCode::UnusedLocalMethod
                || (!diagnostic.message.contains("ПослеКоманды")
                    && !diagnostic.message.contains("ПослеОшибки"))
        }),
        "syntactically known dynamic callback handlers remain protected: {dynamic_diagnostics:#?}"
    );
    assert!(
        dynamic_diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != DiagnosticCode::CommandModuleExportMethods),
        "incomplete constructor facts cannot prove a dynamic callback export is ordinary: {dynamic_diagnostics:#?}"
    );

    // This direct guard control is included to keep its support helper live in this binary.
    let (export_diagnostics, _) =
        command_module_export_methods::command_module_export_diagnostics();
    assert!(export_diagnostics.is_empty(), "{export_diagnostics:#?}");
}
