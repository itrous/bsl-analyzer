use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use crate::mcp_install::error::InstallError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallTarget {
    Codex,
    Gemini,
    Claude,
    Cursor,
}

impl InstallTarget {
    pub const ALL: [Self; 4] = [Self::Codex, Self::Gemini, Self::Claude, Self::Cursor];
}

impl fmt::Display for InstallTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallTargetSelector {
    One(InstallTarget),
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallScope {
    User,
    Project,
    Local,
}

impl fmt::Display for InstallScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPreset {
    Workspace,
    Reference,
}

impl fmt::Display for InstallPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Workspace => "workspace",
            Self::Reference => "reference",
        };
        f.write_str(value)
    }
}

pub fn default_server_name(preset: InstallPreset) -> &'static str {
    match preset {
        InstallPreset::Workspace => "bsl-analyzer-workspace",
        InstallPreset::Reference => "bsl-analyzer-reference",
    }
}

#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub target: InstallTargetSelector,
    pub scope: InstallScope,
    pub preset: InstallPreset,
    pub name: String,
    pub project_dir: PathBuf,
    pub source_dir: PathBuf,
    pub onec_url: Option<String>,
    pub onec_user: String,
    pub onec_password: String,
    pub env: BTreeMap<String, String>,
    /// Opt-in tools the installed server is told to serve. Empty unless the caller asked:
    /// enabling one is a decision of whoever installs, never of a preset.
    pub enable_tools: Vec<String>,
    /// Rendered argv fragments (`["--cache-dir", "<abs>", "--configuration-root", "src/cf"]`)
    /// that shape the workspace the installed server serves. Empty unless asked for; the
    /// reference preset refuses them, because a reference server has no workspace.
    pub workspace_args: Vec<String>,
    pub force: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub(super) struct ServerSpec {
    pub(super) name: String,
    pub(super) command: String,
    pub(super) args: Vec<String>,
    pub(super) env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub(super) struct InstallAction {
    pub(super) target: InstallTarget,
    pub(super) scope: InstallScope,
    pub(super) spec: ServerSpec,
    pub(super) project_dir: PathBuf,
    pub(super) force: bool,
    pub(super) dry_run: bool,
}

#[derive(Debug, Clone)]
pub(super) struct InstallPlan {
    pub(super) actions: Vec<InstallAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ApplyDecision {
    Install,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStatus {
    Installed,
    Updated,
    DryRun,
}

#[derive(Debug, Clone)]
pub struct InstallEntryResult {
    pub target: InstallTarget,
    pub scope: InstallScope,
    pub status: InstallStatus,
    pub location: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct InstallResult {
    pub entries: Vec<InstallEntryResult>,
}

impl ApplyDecision {
    pub(super) fn status(self) -> InstallStatus {
        match self {
            Self::Install => InstallStatus::Installed,
            Self::Update => InstallStatus::Updated,
        }
    }

    pub(super) fn action_label(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
        }
    }
}

pub(super) fn resolve_apply_decision(
    exists: bool,
    force: bool,
    target: InstallTarget,
    scope: InstallScope,
    name: &str,
    location: &str,
) -> Result<ApplyDecision, InstallError> {
    match (exists, force) {
        (false, _) => Ok(ApplyDecision::Install),
        (true, true) => Ok(ApplyDecision::Update),
        (true, false) => Err(InstallError::AlreadyExists {
            name: name.to_owned(),
            target,
            scope,
            location: location.to_owned(),
        }),
    }
}

pub(super) fn normalize_source_dir_for_scope(
    scope: InstallScope,
    project_dir: &Path,
    source_dir: &Path,
) -> String {
    if matches!(scope, InstallScope::Project | InstallScope::Local) {
        if let Ok(relative) = source_dir.strip_prefix(project_dir) {
            if relative.as_os_str().is_empty() {
                return ".".to_owned();
            }
            return relative.to_string_lossy().into_owned();
        }
    }

    source_dir.to_string_lossy().into_owned()
}
