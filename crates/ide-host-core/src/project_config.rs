//! Project-level semantic inputs (`bsl-analyzer.toml` settings and the resolved
//! compatibility mode) applied to a [`RootDatabaseImpl`].
//!
//! One implementation for every host: the LSP server, CLI `analyze` and the MCP
//! resident must analyse a workspace under the same effective configuration, or
//! the same file gets different verdicts depending on who asks.

use std::sync::Arc;

use ide_db::RootDatabaseImpl;
use project_model::{FeaturesConfig, ProjectConfig};

pub fn apply_features_to_db(db: &mut RootDatabaseImpl, features: &FeaturesConfig) {
    set_type_narrowing(db, features.type_narrowing);
    set_env_options(db, env_options_from_features(features));
}

/// [`apply_project_config_to_db`] and then [`apply_configuration_xml_settings_to_db`],
/// in one step that writes each input once and only when its value changes.
///
/// A write sends every query that read the input back to recompute, even when it
/// stores the value already there. The two steps in sequence rebuild the environment
/// options from the settings and only then put back what `Configuration.xml` states,
/// so re-applying an unchanged project rewrote them three times. A host that applies
/// its project again on every workspace publication or opened document threw away
/// the inference it had already warmed.
pub fn apply_project_settings_to_db(
    db: &mut RootDatabaseImpl,
    config: &ProjectConfig,
    project: Option<&project_model::Project>,
) {
    set_type_narrowing(db, config.features.type_narrowing);
    let mut options = env_options_from_features(&config.features);
    options.ordinary_forms_in_managed_app = crate::ordinary_forms_in_managed_app(project);
    options.ordinary_app_support = ordinary_app_support(project);
    options.managed_run_mode = crate::managed_run_mode(project);
    set_env_options(db, options);
    apply_platform_versions_to_db(db, config);
    apply_compatibility_mode_to_db(db, project);
}

fn set_type_narrowing(db: &mut RootDatabaseImpl, enabled: bool) {
    if db.type_narrowing_enabled() != enabled {
        tracing::info!(type_narrowing = enabled, "updated feature flags");
        db.set_type_narrowing_enabled(enabled);
    }
}

fn set_env_options(db: &mut RootDatabaseImpl, options: hir::execution_env::EnvOptions) {
    if db.env_options() != options {
        tracing::info!(?options, "updated environment options");
        db.set_env_options(options);
    }
}

/// The `bsl-analyzer.toml` settings shared by LSP, MCP and batch analysis.
///
/// Rebuilds the execution model from `[features]` rather than merging into it, so
/// it runs before [`apply_configuration_xml_settings_to_db`]: what `Configuration.xml`
/// states would otherwise be reset to defaults. [`apply_project_to_db`] keeps that
/// order.
pub fn apply_project_config_to_db(db: &mut RootDatabaseImpl, config: &ProjectConfig) {
    apply_features_to_db(db, &config.features);
    apply_platform_versions_to_db(db, config);
}

fn apply_platform_versions_to_db(db: &mut RootDatabaseImpl, config: &ProjectConfig) {
    let target = config.target_platform_version.as_deref().map(Arc::<str>::from);
    let target_changed = db.target_platform_version().as_deref() != target.as_deref();
    let catalog = bsl_platform::PlatformGlobalCatalog::instance();
    if target_changed
        && catalog.status_for_target(target.as_deref())
            == bsl_platform::PlatformCatalogStatus::UnsupportedTarget
    {
        tracing::warn!(
            target_platform_version = ?target,
            bundled_catalog_version = ?catalog.metadata().map(|metadata| metadata.platform_version),
            "bundled platform-global catalog does not cover the configured target; absence diagnostics are suppressed"
        );
    }
    if target_changed {
        tracing::info!(target_platform_version = ?target, "updated platform target");
        db.set_target_platform_version(target);
    }
    let min = config.min_platform_version.as_deref().map(Arc::<str>::from);
    if min
        .as_deref()
        .is_some_and(|value| bsl_platform::PlatformVersion::parse_release(value).is_none())
    {
        tracing::warn!(
            min_platform_version = ?min,
            "`min_platform_version` is not a numeric platform version; the min-platform check stays off"
        );
    }
    if db.min_platform_version().as_deref() != min.as_deref() {
        tracing::info!(min_platform_version = ?min, "updated minimum platform");
        db.set_min_platform_version(min);
    }
}

