use crate::handlers;
use crate::handlers::deprecated_platform_api::ExcludeProblem;
use crate::metadata::{
    parse_severity, parse_severity_opt, DiagnosticSeverityLevel, DiagnosticType, MetadataTag,
};
use crate::{DiagnosticCode, Severity};
use base_db::{DiagnosticsConfigInput, Locale};
use std::collections::HashMap;
use std::path::Path;
use stdx::case::CaseExt;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataOverride {
    pub severity: Option<DiagnosticSeverityLevel>,
    // Project configs spell keys in camelCase while the interned form (`to_input`) is
    // snake_case JSON; aliases accept both without changing what is stored.
    #[serde(alias = "diagnosticType")]
    pub diagnostic_type: Option<DiagnosticType>,
    pub tags: Option<Vec<MetadataTag>>,
    #[serde(alias = "lspSeverity")]
    pub lsp_severity: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EffectiveMetadata {
    base: &'static crate::metadata::DiagnosticMetadata,
    tags_override: Option<Vec<MetadataTag>>,
    lsp_severity_override: Option<String>,
}

impl EffectiveMetadata {
    pub fn severity_value(&self) -> Severity {
        if let Some(override_str) = &self.lsp_severity_override {
            return parse_severity(override_str);
        }
        self.base.effective_severity()
    }

    pub fn tags(&self) -> Vec<MetadataTag> {
        self.tags_override.clone().unwrap_or_else(|| self.base.tags.to_vec())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticsConfig {
    pub disabled: Vec<DiagnosticCode>,
    pub enabled: Vec<DiagnosticCode>,
    pub parameters: HashMap<DiagnosticCode, serde_json::Value>,
    pub ordinary_app_support: bool,
    pub dataflow_max_iterations: usize,
    pub metadata_overrides: HashMap<DiagnosticCode, MetadataOverride>,
    pub only_enabled: Option<Vec<DiagnosticCode>>,
    pub locale: Locale,
    /// Recognise bsl-language-server suppression directives (`// BSLLS:Код-off` …) as aliases of
    /// the native ones, so migrating a project does not require rewriting suppression comments.
    /// On by default; set `bsllsSuppressionCompat = false` in the project config to disable.
    pub bslls_suppression_compat: bool,
    /// Restricts diagnostics to files/lines changed relative to a reference state
    /// (vendor-diff filter). Set programmatically by the driving surface, never
    /// from the `[diagnostics]` project config.
    pub scope: Option<std::sync::Arc<base_db::AnalysisScope>>,
    /// Whether findings carry their quick fixes. On by default; a surface that never
    /// offers them (the batch `analyze`) turns it off, so that no fix is built - some
    /// prepare facts of the whole module for it. Set programmatically by the driving
    /// surface, never from the `[diagnostics]` project config.
    pub quick_fixes: bool,
    /// Keep the findings on lines outside `scope` in the computed set, leaving the line
    /// filter to the caller. A baseline fingerprint numbers repeated findings within the
    /// file, so a caller that classifies against a baseline needs the file's whole set:
    /// classifying only the scoped lines would let a new finding inherit the number of a
    /// recorded one. The file gate still applies. Set programmatically by the driving
    /// surface for one computation, never from the `[diagnostics]` project config.
    pub defer_scope_line_gate: bool,
}

/// `serde_json::Value` is only `PartialEq` (floats); a configuration is still
/// a value with a total identity for memoisation.
impl Eq for DiagnosticsConfig {}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            disabled: Vec::new(),
            enabled: Vec::new(),
            parameters: HashMap::new(),
            ordinary_app_support: false,
            dataflow_max_iterations: hir::dataflow::DEFAULT_MAX_ITERATIONS,
            metadata_overrides: HashMap::new(),
            only_enabled: None,
            locale: Locale::default(),
            bslls_suppression_compat: true,
            scope: None,
            quick_fixes: true,
            defer_scope_line_gate: false,
        }
    }
}

