#[path = "support/command_module_export_methods.rs"]
mod command_module_export_methods;
#[path = "support/partial_platform_help.rs"]
mod partial_platform_help;

use bsl_platform::{install_platform_help, InstallOutcome, PlatformData};
use ide_diagnostics::DiagnosticCode;

/// The negative controls that must stay flagged whether callback facts are complete or
/// partial: an unregistered parameter and an unregistered local method are nobody's
/// callbacks, so the guard must not silence their rules.
const UNPROTECTED_CONTROLS: [&str; 2] = ["ОбычныйПараметр", "ОбычныйБезВызовов"];
const ORDINARY_EXPORT: &str = "ОбычныйЭкспорт";
const CALLBACK_EXPORTS: [&str; 2] = ["ПослеКоманды", "ПослеОшибки"];

fn reported_export_names<'a>(
    source: &'a str,
    diagnostics: &[ide_diagnostics::Diagnostic],
) -> Vec<&'a str> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::CommandModuleExportMethods)
        .map(|diagnostic| {
            let start: usize = diagnostic.range.start().into();
            let end: usize = diagnostic.range.end().into();
            &source[start..end]
        })
        .collect()
}

fn assert_handlers_protected_and_controls_flagged(
    source: &str,
    diagnostics: &[ide_diagnostics::Diagnostic],
    context: &str,
) {
    // Presence of the protection: registered callback slots keep their parameters,
    // registered callback methods stay reachable, and callback exports stay exempt.
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
    let export_names = reported_export_names(source, diagnostics);
    assert_eq!(
        export_names,
        [ORDINARY_EXPORT],
        "{context}: only the ordinary export may be reported while callback exports stay exempt: {diagnostics:#?}"
    );
    for callback in CALLBACK_EXPORTS {
        assert!(
            !export_names.contains(&callback),
            "{context}: the registered callback export \"{callback}\" must stay exempt: {diagnostics:#?}"
        );
    }
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
fn partial_callback_facts_report_ordinary_exports_but_keep_callback_protections() {
    assert_eq!(
        install_platform_help(partial_platform_help::array_only_snapshot()),
        Ok(InstallOutcome::Installed)
    );
    let platform = PlatformData::instance();
    assert!(platform.help_origin().is_some(), "the partial snapshot must be served");
    assert!(
        !bsl_platform::has_callback_description_constructor_facts(platform),
        "the snapshot must keep the callback constructor facts incomplete"
    );

    let (diagnostics, source) =
        command_module_export_methods::command_module_callback_guard_diagnostics();
    assert_handlers_protected_and_controls_flagged(&source, &diagnostics, "direct constructor");

    let (dynamic_diagnostics, dynamic_source) =
        command_module_export_methods::command_module_dynamic_callback_guard_diagnostics();
    assert_handlers_protected_and_controls_flagged(
        &dynamic_source,
        &dynamic_diagnostics,
        "dynamic constructor",
    );

    // This direct guard control is included to keep its support helper live in this binary.
    // The facts-present counterpart lives in command_module_export_methods_platform_facts.
    let (export_diagnostics, export_source) =
        command_module_export_methods::command_module_export_diagnostics();
    assert_eq!(
        reported_export_names(&export_source, &export_diagnostics),
        [ORDINARY_EXPORT],
        "partial facts must not hide ordinary command exports: {export_diagnostics:#?}"
    );
}
