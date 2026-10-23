//! Shared analysis host for every BSL consumer (LSP server, MCP server, CLI).
//!
//! Owns the Salsa [`RootDatabaseImpl`] and hands out [`Analysis`] snapshots. Keeping
//! this in one library crate (rather than re-implemented per binary) means the
//! memory model, LRU/durability tuning, and load discipline are shared: optimising
//! the host once benefits LSP, MCP, and CLI alike.

pub mod diagnostics_baseline;
mod load;
mod metadata;
mod project_config;
mod workspace;

pub use load::{
    apply_source_batch, build_source_root, register_files_disk_backed, set_file_text_source,
    BatchTextSource, FileTextSource, SourceBatchChange, SourceBatchEffects,
};
pub use metadata::{bootstrap_metadata_substrate, refresh_metadata_substrate, VfsWrite};
pub use project_config::{
    apply_compatibility_mode_to_db, apply_configuration_xml_settings_to_db, apply_features_to_db,
    apply_ordinary_app_support_to_db, apply_project_config_to_db, apply_project_settings_to_db,
    apply_project_to_db, apply_vendor_support_to_db,
};
pub use workspace::{
    CatalogFile, CatchUp, DiskFileState, EditorView, PublishedChange, SnapshotFile,
    SnapshotSourceRoot, WorkspaceBackend, WorkspaceCatalog, WorkspaceSnapshot,
};

use ide::{Analysis, RootDatabaseImpl};
use salsa::Database as _;

/// `UseOrdinaryFormInManagedApplication` of the main configuration of `project` into
/// the environment options: whether ordinary forms run in the managed thick client.
/// Applied after the options are built from the settings, and again when
/// `Configuration.xml` changes.
pub fn apply_ordinary_forms_setting(
    db: &mut RootDatabaseImpl,
    project: Option<&project_model::Project>,
) {
    let value = ordinary_forms_in_managed_app(project);
    let mut options = db.env_options();
    if options.ordinary_forms_in_managed_app != value {
        tracing::info!(ordinary_forms_in_managed_app = ?value, "updated ordinary-form environments");
        options.ordinary_forms_in_managed_app = value;
        db.set_env_options(options);
    }
}

fn ordinary_forms_in_managed_app(project: Option<&project_model::Project>) -> Option<bool> {
    project.and_then(|project| project.ordinary_forms_in_managed_app())
}

fn managed_run_mode(project: Option<&project_model::Project>) -> bool {
    let mode = project.and_then(project_model::Project::default_run_mode);
    matches!(mode.as_deref(), Some("ManagedApplication" | "Auto"))
}

/// `<DefaultRunMode>` of the main configuration of `project` into the environment
/// options: whether the configuration runs as a managed application
/// (`ManagedApplication` or `Auto`), so that the thick client of the managed
/// application compiles its object and manager modules. Applied after the options
/// are built from the settings, and again when `Configuration.xml` changes, like
/// [`apply_ordinary_forms_setting`].
pub fn apply_default_run_mode(db: &mut RootDatabaseImpl, project: Option<&project_model::Project>) {
    let mode = project.and_then(project_model::Project::default_run_mode);
    let value = managed_run_mode(project);
    let mut options = db.env_options();
    if options.managed_run_mode != value {
        tracing::info!(managed_run_mode = value, default_run_mode = ?mode, "updated run mode");
        options.managed_run_mode = value;
        db.set_env_options(options);
    }
}

/// The `ordinaryAppSupport` the diagnostics rules of `project` end up with: the
/// `[diagnostics]` setting, else the value derived from `<DefaultRunMode>`
/// ([`project_model::Project::diagnostics_rules_json`]). A section the diagnostics
/// loader rejects falls back to its defaults as a whole, so it gives `false` here too.
pub fn effective_ordinary_app_support(project: &project_model::Project) -> bool {
    serde_json::from_value::<ide::DiagnosticsConfig>(project.diagnostics_rules_json())
        .is_ok_and(|config| config.ordinary_app_support)
}

/// Owns the resident Salsa database and produces cheap [`Analysis`] snapshots over
/// cloned handles (the clone shares the Salsa storage / LRU cache).
#[derive(Default)]
pub struct AnalysisHost {
    db: RootDatabaseImpl,
}

impl AnalysisHost {
    pub fn from_database(db: RootDatabaseImpl) -> Self {
        Self { db }
    }

    pub fn analysis(&self) -> Analysis {
        Analysis::from_database(self.db.clone())
    }

    pub fn raw_database(&self) -> &RootDatabaseImpl {
        &self.db
    }

    pub fn raw_database_mut(&mut self) -> &mut RootDatabaseImpl {
        &mut self.db
    }

    pub fn into_database(self) -> RootDatabaseImpl {
        self.db
    }

    pub fn request_cancellation(&mut self) {
        const NOTABLE_MS: u64 = 50;
        const WARN_MS: u64 = 200;

        let start = std::time::Instant::now();
        self.db.trigger_cancellation();
        let elapsed_ms = start.elapsed().as_millis() as u64;
        if elapsed_ms >= WARN_MS {
            tracing::warn!(elapsed_ms, "trigger_cancellation slow — live Salsa snapshots");
        } else if elapsed_ms >= NOTABLE_MS {
            tracing::info!(elapsed_ms, "trigger_cancellation notable");
        } else {
            tracing::debug!(elapsed_ms, "trigger_cancellation");
        }
    }
}
