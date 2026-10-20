use std::{error::Error, fmt::Write as _, io};

use super::source_set::{
    configuration_root_provider, extensions_provider, externals_provider, SourceProvider,
    SourceSetArgs,
};

pub fn check_config(
    config: std::path::PathBuf,
    source_set: SourceSetArgs,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing::info!("Checking configuration: {:?}", config);

    let mut project_config = project_model::ProjectConfig::load_from_file(&config).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "failed to parse configuration file '{}': {}; expected a valid bsl-analyzer.toml, .bsl-analyzer.json, or .bsl-language-server.json",
                config.display(),
                e.message
            ),
        )
    })?;
    // Relative flag paths resolve against this command's project root — the
    // config file's directory — exactly as the paths inside that file do.
    let project_root = match config.parent() {
        Some(parent) if parent.as_os_str().is_empty() => std::path::Path::new("."),
        Some(parent) => parent,
        None => std::path::Path::new("."),
    };
    let providers = SourceProviders {
        configuration_root: configuration_root_provider(
            &source_set,
            project_config.configuration_root.as_deref(),
            project_root,
        ),
        extensions: extensions_provider(&source_set, project_config.extensions.as_ref()),
        externals: externals_provider(&source_set, project_config.externals.as_ref()),
    };
    source_set.resolve(project_root)?.apply_to(&mut project_config);

    ide::DiagnosticsConfig::warn_unknown_parameters(
        &project_config.diagnostics.rules_json(),
        Some(&config),
    );
    let mut diagnostics_config = diagnostics_config_from_project(&project_config)?;
    let diagnostics =
        mcp_server::resolve_project_baseline_diagnostics(config.parent(), &project_config);
    let project = project_model::Project::with_config(project_root, project_config.clone());
    if let Ok(project) = &project {
        project.warn_about_external_discovery_rejections();
    }
    if let Some((value, project_model::OrdinaryAppSupportSource::ConfigurationXml)) =
        project.as_ref().ok().and_then(project_model::Project::ordinary_app_support)
    {
        diagnostics_config.ordinary_app_support = value;
    }
    let diagnostics_baseline = inspect_diagnostics_baseline(&project);
    let report = build_check_config_report(
        &config,
        &project_config,
        &project,
        &diagnostics_config,
        &diagnostics,
        &diagnostics_baseline,
        &providers,
    );

    print!("{report}");

    if let Err(e) = &project {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("configuration is invalid: {e}"),
        )
        .into());
    }
    if diagnostics_baseline.issue {
        // Every failure carries its own detail — the path that is missing, the schema
        // that is unsupported. A caller that captures only stderr sees this line and
        // nothing else, so a constant string there is not actionable.
        let detail = diagnostics_baseline
            .error_detail
            .as_deref()
            .unwrap_or("diagnostics baseline reported an error");
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("configuration is invalid: {detail}"),
        )
        .into());
    }
    if baseline_diagnostics_have_issues(&diagnostics) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration is invalid: search baseline diagnostics reported errors",
        )
        .into());
    }

    if let Some(warning) =
        project.as_ref().ok().and_then(project_model::Project::legacy_extension_scope_warning)
    {
        eprintln!("warning: {warning}");
    }

    Ok(())
}

fn diagnostics_config_from_project(
    project_config: &project_model::ProjectConfig,
) -> Result<ide::DiagnosticsConfig, Box<dyn Error + Send + Sync>> {
    let locale = project_config.output.resolve_locale().unwrap_or_default();
    let diagnostics = project_config.diagnostics.rules_json();

    if diagnostics.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(ide::DiagnosticsConfig { locale, ..Default::default() });
    }

    let mut cfg: ide::DiagnosticsConfig =
        serde_json::from_value(diagnostics).map_err(|error| -> Box<dyn Error + Send + Sync> {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to parse diagnostics section: {error}"),
            )
            .into()
        })?;
    cfg.locale = locale;
    Ok(cfg)
}

struct DiagnosticsBaselineCheck {
    status: String,
    issue: bool,
    error_detail: Option<String>,
}