impl DiagnosticsConfig {
    /// Build the effective diagnostics config from the raw `[diagnostics]` value that
    /// `project-model` loads from `bsl-analyzer.toml` / `.bsl-analyzer.json` /
    /// `.bsl-language-server.json`, then stamp the resolved `locale`.
    ///
    /// This is the single source of truth shared by every runtime mode (LSP, CLI,
    /// MCP), so a project's settings apply identically regardless of how the analyzer
    /// is driven. A malformed config logs a warning and falls back to defaults rather
    /// than failing the analysis.
    pub fn from_project_json(diagnostics: &serde_json::Value, locale: Locale) -> Self {
        let mut config: Self = if diagnostics.is_null() {
            Self::default()
        } else {
            serde_json::from_value(diagnostics.clone()).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to deserialize project diagnostics config; using defaults");
                Self::default()
            })
        };
        config.locale = locale;
        config
    }

    /// [`Self::from_project_json`] for the loaders that know which file the
    /// `[diagnostics]` section came from: what the section cannot apply is reported
    /// (see [`Self::warn_unknown_parameters`]) instead of being dropped without a
    /// trace. The effective config is the same.
    ///
    /// Not used by per-request readers (a `query` call re-reads the section each
    /// time): they would repeat the warning on every call.
    pub fn from_project_file(
        diagnostics: &serde_json::Value,
        locale: Locale,
        source: Option<&Path>,
    ) -> Self {
        Self::warn_unknown_parameters(diagnostics, source);
        Self::from_project_json(diagnostics, locale)
    }

    /// Reports parameter entries that the analyzer cannot apply, for loaders that
    /// parse the section themselves: unknown codes, known codes with an unsupported
    /// outer value, and invalid names in `DeprecatedPlatformApi.exclude`.
    pub fn warn_unknown_parameters(diagnostics: &serde_json::Value, source: Option<&Path>) {
        let file =
            source.map_or_else(|| "<project config>".to_owned(), |p| p.display().to_string());
        if let Some(parameters) =
            diagnostics.get("parameters").and_then(serde_json::Value::as_object)
        {
            for (name, value) in parameters {
                if name.parse::<DiagnosticCode>().is_ok()
                    && !matches!(value, serde_json::Value::Bool(_) | serde_json::Value::Object(_))
                {
                    tracing::warn!(
                        key = name.as_str(),
                        config_file = %file,
                        "invalid value for diagnostic code `{name}` in [diagnostics.parameters] \
                         of {file}: expected a bool or table; the value is ignored and the rule \
                         keeps its default; use `[diagnostics.parameters.{name}]` for rule parameters"
                    );
                }
            }
        }
        for name in unknown_parameter_codes(diagnostics) {
            tracing::warn!(
                key = name,
                config_file = %file,
                "unknown diagnostic code `{name}` in [diagnostics.parameters] of {file}: \
                 the key is ignored and the rule keeps its default; {}",
                unknown_code_hint(name)
            );
        }
        warn_metadata_overrides(diagnostics, &file);
        if let Some(sections) = diagnostics.as_object() {
            for name in
                sections.keys().filter(|name| !KNOWN_DIAGNOSTICS_KEYS.contains(&name.as_str()))
            {
                tracing::warn!(
                    key = name.as_str(),
                    config_file = %file,
                    "unknown key `{name}` in [diagnostics] of {file}: the section is ignored; \
                     the supported keys are {}",
                    KNOWN_DIAGNOSTICS_KEYS.join(", ")
                );
            }
        }
        for problem in handlers::deprecated_platform_api::unknown_excluded_names(diagnostics) {
            match problem {
                ExcludeProblem::UnknownName(name) => tracing::warn!(
                    key = "DeprecatedPlatformApi.exclude",
                    config_file = %file,
                    "unknown deprecated platform feature `{name}` in \
                     [diagnostics.parameters.DeprecatedPlatformApi].exclude of {file}: \
                     the name is ignored, every other name still applies; use the name of \
                     a deprecated platform feature as the diagnostic message quotes it, \
                     without the owner type; an enum-name finding whose quoted name is not \
                     such a feature is silenced by the enum name before the dot"
                ),
                ExcludeProblem::NotAnArray => tracing::warn!(
                    key = "DeprecatedPlatformApi.exclude",
                    config_file = %file,
                    "[diagnostics.parameters.DeprecatedPlatformApi].exclude of {file} \
                     must be an array of names; the value is ignored"
                ),
            }
        }
    }

    pub fn all_enabled() -> Self {
        let mut enabled = Vec::new();
        for code in [
            DiagnosticCode::BadWords,
            DiagnosticCode::CodeAfterAsyncCall,
            DiagnosticCode::DenyIncompleteValues,
            DiagnosticCode::FileSystemAccess,
            DiagnosticCode::FunctionNameStartsWithGet,
            DiagnosticCode::FunctionOutParameter,
            DiagnosticCode::InternetAccess,
            DiagnosticCode::MissingTempStorageDeletion,
            DiagnosticCode::TernaryOperatorUsage,
            DiagnosticCode::TooManyReturns,
            DiagnosticCode::TypeMismatchByDocComment,
            DiagnosticCode::UnresolvedName,
            DiagnosticCode::UseSystemInformation,
            DiagnosticCode::UsingLikeInQuery,
        ] {
            if let Some(meta) = handlers::get_metadata(code) {
                if !meta.activated_by_default {
                    enabled.push(code);
                }
            }
        }

        Self {
            disabled: Vec::new(),
            enabled,
            parameters: HashMap::new(),
            ordinary_app_support: false,
            dataflow_max_iterations: hir::dataflow::DEFAULT_MAX_ITERATIONS,
            metadata_overrides: HashMap::new(),
            only_enabled: None,
            locale: Locale::default(),
            bslls_suppression_compat: true,
            scope: None,
            quick_fixes: true,
            defer_scope_line_gate: false,
        }
    }

    /// The severity a finding of `code` carries under this configuration.
    ///
    /// Lives on the config, not only on `DiagnosticsContext`, because a diagnostic can be
    /// produced without a file to anchor it — validating a bare query text has a
    /// configuration but no `FileId`.
    pub fn severity(&self, code: DiagnosticCode) -> Severity {
        self.get_effective_metadata(code).map(|m| m.severity_value()).unwrap_or(Severity::Warning)
    }

    /// The LSP tags a finding of `code` carries under this configuration. File-free for the
    /// same reason as [`Self::severity`].
    pub fn tags(&self, code: DiagnosticCode) -> Vec<crate::DiagnosticTag> {
        self.get_effective_metadata(code)
            .map(|m| {
                m.tags()
                    .iter()
                    .filter_map(|tag| match tag {
                        MetadataTag::Unused => Some(crate::DiagnosticTag::Unnecessary),
                        MetadataTag::Deprecated => Some(crate::DiagnosticTag::Deprecated),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_effective_metadata(&self, code: DiagnosticCode) -> Option<EffectiveMetadata> {
        let base = handlers::get_metadata(code)?;
        let override_data = self.metadata_overrides.get(&code);

        Some(EffectiveMetadata {
            base,
            tags_override: override_data.and_then(|o| o.tags.clone()),
            lsp_severity_override: override_data.and_then(|o| o.lsp_severity.clone()),
        })
    }
}

/// Top-level keys of the `[diagnostics]` value that reaches [`DiagnosticsConfig`].
/// `baseline` is absent on purpose: `project-model` keeps it in its own field and
/// strips it from the rules JSON, so it never arrives here.
const KNOWN_DIAGNOSTICS_KEYS: &[&str] = &[
    "parameters",
    "metadataOverrides",
    "ordinaryAppSupport",
    "dataflowMaxIterations",
    "bsllsSuppressionCompat",
];

/// Hint text for the unknown-severity warning, not the criterion: `parse_severity_opt`
/// decides, so a stale entry here costs a slightly wrong hint, never a wrong verdict.
const ACCEPTED_SEVERITY_NAMES: &[&str] =
    &["error", "warning", "information", "info", "hint", "blocker", "critical", "major"];

/// Hint text for an unrecognised `diagnosticType`. The parser accepts any case and
/// an underscore (`CODE_SMELL`); a stale entry here only wrongs the hint.
const ACCEPTED_DIAGNOSTIC_TYPES: &[&str] =
    &["Error", "CodeSmell", "Vulnerability", "SecurityHotspot"];

const ACCEPTED_SEVERITY_LEVELS: &[&str] = &["Info", "Minor", "Major", "Critical", "Blocker"];

fn compact_name(s: &str) -> String {
    s.fold_lower().chars().filter(|c| *c != '_').collect()
}

fn parse_diagnostic_type(s: &str) -> Option<DiagnosticType> {
    match compact_name(s).as_str() {
        "error" => Some(DiagnosticType::Error),
        "codesmell" => Some(DiagnosticType::CodeSmell),
        "vulnerability" => Some(DiagnosticType::Vulnerability),
        "securityhotspot" => Some(DiagnosticType::SecurityHotspot),
        _ => None,
    }
}

fn parse_severity_level(s: &str) -> Option<DiagnosticSeverityLevel> {
    match compact_name(s).as_str() {
        "info" => Some(DiagnosticSeverityLevel::Info),
        "minor" => Some(DiagnosticSeverityLevel::Minor),
        "major" => Some(DiagnosticSeverityLevel::Major),
        "critical" => Some(DiagnosticSeverityLevel::Critical),
        "blocker" => Some(DiagnosticSeverityLevel::Blocker),
        _ => None,
    }
}

fn parse_metadata_tag(s: &str) -> Option<MetadataTag> {
    match compact_name(s).as_str() {
        "standard" => Some(MetadataTag::Standard),
        "lockinos" => Some(MetadataTag::Lockinos),
        "sql" => Some(MetadataTag::Sql),
        "performance" => Some(MetadataTag::Performance),
        "brainoverload" => Some(MetadataTag::Brainoverload),
        "badpractice" => Some(MetadataTag::Badpractice),
        "clumsy" => Some(MetadataTag::Clumsy),
        "design" => Some(MetadataTag::Design),
        "suspicious" => Some(MetadataTag::Suspicious),
        "unpredictable" => Some(MetadataTag::Unpredictable),
        "deprecated" => Some(MetadataTag::Deprecated),
        "unused" => Some(MetadataTag::Unused),
        "error" => Some(MetadataTag::Error),
        "localize" => Some(MetadataTag::Localize),
        _ => None,
    }
}

fn known_override_field(name: &str) -> bool {
    matches!(
        name,
        "severity" | "diagnosticType" | "diagnostic_type" | "tags" | "lspSeverity" | "lsp_severity"
    )
}

enum OverrideNote {
    NotATable,
    UnknownField(String),
    AliasClash(&'static str, &'static str),
    InvalidField {
        field: &'static str,
        detail: String,
    },
    UnknownSeverity(String),
    /// Stored for the round trip, but no finding reads it. `lspSeverity` is what changes
    /// the reported grade; `tags` is what changes the tags.
    InertField(&'static str),
}

struct ParsedOverride {
    value: Option<MetadataOverride>,
    notes: Vec<OverrideNote>,
}

fn metadata_overrides_from_section(
    value: &serde_json::Value,
) -> HashMap<DiagnosticCode, MetadataOverride> {
    let Some(entries) = value.as_object() else {
        return HashMap::new();
    };
    entries
        .iter()
        .filter_map(|(name, entry)| {
            let code = name.parse().ok()?;
            parsed_override(entry).value.map(|over| (code, over))
        })
        .collect()
}

fn parsed_override(entry: &serde_json::Value) -> ParsedOverride {
    let Some(object) = entry.as_object() else {
        return ParsedOverride { value: None, notes: vec![OverrideNote::NotATable] };
    };
    let mut notes = Vec::new();
    for key in object.keys() {
        if !known_override_field(key) {
            notes.push(OverrideNote::UnknownField(key.clone()));
        }
    }
    let mut over = MetadataOverride::default();
    let mut applied = false;

    if let Some(text) = take_string_alias(object, "lspSeverity", "lsp_severity", &mut notes) {
        if parse_severity_opt(&text).is_none() {
            notes.push(OverrideNote::UnknownSeverity(text.clone()));
        }
        over.lsp_severity = Some(text);
        applied = true;
    }
    if let Some(text) = take_string_alias(object, "diagnosticType", "diagnostic_type", &mut notes) {
        match parse_diagnostic_type(&text) {
            Some(ty) => {
                over.diagnostic_type = Some(ty);
                applied = true;
                notes.push(OverrideNote::InertField("diagnosticType"));
            }
            None => notes.push(OverrideNote::InvalidField {
                field: "diagnosticType",
                detail: format!(
                    "unknown name `{text}`; the accepted names are {}",
                    ACCEPTED_DIAGNOSTIC_TYPES.join(", ")
                ),
            }),
        }
    }
    if let Some(value) = object.get("severity") {
        match value.as_str() {
            Some(text) => match parse_severity_level(text) {
                Some(level) => {
                    over.severity = Some(level);
                    applied = true;
                    notes.push(OverrideNote::InertField("severity"));
                }
                None => notes.push(OverrideNote::InvalidField {
                    field: "severity",
                    detail: format!(
                        "unknown name `{text}`; the accepted names are {}",
                        ACCEPTED_SEVERITY_LEVELS.join(", ")
                    ),
                }),
            },
            None => notes.push(OverrideNote::InvalidField {
                field: "severity",
                detail: "expected a string".to_owned(),
            }),
        }
    }
    if let Some(value) = object.get("tags") {
        match value.as_array() {
            Some(items) => {
                let mut tags = Vec::new();
                let mut bad = false;
                for item in items {
                    match item.as_str().and_then(parse_metadata_tag) {
                        Some(tag) => tags.push(tag),
                        None => bad = true,
                    }
                }
                if bad {
                    // Tags replace the rule's tags wholesale, so a mixed list would
                    // drop the ones the author did not repeat. Ignore the field.
                    notes.push(OverrideNote::InvalidField {
                        field: "tags",
                        detail: "expected names of metadata tags".to_owned(),
                    });
                } else {
                    over.tags = Some(tags);
                    applied = true;
                }
            }
            None => notes.push(OverrideNote::InvalidField {
                field: "tags",
                detail: "expected an array of names".to_owned(),
            }),
        }
    }

    ParsedOverride { value: applied.then_some(over), notes }
}

/// camelCase wins when both spellings are strings and differ: that is the project-file
/// form. A non-string does not win — the other spelling is used when it is a string,
/// and the warning names the key that was actually written.
fn take_string_alias(
    object: &serde_json::Map<String, serde_json::Value>,
    camel: &'static str,
    snake: &'static str,
    notes: &mut Vec<OverrideNote>,
) -> Option<String> {
    let camel_str = object.get(camel).map(serde_json::Value::as_str);
    let snake_str = object.get(snake).map(serde_json::Value::as_str);
    match (camel_str, snake_str) {
        (None, None) => None,
        (Some(None), None) => {
            notes.push(expected_string(camel));
            None
        }
        (None, Some(None)) => {
            notes.push(expected_string(snake));
            None
        }
        (Some(None), Some(None)) => {
            notes.push(expected_string(camel));
            notes.push(expected_string(snake));
            None
        }
        (Some(None), Some(Some(text))) => {
            notes.push(expected_string(camel));
            Some(text.to_owned())
        }
        (Some(Some(text)), Some(None)) => {
            notes.push(expected_string(snake));
            Some(text.to_owned())
        }
        (Some(Some(camel_text)), Some(Some(snake_text))) => {
            if camel_text != snake_text {
                notes.push(OverrideNote::AliasClash(camel, snake));
            }
            Some(camel_text.to_owned())
        }
        (Some(Some(text)), None) | (None, Some(Some(text))) => Some(text.to_owned()),
    }
}

fn expected_string(field: &'static str) -> OverrideNote {
    OverrideNote::InvalidField { field, detail: "expected a string".to_owned() }
}

fn warn_metadata_overrides(diagnostics: &serde_json::Value, file: &str) {
    let Some(section) = diagnostics.get("metadataOverrides") else {
        return;
    };
    let Some(entries) = section.as_object() else {
        tracing::warn!(
            key = "metadataOverrides",
            config_file = %file,
            "[diagnostics.metadataOverrides] of {file} is not a table: the section is ignored \
             and the other settings still apply"
        );
        return;
    };
    for (name, entry) in entries {
        if name.parse::<DiagnosticCode>().is_err() {
            tracing::warn!(
                key = name.as_str(),
                config_file = %file,
                "unknown diagnostic code `{name}` in [diagnostics.metadataOverrides] of \
                 {file}: the override is ignored and the rule keeps its default severity; \
                 {}",
                unknown_code_hint(name)
            );
            continue;
        }
        for note in parsed_override(entry).notes {
            warn_override_note(name, note, file);
        }
    }
}

fn warn_override_note(name: &str, note: OverrideNote, file: &str) {
    match note {
        OverrideNote::NotATable => tracing::warn!(
            key = name,
            config_file = %file,
            "invalid metadata override for `{name}` in [diagnostics.metadataOverrides] of \
             {file}: expected a table; the override is ignored and the rule keeps its default \
             severity"
        ),
        OverrideNote::UnknownField(field) => tracing::warn!(
            key = field.as_str(),
            config_file = %file,
            "unknown field `{field}` in [diagnostics.metadataOverrides.{name}] of {file}: \
             the field is ignored"
        ),
        OverrideNote::AliasClash(camel, snake) => tracing::warn!(
            key = name,
            config_file = %file,
            "both `{camel}` and `{snake}` are set for `{name}` in \
             [diagnostics.metadataOverrides] of {file}: the values differ, the camelCase \
             value is used"
        ),
        OverrideNote::InvalidField { field, detail } => tracing::warn!(
            key = name,
            config_file = %file,
            "invalid `{field}` for `{name}` in [diagnostics.metadataOverrides] of {file}: \
             {detail}; the field is ignored and the other settings still apply"
        ),
        OverrideNote::UnknownSeverity(value) => tracing::warn!(
            key = name,
            config_file = %file,
            "unknown lspSeverity `{value}` for `{name}` in \
             [diagnostics.metadataOverrides] of {file}: the override falls \
             back to `warning`; the accepted names are {}",
            ACCEPTED_SEVERITY_NAMES.join(", ")
        ),
        OverrideNote::InertField(field) => tracing::warn!(
            key = name,
            config_file = %file,
            "`{field}` for `{name}` in [diagnostics.metadataOverrides] of {file} is stored \
             but does not change the finding; set `lspSeverity` to change the reported severity"
        ),
    }
}

/// Codes this analyzer used to have, with the code that took over their checks.
/// All of them were merged into `DeprecatedPlatformApi` in v0.2.50.
pub(crate) const REMOVED_CODES: &[(&str, &str, &str)] = &[
    ("DeprecatedCurrentDate", "DeprecatedPlatformApi", "v0.2.50"),
    ("DeprecatedFind", "DeprecatedPlatformApi", "v0.2.50"),
    ("DeprecatedMessage", "DeprecatedPlatformApi", "v0.2.50"),
    ("DeprecatedTypeManagedForm", "DeprecatedPlatformApi", "v0.2.50"),
    ("DeprecatedMethods8310", "DeprecatedPlatformApi", "v0.2.50"),
    ("DeprecatedMethods8317", "DeprecatedPlatformApi", "v0.2.50"),
    ("DeprecatedAttributes8312", "DeprecatedPlatformApi", "v0.2.50"),
];

/// The names under `[diagnostics.parameters]` that are not a [`DiagnosticCode`].
/// The deserializer skips them, so a typo or a code removed in a newer release
/// leaves the rule on while the author believes it is off.
pub fn unknown_parameter_codes(diagnostics: &serde_json::Value) -> Vec<&str> {
    diagnostics
        .get("parameters")
        .and_then(|parameters| parameters.as_object())
        .map(|parameters| {
            parameters
                .keys()
                .map(String::as_str)
                .filter(|name| name.parse::<DiagnosticCode>().is_err())
                .collect()
        })
        .unwrap_or_default()
}

/// Why a name is not a code, worded for the person who wrote it. A removed code
/// names its successor but is deliberately not mapped to it: the successor covers
/// a whole group of checks, so switching it off would silence more than the
/// removed key ever did.
/// A stale `Deprecated…` key also gets a ready `exclude` entry when the registry
/// can name its features.
fn unknown_code_hint(name: &str) -> String {
    let reason = match REMOVED_CODES.iter().find(|(removed, _, _)| *removed == name) {
        Some((_, successor, release)) => format!(
            "the code was removed in {release}; its check is now part of `{successor}` \
             (not mapped automatically: `{successor} = false` would switch off every \
             deprecated-API finding, not only this one)"
        ),
        None => "no diagnostic has this name (typo, or a code of another analyzer)".to_owned(),
    };
    match handlers::deprecated_platform_api::exclude_suggestion(name) {
        Some(suggestion) => format!("{reason}; {suggestion}"),
        None => reason,
    }
}

impl<'de> serde::Deserialize<'de> for DiagnosticsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct DiagnosticsConfigVisitor;

        impl<'de> Visitor<'de> for DiagnosticsConfigVisitor {
            type Value = DiagnosticsConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a diagnostics configuration object")
            }

            fn visit_map<M>(self, mut map: M) -> Result<DiagnosticsConfig, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut disabled = Vec::new();
                let mut enabled = Vec::new();
                let mut parameters = HashMap::new();
                let mut ordinary_app_support = false;
                let mut dataflow_max_iterations = hir::dataflow::DEFAULT_MAX_ITERATIONS;
                let mut bslls_suppression_compat = true;
                let mut metadata_overrides = HashMap::new();

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "ordinaryAppSupport" => {
                            ordinary_app_support = map.next_value()?;
                        }
                        "dataflowMaxIterations" => {
                            dataflow_max_iterations = map.next_value()?;
                        }
                        "bsllsSuppressionCompat" => {
                            bslls_suppression_compat = map.next_value()?;
                        }
                        "parameters" => {
                            let params: HashMap<String, serde_json::Value> = map.next_value()?;
                            for (code_str, value) in params {
                                if let Ok(code) = code_str.parse::<DiagnosticCode>() {
                                    match &value {
                                        serde_json::Value::Bool(false) => {
                                            disabled.push(code);
                                        }
                                        serde_json::Value::Bool(true) => {
                                            enabled.push(code);
                                        }
                                        serde_json::Value::Object(_) => {
                                            enabled.push(code);
                                            parameters.insert(code, value);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        "metadataOverrides" => {
                            // One bad entry must not fail the visitor: that would drop every
                            // other setting in the section. Entries are parsed on their own.
                            let value: serde_json::Value = map.next_value()?;
                            metadata_overrides = metadata_overrides_from_section(&value);
                        }
                        _ => {
                            let _: serde_json::Value = map.next_value()?;
                        }
                    }
                }

                Ok(DiagnosticsConfig {
                    disabled,
                    enabled,
                    parameters,
                    ordinary_app_support,
                    dataflow_max_iterations,
                    metadata_overrides,
                    only_enabled: None,
                    locale: Locale::default(),
                    bslls_suppression_compat,
                    scope: None,
                    quick_fixes: true,
                    defer_scope_line_gate: false,
                })
            }
        }

        deserializer.deserialize_map(DiagnosticsConfigVisitor)
    }
}

impl DiagnosticsConfig {
    #[inline]
    pub fn any_enabled(&self, codes: &[DiagnosticCode]) -> bool {
        if let Some(ref only) = self.only_enabled {
            return codes.iter().any(|code| only.contains(code));
        }

        codes.iter().any(|code| !self.is_disabled(*code))
    }

    pub fn is_disabled(&self, code: DiagnosticCode) -> bool {
        if let Some(ref only) = self.only_enabled {
            return !only.contains(&code);
        }

        if self.disabled.contains(&code) {
            return true;
        }

        if let Some(metadata) = handlers::get_metadata(code) {
            if !metadata.activated_by_default
                && !self.enabled.contains(&code)
                && !self.parameters.contains_key(&code)
            {
                return true;
            }
        }

        false
    }

    pub fn get_bool(&self, code: DiagnosticCode, param: &str) -> Option<bool> {
        self.parameters.get(&code).and_then(|v| v.get(param)).and_then(|v| v.as_bool())
    }

    pub fn get_int(&self, code: DiagnosticCode, param: &str) -> Option<i64> {
        self.parameters.get(&code).and_then(|v| v.get(param)).and_then(|v| v.as_i64())
    }

    pub fn get_string(&self, code: DiagnosticCode, param: &str) -> Option<&str> {
        self.parameters.get(&code).and_then(|v| v.get(param)).and_then(|v| v.as_str())
    }

    pub fn get_string_param(&self, code: DiagnosticCode, param: &str) -> Option<String> {
        self.parameters
            .get(&code)
            .and_then(|v| v.get(param))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    pub fn get_string_array(&self, code: DiagnosticCode, param: &str) -> Option<Vec<String>> {
        self.parameters
            .get(&code)
            .and_then(|v| v.get(param))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
    }

    pub fn from_input(input: &DiagnosticsConfigInput) -> Self {
        let disabled: Vec<DiagnosticCode> =
            input.disabled.iter().filter_map(|s| s.parse().ok()).collect();

        let enabled: Vec<DiagnosticCode> =
            input.enabled.iter().filter_map(|s| s.parse().ok()).collect();

        let parameters: HashMap<DiagnosticCode, serde_json::Value> = input
            .parameters
            .iter()
            .filter_map(|(code_str, json_str)| {
                let code: DiagnosticCode = code_str.parse().ok()?;
                let value: serde_json::Value = serde_json::from_str(json_str).ok()?;
                Some((code, value))
            })
            .collect();

        Self {
            disabled,
            enabled,
            parameters,
            ordinary_app_support: input.ordinary_app_support,
            dataflow_max_iterations: input.dataflow_max_iterations,
            metadata_overrides: input
                .metadata_overrides
                .iter()
                .filter_map(|(code_str, json_str)| {
                    let code: DiagnosticCode = code_str.parse().ok()?;
                    let value: MetadataOverride = serde_json::from_str(json_str).ok()?;
                    Some((code, value))
                })
                .collect(),
            only_enabled: input
                .only_enabled
                .as_ref()
                .map(|codes| codes.iter().filter_map(|s| s.parse().ok()).collect()),
            locale: input.locale,
            bslls_suppression_compat: input.bslls_suppression_compat,
            scope: input.scope.clone(),
            quick_fixes: input.quick_fixes,
            defer_scope_line_gate: input.defer_scope_line_gate,
        }
    }

    /// The interned form of this configuration: the key the memoised
    /// diagnostics are stored under. `from_input` restores every field that
    /// influences a diagnostic, so a configuration and its input agree on the
    /// result (`config_survives_the_interned_round_trip`).
    pub fn to_input(&self) -> DiagnosticsConfigInput {
        DiagnosticsConfigInput::from_raw(
            self.disabled.iter().map(|c| c.as_str().to_owned()),
            self.enabled.iter().map(|c| c.as_str().to_owned()),
            self.parameters.iter().map(|(c, v)| (c.as_str().to_owned(), v.to_string())),
            self.ordinary_app_support,
            self.dataflow_max_iterations,
            self.locale,
            self.bslls_suppression_compat,
        )
        .with_filters(
            self.only_enabled.as_ref().map(|codes| codes.iter().map(|c| c.as_str().to_owned())),
            self.metadata_overrides.iter().map(|(c, o)| {
                (c.as_str().to_owned(), serde_json::to_string(o).expect("plain enums serialize"))
            }),
        )
        .with_scope(self.scope.clone())
        .with_quick_fixes(self.quick_fixes)
        .with_defer_scope_line_gate(self.defer_scope_line_gate)
    }

    /// Fails without touching `self` when any name is not a diagnostic code: a
    /// silently ignored `--only-diagnostic` name would widen the run to every code.
    pub fn apply_cli_filters(
        &mut self,
        only_diagnostic: &[String],
        disable_diagnostic: &[String],
    ) -> Result<(), String> {
        let mut problems = Vec::new();
        for (flag, names) in
            [("--only-diagnostic", only_diagnostic), ("--disable-diagnostic", disable_diagnostic)]
        {
            for name in names.iter().filter(|name| name.parse::<DiagnosticCode>().is_err()) {
                problems.push(format!(
                    "unknown diagnostic code `{name}` in {flag}: {}",
                    unknown_code_hint(name)
                ));
            }
        }
        if !problems.is_empty() {
            return Err(problems.join("\n"));
        }

        if !only_diagnostic.is_empty() {
            self.only_enabled =
                Some(only_diagnostic.iter().filter_map(|s| s.parse().ok()).collect());
        }

        for code_str in disable_diagnostic {
            if let Ok(code) = code_str.parse::<DiagnosticCode>() {
                if !self.disabled.contains(&code) {
                    self.disabled.push(code);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tracing::{span, Event, Id, Metadata, Subscriber};

    struct WarningCounter(Arc<AtomicUsize>);

    impl Subscriber for WarningCounter {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &span::Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _: &Id, _: &span::Record<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
    }

    /// Collects every WARN event as one line: the message and its structured fields.
    struct WarningTexts(Arc<std::sync::Mutex<Vec<String>>>);

    struct FieldText(String);

    impl tracing::field::Visit for FieldText {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
            } else {
                self.0.push_str(&format!(" [{}={value:?}]", field.name()));
            }
        }
    }

    impl Subscriber for WarningTexts {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &span::Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _: &Id, _: &span::Record<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                let mut text = FieldText(String::new());
                event.record(&mut text);
                self.0.lock().unwrap().push(text.0);
            }
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
    }

    fn warnings_of(run: impl FnOnce()) -> Vec<String> {
        let texts = Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(WarningTexts(texts.clone()), run);
        let texts = texts.lock().unwrap().clone();
        texts
    }

    /// The shape of a real project config: rules switched off by name, among them
    /// two that this analyzer no longer has (one removed in v0.2.50, one it never had).
    fn project_with_stale_codes() -> serde_json::Value {
        json!({
            "parameters": {
                "LineLength": false,
                "DeprecatedCurrentDate": false,
                "DeprecatedErrorProcessing": false,
                "MethodSize": { "maxMethodSize": 80 },
            }
        })
    }

    #[test]
    fn an_unknown_code_in_parameters_is_reported_with_key_and_file() {
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let mut config = None;
        let warnings = warnings_of(|| {
            config = Some(DiagnosticsConfig::from_project_file(
                &project_with_stale_codes(),
                Locale::En,
                Some(file),
            ));
        });

        assert_eq!(warnings.len(), 2, "one warning per unknown key: {warnings:#?}");
        let removed = warnings.iter().find(|w| w.contains("`DeprecatedCurrentDate`")).unwrap();
        assert!(removed.contains("bsl-analyzer.toml"), "names the file: {removed}");
        assert!(removed.contains("[diagnostics.parameters]"), "names the section: {removed}");
        assert!(removed.contains("removed in v0.2.50"), "{removed}");
        assert!(removed.contains("`DeprecatedPlatformApi`"), "names the successor: {removed}");
        assert!(removed.contains(r#"exclude = ["ТекущаяДата"]"#), "ready entry: {removed}");
        let never_existed =
            warnings.iter().find(|w| w.contains("`DeprecatedErrorProcessing`")).unwrap();
        assert!(never_existed.contains("no diagnostic has this name"), "{never_existed}");
        assert!(
            never_existed.contains(
                r#"exclude = ["КраткоеПредставлениеОшибки", "ПодробноеПредставлениеОшибки", "ПоказатьИнформациюОбОшибке"]"#
            ),
            "{never_existed}"
        );

        // Behaviour is unchanged: the stale keys are still ignored and are NOT
        // mapped to the successor, which would silence the whole group.
        let config = config.unwrap();
        assert!(config.is_disabled(DiagnosticCode::LineLength));
        assert!(!config.is_disabled(DiagnosticCode::DeprecatedPlatformApi));
        assert_eq!(config.get_int(DiagnosticCode::MethodSize, "maxMethodSize"), Some(80));
        assert_eq!(config.disabled, vec![DiagnosticCode::LineLength]);
        assert!(
            !config.parameters.contains_key(&DiagnosticCode::DeprecatedPlatformApi),
            "a stale key adds no `exclude`: {:?}",
            config.parameters
        );
    }

    #[test]
    fn a_parameter_table_with_an_unknown_name_is_reported_too() {
        let raw = json!({ "parameters": { "LineLenght": { "maxLineLength": 150 } } });
        let warnings = warnings_of(|| {
            DiagnosticsConfig::from_project_file(&raw, Locale::En, None);
        });
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].contains("`LineLenght`"), "{}", warnings[0]);
    }

    #[test]
    fn known_codes_and_a_missing_section_stay_silent() {
        let raw =
            json!({ "parameters": { "LineLength": false, "MethodSize": { "maxMethodSize": 1 } } });
        let warnings = warnings_of(|| {
            DiagnosticsConfig::from_project_file(&raw, Locale::En, None);
            DiagnosticsConfig::from_project_file(&serde_json::Value::Null, Locale::En, None);
            DiagnosticsConfig::from_project_file(&json!({}), Locale::En, None);
        });
        assert!(warnings.is_empty(), "{warnings:#?}");
    }

    #[test]
    fn invalid_outer_parameter_shapes_warn_once_and_valid_shapes_stay_silent() {
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let raw = json!({
            "parameters": {
                "LineLength": 120,
                "MethodSize": "line_length_marker",
                "Typo": ["array_marker"],
                "BadWords": null,
                "FunctionNameStartsWithGet": true,
                "CommentedCode": false,
                "CognitiveComplexity": {},
                "NumberOfParams": { "maxParamsCount": 9 },
            }
        });

        let warnings = warnings_of(|| {
            DiagnosticsConfig::from_project_file(&raw, Locale::En, Some(file));
        });

        assert_eq!(warnings.len(), 4, "one warning per invalid known key: {warnings:#?}");
        for code in ["LineLength", "MethodSize", "Typo", "BadWords"] {
            let matching: Vec<_> =
                warnings.iter().filter(|warning| warning.contains(code)).collect();
            assert_eq!(matching.len(), 1, "one warning for {code}: {warnings:#?}");
            let warning = matching[0];
            assert!(warning.contains("[diagnostics.parameters]"), "{warning}");
            assert!(warning.contains("bsl-analyzer.toml"), "{warning}");
            assert!(warning.contains("bool") && warning.contains("table"), "{warning}");
            assert!(warning.contains("default"), "{warning}");
        }
        assert!(
            warnings.iter().all(|warning| {
                !warning.contains("line_length_marker") && !warning.contains("array_marker")
            }),
            "invalid values must not be logged: {warnings:#?}"
        );
    }

    #[test]
    fn invalid_parameter_shape_keeps_defaults_and_valid_siblings() {
        let raw = json!({
            "parameters": {
                "LineLength": 120,
                "Typo": "typo_marker",
                "BadWords": false,
                "MethodSize": { "maxMethodSize": 250 },
            }
        });
        let mut config = None;
        let warnings = warnings_of(|| {
            config = Some(DiagnosticsConfig::from_project_file(&raw, Locale::En, None));
        });

        assert_eq!(warnings.len(), 2, "{warnings:#?}");
        let config = config.unwrap();
        assert_eq!(config.get_int(DiagnosticCode::LineLength, "maxLineLength"), None);
        assert!(!config.is_disabled(DiagnosticCode::LineLength));
        assert!(config.is_disabled(DiagnosticCode::Typo), "Typo remains default-off");
        assert!(config.is_disabled(DiagnosticCode::BadWords));
        assert_eq!(config.get_int(DiagnosticCode::MethodSize, "maxMethodSize"), Some(250));
    }

    #[test]
    fn unknown_invalid_parameter_gets_only_the_existing_warning() {
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let raw = json!({ "parameters": { "UnknownDiagnostic": "private_marker" } });
        let warnings = warnings_of(|| {
            DiagnosticsConfig::from_project_file(&raw, Locale::En, Some(file));
        });

        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].contains("unknown diagnostic code `UnknownDiagnostic`"));
        assert!(warnings[0].contains("[diagnostics.parameters]"));
        assert!(warnings[0].contains("bsl-analyzer.toml"));
        assert!(!warnings[0].contains("private_marker"), "{:#?}", warnings);

        let known = json!({ "parameters": { "LineLength": "private_marker" } });
        let with_source = warnings_of(|| {
            DiagnosticsConfig::from_project_file(&known, Locale::En, Some(file));
        });
        assert_eq!(with_source.len(), 1, "{with_source:#?}");
        assert!(with_source[0].contains("bsl-analyzer.toml"), "{}", with_source[0]);
        assert!(!with_source[0].contains("private_marker"), "{with_source:#?}");

        let without_source = warnings_of(|| {
            DiagnosticsConfig::from_project_file(&known, Locale::En, None);
        });
        assert_eq!(without_source.len(), 1, "{without_source:#?}");
        assert!(without_source[0].contains("<project config>"), "{}", without_source[0]);
        assert!(!without_source[0].contains("private_marker"), "{without_source:#?}");
    }

    #[test]
    fn parameter_shape_warnings_are_load_time_only_and_repeat_on_reload() {
        let raw = json!({ "parameters": { "LineLength": "reload_marker" } });
        let (_, pure_warnings) = parse_with_warning_count(&raw, Locale::En);
        assert_eq!(pure_warnings, 0, "pure from_project_json stays quiet");
        let direct_serde_warnings = warnings_of(|| {
            let _: DiagnosticsConfig = serde_json::from_value(raw.clone()).unwrap();
        });
        assert!(direct_serde_warnings.is_empty(), "pure serde stays quiet");

        let first_load = warnings_of(|| {
            DiagnosticsConfig::from_project_file(&raw, Locale::En, None);
        });
        let second_load = warnings_of(|| {
            DiagnosticsConfig::from_project_file(&raw, Locale::En, None);
        });
        assert_eq!(first_load.len(), 1, "{first_load:#?}");
        assert_eq!(second_load.len(), 1, "a new load warns again: {second_load:#?}");
        assert!(first_load[0].contains("<project config>"), "{}", first_load[0]);
        assert!(second_load[0].contains("<project config>"), "{}", second_load[0]);
        assert!(!first_load[0].contains("reload_marker"), "{first_load:#?}");
        assert!(!second_load[0].contains("reload_marker"), "{second_load:#?}");
    }

    #[test]
    fn an_unknown_code_on_the_command_line_is_reported() {
        let mut config = DiagnosticsConfig::default();
        let err = config
            .apply_cli_filters(&["DeprecatedCurrentDate".to_owned()], &["Nope".to_owned()])
            .unwrap_err();
        let lines: Vec<&str> = err.lines().collect();
        assert_eq!(lines.len(), 2, "{err}");
        assert!(
            lines[0].contains("`DeprecatedCurrentDate`") && lines[0].contains("--only-diagnostic")
        );
        assert!(lines[1].contains("`Nope`") && lines[1].contains("--disable-diagnostic"));
        assert!(config.only_enabled.is_none(), "an unknown name selects nothing");
    }

    #[test]
    fn a_failed_command_line_filter_leaves_the_config_untouched() {
        let mut config = DiagnosticsConfig::default();
        config
            .apply_cli_filters(&["Nope".to_owned(), "LineLength,Other".to_owned()], &[])
            .unwrap_err();
        assert!(config.only_enabled.is_none());
        assert!(config.disabled.is_empty());

        let mut config = DiagnosticsConfig::default();
        config.apply_cli_filters(&["LineLength".to_owned()], &[]).unwrap();
        let before = config.only_enabled.clone();
        assert_eq!(before.as_ref().map(Vec::len), Some(1));
        // A valid name next to an unknown one must not be applied either.
        config
            .apply_cli_filters(
                &["DeprecatedCurrentDate".to_owned()],
                &["LineLength".to_owned(), "Nope".to_owned()],
            )
            .unwrap_err();
        assert_eq!(config.only_enabled, before);
        assert!(config.disabled.is_empty(), "{:?}", config.disabled);
    }

    /// `[diagnostics.parameters.DeprecatedPlatformApi]` with this `exclude` value.
    fn project_excluding(exclude: serde_json::Value) -> serde_json::Value {
        json!({ "parameters": { "DeprecatedPlatformApi": { "exclude": exclude } } })
    }

    #[test]
    fn an_unknown_excluded_name_is_reported() {
        use crate::handlers::deprecated_platform_api::is_excluded;
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let key = "[diagnostics.parameters.DeprecatedPlatformApi].exclude";

        // `ТекущаяДата` is a registry name; `ТекущаяДатаСеанса` is only its
        // replacement, `ТекущаяДатта` a typo, `1` not a string.
        let raw = project_excluding(json!(["ТекущаяДата", "ТекущаяДатаСеанса", "ТекущаяДатта", 1]));
        let mut config = None;
        let warnings = warnings_of(|| {
            config = Some(DiagnosticsConfig::from_project_file(&raw, Locale::En, Some(file)));
        });
        assert_eq!(warnings.len(), 3, "one warning per bad element: {warnings:#?}");
        for name in ["ТекущаяДатаСеанса", "ТекущаяДатта", "1"] {
            let named: Vec<_> =
                warnings.iter().filter(|w| w.contains(&format!("`{name}`"))).collect();
            assert_eq!(named.len(), 1, "one warning names `{name}`: {warnings:#?}");
            assert!(named[0].contains("bsl-analyzer.toml"), "names the file: {}", named[0]);
            assert!(named[0].contains(key), "names the key: {}", named[0]);
        }
        assert!(
            warnings.iter().all(|w| !w.contains("`ТекущаяДата`")),
            "a registry name is not reported: {warnings:#?}"
        );

        // The loader reports the rest and still applies the valid names.
        let config = config.unwrap();
        assert!(is_excluded(&config, "ТекущаяДата"));
        assert!(is_excluded(&config, "CurrentDate"), "one spelling covers both");
        assert!(!is_excluded(&config, "Найти"), "a name outside the list is not silenced");
        assert!(!is_excluded(&config, "ТекущаяДатта"), "a typo silences nothing");

        // The loaders that parse the section themselves (`check-config`) get the
        // same report.
        let direct = warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&raw, Some(file)));
        assert_eq!(direct.len(), 3, "{direct:#?}");

        // A value that is not an array is one warning and is ignored as a whole.
        let raw = project_excluding(json!("ТекущаяДата"));
        let mut config = None;
        let warnings = warnings_of(|| {
            config = Some(DiagnosticsConfig::from_project_file(&raw, Locale::En, Some(file)));
        });
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].contains(key), "{}", warnings[0]);
        assert!(warnings[0].contains("bsl-analyzer.toml"), "{}", warnings[0]);
        assert!(warnings[0].contains("must be an array"), "{}", warnings[0]);
        assert!(!is_excluded(&config.unwrap(), "ТекущаяДата"), "the value is ignored");

        // A qualified name and an enum member are no feature names either.
        for name in ["HTTPСоединение.Получить", "Вертикальная"] {
            let raw = project_excluding(json!([name]));
            let warnings = warnings_of(|| {
                DiagnosticsConfig::from_project_file(&raw, Locale::En, Some(file));
            });
            assert_eq!(warnings.len(), 1, "`{name}`: {warnings:#?}");
            assert!(warnings[0].contains(&format!("`{name}`")), "{}", warnings[0]);
        }

        // The controls: right names of either spelling, an empty list and a
        // section without the table are silent.
        let silent = [
            project_excluding(json!([
                "ТекущаяДата",
                "CurrentDate",
                "Find",
                "сообщить",
                "BriefErrorDescription",
                "УправляемаяФорма",
                "ManagedForm",
                "Получить",
                "ОриентацияМетокДиаграммы",
            ])),
            project_excluding(json!([])),
            json!({ "parameters": { "DeprecatedPlatformApi": {} } }),
            json!({ "parameters": { "LineLength": false } }),
        ];
        for raw in &silent {
            let warnings = warnings_of(|| {
                DiagnosticsConfig::from_project_file(raw, Locale::En, Some(file));
            });
            assert!(warnings.is_empty(), "{raw}: {warnings:#?}");
        }
    }

    #[test]
    fn stale_key_suggestion_is_derived_from_the_registry() {
        use crate::handlers::deprecated_platform_api::exclude_suggestion;
        let entry = |names: &str| {
            format!(
                "to silence only these findings keep `DeprecatedPlatformApi` on and write \
                 `[diagnostics.parameters.DeprecatedPlatformApi] exclude = [{names}]`"
            )
        };
        let general = "to silence individual deprecated features list their names in \
                       `[diagnostics.parameters.DeprecatedPlatformApi] exclude`";

        // The three error-processing methods are exactly the registry entries
        // whose replacement lives in the `ErrorProcessing` manager.
        let error_processing: Vec<String> = bsl_platform::deprecation::registry()
            .entries()
            .iter()
            .filter(|e| e.replacement.is_some_and(|r| r.en.starts_with("ErrorProcessing.")))
            .map(|e| format!("\"{}\"", e.ru))
            .collect();
        assert_eq!(error_processing.len(), 3, "{error_processing:?}");

        assert_eq!(
            exclude_suggestion("DeprecatedCurrentDate").as_deref(),
            Some(entry(r#""ТекущаяДата""#).as_str())
        );
        assert_eq!(
            exclude_suggestion("DeprecatedErrorProcessing").as_deref(),
            Some(entry(&error_processing.join(", ")).as_str())
        );
        assert_eq!(
            exclude_suggestion("DeprecatedErrorProcessing").as_deref(),
            Some(
                entry(
                    r#""КраткоеПредставлениеОшибки", "ПодробноеПредставлениеОшибки", "ПоказатьИнформациюОбОшибке""#
                )
                .as_str()
            )
        );
        assert_eq!(
            exclude_suggestion("DeprecatedFind").as_deref(),
            Some(entry(r#""Найти""#).as_str())
        );
        assert_eq!(
            exclude_suggestion("DeprecatedMessage").as_deref(),
            Some(entry(r#""Сообщить""#).as_str())
        );

        // Removed codes whose features the registry cannot name from the key.
        for removed in [
            "DeprecatedTypeManagedForm",
            "DeprecatedMethods8310",
            "DeprecatedMethods8317",
            "DeprecatedAttributes8312",
        ] {
            assert_eq!(exclude_suggestion(removed).as_deref(), Some(general), "{removed}");
        }

        // Anything else gets no pointer to `exclude` at all.
        for other in ["Nope", "DeprecatedFoo", "Deprecated", "deprecatedFind", "FindDeprecated"] {
            assert_eq!(exclude_suggestion(other), None, "{other}");
        }

        // The loader appends it to the reason of the stale key and to nothing else;
        // the behaviour of the keys is unchanged.
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let raw = json!({
            "parameters": {
                "DeprecatedErrorProcessing": false,
                "DeprecatedTypeManagedForm": false,
                "Nope": false,
                "DeprecatedFoo": false,
                "Deprecated": false,
            }
        });
        let mut config = None;
        let warnings = warnings_of(|| {
            config = Some(DiagnosticsConfig::from_project_file(&raw, Locale::En, Some(file)));
        });
        assert_eq!(warnings.len(), 5, "{warnings:#?}");
        let of = |key: &str| {
            warnings
                .iter()
                .find(|w| w.contains(&format!("`{key}`")))
                .unwrap_or_else(|| panic!("no warning for {key}: {warnings:#?}"))
        };
        assert!(of("DeprecatedErrorProcessing").contains(&entry(&error_processing.join(", "))));
        assert!(of("DeprecatedTypeManagedForm").contains(general));
        for key in ["Nope", "DeprecatedFoo", "Deprecated"] {
            assert!(!of(key).contains("exclude"), "{key} gets no pointer: {}", of(key));
        }
        let config = config.unwrap();
        assert!(config.disabled.is_empty(), "{:?}", config.disabled);
        assert!(config.parameters.is_empty(), "{:?}", config.parameters);

        // The same hint reaches the command-line filters.
        let mut cli = DiagnosticsConfig::default();
        let err = cli
            .apply_cli_filters(
                &["DeprecatedCurrentDate".to_owned()],
                &["DeprecatedErrorProcessing".to_owned(), "Nope".to_owned()],
            )
            .unwrap_err();
        let lines: Vec<&str> = err.lines().collect();
        assert_eq!(lines.len(), 3, "{err}");
        assert!(lines[0].contains(&entry(r#""ТекущаяДата""#)), "{}", lines[0]);
        assert!(lines[1].contains(&entry(&error_processing.join(", "))), "{}", lines[1]);
        assert!(!lines[2].contains("exclude"), "{}", lines[2]);
    }

    fn parse_with_warning_count(
        raw: &serde_json::Value,
        locale: Locale,
    ) -> (DiagnosticsConfig, usize) {
        let warnings = Arc::new(AtomicUsize::new(0));
        let config = tracing::subscriber::with_default(WarningCounter(warnings.clone()), || {
            DiagnosticsConfig::from_project_json(raw, locale)
        });
        (config, warnings.load(Ordering::Relaxed))
    }

    /// The shared project-config parser turns the raw `[diagnostics]` value into the
    /// same effective config every runtime mode consumes: a `parameters` entry of
    /// `false` disables a code, an object enables it with parameters, and the resolved
    /// locale is stamped over the default.
    #[test]
    fn from_project_json_parses_params_and_stamps_locale() {
        let raw = json!({
            "parameters": {
                "Typo": false,
                "LineLength": { "maxLineLength": 150 },
            }
        });
        let (config, warnings) = parse_with_warning_count(&raw, Locale::En);

        assert_eq!(warnings, 0);
        assert!(config.is_disabled(DiagnosticCode::Typo), "a `false` param disables the code");
        assert_eq!(
            config.get_int(DiagnosticCode::LineLength, "maxLineLength"),
            Some(150),
            "an object param carries the project threshold"
        );
        assert_eq!(config.locale, Locale::En, "the resolved locale overrides the default");
    }

    /// A malformed config (not an object) must not fail the analysis: it falls back to
    /// defaults while still stamping the locale.
    #[test]
    fn from_project_json_falls_back_on_garbage() {
        let (config, warnings) = parse_with_warning_count(&json!("not an object"), Locale::Ru);
        assert_eq!(warnings, 1);
        assert!(config.disabled.is_empty());
        assert!(config.parameters.is_empty());
        assert_eq!(config.locale, Locale::Ru);
    }

    /// An absent `[diagnostics]` section (serde null) yields defaults without warning.
    #[test]
    fn from_project_json_handles_null() {
        let (config, warnings) = parse_with_warning_count(&serde_json::Value::Null, Locale::En);
        assert_eq!(warnings, 0);
        assert!(config.disabled.is_empty());
        assert!(config.enabled.is_empty());
        assert_eq!(config.locale, Locale::En);
    }

    #[test]
    fn from_project_json_accepts_empty_object_without_warning() {
        let (config, warnings) = parse_with_warning_count(&json!({}), Locale::Ru);
        assert_eq!(warnings, 0);
        assert!(config.disabled.is_empty());
        assert!(config.enabled.is_empty());
        assert_eq!(config.locale, Locale::Ru);
    }

    #[test]
    fn static_lsp_severity_override_lowers_default_severity() {
        let config = DiagnosticsConfig::default();
        assert_eq!(config.severity(DiagnosticCode::UnavailableInEnvironment), Severity::Warning);
        assert_eq!(config.severity(DiagnosticCode::ModuleAccessibility), Severity::Warning);
        assert_eq!(config.severity(DiagnosticCode::ServerSideExportFormMethod), Severity::Warning);
        assert_eq!(config.severity(DiagnosticCode::AtClientAtServerFormMethod), Severity::Warning);
        // Control: metadata without a static override keeps its computed grade.
        assert_eq!(config.severity(DiagnosticCode::DeletingCollectionItem), Severity::Major);
    }

    #[test]
    fn user_override_wins_over_static_lsp_severity() {
        let mut config = DiagnosticsConfig::default();
        config.metadata_overrides.insert(
            DiagnosticCode::UnavailableInEnvironment,
            MetadataOverride { lsp_severity: Some("hint".into()), ..Default::default() },
        );
        assert_eq!(config.severity(DiagnosticCode::UnavailableInEnvironment), Severity::Hint);
    }

    fn override_section() -> serde_json::Value {
        json!({ "metadataOverrides": { "DeletingCollectionItem": { "lspSeverity": "warning" } } })
    }

    #[test]
    fn metadata_override_from_project_config_lowers_severity() {
        assert_eq!(
            DiagnosticsConfig::default().severity(DiagnosticCode::DeletingCollectionItem),
            Severity::Major,
            "control: the default grade is not Warning"
        );
        let config = DiagnosticsConfig::from_project_json(&override_section(), Locale::En);
        assert_eq!(config.severity(DiagnosticCode::DeletingCollectionItem), Severity::Warning);
        // The snake_case spelling of the interned form keeps working next to the camelCase one.
        let snake = json!({ "metadataOverrides": { "DeletingCollectionItem": { "lsp_severity": "hint" } } });
        let config = DiagnosticsConfig::from_project_json(&snake, Locale::En);
        assert_eq!(config.severity(DiagnosticCode::DeletingCollectionItem), Severity::Hint);
    }

    #[test]
    fn metadata_override_from_project_config_survives_the_interned_round_trip() {
        let config = DiagnosticsConfig::from_project_json(&override_section(), Locale::En);
        let restored = DiagnosticsConfig::from_input(&config.to_input());
        assert_eq!(restored, config);
        assert_eq!(restored.severity(DiagnosticCode::DeletingCollectionItem), Severity::Warning);

        // camelCase `diagnosticType` is the project spelling; the interned form stores
        // the snake_case field, and the round trip must not drop it.
        let with_type = json!({
            "metadataOverrides": {
                "DeletingCollectionItem": {
                    "lspSeverity": "warning",
                    "diagnosticType": "CodeSmell"
                }
            }
        });
        let config = DiagnosticsConfig::from_project_json(&with_type, Locale::En);
        let over = config
            .metadata_overrides
            .get(&DiagnosticCode::DeletingCollectionItem)
            .expect("override parsed");
        assert_eq!(over.diagnostic_type, Some(DiagnosticType::CodeSmell));
        assert_eq!(DiagnosticsConfig::from_input(&config.to_input()), config);
    }

    #[test]
    fn unknown_diagnostics_section_and_override_code_are_reported() {
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let raw = json!({
            "metadataOverride": { "DeletingCollectionItem": { "lspSeverity": "warning" } },
            "metadataOverrides": { "NoSuchCode": { "lspSeverity": "warning" } },
            "ordinaryAppSupport": true,
        });
        let warnings = warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&raw, Some(file)));
        assert_eq!(warnings.len(), 2, "one warning per ignored key: {warnings:#?}");
        let section = warnings.iter().find(|w| w.contains("`metadataOverride`")).unwrap();
        assert!(section.contains("bsl-analyzer.toml"), "names the file: {section}");
        assert!(section.contains("metadataOverrides"), "lists the supported keys: {section}");
        let code = warnings.iter().find(|w| w.contains("`NoSuchCode`")).unwrap();
        assert!(code.contains("[diagnostics.metadataOverrides]"), "names the section: {code}");

        let typo = json!({
            "metadataOverrides": { "DeletingCollectionItem": { "lspSeverity": "warnng" } }
        });
        let warnings =
            warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&typo, Some(file)));
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].contains("`warnng`"), "names the value: {}", warnings[0]);
        assert!(warnings[0].contains("`DeletingCollectionItem`"), "{}", warnings[0]);
        assert!(warnings[0].contains("bsl-analyzer.toml"), "{}", warnings[0]);
        assert!(warnings[0].contains("falls back to `warning`"), "{}", warnings[0]);

        let known = warnings_of(|| {
            DiagnosticsConfig::warn_unknown_parameters(&override_section(), Some(file))
        });
        assert!(known.is_empty(), "a valid section is silent: {known:#?}");
    }

    #[test]
    fn a_bad_override_entry_does_not_discard_the_rest_of_the_config() {
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let raw = json!({
            "parameters": { "LineLength": false },
            "metadataOverrides": {
                "DeletingCollectionItem": { "diagnosticType": "CodeSmel" },
                "Typo": { "lspSeverity": "hint" }
            }
        });
        let config = DiagnosticsConfig::from_project_json(&raw, Locale::En);
        assert!(
            config.is_disabled(DiagnosticCode::LineLength),
            "one bad override must not reset the section: {:?}",
            config.disabled
        );
        assert_eq!(
            config
                .metadata_overrides
                .get(&DiagnosticCode::Typo)
                .and_then(|over| over.lsp_severity.as_deref()),
            Some("hint"),
            "a sibling override still applies"
        );
        assert!(
            config
                .metadata_overrides
                .get(&DiagnosticCode::DeletingCollectionItem)
                .and_then(|over| over.diagnostic_type)
                .is_none(),
            "the bad field is not stored"
        );
        let warnings = warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&raw, Some(file)));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("`CodeSmel`") && w.contains("`DeletingCollectionItem`")),
            "names the bad value and the code: {warnings:#?}"
        );

        // A case difference is a spelling of the same type, not a reason to drop the section.
        let folded = json!({
            "parameters": { "LineLength": false },
            "metadataOverrides": {
                "DeletingCollectionItem": { "diagnosticType": "codeSmell" }
            }
        });
        let config = DiagnosticsConfig::from_project_json(&folded, Locale::En);
        assert!(config.is_disabled(DiagnosticCode::LineLength));
        assert_eq!(
            config
                .metadata_overrides
                .get(&DiagnosticCode::DeletingCollectionItem)
                .and_then(|over| over.diagnostic_type),
            Some(DiagnosticType::CodeSmell)
        );

        let not_a_table = json!({
            "parameters": { "LineLength": false },
            "metadataOverrides": "hint"
        });
        let config = DiagnosticsConfig::from_project_json(&not_a_table, Locale::En);
        assert!(config.is_disabled(DiagnosticCode::LineLength));
    }

    #[test]
    fn both_severity_spellings_do_not_drop_the_override() {
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let same = json!({
            "metadataOverrides": {
                "DeletingCollectionItem": { "lspSeverity": "hint", "lsp_severity": "hint" }
            }
        });
        let config = DiagnosticsConfig::from_project_json(&same, Locale::En);
        assert_eq!(config.severity(DiagnosticCode::DeletingCollectionItem), Severity::Hint);
        let warnings =
            warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&same, Some(file)));
        assert!(warnings.is_empty(), "equal spellings are not a clash: {warnings:#?}");

        let clash = json!({
            "parameters": { "LineLength": false },
            "metadataOverrides": {
                "DeletingCollectionItem": { "lspSeverity": "hint", "lsp_severity": "error" }
            }
        });
        let config = DiagnosticsConfig::from_project_json(&clash, Locale::En);
        assert!(config.is_disabled(DiagnosticCode::LineLength));
        assert_eq!(
            config.severity(DiagnosticCode::DeletingCollectionItem),
            Severity::Hint,
            "camelCase wins"
        );
        let warnings =
            warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&clash, Some(file)));
        assert!(
            warnings.iter().any(|w| w.contains("lspSeverity") && w.contains("lsp_severity")),
            "{warnings:#?}"
        );
    }

    #[test]
    fn an_unknown_override_field_is_reported_and_not_applied() {
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let raw = json!({
            "metadataOverrides": { "DeletingCollectionItem": { "lspSeveriti": "hint" } }
        });
        let config = DiagnosticsConfig::from_project_json(&raw, Locale::En);
        assert_eq!(
            config.severity(DiagnosticCode::DeletingCollectionItem),
            Severity::Major,
            "a misspelled field must not change the grade"
        );
        let warnings = warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&raw, Some(file)));
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].contains("`lspSeveriti`"), "{}", warnings[0]);
        assert!(warnings[0].contains("bsl-analyzer.toml"), "{}", warnings[0]);
    }

    #[test]
    fn an_inert_override_field_is_reported() {
        let file = Path::new("/work/project/bsl-analyzer.toml");
        let raw = json!({
            "metadataOverrides": {
                "DeletingCollectionItem": { "diagnosticType": "Vulnerability", "severity": "Blocker" }
            }
        });
        let config = DiagnosticsConfig::from_project_json(&raw, Locale::En);
        assert_eq!(
            config.severity(DiagnosticCode::DeletingCollectionItem),
            Severity::Major,
            "neither field changes the reported grade"
        );
        let over = config.metadata_overrides.get(&DiagnosticCode::DeletingCollectionItem).unwrap();
        assert_eq!(over.diagnostic_type, Some(DiagnosticType::Vulnerability));
        assert_eq!(over.severity, Some(DiagnosticSeverityLevel::Blocker));
        let warnings = warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&raw, Some(file)));
        assert!(
            warnings.iter().any(
                |w| w.contains("`diagnosticType`") && w.contains("does not change the finding")
            ),
            "{warnings:#?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("`severity`") && w.contains("does not change the finding")),
            "{warnings:#?}"
        );
    }

    #[test]
    fn a_mixed_tag_list_is_not_applied() {
        let raw = json!({
            "metadataOverrides": {
                "DeletingCollectionItem": { "tags": ["Unused", "NoSuchTag"] }
            }
        });
        let config = DiagnosticsConfig::from_project_json(&raw, Locale::En);
        assert!(
            config
                .metadata_overrides
                .get(&DiagnosticCode::DeletingCollectionItem)
                .and_then(|over| over.tags.as_ref())
                .is_none(),
            "a mixed list must not replace the rule's tags"
        );
        let warnings = warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&raw, None));
        assert!(
            warnings.iter().any(|w| w.contains("`tags`") && w.contains("the field is ignored")),
            "{warnings:#?}"
        );
    }

    #[test]
    fn a_non_string_alias_does_not_hide_the_other_spelling() {
        let raw = json!({
            "metadataOverrides": {
                "DeletingCollectionItem": {
                    "diagnosticType": 1,
                    "diagnostic_type": "CodeSmell"
                }
            }
        });
        let config = DiagnosticsConfig::from_project_json(&raw, Locale::En);
        assert_eq!(
            config
                .metadata_overrides
                .get(&DiagnosticCode::DeletingCollectionItem)
                .and_then(|over| over.diagnostic_type),
            Some(DiagnosticType::CodeSmell),
            "the string spelling still applies"
        );

        let snake = json!({
            "metadataOverrides": { "DeletingCollectionItem": { "lsp_severity": 5 } }
        });
        let warnings = warnings_of(|| DiagnosticsConfig::warn_unknown_parameters(&snake, None));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("`lsp_severity`") && w.contains("expected a string")),
            "names the key that was written: {warnings:#?}"
        );
        assert!(
            warnings.iter().all(|w| !w.contains("`lspSeverity`")),
            "does not name the spelling the author did not write: {warnings:#?}"
        );
    }
}

