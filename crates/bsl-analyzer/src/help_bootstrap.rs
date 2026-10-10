//! Platform help selection for every command that analyzes or serves platform
//! docs. It runs before the first platform lookup, so the configured source —
//! not the build default — serves the whole process.

use std::path::Path;

use project_model::ProjectConfig;

/// Selects the platform help of the project at `root` (or of the explicit config
/// file). An unreadable config leaves the build default in place; the command
/// reports the config error itself when it loads the project.
pub fn bootstrap_for_root(root: &Path, explicit_config: Option<&Path>) {
    let config = match explicit_config {
        Some(path) => ProjectConfig::load_from_file(path).map(Some),
        None => ProjectConfig::load(root),
    };
    match config {
        Ok(config) => {
            platform_help::bootstrap(config.as_ref(), root);
        }
        Err(error) => {
            tracing::warn!(%error, "project config unreadable; platform help uses the default source");
            platform_help::bootstrap(None, root);
        }
    }
}

/// Selects the platform help from an already loaded project config.
pub fn bootstrap_for_config(config: &ProjectConfig, root: &Path) {
    platform_help::bootstrap(Some(config), root);
}
