//! Why this test exists: a reader of a handler's `METADATA` sees `severity: Major` or
//! `Critical` and `diagnostic_type: Error` and concludes that the editor shows an error.
//! It does not. The metadata level and type are kept on purpose (they feed the Sonar rule
//! export), and `lsp_severity_override: "warning"` is what a user actually sees, because
//! `effective_severity()` consults the override before the computed grade.
//!
//! The 15 rules below encode a coding standard or a recommendation, not wrong behaviour at
//! run time. The list is spelled out code by code so that changing the override of any one
//! of them fails here instead of passing silently; it is deliberately not derived by
//! filtering all rules on "has an override".

use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig, Severity};

#[test]
fn standards_rules_keep_major_or_critical_metadata_but_show_warning_in_the_editor() {
    let config = DiagnosticsConfig::default();
    let codes = [
        DiagnosticCode::InvalidCharacterInFile,
        DiagnosticCode::CalculationRegisterVirtualTableWithoutRecorder,
        DiagnosticCode::IncorrectUseLikeInQuery,
        DiagnosticCode::VirtualTableCallWithoutParameters,
        DiagnosticCode::UsingObjectNotAvailableUnix,
        DiagnosticCode::TimeoutsInExternalResources,
        DiagnosticCode::DataExchangeLoading,
        DiagnosticCode::CommonModuleInvalidType,
        DiagnosticCode::SameMetadataObjectAndChildNames,
        DiagnosticCode::UsingHardcodePath,
        DiagnosticCode::UsingHardcodeNetworkAddress,
        DiagnosticCode::MetadataObjectNameLength,
        DiagnosticCode::MissingCodeTryCatchEx,
        DiagnosticCode::MissingTemporaryFileDeletion,
        DiagnosticCode::StyleElementConstructors,
    ];
    for code in codes {
        assert_eq!(config.severity(code), Severity::Warning, "{code:?}");
    }

    // Control: a rule with no static override keeps its computed grade, so the loop above
    // cannot pass with an accessor that always answers `Warning`.
    assert_eq!(config.severity(DiagnosticCode::DeletingCollectionItem), Severity::Major);
}