/// The compatibility mode of `project` ([`project_model::Project::compatibility_mode`]:
/// the setting, else the main configuration's `Configuration.xml`) into the
/// database. Kept apart from [`apply_project_config_to_db`] because it needs the
/// project's files, not just its settings.
pub fn apply_compatibility_mode_to_db(
    db: &mut RootDatabaseImpl,
    project: Option<&project_model::Project>,
) {
    let resolved = project.and_then(|project| project.compatibility_mode());
    if let Some((value, source)) = &resolved {
        if bsl_platform::PlatformVersion::parse_compatibility_mode(value).is_none() {
            tracing::warn!(
                compatibility_mode = %value,
                ?source,
                "compatibility mode is not a recognized value; the compatibility-mode check stays off"
            );
        }
    }
    let mode = resolved.map(|(value, _)| Arc::<str>::from(value));
    if db.compatibility_mode().as_deref() != mode.as_deref() {
        tracing::info!(compatibility_mode = ?mode, "updated compatibility mode");
        db.set_compatibility_mode(mode);
    }
}

/// The execution model's ordinary-application switch from the same effective value
/// the diagnostics rules get ([`crate::effective_ordinary_app_support`]: the
/// `[diagnostics] ordinaryAppSupport` setting, else `<DefaultRunMode>` of the main
/// configuration). Naming the legacy thick client in `checked_environments` still
/// turns it on regardless: an environment that is checked must be modelled.
///
/// Recomputed from the features rather than only ever switched on, so a
/// configuration switched back to the managed application turns the flag off.
/// Applied after [`apply_project_config_to_db`] and again when `Configuration.xml`
/// changes, like [`crate::apply_ordinary_forms_setting`].
pub fn apply_ordinary_app_support_to_db(
    db: &mut RootDatabaseImpl,
    project: Option<&project_model::Project>,
) {
    let value = ordinary_app_support(project);
    let mut options = db.env_options();
    if options.ordinary_app_support != value {
        tracing::info!(
            ordinary_app_support = value,
            source = ?project.and_then(|project| project.ordinary_app_support()).map(|(_, source)| source),
            "updated ordinary-application support"
        );
        options.ordinary_app_support = value;
        db.set_env_options(options);
    }
}

fn ordinary_app_support(project: Option<&project_model::Project>) -> bool {
    use hir::execution_env::EnvFlags;
    let checked_by_name = project
        .and_then(|project| project.config.features.checked_environments.as_deref())
        .unwrap_or_default()
        .iter()
        .filter_map(|name| EnvFlags::from_config_name(name))
        .any(|flag| flag.contains(EnvFlags::THICK_CLIENT_ORDINARY));
    checked_by_name || project.is_some_and(crate::effective_ordinary_app_support)
}

/// The settings the main configuration's `Configuration.xml` states, read into the
/// database: the compatibility mode, whether ordinary forms run in the managed thick
/// client, the ordinary-application support its default run mode implies and whether
/// that run mode is the managed application. Applied after
/// [`apply_project_config_to_db`] and again whenever `Configuration.xml` changes.
pub fn apply_configuration_xml_settings_to_db(
    db: &mut RootDatabaseImpl,
    project: Option<&project_model::Project>,
) {
    apply_compatibility_mode_to_db(db, project);
    crate::apply_ordinary_forms_setting(db, project);
    apply_ordinary_app_support_to_db(db, project);
    crate::apply_default_run_mode(db, project);
}

/// The main configuration's vendor support map into the database, read only when
/// the project's `[diagnostics]` asks for it (see [`ide::load_vendor_support`]).
/// Kept apart from [`apply_project_config_to_db`] because it reads the project's
/// files, not just its settings.
pub fn apply_vendor_support_to_db(
    db: &mut RootDatabaseImpl,
    project: Option<&project_model::Project>,
) {
    let support = project.and_then(|project| {
        ide::load_vendor_support(
            &project.config.diagnostics.rules_json(),
            project.configuration_path(),
        )
    });
    if db.vendor_support() != support {
        db.set_vendor_support(support);
    }
}

/// Every project-level semantic input of `project`: its `bsl-analyzer.toml` settings,
/// what its `Configuration.xml` states and the vendor support map. CLI `analyze` and
/// the MCP resident build their database through this; the LSP server applies the
/// same three steps.
pub fn apply_project_to_db(db: &mut RootDatabaseImpl, project: &project_model::Project) {
    apply_project_settings_to_db(db, &project.config, Some(project));
    apply_vendor_support_to_db(db, Some(project));
}

