use base_db::{DiagnosticsConfigId, DiagnosticsConfigInput, Locale};

use crate::global_state::GlobalState;
use crate::locale::resolve_locale;

impl GlobalState {
    pub fn diagnostics_config_id(&self) -> DiagnosticsConfigId<'_> {
        DiagnosticsConfigId::new(self.analysis_host.raw_database(), self.diagnostics_config.clone())
    }

    pub fn diagnostics_config(&self) -> &DiagnosticsConfigInput {
        &self.diagnostics_config
    }

    pub fn update_diagnostics_config(&mut self) {
        let project_locale = self.project.as_ref().and_then(|p| Self::project_locale(&p.config));
        let locale = resolve_locale(project_locale, self.lsp_locale);

        self.diagnostics_config =
            self.project.as_ref().map(|p| Self::config_from_project(p, locale)).unwrap_or_else(
                || {
                    DiagnosticsConfigInput::from_raw(
                        Vec::<String>::new(),
                        Vec::<String>::new(),
                        Vec::<(String, String)>::new(),
                        false,
                        hir::dataflow::DEFAULT_MAX_ITERATIONS,
                        locale,
                        true,
                    )
                },
            );

        // The input was rebuilt from the project config; re-attach the current
        // vendor-diff scope so a config reload does not silently drop the filter.
        self.apply_scope_to_config();

        tracing::info!(
            disabled_count = self.diagnostics_config.disabled.len(),
            enabled_count = self.diagnostics_config.enabled.len(),
            params_count = self.diagnostics_config.parameters.len(),
            scope = self.diagnostics_config.scope.is_some(),
            ?locale,
            "updated diagnostics config"
        );

        if !self.diagnostics_config.disabled.is_empty() {
            tracing::debug!(
                disabled = ?self.diagnostics_config.disabled,
                "disabled diagnostics from config"
            );
        }
    }

    fn project_locale(config: &project_model::ProjectConfig) -> Option<Locale> {
        config.output.resolve_locale()
    }

    fn config_from_project(
        project: &project_model::Project,
        locale: Locale,
    ) -> DiagnosticsConfigInput {
        let diagnostics = project.config.diagnostics.rules_json();
        let config = ide::DiagnosticsConfig::from_project_file(
            &diagnostics,
            locale,
            project.config.config_file_path(),
        );

        let disabled: Vec<String> = config.disabled.iter().map(|code| code.to_string()).collect();
        let enabled: Vec<String> = config.enabled.iter().map(|code| code.to_string()).collect();

        let parameters: Vec<(String, String)> = config
            .parameters
            .iter()
            .map(|(code, value)| {
                (code.to_string(), serde_json::to_string(value).unwrap_or_default())
            })
            .collect();

        DiagnosticsConfigInput::from_raw(
            disabled,
            enabled,
            parameters,
            config.ordinary_app_support,
            config.dataflow_max_iterations,
            locale,
            config.bslls_suppression_compat,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::{span, Event, Id, Metadata, Subscriber};

    struct WarningTexts(Arc<Mutex<Vec<String>>>);

    struct MessageText(String);

    impl tracing::field::Visit for MessageText {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
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
                let mut text = MessageText(String::new());
                event.record(&mut text);
                self.0.lock().unwrap().push(text.0);
            }
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    /// The loader path of the LSP: a project config that switches rules off by name,
    /// two of them names this analyzer does not have, must say so - otherwise the
    /// author believes the warnings are off while they keep coming.
    #[test]
    fn stale_codes_in_the_project_config_are_reported_by_the_loader() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            r#"
min_platform_version = "8.3.17"

[diagnostics.parameters]
LineLength = false
DeprecatedCurrentDate = false  # removed in v0.2.50
DeprecatedErrorProcessing = false  # never existed
MagicNumber = false
"#,
        )
        .unwrap();
        let project = project_model::Project::new(dir.path()).expect("valid test project");

        let texts = Arc::new(Mutex::new(Vec::new()));
        let input = tracing::subscriber::with_default(WarningTexts(texts.clone()), || {
            GlobalState::config_from_project(&project, Locale::default())
        });
        let warnings = texts.lock().unwrap().clone();

        assert_eq!(warnings.len(), 2, "{warnings:#?}");
        assert!(warnings.iter().any(|w| w.contains("`DeprecatedCurrentDate`")), "{warnings:#?}");
        assert!(
            warnings.iter().any(|w| w.contains("`DeprecatedErrorProcessing`")),
            "{warnings:#?}"
        );
        assert!(warnings.iter().all(|w| w.contains("bsl-analyzer.toml")), "{warnings:#?}");

        // The valid keys still take effect; the stale ones add nothing.
        let mut disabled = input.disabled.clone();
        disabled.sort();
        assert_eq!(disabled, ["LineLength", "MagicNumber"]);
    }
}