#[cfg(test)]
mod interned_round_trip {
    use super::*;

    /// A configuration and the input it interns to must agree on every
    /// field that influences a diagnostic: the memoised checks run under
    /// `from_input(to_input(config))`, the caller under `config`.
    #[test]
    fn config_survives_the_interned_round_trip() {
        let mut config = DiagnosticsConfig::all_enabled();
        config.disabled = vec![DiagnosticCode::LineLength];
        config
            .parameters
            .insert(DiagnosticCode::MethodSize, serde_json::json!({ "maxMethodSize": 10 }));
        config.only_enabled =
            Some(vec![DiagnosticCode::MethodSize, DiagnosticCode::UnusedLocalVariable]);
        config.metadata_overrides.insert(
            DiagnosticCode::MethodSize,
            MetadataOverride {
                severity: Some(DiagnosticSeverityLevel::Blocker),
                diagnostic_type: Some(DiagnosticType::Vulnerability),
                tags: Some(vec![MetadataTag::Performance]),
                lsp_severity: Some("Error".to_owned()),
            },
        );
        config.dataflow_max_iterations = 7;
        config.bslls_suppression_compat = false;
        config.quick_fixes = false;

        let restored = DiagnosticsConfig::from_input(&config.to_input());
        assert_eq!(restored, config);

        // The round trip is not vacuous: a configuration that differs in a
        // filter interns to a different key.
        let mut other = config.clone();
        other.only_enabled = None;
        assert_ne!(other.to_input(), config.to_input());
        let mut with_fixes = config.clone();
        with_fixes.quick_fixes = true;
        assert_ne!(with_fixes.to_input(), config.to_input());
    }
}