/// The availability diagnostics report only environments the project actually
/// targets: `[features] checked_environments` lists them by preprocessor-style
/// names; an unrecognized name is skipped with a warning rather than silently
/// changing the set.
fn env_options_from_features(features: &FeaturesConfig) -> hir::execution_env::EnvOptions {
    use hir::execution_env::{EnvFlags, EnvOptions};
    let mut options = EnvOptions::default();
    if let Some(names) = &features.checked_environments {
        let mut checked = EnvFlags::EMPTY;
        for name in names {
            match EnvFlags::from_config_name(name) {
                Some(flag) => checked = checked | flag,
                None => tracing::warn!(
                    name,
                    "unrecognized environment in `checked_environments`; expected a \
                     preprocessor-style name such as ВебКлиент/WebClient"
                ),
            }
        }
        // A non-empty list where nothing parsed is a typo, not an opt-out —
        // silently disabling both availability diagnostics would hide the
        // mistake. An explicit `[]` remains the documented opt-out.
        if checked.is_empty() && !names.is_empty() {
            tracing::warn!(
                "`checked_environments` names no recognized environment; keeping the default set"
            );
        } else {
            options.checked_environments = checked;
            // An opted-in environment must also enter the execution model,
            // or the checked mask would never intersect a body's set: the
            // mobile client is not in the default client environments, and
            // the legacy thick client is gated by ordinary-app support.
            if checked.contains(EnvFlags::MOBILE_CLIENT) {
                options.client_environments = options.client_environments | EnvFlags::MOBILE_CLIENT;
            }
            if checked.contains(EnvFlags::THICK_CLIENT_ORDINARY) {
                options.ordinary_app_support = true;
            }
        }
    }
    options
}

#[cfg(test)]
mod tests {
    use super::{apply_features_to_db, apply_project_config_to_db};
    use ide_db::RootDatabaseImpl;
    use project_model::{FeaturesConfig, Project, ProjectConfig};
    use std::fs;
    use tempfile::tempdir;

    fn resolve_features(project: Option<&Project>) -> FeaturesConfig {
        project.map(|p| p.config.features.clone()).unwrap_or_default()
    }

    #[test]
    fn resolve_features_returns_defaults_without_project() {
        let features = resolve_features(None);
        assert!(
            features.type_narrowing,
            "missing project must fall back to `FeaturesConfig::default` (narrowing on)"
        );
    }

    #[test]
    fn apply_features_propagates_disable_to_database() {
        let mut db = RootDatabaseImpl::new();
        assert!(db.type_narrowing_enabled(), "fresh database defaults to narrowing on");

        let disabled = FeaturesConfig { type_narrowing: false, ..FeaturesConfig::default() };
        apply_features_to_db(&mut db, &disabled);
        assert!(!db.type_narrowing_enabled(), "apply must flip the Salsa input to false");

        let enabled = FeaturesConfig::default();
        apply_features_to_db(&mut db, &enabled);
        assert!(db.type_narrowing_enabled(), "apply must flip the Salsa input back to true");
    }

