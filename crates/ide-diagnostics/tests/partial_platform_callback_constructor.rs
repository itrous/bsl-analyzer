#[path = "support/command_module_export_methods.rs"]
mod command_module_export_methods;
#[path = "support/partial_platform_help.rs"]
mod partial_platform_help;

use bsl_platform::{install_platform_help, InstallOutcome};
use ide_diagnostics::DiagnosticCode;

/// The negative controls that must stay flagged whether callback facts are complete or
/// partial: an unregistered parameter and an unregistered local method are nobody's
/// callbacks, so the guard must not silence their rules.
const UNPROTECTED_CONTROLS: [&str; 2] = ["ОбычныйПараметр", "ОбычныйБезВызовов"];

fn assert_handlers_protected_and_controls_flagged(
    diagnostics: &[ide_diagnostics::Diagnostic],
    context: &str,
) {
    // Presence of the protection: registered callback slots keep their parameters,
    // and registered callback methods stay reachable.
    assert!(
        diagnostics.iter().all(|diagnostic| !diagnostic.message.contains("ФиксированныйПараметр")),
        "{context}: the callback parameter must remain protected: {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().all(|diagnostic| {
            diagnostic.code != DiagnosticCode::UnusedLocalMethod
                || (!diagnostic.message.contains("ПослеКоманды")
                    && !diagnostic.message.contains("ПослеОшибки"))
        }),
        "{context}: the callback methods must remain protected: {diagnostics:#?}"
    );
    // Absence of the protection: the same rules must still fire on unregistered names,
    // otherwise a silent guard would be indistinguishable from a working one.
    for control in UNPROTECTED_CONTROLS {
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.message.contains(control)),
            "{context}: the unregistered control \"{control}\" must stay flagged: {diagnostics:#?}"
        );
    }
}

#[test]
fn partial_array_surface_without_callback_description_keeps_callback_protections() {
    assert_eq!(
        install_platform_help(partial_platform_help::array_only_snapshot()),
        Ok(InstallOutcome::Installed)
    );

    let (diagnostics, _) =
        command_module_export_methods::command_module_callback_guard_diagnostics();
    assert_handlers_protected_and_controls_flagged(&diagnostics, "direct constructor");
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != DiagnosticCode::CommandModuleExportMethods),
        "incomplete constructor facts cannot prove an export is not a callback: {diagnostics:#?}"
    );

    let (dynamic_diagnostics, _) =
        command_module_export_methods::command_module_dynamic_callback_guard_diagnostics();
    assert_handlers_protected_and_controls_flagged(&dynamic_diagnostics, "dynamic constructor");
    assert!(
        dynamic_diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != DiagnosticCode::CommandModuleExportMethods),
        "incomplete constructor facts cannot prove a dynamic callback export is ordinary: {dynamic_diagnostics:#?}"
    );

    // This direct guard control is included to keep its support helper live in this binary.
    // The facts-present counterpart lives in command_module_export_methods_platform_facts.
    let (export_diagnostics, _) =
        command_module_export_methods::command_module_export_diagnostics();
    assert!(export_diagnostics.is_empty(), "{export_diagnostics:#?}");
}
