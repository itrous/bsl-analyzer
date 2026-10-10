use ide_db::RootDatabaseImpl;
use project_model::{FeaturesConfig, ProjectConfig};
use std::sync::Arc;

use crate::global_state::GlobalState;

impl GlobalState {
    pub fn update_features_config(&mut self) {
        let config =
            self.project.as_ref().map(|project| project.config.clone()).unwrap_or_default();
        let db = self.analysis_host.raw_database_mut();
        apply_project_config_to_db(db, &config);
        apply_compatibility_mode_to_db(db, self.project.as_ref());
        if let Some(project) = self.project.as_ref() {
            if let Some(warning) =
                platform_help::source_change_warning(Some(&project.config), &project.root)
            {
                tracing::warn!("{warning}");
                self.show_warning_message(warning);
            }
        }
    }
}

#[cfg(test)]
fn resolve_features(project: Option<&project_model::Project>) -> FeaturesConfig {
    project.map(|p| p.config.features.clone()).unwrap_or_default()
}

pub fn apply_features_to_db(db: &mut RootDatabaseImpl, features: &FeaturesConfig) {
    tracing::info!(type_narrowing = features.type_narrowing, "updated feature flags");
    db.set_type_narrowing_enabled(features.type_narrowing);
    db.set_env_options(env_options_from_features(features));
}

/// Apply every project-level semantic input shared by LSP and batch analysis.
pub fn apply_project_config_to_db(db: &mut RootDatabaseImpl, config: &ProjectConfig) {
    apply_features_to_db(db, &config.features);
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
    tracing::info!(target_platform_version = ?target, "updated platform target");
    db.set_target_platform_version(target);
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
    use super::{apply_features_to_db, apply_project_config_to_db, resolve_features};
    use ide_db::RootDatabaseImpl;
    use project_model::{FeaturesConfig, Project, ProjectConfig};
    use std::fs;
    use tempfile::tempdir;

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
}
