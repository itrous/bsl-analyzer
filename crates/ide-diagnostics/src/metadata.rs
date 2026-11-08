use crate::Severity;
use serde::{Deserialize, Serialize};
use stdx::case::CaseExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CleanCodeAttribute {
    #[serde(rename = "CONVENTIONAL")]
    Consistent,
    #[serde(rename = "CLEAR")]
    Intentional,
    #[serde(rename = "FOCUSED")]
    Adaptable,
    #[serde(rename = "TRUSTWORTHY")]
    Responsible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SoftwareQuality {
    Maintainability,
    Reliability,
    Security,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ImpactSeverity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Impact {
    #[serde(rename = "softwareQuality")]
    pub software_quality: SoftwareQuality,
    pub severity: ImpactSeverity,
}

impl Impact {
    pub const fn new(software_quality: SoftwareQuality, severity: ImpactSeverity) -> Self {
        Self { software_quality, severity }
    }
}

pub const fn derive_clean_code_attribute(
    tag: MetadataTag,
    dtype: DiagnosticType,
) -> CleanCodeAttribute {
    match (tag, dtype) {
        (_, DiagnosticType::Vulnerability | DiagnosticType::SecurityHotspot) => {
            CleanCodeAttribute::Responsible
        }

        (MetadataTag::Localize, _) => CleanCodeAttribute::Responsible,

        (MetadataTag::Standard, _) => CleanCodeAttribute::Consistent,
        (MetadataTag::Deprecated, _) => CleanCodeAttribute::Consistent,

        (MetadataTag::Design, _) => CleanCodeAttribute::Adaptable,
        (MetadataTag::Performance, _) => CleanCodeAttribute::Adaptable,
        (MetadataTag::Unused, _) => CleanCodeAttribute::Adaptable,
        (MetadataTag::Lockinos, _) => CleanCodeAttribute::Adaptable,

        (MetadataTag::Error, _) => CleanCodeAttribute::Intentional,
        (MetadataTag::Suspicious, _) => CleanCodeAttribute::Intentional,
        (MetadataTag::Unpredictable, _) => CleanCodeAttribute::Intentional,
        (MetadataTag::Brainoverload, _) => CleanCodeAttribute::Intentional,
        (MetadataTag::Clumsy, _) => CleanCodeAttribute::Intentional,

        (MetadataTag::Badpractice, DiagnosticType::Error) => CleanCodeAttribute::Intentional,
        (MetadataTag::Badpractice, _) => CleanCodeAttribute::Consistent,

        (MetadataTag::Sql, DiagnosticType::Error) => CleanCodeAttribute::Intentional,
        (MetadataTag::Sql, _) => CleanCodeAttribute::Adaptable,
    }
}

pub const fn derive_primary_impact(
    dtype: DiagnosticType,
    severity: DiagnosticSeverityLevel,
) -> Impact {
    let software_quality = match dtype {
        DiagnosticType::Vulnerability | DiagnosticType::SecurityHotspot => {
            SoftwareQuality::Security
        }
        DiagnosticType::Error => SoftwareQuality::Reliability,
        DiagnosticType::CodeSmell => SoftwareQuality::Maintainability,
    };

    let impact_severity = match severity {
        DiagnosticSeverityLevel::Blocker | DiagnosticSeverityLevel::Critical => {
            ImpactSeverity::High
        }
        DiagnosticSeverityLevel::Major => ImpactSeverity::Medium,
        DiagnosticSeverityLevel::Minor | DiagnosticSeverityLevel::Info => ImpactSeverity::Low,
    };

    Impact::new(software_quality, impact_severity)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DiagnosticType {
    Error,
    CodeSmell,
    Vulnerability,
    SecurityHotspot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DiagnosticSeverityLevel {
    Info,
    Minor,
    Major,
    Critical,
    Blocker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MetadataTag {
    Standard,
    Lockinos,
    Sql,
    Performance,
    Brainoverload,
    Badpractice,
    Clumsy,
    Design,
    Suspicious,
    Unpredictable,
    Deprecated,
    Unused,
    Error,
    Localize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticScope {
    All,
    Os,
    Bsl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticCompatibilityMode {
    Undefined,
    CompatibilityMode8_3_1,
    CompatibilityMode8_3_3,
    CompatibilityMode8_3_6,
    CompatibilityMode8_3_10,
    CompatibilityMode8_3_12,
    CompatibilityMode8_3_14,
    CompatibilityMode8_3_17,
    Compatibility8320,
}

#[derive(Debug, Clone, Copy)]
pub struct DiagnosticMetadata {
    pub diagnostic_type: DiagnosticType,
    pub severity: DiagnosticSeverityLevel,
    pub scope: DiagnosticScope,
    pub modules: &'static [bsl_metadata::ModuleType],
    pub minutes_to_fix: u32,
    pub activated_by_default: bool,
    pub compatibility_mode: DiagnosticCompatibilityMode,
    pub tags: &'static [MetadataTag],
    pub can_locate_on_project: bool,
    pub extra_min_for_complexity: f64,
    pub lsp_severity_override: &'static str,
    pub clean_code_attribute: CleanCodeAttribute,
    pub impacts: &'static [Impact],
}

// Two entry points: appliers of a severity want the `Warning` fallback, the project
// config check wants to know that the name was not recognised.
pub fn parse_severity_opt(s: &str) -> Option<Severity> {
    Some(match s.fold_lower().as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "information" | "info" => Severity::Information,
        "hint" => Severity::Hint,
        "blocker" => Severity::Blocker,
        "critical" => Severity::Critical,
        "major" => Severity::Major,
        _ => return None,
    })
}

pub fn parse_severity(s: &str) -> Severity {
    parse_severity_opt(s).unwrap_or(Severity::Warning)
}

impl DiagnosticMetadata {
    /// The severity a finding carries with no user override: the static
    /// `lsp_severity_override` when the metadata sets one, else the grade computed
    /// from the diagnostic type and level.
    pub fn effective_severity(&self) -> Severity {
        if !self.lsp_severity_override.is_empty() {
            return parse_severity(self.lsp_severity_override);
        }
        self.calculate_severity()
    }

    pub const fn calculate_severity(&self) -> Severity {
        match self.diagnostic_type {
            DiagnosticType::CodeSmell => match self.severity {
                DiagnosticSeverityLevel::Info => Severity::Hint,
                DiagnosticSeverityLevel::Minor => Severity::Information,
                DiagnosticSeverityLevel::Major
                | DiagnosticSeverityLevel::Critical
                | DiagnosticSeverityLevel::Blocker => Severity::Warning,
            },
            DiagnosticType::SecurityHotspot => Severity::Warning,
            DiagnosticType::Error | DiagnosticType::Vulnerability => match self.severity {
                DiagnosticSeverityLevel::Blocker => Severity::Blocker,
                DiagnosticSeverityLevel::Critical => Severity::Critical,
                DiagnosticSeverityLevel::Major => Severity::Major,
                DiagnosticSeverityLevel::Minor | DiagnosticSeverityLevel::Info => Severity::Error,
            },
        }
    }
}

const DEFAULT_IMPACT: Impact =
    Impact::new(SoftwareQuality::Maintainability, ImpactSeverity::Medium);

impl Default for DiagnosticMetadata {
    fn default() -> Self {
        Self {
            diagnostic_type: DiagnosticType::CodeSmell,
            severity: DiagnosticSeverityLevel::Major,
            scope: DiagnosticScope::All,
            modules: &[],
            minutes_to_fix: 5,
            activated_by_default: true,
            compatibility_mode: DiagnosticCompatibilityMode::Undefined,
            tags: &[],
            can_locate_on_project: false,
            extra_min_for_complexity: 0.0,
            lsp_severity_override: "",
            clean_code_attribute: CleanCodeAttribute::Intentional,
            impacts: &[DEFAULT_IMPACT],
        }
    }
}

#[macro_export]
macro_rules! define_metadata {
    (
        diagnostic_type: $dtype:expr,
        severity: $severity:expr,
        scope: $scope:expr,
        modules: $modules:expr,
        minutes_to_fix: $mtf:expr,
        activated_by_default: $abd:expr,
        compatibility_mode: $cm:expr,
        tags: $tags:expr,
        can_locate_on_project: $clop:expr,
        extra_min_for_complexity: $emfc:expr,
        lsp_severity_override: $lso:expr,
        clean_code_attribute: $cca:expr $(,)?
    ) => {{
        const IMPACT: $crate::metadata::Impact =
            $crate::metadata::derive_primary_impact($dtype, $severity);

        $crate::metadata::DiagnosticMetadata {
            diagnostic_type: $dtype,
            severity: $severity,
            scope: $scope,
            modules: $modules,
            minutes_to_fix: $mtf,
            activated_by_default: $abd,
            compatibility_mode: $cm,
            tags: $tags,
            can_locate_on_project: $clop,
            extra_min_for_complexity: $emfc,
            lsp_severity_override: $lso,
            clean_code_attribute: $cca,
            impacts: &[IMPACT],
        }
    }};
    (
        diagnostic_type: $dtype:expr,
        severity: $severity:expr,
        scope: $scope:expr,
        modules: $modules:expr,
        minutes_to_fix: $mtf:expr,
        activated_by_default: $abd:expr,
        compatibility_mode: $cm:expr,
        tags: $tags:expr,
        can_locate_on_project: $clop:expr,
        extra_min_for_complexity: $emfc:expr,
        lsp_severity_override: $lso:expr $(,)?
    ) => {{
        const FIRST_TAG: $crate::metadata::MetadataTag =
            if $tags.is_empty() { $crate::metadata::MetadataTag::Badpractice } else { $tags[0] };
        const CCA: $crate::metadata::CleanCodeAttribute =
            $crate::metadata::derive_clean_code_attribute(FIRST_TAG, $dtype);
        const IMPACT: $crate::metadata::Impact =
            $crate::metadata::derive_primary_impact($dtype, $severity);

        $crate::metadata::DiagnosticMetadata {
            diagnostic_type: $dtype,
            severity: $severity,
            scope: $scope,
            modules: $modules,
            minutes_to_fix: $mtf,
            activated_by_default: $abd,
            compatibility_mode: $cm,
            tags: $tags,
            can_locate_on_project: $clop,
            extra_min_for_complexity: $emfc,
            lsp_severity_override: $lso,
            clean_code_attribute: CCA,
            impacts: &[IMPACT],
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_severity_opt_agrees_with_parse_severity() {
        let known = [
            ("error", Severity::Error),
            ("Warning", Severity::Warning),
            ("information", Severity::Information),
            ("INFO", Severity::Information),
            ("hint", Severity::Hint),
            ("blocker", Severity::Blocker),
            ("critical", Severity::Critical),
            ("major", Severity::Major),
        ];
        for (name, severity) in known {
            assert_eq!(parse_severity_opt(name), Some(severity), "{name}");
            assert_eq!(parse_severity(name), severity, "{name}");
        }
        assert_eq!(parse_severity_opt("warnng"), None);
        assert_eq!(parse_severity("warnng"), Severity::Warning);
    }

    #[test]
    fn test_severity_mapping_code_smell() {
        let meta = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::CodeSmell,
            severity: DiagnosticSeverityLevel::Info,
            ..Default::default()
        };
        assert_eq!(meta.calculate_severity(), Severity::Hint);

        let meta = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::CodeSmell,
            severity: DiagnosticSeverityLevel::Minor,
            ..Default::default()
        };
        assert_eq!(meta.calculate_severity(), Severity::Information);

        let meta = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::CodeSmell,
            severity: DiagnosticSeverityLevel::Major,
            ..Default::default()
        };
        assert_eq!(meta.calculate_severity(), Severity::Warning);
    }

    #[test]
    fn test_severity_mapping_error() {
        let meta_blocker = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::Error,
            severity: DiagnosticSeverityLevel::Blocker,
            ..Default::default()
        };
        assert_eq!(meta_blocker.calculate_severity(), Severity::Blocker);

        let meta_critical = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::Error,
            severity: DiagnosticSeverityLevel::Critical,
            ..Default::default()
        };
        assert_eq!(meta_critical.calculate_severity(), Severity::Critical);

        let meta_major = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::Error,
            severity: DiagnosticSeverityLevel::Major,
            ..Default::default()
        };
        assert_eq!(meta_major.calculate_severity(), Severity::Major);

        let meta_minor = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::Error,
            severity: DiagnosticSeverityLevel::Minor,
            ..Default::default()
        };
        assert_eq!(meta_minor.calculate_severity(), Severity::Error);
    }

    #[test]
    fn test_severity_mapping_vulnerability() {
        let meta_blocker = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::Vulnerability,
            severity: DiagnosticSeverityLevel::Blocker,
            ..Default::default()
        };
        assert_eq!(meta_blocker.calculate_severity(), Severity::Blocker);

        let meta_critical = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::Vulnerability,
            severity: DiagnosticSeverityLevel::Critical,
            ..Default::default()
        };
        assert_eq!(meta_critical.calculate_severity(), Severity::Critical);

        let meta_major = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::Vulnerability,
            severity: DiagnosticSeverityLevel::Major,
            ..Default::default()
        };
        assert_eq!(meta_major.calculate_severity(), Severity::Major);

        let meta_minor = DiagnosticMetadata {
            diagnostic_type: DiagnosticType::Vulnerability,
            severity: DiagnosticSeverityLevel::Minor,
            ..Default::default()
        };
        assert_eq!(meta_minor.calculate_severity(), Severity::Error);
    }

    #[test]
    fn test_derive_clean_code_attribute() {
        assert_eq!(
            derive_clean_code_attribute(MetadataTag::Badpractice, DiagnosticType::Vulnerability),
            CleanCodeAttribute::Responsible
        );
        assert_eq!(
            derive_clean_code_attribute(MetadataTag::Error, DiagnosticType::SecurityHotspot),
            CleanCodeAttribute::Responsible
        );

        assert_eq!(
            derive_clean_code_attribute(MetadataTag::Standard, DiagnosticType::CodeSmell),
            CleanCodeAttribute::Consistent
        );

        assert_eq!(
            derive_clean_code_attribute(MetadataTag::Design, DiagnosticType::CodeSmell),
            CleanCodeAttribute::Adaptable
        );

        assert_eq!(
            derive_clean_code_attribute(MetadataTag::Performance, DiagnosticType::CodeSmell),
            CleanCodeAttribute::Adaptable
        );

        assert_eq!(
            derive_clean_code_attribute(MetadataTag::Localize, DiagnosticType::CodeSmell),
            CleanCodeAttribute::Responsible
        );

        assert_eq!(
            derive_clean_code_attribute(MetadataTag::Unused, DiagnosticType::CodeSmell),
            CleanCodeAttribute::Adaptable
        );

        assert_eq!(
            derive_clean_code_attribute(MetadataTag::Error, DiagnosticType::Error),
            CleanCodeAttribute::Intentional
        );
        assert_eq!(
            derive_clean_code_attribute(MetadataTag::Error, DiagnosticType::CodeSmell),
            CleanCodeAttribute::Intentional
        );
    }

    #[test]
    fn test_derive_primary_impact() {
        let impact =
            derive_primary_impact(DiagnosticType::Vulnerability, DiagnosticSeverityLevel::Critical);
        assert_eq!(impact.software_quality, SoftwareQuality::Security);
        assert_eq!(impact.severity, ImpactSeverity::High);

        let impact =
            derive_primary_impact(DiagnosticType::SecurityHotspot, DiagnosticSeverityLevel::Major);
        assert_eq!(impact.software_quality, SoftwareQuality::Security);
        assert_eq!(impact.severity, ImpactSeverity::Medium);

        let impact = derive_primary_impact(DiagnosticType::Error, DiagnosticSeverityLevel::Major);
        assert_eq!(impact.software_quality, SoftwareQuality::Reliability);
        assert_eq!(impact.severity, ImpactSeverity::Medium);

        let impact =
            derive_primary_impact(DiagnosticType::CodeSmell, DiagnosticSeverityLevel::Minor);
        assert_eq!(impact.software_quality, SoftwareQuality::Maintainability);
        assert_eq!(impact.severity, ImpactSeverity::Low);

        let impact =
            derive_primary_impact(DiagnosticType::CodeSmell, DiagnosticSeverityLevel::Blocker);
        assert_eq!(impact.severity, ImpactSeverity::High);

        let impact =
            derive_primary_impact(DiagnosticType::CodeSmell, DiagnosticSeverityLevel::Info);
        assert_eq!(impact.severity, ImpactSeverity::Low);
    }

    #[test]
    fn test_clean_code_attribute_serialization() {
        assert_eq!(
            serde_json::to_string(&CleanCodeAttribute::Consistent).unwrap(),
            "\"CONVENTIONAL\""
        );
        assert_eq!(serde_json::to_string(&CleanCodeAttribute::Intentional).unwrap(), "\"CLEAR\"");
        assert_eq!(serde_json::to_string(&CleanCodeAttribute::Adaptable).unwrap(), "\"FOCUSED\"");
        assert_eq!(
            serde_json::to_string(&CleanCodeAttribute::Responsible).unwrap(),
            "\"TRUSTWORTHY\""
        );
    }

    #[test]
    fn test_software_quality_serialization() {
        assert_eq!(
            serde_json::to_string(&SoftwareQuality::Maintainability).unwrap(),
            "\"MAINTAINABILITY\""
        );
        assert_eq!(
            serde_json::to_string(&SoftwareQuality::Reliability).unwrap(),
            "\"RELIABILITY\""
        );
        assert_eq!(serde_json::to_string(&SoftwareQuality::Security).unwrap(), "\"SECURITY\"");
    }

    #[test]
    fn test_impact_serialization() {
        let impact = Impact::new(SoftwareQuality::Maintainability, ImpactSeverity::Medium);
        let json = serde_json::to_string(&impact).unwrap();
        assert_eq!(json, r#"{"softwareQuality":"MAINTAINABILITY","severity":"MEDIUM"}"#);
    }
}