fn inspect_diagnostics_baseline(
    project: &Result<project_model::Project, project_model::ProjectError>,
) -> DiagnosticsBaselineCheck {
    let Ok(project) = project else {
        return DiagnosticsBaselineCheck {
            status: "unavailable (project invalid)".to_owned(),
            issue: false,
            error_detail: None,
        };
    };
    use ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot;

    match DiagnosticsBaselineSnapshot::load(project) {
        DiagnosticsBaselineSnapshot::Disabled => DiagnosticsBaselineCheck {
            status: "disabled".to_owned(),
            issue: false,
            error_detail: None,
        },
        DiagnosticsBaselineSnapshot::Ready { baseline, project_path, .. } => {
            DiagnosticsBaselineCheck {
                status: format!(
                    "ready: {} (schema {}, {} entries)",
                    project_path,
                    baseline.schema_version,
                    baseline.diagnostics.len()
                ),
                issue: false,
                error_detail: None,
            }
        }
        DiagnosticsBaselineSnapshot::ReadySet { baseline, plan, project_path, .. } => {
            let selection = match plan.selection {
                project_model::DiagnosticsBaselineSelection::All => "all",
                project_model::DiagnosticsBaselineSelection::Selective => "selective",
            };
            let enabled = plan.enabled_partition_ids.join(", ");
            let unsuppressed = plan
                .partitions
                .iter()
                .filter(|partition| {
                    !plan.enabled_partition_ids.iter().any(|enabled| enabled == &partition.id)
                })
                .map(|partition| partition.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            DiagnosticsBaselineCheck {
                status: format!(
                    "ready: {} (schema {}, selection {selection}; enabled partitions: {enabled}; unsuppressed partitions: {})",
                    project_path,
                    baseline.manifest.schema_version,
                    if unsuppressed.is_empty() { "none" } else { &unsuppressed }
                ),
                issue: false,
                error_detail: None,
            }
        }
        DiagnosticsBaselineSnapshot::Error { path, code, detail, .. } => {
            let path = path.as_deref().map(|path| path.display().to_string()).unwrap_or_default();
            let status = match code.as_str() {
                "invalid_configuration" => format!("ERROR: invalid configuration: {detail}"),
                "missing" => format!("ERROR: file is missing: {path}"),
                "unreadable" => format!("ERROR: cannot read {path}: {detail}"),
                "unsupported_schema" => format!("ERROR: {detail}: {path}"),
                _ => format!("ERROR: invalid file {path}: {detail}"),
            };
            DiagnosticsBaselineCheck { status, issue: true, error_detail: Some(detail) }
        }
    }
}

fn baseline_diagnostics_have_issues(
    baseline_diagnostics: &mcp_server::BaselineConfigDiagnostics,
) -> bool {
    baseline_diagnostics.workspace.issue.is_some() || baseline_diagnostics.reference.issue.is_some()
}

/// Where each resolved source-set field came from. Reported per field: the root
/// and the extension list are decided independently, so one label for both
/// would misname the origin whenever only one of them is overridden.
struct SourceProviders {
    configuration_root: SourceProvider,
    extensions: SourceProvider,
    externals: SourceProvider,
}

fn build_check_config_report(
    config_path: &std::path::Path,
    project_config: &project_model::ProjectConfig,
    project: &Result<project_model::Project, project_model::ProjectError>,
    diagnostics_config: &ide::DiagnosticsConfig,
    baseline_diagnostics: &mcp_server::BaselineConfigDiagnostics,
    diagnostics_baseline: &DiagnosticsBaselineCheck,
    providers: &SourceProviders,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Configuration is {}.",
        if baseline_diagnostics_have_issues(baseline_diagnostics)
            || diagnostics_baseline.issue
            || project.is_err()
        {
            "invalid"
        } else {
            "valid"
        }
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Config file: {}", config_path.display());
    let _ = writeln!(
        out,
        "Format: {}",
        match config_path.extension().and_then(|ext| ext.to_str()) {
            Some("toml") => "TOML",
            Some("json") => "JSON",
            _ => "auto",
        }
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Project:");
    let declared_root = project_config.configuration_root.as_deref();
    let used_root = match providers.configuration_root {
        SourceProvider::AutoDiscovery => None,
        _ => declared_root,
    };
    let _ = writeln!(
        out,
        "  Source root: {} [from: {}]",
        used_root.unwrap_or("auto-discovery"),
        providers.configuration_root.label(config_path)
    );
    // A declared root without a `Configuration.xml` does not decide anything —
    // the model warns and searches instead. Saying so beats printing the value
    // next to a provider that contradicts it.
    if used_root.is_none() {
        if let Some(ignored) = declared_root {
            let _ = writeln!(
                out,
                "               (declared \"{ignored}\" ignored: no Configuration.xml there)"
            );
        }
    }
    let _ = writeln!(
        out,
        "  Extensions:  {}",
        match &project_config.extensions {
            None => "auto-discovery (src/cfe/*)".to_owned(),
            Some(list) if list.is_empty() => "none".to_owned(),
            Some(list) => list
                .iter()
                .map(|decl| match decl {
                    project_model::ExtensionDecl::Path(path) => path.clone(),
                    project_model::ExtensionDecl::Structured(entry) => {
                        let scope = entry
                            .scope
                            .map(|scope| format!(", scope: {scope:?}"))
                            .unwrap_or_default();
                        if entry.depends_on.is_empty() {
                            format!("{} ({}{scope})", entry.name, entry.path)
                        } else {
                            format!(
                                "{} ({}{scope}, dependsOn: {})",
                                entry.name,
                                entry.path,
                                entry.depends_on.join(", ")
                            )
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
        }
    );
    let _ = writeln!(out, "  Extensions from: {}", providers.extensions.label(config_path));
    let _ = writeln!(
        out,
        "  Externals:   {} [from: {}]",
        match &project_config.externals {
            None => "auto-discovery (src/epf/*, src/erf/*)".to_owned(),
            Some(list) if list.is_empty() => "none".to_owned(),
            Some(list) => list
                .iter()
                .map(|decl| match &decl.depends_on {
                    None => format!("{} ({})", decl.name, decl.path),
                    Some(deps) => format!(
                        "{} ({}, dependsOn: {})",
                        decl.name,
                        decl.path,
                        if deps.is_empty() { "none".to_owned() } else { deps.join(", ") }
                    ),
                })
                .collect::<Vec<_>>()
                .join(", "),
        },
        providers.externals.label(config_path)
    );
    if let Ok(project) = project {
        for notice in [project.standalone_extension_notice(), project.standalone_external_notice()]
            .into_iter()
            .flatten()
        {
            let _ = writeln!(out, "  WARNING: {notice}");
        }
    }
    let _ =
        writeln!(out, "  Language:    {}", project_config.language.as_deref().unwrap_or("default"));
    let _ = writeln!(
        out,
        "  Diff base:   {}",
        project_config.analysis.diff_base.as_deref().unwrap_or("none (full analysis)")
    );
    let _ = writeln!(
        out,
        "  Ignored authors: {}",
        if project_config.analysis.ignored_authors.is_empty() {
            "none".to_owned()
        } else {
            project_config.analysis.ignored_authors.join(", ")
        }
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Extension topology:");
    match project {
        Ok(project) => {
            let topology = project.extension_topology();
            if topology.nodes().is_empty() {
                let _ = writeln!(out, "  no extensions resolved");
            } else {
                for node in topology.nodes() {
                    let deps = if let project_model::NodeKind::External(kind) = node.kind() {
                        format!("external {}", kind.element_name())
                    } else if node.depends_on().is_empty() {
                        "independent".to_owned()
                    } else {
                        format!(
                            "dependsOn: {}",
                            node.depends_on()
                                .iter()
                                .map(|id| topology.node(*id).name())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let scope =
                        node.scope().map(|scope| format!(", scope: {scope:?}")).unwrap_or_default();
                    let _ = writeln!(
                        out,
                        "  {} — {} ({deps}{scope})",
                        node.name(),
                        node.path().display()
                    );
                }
                let _ = writeln!(
                    out,
                    "  Order:       {}",
                    topology
                        .topological_order()
                        .iter()
                        .map(|id| topology.node(*id).name())
                        .collect::<Vec<_>>()
                        .join(" -> ")
                );
                let _ = writeln!(out, "  Fingerprint: {}", topology.fingerprint().to_hex());
            }
        }
        Err(e) => {
            let _ = writeln!(out, "  invalid: {e}");
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Diagnostics:");
    let ordinary_source =
        match project.as_ref().ok().and_then(project_model::Project::ordinary_app_support) {
            Some((_, project_model::OrdinaryAppSupportSource::Setting)) => "",
            Some((_, project_model::OrdinaryAppSupportSource::ConfigurationXml)) => {
                " (from DefaultRunMode of Configuration.xml)"
            }
            None => " (default)",
        };
    let _ = writeln!(
        out,
        "  ordinaryAppSupport: {}{ordinary_source}",
        diagnostics_config.ordinary_app_support
    );
    let _ =
        writeln!(out, "  dataflowMaxIterations: {}", diagnostics_config.dataflow_max_iterations);
    let _ = writeln!(out, "  Disabled:   {}", diagnostics_config.disabled.len());
    let _ = writeln!(out, "  Enabled:    {}", diagnostics_config.enabled.len());
    let _ = writeln!(out, "  Parameters: {}", diagnostics_config.parameters.len());
    let _ = writeln!(
        out,
        "  Disabled codes: {}",
        summarize_diagnostic_codes(diagnostics_config.disabled.iter().map(ToString::to_string))
    );
    let _ = writeln!(
        out,
        "  Explicitly enabled: {}",
        summarize_diagnostic_codes(diagnostics_config.enabled.iter().map(ToString::to_string))
    );
    let _ = writeln!(
        out,
        "  Parameterized: {}",
        summarize_diagnostic_codes(diagnostics_config.parameters.keys().map(ToString::to_string))
    );
    let _ = writeln!(out, "  Baseline:   {}", diagnostics_baseline.status);
    let _ = writeln!(out);
    let _ = writeln!(out, "Code lens:");
    let _ = writeln!(
        out,
        "  Cognitive complexity: {}",
        on_off(project_config.code_lens.show_cognitive_complexity)
    );
    let _ = writeln!(
        out,
        "  Cyclomatic complexity: {}",
        on_off(project_config.code_lens.show_cyclomatic_complexity)
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Formatting:");
    let _ = writeln!(out, "  Indent: {}", formatting_summary(&project_config.formatting));
    let _ = writeln!(out);
    let _ = writeln!(out, "Search baseline:");
    append_baseline_summary(&mut out, "Workspace", &baseline_diagnostics.workspace);
    append_baseline_summary(&mut out, "Reference", &baseline_diagnostics.reference);
    out
}

fn append_baseline_summary(
    out: &mut String,
    label: &str,
    summary: &mcp_server::BaselineResolutionSummary,
) {
    let _ = writeln!(out, "  {label}:");
    let _ = writeln!(out, "    Backend: {}", summary.backend);
    let _ = writeln!(out, "    Select:  {}", summary.selection);
    let _ = writeln!(out, "    Status:  {}", summary.issue.as_deref().unwrap_or("ready"));
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn formatting_summary(config: &project_model::FormattingConfig) -> String {
    if config.indent_size == 0 && !config.use_tabs {
        "not configured".to_owned()
    } else if config.use_tabs {
        format!("tabs x{}", config.indent_size)
    } else {
        format!("spaces x{}", config.indent_size)
    }
}

fn summarize_diagnostic_codes(codes: impl Iterator<Item = String>) -> String {
    let mut codes: Vec<String> = codes.collect();
    if codes.is_empty() {
        return "none".to_owned();
    }

    codes.sort();
    const LIMIT: usize = 8;
    if codes.len() <= LIMIT {
        return codes.join(", ");
    }

    let remaining = codes.len() - LIMIT;
    format!("{}, … (+{remaining} more)", codes[..LIMIT].join(", "))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        build_check_config_report, check_config, diagnostics_config_from_project,
        inspect_diagnostics_baseline, SourceProvider, SourceProviders, SourceSetArgs,
    };

    #[test]
    fn check_config_accepts_toml_project_config() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("bsl-analyzer.toml");
        fs::write(
            &config,
            r#"
[source]
root = "src/cf"

[search.baseline]
backend = "postgres"

[search.baseline.postgres]
host = "127.0.0.1"
port = 1
dbname = "bsl_search"
schema = "bsl_search"
vault_role_base = "prod/search/bsl-analyzer"

[search.baseline.postgres.credential_helper]
program = "python3"
args = [
  "-c",
  "import sys; sys.stdin.readline(); sys.stdout.write(sys.argv[1])",
  '{"protocol":"bsl-analyzer.postgres-helper.v1","ok":true,"url":"postgres://user:pass@127.0.0.1:1/bsl_search"}'
]

[search.baseline.workspace_code.policy]
publish_branches = ["develop"]

[[search.baseline.workspace_code.policy.branches]]
match = "*"
select_branch = "develop"
"#,
        )
        .unwrap();

        check_config(config, SourceSetArgs::default()).unwrap();
    }

    #[test]
    fn check_config_accepts_helper_based_json_project_config() {
        let dir = tempdir().unwrap();
        let config = dir.path().join(".bsl-analyzer.json");
        fs::write(
            &config,
            r#"{
                "search": {
                    "baseline": {
                        "backend": "postgres",
                        "postgres": {
                            "host": "127.0.0.1",
                            "port": 1,
                            "dbname": "bsl_search",
                            "schema": "bsl_search",
                            "vaultRoleBase": "prod/search/bsl-analyzer",
                            "credentialHelper": {
                                "program": "python3",
                                "args": [
                                    "-c",
                                    "import sys; sys.stdin.readline(); sys.stdout.write(sys.argv[1])",
                                    "{\"protocol\":\"bsl-analyzer.postgres-helper.v1\",\"ok\":true,\"url\":\"postgres://user:pass@127.0.0.1:1/bsl_search\"}"
                                ]
                            }
                        },
                        "workspaceCode": {
                            "policy": {
                                "publishBranches": ["develop"],
                                "branches": [
                                    { "match": "*", "selectBranch": "develop" }
                                ]
                            }
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        check_config(config, SourceSetArgs::default()).unwrap();
    }

    #[test]
    fn check_config_rejects_invalid_toml() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("bsl-analyzer.toml");
        fs::write(&config, "invalid {{{ toml").unwrap();

        let error = check_config(config, SourceSetArgs::default()).unwrap_err();

        assert!(error.to_string().contains("failed to parse configuration file"));
    }

    #[test]
    fn check_config_rejects_invalid_postgres_baseline_config() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("bsl-analyzer.toml");
        fs::write(
            &config,
            r#"
[search.baseline]
backend = "postgres"
"#,
        )
        .unwrap();

        let error = check_config(config, SourceSetArgs::default()).unwrap_err();

        assert!(error.to_string().contains("search baseline diagnostics reported errors"));
    }

    #[test]
    fn diagnostics_section_must_be_object_when_present() {
        let error = serde_json::from_str::<project_model::ProjectConfig>(
            r#"{
                "diagnostics": []
            }"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid type"));
    }

    #[test]
    fn check_config_baseline_reports_missing_file() {
        let dir = tempdir().unwrap();
        let project = baseline_project(dir.path());
        let summary = inspect_diagnostics_baseline(&project);
        assert!(summary.issue);
        assert!(summary.status.contains("file is missing"), "{}", summary.status);
    }

    #[test]
    fn check_config_baseline_reports_unsupported_schema() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("baseline.json"),
            r#"{"schema_version":99,"scope":{"source_root":"","extensions":[]},"diagnostics":[]}"#,
        )
        .unwrap();
        let summary = inspect_diagnostics_baseline(&baseline_project(dir.path()));
        assert!(summary.issue);
        assert!(
            summary.status.contains("unsupported diagnostics baseline schema version 99"),
            "{}",
            summary.status
        );
    }

    #[test]
    fn check_config_baseline_reports_corrupt_file_alongside_search_baseline() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("baseline.json"), "{").unwrap();
        let project = baseline_project(dir.path());
        let summary = inspect_diagnostics_baseline(&project);
        assert!(summary.issue);
        assert!(summary.status.contains("invalid file"), "{}", summary.status);

        let project_config = project.as_ref().unwrap().config.clone();
        let search =
            mcp_server::resolve_project_baseline_diagnostics(Some(dir.path()), &project_config);
        let report = build_check_config_report(
            &dir.path().join("bsl-analyzer.toml"),
            &project_config,
            &project,
            &diagnostics_config_from_project(&project_config).unwrap(),
            &search,
            &summary,
            &SourceProviders {
                configuration_root: SourceProvider::ConfigFile,
                extensions: SourceProvider::ConfigFile,
                externals: SourceProvider::AutoDiscovery,
            },
        );
        assert!(report.contains("Baseline:   ERROR: invalid file"));
        assert!(report.contains("Search baseline:"));
    }

    fn baseline_project(
        root: &std::path::Path,
    ) -> Result<project_model::Project, project_model::ProjectError> {
        let config: project_model::ProjectConfig = serde_json::from_str(
            r#"{"diagnostics":{"baseline":{"path":"baseline.json"}},"extensions":[]}"#,
        )
        .unwrap();
        project_model::Project::with_config(root, config)
    }

    fn configuration_dir(root: &std::path::Path, rel: &str, extension: bool) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        let purpose = if extension {
            "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
        } else {
            ""
        };
        std::fs::write(
            dir.join("Configuration.xml"),
            format!(
                "<MetaDataObject><Configuration><Properties>\
                 <ConfigurationExtensionCompatibilityMode>8.3.21</ConfigurationExtensionCompatibilityMode>\
                 {purpose}</Properties></Configuration></MetaDataObject>"
            ),
        )
        .unwrap();
    }

    fn report_for(root: &std::path::Path, configuration_root: &str) -> String {
        let project_config = project_model::ProjectConfig {
            configuration_root: Some(configuration_root.to_string()),
            extensions: Some(Vec::new()),
            ..Default::default()
        };
        let project = project_model::Project::with_config(root, project_config.clone());
        let diagnostics_baseline = inspect_diagnostics_baseline(&project);
        build_check_config_report(
            &root.join("bsl-analyzer.toml"),
            &project_config,
            &project,
            &diagnostics_config_from_project(&project_config).unwrap(),
            &mcp_server::resolve_project_baseline_diagnostics(Some(root), &project_config),
            &diagnostics_baseline,
            &SourceProviders {
                configuration_root: SourceProvider::Cli,
                extensions: SourceProvider::Cli,
                externals: SourceProvider::AutoDiscovery,
            },
        )
    }

    #[test]
    fn report_lists_declared_externals_with_their_narrowing() {
        let dir = tempfile::tempdir().unwrap();
        configuration_dir(dir.path(), "cf", false);
        let mut project_config = project_model::ProjectConfig {
            configuration_root: Some("cf".to_string()),
            extensions: Some(Vec::new()),
            ..Default::default()
        };
        project_config.externals = Some(vec![
            project_model::ExternalDecl { name: "A".into(), path: "a".into(), depends_on: None },
            project_model::ExternalDecl {
                name: "B".into(),
                path: "b".into(),
                depends_on: Some(vec!["X".into(), "Y".into()]),
            },
            project_model::ExternalDecl {
                name: "C".into(),
                path: "c".into(),
                depends_on: Some(vec![]),
            },
        ]);
        let project = project_model::Project::with_config(dir.path(), project_config.clone());
        let diagnostics_baseline = inspect_diagnostics_baseline(&project);
        let report = build_check_config_report(
            &dir.path().join("bsl-analyzer.toml"),
            &project_config,
            &project,
            &diagnostics_config_from_project(&project_config).unwrap(),
            &mcp_server::resolve_project_baseline_diagnostics(Some(dir.path()), &project_config),
            &diagnostics_baseline,
            &SourceProviders {
                configuration_root: SourceProvider::Cli,
                extensions: SourceProvider::Cli,
                externals: SourceProvider::ConfigFile,
            },
        );
        assert!(
            report.contains("Externals:   A (a), B (b, dependsOn: X, Y), C (c, dependsOn: none)"),
            "{report}"
        );
        project_config.externals = Some(Vec::new());
        let project = project_model::Project::with_config(dir.path(), project_config.clone());
        let diagnostics_baseline = inspect_diagnostics_baseline(&project);
        let report = build_check_config_report(
            &dir.path().join("bsl-analyzer.toml"),
            &project_config,
            &project,
            &diagnostics_config_from_project(&project_config).unwrap(),
            &mcp_server::resolve_project_baseline_diagnostics(Some(dir.path()), &project_config),
            &diagnostics_baseline,
            &SourceProviders {
                configuration_root: SourceProvider::Cli,
                extensions: SourceProvider::Cli,
                externals: SourceProvider::Cli,
            },
        );
        assert!(report.contains("Externals:   none [from: cli]"), "{report}");
    }

    #[test]
    fn report_warns_only_when_the_root_is_itself_an_extension() {
        let dir = tempfile::tempdir().unwrap();
        configuration_dir(dir.path(), "cf", false);
        configuration_dir(dir.path(), "cfe", true);

        assert!(
            report_for(dir.path(), "cfe").contains("is a configuration extension analyzed without"),
            "an extension taken as the main root must say so"
        );
        assert!(
            !report_for(dir.path(), "cf").contains("is a configuration extension analyzed without"),
            "a main configuration carries the compatibility-mode element too and must stay silent"
        );
    }

    #[test]
    fn check_config_report_contains_project_and_diagnostics_summary() {
        let project_config: project_model::ProjectConfig = serde_json::from_str(
            r#"{
                "configurationRoot": "src/cf",
                "extensions": ["src/cfe/ExtA"],
                "analysis": { "diffBase": "vendor", "ignoredAuthors": ["Фирма 1С"] },
                "formatting": { "use_tabs": true, "indent_size": 1 },
                "codeLens": {
                    "showCognitiveComplexity": true,
                    "showCyclomaticComplexity": false
                },
                "diagnostics": {
                    "ordinaryAppSupport": true,
                    "dataflowMaxIterations": 20000,
                    "parameters": {
                        "CommentedCode": false,
                        "BadWords": true,
                        "CyclomaticComplexity": { "complexityThreshold": 15 }
                    }
                },
                "search": {
                    "baseline": {
                        "backend": "postgres",
                        "workspaceCode": {
                            "policy": {
                                "publishBranches": ["develop"],
                                "branches": [
                                    { "match": "*", "selectBranch": "develop" }
                                ]
                            }
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        let diag_config = diagnostics_config_from_project(&project_config).unwrap();
        let baseline = mcp_server::resolve_project_baseline_diagnostics(
            Some(std::path::Path::new(".")),
            &project_config,
        );

        let project =
            project_model::Project::with_config(std::env::temp_dir(), project_config.clone());
        let diagnostics_baseline = inspect_diagnostics_baseline(&project);
        let report = build_check_config_report(
            std::path::Path::new("bsl-analyzer.toml"),
            &project_config,
            &project,
            &diag_config,
            &baseline,
            &diagnostics_baseline,
            &SourceProviders {
                configuration_root: SourceProvider::ConfigFile,
                extensions: SourceProvider::ConfigFile,
                externals: SourceProvider::AutoDiscovery,
            },
        );

        assert!(report.contains("Configuration is invalid."));
        assert!(report.contains("Project:"));
        assert!(report.contains("Source root: src/cf"));
        assert!(report.contains("Extensions:  src/cfe/ExtA"));
        assert!(
            report.contains(
                "Externals:   auto-discovery (src/epf/*, src/erf/*) [from: auto-discovery]"
            ),
            "{report}"
        );
        assert!(report.contains("Diff base:   vendor"));
        assert!(report.contains("Ignored authors: Фирма 1С"));
        assert!(report.contains("ordinaryAppSupport: true"));
        assert!(report.contains("dataflowMaxIterations: 20000"));
        assert!(report.contains("Disabled codes: CommentedCode"));
        assert!(report.contains("Explicitly enabled: BadWords"));
        assert!(report.contains("Parameterized: CyclomaticComplexity"));
        assert!(report.contains("Code lens:"));
        assert!(report.contains("Cognitive complexity: on"));
        assert!(report.contains("Formatting:"));
        assert!(report.contains("Indent: tabs x1"));
        assert!(report.contains("Search baseline:"));
    }
}