    #[test]
    fn update_features_pipeline_threads_toml_flag_end_to_end() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("bsl-analyzer.toml"),
            r#"
[features]
type_narrowing = false
"#,
        )
        .unwrap();

        let project = Project::new(dir.path()).expect("valid test project");
        assert!(
            !project.config.features.type_narrowing,
            "Project::new must surface the disabled flag from the TOML"
        );

        let mut db = RootDatabaseImpl::new();
        apply_features_to_db(&mut db, &resolve_features(Some(&project)));
        assert!(
            !db.type_narrowing_enabled(),
            "full pipeline must land the disabled flag on the Salsa input"
        );

        let default_project_config = ProjectConfig::default();
        assert!(default_project_config.features.type_narrowing);
        apply_features_to_db(&mut db, &default_project_config.features);
        assert!(db.type_narrowing_enabled());
    }

    #[test]
    fn target_platform_version_threads_to_database() {
        let mut db = RootDatabaseImpl::new();
        assert!(db.target_platform_version().is_none());

        let config = ProjectConfig {
            target_platform_version: Some("8.3.27.1644".to_string()),
            ..ProjectConfig::default()
        };
        apply_project_config_to_db(&mut db, &config);
        assert_eq!(db.target_platform_version().as_deref(), Some("8.3.27.1644"));

        apply_project_config_to_db(&mut db, &ProjectConfig::default());
        assert!(db.target_platform_version().is_none());
    }

    #[test]
    fn min_platform_version_threads_to_database_apart_from_the_target() {
        let mut db = RootDatabaseImpl::new();
        assert!(db.min_platform_version().is_none());

        let config = ProjectConfig {
            min_platform_version: Some("8.3.17".to_string()),
            ..ProjectConfig::default()
        };
        apply_project_config_to_db(&mut db, &config);
        assert_eq!(db.min_platform_version().as_deref(), Some("8.3.17"));
        assert!(db.target_platform_version().is_none(), "the floor must not set the target");

        apply_project_config_to_db(&mut db, &ProjectConfig::default());
        assert!(db.min_platform_version().is_none());
    }

    #[test]
    fn checked_environments_thread_from_toml_to_env_options() {
        use hir::execution_env::EnvFlags;

        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("bsl-analyzer.toml"),
            r#"
[features]
checked_environments = ["ТонкийКлиент", "Server", "НеизвестнаяСреда"]
"#,
        )
        .unwrap();

        let project = Project::new(dir.path()).expect("valid test project");
        let mut db = RootDatabaseImpl::new();
        apply_features_to_db(&mut db, &resolve_features(Some(&project)));
        // The unknown name is skipped with a warning; the rest form the mask.
        assert_eq!(db.env_options().checked_environments, EnvFlags::THIN_CLIENT | EnvFlags::SERVER);

        apply_features_to_db(&mut db, &FeaturesConfig::default());
        assert_eq!(
            db.env_options().checked_environments,
            hir::execution_env::EnvOptions::default().checked_environments,
            "omitting the setting must restore the default checked set"
        );
    }

    #[test]
    fn opted_in_environments_enter_the_execution_model() {
        use hir::execution_env::EnvFlags;

        let mut db = RootDatabaseImpl::new();
        let features = FeaturesConfig {
            checked_environments: Some(vec![
                "МобильныйКлиент".to_string(),
                "ТолстыйКлиентОбычноеПриложение".to_string(),
            ]),
            ..FeaturesConfig::default()
        };
        apply_features_to_db(&mut db, &features);
        let options = db.env_options();
        assert!(
            options.client_environments.contains(EnvFlags::MOBILE_CLIENT),
            "checking the mobile client requires it in the client environments"
        );
        assert!(
            options.ordinary_app_support,
            "checking the ordinary thick client requires ordinary-app support"
        );
    }

    /// Собирательные имена конфигурация принимает, и их последствие названо
    /// целиком.
    ///
    /// `Клиент` — управляемые клиенты, поэтому мобильный клиент входит в
    /// модель исполнения, а устаревший толстый клиент обычного приложения не
    /// включается: он входит только собственным именем. До сведения списков
    /// это имя давало предупреждение и умолчания.
    #[test]
    fn aggregate_client_name_is_accepted_and_its_consequence_is_pinned() {
        use hir::execution_env::EnvFlags;

        let mut db = RootDatabaseImpl::new();
        let features = FeaturesConfig {
            checked_environments: Some(vec!["Клиент".to_string()]),
            ..FeaturesConfig::default()
        };
        apply_features_to_db(&mut db, &features);
        let options = db.env_options();
        assert_eq!(
            options.checked_environments,
            EnvFlags::MANAGED_CLIENTS,
            "`Клиент` обязан назвать управляемые клиенты"
        );
        assert!(
            options.client_environments.contains(EnvFlags::MOBILE_CLIENT),
            "названный клиент обязан войти в модель исполнения"
        );
        assert!(
            !options.ordinary_app_support,
            "`Клиент` не включает устаревший толстый клиент обычного приложения"
        );

        // `НаСервере` — то же имя среды, что `Сервер`; до сведения списков
        // оно уходило в предупреждение.
        let features = FeaturesConfig {
            checked_environments: Some(vec!["НаСервере".to_string()]),
            ..FeaturesConfig::default()
        };
        apply_features_to_db(&mut db, &features);
        assert_eq!(db.env_options().checked_environments, EnvFlags::SERVER);
    }

    #[test]
    fn unrecognized_only_list_keeps_the_default_checked_set() {
        let mut db = RootDatabaseImpl::new();
        let features = FeaturesConfig {
            checked_environments: Some(vec!["Sever".to_string()]),
            ..FeaturesConfig::default()
        };
        apply_features_to_db(&mut db, &features);
        assert_eq!(
            db.env_options().checked_environments,
            hir::execution_env::EnvOptions::default().checked_environments,
            "a typo-only list must not silently disable the availability diagnostics"
        );

        let explicit_off =
            FeaturesConfig { checked_environments: Some(vec![]), ..FeaturesConfig::default() };
        apply_features_to_db(&mut db, &explicit_off);
        assert!(
            db.env_options().checked_environments.is_empty(),
            "an explicit empty list is the documented opt-out"
        );
    }

    /// The execution model takes the same effective `ordinaryAppSupport` as the
    /// diagnostics: derived from `DefaultRunMode` when unset, the setting otherwise,
    /// switched off again when the derivation flips, and forced on by naming the
    /// legacy thick client in `checked_environments`.
    #[test]
    fn ordinary_app_support_reaches_the_execution_model() {
        use super::apply_ordinary_app_support_to_db;

        fn project_with(run_mode: &str, toml: &str) -> (tempfile::TempDir, Project) {
            let dir = tempdir().unwrap();
            let cf = dir.path().join("src").join("cf");
            fs::create_dir_all(&cf).unwrap();
            fs::write(
                cf.join("Configuration.xml"),
                format!(
                    "<MetaDataObject><Configuration><Properties><DefaultRunMode>{run_mode}\
                     </DefaultRunMode><UseOrdinaryFormInManagedApplication>false\
                     </UseOrdinaryFormInManagedApplication></Properties></Configuration>\
                     </MetaDataObject>"
                ),
            )
            .unwrap();
            fs::write(dir.path().join("bsl-analyzer.toml"), toml).unwrap();
            let project = Project::new(dir.path()).unwrap();
            (dir, project)
        }
        fn apply(db: &mut RootDatabaseImpl, project: &Project) {
            apply_project_config_to_db(db, &project.config);
            crate::apply_ordinary_forms_setting(db, Some(project));
            apply_ordinary_app_support_to_db(db, Some(project));
        }

        let mut db = RootDatabaseImpl::new();
        let (_ordinary, project) = project_with("OrdinaryApplication", "");
        apply(&mut db, &project);
        assert!(db.env_options().ordinary_app_support, "derived from OrdinaryApplication");
        assert_eq!(
            db.env_options().ordinary_forms_in_managed_app,
            Some(false),
            "the ordinary-form setting is kept"
        );

        let (_managed, project) = project_with("ManagedApplication", "");
        apply(&mut db, &project);
        assert!(!db.env_options().ordinary_app_support, "the managed mode turns it back off");

        let (_explicit, project) =
            project_with("OrdinaryApplication", "[diagnostics]\nordinaryAppSupport = false\n");
        apply(&mut db, &project);
        assert!(!db.env_options().ordinary_app_support, "an explicit false wins");

        let (_explicit, project) =
            project_with("ManagedApplication", "[diagnostics]\nordinaryAppSupport = true\n");
        apply(&mut db, &project);
        assert!(db.env_options().ordinary_app_support, "an explicit true wins");

        let (_checked, project) = project_with(
            "ManagedApplication",
            "[features]\nchecked_environments = [\"ТолстыйКлиентОбычноеПриложение\"]\n",
        );
        apply(&mut db, &project);
        assert!(db.env_options().ordinary_app_support, "a checked legacy client is modelled");

        // A section the diagnostics loader rejects falls back to its defaults as a
        // whole; the execution model follows the value the rules actually get.
        let (_invalid, project) =
            project_with("OrdinaryApplication", "[diagnostics]\ndataflowMaxIterations = \"abc\"\n");
        apply(&mut db, &project);
        assert!(!db.env_options().ordinary_app_support, "a rejected section derives nothing");
    }

    fn project_with_configuration(run_mode: &str, toml: &str) -> (tempfile::TempDir, Project) {
        let dir = tempdir().unwrap();
        let cf = dir.path().join("src").join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(
            cf.join("Configuration.xml"),
            format!(
                "<MetaDataObject><Configuration><Properties><DefaultRunMode>{run_mode}\
                 </DefaultRunMode><UseOrdinaryFormInManagedApplication>true\
                 </UseOrdinaryFormInManagedApplication><CompatibilityMode>8.3.20\
                 </CompatibilityMode></Properties></Configuration></MetaDataObject>"
            ),
        )
        .unwrap();
        fs::write(dir.path().join("bsl-analyzer.toml"), toml).unwrap();
        let project = Project::new(dir.path()).unwrap();
        (dir, project)
    }

    /// The one-step application stores what the two steps it replaces store, for
    /// settings that touch every input it writes.
    #[test]
    fn project_settings_store_what_the_two_steps_store() {
        use super::{apply_configuration_xml_settings_to_db, apply_project_settings_to_db};

        for (run_mode, toml) in [
            ("ManagedApplication", ""),
            ("OrdinaryApplication", "target_platform_version = \"8.3.24\"\n"),
            (
                "Auto",
                "min_platform_version = \"8.3.20\"\n[features]\ntype_narrowing = false\n\
                 checked_environments = [\"ТолстыйКлиентОбычноеПриложение\", \"МобильныйКлиент\"]\n",
            ),
            ("OrdinaryApplication", "[diagnostics]\nordinaryAppSupport = false\n"),
        ] {
            let (_dir, project) = project_with_configuration(run_mode, toml);
            let mut two_steps = RootDatabaseImpl::new();
            apply_project_config_to_db(&mut two_steps, &project.config);
            apply_configuration_xml_settings_to_db(&mut two_steps, Some(&project));
            let mut one_step = RootDatabaseImpl::new();
            apply_project_settings_to_db(&mut one_step, &project.config, Some(&project));

            assert_eq!(one_step.env_options(), two_steps.env_options(), "{run_mode} {toml}");
            assert_eq!(one_step.type_narrowing_enabled(), two_steps.type_narrowing_enabled());
            assert_eq!(one_step.target_platform_version(), two_steps.target_platform_version());
            assert_eq!(one_step.min_platform_version(), two_steps.min_platform_version());
            assert_eq!(one_step.compatibility_mode(), two_steps.compatibility_mode());
        }
    }

    /// Re-applying an unchanged project writes nothing: a write would send every query
    /// that read the input back to recompute. The project states run-mode settings in
    /// `Configuration.xml`, the values the settings step used to reset and put back.
    #[test]
    fn re_applying_an_unchanged_project_writes_nothing() {
        use super::apply_project_settings_to_db;

        let (_dir, project) = project_with_configuration(
            "OrdinaryApplication",
            "target_platform_version = \"8.3.24\"\n",
        );
        let mut db = RootDatabaseImpl::new();
        apply_project_settings_to_db(&mut db, &project.config, Some(&project));
        let applied = salsa::plumbing::current_revision(&db);

        apply_project_settings_to_db(&mut db, &project.config, Some(&project));

        assert_eq!(salsa::plumbing::current_revision(&db), applied);
    }

    /// `managed_run_mode` follows `<DefaultRunMode>` alone: `Auto` counts as the
    /// managed application, `OrdinaryApplication` and an unknown mode do not, and no
    /// `ordinaryAppSupport` setting changes it.
    #[test]
    fn default_run_mode_reaches_the_execution_model() {
        fn run_mode_of(configuration: Option<&str>, toml: &str) -> bool {
            let dir = tempdir().unwrap();
            let cf = dir.path().join("src").join("cf");
            fs::create_dir_all(&cf).unwrap();
            if let Some(run_mode) = configuration {
                fs::write(
                    cf.join("Configuration.xml"),
                    format!(
                        "<MetaDataObject><Configuration><Properties><DefaultRunMode>{run_mode}\
                         </DefaultRunMode></Properties></Configuration></MetaDataObject>"
                    ),
                )
                .unwrap();
            }
            fs::write(dir.path().join("bsl-analyzer.toml"), toml).unwrap();
            let project = Project::new(dir.path()).unwrap();
            let mut db = RootDatabaseImpl::new();
            apply_project_config_to_db(&mut db, &project.config);
            crate::apply_default_run_mode(&mut db, Some(&project));
            db.env_options().managed_run_mode
        }

        assert!(run_mode_of(Some("ManagedApplication"), ""));
        assert!(run_mode_of(Some("Auto"), ""));
        assert!(!run_mode_of(Some("OrdinaryApplication"), ""));
        assert!(!run_mode_of(None, ""), "no main configuration");
        assert!(!run_mode_of(Some("Managed"), ""), "an unknown mode is not known");
        assert!(!run_mode_of(Some("managedapplication"), ""), "the values are case-sensitive");
        assert!(
            run_mode_of(Some("ManagedApplication"), "[diagnostics]\nordinaryAppSupport = true\n"),
            "the setting does not change the run mode"
        );
        assert!(!run_mode_of(
            Some("OrdinaryApplication"),
            "[diagnostics]\nordinaryAppSupport = false\n"
        ));
    }
}
