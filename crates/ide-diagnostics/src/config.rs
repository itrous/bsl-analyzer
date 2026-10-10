use crate::handlers;
use crate::metadata::{DiagnosticSeverityLevel, DiagnosticType, MetadataTag};
use crate::{DiagnosticCode, Severity};
use base_db::{DiagnosticsConfigInput, Locale};
use std::collections::HashMap;
use std::path::Path;
use stdx::case::CaseExt;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetadataOverride {
    pub severity: Option<DiagnosticSeverityLevel>,
    pub diagnostic_type: Option<DiagnosticType>,
    pub tags: Option<Vec<MetadataTag>>,
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
        self.base.calculate_severity()
    }

    pub fn tags(&self) -> Vec<MetadataTag> {
        self.tags_override.clone().unwrap_or_else(|| self.base.tags.to_vec())
    }
}

fn parse_severity(s: &str) -> Severity {
    match s.fold_lower().as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "information" | "info" => Severity::Information,
        "hint" => Severity::Hint,
        "blocker" => Severity::Blocker,
        "critical" => Severity::Critical,
        "major" => Severity::Major,
        _ => Severity::Warning,
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
    /// `[diagnostics]` section came from: a code in `[diagnostics.parameters]` that
    /// is not a diagnostic of this analyzer is reported (key and file) instead of
    /// being dropped without a trace. The effective config is the same.
    ///
    /// Not used by per-request readers (a `query` call re-reads the section each
    /// time): they would repeat the warning on every call.
    pub fn from_project_file(
        diagnostics: &serde_json::Value,
        locale: Locale,
        source: Option<&Path>,
    ) -> Self {
        warn_unknown_parameter_codes(diagnostics, source);
        Self::from_project_json(diagnostics, locale)
    }

    pub fn all_enabled() -> Self {
        let mut enabled = Vec::new();
        for code in [
            DiagnosticCode::BadWords,
            DiagnosticCode::CodeAfterAsyncCall,
            DiagnosticCode::DenyIncompleteValues,
            DiagnosticCode::FieldsFromJoinsWithoutIsNull,
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

/// Codes this analyzer used to have, with the code that took over their checks.
/// All of them were merged into `DeprecatedPlatformApi` in v0.2.50.
const REMOVED_CODES: &[(&str, &str, &str)] = &[
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
fn unknown_code_hint(name: &str) -> String {
    match REMOVED_CODES.iter().find(|(removed, _, _)| *removed == name) {
        Some((_, successor, release)) => format!(
            "the code was removed in {release}; its check is now part of `{successor}` \
             (not mapped automatically: `{successor} = false` would switch off every \
             deprecated-API finding, not only this one)"
        ),
        None => "no diagnostic has this name (typo, or a code of another analyzer)".to_owned(),
    }
}

fn warn_unknown_parameter_codes(diagnostics: &serde_json::Value, source: Option<&Path>) {
    for name in unknown_parameter_codes(diagnostics) {
        let file =
            source.map_or_else(|| "<project config>".to_owned(), |p| p.display().to_string());
        tracing::warn!(
            key = name,
            config_file = %file,
            "unknown diagnostic code `{name}` in [diagnostics.parameters] of {file}: \
             the key is ignored and the rule keeps its default; {}",
            unknown_code_hint(name)
        );
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
                    metadata_overrides: HashMap::new(),
                    only_enabled: None,
                    locale: Locale::default(),
                    bslls_suppression_compat,
                    scope: None,
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
    }

    pub fn apply_cli_filters(&mut self, only_diagnostic: &[String], disable_diagnostic: &[String]) {
        warn_unknown_cli_codes(only_diagnostic, "--only-diagnostic");
        warn_unknown_cli_codes(disable_diagnostic, "--disable-diagnostic");

        if !only_diagnostic.is_empty() {
            let codes: Vec<DiagnosticCode> =
                only_diagnostic.iter().filter_map(|s| s.parse().ok()).collect();
            if !codes.is_empty() {
                self.only_enabled = Some(codes);
            }
        }

        for code_str in disable_diagnostic {
            if let Ok(code) = code_str.parse::<DiagnosticCode>() {
                if !self.disabled.contains(&code) {
                    self.disabled.push(code);
                }
            }
        }
    }
}

fn warn_unknown_cli_codes(names: &[String], flag: &str) {
    for name in names.iter().filter(|name| name.parse::<DiagnosticCode>().is_err()) {
        tracing::warn!(
            key = name.as_str(),
            "unknown diagnostic code `{name}` in {flag}: the name is ignored; {}",
            unknown_code_hint(name)
        );
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
        let never_existed =
            warnings.iter().find(|w| w.contains("`DeprecatedErrorProcessing`")).unwrap();
        assert!(never_existed.contains("no diagnostic has this name"), "{never_existed}");
        assert!(!never_existed.contains("DeprecatedPlatformApi"), "{never_existed}");

        // Behaviour is unchanged: the stale keys are still ignored and are NOT
        // mapped to the successor, which would silence the whole group.
        let config = config.unwrap();
        assert!(config.is_disabled(DiagnosticCode::LineLength));
        assert!(!config.is_disabled(DiagnosticCode::DeprecatedPlatformApi));
        assert_eq!(config.get_int(DiagnosticCode::MethodSize, "maxMethodSize"), Some(80));
        assert_eq!(config.disabled, vec![DiagnosticCode::LineLength]);
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
    fn an_unknown_code_on_the_command_line_is_reported() {
        let mut config = DiagnosticsConfig::default();
        let warnings = warnings_of(|| {
            config.apply_cli_filters(&["DeprecatedCurrentDate".to_owned()], &["Nope".to_owned()]);
        });
        assert_eq!(warnings.len(), 2, "{warnings:#?}");
        assert!(
            warnings[0].contains("`DeprecatedCurrentDate`")
                && warnings[0].contains("--only-diagnostic")
        );
        assert!(warnings[1].contains("`Nope`") && warnings[1].contains("--disable-diagnostic"));
        assert!(config.only_enabled.is_none(), "an unknown name selects nothing");
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

        let restored = DiagnosticsConfig::from_input(&config.to_input());
        assert_eq!(restored, config);

        // The round trip is not vacuous: a configuration that differs in a
        // filter interns to a different key.
        let mut other = config.clone();
        other.only_enabled = None;
        assert_ne!(other.to_input(), config.to_input());
    }
}
