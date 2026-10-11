use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SHARED_CONFIGURATIONS_ROOT_ENV: &str = "ONEC_CONFIGURATIONS_ROOT";

mod diagnostics_baseline_fs;
pub use diagnostics_baseline_fs::{ManagedBaselineDirectory, ManagedEntryReading};

pub mod extension_topology;
pub mod file_role;
pub mod path_scope;
pub mod source_set;
pub mod workspace_walk;
pub use extension_topology::{
    ExtensionNode, ExtensionNodeSpec, ExtensionTopology, ExternalObjectKind, NodeId, NodeKind,
    TopologyError, TopologyFingerprint, TOPOLOGY_FORMAT_VERSION,
};
pub use file_role::{
    file_role, is_bsl_source_path, is_common_module_body_path, is_metadata_path,
    is_substrate_listed_body_path, FileRole, METADATA_WATCHED_EXTENSIONS, SOURCE_EXTENSIONS,
};
pub use path_scope::{PathScope, ResolvedDirs, Spellings};
pub use source_set::SourceSet;
pub use stdx::path_exclusion::ExcludedPaths;
pub use workspace_walk::{
    path_crosses_a_link_cycle, walk_workspace_roots, WalkOutcome, WalkedFile,
};

/// A config file exists but cannot be read or parsed. Deliberately not folded
/// into a default config: a broken file silently reverting to auto-discovery
/// would analyze a different project than the one configured.
#[derive(Debug, Clone)]
pub struct ConfigLoadError {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to load config {}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for ConfigLoadError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineProjectScope {
    /// The base configuration root, relative to the project. `None` for an
    /// extension-only project, which has no base at all — as opposed to a base
    /// that happens to sit at the project root. Serialization is unchanged for a
    /// project that has one, so published baselines keep their fingerprints.
    pub source_root: Option<String>,
    pub extensions: Vec<DiagnosticsBaselineProjectExtension>,
    /// External data processors and reports. Absent from the serialized form when
    /// empty, so every baseline published before externals existed keeps its
    /// scope fingerprint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub externals: Vec<DiagnosticsBaselineProjectExternal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineProjectExtension {
    pub name: String,
    pub path: String,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineProjectExternal {
    pub name: String,
    pub path: String,
    /// The extensions the object narrowed its view to; `None` when it sees
    /// every extension. Absent from the serialized form then, so a baseline
    /// published before the key existed keeps its scope fingerprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDiagnosticsBaseline {
    pub path: PathBuf,
    pub project_path: String,
    pub scope: DiagnosticsBaselineProjectScope,
    pub mode: DiagnosticsBaselineProjectMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticsBaselineProjectMode {
    Legacy,
    Partitioned { groups: Vec<DiagnosticsBaselineGroupConfig>, include: Option<Vec<String>> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DiagnosticsBaselinePartitionIdentity {
    Main {
        path: String,
    },
    Extension {
        name: String,
        path: String,
        depends_on: Vec<String>,
    },
    Group {
        name: String,
        members: Vec<DiagnosticsBaselineProjectExtension>,
    },
    External {
        name: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depends_on: Option<Vec<String>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsBaselinePartition {
    pub id: String,
    pub key: String,
    pub identity: DiagnosticsBaselinePartitionIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsBaselineRootOwner {
    pub root: String,
    pub partition_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsBaselinePartitionPolicy {
    Baseline,
    Unsuppressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsBaselineSelection {
    All,
    Selective,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsBaselinePartitionPlan {
    pub project_scope: DiagnosticsBaselineProjectScope,
    pub project_scope_fingerprint: String,
    pub selection_fingerprint: String,
    pub partitions: Vec<DiagnosticsBaselinePartition>,
    pub enabled_partition_ids: Vec<String>,
    pub selection: DiagnosticsBaselineSelection,
    pub roots: Vec<DiagnosticsBaselineRootOwner>,
}

impl DiagnosticsBaselinePartitionPlan {
    pub fn owner_for_project_path(&self, path: &str) -> Option<&str> {
        self.roots
            .iter()
            .find(|owner| {
                owner.root.is_empty()
                    || path == owner.root
                    || path.strip_prefix(&owner.root).is_some_and(|suffix| suffix.starts_with('/'))
            })
            .map(|owner| owner.partition_id.as_str())
    }

    pub fn policy_for_partition(&self, id: &str) -> Option<DiagnosticsBaselinePartitionPolicy> {
        self.partitions.iter().any(|partition| partition.id == id).then(|| {
            if self.enabled_partition_ids.iter().any(|enabled| enabled == id) {
                DiagnosticsBaselinePartitionPolicy::Baseline
            } else {
                DiagnosticsBaselinePartitionPolicy::Unsuppressed
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticsBaselineProjectError {
    CannotResolve { path: PathBuf, message: String },
    OutsideProject(PathBuf),
    Symlink(PathBuf),
    NotAFile(PathBuf),
    NonUtf8(PathBuf),
    PathCollision(String),
    InvalidConfig(String),
    LegacyExtension(String),
    InvalidGroup(String),
}

impl fmt::Display for DiagnosticsBaselineProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CannotResolve { path, message } => {
                write!(f, "cannot resolve {}: {message}", path.display())
            }
            Self::OutsideProject(path) => {
                write!(f, "path is outside the project: {}", path.display())
            }
            Self::Symlink(path) => {
                write!(f, "diagnostics baseline target is a symlink: {}", path.display())
            }
            Self::NotAFile(path) => {
                write!(f, "diagnostics baseline target is not a file: {}", path.display())
            }
            Self::NonUtf8(path) => write!(f, "project path is not valid UTF-8: {}", path.display()),
            Self::PathCollision(path) => write!(f, "project roots collide at: {path}"),
            Self::InvalidConfig(message) => {
                write!(f, "invalid diagnostics baseline config: {message}")
            }
            Self::LegacyExtension(name) => write!(
                f,
                "partitioned diagnostics baseline requires a structured extension entry: {name}"
            ),
            Self::InvalidGroup(message) => {
                write!(f, "invalid diagnostics baseline group: {message}")
            }
        }
    }
}

impl std::error::Error for DiagnosticsBaselineProjectError {}

#[derive(Debug, Clone)]
pub enum ProjectError {
    ConfigLoad(ConfigLoadError),
    Topology(TopologyError),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectError::ConfigLoad(err) => err.fmt(f),
            ProjectError::Topology(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for ProjectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProjectError::ConfigLoad(err) => Some(err),
            ProjectError::Topology(err) => Some(err),
        }
    }
}

impl From<ConfigLoadError> for ProjectError {
    fn from(err: ConfigLoadError) -> Self {
        ProjectError::ConfigLoad(err)
    }
}

impl From<TopologyError> for ProjectError {
    fn from(err: TopologyError) -> Self {
        ProjectError::Topology(err)
    }
}

/// Where [`Project::compatibility_mode`] found the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityModeSource {
    /// The `compatibility_mode` project setting.
    Setting,
    /// `<CompatibilityMode>` of the main configuration's `Configuration.xml`.
    ConfigurationXml,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub config: ProjectConfig,
    source_path: Option<PathBuf>,
    /// Set when discovery found `Configuration.xml` where a main configuration is
    /// expected and it turned out to be an extension. Never a source root.
    extension_in_config_location: Option<PathBuf>,
    extension_paths: Vec<(String, PathBuf)>,
    external_paths: Vec<(String, PathBuf)>,
    topology: ExtensionTopology,
    /// `[source].exclude`, resolved. Wins over every root, declared or discovered.
    exclusions: ExcludedPaths,
    /// Whether the exclusions removed a root the project would otherwise have had.
    /// Such a project must not fall back to the workspace root as an anonymous source
    /// root: the user narrowed the project, and the fallback would widen it instead.
    sources_cut: bool,
}

/// What discovery found at the places a main configuration is expected.
struct DiscoveredSource {
    configuration: Option<PathBuf>,
    extension: Option<PathBuf>,
    /// A configuration was declared or found, and the user excluded it.
    excluded: bool,
}

impl Project {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ProjectError> {
        let root = root.into();
        let config = ProjectConfig::load(&root)?.unwrap_or_default();
        Self::with_config(root, config)
    }

    /// Like [`Project::new`] but with an already-resolved config, so a caller that
    /// loaded the config from an explicit path (e.g. the CLI `--config` flag) keeps
    /// it instead of having `bsl-analyzer.toml` re-discovered under `root`.
    pub fn with_config(
        root: impl Into<PathBuf>,
        config: ProjectConfig,
    ) -> Result<Self, ProjectError> {
        let root = root.into();
        let exclusions = config.source_exclusions(&root)?;
        let DiscoveredSource {
            configuration: source_path,
            extension: extension_in_config_location,
            excluded: base_excluded,
        } = Self::discover_source_path(&root, &config, &exclusions)?;
        let mut candidates_cut = false;
        let specs =
            Self::resolve_extension_specs(&root, &config, &exclusions, &mut candidates_cut)?;
        let base_path = source_path.as_deref().unwrap_or(&root);
        let canonical_base =
            std::fs::canonicalize(base_path).unwrap_or_else(|_| base_path.to_path_buf());
        let mut topology = ExtensionTopology::build(&canonical_base, specs)?;
        let node_excluded = |node: &extension_topology::ExtensionNode| {
            exclusions.is_excluded(node.path()) || exclusions.is_excluded(node.canonical_path())
        };
        let nodes_cut = topology.nodes().iter().any(node_excluded);
        if nodes_cut {
            topology = topology.retain(&canonical_base, |node| !node_excluded(node));
        }
        let paths_of_kind = |external: bool| -> Vec<(String, PathBuf)> {
            topology
                .nodes()
                .iter()
                .filter(|node| node.kind().is_external() == external)
                .map(|node| (node.name().to_string(), node.path().to_path_buf()))
                .collect()
        };
        let extension_paths = paths_of_kind(false);
        let external_paths = paths_of_kind(true);
        Ok(Self {
            root,
            config,
            source_path,
            extension_in_config_location,
            extension_paths,
            external_paths,
            topology,
            exclusions,
            sources_cut: base_excluded || candidates_cut || nodes_cut,
        })
    }

    /// The directories the user took out of the project with `[source].exclude`.
    pub fn source_exclusions(&self) -> &ExcludedPaths {
        &self.exclusions
    }

    /// Whether `path` lies in a directory the user took out of the project.
    pub fn is_source_excluded(&self, path: &Path) -> bool {
        self.exclusions.is_excluded_resolved(path)
    }

    pub fn source_path(&self) -> &Path {
        self.source_path.as_deref().unwrap_or(&self.root)
    }

    /// Directories to scan for BSL sources: the configuration source root plus each
    /// resolved extension root. Scoping a file walk to these (instead of the raw
    /// project root) excludes vendored/build copies like `.build/vendor` that would
    /// otherwise be analyzed as a duplicate configuration.
    ///
    /// Empty when the user excluded every root, the workspace root included: an empty
    /// project is what was asked for, not a reason to fall back to a wider one.
    pub fn source_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(source_path) = self.configuration_path() {
            roots.push(source_path.to_path_buf());
        }
        roots.extend(self.extension_paths.iter().map(|(_, path)| path.clone()));
        roots.extend(self.external_paths.iter().map(|(_, path)| path.clone()));
        if roots.is_empty() && self.anonymous_root_allowed() {
            roots.push(self.root.clone());
        }
        roots
    }

    /// Whether the workspace root may stand in as the anonymous source root of a project
    /// that found nothing else — not when the user removed what it did find, and not
    /// when the user excluded the workspace root itself.
    fn anonymous_root_allowed(&self) -> bool {
        !self.sources_cut && !self.exclusions.is_excluded_resolved(&self.root)
    }

    pub fn configuration_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    /// The compatibility mode the project's code compiles under, with where it came
    /// from: the `compatibility_mode` setting first, then `<CompatibilityMode>` of
    /// the main configuration. Extensions and external data processors have no mode
    /// of their own — an external processor compiles under the mode of the base it
    /// is opened in (checked live on 8.3.17 and 8.3.27) — so the configuration's
    /// mode stands for every root. `None` when neither source states one.
    pub fn compatibility_mode(&self) -> Option<(String, CompatibilityModeSource)> {
        if let Some(value) = self.config.compatibility_mode.as_deref() {
            return Some((value.to_owned(), CompatibilityModeSource::Setting));
        }
        let configuration = self.configuration_path()?;
        configuration_compatibility_mode(configuration)
            .map(|value| (value, CompatibilityModeSource::ConfigurationXml))
    }

    /// Advisory for a project that is a configuration extension analyzed without
    /// the main configuration it extends. In that state its calls into the main
    /// configuration cannot resolve, so valid code is reported as broken — saying
    /// so beats letting the findings speak for themselves, because from the outside
    /// they read as the analyzer being wrong.
    ///
    /// The decision belongs to the project, not to a path a caller happens to hold:
    /// what matters is the directory the analyzer treats as the main configuration,
    /// and a base-less project reports the workspace root as its source path even
    /// when its extension roots are the real subject.
    pub fn standalone_extension_notice(&self) -> Option<String> {
        // Whatever the analyzer is treating as the main configuration is the subject:
        // a root pointed at an extension on purpose is just as standalone as one that
        // has no base at all, and both report the same unresolved calls.
        let subject = self
            .configuration_path()
            .or(self.extension_in_config_location.as_deref())
            .or_else(|| self.extension_paths.first().map(|(_, path)| path.as_path()))
            .unwrap_or(&self.root);
        (configuration_kind(subject) == ConfigurationKind::Extension).then(|| {
            format!(
                "{} is a configuration extension analyzed without its main configuration. \
                 Calls into the main configuration will be reported as unresolved. \
                 Point --configuration-root at the main configuration, declare it in \
                 [source].root, or use [source.configuration] with a shared configuration \
                 id/version.",
                subject.display()
            )
        })
    }

    /// Advisory for external data processors or reports declared in a project
    /// that has no main configuration. Their calls into the configuration they
    /// were written for cannot resolve, so valid code is reported as broken.
    /// Unlike an extension, an external root cannot be mistaken for the base,
    /// so the subject is the declared list, not the resolved source path.
    pub fn standalone_external_notice(&self) -> Option<String> {
        if self.configuration_path().is_some() || self.external_paths.is_empty() {
            return None;
        }
        let names: Vec<&str> = self.external_paths.iter().map(|(name, _)| name.as_str()).collect();
        Some(format!(
            "external data processors/reports [{}] are analyzed without an owning \
             configuration. Calls into the configuration will be reported as unresolved. \
             Point --configuration-root at the main configuration, declare it in \
             [source].root, or use [source.configuration] with a shared configuration \
             id/version.",
            names.join(", ")
        ))
    }

    /// Base root used by semantic/search compatibility layers. A configured
    /// configuration always wins. A project with extensions or external objects
    /// but no base has no semantic base at all. Only a project with none of them —
    /// one that is just a directory of sources — treats the workspace root as its
    /// anonymous source root.
    pub fn semantic_base_path(&self) -> Option<&Path> {
        self.configuration_path().or_else(|| {
            (self.extension_paths.is_empty()
                && self.external_paths.is_empty()
                && self.anonymous_root_allowed())
            .then_some(self.root.as_path())
        })
    }

    pub fn diagnostics_baseline(
        &self,
    ) -> Result<Option<ResolvedDiagnosticsBaseline>, DiagnosticsBaselineProjectError> {
        let Some(config) = self.config.diagnostics.baseline.as_ref() else {
            return Ok(None);
        };
        let (configured_path, mode) = match (&config.path, &config.directory) {
            (Some(path), None) if config.groups.is_empty() && config.include.is_none() => (
                self.config.resolve_config_path(&self.root, path),
                DiagnosticsBaselineProjectMode::Legacy,
            ),
            (None, Some(directory)) => {
                validate_partitioned_config_path(directory)?;
                if let Some(include) = &config.include {
                    if include.is_empty() {
                        return Err(DiagnosticsBaselineProjectError::InvalidConfig(
                            "include must not be empty".to_owned(),
                        ));
                    }
                    let mut seen = std::collections::HashSet::new();
                    for selector in include {
                        if !seen.insert(stdx::case::fold_lower_per_char(selector)) {
                            return Err(DiagnosticsBaselineProjectError::InvalidConfig(format!(
                                "duplicate include selector after case folding: {selector}"
                            )));
                        }
                    }
                }
                (
                    self.config.resolve_config_path(&self.root, directory),
                    DiagnosticsBaselineProjectMode::Partitioned {
                        groups: config.groups.clone(),
                        include: config.include.clone(),
                    },
                )
            }
            (Some(_), Some(_)) => {
                return Err(DiagnosticsBaselineProjectError::InvalidConfig(
                    "path and directory are mutually exclusive".to_owned(),
                ))
            }
            (Some(_), None) => {
                return Err(DiagnosticsBaselineProjectError::InvalidConfig(
                    "path cannot be combined with groups or include".to_owned(),
                ))
            }
            (None, None) => {
                return Err(DiagnosticsBaselineProjectError::InvalidConfig(
                    "either path or directory is required".to_owned(),
                ))
            }
        };
        let project_root = canonicalize_baseline_path(&self.root)?;
        let target = match &mode {
            DiagnosticsBaselineProjectMode::Legacy => {
                resolve_baseline_target(&configured_path, &project_root)?
            }
            DiagnosticsBaselineProjectMode::Partitioned { .. } => {
                resolve_baseline_directory(&configured_path, &project_root)?
            }
        };
        // `semantic_base_path`, not `configuration_path`: a project that is just a
        // directory of sources has no configuration but is not extension-only. Its
        // base is the workspace root, which is what published baselines already
        // record. Only a project WITH extensions and no configuration has no base.
        let source_root = match self.semantic_base_path() {
            Some(base) => {
                Some(relative_baseline_path(&canonicalize_baseline_path(base)?, &project_root)?)
            }
            None => None,
        };

        let mut seen: std::collections::HashSet<String> = source_root.iter().cloned().collect();
        let mut extensions = Vec::with_capacity(self.topology.nodes().len());
        let mut externals = Vec::new();
        for id in self.topology.topological_order() {
            let node = self.topology.node(*id);
            let path = relative_baseline_path(
                &canonicalize_baseline_path(node.canonical_path())?,
                &project_root,
            )?;
            if !seen.insert(path.clone()) {
                return Err(DiagnosticsBaselineProjectError::PathCollision(path));
            }
            let depends_on: Vec<String> = node
                .depends_on()
                .iter()
                .map(|id| self.topology.node(*id).name().to_owned())
                .collect();
            if node.kind().is_external() {
                externals.push(DiagnosticsBaselineProjectExternal {
                    name: node.name().to_owned(),
                    path,
                    depends_on: (!node.sees_every_extension()).then_some(depends_on),
                });
                continue;
            }
            extensions.push(DiagnosticsBaselineProjectExtension {
                name: node.name().to_owned(),
                path,
                depends_on,
            });
        }

        Ok(Some(ResolvedDiagnosticsBaseline {
            project_path: relative_baseline_path(&target, &project_root)?,
            path: target,
            scope: DiagnosticsBaselineProjectScope { source_root, extensions, externals },
            mode,
        }))
    }

    pub fn diagnostics_baseline_partition_plan(
        &self,
    ) -> Result<Option<DiagnosticsBaselinePartitionPlan>, DiagnosticsBaselineProjectError> {
        let Some(resolved) = self.diagnostics_baseline()? else { return Ok(None) };
        let DiagnosticsBaselineProjectMode::Partitioned { groups, include } = &resolved.mode else {
            return Ok(None);
        };
        for node in self.topology.nodes() {
            if !node.is_structured() {
                return Err(DiagnosticsBaselineProjectError::LegacyExtension(
                    node.name().to_owned(),
                ));
            }
        }

        // The nesting check takes only a CONFIGURED base — deliberately narrower
        // than the scope above. An anonymous base at the workspace root contains
        // every extension by construction, so including it would make each of them
        // look nested inside the base and reject the whole plan.
        let mut canonical_roots = match self.configuration_path() {
            Some(base) => vec![canonicalize_baseline_path(base)?],
            None => Vec::new(),
        };
        canonical_roots.extend(
            self.topology
                .nodes()
                .iter()
                .map(|node| canonicalize_baseline_path(node.canonical_path()))
                .collect::<Result<Vec<_>, _>>()?,
        );
        for (index, left) in canonical_roots.iter().enumerate() {
            for right in canonical_roots.iter().skip(index + 1) {
                if left.starts_with(right) || right.starts_with(left) {
                    return Err(DiagnosticsBaselineProjectError::PathCollision(
                        relative_baseline_path(
                            if left.components().count() >= right.components().count() {
                                left
                            } else {
                                right
                            },
                            &canonicalize_baseline_path(&self.root)?,
                        )?,
                    ));
                }
            }
        }

        let mut group_names = std::collections::HashSet::new();
        let mut member_to_group = std::collections::HashMap::<String, String>::new();
        let mut normalized_groups = Vec::with_capacity(groups.len());
        for group in groups {
            let name = group.name.trim();
            if name.is_empty() {
                return Err(DiagnosticsBaselineProjectError::InvalidGroup(
                    "name is empty".to_owned(),
                ));
            }
            let folded_name = stdx::case::fold_lower_per_char(name);
            if !group_names.insert(folded_name) {
                return Err(DiagnosticsBaselineProjectError::InvalidGroup(format!(
                    "duplicate name after case folding: {name}"
                )));
            }
            if group.extensions.is_empty() {
                return Err(DiagnosticsBaselineProjectError::InvalidGroup(format!(
                    "{name} has no extensions"
                )));
            }
            let mut requested = std::collections::HashSet::new();
            for extension in &group.extensions {
                let folded = stdx::case::fold_lower_per_char(extension);
                if !requested.insert(folded.clone()) {
                    return Err(DiagnosticsBaselineProjectError::InvalidGroup(format!(
                        "{name} repeats extension {extension}"
                    )));
                }
                let Some(node) = self
                    .topology
                    .nodes()
                    .iter()
                    .filter(|node| !node.kind().is_external())
                    .find(|node| stdx::case::fold_lower_per_char(node.name()) == folded)
                else {
                    return Err(DiagnosticsBaselineProjectError::InvalidGroup(format!(
                        "{name} references unknown extension {extension}"
                    )));
                };
                if let Some(first) = member_to_group.insert(folded, name.to_owned()) {
                    return Err(DiagnosticsBaselineProjectError::InvalidGroup(format!(
                        "extension {} belongs to both {first} and {name}",
                        node.name()
                    )));
                }
            }
            normalized_groups.push((name.to_owned(), requested));
        }
        normalized_groups.sort_by(|left, right| left.0.cmp(&right.0));

        // The base owns a partition only when the project has one. A project without
        // it must not grow a "main" partition rooted at the workspace directory: that
        // would claim every file outside an extension for a configuration that does
        // not exist.
        let mut partitions = Vec::new();
        let mut roots = Vec::new();
        if let Some(base) = resolved.scope.source_root.clone() {
            partitions.push(partition(
                "main".to_owned(),
                DiagnosticsBaselinePartitionIdentity::Main { path: base.clone() },
            ));
            roots
                .push(DiagnosticsBaselineRootOwner { root: base, partition_id: "main".to_owned() });
        }

        for (group_name, members) in &normalized_groups {
            let id = format!("group:{group_name}");
            validate_partition_id(&id)?;
            let identity_members = resolved
                .scope
                .extensions
                .iter()
                .filter(|extension| {
                    members.contains(&stdx::case::fold_lower_per_char(&extension.name))
                })
                .cloned()
                .collect();
            partitions.push(partition(
                id.clone(),
                DiagnosticsBaselinePartitionIdentity::Group {
                    name: group_name.clone(),
                    members: identity_members,
                },
            ));
            roots.extend(
                resolved
                    .scope
                    .extensions
                    .iter()
                    .filter(|extension| {
                        members.contains(&stdx::case::fold_lower_per_char(&extension.name))
                    })
                    .map(|extension| DiagnosticsBaselineRootOwner {
                        root: extension.path.clone(),
                        partition_id: id.clone(),
                    }),
            );
        }

        for extension in &resolved.scope.extensions {
            if member_to_group.contains_key(&stdx::case::fold_lower_per_char(&extension.name)) {
                continue;
            }
            let id = format!("extension:{}", extension.name);
            validate_partition_id(&id)?;
            partitions.push(partition(
                id.clone(),
                DiagnosticsBaselinePartitionIdentity::Extension {
                    name: extension.name.clone(),
                    path: extension.path.clone(),
                    depends_on: extension.depends_on.clone(),
                },
            ));
            roots.push(DiagnosticsBaselineRootOwner {
                root: extension.path.clone(),
                partition_id: id,
            });
        }

        for external in &resolved.scope.externals {
            let id = format!("external:{}", external.name);
            validate_partition_id(&id)?;
            partitions.push(partition(
                id.clone(),
                DiagnosticsBaselinePartitionIdentity::External {
                    name: external.name.clone(),
                    path: external.path.clone(),
                    depends_on: external.depends_on.clone(),
                },
            ));
            roots.push(DiagnosticsBaselineRootOwner {
                root: external.path.clone(),
                partition_id: id,
            });
        }

        partitions.sort_by(|left, right| {
            partition_sort_key(&left.id).cmp(&partition_sort_key(&right.id))
        });
        if let Some(include) = include {
            for selector in include {
                if !partitions.iter().any(|partition| partition.id == *selector) {
                    return Err(DiagnosticsBaselineProjectError::InvalidConfig(format!(
                        "include references unknown partition selector: {selector}"
                    )));
                }
            }
        }
        let selection = if include.is_some() {
            DiagnosticsBaselineSelection::Selective
        } else {
            DiagnosticsBaselineSelection::All
        };
        let enabled_partition_ids = partitions
            .iter()
            .filter(|partition| {
                include.as_ref().is_none_or(|selectors| selectors.contains(&partition.id))
            })
            .map(|partition| partition.id.clone())
            .collect::<Vec<_>>();
        let selection_bytes = serde_json::to_vec(
            &partitions
                .iter()
                .map(|partition| {
                    (
                        partition.id.clone(),
                        if enabled_partition_ids.contains(&partition.id) {
                            DiagnosticsBaselinePartitionPolicy::Baseline
                        } else {
                            DiagnosticsBaselinePartitionPolicy::Unsuppressed
                        },
                        canonical_identity(&partition.identity),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .map_err(|error| DiagnosticsBaselineProjectError::InvalidConfig(error.to_string()))?;
        let mut selection_hasher = blake3::Hasher::new();
        selection_hasher.update(b"bsl-analyzer/diagnostics-baseline/selection/v1\0");
        selection_hasher.update(&selection_bytes);
        roots.sort_by(|left, right| {
            right.root.len().cmp(&left.root.len()).then(left.root.cmp(&right.root))
        });
        // Hashed over a canonical ordering: the declaration order of independent
        // extensions carries no meaning — ownership, ids and roots do not depend on it —
        // but the fingerprint would, and a reordered `bsl-analyzer.toml` would then
        // invalidate the whole published set as a scope mismatch.
        let scope_bytes = serde_json::to_vec(&canonical_scope(&resolved.scope))
            .map_err(|error| DiagnosticsBaselineProjectError::InvalidConfig(error.to_string()))?;
        let mut scope_hasher = blake3::Hasher::new();
        scope_hasher.update(b"bsl-analyzer/diagnostics-baseline/project-scope/v1\0");
        scope_hasher.update(&scope_bytes);
        Ok(Some(DiagnosticsBaselinePartitionPlan {
            project_scope: resolved.scope,
            project_scope_fingerprint: scope_hasher.finalize().to_hex().to_string(),
            selection_fingerprint: selection_hasher.finalize().to_hex().to_string(),
            partitions,
            enabled_partition_ids,
            selection,
            roots,
        }))
    }

    fn discover_source_path(
        root: &Path,
        config: &ProjectConfig,
        exclusions: &ExcludedPaths,
    ) -> Result<DiscoveredSource, ConfigLoadError> {
        let found = |path: PathBuf| DiscoveredSource {
            configuration: Some(path),
            extension: None,
            excluded: false,
        };
        // A directory that carries `Configuration.xml` where a MAIN configuration is
        // expected, but turned out to be an extension. It is not a base and does not
        // become a source root, yet it is what makes this project a base-less
        // extension — which is exactly what the standalone advisory reports.
        let mut extension = None;
        if config.configuration_root.is_some() || config.configuration_dependency.is_some() {
            let path = config
                .resolve_configuration_path(root)?
                .expect("configured source resolves to a path");
            // Decided before anything inside is read, and final: the user excluded the
            // configuration they declared, so no other directory is searched in its place.
            if exclusions.is_excluded_resolved(&path) {
                tracing::info!(?path, "configured source root is excluded by [source].exclude");
                return Ok(DiscoveredSource {
                    configuration: None,
                    extension: None,
                    excluded: true,
                });
            }
            if configuration_xml_in(&path).is_some() {
                tracing::info!(?path, "found explicitly configured 1C configuration");
                return Ok(found(path));
            }
            if config.configuration_dependency.is_some() {
                return Err(config.config_error(format!(
                    "shared configuration {} does not contain {}",
                    path.display(),
                    bsl_conventions::ConventionalName::ConfigurationXml.canonical()
                )));
            }
            tracing::warn!(?path, "configuration root specified but Configuration.xml not found");
        }

        let mut excluded = false;
        match search_configuration(root, 2, exclusions, &mut excluded) {
            SearchOutcome::Configuration(path) => {
                tracing::info!(?path, "found Configuration.xml by search");
                return Ok(found(path));
            }
            SearchOutcome::Extension(path) => extension = extension.or(Some(path)),
            SearchOutcome::None => {}
        }

        for pattern in &["src/cf", "Configuration"] {
            let path = root.join(pattern);
            if exclusions.is_excluded_resolved(&path) {
                excluded |= configuration_xml_in(&path).is_some();
                continue;
            }
            if configuration_xml_in(&path).is_none() {
                continue;
            }
            if configuration_kind(&path) == ConfigurationKind::Extension {
                extension = extension.or(Some(path));
                continue;
            }
            tracing::info!(?path, pattern, "found configuration using common pattern");
            return Ok(found(path));
        }

        tracing::debug!(?root, "no 1C configuration found");
        Ok(DiscoveredSource { configuration: None, extension, excluded })
    }

    pub fn extension_paths(&self) -> &[(String, PathBuf)] {
        &self.extension_paths
    }

    /// External data processors and reports, by declared name, in declaration
    /// order. Never part of [`Self::extension_paths`]: the two compose differently.
    pub fn external_paths(&self) -> &[(String, PathBuf)] {
        &self.external_paths
    }

    /// The validated extension dependency topology. Holding a `Project` implies
    /// the topology is valid — invalid graphs fail construction instead.
    pub fn extension_topology(&self) -> &ExtensionTopology {
        &self.topology
    }

    /// Test-visible projection of [`Self::resolve_extension_specs`] onto the
    /// legacy `(name, path)` pairs.
    #[cfg(test)]
    fn resolve_extensions(
        root: &Path,
        config: &ProjectConfig,
    ) -> Result<Vec<(String, PathBuf)>, TopologyError> {
        let specs =
            Self::resolve_extension_specs(root, config, &ExcludedPaths::default(), &mut false)?;
        Ok(specs.into_iter().map(|spec| (spec.name, spec.path)).collect())
    }

    /// Resolves the extension source roots. With no `extensions` configured this
    /// auto-discovers the conventional `src/cfe/*` layout (mirroring how the main
    /// configuration is found under `src/cf` without any setting); an explicit list
    /// takes over and disables discovery. Bare string entries (a final-segment `*`
    /// glob allowed) keep their historical lenient semantics: a missing or
    /// non-extension path is skipped with a warning and textual path variants
    /// collapse silently. Structured entries carry a user-declared identity, so
    /// for them every such degradation is an error instead.
    ///
    /// Discovery — the conventional layouts and a declared glob — never looks inside a
    /// directory the user excluded, and sets `cut` when it passed one over. A literally
    /// declared entry is still resolved and validated as declared; the caller removes it
    /// from the topology afterwards, so an excluded dependency stays a legal name.
    fn resolve_extension_specs(
        root: &Path,
        config: &ProjectConfig,
        exclusions: &ExcludedPaths,
        cut: &mut bool,
    ) -> Result<Vec<ExtensionNodeSpec>, TopologyError> {
        enum Candidate {
            Legacy(PathBuf),
            Structured(StructuredExtensionDecl, PathBuf),
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        match &config.extensions {
            // Unset → mirror main-config zero-config discovery. An explicit list
            // (including an empty one, i.e. opt-out) is taken as authoritative.
            None => {
                candidates.extend(
                    Self::auto_discover_extensions(root, exclusions, cut)
                        .into_iter()
                        .map(Candidate::Legacy),
                );
            }
            Some(list) => {
                for decl in list {
                    match decl {
                        ExtensionDecl::Path(ext_path_str) => {
                            if ext_path_str.contains('*') {
                                candidates.extend(
                                    expand_extension_glob(root, ext_path_str, exclusions, cut)
                                        .into_iter()
                                        .map(Candidate::Legacy),
                                );
                            } else {
                                candidates.push(Candidate::Legacy(root.join(ext_path_str)));
                            }
                        }
                        ExtensionDecl::Structured(structured) => {
                            if structured.path.contains('*') {
                                return Err(TopologyError::GlobInStructuredEntry {
                                    name: structured.name.clone(),
                                    pattern: structured.path.clone(),
                                });
                            }
                            let path = root.join(&structured.path);
                            candidates.push(Candidate::Structured(structured.clone(), path));
                        }
                    }
                }
            }
        }

        let mut specs: Vec<ExtensionNodeSpec> = Vec::new();
        let mut seen: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
        // Legacy candidates skipped by validation, so a repeated declaration of
        // the same broken path stays silent instead of re-warning per variant.
        let mut skipped: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for candidate in candidates {
            match candidate {
                Candidate::Legacy(path) => {
                    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    if seen.contains_key(&canonical) || skipped.contains(&canonical) {
                        continue;
                    }
                    let label = path.to_string_lossy().into_owned();
                    let Some((name, path)) = Self::validate_extension(&label, path) else {
                        skipped.insert(canonical);
                        continue;
                    };
                    seen.insert(canonical.clone(), specs.len());
                    specs.push(ExtensionNodeSpec {
                        kind: NodeKind::Extension,
                        name,
                        path,
                        canonical_path: canonical,
                        depends_on: None,
                        structured: false,
                    });
                }
                Candidate::Structured(structured, path) => {
                    if !path.exists() {
                        return Err(TopologyError::StructuredPathMissing {
                            name: structured.name,
                            path,
                        });
                    }
                    if configuration_xml_in(&path).is_none() {
                        return Err(TopologyError::StructuredNotAnExtension {
                            name: structured.name,
                            path,
                        });
                    }
                    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    if let Some(&first) = seen.get(&canonical) {
                        return Err(TopologyError::DuplicatePath {
                            path: canonical,
                            first: specs[first].name.clone(),
                            second: structured.name,
                        });
                    }
                    tracing::info!(
                        name = %structured.name,
                        path = %path.display(),
                        "resolved extension"
                    );
                    seen.insert(canonical.clone(), specs.len());
                    specs.push(ExtensionNodeSpec {
                        kind: NodeKind::Extension,
                        name: structured.name,
                        path,
                        canonical_path: canonical,
                        depends_on: Some(structured.depends_on),
                        structured: true,
                    });
                }
            }
        }

        // External objects go after every extension so their declaration index
        // never competes with an extension's. A declared one carries a
        // user-declared identity and is strict; a discovered one is lenient the
        // way a discovered extension is — whatever lies under the container must
        // not stop a project that loaded before the container was looked at —
        // and enters with a unique name so that it is indistinguishable from a
        // declared one afterwards.
        match &config.externals {
            None => Self::discover_externals(root, exclusions, cut, &mut specs, &mut seen),
            Some(list) => Self::declare_externals(root, list, &mut specs, &mut seen)?,
        }
        Ok(specs)
    }

    fn declare_externals(
        root: &Path,
        list: &[ExternalDecl],
        specs: &mut Vec<ExtensionNodeSpec>,
        seen: &mut std::collections::HashMap<PathBuf, usize>,
    ) -> Result<(), TopologyError> {
        // The object name, not the declared alias, is the identity every
        // `(MdoType, name)`-keyed surface sees; two exports of one name would
        // leave the later one invisible there.
        let mut seen_objects: std::collections::HashMap<
            (ExternalObjectKind, String),
            (usize, String),
        > = std::collections::HashMap::new();
        for decl in list {
            let path = root.join(&decl.path);
            if !path.is_dir() {
                return Err(TopologyError::ExternalPathMissing { name: decl.name.clone(), path });
            }
            let (kind, object) = classify_external_root(&path)
                .map_err(|problem| problem.into_error(decl.name.clone(), path.clone()))?;
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if let Some(&first) = seen.get(&canonical) {
                return Err(TopologyError::DuplicatePath {
                    path: canonical,
                    first: specs[first].name.clone(),
                    second: decl.name.clone(),
                });
            }
            let object_key = (kind, stdx::case::fold_lower_per_char(&object));
            if let Some((first, object)) = seen_objects.get(&object_key) {
                return Err(TopologyError::ExternalObjectNameDuplicate {
                    object: object.clone(),
                    first: specs[*first].name.clone(),
                    second: decl.name.clone(),
                });
            }
            seen_objects.insert(object_key, (specs.len(), object));
            tracing::info!(name = %decl.name, path = %path.display(), ?kind, "resolved external");
            seen.insert(canonical.clone(), specs.len());
            specs.push(ExtensionNodeSpec {
                kind: NodeKind::External(kind),
                name: decl.name.clone(),
                path,
                canonical_path: canonical,
                depends_on: decl.depends_on.clone(),
                structured: true,
            });
        }
        Ok(())
    }

    /// Zero-config discovery of external objects. Every candidate the topology
    /// would refuse is skipped here with a warning instead: a directory that is
    /// not one export, an empty directory name, an object already exported by an
    /// accepted candidate, or a directory name already taken by any root — the
    /// topology's name table treats a collision with a structured name as an
    /// error, and a discovered directory the user never declared must not raise
    /// one. What remains has a unique name and is a structured node.
    fn discover_externals(
        root: &Path,
        exclusions: &ExcludedPaths,
        cut: &mut bool,
        specs: &mut Vec<ExtensionNodeSpec>,
        seen: &mut std::collections::HashMap<PathBuf, usize>,
    ) {
        let mut taken_names: std::collections::HashMap<String, usize> = specs
            .iter()
            .enumerate()
            .map(|(idx, spec)| (stdx::case::fold_lower_per_char(&spec.name), idx))
            .collect();
        let mut seen_objects: std::collections::HashMap<(ExternalObjectKind, String), usize> =
            std::collections::HashMap::new();
        for path in Self::auto_discover_externals(root, exclusions, cut) {
            let (kind, object) = match classify_external_root(&path) {
                Ok(classified) => classified,
                Err(problem) => {
                    tracing::warn!(
                        path = %path.display(),
                        problem = %problem.into_error(String::new(), path.clone()),
                        "discovered directory is not one external object export, skipping"
                    );
                    continue;
                }
            };
            let name =
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.trim().is_empty() {
                tracing::warn!(path = %path.display(), "discovered external has an empty name, skipping");
                continue;
            }
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if let Some(&first) = seen.get(&canonical) {
                tracing::warn!(
                    path = %path.display(),
                    first = %specs[first].name,
                    "discovered external is already a root, skipping"
                );
                continue;
            }
            let folded_name = stdx::case::fold_lower_per_char(&name);
            if let Some(&first) = taken_names.get(&folded_name) {
                tracing::warn!(
                    path = %path.display(),
                    first = %specs[first].path.display(),
                    name = %name,
                    "discovered external shares a root name, skipping"
                );
                continue;
            }
            let object_key = (kind, stdx::case::fold_lower_per_char(&object));
            if let Some(&first) = seen_objects.get(&object_key) {
                tracing::warn!(
                    path = %path.display(),
                    first = %specs[first].path.display(),
                    object = %object,
                    "discovered external exports an object another root already exports, skipping"
                );
                continue;
            }
            tracing::info!(name = %name, path = %path.display(), ?kind, "discovered external");
            taken_names.insert(folded_name, specs.len());
            seen_objects.insert(object_key, specs.len());
            seen.insert(canonical.clone(), specs.len());
            specs.push(ExtensionNodeSpec {
                kind: NodeKind::External(kind),
                name,
                path,
                canonical_path: canonical,
                depends_on: None,
                structured: true,
            });
        }
    }

    /// The conventional containers of external objects. Two families, both
    /// scanned — unlike extensions, processors and reports live side by side —
    /// and within a family the first directory that exists wins. The kind of
    /// what is found comes from its XML, not from the container. Candidates
    /// come back in path order across both families, because a name collision
    /// keeps the first by path: family order would otherwise decide it when a
    /// family fell back to its bare container.
    fn auto_discover_externals(
        root: &Path,
        exclusions: &ExcludedPaths,
        cut: &mut bool,
    ) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for family in [["src/epf", "epf"], ["src/erf", "erf"]] {
            if let Some(parent) = family.into_iter().find(|parent| root.join(parent).is_dir()) {
                let candidates =
                    expand_extension_glob(root, &format!("{parent}/*"), exclusions, cut);
                tracing::info!(
                    parent,
                    count = candidates.len(),
                    "auto-discovered external candidates"
                );
                found.extend(candidates);
            }
        }
        found.sort();
        found
    }

    /// Zero-config extension discovery: the first conventional extensions directory
    /// that exists wins, contributing each of its immediate child directories as a
    /// candidate (later validated for `Configuration.xml`).
    fn auto_discover_extensions(
        root: &Path,
        exclusions: &ExcludedPaths,
        cut: &mut bool,
    ) -> Vec<PathBuf> {
        for parent in ["src/cfe", "cfe", "Расширения"] {
            if root.join(parent).is_dir() {
                let found = expand_extension_glob(root, &format!("{parent}/*"), exclusions, cut);
                tracing::info!(parent, count = found.len(), "auto-discovered extension candidates");
                return found;
            }
        }
        Vec::new()
    }

    fn validate_extension(ext_path_str: &str, path: PathBuf) -> Option<(String, PathBuf)> {
        if !path.exists() {
            tracing::warn!(path = %path.display(), "extension path not found, skipping");
            return None;
        }
        if configuration_xml_in(&path).is_none() {
            tracing::warn!(
                path = %path.display(),
                "extension has no Configuration.xml, skipping"
            );
            return None;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| ext_path_str.to_string());
        tracing::info!(name = %name, path = %path.display(), "resolved extension");
        Some((name, path))
    }
}

fn canonicalize_baseline_path(path: &Path) -> Result<PathBuf, DiagnosticsBaselineProjectError> {
    std::fs::canonicalize(path).map_err(|error| DiagnosticsBaselineProjectError::CannotResolve {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn resolve_baseline_target(
    path: &Path,
    project_root: &Path,
) -> Result<PathBuf, DiagnosticsBaselineProjectError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(DiagnosticsBaselineProjectError::Symlink(path.to_path_buf()));
            }
            if !metadata.is_file() {
                return Err(DiagnosticsBaselineProjectError::NotAFile(path.to_path_buf()));
            }
            let target = canonicalize_baseline_path(path)?;
            relative_baseline_path(&target, project_root)?;
            Ok(target)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent =
                path.parent().ok_or_else(|| DiagnosticsBaselineProjectError::CannotResolve {
                    path: path.to_path_buf(),
                    message: "target has no parent directory".to_owned(),
                })?;
            let file_name =
                path.file_name().ok_or_else(|| DiagnosticsBaselineProjectError::CannotResolve {
                    path: path.to_path_buf(),
                    message: "target has no file name".to_owned(),
                })?;
            let target = canonicalize_baseline_path(parent)?.join(file_name);
            relative_baseline_path(&target, project_root)?;
            Ok(target)
        }
        Err(error) => Err(DiagnosticsBaselineProjectError::CannotResolve {
            path: path.to_path_buf(),
            message: error.to_string(),
        }),
    }
}

fn resolve_baseline_directory(
    path: &Path,
    project_root: &Path,
) -> Result<PathBuf, DiagnosticsBaselineProjectError> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let name =
            ancestor.file_name().ok_or_else(|| DiagnosticsBaselineProjectError::CannotResolve {
                path: path.to_path_buf(),
                message: "directory has no existing ancestor".to_owned(),
            })?;
        suffix.push(name.to_owned());
        ancestor =
            ancestor.parent().ok_or_else(|| DiagnosticsBaselineProjectError::CannotResolve {
                path: path.to_path_buf(),
                message: "directory has no parent".to_owned(),
            })?;
    }
    let metadata = std::fs::symlink_metadata(ancestor).map_err(|error| {
        DiagnosticsBaselineProjectError::CannotResolve {
            path: ancestor.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DiagnosticsBaselineProjectError::Symlink(ancestor.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(DiagnosticsBaselineProjectError::CannotResolve {
            path: ancestor.to_path_buf(),
            message: "ancestor is not a directory".to_owned(),
        });
    }
    let mut target = canonicalize_baseline_path(ancestor)?;
    for component in suffix.into_iter().rev() {
        target.push(component);
    }
    relative_baseline_path(&target, project_root)?;
    Ok(target)
}

fn validate_partitioned_config_path(path: &str) -> Result<(), DiagnosticsBaselineProjectError> {
    diagnostics_baseline_fs::validate_managed_path(path)
        .map(|_| ())
        .map_err(|error| DiagnosticsBaselineProjectError::InvalidConfig(error.to_string()))
}

/// The project scope in a declaration-order-independent form, for fingerprinting only.
/// The scope itself keeps topological order, which callers rely on.
fn canonical_scope(scope: &DiagnosticsBaselineProjectScope) -> DiagnosticsBaselineProjectScope {
    let mut scope = scope.clone();
    for extension in &mut scope.extensions {
        extension.depends_on.sort();
    }
    for external in &mut scope.externals {
        if let Some(depends_on) = &mut external.depends_on {
            depends_on.sort();
        }
    }
    scope.extensions.sort_by(|left, right| left.name.cmp(&right.name));
    scope.externals.sort_by(|left, right| left.name.cmp(&right.name));
    scope
}

/// The same canonicalisation for a partition identity: group members and declared
/// dependencies are sets, and their order must not move the selection fingerprint.
fn canonical_identity(
    identity: &DiagnosticsBaselinePartitionIdentity,
) -> DiagnosticsBaselinePartitionIdentity {
    let mut identity = identity.clone();
    match &mut identity {
        DiagnosticsBaselinePartitionIdentity::Main { .. }
        | DiagnosticsBaselinePartitionIdentity::External { depends_on: None, .. } => {}
        DiagnosticsBaselinePartitionIdentity::Extension { depends_on, .. }
        | DiagnosticsBaselinePartitionIdentity::External { depends_on: Some(depends_on), .. } => {
            depends_on.sort()
        }
        DiagnosticsBaselinePartitionIdentity::Group { members, .. } => {
            for member in members.iter_mut() {
                member.depends_on.sort();
            }
            members.sort_by(|left, right| left.name.cmp(&right.name));
        }
    }
    identity
}

fn validate_partition_id(id: &str) -> Result<(), DiagnosticsBaselineProjectError> {
    // Counted in CHARACTERS: the id never reaches the file system — object paths are
    // built from the blake3 key and hash — so the bound is only a sanity limit on a
    // machine identifier. Counting bytes would halve it for Cyrillic, and extension
    // names in 1C configurations are Russian, so an ordinary name would be rejected.
    const MAX_CHARS: usize = 128;
    let length = id.chars().count();
    if length > MAX_CHARS {
        return Err(DiagnosticsBaselineProjectError::InvalidConfig(format!(
            "partition id is {length} characters, over the {MAX_CHARS} limit: {id}"
        )));
    }
    Ok(())
}

fn partition(
    id: String,
    identity: DiagnosticsBaselinePartitionIdentity,
) -> DiagnosticsBaselinePartition {
    let key = blake3::hash(id.as_bytes()).to_hex().to_string();
    DiagnosticsBaselinePartition { id, key, identity }
}

fn partition_sort_key(id: &str) -> (u8, &str) {
    if id == "main" {
        (0, id)
    } else if id.starts_with("group:") {
        (1, id)
    } else {
        (2, id)
    }
}

fn relative_baseline_path(
    path: &Path,
    project_root: &Path,
) -> Result<String, DiagnosticsBaselineProjectError> {
    let relative = path
        .strip_prefix(project_root)
        .map_err(|_| DiagnosticsBaselineProjectError::OutsideProject(path.to_path_buf()))?;
    relative
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .ok_or_else(|| DiagnosticsBaselineProjectError::NonUtf8(path.to_path_buf()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|components| components.join("/"))
}

/// Expands an `extensions` entry whose final path segment contains `*`
/// (e.g. `src/cfe/*` or `src/cfe/БУС_*`) into every immediate child directory
/// of the parent that matches the wildcard. The wildcard is only honoured in
/// the last segment; results are sorted for deterministic source-root ordering.
///
/// An excluded parent is not listed at all, and an excluded child is not returned.
/// `cut` is set only for an excluded child that carries a root marker — the
/// `Configuration.xml` of an extension or the object XML at the top of an external's
/// directory — looked up by name and never descended into: a directory that merely
/// matches the pattern was never going to be a root, and treating it as one would
/// take the workspace fallback away from a project that never had another root. An
/// excluded parent is not listed, so what it held is not known and nothing is cut.
fn expand_extension_glob(
    root: &Path,
    pattern: &str,
    exclusions: &ExcludedPaths,
    cut: &mut bool,
) -> Vec<PathBuf> {
    let normalized = pattern.replace('\\', "/");
    let (parent_rel, name_pattern) = match normalized.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", normalized.as_str()),
    };
    // The wildcard must live solely in the final segment. This rejects a parent
    // wildcard (`src/*/Foo`) and a trailing slash (`src/cfe/*/` → empty final
    // segment), both of which would otherwise fall through to a literal `read_dir`
    // and silently resolve nothing.
    if parent_rel.contains('*') || !name_pattern.contains('*') {
        tracing::warn!(
            pattern = %pattern,
            "extension glob supports a wildcard only in the final path segment, skipping"
        );
        return Vec::new();
    }
    let parent_dir = if parent_rel.is_empty() { root.to_path_buf() } else { root.join(parent_rel) };
    if exclusions.is_excluded_resolved(&parent_dir) {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(&parent_dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                path = %parent_dir.display(),
                error = %e,
                "extension glob parent directory not readable, skipping"
            );
            return Vec::new();
        }
    };
    let mut matched: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|entry| wildcard_matches(name_pattern, &entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .filter(|path| {
            let excluded = exclusions.is_excluded_resolved(path);
            *cut |= excluded && carries_root_marker(path);
            !excluded
        })
        .collect();
    matched.sort();
    matched
}

/// Whether `dir` names a root by its own marker file: `Configuration.xml` inside it,
/// or the object XML an exported external keeps at its top. Only the names directly
/// in `dir` are listed; nothing below it is read.
fn carries_root_marker(dir: &Path) -> bool {
    configuration_xml_in(dir).is_some()
        || std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry.file_type().is_ok_and(|t| t.is_file())
                    && bsl_conventions::has_extension(&entry.path(), bsl_conventions::XML_EXTENSION)
            })
        })
}

/// Case-insensitive single-segment wildcard match where `*` matches any run of
/// characters (including empty). Supports multiple `*` (e.g. `*_UT`, `БУС_*`).
fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let text: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut star_t) = (None, 0usize);
    while t < text.len() {
        if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            star_t = t;
            p += 1;
        } else if p < pat.len() && pat[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

fn validate_configuration_component(value: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed != value
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
    {
        return Err("must be one safe directory name without path separators".to_owned());
    }
    Ok(())
}

fn dotenv_value(path: &Path, requested: &str) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    // Several editors write a BOM in front of a file saved as UTF-8. It is not
    // whitespace, so it would otherwise glue itself to the first key and hide it —
    // and the resulting error names a variable the file plainly contains.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    for (index, raw_line) in text.lines().enumerate() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim_start();
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            return Err(format!(
                "invalid line {} in {}: expected KEY=VALUE",
                index + 1,
                path.display()
            ));
        };
        if key.trim() != requested {
            continue;
        }
        return parse_dotenv_value(raw_value.trim(), path, index + 1).map(Some);
    }
    Ok(None)
}

fn parse_dotenv_value(value: &str, path: &Path, line: usize) -> Result<String, String> {
    if let Some(quoted) = value.strip_prefix('\'') {
        let Some(end) = quoted.find('\'') else {
            return Err(format!("unclosed single quote in {}:{line}", path.display()));
        };
        let tail = quoted[end + 1..].trim();
        if !tail.is_empty() && !tail.starts_with('#') {
            return Err(format!("unexpected trailing content in {}:{line}", path.display()));
        }
        return Ok(quoted[..end].to_owned());
    }
    if let Some(quoted) = value.strip_prefix('"') {
        let mut result = String::new();
        let mut escaped = false;
        let mut closed_at = None;
        for (offset, ch) in quoted.char_indices() {
            if escaped {
                match ch {
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    '"' => result.push('"'),
                    '\\' => result.push('\\'),
                    // The values here are filesystem paths, and on Windows a
                    // backslash is an ordinary path separator rather than the start
                    // of an escape. Swallowing it turns `C:\1C\Store` into
                    // `C:1CStore` and the project stops loading.
                    other => {
                        result.push('\\');
                        result.push(other);
                    }
                }
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                closed_at = Some(offset);
                break;
            } else {
                result.push(ch);
            }
        }
        let Some(end) = closed_at else {
            return Err(format!("unclosed double quote in {}:{line}", path.display()));
        };
        let tail = quoted[end + '"'.len_utf8()..].trim();
        if !tail.is_empty() && !tail.starts_with('#') {
            return Err(format!("unexpected trailing content in {}:{line}", path.display()));
        }
        return Ok(result);
    }
    let unquoted = value.find(" #").map(|index| &value[..index]).unwrap_or(value).trim_end();
    Ok(unquoted.to_owned())
}

/// What a bounded search for a main configuration turned up.
enum SearchOutcome {
    Configuration(PathBuf),
    /// A `Configuration.xml` was found, but it belongs to an extension. Reported
    /// rather than dropped: it is not a base, yet it tells the caller the project
    /// is a base-less extension rather than an ordinary directory of sources.
    Extension(PathBuf),
    None,
}

/// `excluded` is set when the search passed over an excluded directory that carries a
/// `Configuration.xml` — a configuration the user took out, which is looked at only as
/// far as its presence and never descended into.
fn search_configuration(
    root: &Path,
    max_depth: usize,
    exclusions: &ExcludedPaths,
    excluded: &mut bool,
) -> SearchOutcome {
    let mut extension = None;
    match search_configuration_recursive(root, max_depth, 0, exclusions, excluded, &mut extension) {
        Some(path) => SearchOutcome::Configuration(path),
        None => match extension {
            Some(path) => SearchOutcome::Extension(path),
            None => SearchOutcome::None,
        },
    }
}

/// Returns the first main configuration; the first extension the walk meets is
/// left in `extension` — first in traversal order, which is not necessarily the
/// shallowest. A configuration always wins over an extension, whatever the visit
/// order, because the search only stops on a configuration.
fn search_configuration_recursive(
    dir: &Path,
    max_depth: usize,
    current_depth: usize,
    exclusions: &ExcludedPaths,
    excluded: &mut bool,
    extension: &mut Option<PathBuf>,
) -> Option<PathBuf> {
    if current_depth > max_depth {
        return None;
    }
    if exclusions.is_excluded_resolved(dir) {
        *excluded |= configuration_xml_in(dir).is_some();
        return None;
    }

    if configuration_xml_in(dir).is_some() {
        if configuration_kind(dir) != ConfigurationKind::Extension {
            return Some(dir.to_path_buf());
        }
        if extension.is_none() {
            *extension = Some(dir.to_path_buf());
        }
    }

    if current_depth < max_depth {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(file_type) = entry.file_type() {
                    if file_type.is_dir() {
                        let name = entry.file_name();
                        if !name.to_string_lossy().starts_with('.') {
                            if let Some(path) = search_configuration_recursive(
                                &entry.path(),
                                max_depth,
                                current_depth + 1,
                                exclusions,
                                excluded,
                                extension,
                            ) {
                                return Some(path);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// What a `Configuration.xml`-bearing directory actually is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationKind {
    /// A main configuration.
    Configuration,
    /// A configuration extension (CFE).
    Extension,
    /// No readable `Configuration.xml` at that path.
    Unknown,
}

/// Backstop on how much of `Configuration.xml` is read looking for the marker.
///
/// The scan normally stops at `</Properties>`, a few kilobytes in, because the
/// marker can only appear before it. This bound exists only so a malformed file
/// with no closing tag cannot make project construction read a whole megabyte
/// dump — it is not meant to be reached by any well-formed input.
const CONFIGURATION_KIND_SCAN_CAP: u64 = 8 * 1024 * 1024;

/// The element the platform writes only for an extension. Deliberately not
/// `ConfigurationExtensionCompatibilityMode`, which a *main* configuration also
/// carries — that one states which extensions it accepts, not that it is one.
const EXTENSION_MARKER: &[u8] = b"<ConfigurationExtensionPurpose";

/// Classifies a configuration root by its `Configuration.xml`.
///
/// Used to tell an operator that the directory handed to us is an extension
/// analyzed without its main configuration — the state in which valid calls
/// into the main configuration's exported common modules are reported as
/// unresolved.
/// The root's `Configuration.xml`, in whatever ASCII case the tree spells it.
fn configuration_xml_in(dir: &Path) -> Option<PathBuf> {
    bsl_conventions::find_child_ci(
        dir,
        bsl_conventions::ConventionalName::ConfigurationXml.canonical(),
    )
    .filter(|p| p.is_file())
}

/// Why a directory is not one external object export.
enum ExternalRootProblem {
    IsAConfiguration,
    NoObjectXml,
    AmbiguousObjectXml,
    ObjectElementMissing,
    NotAnExternalObject { tag: String },
    ObjectDirMissing { object: String },
}

impl ExternalRootProblem {
    fn into_error(self, name: String, path: PathBuf) -> TopologyError {
        match self {
            Self::IsAConfiguration => TopologyError::ExternalIsAConfiguration { name, path },
            Self::NoObjectXml => TopologyError::ExternalNoObjectXml { name, path },
            Self::AmbiguousObjectXml => TopologyError::ExternalAmbiguousObjectXml { name, path },
            Self::ObjectElementMissing => {
                TopologyError::ExternalObjectElementMissing { name, path }
            }
            Self::NotAnExternalObject { tag } => {
                TopologyError::ExternalNotAnExternalObject { name, path, tag }
            }
            Self::ObjectDirMissing { object } => {
                TopologyError::ExternalObjectDirMissing { name, path, object }
            }
        }
    }
}

/// Backstop on how much of the object XML is read looking for its element. The
/// element opens within the first kilobyte of any designer export; the bound is
/// deliberately generous (a mebibyte) and only keeps a malformed file from being
/// read whole.
const EXTERNAL_OBJECT_SCAN_CAP: u64 = 1024 * 1024;

/// Classifies a directory as the export of one external data processor or report.
///
/// The export has no `Configuration.xml`; what identifies it is the single
/// `<Name>.xml` directly under the root whose first element under
/// `MetaDataObject` is `ExternalDataProcessor` or `ExternalReport`, with the
/// `<Name>/` directory beside it. Anything else — a configuration, several
/// exports in one directory, an internal object's export, an XML that lost its
/// directory — is refused by name, because declaring the wrong directory would
/// otherwise analyze a different object than the one meant, or none at all.
fn classify_external_root(dir: &Path) -> Result<(ExternalObjectKind, String), ExternalRootProblem> {
    use std::io::Read as _;

    if configuration_xml_in(dir).is_some() {
        return Err(ExternalRootProblem::IsAConfiguration);
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Err(ExternalRootProblem::NoObjectXml);
    };
    let mut object_xmls: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| bsl_conventions::has_extension(path, bsl_conventions::XML_EXTENSION))
        .collect();
    object_xmls.sort();
    let main = match object_xmls.as_slice() {
        [] => return Err(ExternalRootProblem::NoObjectXml),
        [only] => only,
        _ => return Err(ExternalRootProblem::AmbiguousObjectXml),
    };

    let Ok(file) = std::fs::File::open(main) else {
        return Err(ExternalRootProblem::NoObjectXml);
    };
    let mut head = Vec::new();
    if file.take(EXTERNAL_OBJECT_SCAN_CAP).read_to_end(&mut head).is_err() {
        return Err(ExternalRootProblem::NoObjectXml);
    }
    let Some(tag) = first_element_under_metadata_object(&head) else {
        return Err(ExternalRootProblem::ObjectElementMissing);
    };
    let kind = ExternalObjectKind::from_element(&tag)
        .ok_or(ExternalRootProblem::NotAnExternalObject { tag })?;
    let object = main.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default().to_owned();
    if !dir.join(&object).is_dir() {
        return Err(ExternalRootProblem::ObjectDirMissing { object });
    }
    Ok((kind, object))
}

/// The name of the first element inside `<MetaDataObject …>`, skipping comments
/// and processing instructions. `None` when there is no `MetaDataObject` at all.
fn first_element_under_metadata_object(head: &[u8]) -> Option<String> {
    const OPEN: &[u8] = b"<MetaDataObject";
    let start = find(head, OPEN)? + OPEN.len();
    // The container's own tag ends at its `>`; the child element comes after.
    let mut i = start + find(&head[start..], b">")? + 1;
    while i < head.len() {
        let rest = &head[i..];
        if !rest.starts_with(b"<") {
            i += 1;
            continue;
        }
        if rest.starts_with(b"<!--") {
            i += 4 + find(&rest[4..], b"-->")? + 3;
            continue;
        }
        if rest.starts_with(b"<?") {
            i += 2 + find(&rest[2..], b"?>")? + 2;
            continue;
        }
        let name: Vec<u8> = rest[1..]
            .iter()
            .take_while(|b| !b.is_ascii_whitespace() && **b != b'>' && **b != b'/')
            .copied()
            .collect();
        return String::from_utf8(name).ok().filter(|name| !name.is_empty());
    }
    None
}

pub fn configuration_kind(root: &Path) -> ConfigurationKind {
    use std::io::Read as _;

    // `is_file` before opening, not just to reject a directory: opening a FIFO
    // with no writer blocks forever, and this runs on every project build —
    // including LSP startup, which would never finish.
    let Some(path) = configuration_xml_in(root) else {
        return ConfigurationKind::Unknown;
    };
    let Ok(file) = std::fs::File::open(&path) else {
        return ConfigurationKind::Unknown;
    };

    let mut reader = file.take(CONFIGURATION_KIND_SCAN_CAP);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    let mut scanned = 0usize;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return ConfigurationKind::Unknown,
        }
        // Re-scan from a little before the previous end so a marker straddling
        // two reads is still seen.
        let from = scanned.saturating_sub(EXTENSION_MARKER.len());
        match scan_for_extension_marker(&buf[from..]) {
            MarkerScan::Found => return ConfigurationKind::Extension,
            MarkerScan::PropertiesClosed => return ConfigurationKind::Configuration,
            MarkerScan::NeedMore => scanned = buf.len(),
        }
    }
    ConfigurationKind::Configuration
}

enum MarkerScan {
    Found,
    /// `</Properties>` passed without a marker: it cannot appear later.
    PropertiesClosed,
    NeedMore,
}

/// Looks for the marker as an element, skipping comments.
///
/// A raw substring search would classify a main configuration as an extension
/// when the marker merely appears inside `<!-- ... -->`, which is text and
/// declares nothing.
fn scan_for_extension_marker(head: &[u8]) -> MarkerScan {
    const COMMENT_OPEN: &[u8] = b"<!--";
    const COMMENT_CLOSE: &[u8] = b"-->";
    const PROPERTIES_CLOSE: &[u8] = b"</Properties>";

    let mut i = 0;
    while i < head.len() {
        let rest = &head[i..];
        if rest.starts_with(COMMENT_OPEN) {
            match find(&rest[COMMENT_OPEN.len()..], COMMENT_CLOSE) {
                Some(end) => i += COMMENT_OPEN.len() + end + COMMENT_CLOSE.len(),
                // Comment continues past what we have; nothing after it can be
                // read as an element yet.
                None => return MarkerScan::NeedMore,
            }
            continue;
        }
        if rest.starts_with(EXTENSION_MARKER) {
            return MarkerScan::Found;
        }
        if rest.starts_with(PROPERTIES_CLOSE) {
            return MarkerScan::PropertiesClosed;
        }
        i += 1;
    }
    MarkerScan::NeedMore
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The `<CompatibilityMode>` of a MAIN configuration's `Configuration.xml`, as
/// written there (`Version8_2_13`, `Version8_3_17`, `DontUse`).
///
/// `None` for an extension (its modules run under the mode of the configuration
/// they extend), for a missing or unreadable file, and when `</Properties>` closes
/// without the element. Comments are skipped, the read is bounded by
/// [`CONFIGURATION_KIND_SCAN_CAP`] like [`configuration_kind`].
pub fn configuration_compatibility_mode(root: &Path) -> Option<String> {
    use std::io::Read as _;

    let path = configuration_xml_in(root)?;
    let file = std::fs::File::open(&path).ok()?;
    let mut head = Vec::new();
    file.take(CONFIGURATION_KIND_SCAN_CAP).read_to_end(&mut head).ok()?;
    compatibility_mode_in(&head)
}

fn compatibility_mode_in(head: &[u8]) -> Option<String> {
    const COMMENT_OPEN: &[u8] = b"<!--";
    const COMMENT_CLOSE: &[u8] = b"-->";
    const PROPERTIES_CLOSE: &[u8] = b"</Properties>";
    const MODE_OPEN: &[u8] = b"<CompatibilityMode>";

    let mut i = 0;
    while i < head.len() {
        let rest = &head[i..];
        if rest.starts_with(COMMENT_OPEN) {
            i += COMMENT_OPEN.len() + find(&rest[COMMENT_OPEN.len()..], COMMENT_CLOSE)?;
            i += COMMENT_CLOSE.len();
            continue;
        }
        if rest.starts_with(EXTENSION_MARKER) || rest.starts_with(PROPERTIES_CLOSE) {
            return None;
        }
        if rest.starts_with(MODE_OPEN) {
            let value = &rest[MODE_OPEN.len()..];
            let end = find(value, b"<")?;
            let value = std::str::from_utf8(&value[..end]).ok()?.trim();
            return (!value.is_empty()).then(|| value.to_owned());
        }
        i += 1;
    }
    None
}

/// One entry of the `extensions` list: either a bare path string (legacy,
/// independent extension) or a structured entry with a stable name and
/// declared dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionDecl {
    Path(String),
    Structured(StructuredExtensionDecl),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredExtensionDecl {
    pub name: String,
    pub path: String,
    pub depends_on: Vec<String>,
}

/// One entry of `[source] externals`: an external data processor or report
/// export, declared by name. Only the named form exists, and nothing about a
/// declared entry is lenient: a missing path or a directory that is not one
/// export fails project construction.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalDecl {
    pub name: String,
    pub path: String,
    /// The extensions the object sees, by name, transitively. Without the key
    /// it sees every extension of the project; with it, exactly the closure of
    /// the listed ones — an empty list is the base alone.
    #[serde(default, alias = "dependsOn")]
    pub depends_on: Option<Vec<String>>,
}

/// Source-set fields supplied outside the config file — today, by CLI flags.
///
/// Applied as a mutation of [`ProjectConfig`] rather than passed to a `Project`
/// constructor, because the config is read by more than the constructors: the
/// analyze path resolves `configuration_path` and loads metadata from the raw
/// config before building the project, and that path feeds the interned
/// configuration input used by diagnostics. An override living inside the
/// constructor would leave those reads on the file-declared source set.
///
/// Fields override independently: a field that is set replaces the config's
/// field outright, a field left unset keeps whatever the config declared. Paths
/// are strings for the same reason the config stores them as strings — the
/// whole model is string-based, so anything else would convert at every
/// boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceSetOverride {
    /// Replaces [`ProjectConfig::configuration_root`].
    pub configuration_root: Option<String>,
    /// Replaces [`ProjectConfig::extensions`], including the tri-state:
    /// `Some(vec![])` is an explicit "no extensions", distinct from leaving the
    /// field unset and inheriting discovery.
    pub extensions: Option<Vec<ExtensionDecl>>,
    /// Replaces [`ProjectConfig::externals`].
    pub externals: Option<Vec<ExternalDecl>>,
}

impl SourceSetOverride {
    /// True when nothing is overridden, i.e. applying this is a no-op.
    pub fn is_empty(&self) -> bool {
        self.configuration_root.is_none() && self.extensions.is_none() && self.externals.is_none()
    }

    /// Overwrites the config's source-set fields with the ones set here.
    pub fn apply_to(&self, config: &mut ProjectConfig) {
        if let Some(ref root) = self.configuration_root {
            config.configuration_root = Some(root.clone());
            config.configuration_dependency = None;
        }
        if let Some(ref extensions) = self.extensions {
            config.extensions = Some(extensions.clone());
        }
        if let Some(ref externals) = self.externals {
            config.externals = Some(externals.clone());
        }
    }
}

impl From<&str> for ExtensionDecl {
    fn from(path: &str) -> Self {
        ExtensionDecl::Path(path.to_string())
    }
}

impl From<String> for ExtensionDecl {
    fn from(path: String) -> Self {
        ExtensionDecl::Path(path)
    }
}

// Hand-written instead of `#[serde(untagged)]`: the derived untagged impl
// discards each variant's real error and reports only "data did not match any
// variant", which turns a typo inside a table into an unusable message. The
// two shapes are disjoint (string vs table), so dispatching in a visitor keeps
// serde's precise unknown-field / missing-field / wrong-type errors.
impl<'de> Deserialize<'de> for ExtensionDecl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct DeclVisitor;

        const FIELDS: &[&str] = &["name", "path", "dependsOn", "depends_on"];

        impl<'de> serde::de::Visitor<'de> for DeclVisitor {
            type Value = ExtensionDecl;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an extension path string or a table { name, path, dependsOn = [...] }")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ExtensionDecl::Path(value.to_string()))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(ExtensionDecl::Path(value))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                use serde::de::Error as _;
                let mut name: Option<String> = None;
                let mut path: Option<String> = None;
                let mut depends_on: Option<Vec<String>> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            if name.is_some() {
                                return Err(A::Error::duplicate_field("name"));
                            }
                            name = Some(map.next_value()?);
                        }
                        "path" => {
                            if path.is_some() {
                                return Err(A::Error::duplicate_field("path"));
                            }
                            path = Some(map.next_value()?);
                        }
                        "dependsOn" | "depends_on" => {
                            if depends_on.is_some() {
                                return Err(A::Error::duplicate_field("dependsOn"));
                            }
                            depends_on = Some(map.next_value()?);
                        }
                        other => return Err(A::Error::unknown_field(other, FIELDS)),
                    }
                }
                Ok(ExtensionDecl::Structured(StructuredExtensionDecl {
                    name: name.ok_or_else(|| A::Error::missing_field("name"))?,
                    path: path.ok_or_else(|| A::Error::missing_field("path"))?,
                    depends_on: depends_on.unwrap_or_default(),
                }))
            }
        }

        deserializer.deserialize_any(DeclVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConfigurationDependency {
    pub id: String,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfig {
    #[serde(default)]
    pub diagnostics: ProjectDiagnosticsConfig,

    #[serde(skip)]
    #[doc(hidden)]
    pub config_file_path: Option<PathBuf>,

    #[serde(default)]
    pub code_lens: CodeLensConfig,

    #[serde(default)]
    pub formatting: FormattingConfig,

    #[serde(default)]
    pub configuration_root: Option<String>,

    #[serde(default, rename = "configuration")]
    pub configuration_dependency: Option<ConfigurationDependency>,

    /// `[source].exclude`: directories taken out of the project, as literal paths
    /// relative to the directory of the config file. Only the TOML config carries it.
    #[serde(skip)]
    pub source_exclude: Vec<String>,

    #[serde(default, alias = "target_platform_version")]
    pub target_platform_version: Option<String>,

    /// `[platform_help]`: where the platform help corpus comes from. `None`
    /// (unset) leaves the choice to the build's default source.
    #[serde(default, alias = "platform_help")]
    pub platform_help: Option<PlatformHelpConfig>,

    /// Oldest 1C platform release the code must still compile on (production may
    /// run an older platform than the one development happens on). Unset keeps
    /// `PlatformMemberNewerThanMinVersion` silent; it never changes what
    /// `target_platform_version` selects.
    #[serde(default, alias = "min_platform_version")]
    pub min_platform_version: Option<String>,

    /// The compatibility mode the code runs under (`8.2.13`, `Version8_2_13`,
    /// `DontUse`). Overrides the `<CompatibilityMode>` of the main configuration's
    /// `Configuration.xml` — for sources exported from a copy whose mode differs
    /// from the customer's. See [`Project::compatibility_mode`].
    #[serde(default, alias = "compatibility_mode")]
    pub compatibility_mode: Option<String>,

    #[serde(default)]
    pub language: Option<String>,

    /// Configuration extensions. `None` (unset) auto-discovers the conventional
    /// `src/cfe/*` layout; `Some([])` is an explicit opt-out (no extensions);
    /// a non-empty list is taken verbatim (string entries may use a
    /// final-segment `*` glob; structured entries add a name and dependencies).
    #[serde(default)]
    pub extensions: Option<Vec<ExtensionDecl>>,

    /// External data processors and reports (EPF/ERF exports) analyzed against
    /// this project's main configuration. `None` (unset) auto-discovers the
    /// conventional `src/epf/*` and `src/erf/*` layouts; `Some([])` is an
    /// explicit opt-out; a non-empty list is taken verbatim.
    #[serde(default)]
    pub externals: Option<Vec<ExternalDecl>>,

    #[serde(default)]
    pub search: SearchConfig,

    #[serde(default)]
    pub features: FeaturesConfig,

    #[serde(default)]
    pub output: OutputConfig,

    #[serde(default)]
    pub analysis: AnalysisConfig,
}

/// `[platform_help]` — the source of the platform help corpus. The process loads
/// it once at startup; a changed value takes effect after a restart. Field-less
/// sources are empty struct variants so that a stray field is refused rather
/// than ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase", deny_unknown_fields)]
pub enum PlatformHelpConfig {
    /// The interface facts compiled into this build, without 1C's texts.
    Bundled {},
    /// HBK archives of an installed platform. Without `path`, the
    /// `BSL_PLATFORM_PATH` directory, then the conventional install locations.
    Installed {
        #[serde(default)]
        path: Option<String>,
    },
    /// A prepared corpus: exactly one of a package/JSON `path` or a manifest `url`.
    External {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
    /// The snapshot saved for `auto`, else an installed platform, else the
    /// built-in interface facts.
    Auto {},
    /// No platform help; nothing is searched, extracted or downloaded.
    None {},
}

/// [`PlatformHelpConfig`] with its paths resolved against the config directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformHelpSelection {
    Bundled,
    Installed { path: Option<PathBuf> },
    ExternalPath(PathBuf),
    ExternalUrl(String),
    Auto,
    None,
}

/// `[analysis]` — restricting the set of files/lines diagnostics are reported
/// for. Affects diagnostics only; indexing and type inference still cover the
/// whole configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisConfig {
    /// Git ref (typically the vendor branch) to diff against: only files/lines
    /// that differ from `merge-base(diff_base, HEAD)` get diagnostics.
    /// `None` = no restriction.
    #[serde(default)]
    pub diff_base: Option<String>,

    /// Suppress diagnostics on lines whose git-blame author (name or email,
    /// exact match) is listed here — "show findings only on our own code".
    /// Empty = no restriction. Applied by CLI `analyze` and MCP; the LSP does
    /// not apply it yet and warns when it is configured.
    #[serde(default)]
    pub ignored_authors: Vec<String>,
}

/// The conventional project-config file names at a workspace root, in load
/// precedence order — the files [`ProjectConfig::load`] reads. This is NOT the
/// set that decides whether a change re-derives the project: use
/// [`is_project_input_file_name`] for that.
pub const CONFIG_FILE_NAMES: [&str; 3] =
    ["bsl-analyzer.toml", ".bsl-analyzer.json", ".bsl-language-server.json"];
pub const PROJECT_ENV_FILE_NAME: &str = ".env";
/// Every file at a workspace root the project is derived from: the config files
/// plus the environment file that carries machine-local values such as the
/// shared-configuration store root.
pub const PROJECT_INPUT_FILE_NAMES: [&str; 4] =
    ["bsl-analyzer.toml", ".bsl-analyzer.json", ".bsl-language-server.json", PROJECT_ENV_FILE_NAME];

/// Whether a file name is one the project is derived from, and therefore whether
/// a change to it must re-derive the project.
///
/// Every host that turns a file event into a project reload asks this, rather
/// than comparing against its own copy of the names: a second copy is a second
/// answer, and the one that drifts is silently the one that stops reacting.
pub fn is_project_input_file_name(name: &str) -> bool {
    PROJECT_INPUT_FILE_NAMES.contains(&name)
}

impl ProjectConfig {
    /// Loads the project config from the conventional file names under `root`.
    /// `Ok(None)` means no config file exists (callers default); a file that
    /// exists but cannot be read or parsed is an error — falling back to a
    /// default config would silently analyze a differently-shaped project.
    pub fn load(root: &Path) -> Result<Option<Self>, ConfigLoadError> {
        let toml_path = root.join(CONFIG_FILE_NAMES[0]);
        if toml_path.exists() {
            return Self::load_from_file(&toml_path).map(Some);
        }
        for filename in [CONFIG_FILE_NAMES[1], CONFIG_FILE_NAMES[2]] {
            let config_path = root.join(filename);
            if config_path.exists() {
                let config = Self::load_from_file(&config_path)?;
                tracing::info!(
                    path = %config_path.display(),
                    diagnostics_has_content = !config.diagnostics.is_empty(),
                    "loaded project config"
                );
                return Ok(Some(config));
            }
        }
        Ok(None)
    }

    pub fn load_from_file(path: &Path) -> Result<Self, ConfigLoadError> {
        let err = |message: String| ConfigLoadError { path: path.to_path_buf(), message };
        if !path.exists() {
            return Err(err("file not found".to_string()));
        }
        let path = std::path::absolute(path).map_err(|error| err(error.to_string()))?;
        let content = std::fs::read_to_string(&path).map_err(|e| err(e.to_string()))?;
        if path.extension().is_some_and(|ext| ext == "toml") {
            let toml_config =
                toml::from_str::<TomlConfig>(&content).map_err(|e| err(e.to_string()))?;
            let mut config = ProjectConfig::from(toml_config);
            config.config_file_path = Some(path.to_path_buf());
            config.validate_configuration_source()?;
            tracing::info!(
                path = %path.display(),
                diagnostics_has_content = !config.diagnostics.is_empty(),
                "loaded TOML project config"
            );
            Ok(config)
        } else {
            let mut config: ProjectConfig =
                serde_json::from_str(&content).map_err(|e| err(e.to_string()))?;
            config.config_file_path = Some(path.to_path_buf());
            config.validate_configuration_source()?;
            Ok(config)
        }
    }

    pub fn config_file_path(&self) -> Option<&Path> {
        self.config_file_path.as_deref()
    }

    /// Resolve the local tokenizer artifact from its declaring config directory.
    pub fn search_tokenizer_file(&self, project_root: &Path) -> Option<PathBuf> {
        self.search.baseline.embedding.tokenizer_file.as_deref().map(|path| {
            self.config_file_path
                .as_deref()
                .and_then(Path::parent)
                .unwrap_or(project_root)
                .join(path)
        })
    }

    fn config_error(&self, message: String) -> ConfigLoadError {
        ConfigLoadError {
            path: self
                .config_file_path
                .clone()
                .unwrap_or_else(|| PathBuf::from(CONFIG_FILE_NAMES[0])),
            message,
        }
    }

    fn validate_configuration_source(&self) -> Result<(), ConfigLoadError> {
        self.validate_source_exclude()?;
        self.validate_platform_help()?;
        if self.configuration_root.is_some() && self.configuration_dependency.is_some() {
            return Err(
                self.config_error("[source] cannot define both root and configuration".to_owned())
            );
        }
        if let Some(configuration) = self.configuration_dependency.as_ref() {
            validate_configuration_component(&configuration.id).map_err(|message| {
                self.config_error(format!("source.configuration.id {message}"))
            })?;
            if let Some(version) = configuration.version.as_deref() {
                validate_configuration_component(version).map_err(|message| {
                    self.config_error(format!("source.configuration.version {message}"))
                })?;
            }
        }
        Ok(())
    }

    fn validate_platform_help(&self) -> Result<(), ConfigLoadError> {
        let empty = |value: &Option<String>| value.as_deref().is_some_and(|v| v.trim().is_empty());
        match &self.platform_help {
            Some(PlatformHelpConfig::Installed { path }) if empty(path) => {
                Err(self.config_error("platform_help.path must not be empty".to_owned()))
            }
            Some(PlatformHelpConfig::External { path, url }) => {
                if empty(path) || empty(url) {
                    return Err(self.config_error(
                        "platform_help.path and platform_help.url must not be empty".to_owned(),
                    ));
                }
                match (path, url) {
                    (Some(_), None) | (None, Some(_)) => Ok(()),
                    _ => Err(self.config_error(
                        "platform_help source \"external\" needs exactly one of path or url"
                            .to_owned(),
                    )),
                }
            }
            _ => Ok(()),
        }
    }

    /// The configured help source with paths resolved against the directory of the
    /// config file (or `project_root` for a config not read from a file); `None`
    /// when the configuration leaves the choice to the build default.
    pub fn platform_help_selection(
        &self,
        project_root: &Path,
    ) -> Result<Option<PlatformHelpSelection>, ConfigLoadError> {
        self.validate_platform_help()?;
        let Some(config) = &self.platform_help else {
            return Ok(None);
        };
        let resolve = |value: &str| {
            let path = self.resolve_config_path(project_root, value);
            std::path::absolute(&path).unwrap_or(path)
        };
        Ok(Some(match config {
            PlatformHelpConfig::Bundled {} => PlatformHelpSelection::Bundled,
            PlatformHelpConfig::Installed { path } => {
                PlatformHelpSelection::Installed { path: path.as_deref().map(resolve) }
            }
            PlatformHelpConfig::External { path: Some(path), .. } => {
                PlatformHelpSelection::ExternalPath(resolve(path))
            }
            PlatformHelpConfig::External { url: Some(url), .. } => {
                PlatformHelpSelection::ExternalUrl(url.clone())
            }
            PlatformHelpConfig::External { path: None, url: None } => {
                unreachable!("validated: external has a path or a url")
            }
            PlatformHelpConfig::Auto {} => PlatformHelpSelection::Auto,
            PlatformHelpConfig::None {} => PlatformHelpSelection::None,
        }))
    }

    /// Each `[source].exclude` value must name a directory relative to the config file.
    /// An absolute path or an empty value is refused rather than ignored: a silently
    /// dropped exclusion would put the very tree the user meant to hide back into
    /// the project.
    fn validate_source_exclude(&self) -> Result<(), ConfigLoadError> {
        for value in &self.source_exclude {
            if value.trim().is_empty() {
                return Err(
                    self.config_error("source.exclude entries must not be empty".to_owned())
                );
            }
            let path = Path::new(value);
            // A drive-relative Windows value (`C:tmp`) is neither absolute nor rooted,
            // yet `join` would let it replace the config directory it must be read from.
            let prefixed = path.components().any(|c| matches!(c, std::path::Component::Prefix(_)));
            if path.is_absolute() || path.has_root() || prefixed {
                return Err(self.config_error(format!(
                    "source.exclude entry {value:?} must be a path relative to the config file"
                )));
            }
        }
        Ok(())
    }

    /// `[source].exclude` resolved against the directory of the config file — not the
    /// source root and not the working directory — or against `project_root` for a
    /// config that did not come from a file.
    pub fn source_exclusions(
        &self,
        project_root: &Path,
    ) -> Result<stdx::path_exclusion::ExcludedPaths, ConfigLoadError> {
        self.validate_source_exclude()?;
        Ok(stdx::path_exclusion::ExcludedPaths::new(
            self.source_exclude.iter().map(|value| self.resolve_config_path(project_root, value)),
        ))
    }

    pub fn resolve_configuration_path(
        &self,
        project_root: &Path,
    ) -> Result<Option<PathBuf>, ConfigLoadError> {
        self.validate_configuration_source()?;
        if let Some(root) = self.configuration_root.as_deref() {
            return Ok(Some(project_root.join(root)));
        }
        let Some(configuration) = self.configuration_dependency.as_ref() else {
            return Ok(None);
        };
        let env_file = self
            .config_file_path
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or(project_root)
            .join(".env");
        let shared_root = match env::var_os(SHARED_CONFIGURATIONS_ROOT_ENV) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => dotenv_value(&env_file, SHARED_CONFIGURATIONS_ROOT_ENV)
                .map_err(|message| self.config_error(message))?
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| {
                    self.config_error(format!(
                        "source.configuration.id={:?} requires {SHARED_CONFIGURATIONS_ROOT_ENV}; set it in the process environment or {}",
                        configuration.id,
                        env_file.display()
                    ))
                })?,
        };
        // Anchored to the project exactly like `[source].root` is: an absolute
        // value passes through `join` untouched, while a relative one — the natural
        // way to point at a store next to the checkout — stops depending on the
        // process working directory. Every consumer of a source root demands an
        // absolute path, and the LSP aborts on anything else.
        let shared_root = project_root.join(shared_root);
        let mut resolved = shared_root.join(&configuration.id);
        if let Some(version) = configuration.version.as_deref() {
            resolved.push(version);
        }
        Ok(Some(resolved))
    }

    pub fn diagnostics_baseline_path(&self, project_root: &Path) -> Option<PathBuf> {
        let path = self.diagnostics.baseline.as_ref()?.path.as_ref()?;
        Some(self.resolve_config_path(project_root, path))
    }

    pub fn diagnostics_baseline_directory(&self, project_root: &Path) -> Option<PathBuf> {
        let path = self.diagnostics.baseline.as_ref()?.directory.as_ref()?;
        Some(self.resolve_config_path(project_root, path))
    }

    fn resolve_config_path(&self, project_root: &Path, path: &str) -> PathBuf {
        let base = self.config_file_path.as_deref().and_then(Path::parent).unwrap_or(project_root);
        base.join(path)
    }

    pub fn configuration_path(&self, project_root: &Path) -> Option<PathBuf> {
        self.configuration_root.as_ref().map(|root| project_root.join(root))
    }

    pub fn load_metadata(
        &self,
        workspace_root: &Path,
    ) -> Result<Option<bsl_metadata::Configuration>, ConfigLoadError> {
        let Some(cfg_path) = self.resolve_configuration_path(workspace_root)? else {
            return Ok(None);
        };

        if !cfg_path.exists() {
            tracing::warn!(path = ?cfg_path, "Configuration root not found");
            return Ok(None);
        }

        tracing::info!(path = ?cfg_path, "Loading 1C metadata");
        let start = std::time::Instant::now();

        // The same exclusions every other reader of this project honours: nothing inside
        // them is listed or read, and an excluded root loads as an empty configuration.
        let excluded = self.source_exclusions(workspace_root)?;
        match bsl_metadata::load_from_directory_scoped(&cfg_path, &excluded) {
            Ok(config) => {
                let elapsed = start.elapsed();
                tracing::info!(
                    elapsed_ms = elapsed.as_millis(),
                    common_modules = config.common_modules().len(),
                    "1C metadata loaded"
                );
                Ok(Some(config))
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load 1C metadata");
                Ok(None)
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectDiagnosticsConfig {
    #[serde(default)]
    pub baseline: Option<DiagnosticsBaselineConfig>,
    #[serde(flatten)]
    rules: serde_json::Map<String, serde_json::Value>,
}

impl ProjectDiagnosticsConfig {
    pub fn rules_json(&self) -> serde_json::Value {
        serde_json::Value::Object(self.rules.clone())
    }

    fn is_empty(&self) -> bool {
        self.baseline.is_none() && self.rules.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineConfig {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub directory: Option<String>,
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub groups: Vec<DiagnosticsBaselineGroupConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineGroupConfig {
    pub name: String,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeLensConfig {
    #[serde(default, alias = "show_cognitive_complexity")]
    pub show_cognitive_complexity: bool,

    #[serde(default, alias = "show_cyclomatic_complexity")]
    pub show_cyclomatic_complexity: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FormattingConfig {
    #[serde(default = "default_indent_size")]
    pub indent_size: u32,

    #[serde(default)]
    pub use_tabs: bool,
}

fn default_indent_size() -> u32 {
    4
}

/// Scope of the LSP pull `workspace/diagnostic` feature.
///
/// `Off` (the default) keeps the server on push-only diagnostics for open buffers:
/// no pull diagnostic provider is advertised, so there is zero behavior change.
/// `Extensions` reports diagnostics only for files under configuration-extension
/// roots — the base configuration stays loaded so cross-references resolve, but is
/// not itself reported. `All` reports the whole configuration, which is expensive on
/// large configs and therefore strictly opt-in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceDiagnosticsScope {
    #[default]
    Off,
    Extensions,
    All,
}

impl WorkspaceDiagnosticsScope {
    /// Whether any pull diagnostic provider should be advertised at all.
    pub fn is_enabled(self) -> bool {
        !matches!(self, WorkspaceDiagnosticsScope::Off)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FeaturesConfig {
    #[serde(default = "default_true", alias = "type_narrowing")]
    pub type_narrowing: bool,

    #[serde(default, alias = "workspace_diagnostics")]
    pub workspace_diagnostics: WorkspaceDiagnosticsScope,

    /// Execution environments the availability diagnostics
    /// (`UnavailableInEnvironment`, `ModuleAccessibility`) report violations
    /// for, named like preprocessor symbols (`ВебКлиент`/`WebClient`,
    /// `ТонкийКлиент`, `Сервер`, …). A configuration that never runs in the
    /// web client lists everything except `ВебКлиент`. `None` keeps the
    /// default set (thin client, web client, managed thick client, server).
    #[serde(default, alias = "checked_environments")]
    pub checked_environments: Option<Vec<String>>,
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            type_narrowing: true,
            workspace_diagnostics: WorkspaceDiagnosticsScope::default(),
            checked_environments: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputConfig {
    #[serde(default, alias = "display_language")]
    pub display_language: Option<String>,
}

impl OutputConfig {
    pub fn resolve_locale(&self) -> Option<base_db::Locale> {
        let raw = self.display_language.as_deref()?;
        match base_db::Locale::from_config_str(raw) {
            Ok(locale) => Some(locale),
            Err(e) => {
                tracing::warn!(
                    value = %e.0,
                    "[output] display_language has unknown value; ignoring (will use other locale signals)"
                );
                None
            }
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchConfig {
    #[serde(default)]
    pub baseline: SearchBaselineConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchBaselineConfig {
    #[serde(default)]
    pub backend: SearchBaselineBackend,

    #[serde(default)]
    pub postgres: SearchPostgresConfig,

    #[serde(default)]
    pub embedding: SearchEmbeddingConfig,

    #[serde(default, alias = "workspace_code")]
    pub workspace_code: SearchBaselineTargetConfig,

    #[serde(default)]
    pub reference: SearchBaselineTargetConfig,
}

/// Declares the embedding identity (model + dimension) of a shared search
/// baseline so it is pinned in committed config rather than per-developer env.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchEmbeddingConfig {
    #[serde(default)]
    pub model: Option<String>,

    #[serde(default)]
    pub dimension: Option<usize>,

    #[serde(default)]
    pub provider: Option<String>,

    #[serde(default)]
    pub url: Option<String>,

    /// Literal model input prefixes. `Some("")` deliberately disables an environment value.
    #[serde(default)]
    pub query_prefix: Option<String>,

    #[serde(default)]
    pub document_prefix: Option<String>,

    /// Opt-in exact input token ceiling. It is ignored unless tokenizer path and hash
    /// are also supplied by project config or the corresponding environment values.
    #[serde(default)]
    pub max_input_tokens: Option<toml::Value>,

    /// Local tokenizer artifact path. Relative project values use the config directory;
    /// relative environment values are resolved by the workspace runtime.
    #[serde(default)]
    pub tokenizer_file: Option<String>,

    /// Expected SHA256 of the local tokenizer artifact.
    #[serde(default)]
    pub tokenizer_sha256: Option<String>,
}

impl SearchEmbeddingConfig {
    pub fn resolve_query_prefix(&self, environment: Option<&str>) -> String {
        self.query_prefix.as_deref().or(environment).unwrap_or_default().to_owned()
    }

    pub fn resolve_document_prefix(&self, environment: Option<&str>) -> String {
        self.document_prefix.as_deref().or(environment).unwrap_or_default().to_owned()
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SearchBaselineBackend {
    #[default]
    Sqlite,
    Postgres,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPostgresConfig {
    #[serde(default)]
    pub host: Option<String>,

    #[serde(default)]
    pub port: Option<u16>,

    #[serde(default)]
    pub dbname: Option<String>,

    #[serde(default)]
    pub schema: Option<String>,

    #[serde(default)]
    pub vault_role_base: Option<String>,

    #[serde(default)]
    pub credential_helper: SearchPostgresCredentialHelperConfig,
}

impl SearchPostgresConfig {
    pub fn is_configured(&self) -> bool {
        self.host.is_some()
            || self.port.is_some()
            || self.dbname.is_some()
            || self.schema.is_some()
            || self.vault_role_base.is_some()
            || self.credential_helper.program.is_some()
            || !self.credential_helper.args.is_empty()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPostgresCredentialHelperConfig {
    #[serde(default)]
    pub program: Option<String>,

    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchBaselineTargetConfig {
    #[serde(default, alias = "snapshot_id")]
    pub snapshot_id: Option<String>,

    #[serde(default)]
    pub branch: Option<String>,

    #[serde(default)]
    pub commit: Option<String>,

    #[serde(default)]
    pub policy: SearchBaselinePolicyConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchBaselinePolicyConfig {
    #[serde(default, alias = "publish_branches")]
    pub publish_branches: Vec<String>,

    #[serde(default)]
    pub branches: Vec<SearchBaselineBranchPolicyRuleConfig>,

    #[serde(default)]
    pub support: SearchBaselineSupportConfig,

    #[serde(default)]
    pub retention: SearchBaselineRetentionConfig,
}

impl SearchBaselinePolicyConfig {
    pub fn is_configured(&self) -> bool {
        !self.publish_branches.is_empty() || !self.branches.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchBaselineSupportConfig {
    #[serde(default = "default_workspace_stale_after_days", alias = "stale_after_days")]
    pub stale_after_days: u32,

    #[serde(default = "default_workspace_expire_after_days", alias = "expire_after_days")]
    pub expire_after_days: u32,
}

impl Default for SearchBaselineSupportConfig {
    fn default() -> Self {
        Self {
            stale_after_days: default_workspace_stale_after_days(),
            expire_after_days: default_workspace_expire_after_days(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchBaselineRetentionConfig {
    #[serde(default = "default_develop_retention_days", alias = "develop_retention_days")]
    pub develop_retention_days: u32,

    #[serde(default = "default_vendor_keep_heads", alias = "vendor_keep_heads")]
    pub vendor_keep_heads: usize,

    #[serde(default = "default_min_snapshots_per_branch", alias = "min_snapshots_per_branch")]
    pub min_snapshots_per_branch: usize,
}

impl Default for SearchBaselineRetentionConfig {
    fn default() -> Self {
        Self {
            develop_retention_days: default_develop_retention_days(),
            vendor_keep_heads: default_vendor_keep_heads(),
            min_snapshots_per_branch: default_min_snapshots_per_branch(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchBaselineSupportState {
    Supported,
    Stale,
    Expired,
}

impl SearchBaselineSupportState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Stale => "stale",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkspaceBaselineSupport {
    pub state: SearchBaselineSupportState,
    pub workspace_branch: Option<String>,
    pub selected_branch: Option<String>,
    pub snapshot_age_days: u32,
    pub stale_after_days: u32,
    pub expire_after_days: u32,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchBaselineBranchPolicyRuleConfig {
    #[serde(rename = "match")]
    pub pattern: String,

    #[serde(alias = "select_branch")]
    pub select_branch: String,

    #[serde(default, alias = "fallback_branch")]
    pub fallback_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkspaceBranchPolicy {
    pub workspace_branch: Option<String>,
    pub matched_pattern: String,
    pub select_branch: String,
    pub fallback_branch: Option<String>,
}

impl ResolvedWorkspaceBranchPolicy {
    pub fn candidate_branches(&self) -> Vec<String> {
        let mut branches = vec![self.select_branch.clone()];
        if let Some(fallback_branch) = &self.fallback_branch {
            if branches.iter().all(|branch| branch != fallback_branch) {
                branches.push(fallback_branch.clone());
            }
        }
        branches
    }

    pub fn selection_description(&self) -> String {
        let workspace_branch = self.workspace_branch.as_deref().unwrap_or("<unknown>");
        let chain = self
            .candidate_branches()
            .into_iter()
            .map(|branch| format!("branch {branch}"))
            .collect::<Vec<_>>()
            .join(" -> ");
        format!("workspace branch {workspace_branch} -> {chain}")
    }
}

pub fn resolve_workspace_branch_policy(
    policy: &SearchBaselinePolicyConfig,
    workspace_branch: Option<&str>,
) -> Option<ResolvedWorkspaceBranchPolicy> {
    let workspace_branch =
        workspace_branch.map(str::trim).filter(|branch| !branch.is_empty()).map(ToOwned::to_owned);
    let rule = policy
        .branches
        .iter()
        .find(|rule| branch_pattern_matches(&rule.pattern, workspace_branch.as_deref()))?;

    Some(ResolvedWorkspaceBranchPolicy {
        workspace_branch,
        matched_pattern: rule.pattern.clone(),
        select_branch: rule.select_branch.clone(),
        fallback_branch: rule.fallback_branch.clone(),
    })
}

pub fn is_publish_branch_allowed(policy: &SearchBaselinePolicyConfig, branch: &str) -> bool {
    let branch = branch.trim();
    !branch.is_empty()
        && policy
            .publish_branches
            .iter()
            .any(|pattern| branch_pattern_matches(pattern, Some(branch)))
}

pub fn branch_pattern_matches(pattern: &str, branch: Option<&str>) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }

    let Some(branch) = branch.map(str::trim).filter(|branch| !branch.is_empty()) else {
        return false;
    };

    if let Some(prefix) = pattern.strip_suffix("/*") {
        return branch
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/') && suffix.len() > 1);
    }

    branch == pattern
}

pub fn current_git_branch(start_dir: &Path) -> Option<String> {
    let git_dir = discover_git_dir(start_dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let ref_path = head.trim().strip_prefix("ref: ")?;
    ref_path.strip_prefix("refs/heads/").map(ToOwned::to_owned)
}

pub fn current_git_commit(start_dir: &Path) -> Option<String> {
    let git_dir = discover_git_dir(start_dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(ref_path) = head.strip_prefix("ref: ") {
        return std::fs::read_to_string(git_dir.join(ref_path))
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
    }

    (!head.is_empty()).then(|| head.to_owned())
}

pub fn parse_timestamp_utc(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    DateTime::parse_from_rfc3339(value)
        .or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f%:z"))
        .or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%:z"))
        .map(|value| value.with_timezone(&Utc))
        .ok()
}

pub fn evaluate_workspace_baseline_support_now(
    policy: &SearchBaselinePolicyConfig,
    workspace_branch: Option<&str>,
    selected_branch: Option<&str>,
    snapshot_created_at: Option<DateTime<Utc>>,
) -> Option<ResolvedWorkspaceBaselineSupport> {
    evaluate_workspace_baseline_support(
        policy,
        workspace_branch,
        selected_branch,
        snapshot_created_at,
        Utc::now(),
    )
}

pub fn evaluate_workspace_baseline_support(
    policy: &SearchBaselinePolicyConfig,
    workspace_branch: Option<&str>,
    selected_branch: Option<&str>,
    snapshot_created_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<ResolvedWorkspaceBaselineSupport> {
    if !policy.is_configured() {
        return None;
    }

    let snapshot_created_at = snapshot_created_at?;
    let selected_branch =
        selected_branch.map(str::trim).filter(|branch| !branch.is_empty()).map(ToOwned::to_owned);
    let workspace_branch =
        workspace_branch.map(str::trim).filter(|branch| !branch.is_empty()).map(ToOwned::to_owned);

    let stale_after_days = policy.support.stale_after_days.min(policy.support.expire_after_days);
    let expire_after_days = policy.support.expire_after_days.max(stale_after_days);
    let age_days = now.signed_duration_since(snapshot_created_at).num_days().max(0) as u32;
    let state = if age_days >= expire_after_days {
        SearchBaselineSupportState::Expired
    } else if age_days >= stale_after_days {
        SearchBaselineSupportState::Stale
    } else {
        SearchBaselineSupportState::Supported
    };

    let reason = match (workspace_branch.as_deref(), selected_branch.as_deref()) {
        (Some(workspace_branch), Some(selected_branch)) if workspace_branch != selected_branch => {
            format!(
                "workspace branch '{workspace_branch}' uses shared baseline branch '{selected_branch}' published {age_days} days ago"
            )
        }
        (Some(workspace_branch), _) => {
            format!("workspace branch '{workspace_branch}' uses a shared baseline published {age_days} days ago")
        }
        (None, Some(selected_branch)) => {
            format!("shared baseline branch '{selected_branch}' was published {age_days} days ago")
        }
        (None, None) => format!("shared baseline was published {age_days} days ago"),
    };

    Some(ResolvedWorkspaceBaselineSupport {
        state,
        workspace_branch,
        selected_branch,
        snapshot_age_days: age_days,
        stale_after_days,
        expire_after_days,
        reason,
    })
}

fn default_workspace_stale_after_days() -> u32 {
    21
}

fn default_workspace_expire_after_days() -> u32 {
    30
}

fn default_develop_retention_days() -> u32 {
    30
}

fn default_vendor_keep_heads() -> usize {
    2
}

fn default_min_snapshots_per_branch() -> usize {
    1
}

fn discover_git_dir(start_dir: &Path) -> Option<PathBuf> {
    for candidate in start_dir.ancestors() {
        let dot_git = candidate.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let path = content.trim().strip_prefix("gitdir: ")?;
            let git_dir = candidate.join(path);
            if git_dir.exists() {
                return Some(git_dir);
            }
        }
    }

    None
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlConfig {
    #[serde(default)]
    source: TomlSourceConfig,
    #[serde(default)]
    analysis: TomlAnalysisConfig,
    #[serde(default)]
    diagnostics: ProjectDiagnosticsConfig,
    #[serde(default)]
    code_lens: CodeLensConfig,
    #[serde(default)]
    formatting: FormattingConfig,
    #[serde(default)]
    target_platform_version: Option<String>,
    #[serde(default)]
    platform_help: Option<PlatformHelpConfig>,
    #[serde(default)]
    min_platform_version: Option<String>,
    #[serde(default)]
    compatibility_mode: Option<String>,
    #[serde(default)]
    search: TomlSearchConfig,
    #[serde(default)]
    features: FeaturesConfig,
    #[serde(default)]
    output: OutputConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlSourceConfig {
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    configuration: Option<ConfigurationDependency>,
    #[serde(default)]
    extensions: Option<Vec<ExtensionDecl>>,
    #[serde(default)]
    externals: Option<Vec<ExternalDecl>>,
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlAnalysisConfig {
    #[serde(default)]
    diff_base: Option<String>,
    #[serde(default)]
    ignored_authors: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TomlSearchConfig {
    #[serde(default)]
    baseline: TomlSearchBaselineConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TomlSearchBaselineConfig {
    #[serde(default)]
    backend: SearchBaselineBackend,
    #[serde(default)]
    postgres: TomlSearchPostgresConfig,
    #[serde(default)]
    embedding: SearchEmbeddingConfig,
    #[serde(default)]
    workspace_code: SearchBaselineTargetConfig,
    #[serde(default)]
    reference: SearchBaselineTargetConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TomlSearchPostgresConfig {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    dbname: Option<String>,
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    vault_role_base: Option<String>,
    #[serde(default)]
    credential_helper: TomlSearchPostgresCredentialHelperConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TomlSearchPostgresCredentialHelperConfig {
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

impl From<TomlConfig> for ProjectConfig {
    fn from(toml: TomlConfig) -> Self {
        let pg = toml.search.baseline.postgres;
        Self {
            diagnostics: toml.diagnostics,
            config_file_path: None,
            code_lens: toml.code_lens,
            formatting: toml.formatting,
            configuration_root: toml.source.root,
            configuration_dependency: toml.source.configuration,
            source_exclude: toml.source.exclude,
            target_platform_version: toml.target_platform_version,
            platform_help: toml.platform_help,
            min_platform_version: toml.min_platform_version,
            compatibility_mode: toml.compatibility_mode,
            language: None,
            extensions: toml.source.extensions,
            externals: toml.source.externals,
            analysis: AnalysisConfig {
                diff_base: toml.analysis.diff_base,
                ignored_authors: toml.analysis.ignored_authors,
            },
            search: SearchConfig {
                baseline: SearchBaselineConfig {
                    backend: toml.search.baseline.backend,
                    postgres: SearchPostgresConfig {
                        host: pg.host,
                        port: pg.port,
                        dbname: pg.dbname,
                        schema: pg.schema,
                        vault_role_base: pg.vault_role_base,
                        credential_helper: SearchPostgresCredentialHelperConfig {
                            program: pg.credential_helper.program,
                            args: pg.credential_helper.args,
                        },
                    },
                    embedding: toml.search.baseline.embedding,
                    workspace_code: toml.search.baseline.workspace_code,
                    reference: toml.search.baseline.reference,
                },
            },
            features: toml.features,
            output: toml.output,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PostgresAccessMode {
    Reader,
    Writer,
    Migrator,
}

impl PostgresAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Writer => "writer",
            Self::Migrator => "migrator",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPostgresUrl {
    pub url: String,
    pub lease_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub renewable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPostgresTarget {
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub schema: String,
}

impl SearchPostgresConfig {
    pub fn resolved_target(&self) -> Result<ResolvedPostgresTarget, ResolvePostgresUrlError> {
        let host = self
            .host
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(ResolvePostgresUrlError::MissingField("search.baseline.postgres.host"))?
            .to_owned();
        let dbname = self
            .dbname
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(ResolvePostgresUrlError::MissingField("search.baseline.postgres.dbname"))?
            .to_owned();
        let schema = self
            .schema
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(ResolvePostgresUrlError::MissingField("search.baseline.postgres.schema"))?
            .to_owned();
        Ok(ResolvedPostgresTarget { host, port: self.port.unwrap_or(5432), dbname, schema })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvePostgresUrlError {
    MissingField(&'static str),
    MissingCredentialHelper,
    HelperSpawn {
        program: String,
        message: String,
    },
    HelperTimeout {
        program: String,
        timeout: std::time::Duration,
    },
    HelperProtocol {
        program: String,
        message: String,
        stderr: String,
        stdout: String,
    },
    HelperRejected {
        program: String,
        exit_code: Option<i32>,
        stderr: String,
        error: CredentialHelperErrorPayload,
    },
    UnsupportedUrlScheme(String),
    InvalidResolvedUrl(String),
    TargetMismatch {
        expected_host: String,
        expected_port: u16,
        expected_dbname: String,
        actual_host: Option<String>,
        actual_port: Option<u16>,
        actual_dbname: String,
    },
}

impl ResolvePostgresUrlError {
    pub fn retryable(&self) -> bool {
        match self {
            Self::HelperTimeout { .. } => true,
            Self::HelperRejected { error, .. } => error.retryable,
            _ => false,
        }
    }
}

impl std::fmt::Display for ResolvePostgresUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(field) => write!(f, "missing required PostgreSQL config field: {field}"),
            Self::MissingCredentialHelper => write!(
                f,
                "missing required PostgreSQL credential helper config: search.baseline.postgres.credential_helper.program"
            ),
            Self::HelperSpawn { program, message } => {
                write!(f, "failed to spawn credential helper '{program}': {message}")
            }
            Self::HelperTimeout { program, timeout } => write!(
                f,
                "credential helper '{program}' timed out after {}s",
                timeout.as_secs()
            ),
            Self::HelperProtocol { program, message, .. } => {
                write!(f, "credential helper '{program}' returned an invalid response: {message}")
            }
            Self::HelperRejected { program, exit_code, error, .. } => {
                write!(
                    f,
                    "credential helper '{program}' rejected the request{}: {}: {}",
                    exit_code
                        .map(|code| format!(" (exit {code})"))
                        .unwrap_or_default(),
                    error.code,
                    error.message
                )
            }
            Self::UnsupportedUrlScheme(scheme) => {
                write!(f, "credential helper returned unsupported URL scheme '{scheme}'")
            }
            Self::InvalidResolvedUrl(message) => write!(f, "credential helper returned invalid URL: {message}"),
            Self::TargetMismatch {
                expected_host,
                expected_port,
                expected_dbname,
                actual_host,
                actual_port,
                actual_dbname,
            } => write!(
                f,
                "credential helper returned URL for unexpected target: expected {}:{}/{} but got {}:{}/{}",
                expected_host,
                expected_port,
                expected_dbname,
                actual_host.as_deref().unwrap_or("<missing-host>"),
                actual_port.unwrap_or(5432),
                actual_dbname
            ),
        }
    }
}

impl std::error::Error for ResolvePostgresUrlError {}

#[derive(Debug, Clone, Serialize)]
struct CredentialHelperRequest {
    protocol: &'static str,
    action: &'static str,
    mode: PostgresAccessMode,
    vault_role_base: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CredentialHelperResponse {
    protocol: String,
    ok: bool,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    lease_id: Option<String>,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    renewable: bool,
    #[serde(default)]
    error: Option<CredentialHelperErrorPayload>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CredentialHelperErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

const POSTGRES_HELPER_PROTOCOL: &str = "bsl-analyzer.postgres-helper.v1";
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub fn resolve_postgres_url(
    postgres: &SearchPostgresConfig,
    mode: PostgresAccessMode,
) -> Result<ResolvedPostgresUrl, ResolvePostgresUrlError> {
    let target = postgres.resolved_target()?;
    let vault_role_base = postgres
        .vault_role_base
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(ResolvePostgresUrlError::MissingField("search.baseline.postgres.vault_role_base"))?
        .to_owned();
    let program = postgres
        .credential_helper
        .program
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(ResolvePostgresUrlError::MissingCredentialHelper)?
        .to_owned();
    let request = CredentialHelperRequest {
        protocol: POSTGRES_HELPER_PROTOCOL,
        action: "resolve-url",
        mode,
        vault_role_base,
    };
    let response = run_credential_helper(&program, &postgres.credential_helper.args, &request)?;
    let url = response.url.ok_or_else(|| ResolvePostgresUrlError::HelperProtocol {
        program: program.clone(),
        message: "missing url in successful response".to_owned(),
        stderr: String::new(),
        stdout: String::new(),
    })?;
    validate_resolved_postgres_url(&url, &target)?;
    Ok(ResolvedPostgresUrl {
        url,
        lease_id: response.lease_id,
        expires_at: response.expires_at,
        renewable: response.renewable,
    })
}

fn run_credential_helper(
    program: &str,
    args: &[String],
    request: &CredentialHelperRequest,
) -> Result<CredentialHelperResponse, ResolvePostgresUrlError> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ResolvePostgresUrlError::HelperSpawn {
            program: program.to_owned(),
            message: error.to_string(),
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_vec(request).map_err(|error| {
            ResolvePostgresUrlError::HelperProtocol {
                program: program.to_owned(),
                message: format!("failed to serialize helper request: {error}"),
                stderr: String::new(),
                stdout: String::new(),
            }
        })?;
        stdin.write_all(&payload).and_then(|_| stdin.write_all(b"\n")).map_err(|error| {
            ResolvePostgresUrlError::HelperProtocol {
                program: program.to_owned(),
                message: format!("failed to write helper request: {error}"),
                stderr: String::new(),
                stdout: String::new(),
            }
        })?;
    }

    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().map_err(|error| {
                    ResolvePostgresUrlError::HelperProtocol {
                        program: program.to_owned(),
                        message: format!("failed to read helper output: {error}"),
                        stderr: String::new(),
                        stdout: String::new(),
                    }
                })?;
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                return parse_credential_helper_response(program, status.code(), &stdout, &stderr);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ResolvePostgresUrlError::HelperTimeout {
                        program: program.to_owned(),
                        timeout: COMMAND_TIMEOUT,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                return Err(ResolvePostgresUrlError::HelperProtocol {
                    program: program.to_owned(),
                    message: format!("failed to wait for helper: {error}"),
                    stderr: String::new(),
                    stdout: String::new(),
                });
            }
        }
    }
}

fn parse_credential_helper_response(
    program: &str,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<CredentialHelperResponse, ResolvePostgresUrlError> {
    let response = serde_json::from_str::<CredentialHelperResponse>(stdout).map_err(|error| {
        ResolvePostgresUrlError::HelperProtocol {
            program: program.to_owned(),
            message: format!("failed to parse helper JSON: {error}"),
            stderr: stderr.to_owned(),
            stdout: stdout.to_owned(),
        }
    })?;
    if response.protocol != POSTGRES_HELPER_PROTOCOL {
        return Err(ResolvePostgresUrlError::HelperProtocol {
            program: program.to_owned(),
            message: format!("unexpected helper protocol '{}'", response.protocol),
            stderr: stderr.to_owned(),
            stdout: stdout.to_owned(),
        });
    }
    if response.ok {
        if exit_code.unwrap_or(0) != 0 {
            return Err(ResolvePostgresUrlError::HelperProtocol {
                program: program.to_owned(),
                message: format!(
                    "successful helper response used non-zero exit code {:?}",
                    exit_code
                ),
                stderr: stderr.to_owned(),
                stdout: stdout.to_owned(),
            });
        }
        return Ok(response);
    }
    let error = response.error.ok_or_else(|| ResolvePostgresUrlError::HelperProtocol {
        program: program.to_owned(),
        message: "missing error payload in failed helper response".to_owned(),
        stderr: stderr.to_owned(),
        stdout: stdout.to_owned(),
    })?;
    Err(ResolvePostgresUrlError::HelperRejected {
        program: program.to_owned(),
        exit_code,
        stderr: stderr.to_owned(),
        error,
    })
}

fn validate_resolved_postgres_url(
    url: &str,
    target: &ResolvedPostgresTarget,
) -> Result<(), ResolvePostgresUrlError> {
    let parsed = url::Url::parse(url)
        .map_err(|error| ResolvePostgresUrlError::InvalidResolvedUrl(error.to_string()))?;
    if parsed.scheme() != "postgres" {
        return Err(ResolvePostgresUrlError::UnsupportedUrlScheme(parsed.scheme().to_owned()));
    }
    let actual_host = parsed.host_str().map(ToOwned::to_owned);
    let actual_port = parsed.port_or_known_default();
    let actual_dbname = parsed.path().trim_start_matches('/').to_owned();
    if actual_host.as_deref() != Some(target.host.as_str())
        || actual_port != Some(target.port)
        || actual_dbname != target.dbname
    {
        return Err(ResolvePostgresUrlError::TargetMismatch {
            expected_host: target.host.clone(),
            expected_port: target.port,
            expected_dbname: target.dbname.clone(),
            actual_host,
            actual_port,
            actual_dbname,
        });
    }
    Ok(())
}

#[cfg(test)]
mod root_probe_case_tests {
    use super::*;

    #[test]
    fn a_case_variant_configuration_xml_names_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let cf = dir.path().join("cf");
        std::fs::create_dir_all(&cf).unwrap();
        std::fs::write(
            cf.join("CONFIGURATION.XML"),
            "<Configuration><ConfigurationExtensionPurpose>Customization             </ConfigurationExtensionPurpose></Configuration>",
        )
        .unwrap();
        assert_eq!(
            configuration_kind(&cf),
            ConfigurationKind::Extension,
            "вид конфигурации читается из CONFIGURATION.XML"
        );
        assert!(
            configuration_xml_in(&cf).is_some(),
            "каталог с CONFIGURATION.XML опознаётся корнем"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        branch_pattern_matches, current_git_branch, current_git_commit,
        evaluate_workspace_baseline_support, is_publish_branch_allowed, parse_timestamp_utc,
        resolve_postgres_url, resolve_workspace_branch_policy, wildcard_matches, ConfigLoadError,
        DiagnosticsBaselineConfig, DiagnosticsBaselineGroupConfig,
        DiagnosticsBaselinePartitionIdentity, DiagnosticsBaselinePartitionPolicy,
        DiagnosticsBaselineProjectError, DiagnosticsBaselineProjectMode,
        DiagnosticsBaselineProjectScope, DiagnosticsBaselineSelection, ExtensionDecl,
        FeaturesConfig, PlatformHelpSelection, PostgresAccessMode, Project, ProjectConfig,
        ProjectDiagnosticsConfig, ProjectError, ResolvePostgresUrlError, SearchBaselineBackend,
        SearchBaselinePolicyConfig, SearchBaselineSupportState, SearchPostgresConfig,
        SearchPostgresCredentialHelperConfig, SourceSet, SourceSetOverride,
        StructuredExtensionDecl, TopologyError, WorkspaceDiagnosticsScope,
    };
    use super::{
        compatibility_mode_in, configuration_kind, CompatibilityModeSource, ConfigurationKind,
    };
    use super::{dotenv_value, parse_dotenv_value};
    use super::{PROJECT_ENV_FILE_NAME, SHARED_CONFIGURATIONS_ROOT_ENV};
    use chrono::{Duration, TimeZone, Utc};
    use std::env;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn helper_program(response: &str, exit_code: i32) -> SearchPostgresCredentialHelperConfig {
        SearchPostgresCredentialHelperConfig {
            program: Some("python3".to_owned()),
            args: vec![
                "-c".to_owned(),
                "import sys; sys.stdin.readline(); sys.stdout.write(sys.argv[1]); sys.exit(int(sys.argv[2]))"
                    .to_owned(),
                response.to_owned(),
                exit_code.to_string(),
            ],
        }
    }

    #[test]
    fn project_config_defaults_search_baseline_to_sqlite() {
        let config: ProjectConfig = serde_json::from_str("{}").unwrap();

        assert_eq!(config.search.baseline.backend, SearchBaselineBackend::Sqlite);
        assert!(!config.search.baseline.postgres.is_configured());
        assert!(config.search.baseline.workspace_code.branch.is_none());
        assert!(config.search.baseline.reference.snapshot_id.is_none());
    }

    #[test]
    fn project_config_reads_target_platform_version_json() {
        let config: ProjectConfig =
            serde_json::from_str(r#"{"targetPlatformVersion":"8.3.27.1644"}"#).unwrap();
        assert_eq!(config.target_platform_version.as_deref(), Some("8.3.27.1644"));

        let snake: ProjectConfig =
            serde_json::from_str(r#"{"target_platform_version":"8.3.27"}"#).unwrap();
        assert_eq!(snake.target_platform_version.as_deref(), Some("8.3.27"));
    }

    #[test]
    fn project_config_reads_target_platform_version_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bsl-analyzer.toml");
        fs::write(&path, "target_platform_version = \"8.3.27.1644\"\n").unwrap();
        let config = ProjectConfig::load_from_file(&path).unwrap();
        assert_eq!(config.target_platform_version.as_deref(), Some("8.3.27.1644"));
    }

    fn load_toml(dir: &Path, body: &str) -> Result<ProjectConfig, ConfigLoadError> {
        let path = dir.join("bsl-analyzer.toml");
        fs::write(&path, body).unwrap();
        ProjectConfig::load_from_file(&path)
    }

    #[test]
    fn platform_help_reads_toml_and_resolves_paths_from_config_dir() {
        let dir = tempdir().unwrap();
        let config =
            load_toml(dir.path(), "[platform_help]\nsource = \"installed\"\npath = \"hbk\"\n")
                .unwrap();
        let elsewhere = Path::new("/somewhere/else");
        let config_dir = std::path::absolute(dir.path()).unwrap();
        assert_eq!(
            config.platform_help_selection(elsewhere).unwrap(),
            Some(PlatformHelpSelection::Installed { path: Some(config_dir.join("hbk")) })
        );

        let config =
            load_toml(dir.path(), "[platform_help]\nsource = \"external\"\npath = \"c.json\"\n")
                .unwrap();
        assert_eq!(
            config.platform_help_selection(elsewhere).unwrap(),
            Some(PlatformHelpSelection::ExternalPath(config_dir.join("c.json")))
        );
        for (source, expected) in [
            ("none", PlatformHelpSelection::None),
            ("auto", PlatformHelpSelection::Auto),
            ("bundled", PlatformHelpSelection::Bundled),
        ] {
            let config =
                load_toml(dir.path(), &format!("[platform_help]\nsource = \"{source}\"\n"))
                    .unwrap();
            assert_eq!(config.platform_help_selection(elsewhere).unwrap(), Some(expected));
        }
        let unset = load_toml(dir.path(), "").unwrap();
        assert_eq!(unset.platform_help_selection(elsewhere).unwrap(), None);
    }

    #[test]
    fn platform_help_and_compatibility_settings_coexist() {
        let dir = tempdir().unwrap();
        let toml = load_toml(
            dir.path(),
            "target_platform_version = \"8.3.27\"\nmin_platform_version = \"8.3.17\"\ncompatibility_mode = \"8.3.15\"\n[platform_help]\nsource = \"auto\"\n",
        )
        .unwrap();
        let json: ProjectConfig = serde_json::from_str(
            r#"{"targetPlatformVersion":"8.3.27","minPlatformVersion":"8.3.17","compatibilityMode":"8.3.15","platformHelp":{"source":"auto"}}"#,
        )
        .unwrap();
        for config in [toml, json] {
            assert_eq!(config.target_platform_version.as_deref(), Some("8.3.27"));
            assert_eq!(config.min_platform_version.as_deref(), Some("8.3.17"));
            assert_eq!(config.compatibility_mode.as_deref(), Some("8.3.15"));
            assert_eq!(
                config.platform_help_selection(dir.path()).unwrap(),
                Some(PlatformHelpSelection::Auto)
            );
        }
    }

    #[test]
    fn platform_help_reads_json_camel_and_snake_case() {
        let camel: ProjectConfig = serde_json::from_str(
            r#"{"platformHelp":{"source":"external","url":"https://example.invalid/m.json"}}"#,
        )
        .unwrap();
        assert_eq!(
            camel.platform_help_selection(Path::new("/root")).unwrap(),
            Some(PlatformHelpSelection::ExternalUrl("https://example.invalid/m.json".to_owned()))
        );
        let snake: ProjectConfig = serde_json::from_str(
            r#"{"platform_help":{"source":"installed","path":"/opt/1cv8/x"}}"#,
        )
        .unwrap();
        assert_eq!(
            snake.platform_help_selection(Path::new("/root")).unwrap(),
            Some(PlatformHelpSelection::Installed { path: Some(PathBuf::from("/opt/1cv8/x")) })
        );
        // No config file: relative paths resolve against the project root.
        let relative: ProjectConfig =
            serde_json::from_str(r#"{"platformHelp":{"source":"external","path":"c.json"}}"#)
                .unwrap();
        assert_eq!(
            relative.platform_help_selection(Path::new("/root")).unwrap(),
            Some(PlatformHelpSelection::ExternalPath(PathBuf::from("/root/c.json")))
        );
    }

    #[test]
    fn platform_help_rejects_malformed_values_as_config_errors() {
        let dir = tempdir().unwrap();
        for body in [
            "[platform_help]\nsource = \"process\"\n",
            "[platform_help]\nsource = \"Installed\"\n",
            "[platform_help]\npath = \"x\"\n",
            "[platform_help]\nsource = \"none\"\npath = \"x\"\n",
            "[platform_help]\nsource = \"installed\"\nurl = \"http://x\"\n",
            "[platform_help]\nsource = \"external\"\n",
            "[platform_help]\nsource = \"external\"\npath = \"a\"\nurl = \"http://x\"\n",
            "[platform_help]\nsource = \"installed\"\npath = \"\"\n",
        ] {
            assert!(load_toml(dir.path(), body).is_err(), "must be refused: {body}");
        }
    }

    #[test]
    fn project_config_reads_min_platform_version_json() {
        let config: ProjectConfig =
            serde_json::from_str(r#"{"minPlatformVersion":"8.3.17"}"#).unwrap();
        assert_eq!(config.min_platform_version.as_deref(), Some("8.3.17"));
        assert!(config.target_platform_version.is_none());

        let snake: ProjectConfig =
            serde_json::from_str(r#"{"min_platform_version":"8.3.17.1549"}"#).unwrap();
        assert_eq!(snake.min_platform_version.as_deref(), Some("8.3.17.1549"));
    }

    #[test]
    fn project_config_reads_min_platform_version_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bsl-analyzer.toml");
        fs::write(&path, "min_platform_version = \"8.3.17\"\n").unwrap();
        let config = ProjectConfig::load_from_file(&path).unwrap();
        assert_eq!(config.min_platform_version.as_deref(), Some("8.3.17"));
        assert!(config.target_platform_version.is_none());
    }

    #[test]
    fn project_config_reads_compatibility_mode_json_and_toml() {
        let config: ProjectConfig =
            serde_json::from_str(r#"{"compatibilityMode":"8.2.13"}"#).unwrap();
        assert_eq!(config.compatibility_mode.as_deref(), Some("8.2.13"));
        let snake: ProjectConfig =
            serde_json::from_str(r#"{"compatibility_mode":"Version8_3_17"}"#).unwrap();
        assert_eq!(snake.compatibility_mode.as_deref(), Some("Version8_3_17"));

        let dir = tempdir().unwrap();
        let path = dir.path().join("bsl-analyzer.toml");
        fs::write(&path, "compatibility_mode = \"8.3.15\"\n").unwrap();
        let config = ProjectConfig::load_from_file(&path).unwrap();
        assert_eq!(config.compatibility_mode.as_deref(), Some("8.3.15"));
    }

    #[test]
    fn compatibility_mode_is_read_from_a_main_configuration_only() {
        let main = br#"<MetaDataObject><Configuration><Properties>
<!-- <CompatibilityMode>Version8_1</CompatibilityMode> -->
<ConfigurationExtensionCompatibilityMode>Version8_3_17</ConfigurationExtensionCompatibilityMode>
<CompatibilityMode>Version8_2_13</CompatibilityMode>
</Properties></Configuration></MetaDataObject>"#;
        assert_eq!(compatibility_mode_in(main).as_deref(), Some("Version8_2_13"));

        let extension = br#"<MetaDataObject><Configuration><Properties>
<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>
<CompatibilityMode>Version8_3_17</CompatibilityMode>
</Properties></Configuration></MetaDataObject>"#;
        assert_eq!(compatibility_mode_in(extension), None, "an extension runs under its base");

        let after_properties =
            br#"<Properties></Properties><CompatibilityMode>Version8_2_13</CompatibilityMode>"#;
        assert_eq!(compatibility_mode_in(after_properties), None);
    }

    #[test]
    fn project_compatibility_mode_prefers_the_setting() {
        let dir = tempdir().unwrap();
        let cf = dir.path().join("src").join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(
            cf.join("Configuration.xml"),
            "<MetaDataObject><Configuration><Properties><CompatibilityMode>Version8_2_13</CompatibilityMode></Properties></Configuration></MetaDataObject>",
        )
        .unwrap();
        let project = Project::with_config(dir.path(), ProjectConfig::default()).unwrap();
        assert_eq!(
            project.compatibility_mode(),
            Some(("Version8_2_13".to_string(), CompatibilityModeSource::ConfigurationXml))
        );

        let overridden =
            ProjectConfig { compatibility_mode: Some("8.3.15".into()), ..ProjectConfig::default() };
        let project = Project::with_config(dir.path(), overridden).unwrap();
        assert_eq!(
            project.compatibility_mode(),
            Some(("8.3.15".to_string(), CompatibilityModeSource::Setting))
        );
    }

    #[test]
    fn project_config_deserializes_search_baseline_settings() {
        let config: ProjectConfig = serde_json::from_str(
            r#"{
                "search": {
                    "baseline": {
                        "backend": "postgres",
                        "postgres": {
                            "host": "pg-central",
                            "port": 5432,
                            "dbname": "bsl_search",
                            "schema": "corp_search",
                            "vaultRoleBase": "search/bsl",
                            "credentialHelper": {
                                "program": "vault-helper",
                                "args": ["--mode"]
                            }
                        },
                        "workspaceCode": {
                            "branch": "main",
                            "commit": "abc123"
                        },
                        "reference": {
                            "snapshotId": "reference:0.1.104"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(config.search.baseline.backend, SearchBaselineBackend::Postgres);
        assert_eq!(config.search.baseline.postgres.host.as_deref(), Some("pg-central"));
        assert_eq!(config.search.baseline.postgres.port, Some(5432));
        assert_eq!(config.search.baseline.postgres.dbname.as_deref(), Some("bsl_search"));
        assert_eq!(config.search.baseline.postgres.schema.as_deref(), Some("corp_search"));
        assert_eq!(config.search.baseline.postgres.vault_role_base.as_deref(), Some("search/bsl"));
        assert_eq!(
            config.search.baseline.postgres.credential_helper.program.as_deref(),
            Some("vault-helper")
        );
        assert_eq!(config.search.baseline.postgres.credential_helper.args, vec!["--mode"]);
        assert_eq!(config.search.baseline.workspace_code.branch.as_deref(), Some("main"));
        assert_eq!(config.search.baseline.workspace_code.commit.as_deref(), Some("abc123"));
        assert_eq!(
            config.search.baseline.reference.snapshot_id.as_deref(),
            Some("reference:0.1.104")
        );
    }

    #[test]
    fn embedding_prefix_fields_preserve_explicit_empty_and_whitespace() {
        let config: super::TomlConfig = toml::from_str(
            "[search.baseline.embedding]\nqueryPrefix = \"\"\ndocumentPrefix = \"  \"\n",
        )
        .unwrap();
        let embedding = &config.search.baseline.embedding;
        assert_eq!(embedding.query_prefix.as_deref(), Some(""));
        assert_eq!(embedding.resolve_query_prefix(Some("env query")), "");
        assert_eq!(embedding.document_prefix.as_deref(), Some("  "));
        assert_eq!(embedding.resolve_document_prefix(Some("env document")), "  ");

        let defaults = super::SearchEmbeddingConfig::default();
        assert_eq!(defaults.resolve_query_prefix(Some("env query")), "env query");
        assert_eq!(defaults.resolve_document_prefix(None), "");
    }

    #[test]
    fn token_profile_fields_deserialize_and_project_tokenizer_paths_use_config_origin() {
        let dir = tempdir().unwrap();
        let config_dir = dir.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("bsl-analyzer.toml");
        fs::write(
            &config_path,
            "[search.baseline.embedding]\nmaxInputTokens = 8192\ntokenizerFile = \"models/tokenizer.json\"\ntokenizerSha256 = \"abc\"\n",
        )
        .unwrap();
        let config = ProjectConfig::load_from_file(&config_path).unwrap();

        assert_eq!(
            config
                .search
                .baseline
                .embedding
                .max_input_tokens
                .as_ref()
                .and_then(toml::Value::as_integer),
            Some(8192)
        );
        assert_eq!(
            config.search_tokenizer_file(dir.path()),
            Some(config_dir.join("models/tokenizer.json"))
        );
    }

    #[test]
    fn project_config_deserializes_workspace_branch_policy() {
        let config: ProjectConfig = serde_json::from_str(
            r#"{
                "search": {
                    "baseline": {
                        "workspaceCode": {
                            "policy": {
                                "publishBranches": ["vendor", "develop"],
                                "branches": [
                                    { "match": "vendor", "selectBranch": "vendor" },
                                    {
                                        "match": "feature/*",
                                        "selectBranch": "develop",
                                        "fallbackBranch": "vendor"
                                    },
                                    {
                                        "match": "*",
                                        "selectBranch": "develop",
                                        "fallbackBranch": "vendor"
                                    }
                                ]
                            }
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let policy = &config.search.baseline.workspace_code.policy;
        assert!(policy.is_configured());
        assert_eq!(policy.publish_branches, vec!["vendor", "develop"]);
        assert_eq!(policy.branches.len(), 3);
        assert_eq!(policy.branches[1].pattern, "feature/*");
        assert_eq!(policy.branches[1].select_branch, "develop");
        assert_eq!(policy.branches[1].fallback_branch.as_deref(), Some("vendor"));
        assert_eq!(policy.support.stale_after_days, 21);
        assert_eq!(policy.support.expire_after_days, 30);
        assert_eq!(policy.retention.develop_retention_days, 30);
        assert_eq!(policy.retention.vendor_keep_heads, 2);
    }

    #[test]
    fn parse_timestamp_supports_postgres_text_format() {
        let parsed = parse_timestamp_utc("2026-04-02 09:01:53.271613+00:00").unwrap();

        assert_eq!(
            parsed,
            Utc.with_ymd_and_hms(2026, 4, 2, 9, 1, 53).unwrap() + Duration::microseconds(271613)
        );
    }

    #[test]
    fn workspace_baseline_support_becomes_stale_and_expired_by_age() {
        let policy: SearchBaselinePolicyConfig = serde_json::from_value(serde_json::json!({
            "publishBranches": ["vendor", "develop"],
            "branches": [{ "match": "*", "selectBranch": "develop", "fallbackBranch": "vendor" }],
            "support": { "staleAfterDays": 10, "expireAfterDays": 20 }
        }))
        .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 2, 12, 0, 0).unwrap();

        let stale = evaluate_workspace_baseline_support(
            &policy,
            Some("feature/demo"),
            Some("develop"),
            Some(now - Duration::days(12)),
            now,
        )
        .unwrap();
        assert_eq!(stale.state, SearchBaselineSupportState::Stale);
        assert!(stale.reason.contains("feature/demo"));
        assert!(stale.reason.contains("develop"));

        let expired = evaluate_workspace_baseline_support(
            &policy,
            Some("feature/demo"),
            Some("develop"),
            Some(now - Duration::days(25)),
            now,
        )
        .unwrap();
        assert_eq!(expired.state, SearchBaselineSupportState::Expired);
        assert_eq!(expired.snapshot_age_days, 25);
    }

    #[test]
    fn workspace_baseline_support_is_none_when_policy_is_not_configured() {
        let now = Utc.with_ymd_and_hms(2026, 4, 2, 12, 0, 0).unwrap();

        assert!(evaluate_workspace_baseline_support(
            &SearchBaselinePolicyConfig::default(),
            Some("feature/demo"),
            Some("develop"),
            Some(now - Duration::days(5)),
            now,
        )
        .is_none());
    }

    #[test]
    fn branch_pattern_matches_exact_prefix_and_wildcard() {
        assert!(branch_pattern_matches("develop", Some("develop")));
        assert!(branch_pattern_matches("feature/*", Some("feature/test")));
        assert!(branch_pattern_matches("*", Some("custom/branch")));
        assert!(!branch_pattern_matches("feature/*", Some("feature")));
        assert!(!branch_pattern_matches("feature/*", Some("fix/test")));
    }

    #[test]
    fn workspace_branch_policy_resolves_branch_chain() {
        let policy = SearchBaselinePolicyConfig {
            publish_branches: vec!["vendor".to_owned(), "develop".to_owned()],
            branches: serde_json::from_value(serde_json::json!([
                { "match": "vendor", "selectBranch": "vendor" },
                { "match": "develop", "selectBranch": "develop", "fallbackBranch": "vendor" },
                { "match": "feature/*", "selectBranch": "develop", "fallbackBranch": "vendor" },
                { "match": "*", "selectBranch": "develop", "fallbackBranch": "vendor" }
            ]))
            .unwrap(),
            ..SearchBaselinePolicyConfig::default()
        };

        let resolved = resolve_workspace_branch_policy(&policy, Some("feature/test")).unwrap();
        assert_eq!(resolved.workspace_branch.as_deref(), Some("feature/test"));
        assert_eq!(resolved.matched_pattern, "feature/*");
        assert_eq!(resolved.candidate_branches(), vec!["develop", "vendor"]);
        assert_eq!(
            resolved.selection_description(),
            "workspace branch feature/test -> branch develop -> branch vendor"
        );
    }

    #[test]
    fn workspace_branch_policy_uses_wildcard_for_unknown_branch() {
        let policy = SearchBaselinePolicyConfig {
            publish_branches: vec![],
            branches: serde_json::from_value(serde_json::json!([
                { "match": "*", "selectBranch": "develop", "fallbackBranch": "vendor" }
            ]))
            .unwrap(),
            ..SearchBaselinePolicyConfig::default()
        };

        let resolved = resolve_workspace_branch_policy(&policy, Some("release/1.0")).unwrap();
        assert_eq!(resolved.matched_pattern, "*");
        assert_eq!(resolved.candidate_branches(), vec!["develop", "vendor"]);
    }

    #[test]
    fn publish_branch_policy_uses_pattern_matching() {
        let policy = SearchBaselinePolicyConfig {
            publish_branches: vec!["vendor".to_owned(), "develop".to_owned()],
            branches: vec![],
            ..SearchBaselinePolicyConfig::default()
        };

        assert!(is_publish_branch_allowed(&policy, "vendor"));
        assert!(is_publish_branch_allowed(&policy, "develop"));
        assert!(!is_publish_branch_allowed(&policy, "feature/test"));
    }

    #[test]
    fn current_git_branch_reads_direct_git_dir() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/test\n").unwrap();

        assert_eq!(current_git_branch(dir.path()).as_deref(), Some("feature/test"));
    }

    #[test]
    fn current_git_branch_reads_gitdir_file() {
        let dir = tempdir().unwrap();
        let actual_git_dir = dir.path().join(".git-data");
        fs::create_dir_all(&actual_git_dir).unwrap();
        fs::write(actual_git_dir.join("HEAD"), "ref: refs/heads/develop\n").unwrap();
        fs::write(dir.path().join(".git"), "gitdir: .git-data\n").unwrap();

        assert_eq!(current_git_branch(dir.path()).as_deref(), Some("develop"));
    }

    #[test]
    fn current_git_branch_returns_none_for_detached_head() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "0123456789abcdef\n").unwrap();

        assert_eq!(current_git_branch(dir.path()), None);
    }

    #[test]
    fn current_git_commit_reads_direct_git_dir_ref() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        let ref_file = git_dir.join("refs/heads/feature/test");
        fs::create_dir_all(ref_file.parent().unwrap()).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/test\n").unwrap();
        fs::write(&ref_file, "0123456789abcdef\n").unwrap();

        assert_eq!(current_git_commit(dir.path()).as_deref(), Some("0123456789abcdef"));
    }

    #[test]
    fn current_git_commit_reads_detached_head() {
        let dir = tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "0123456789abcdef\n").unwrap();

        assert_eq!(current_git_commit(dir.path()).as_deref(), Some("0123456789abcdef"));
    }

    #[test]
    fn project_config_defaults_workspace_policy_to_empty() {
        let config: ProjectConfig = serde_json::from_str("{}").unwrap();

        assert!(!config.search.baseline.workspace_code.policy.is_configured());
        assert!(config.search.baseline.reference.policy.branches.is_empty());
    }

    #[test]
    fn toml_config_deserializes_minimal() {
        let config: super::TomlConfig = toml::from_str("").unwrap();
        let project = ProjectConfig::from(config);
        assert_eq!(project.search.baseline.backend, SearchBaselineBackend::Sqlite);
        assert!(project.configuration_root.is_none());
        assert!(!project.search.baseline.postgres.is_configured());
    }

    #[test]
    fn toml_config_reads_extensions_from_source_section() {
        let toml_str = r#"
[source]
root = "src/cf"
extensions = ["src/cfe/BMS_RU_UT", "src/cfe/YAxUnit"]
"#;
        let config: super::TomlConfig = toml::from_str(toml_str).unwrap();
        let project = ProjectConfig::from(config);
        assert_eq!(project.configuration_root.as_deref(), Some("src/cf"));
        assert_eq!(
            project.extensions,
            Some(vec!["src/cfe/BMS_RU_UT".into(), "src/cfe/YAxUnit".into()])
        );
    }

    #[test]
    fn toml_config_reads_analysis_diff_base() {
        let toml_str = "[analysis]\ndiff_base = \"vendor\"\n";
        let config: super::TomlConfig = toml::from_str(toml_str).unwrap();
        let project = ProjectConfig::from(config);
        assert_eq!(project.analysis.diff_base.as_deref(), Some("vendor"));

        let unset: super::TomlConfig = toml::from_str("").unwrap();
        assert!(ProjectConfig::from(unset).analysis.diff_base.is_none());

        let unknown: Result<super::TomlConfig, _> =
            toml::from_str("[analysis]\ndiffBase = \"vendor\"\n");
        assert!(unknown.is_err(), "unknown [analysis] keys must be rejected");
    }

    #[test]
    fn json_config_reads_analysis_diff_base_camel_case() {
        let json = r#"{ "analysis": { "diffBase": "vendor" } }"#;
        let project: ProjectConfig = serde_json::from_str(json).unwrap();
        assert_eq!(project.analysis.diff_base.as_deref(), Some("vendor"));
    }

    #[test]
    fn toml_config_reads_analysis_ignored_authors() {
        let toml_str = "[analysis]\nignored_authors = [\"Фирма 1С\", \"vendor@example.com\"]\n";
        let config: super::TomlConfig = toml::from_str(toml_str).unwrap();
        let project = ProjectConfig::from(config);
        assert_eq!(project.analysis.ignored_authors, ["Фирма 1С", "vendor@example.com"]);

        let unset: super::TomlConfig = toml::from_str("").unwrap();
        assert!(ProjectConfig::from(unset).analysis.ignored_authors.is_empty());
    }

    #[test]
    fn json_config_reads_analysis_ignored_authors_camel_case() {
        let json = r#"{ "analysis": { "ignoredAuthors": ["Фирма 1С"] } }"#;
        let project: ProjectConfig = serde_json::from_str(json).unwrap();
        assert_eq!(project.analysis.ignored_authors, ["Фирма 1С"]);
    }

    #[test]
    fn toml_config_rejects_top_level_extensions() {
        let toml_str = r#"
extensions = ["src/cfe/BMS_RU_UT"]

[source]
root = "src/cf"
"#;
        let result: Result<super::TomlConfig, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "top-level extensions must be rejected; use [source].extensions instead"
        );
    }

    fn touch_extension(root: &std::path::Path, rel: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Configuration.xml"), "<xml/>").unwrap();
    }

    fn structured(name: &str, path: &str, deps: &[&str]) -> ExtensionDecl {
        ExtensionDecl::Structured(StructuredExtensionDecl {
            name: name.to_string(),
            path: path.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn toml_mixed_extension_entries_parse() {
        let toml_str = r#"
[source]
root = "src/cf"
extensions = [
  "vendor/legacy",
  { name = "yaxunit", path = "vendor/yaxunit" },
  { name = "TESTS", path = "src/cfe/TESTS", dependsOn = ["yaxunit"] },
]
"#;
        let config: super::TomlConfig = toml::from_str(toml_str).unwrap();
        let project = ProjectConfig::from(config);
        assert_eq!(
            project.extensions,
            Some(vec![
                "vendor/legacy".into(),
                structured("yaxunit", "vendor/yaxunit", &[]),
                structured("TESTS", "src/cfe/TESTS", &["yaxunit"]),
            ])
        );
    }

    #[test]
    fn toml_depends_on_accepts_snake_case_alias() {
        let toml_str = r#"
[source]
extensions = [{ name = "T", path = "src/cfe/T", depends_on = ["Y"] }]
"#;
        let config: super::TomlConfig = toml::from_str(toml_str).unwrap();
        let project = ProjectConfig::from(config);
        assert_eq!(project.extensions, Some(vec![structured("T", "src/cfe/T", &["Y"])]));
    }

    #[test]
    fn toml_structured_entry_rejects_unknown_field_with_its_name() {
        let toml_str = r#"
[source]
extensions = [{ name = "T", path = "src/cfe/T", dependson = ["Y"] }]
"#;
        let err = toml::from_str::<super::TomlConfig>(toml_str).unwrap_err().to_string();
        assert!(err.contains("dependson"), "the offending key must be named: {err}");
    }

    #[test]
    fn toml_structured_entry_rejects_missing_path() {
        let toml_str = r#"
[source]
extensions = [{ name = "T" }]
"#;
        let err = toml::from_str::<super::TomlConfig>(toml_str).unwrap_err().to_string();
        assert!(err.contains("path"), "the missing field must be named: {err}");
    }

    #[test]
    fn json_structured_extension_entries_parse() {
        let json = r#"{
            "extensions": [
                "vendor/legacy",
                { "name": "TESTS", "path": "src/cfe/TESTS", "dependsOn": ["yaxunit"] }
            ]
        }"#;
        let config: ProjectConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.extensions,
            Some(vec!["vendor/legacy".into(), structured("TESTS", "src/cfe/TESTS", &["yaxunit"]),])
        );
    }

    #[test]
    fn structured_entry_with_glob_path_is_rejected() {
        let dir = tempdir().unwrap();
        let config = ProjectConfig {
            extensions: Some(vec![structured("T", "src/cfe/*", &[])]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::GlobInStructuredEntry { .. })),
            "got: {err}"
        );
    }

    #[test]
    fn structured_entry_with_missing_path_is_rejected() {
        let dir = tempdir().unwrap();
        let config = ProjectConfig {
            extensions: Some(vec![structured("T", "src/cfe/T", &[])]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::StructuredPathMissing { .. })),
            "got: {err}"
        );
    }

    #[test]
    fn structured_entry_without_configuration_xml_is_rejected() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/cfe/T")).unwrap();
        let config = ProjectConfig {
            extensions: Some(vec![structured("T", "src/cfe/T", &[])]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::StructuredNotAnExtension { .. })),
            "got: {err}"
        );
    }

    #[test]
    fn structured_entry_duplicating_a_path_is_rejected() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "src/cfe/Ext");
        let config = ProjectConfig {
            extensions: Some(vec!["src/cfe/Ext".into(), structured("Named", "./src/cfe/Ext", &[])]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::DuplicatePath { .. })),
            "textual path variants must collide on the canonical path: {err}"
        );
    }

    #[test]
    fn depends_on_builds_a_project_with_the_declared_closure() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "vendor/yaxunit");
        touch_extension(dir.path(), "src/cfe/TESTS");
        let config = ProjectConfig {
            extensions: Some(vec![
                structured("yaxunit", "vendor/yaxunit", &[]),
                structured("TESTS", "src/cfe/TESTS", &["yaxunit"]),
            ]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        let topology = project.extension_topology();
        let tests =
            topology.nodes().iter().find(|n| n.name() == "TESTS").expect("TESTS node exists");
        let closure_names: Vec<&str> =
            tests.closure().iter().map(|id| topology.node(*id).name()).collect();
        assert_eq!(closure_names, ["yaxunit"]);
    }

    #[test]
    fn depends_on_unknown_name_reports_the_specific_error() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "src/cfe/TESTS");
        // The dependency target exists on disk but is skipped (no
        // Configuration.xml), so the edge must dangle into a hard error.
        std::fs::create_dir_all(dir.path().join("vendor/yaxunit")).unwrap();
        let config = ProjectConfig {
            extensions: Some(vec![
                "vendor/yaxunit".into(),
                structured("TESTS", "src/cfe/TESTS", &["yaxunit"]),
            ]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::UnknownDependency { .. })),
            "got: {err}"
        );
    }

    #[test]
    fn structured_entries_without_depends_on_resolve_like_legacy() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "vendor/yaxunit");
        touch_extension(dir.path(), "src/cfe/TESTS");
        let config = ProjectConfig {
            extensions: Some(vec![
                structured("named-yaxunit", "vendor/yaxunit", &[]),
                "src/cfe/TESTS".into(),
            ]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        let names: Vec<&str> = project.extension_paths().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["named-yaxunit", "TESTS"],
            "structured entries keep the declared name; legacy entries derive it from the dir"
        );
        let topology = project.extension_topology();
        assert_eq!(topology.nodes().len(), 2);
        assert!(topology.nodes().iter().all(|n| n.closure().is_empty()));
    }

    #[test]
    fn topology_fingerprint_changes_when_an_extension_is_added() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "src/cfe/A");
        touch_extension(dir.path(), "src/cfe/B");
        let fingerprint = |exts: &[&str]| {
            let config = ProjectConfig {
                extensions: Some(exts.iter().map(|s| ExtensionDecl::from(*s)).collect()),
                ..Default::default()
            };
            Project::with_config(dir.path(), config).unwrap().extension_topology().fingerprint()
        };
        assert_eq!(fingerprint(&["src/cfe/A"]), fingerprint(&["src/cfe/A"]));
        assert_ne!(fingerprint(&["src/cfe/A"]), fingerprint(&["src/cfe/A", "src/cfe/B"]));
    }

    #[test]
    fn extension_glob_expands_to_all_subdirs_with_configuration_xml() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/BMS_RU_UT");
        touch_extension(root, "src/cfe/YAxUnit");
        // A cfe subdir without Configuration.xml must be skipped.
        std::fs::create_dir_all(root.join("src/cfe/NotAnExtension")).unwrap();

        let config =
            ProjectConfig { extensions: Some(vec!["src/cfe/*".into()]), ..Default::default() };
        let resolved = Project::resolve_extensions(root, &config).unwrap();

        let names: Vec<&str> = resolved.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["BMS_RU_UT", "YAxUnit"]);
    }

    #[test]
    fn extension_glob_honours_prefix_wildcard() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/БУС_ОбменДанными");
        touch_extension(root, "src/cfe/YAxUnit");

        let config = ProjectConfig {
            extensions: Some(vec!["src/cfe/БУС_*".into()]),
            ..Default::default()
        };
        let resolved = Project::resolve_extensions(root, &config).unwrap();

        let names: Vec<&str> = resolved.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["БУС_ОбменДанными"]);
    }

    #[test]
    fn extension_glob_and_explicit_paths_dedup() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/BMS_RU_UT");

        let config = ProjectConfig {
            extensions: Some(vec!["src/cfe/*".into(), "src/cfe/BMS_RU_UT".into()]),
            ..Default::default()
        };
        let resolved = Project::resolve_extensions(root, &config).unwrap();
        assert_eq!(resolved.len(), 1, "the same extension must not be added twice");
    }

    #[test]
    fn extensions_auto_discovered_from_src_cfe_when_unset() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/BMS_RU_UT");
        touch_extension(root, "src/cfe/YAxUnit");
        std::fs::create_dir_all(root.join("src/cfe/NotAnExtension")).unwrap();

        // No `extensions` setting at all → discover every src/cfe child with Configuration.xml.
        let config = ProjectConfig::default();
        let resolved = Project::resolve_extensions(root, &config).unwrap();

        let names: Vec<&str> = resolved.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["BMS_RU_UT", "YAxUnit"]);
    }

    /// Writes a main configuration at `<root>/<rel>`.
    fn touch_configuration(root: &std::path::Path, rel: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Configuration.xml"), "<Configuration/>").unwrap();
    }

    /// `.env` is machine-local, and a store sitting next to the checkout is the
    /// natural thing to write there — so a relative value must be anchored to the
    /// project, exactly like `[source].root` is. Unanchored, it resolves against
    /// whatever the process cwd happens to be, and every consumer that demands an
    /// absolute root breaks on it.
    #[test]
    fn a_relative_shared_configuration_root_is_anchored_to_the_project() {
        assert!(
            env::var_os(SHARED_CONFIGURATIONS_ROOT_ENV).is_none(),
            "this test measures the .env path; the process environment wins over it, \
             so {SHARED_CONFIGURATIONS_ROOT_ENV} must be unset to run it"
        );
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_configuration(root, "configurations/UT11/1.0.0");
        let project_root = root.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(
            project_root.join("bsl-analyzer.toml"),
            "[source.configuration]\nid = \"UT11\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();

        let resolved = |value: &str| -> PathBuf {
            std::fs::write(
                project_root.join(PROJECT_ENV_FILE_NAME),
                format!("{SHARED_CONFIGURATIONS_ROOT_ENV}={value}\n"),
            )
            .unwrap();
            let project = Project::new(&project_root).expect("the shared configuration resolves");
            for source_root in project.source_roots() {
                assert!(
                    source_root.is_absolute(),
                    "every root the project hands out must be absolute, got {}",
                    source_root.display()
                );
            }
            let path = project.configuration_path().expect("a base is configured").to_path_buf();
            std::fs::canonicalize(&path).unwrap_or(path)
        };

        let absolute = resolved(&root.join("configurations").to_string_lossy());
        let relative = resolved("../configurations");
        assert_eq!(
            relative, absolute,
            "a relative store root must name the same directory as the absolute one"
        );
    }

    /// A path is not a string literal: in `ONEC_CONFIGURATIONS_ROOT="C:\1C\Store"`
    /// the backslashes are the path, not escape sequences. Dropping them turns a
    /// correctly-written Windows `.env` into an unloadable project.
    #[test]
    fn a_double_quoted_dotenv_value_keeps_unknown_escapes_intact() {
        let at = std::path::Path::new(".env");
        assert_eq!(
            parse_dotenv_value(r#""D:\onec\configurations""#, at, 1).unwrap(),
            r"D:\onec\configurations"
        );
        // The control: escapes the format does define still mean what they mean,
        // so this is not "stop interpreting escapes" applied with a hammer.
        assert_eq!(parse_dotenv_value(r#""a\nb\\c\"d""#, at, 1).unwrap(), "a\nb\\c\"d");
    }

    /// A BOM is what several Windows editors put in front of a file they save as
    /// UTF-8. It must not hide the first key — the resulting error names a missing
    /// variable that is plainly there in the file.
    #[test]
    fn a_dotenv_file_is_read_through_a_leading_bom() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(PROJECT_ENV_FILE_NAME);
        let line = format!("{SHARED_CONFIGURATIONS_ROOT_ENV}=/opt/configurations\n");

        std::fs::write(&path, &line).unwrap();
        let plain = dotenv_value(&path, SHARED_CONFIGURATIONS_ROOT_ENV).unwrap();
        assert_eq!(plain.as_deref(), Some("/opt/configurations"), "control: no BOM, key found");

        std::fs::write(&path, format!("\u{feff}{line}")).unwrap();
        assert_eq!(
            dotenv_value(&path, SHARED_CONFIGURATIONS_ROOT_ENV).unwrap(),
            plain,
            "a leading BOM must not hide the first key"
        );
    }

    /// The advisory exists because an extension analyzed without its main
    /// configuration reports valid calls as unresolved, and from the outside that
    /// reads as the analyzer being wrong. It must therefore fire for every shape a
    /// base-less extension project takes, not only the one where the extension
    /// happens to sit at the workspace root.
    #[test]
    fn a_base_less_extension_project_says_so_whichever_shape_it_takes() {
        fn extension_at(root: &std::path::Path, rel: &str) {
            let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Configuration.xml"),
                "<Properties><ConfigurationExtensionPurpose>Customization\
                 </ConfigurationExtensionPurpose></Properties>",
            )
            .unwrap();
        }
        let notice_for = |build: &dyn Fn(&std::path::Path)| -> Option<String> {
            let dir = tempdir().unwrap();
            build(dir.path());
            Project::new(dir.path()).unwrap().standalone_extension_notice()
        };

        // The extension is the workspace root itself.
        assert!(notice_for(&|root| extension_at(root, "")).is_some(), "extension at the root");
        // The extension was exported into the place a main configuration lives.
        assert!(
            notice_for(&|root| extension_at(root, "src/cf")).is_some(),
            "extension in the conventional configuration directory"
        );
        // The extension is declared, which is the shape extension-only projects take.
        assert!(
            notice_for(&|root| {
                extension_at(root, "Расширения/Feature");
                std::fs::write(
                    root.join("bsl-analyzer.toml"),
                    "[source]\nextensions = [\"Расширения/Feature\"]\n",
                )
                .unwrap();
            })
            .is_some(),
            "declared extension-only project"
        );

        // Controls, one per reason the advisory must stay silent. A configured base
        // is the whole point of the shared-configuration work: it must not warn.
        assert!(
            notice_for(&|root| {
                let cf = root.join("src/cf");
                std::fs::create_dir_all(&cf).unwrap();
                std::fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
                extension_at(root, "src/cfe/A");
            })
            .is_none(),
            "a project with a main configuration stays silent"
        );
        assert!(
            notice_for(&|root| {
                std::fs::write(root.join("bsl-analyzer.toml"), "[diagnostics]\n").unwrap();
            })
            .is_none(),
            "a project that is not an extension at all stays silent"
        );
    }

    /// "No configuration" and "no base" are different projects. A directory of
    /// sources with no `Configuration.xml` has an anonymous base at the workspace
    /// root — that is what its published baselines already record — while only a
    /// project WITH extensions and no configuration has no base at all. Collapsing
    /// the two leaves the first with an empty plan: no partition owns its files, so
    /// every published baseline is rejected and a v1 migration produces unowned
    /// diagnostics.
    #[test]
    fn a_plain_directory_of_sources_keeps_its_anonymous_base_partition() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = ProjectConfig {
            diagnostics: partitioned_config("baselines", vec![]),
            ..Default::default()
        };
        let project = Project::with_config(root, config).unwrap();
        assert!(project.configuration_path().is_none(), "there is no configuration");

        let plan = project
            .diagnostics_baseline_partition_plan()
            .expect("the plan is built")
            .expect("the baseline is partitioned");
        assert_eq!(
            plan.project_scope.source_root.as_deref(),
            Some(""),
            "the anonymous base keeps the spelling published baselines carry"
        );
        assert!(plan.partitions.iter().any(|partition| partition.id == "main"));
        assert_eq!(
            plan.owner_for_project_path("Module.bsl"),
            Some("main"),
            "and it still owns the project's files"
        );
    }

    /// A partitioned baseline over an extension-only project must produce a plan.
    /// The nesting check exists to catch two roots that contain one another; a base
    /// the project does not have is not such a root, and letting the project root
    /// stand in for it makes every extension look nested inside the base.
    #[test]
    fn a_partitioned_baseline_plans_an_extension_only_project() {
        fn extension_at(root: &std::path::Path, rel: &str) {
            let dir = root.join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Configuration.xml"),
                "<Properties><ConfigurationExtensionPurpose>Customization\
                 </ConfigurationExtensionPurpose></Properties>",
            )
            .unwrap();
        }
        let plan_for = |with_base: bool| {
            let dir = tempdir().unwrap();
            let root = dir.path();
            extension_at(root, "Расширения/A");
            extension_at(root, "Расширения/B");
            let source = if with_base {
                let cf = root.join("src/cf");
                std::fs::create_dir_all(&cf).unwrap();
                std::fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
                Some("src/cf".to_owned())
            } else {
                None
            };
            let config = ProjectConfig {
                configuration_root: source,
                extensions: Some(vec![
                    structured("A", "Расширения/A", &[]),
                    structured("B", "Расширения/B", &[]),
                ]),
                diagnostics: partitioned_config("baselines", vec![]),
                ..Default::default()
            };
            Project::with_config(root, config).unwrap().diagnostics_baseline_partition_plan()
        };

        // The control: with a base the plan is built, as it always was.
        let with_base = plan_for(true).expect("a base project plans").expect("partitioned");
        assert!(with_base.partitions.iter().any(|partition| partition.id == "main"));

        let plan = plan_for(false).expect("an extension-only project plans too");
        let plan = plan.expect("the baseline is partitioned");
        let ids: Vec<&str> =
            plan.partitions.iter().map(|partition| partition.id.as_str()).collect();
        assert!(
            ids.contains(&"extension:A") && ids.contains(&"extension:B"),
            "both extensions own a partition: {ids:?}"
        );
        // "The plan is built" alone would pass while a base the project does not
        // have still owns the workspace root and every file outside an extension.
        assert!(!ids.contains(&"main"), "no base partition without a base: {ids:?}");
        assert!(
            !plan.roots.iter().any(|owner| owner.partition_id == "main"),
            "and no root is owned by one: {:?}",
            plan.roots
        );
        assert!(plan.project_scope.source_root.is_none(), "the scope names no base either");
    }

    /// The base is optional in the scope, but a project that HAS one must serialize
    /// exactly as before: the published baselines carry a fingerprint taken over
    /// this JSON, and a shape change would invalidate every one of them.
    #[test]
    fn an_optional_base_does_not_change_how_a_present_one_serializes() {
        let scope = DiagnosticsBaselineProjectScope {
            source_root: Some("src/cf".to_owned()),
            extensions: Vec::new(),
            externals: vec![],
        };
        let json = serde_json::to_string(&scope).unwrap();
        assert_eq!(json, r#"{"source_root":"src/cf","extensions":[]}"#);
        // And a baseline written before the base became optional still loads.
        let parsed: DiagnosticsBaselineProjectScope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, scope);
    }

    #[test]
    fn extensions_auto_discovery_supports_russian_container() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "Расширения/FeatureA");
        touch_extension(root, "Расширения/FeatureB");

        let resolved = Project::resolve_extensions(root, &ProjectConfig::default()).unwrap();
        let names: Vec<&str> = resolved.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["FeatureA", "FeatureB"]);
    }

    #[test]
    fn extension_only_russian_layout_is_not_misidentified_as_base_configuration() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let extension = root.join("Расширения/Feature");
        std::fs::create_dir_all(&extension).unwrap();
        std::fs::write(
            extension.join("Configuration.xml"),
            "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
        )
        .unwrap();
        let config = ProjectConfig {
            extensions: Some(vec!["Расширения/Feature".into()]),
            ..Default::default()
        };

        let project = Project::with_config(root, config).unwrap();
        assert!(project.configuration_path().is_none());
        assert_eq!(project.source_roots(), vec![extension]);
    }

    #[test]
    fn extensions_auto_discovery_falls_back_to_bare_cfe() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "cfe/SomeExt");

        let resolved = Project::resolve_extensions(root, &ProjectConfig::default()).unwrap();
        let names: Vec<&str> = resolved.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["SomeExt"]);
    }

    #[test]
    fn explicit_extensions_disable_auto_discovery() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/BMS_RU_UT");
        touch_extension(root, "src/cfe/YAxUnit");

        // An explicit list must win, even when it points elsewhere — no src/cfe sweep.
        let config = ProjectConfig {
            extensions: Some(vec!["src/cfe/YAxUnit".into()]),
            ..Default::default()
        };
        let resolved = Project::resolve_extensions(root, &config).unwrap();
        let names: Vec<&str> = resolved.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["YAxUnit"]);
    }

    #[test]
    fn explicit_empty_extensions_opt_out_of_discovery() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/BMS_RU_UT");

        // `extensions = []` is an explicit opt-out and must NOT auto-discover src/cfe.
        let config = ProjectConfig { extensions: Some(vec![]), ..Default::default() };
        let resolved = Project::resolve_extensions(root, &config).unwrap();
        assert!(resolved.is_empty(), "explicit empty list must disable auto-discovery");
    }

    fn write_configuration_xml(root: &std::path::Path, body: &str) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("Configuration.xml"), body).unwrap();
    }

    #[test]
    fn an_extension_root_is_told_apart_from_a_main_configuration() {
        let dir = tempdir().unwrap();

        // A main configuration carries ConfigurationExtensionCompatibilityMode
        // too, so that element cannot be the marker.
        write_configuration_xml(
            &dir.path().join("cf"),
            "<MetaDataObject><Configuration><Properties>\
             <ConfigurationExtensionCompatibilityMode>8.3.21</ConfigurationExtensionCompatibilityMode>\
             </Properties></Configuration></MetaDataObject>",
        );
        write_configuration_xml(
            &dir.path().join("cfe"),
            "<MetaDataObject><Configuration><Properties>\
             <ConfigurationExtensionCompatibilityMode>8.3.21</ConfigurationExtensionCompatibilityMode>\
             <ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>\
             </Properties></Configuration></MetaDataObject>",
        );

        assert_eq!(configuration_kind(&dir.path().join("cf")), ConfigurationKind::Configuration);
        assert_eq!(configuration_kind(&dir.path().join("cfe")), ConfigurationKind::Extension);
        assert_eq!(configuration_kind(&dir.path().join("absent")), ConfigurationKind::Unknown);
    }

    #[test]
    fn the_marker_is_found_past_a_realistic_amount_of_preamble() {
        let dir = tempdir().unwrap();
        // Real dumps put the marker about 3 KB in; pad well beyond that to show
        // the probe window is not the binding constraint.
        let padding = " ".repeat(64 * 1024);
        write_configuration_xml(
            &dir.path().join("cfe"),
            &format!(
                "<MetaDataObject>{padding}<Configuration><Properties>\
                 <ConfigurationExtensionPurpose>Patch</ConfigurationExtensionPurpose>\
                 </Properties></Configuration></MetaDataObject>"
            ),
        );

        assert_eq!(configuration_kind(&dir.path().join("cfe")), ConfigurationKind::Extension);
    }

    #[test]
    fn the_marker_is_found_far_past_any_fixed_probe_window() {
        let dir = tempdir().unwrap();
        // Padding well past any fixed prefix bound: the scan is ended by
        // `</Properties>`, not by a byte count.
        let padding = "x".repeat(512 * 1024);
        write_configuration_xml(
            &dir.path().join("cfe"),
            &format!(
                "<MetaDataObject><Configuration><Properties><Comment>{padding}</Comment>\
                 <ConfigurationExtensionPurpose>Patch</ConfigurationExtensionPurpose>\
                 </Properties></Configuration></MetaDataObject>"
            ),
        );

        assert_eq!(configuration_kind(&dir.path().join("cfe")), ConfigurationKind::Extension);
    }

    #[test]
    fn a_marker_inside_a_comment_declares_nothing() {
        let dir = tempdir().unwrap();
        write_configuration_xml(
            &dir.path().join("cf"),
            "<MetaDataObject><Configuration><Properties>\
             <!-- <ConfigurationExtensionPurpose>fake</ConfigurationExtensionPurpose> -->\
             </Properties></Configuration></MetaDataObject>",
        );

        assert_eq!(configuration_kind(&dir.path().join("cf")), ConfigurationKind::Configuration);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_named_configuration_xml_does_not_block_the_scan() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let root = dir.path().join("weird");
        std::fs::create_dir_all(&root).unwrap();
        let fifo = root.join("Configuration.xml");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `path` is a valid NUL-terminated string for the duration of
        // the call, and mkfifo touches nothing else.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0, "mkfifo failed");

        // Opening a writer-less FIFO blocks forever, so the failure mode is a
        // hang, not a wrong answer — a watchdog is the only way to see it fail
        // instead of freezing the suite. The same call runs during LSP startup.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(configuration_kind(&root));
        });

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(kind) => assert_eq!(kind, ConfigurationKind::Unknown),
            Err(_) => panic!("configuration_kind blocked on a FIFO"),
        }
    }

    #[test]
    fn override_replaces_extensions_and_keeps_unset_root() {
        let mut config = ProjectConfig {
            configuration_root: Some("src/cf".to_string()),
            extensions: Some(vec!["src/cfe/FromConfig".into()]),
            ..Default::default()
        };
        let over = SourceSetOverride {
            extensions: Some(vec![structured("Flag", "vendor/flag", &[])]),
            ..Default::default()
        };

        over.apply_to(&mut config);

        assert_eq!(config.configuration_root.as_deref(), Some("src/cf"), "root untouched");
        assert_eq!(
            config.extensions,
            Some(vec![structured("Flag", "vendor/flag", &[])]),
            "list replaced outright, not merged"
        );
    }

    #[test]
    fn override_replaces_root_and_keeps_unset_extensions() {
        let mut config = ProjectConfig {
            configuration_root: Some("src/cf".to_string()),
            extensions: Some(vec!["src/cfe/FromConfig".into()]),
            ..Default::default()
        };
        let over =
            SourceSetOverride { configuration_root: Some("other/cf".into()), ..Default::default() };

        over.apply_to(&mut config);

        assert_eq!(config.configuration_root.as_deref(), Some("other/cf"));
        assert_eq!(config.extensions, Some(vec!["src/cfe/FromConfig".into()]));
    }

    #[test]
    fn empty_override_changes_nothing() {
        let original = ProjectConfig {
            configuration_root: Some("src/cf".to_string()),
            extensions: Some(vec!["src/cfe/FromConfig".into()]),
            ..Default::default()
        };
        let mut config = original.clone();
        let over = SourceSetOverride::default();
        assert!(over.is_empty());

        over.apply_to(&mut config);

        assert_eq!(config.configuration_root, original.configuration_root);
        assert_eq!(config.extensions, original.extensions);
    }

    #[test]
    fn override_empty_list_opts_out_of_a_configured_list() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/FromConfig");

        let mut config = ProjectConfig {
            extensions: Some(vec!["src/cfe/FromConfig".into()]),
            ..Default::default()
        };
        SourceSetOverride { extensions: Some(vec![]), ..Default::default() }.apply_to(&mut config);

        let resolved = Project::resolve_extensions(root, &config).unwrap();
        assert!(resolved.is_empty(), "explicit empty override must drop the configured list");
    }

    #[test]
    fn override_empty_list_opts_out_of_auto_discovery() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/Discovered");

        // Unset in the config would auto-discover `src/cfe/*`; the override must
        // reach the same opt-out the config expresses as `extensions = []`.
        let mut config = ProjectConfig::default();
        SourceSetOverride { extensions: Some(vec![]), ..Default::default() }.apply_to(&mut config);

        let resolved = Project::resolve_extensions(root, &config).unwrap();
        assert!(resolved.is_empty(), "explicit empty override must disable auto-discovery");
    }

    #[test]
    fn override_list_beats_a_configured_opt_out() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "vendor/flag");

        let mut config = ProjectConfig { extensions: Some(vec![]), ..Default::default() };
        SourceSetOverride {
            extensions: Some(vec![structured("Flag", "vendor/flag", &[])]),
            ..Default::default()
        }
        .apply_to(&mut config);

        let resolved = Project::resolve_extensions(root, &config).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "Flag");
    }

    #[test]
    fn same_source_set_from_config_and_from_override_share_a_fingerprint() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cf");
        touch_extension(root, "ext");

        let from_config = ProjectConfig {
            configuration_root: Some("src/cf".to_string()),
            extensions: Some(vec!["ext".into()]),
            ..Default::default()
        };

        // The flag spelling of the same set: a named entry whose derived name
        // matches the directory the bare path resolves to.
        let mut from_override = ProjectConfig::default();
        SourceSetOverride {
            configuration_root: Some("src/cf".into()),
            extensions: Some(vec![structured("ext", "ext", &[])]),
            externals: None,
        }
        .apply_to(&mut from_override);

        let a = Project::with_config(root, from_config).unwrap();
        let b = Project::with_config(root, from_override).unwrap();

        assert_eq!(
            a.extension_topology().fingerprint(),
            b.extension_topology().fingerprint(),
            "identical sets must share project identity regardless of how they were declared"
        );
    }

    #[test]
    fn override_root_reaches_configuration_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Negative control: without the override the config declares no root.
        let bare = ProjectConfig::default();
        assert_eq!(bare.configuration_path(root), None);

        let mut config = ProjectConfig::default();
        SourceSetOverride { configuration_root: Some("base".into()), ..Default::default() }
            .apply_to(&mut config);

        assert_eq!(
            config.configuration_path(root),
            Some(root.join("base")),
            "the override must be visible to the config reads that happen before Project is built"
        );
    }

    #[test]
    fn toml_distinguishes_unset_from_empty_extensions() {
        let unset: super::TomlConfig = toml::from_str("[source]\nroot = \"src/cf\"\n").unwrap();
        assert_eq!(ProjectConfig::from(unset).extensions, None, "unset → None (discovery)");

        let empty: super::TomlConfig =
            toml::from_str("[source]\nroot = \"src/cf\"\nextensions = []\n").unwrap();
        assert_eq!(ProjectConfig::from(empty).extensions, Some(vec![]), "[] → Some([]) (opt-out)");
    }

    #[test]
    fn source_exclude_is_resolved_from_the_config_directory_not_the_source_root() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("project");
        let config_dir = project_root.join("ci");
        fs::create_dir_all(&config_dir).unwrap();
        let path = config_dir.join("bsl-analyzer.toml");
        fs::write(&path, "[source]\nroot = \"../src/cf\"\nexclude = [\".tmp\"]\n").unwrap();

        let config = ProjectConfig::load_from_file(&path).unwrap();
        let excluded = config.source_exclusions(&project_root).unwrap();

        assert!(excluded.is_excluded(&config_dir.join(".tmp/Module.bsl")));
        assert!(!excluded.is_excluded(&project_root.join("src/cf/.tmp/Module.bsl")));
        assert!(!excluded.is_excluded(&project_root.join(".tmp/Module.bsl")));

        fs::write(&path, "[source]\nroot = \"../src/cf\"\nexclude = [\".\"]\n").unwrap();
        let nested =
            ProjectConfig::load_from_file(&path).unwrap().source_exclusions(&project_root).unwrap();
        assert!(nested.is_excluded(&config_dir.join("anything/Module.bsl")));
        assert!(
            !nested.is_excluded(&project_root.join("src/cf/Module.bsl")),
            "a nested config resolved '.' from the project root instead of its own directory"
        );
    }

    #[test]
    fn source_exclude_is_resolved_from_an_external_config_directory() {
        let project = tempdir().unwrap();
        let config = tempdir().unwrap();
        let path = config.path().join("bsl-analyzer.toml");
        fs::write(&path, "[source]\nroot = \".\"\nexclude = [\".\"]\n").unwrap();

        let exclusions = ProjectConfig::load_from_file(&path)
            .unwrap()
            .source_exclusions(project.path())
            .unwrap();

        assert!(exclusions.is_excluded(&config.path().join("nested/Hidden.bsl")));
        assert!(
            !exclusions.is_excluded(&project.path().join("Allowed.bsl")),
            "an external config resolved its exclusion from the project directory"
        );
    }

    #[test]
    fn an_explicit_empty_source_exclude_is_the_same_as_an_absent_one() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/Allowed.bsl"), "").unwrap();
        let path = root.join("bsl-analyzer.toml");

        let load = |body: &str| {
            fs::write(&path, body).unwrap();
            let config = ProjectConfig::load_from_file(&path).unwrap();
            let exclusions = config.source_exclusions(root).unwrap();
            let project = Project::with_config(root, config).unwrap();
            let scan = SourceSet::scan_in_scope(&project.source_roots(), &[], &exclusions);
            (
                exclusions,
                project.source_roots(),
                project.semantic_base_path().map(|path| path.to_path_buf()),
                scan,
            )
        };

        let absent = load("[source]\nroot = \"src\"\n");
        let explicit = load("[source]\nroot = \"src\"\nexclude = []\n");
        assert_eq!(absent.0, explicit.0);
        assert_eq!(absent.1, explicit.1);
        assert_eq!(absent.2, explicit.2);
        let signature = |scan: &SourceSet| {
            scan.files
                .iter()
                .map(|file| (file.walked.clone(), file.canonical.clone(), file.role))
                .collect::<Vec<_>>()
        };
        assert_eq!(signature(&absent.3), signature(&explicit.3));
        assert!(explicit.3.files.iter().any(|file| file.walked.ends_with("Allowed.bsl")));
    }

    #[test]
    fn source_exclude_rejects_non_lists_absolute_paths_and_empty_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bsl-analyzer.toml");

        fs::write(&path, "[source]\nexclude = \".tmp\"\n").unwrap();
        assert!(ProjectConfig::load_from_file(&path).is_err(), "a string is not the list contract");

        let absolute = dir.path().join("outside");
        fs::write(&path, format!("[source]\nexclude = [{:?}]\n", absolute.to_string_lossy()))
            .unwrap();
        assert!(ProjectConfig::load_from_file(&path).is_err(), "an absolute path was accepted");

        fs::write(&path, "[source]\nexclude = [\"\"]\n").unwrap();
        assert!(ProjectConfig::load_from_file(&path).is_err(), "an empty path was accepted");
    }

    #[test]
    fn excluding_every_declared_source_yields_an_intentionally_empty_project() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_configuration_xml(&root.join("src/cf"), "<Configuration/>");
        touch_extension(root, "src/cfe/VisibleWithoutExclude");

        let ordinary = Project::with_config(
            root,
            ProjectConfig {
                configuration_root: Some("src/cf".to_owned()),
                extensions: Some(vec!["src/cfe/VisibleWithoutExclude".into()]),
                ..ProjectConfig::default()
            },
        )
        .unwrap();
        assert_eq!(ordinary.source_roots().len(), 2, "positive control: both roots exist");

        let excluded = Project::with_config(
            root,
            ProjectConfig {
                configuration_root: Some("src/cf".to_owned()),
                extensions: Some(vec!["src/cfe/VisibleWithoutExclude".into()]),
                source_exclude: vec!["src".to_owned()],
                ..ProjectConfig::default()
            },
        )
        .unwrap();
        assert!(excluded.source_roots().is_empty(), "the workspace fallback restored the roots");
        assert!(excluded.semantic_base_path().is_none(), "an excluded base stayed semantic");
    }

    #[test]
    fn exclusions_apply_to_the_descriptorless_fallback_and_to_a_discovered_configuration() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/.tmp")).unwrap();
        fs::write(root.join("src/Allowed.bsl"), "").unwrap();
        fs::write(root.join("src/.tmp/Hidden.bsl"), "").unwrap();
        let config = ProjectConfig {
            configuration_root: Some("src".to_owned()),
            source_exclude: vec!["src/.tmp".to_owned()],
            ..ProjectConfig::default()
        };

        let fallback = Project::with_config(root, config.clone()).unwrap();
        assert_eq!(fallback.source_roots(), [root.to_path_buf()]);
        let fallback_scan = crate::SourceSet::scan_in_scope(
            &fallback.source_roots(),
            &[],
            fallback.source_exclusions(),
        );
        assert!(fallback_scan.files.iter().any(|file| file.walked.ends_with("src/Allowed.bsl")));
        assert!(fallback_scan.files.iter().all(|file| !file.walked.ends_with("Hidden.bsl")));

        write_configuration_xml(&root.join("src"), "<Configuration/>");
        let discovered = Project::with_config(root, config).unwrap();
        assert_eq!(discovered.source_roots(), [root.join("src")]);
        let discovered_scan = crate::SourceSet::scan_in_scope(
            &discovered.source_roots(),
            &[],
            discovered.source_exclusions(),
        );
        assert!(discovered_scan.files.iter().any(|file| file.walked.ends_with("Allowed.bsl")));
        assert!(discovered_scan.files.iter().all(|file| !file.walked.ends_with("Hidden.bsl")));

        let root_mode_dir = tempdir().unwrap();
        let project_root = root_mode_dir.path();
        fs::create_dir_all(project_root.join(".tmp")).unwrap();
        fs::write(project_root.join("Allowed.bsl"), "").unwrap();
        fs::write(project_root.join(".tmp/Hidden.bsl"), "").unwrap();
        let root_config = ProjectConfig {
            configuration_root: Some(".".to_owned()),
            source_exclude: vec![".tmp".to_owned()],
            ..ProjectConfig::default()
        };
        let before_descriptor = Project::with_config(project_root, root_config.clone()).unwrap();
        assert_eq!(before_descriptor.source_roots(), [project_root.to_path_buf()]);
        let before_scan = crate::SourceSet::scan_in_scope(
            &before_descriptor.source_roots(),
            &[],
            before_descriptor.source_exclusions(),
        );
        assert!(before_scan.files.iter().any(|file| file.walked.ends_with("Allowed.bsl")));
        assert!(before_scan.files.iter().all(|file| !file.walked.ends_with("Hidden.bsl")));

        write_configuration_xml(project_root, "<Configuration/>");
        let root_discovered = Project::with_config(project_root, root_config).unwrap();
        assert_eq!(root_discovered.source_roots(), [project_root.to_path_buf()]);
        let root_scan = crate::SourceSet::scan_in_scope(
            &root_discovered.source_roots(),
            &[],
            root_discovered.source_exclusions(),
        );
        assert!(root_scan.files.iter().any(|file| file.walked.ends_with("Allowed.bsl")));
        assert!(root_scan.files.iter().all(|file| !file.walked.ends_with("Hidden.bsl")));
    }

    #[test]
    fn source_exclusions_prune_recursive_configuration_and_extension_discovery() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_configuration_xml(&root.join("a-hidden"), "<Configuration/>");
        write_configuration_xml(&root.join("z-allowed"), "<Configuration/>");
        touch_extension(root, "src/cfe/Hidden");
        touch_extension(root, "src/cfe/Allowed");

        let project = Project::with_config(
            root,
            ProjectConfig {
                source_exclude: vec!["a-hidden".to_owned(), "src/cfe/Hidden".to_owned()],
                ..ProjectConfig::default()
            },
        )
        .unwrap();

        assert_eq!(project.semantic_base_path(), Some(root.join("z-allowed").as_path()));
        assert_eq!(
            project.extension_paths(),
            &[("Allowed".to_owned(), root.join("src/cfe/Allowed"))],
            "automatic extension discovery admitted an excluded export"
        );
        assert!(project.source_roots().iter().all(|path| !path.starts_with(root.join("a-hidden"))));
        assert!(project
            .source_roots()
            .iter()
            .all(|path| !path.starts_with(root.join("src/cfe/Hidden"))));

        let nested_dir = tempdir().unwrap();
        let nested_root = nested_dir.path();
        write_configuration_xml(&nested_root.join("excluded/deep"), "<Configuration/>");
        std::fs::write(nested_root.join("Allowed.bsl"), "").unwrap();
        let fallback = Project::with_config(
            nested_root,
            ProjectConfig {
                source_exclude: vec!["excluded".to_owned()],
                ..ProjectConfig::default()
            },
        )
        .unwrap();
        assert_eq!(
            fallback.source_roots(),
            [nested_root.to_path_buf()],
            "recursive discovery entered an excluded parent and suppressed the ordinary fallback"
        );
    }

    #[test]
    fn source_exclusions_follow_validation_and_rebuild_survivor_closures() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("base"), "<Configuration/>");
        touch_extension(dir.path(), "ext/a");
        touch_extension(dir.path(), "ext/b");
        touch_extension(dir.path(), "ext/c");

        let config = ProjectConfig {
            configuration_root: Some("base".to_owned()),
            extensions: Some(vec![
                structured("A", "ext/a", &[]),
                structured("B", "ext/b", &["A"]),
                structured("C", "ext/c", &["B"]),
            ]),
            source_exclude: vec!["ext/b".to_owned()],
            ..ProjectConfig::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        let topology = project.extension_topology();
        let names: Vec<_> = topology.nodes().iter().map(|node| node.name()).collect();
        assert_eq!(names, ["A", "C"], "the excluded node survived topology filtering");
        let c = topology.nodes().iter().find(|node| node.name() == "C").unwrap();
        let closure_names: Vec<_> =
            c.closure().iter().map(|id| topology.node(*id).name()).collect();
        assert_eq!(closure_names, ["A"], "the survivor closure retained or bypassed B");
        assert!(c.depends_on().is_empty(), "a direct edge to the excluded node survived");

        let invalid = ProjectConfig {
            configuration_root: Some("base".to_owned()),
            extensions: Some(vec![structured("C", "ext/c", &["Missing"])]),
            source_exclude: vec!["ext/c".to_owned()],
            ..ProjectConfig::default()
        };
        assert!(
            matches!(
                Project::with_config(dir.path(), invalid),
                Err(ProjectError::Topology(TopologyError::UnknownDependency { .. }))
            ),
            "excluding the invalid node must not make an invalid declaration valid"
        );
    }

    #[test]
    fn no_extensions_when_no_cfe_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cf"); // main config only, no extensions dir

        let resolved = Project::resolve_extensions(root, &ProjectConfig::default()).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn extension_glob_rejects_wildcard_outside_final_segment() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/BMS_RU_UT");

        // Trailing slash (empty final segment) and parent-segment wildcards are
        // unsupported and must resolve nothing rather than silently misbehave.
        for pat in ["src/cfe/*/", "src/*/BMS_RU_UT"] {
            let config = ProjectConfig { extensions: Some(vec![pat.into()]), ..Default::default() };
            let resolved = Project::resolve_extensions(root, &config).unwrap();
            assert!(resolved.is_empty(), "pattern {pat} must resolve no extensions");
        }
    }

    #[test]
    fn extension_glob_dedups_dot_slash_against_glob() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch_extension(root, "src/cfe/BMS_RU_UT");

        let config = ProjectConfig {
            extensions: Some(vec!["src/cfe/*".into(), "./src/cfe/BMS_RU_UT".into()]),
            ..Default::default()
        };
        let resolved = Project::resolve_extensions(root, &config).unwrap();
        assert_eq!(resolved.len(), 1, "`./`-prefixed literal must dedup against the glob result");
    }

    #[test]
    fn wildcard_matches_basic_cases() {
        assert!(wildcard_matches("*", "anything"));
        assert!(wildcard_matches("БУС_*", "БУС_ОбщегоНазначения"));
        assert!(wildcard_matches("*_UT", "BMS_RU_UT"));
        assert!(wildcard_matches("a*b*c", "axxbyyc"));
        assert!(wildcard_matches("bms_ru_ut", "BMS_RU_UT")); // case-insensitive
        assert!(!wildcard_matches("БУС_*", "YAxUnit"));
        assert!(!wildcard_matches("a*b", "axxc"));
    }

    #[test]
    fn toml_config_deserializes_full_baseline() {
        let toml_str = r#"
[source]
root = "src/cf"

[search.baseline]
backend = "postgres"

[search.baseline.postgres]
host = "pg-central.company.com"
port = 5432
dbname = "bsl_search"
schema = "bsl_search"
vault_role_base = "prod/search/bsl-analyzer"

[search.baseline.postgres.credential_helper]
program = "rtools"
args = ["vault", "credential-helper"]

[search.baseline.workspace_code]
branch = "develop"

[search.baseline.workspace_code.policy]
publish_branches = ["vendor", "develop"]

[[search.baseline.workspace_code.policy.branches]]
match = "develop"
select_branch = "develop"
fallback_branch = "vendor"

[[search.baseline.workspace_code.policy.branches]]
match = "feature/*"
select_branch = "develop"
fallback_branch = "vendor"
"#;
        let config: super::TomlConfig = toml::from_str(toml_str).unwrap();
        let project = ProjectConfig::from(config);
        assert_eq!(project.configuration_root.as_deref(), Some("src/cf"));
        assert_eq!(project.search.baseline.backend, SearchBaselineBackend::Postgres);
        assert_eq!(project.search.baseline.workspace_code.branch.as_deref(), Some("develop"));
        assert_eq!(
            project.search.baseline.workspace_code.policy.publish_branches,
            vec!["vendor", "develop"]
        );
        assert_eq!(project.search.baseline.workspace_code.policy.branches.len(), 2);
        assert_eq!(project.search.baseline.workspace_code.policy.branches[0].pattern, "develop");

        let pg = &project.search.baseline.postgres;
        assert_eq!(pg.host.as_deref(), Some("pg-central.company.com"));
        assert_eq!(pg.port, Some(5432));
        assert_eq!(pg.dbname.as_deref(), Some("bsl_search"));
        assert_eq!(pg.schema.as_deref(), Some("bsl_search"));
        assert_eq!(pg.vault_role_base.as_deref(), Some("prod/search/bsl-analyzer"));
        assert_eq!(pg.credential_helper.program.as_deref(), Some("rtools"));
        assert_eq!(pg.credential_helper.args, vec!["vault", "credential-helper"]);
    }

    #[test]
    fn toml_diagnostics_converts_to_json_value() {
        let toml_str = r#"
[diagnostics.parameters]
EmptyCodeBlock = false
LineLength = { maxLineLength = 120 }
"#;
        let config: super::TomlConfig = toml::from_str(toml_str).unwrap();
        let project = ProjectConfig::from(config);
        let rules = project.diagnostics.rules_json();
        assert!(rules.is_object());
        assert_eq!(rules["parameters"]["EmptyCodeBlock"], false);
        assert_eq!(rules["parameters"]["LineLength"]["maxLineLength"], 120);
    }

    #[test]
    fn diagnostics_baseline_path_uses_toml_origin_and_stays_out_of_rules() {
        let dir = tempdir().unwrap();
        let config_dir = dir.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        let path = config_dir.join("project.toml");
        fs::write(
            &path,
            r#"
[diagnostics.baseline]
path = "../baseline.json"

[diagnostics.parameters]
LineLength = { maxLineLength = 120 }
"#,
        )
        .unwrap();

        let config = ProjectConfig::load_from_file(&path).unwrap();
        assert_eq!(config.config_file_path(), Some(path.as_path()));
        assert_eq!(
            config.diagnostics_baseline_path(dir.path()),
            Some(config_dir.join("../baseline.json"))
        );
        let rules = config.diagnostics.rules_json();
        assert!(rules.get("baseline").is_none());
        assert_eq!(rules["parameters"]["LineLength"]["maxLineLength"], 120);
    }

    #[test]
    fn diagnostics_baseline_path_uses_json_origin_and_stays_out_of_rules() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("custom.json");
        fs::write(
            &path,
            r#"{"diagnostics":{"baseline":{"path":"baseline.json"},"parameters":{"EmptyCodeBlock":false}}}"#,
        )
        .unwrap();

        let config = ProjectConfig::load_from_file(&path).unwrap();
        assert_eq!(config.config_file_path(), Some(path.as_path()));
        assert_eq!(
            config.diagnostics_baseline_path(std::path::Path::new("ignored")),
            Some(dir.path().join("baseline.json"))
        );
        let rules = config.diagnostics.rules_json();
        assert!(rules.get("baseline").is_none());
        assert_eq!(rules["parameters"]["EmptyCodeBlock"], false);
    }

    #[test]
    fn partitioned_baseline_config_contract() {
        let dir = tempdir().unwrap();
        let config_dir = dir.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        let path = config_dir.join("project.toml");
        fs::write(
            &path,
            r#"
[diagnostics.baseline]
directory = "baselines"

[[diagnostics.baseline.groups]]
name = "vendor"
extensions = ["VendorCore", "VendorReports"]
"#,
        )
        .unwrap();

        let config = ProjectConfig::load_from_file(&path).unwrap();
        assert_eq!(config.diagnostics_baseline_path(dir.path()), None);
        assert_eq!(
            config.diagnostics_baseline_directory(dir.path()),
            Some(config_dir.join("baselines"))
        );
        let resolved = Project::with_config(dir.path(), config)
            .unwrap()
            .diagnostics_baseline()
            .unwrap()
            .unwrap();
        assert_eq!(resolved.project_path, "config/baselines");
        assert_eq!(
            resolved.mode,
            DiagnosticsBaselineProjectMode::Partitioned {
                groups: vec![DiagnosticsBaselineGroupConfig {
                    name: "vendor".to_owned(),
                    extensions: vec!["VendorCore".to_owned(), "VendorReports".to_owned()],
                }],
                include: None,
            }
        );
    }

    #[test]
    fn selective_baseline_default_includes_all() {
        let config: ProjectConfig = toml::from_str(
            r#"
[diagnostics.baseline]
directory = "baselines"
"#,
        )
        .unwrap();
        assert_eq!(config.diagnostics.baseline.unwrap().include, None);
    }

    #[test]
    fn selective_baseline_include_selects_exact_partition_ids() {
        let config: ProjectConfig = toml::from_str(
            r#"
[diagnostics.baseline]
directory = "baselines"
include = ["main", "extension:Sales"]
"#,
        )
        .unwrap();
        assert_eq!(
            config.diagnostics.baseline.unwrap().include,
            Some(vec!["main".to_owned(), "extension:Sales".to_owned()])
        );
    }

    #[test]
    fn selective_baseline_rejects_empty_duplicate_unknown_and_legacy_include() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        touch_extension(dir.path(), "src/cfe/Sales");
        touch_extension(dir.path(), "src/cfe/Vendor");

        for include in [vec![], vec!["main", "main"], vec!["main", "MAIN"]] {
            let mut diagnostics = partitioned_config("baselines", vec![]);
            diagnostics.baseline.as_mut().unwrap().include =
                Some(include.into_iter().map(str::to_owned).collect());
            let error = Project::with_config(
                dir.path(),
                ProjectConfig {
                    diagnostics,
                    configuration_root: Some("src/cf".to_owned()),
                    extensions: Some(vec![structured("Sales", "src/cfe/Sales", &[])]),
                    ..Default::default()
                },
            )
            .unwrap()
            .diagnostics_baseline_partition_plan()
            .unwrap_err();
            assert!(matches!(error, DiagnosticsBaselineProjectError::InvalidConfig(_)));
        }

        for include in [vec!["extension:Missing"], vec!["extension:Vendor"]] {
            let mut diagnostics = partitioned_config(
                "baselines",
                vec![DiagnosticsBaselineGroupConfig {
                    name: "vendor".to_owned(),
                    extensions: vec!["Vendor".to_owned()],
                }],
            );
            diagnostics.baseline.as_mut().unwrap().include =
                Some(include.into_iter().map(str::to_owned).collect());
            let error = Project::with_config(
                dir.path(),
                ProjectConfig {
                    diagnostics,
                    configuration_root: Some("src/cf".to_owned()),
                    extensions: Some(vec![structured("Vendor", "src/cfe/Vendor", &[])]),
                    ..Default::default()
                },
            )
            .unwrap()
            .diagnostics_baseline_partition_plan()
            .unwrap_err();
            assert!(matches!(error, DiagnosticsBaselineProjectError::InvalidConfig(_)));
        }

        let config = ProjectConfig {
            diagnostics: ProjectDiagnosticsConfig {
                baseline: Some(DiagnosticsBaselineConfig {
                    path: Some("baseline.json".to_owned()),
                    include: Some(vec!["main".to_owned()]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            extensions: Some(vec![]),
            ..Default::default()
        };
        let error =
            Project::with_config(dir.path(), config).unwrap().diagnostics_baseline().unwrap_err();
        assert!(matches!(
            error,
            DiagnosticsBaselineProjectError::InvalidConfig(message)
                if message == "path cannot be combined with groups or include"
        ));
    }

    #[test]
    fn selective_baseline_config_contract() {
        selective_baseline_default_includes_all();
        selective_baseline_include_selects_exact_partition_ids();
        selective_baseline_rejects_empty_duplicate_unknown_and_legacy_include();
    }

    #[test]
    fn partitioned_baseline_config_contract_rejects_conflicting_modes() {
        let dir = tempdir().unwrap();
        for baseline in [
            DiagnosticsBaselineConfig {
                path: Some("baseline.json".to_owned()),
                directory: Some("baselines".to_owned()),
                include: None,
                groups: vec![],
            },
            DiagnosticsBaselineConfig {
                path: Some("baseline.json".to_owned()),
                directory: None,
                include: None,
                groups: vec![DiagnosticsBaselineGroupConfig {
                    name: "vendor".to_owned(),
                    extensions: vec!["Ext".to_owned()],
                }],
            },
            DiagnosticsBaselineConfig::default(),
        ] {
            let config = ProjectConfig {
                diagnostics: ProjectDiagnosticsConfig {
                    baseline: Some(baseline),
                    ..Default::default()
                },
                extensions: Some(vec![]),
                ..Default::default()
            };
            let error = Project::with_config(dir.path(), config)
                .unwrap()
                .diagnostics_baseline()
                .unwrap_err();
            assert!(matches!(error, DiagnosticsBaselineProjectError::InvalidConfig(_)));
        }
    }

    #[test]
    fn partitioned_baseline_config_contract_rejects_absolute_and_parent_paths() {
        let dir = tempdir().unwrap();
        for directory in ["../baselines", "/tmp/baselines"] {
            let config = ProjectConfig {
                diagnostics: ProjectDiagnosticsConfig {
                    baseline: Some(DiagnosticsBaselineConfig {
                        directory: Some(directory.to_owned()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                extensions: Some(vec![]),
                ..Default::default()
            };
            let error = Project::with_config(dir.path(), config)
                .unwrap()
                .diagnostics_baseline()
                .unwrap_err();
            assert!(matches!(error, DiagnosticsBaselineProjectError::InvalidConfig(_)));
        }
    }

    pub(super) fn partitioned_config(
        directory: &str,
        groups: Vec<DiagnosticsBaselineGroupConfig>,
    ) -> ProjectDiagnosticsConfig {
        ProjectDiagnosticsConfig {
            baseline: Some(DiagnosticsBaselineConfig {
                directory: Some(directory.to_owned()),
                groups,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn diagnostics_partition_identity_and_ownership() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        for name in ["A", "B", "C"] {
            touch_extension(dir.path(), &format!("src/cfe/{name}"));
        }
        let config = ProjectConfig {
            diagnostics: partitioned_config(
                "baselines",
                vec![DiagnosticsBaselineGroupConfig {
                    name: "vendor".to_owned(),
                    extensions: vec!["B".to_owned(), "A".to_owned()],
                }],
            ),
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![
                structured("B", "src/cfe/B", &["A"]),
                structured("A", "src/cfe/A", &[]),
                structured("C", "src/cfe/C", &[]),
            ]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        let first = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let second = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.partitions.iter().map(|partition| partition.id.as_str()).collect::<Vec<_>>(),
            ["main", "group:vendor", "extension:C"]
        );
        let group = &first.partitions[1].identity;
        let DiagnosticsBaselinePartitionIdentity::Group { members, .. } = group else {
            panic!("expected group identity")
        };
        assert_eq!(
            members.iter().map(|member| member.name.as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
        assert_eq!(members[1].depends_on, ["A"]);
        assert_eq!(
            first.owner_for_project_path("src/cf/CommonModules/Main/Ext/Module.bsl"),
            Some("main")
        );
        assert_eq!(
            first.owner_for_project_path("src/cfe/A/CommonModules/A/Ext/Module.bsl"),
            Some("group:vendor")
        );
        assert_eq!(
            first.owner_for_project_path("src/cfe/C/CommonModules/C/Ext/Module.bsl"),
            Some("extension:C")
        );
        assert_eq!(first.owner_for_project_path("outside/file.bsl"), None);
        assert!(first.partitions.iter().all(|partition| {
            partition.key.len() == 64
                && partition
                    .key
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }));
    }

    #[test]
    fn selective_baseline_policy_plan() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        for name in ["A", "B", "C"] {
            touch_extension(dir.path(), &format!("src/cfe/{name}"));
        }

        let project = |extensions: &[&str], include: Option<Vec<&str>>| {
            let mut diagnostics = partitioned_config("baselines", vec![]);
            diagnostics.baseline.as_mut().unwrap().include = include
                .map(|selectors| selectors.into_iter().map(str::to_owned).collect::<Vec<_>>());
            Project::with_config(
                dir.path(),
                ProjectConfig {
                    diagnostics,
                    configuration_root: Some("src/cf".to_owned()),
                    extensions: Some(
                        extensions
                            .iter()
                            .map(|name| structured(name, &format!("src/cfe/{name}"), &[]))
                            .collect(),
                    ),
                    ..Default::default()
                },
            )
            .unwrap()
        };

        let full =
            project(&["A", "B"], None).diagnostics_baseline_partition_plan().unwrap().unwrap();
        assert_eq!(full.selection, DiagnosticsBaselineSelection::All);
        assert_eq!(full.enabled_partition_ids, ["main", "extension:A", "extension:B"]);
        assert!(full.partitions.iter().all(|partition| {
            full.policy_for_partition(&partition.id)
                == Some(DiagnosticsBaselinePartitionPolicy::Baseline)
        }));

        let selective = project(&["A", "B"], Some(vec!["extension:B", "main"]))
            .diagnostics_baseline_partition_plan()
            .unwrap()
            .unwrap();
        assert_eq!(selective.selection, DiagnosticsBaselineSelection::Selective);
        assert_eq!(selective.enabled_partition_ids, ["main", "extension:B"]);
        assert_eq!(
            selective.policy_for_partition("extension:A"),
            Some(DiagnosticsBaselinePartitionPolicy::Unsuppressed)
        );
        assert_eq!(selective.roots, full.roots);
        assert_eq!(selective.project_scope, full.project_scope);

        let added = project(&["A", "B", "C"], Some(vec!["main", "extension:B"]))
            .diagnostics_baseline_partition_plan()
            .unwrap()
            .unwrap();
        assert_eq!(
            added.policy_for_partition("extension:C"),
            Some(DiagnosticsBaselinePartitionPolicy::Unsuppressed)
        );
        let removed = project(&["B"], Some(vec!["main", "extension:B"]))
            .diagnostics_baseline_partition_plan()
            .unwrap()
            .unwrap();
        assert_eq!(removed.owner_for_project_path("src/cfe/A/file.bsl"), None);

        let error = project(&["C"], Some(vec!["main", "extension:B"]))
            .diagnostics_baseline_partition_plan()
            .unwrap_err();
        assert!(matches!(error, DiagnosticsBaselineProjectError::InvalidConfig(_)));
    }

    #[test]
    fn selective_policy_does_not_change_partition_ownership() {
        selective_baseline_policy_plan();
    }

    #[test]
    fn selective_baseline_selection_fingerprint() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        touch_extension(dir.path(), "src/cfe/A");

        let plan = |include: Vec<&str>| {
            let mut diagnostics = partitioned_config("baselines", vec![]);
            diagnostics.baseline.as_mut().unwrap().include =
                Some(include.into_iter().map(str::to_owned).collect());
            Project::with_config(
                dir.path(),
                ProjectConfig {
                    diagnostics,
                    configuration_root: Some("src/cf".to_owned()),
                    extensions: Some(vec![structured("A", "src/cfe/A", &[])]),
                    ..Default::default()
                },
            )
            .unwrap()
            .diagnostics_baseline_partition_plan()
            .unwrap()
            .unwrap()
        };

        let first = plan(vec!["extension:A", "main"]);
        let reordered = plan(vec!["main", "extension:A"]);
        let selective = plan(vec!["main"]);
        assert_eq!(first.selection_fingerprint, reordered.selection_fingerprint);
        assert_ne!(first.selection_fingerprint, selective.selection_fingerprint);
        assert_eq!(
            selective.selection_fingerprint,
            "de9c18d9f747ca156eefc83917fbaa60d748cd8410ece8f043384354bbad559d"
        );

        touch_extension(dir.path(), "src/cfe/A2");
        let mut diagnostics = partitioned_config("baselines", vec![]);
        diagnostics.baseline.as_mut().unwrap().include = Some(vec!["main".to_owned()]);
        let changed_identity = Project::with_config(
            dir.path(),
            ProjectConfig {
                diagnostics,
                configuration_root: Some("src/cf".to_owned()),
                extensions: Some(vec![structured("A", "src/cfe/A2", &[])]),
                ..Default::default()
            },
        )
        .unwrap()
        .diagnostics_baseline_partition_plan()
        .unwrap()
        .unwrap();
        assert_ne!(selective.selection_fingerprint, changed_identity.selection_fingerprint);
    }

    #[test]
    fn diagnostics_partition_identity_and_ownership_rejects_bad_groups() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        touch_extension(dir.path(), "src/cfe/A");
        let extensions = Some(vec![structured("A", "src/cfe/A", &[])]);
        let cases = [
            vec![DiagnosticsBaselineGroupConfig { name: "empty".to_owned(), extensions: vec![] }],
            vec![DiagnosticsBaselineGroupConfig {
                name: "unknown".to_owned(),
                extensions: vec!["Missing".to_owned()],
            }],
            vec![DiagnosticsBaselineGroupConfig {
                name: "repeat".to_owned(),
                extensions: vec!["A".to_owned(), "a".to_owned()],
            }],
            vec![
                DiagnosticsBaselineGroupConfig {
                    name: "one".to_owned(),
                    extensions: vec!["A".to_owned()],
                },
                DiagnosticsBaselineGroupConfig {
                    name: "two".to_owned(),
                    extensions: vec!["A".to_owned()],
                },
            ],
            vec![
                DiagnosticsBaselineGroupConfig {
                    name: "Vendor".to_owned(),
                    extensions: vec!["A".to_owned()],
                },
                DiagnosticsBaselineGroupConfig {
                    name: "vendor".to_owned(),
                    extensions: vec!["A".to_owned()],
                },
            ],
        ];
        for groups in cases {
            let config = ProjectConfig {
                diagnostics: partitioned_config("baselines", groups),
                configuration_root: Some("src/cf".to_owned()),
                extensions: extensions.clone(),
                ..Default::default()
            };
            let error = Project::with_config(dir.path(), config)
                .unwrap()
                .diagnostics_baseline_partition_plan()
                .unwrap_err();
            assert!(matches!(error, DiagnosticsBaselineProjectError::InvalidGroup(_)));
        }
    }

    #[test]
    fn diagnostics_partition_identity_bounds_machine_ids() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        touch_extension(dir.path(), "src/cfe/A");
        let config = ProjectConfig {
            diagnostics: partitioned_config(
                "baselines",
                vec![DiagnosticsBaselineGroupConfig {
                    name: "x".repeat(200),
                    extensions: vec!["A".to_owned()],
                }],
            ),
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![structured("A", "src/cfe/A", &[])]),
            ..Default::default()
        };
        let error = Project::with_config(dir.path(), config)
            .unwrap()
            .diagnostics_baseline_partition_plan()
            .unwrap_err();
        assert!(matches!(error, DiagnosticsBaselineProjectError::InvalidConfig(_)));
    }

    /// Declaration order of independent extensions is not part of the scope: a
    /// reordered config must not read as a different project and invalidate the whole
    /// published set.
    #[test]
    fn diagnostics_partition_scope_fingerprint_ignores_declaration_order() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        touch_extension(dir.path(), "src/cfe/A");
        touch_extension(dir.path(), "src/cfe/C");
        let plan_for = |order: [&str; 2]| {
            let config = ProjectConfig {
                diagnostics: partitioned_config("baselines", vec![]),
                configuration_root: Some("src/cf".to_owned()),
                extensions: Some(
                    order
                        .iter()
                        .map(|name| structured(name, &format!("src/cfe/{name}"), &[]))
                        .collect(),
                ),
                ..Default::default()
            };
            Project::with_config(dir.path(), config)
                .unwrap()
                .diagnostics_baseline_partition_plan()
                .unwrap()
                .unwrap()
        };
        let straight = plan_for(["A", "C"]);
        let swapped = plan_for(["C", "A"]);
        assert_ne!(
            straight.project_scope.extensions[0].name, swapped.project_scope.extensions[0].name,
            "positive control: the fixture must actually change the declared order"
        );
        assert_eq!(straight.project_scope_fingerprint, swapped.project_scope_fingerprint);
        assert_eq!(straight.selection_fingerprint, swapped.selection_fingerprint);
    }

    /// Extension names in 1C configurations are Russian, and a byte-counted bound
    /// would reject an ordinary one at half the length it allows in Latin.
    #[test]
    fn diagnostics_partition_id_accepts_a_russian_extension_name() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        // Long enough that the id exceeds the limit when counted in BYTES but not in
        // characters: an input on which byte counting and character counting differ.
        let name = "РасширениеДляОбменаСМаркировкой".repeat(3);
        let id = format!("extension:{name}");
        assert!(id.len() > 128 && id.chars().count() <= 128, "id {} bytes", id.len());
        touch_extension(dir.path(), "src/cfe/Marking");
        let config = ProjectConfig {
            diagnostics: partitioned_config("baselines", vec![]),
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![structured(&name, "src/cfe/Marking", &[])]),
            ..Default::default()
        };
        let plan = Project::with_config(dir.path(), config)
            .unwrap()
            .diagnostics_baseline_partition_plan()
            .unwrap()
            .unwrap();
        assert!(plan.partitions.iter().any(|partition| partition.id == id));
    }

    #[test]
    fn diagnostics_partition_identity_and_ownership_rejects_legacy_and_nested_roots() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        touch_extension(dir.path(), "src/cfe/A");
        let legacy = ProjectConfig {
            diagnostics: partitioned_config("baselines", vec![]),
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec!["src/cfe/A".into()]),
            ..Default::default()
        };
        let error = Project::with_config(dir.path(), legacy)
            .unwrap()
            .diagnostics_baseline_partition_plan()
            .unwrap_err();
        assert!(matches!(error, DiagnosticsBaselineProjectError::LegacyExtension(_)));

        let nested_dir = tempdir().unwrap();
        write_configuration_xml(&nested_dir.path().join("src"), "<xml/>");
        touch_extension(nested_dir.path(), "src/cfe/A");
        let nested = ProjectConfig {
            diagnostics: partitioned_config("baselines", vec![]),
            configuration_root: Some("src".to_owned()),
            extensions: Some(vec![structured("A", "src/cfe/A", &[])]),
            ..Default::default()
        };
        let error = Project::with_config(nested_dir.path(), nested)
            .unwrap()
            .diagnostics_baseline_partition_plan()
            .unwrap_err();
        assert!(matches!(error, DiagnosticsBaselineProjectError::PathCollision(_)));
    }

    #[test]
    fn diagnostics_baseline_scope_preserves_topology_and_normalized_paths() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        touch_extension(dir.path(), "src/cfe/vendor");
        touch_extension(dir.path(), "src/cfe/tests");
        let config = ProjectConfig {
            diagnostics: baseline_config("baseline.json"),
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![
                structured("tests", "src/cfe/tests", &["vendor"]),
                structured("vendor", "src/cfe/vendor", &[]),
            ]),
            ..Default::default()
        };

        let resolved = Project::with_config(dir.path(), config)
            .unwrap()
            .diagnostics_baseline()
            .unwrap()
            .unwrap();
        // `path` is resolved THROUGH the filesystem, so it is the canonical spelling of
        // the target; a temporary directory may be reached through a link (`/var` is one
        // to `/private/var` on macOS), and the declared spelling of the fixture root is
        // then not the same string.
        assert_eq!(resolved.path, fs::canonicalize(dir.path()).unwrap().join("baseline.json"));
        assert_eq!(resolved.project_path, "baseline.json");
        assert_eq!(resolved.scope.source_root.as_deref(), Some("src/cf"));
        assert_eq!(resolved.scope.extensions[0].name, "vendor");
        assert_eq!(resolved.scope.extensions[0].path, "src/cfe/vendor");
        assert_eq!(resolved.scope.extensions[1].name, "tests");
        assert_eq!(resolved.scope.extensions[1].depends_on, ["vendor"]);
    }

    #[test]
    fn diagnostics_baseline_scope_is_independent_of_working_directory() {
        const ROOT_ENV: &str = "BSL_TEST_DIAGNOSTICS_BASELINE_SCOPE_ROOT";
        if let Some(root) = std::env::var_os(ROOT_ENV) {
            let root = std::path::PathBuf::from(root);
            let config = ProjectConfig::load_from_file(&root.join("config/project.toml")).unwrap();
            let resolved = Project::with_config(&root, config)
                .unwrap()
                .diagnostics_baseline()
                .unwrap()
                .unwrap();
            // The root goes in spelled as the caller wrote it — that is what this test is
            // about — while `path` comes back canonical, so only the expectation is
            // resolved through the filesystem.
            assert_eq!(resolved.path, fs::canonicalize(&root).unwrap().join("baseline.json"));
            assert_eq!(resolved.scope.source_root.as_deref(), Some("src/cf"));
            return;
        }

        let dir = tempdir().unwrap();
        let root = dir.path().join("project");
        let other = dir.path().join("other");
        write_configuration_xml(&root.join("src/cf"), "<xml/>");
        fs::create_dir_all(root.join("config")).unwrap();
        fs::create_dir(&other).unwrap();
        fs::write(
            root.join("config/project.toml"),
            "[source]\nroot = \"src/cf\"\n[diagnostics.baseline]\npath = \"../baseline.json\"\n",
        )
        .unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("tests::diagnostics_baseline_scope_is_independent_of_working_directory")
            .arg("--nocapture")
            .current_dir(other)
            .env(ROOT_ENV, &root)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn diagnostics_baseline_scope_rejects_external_roots_and_target() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        let outside = parent.path().join("outside");
        fs::create_dir(&root).unwrap();
        write_configuration_xml(&outside.join("cf"), "<xml/>");

        let external_source = ProjectConfig {
            diagnostics: baseline_config("baseline.json"),
            configuration_root: Some("../outside/cf".to_owned()),
            extensions: Some(vec![]),
            ..Default::default()
        };
        let error = Project::with_config(&root, external_source)
            .unwrap()
            .diagnostics_baseline()
            .unwrap_err();
        assert!(matches!(error, DiagnosticsBaselineProjectError::OutsideProject(_)));

        write_configuration_xml(&root.join("src/cf"), "<xml/>");
        let external_target = ProjectConfig {
            diagnostics: baseline_config(outside.join("baseline.json").to_str().unwrap()),
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![]),
            ..Default::default()
        };
        let error = Project::with_config(&root, external_target)
            .unwrap()
            .diagnostics_baseline()
            .unwrap_err();
        assert!(matches!(error, DiagnosticsBaselineProjectError::OutsideProject(_)));
    }

    #[test]
    fn diagnostics_baseline_scope_rejects_root_collision() {
        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        let config = ProjectConfig {
            diagnostics: baseline_config("baseline.json"),
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![structured("same", "src/cf", &[])]),
            ..Default::default()
        };
        let error =
            Project::with_config(dir.path(), config).unwrap().diagnostics_baseline().unwrap_err();
        assert!(matches!(error, DiagnosticsBaselineProjectError::PathCollision(_)));
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_baseline_scope_rejects_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        write_configuration_xml(&dir.path().join("src/cf"), "<xml/>");
        fs::write(dir.path().join("real.json"), "{}").unwrap();
        symlink("real.json", dir.path().join("baseline.json")).unwrap();
        let config = ProjectConfig {
            diagnostics: baseline_config("baseline.json"),
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![]),
            ..Default::default()
        };
        let error =
            Project::with_config(dir.path(), config).unwrap().diagnostics_baseline().unwrap_err();
        assert!(matches!(error, DiagnosticsBaselineProjectError::Symlink(_)));
    }

    fn baseline_config(path: &str) -> ProjectDiagnosticsConfig {
        ProjectDiagnosticsConfig {
            baseline: Some(DiagnosticsBaselineConfig {
                path: Some(path.to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn load_prefers_toml_over_json() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("bsl-analyzer.toml"), "[source]\nroot = \"from-toml\"\n")
            .unwrap();
        fs::write(dir.path().join(".bsl-analyzer.json"), r#"{"configurationRoot": "from-json"}"#)
            .unwrap();
        let config = ProjectConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(config.configuration_root.as_deref(), Some("from-toml"));
    }

    #[test]
    fn load_falls_back_to_json_when_no_toml() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".bsl-analyzer.json"), r#"{"configurationRoot": "from-json"}"#)
            .unwrap();
        let config = ProjectConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(config.configuration_root.as_deref(), Some("from-json"));
    }

    #[test]
    fn toml_present_but_invalid_blocks_json_fallback() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("bsl-analyzer.toml"), "invalid {{{{ toml").unwrap();
        fs::write(dir.path().join(".bsl-analyzer.json"), r#"{"configurationRoot": "from-json"}"#)
            .unwrap();
        let config = ProjectConfig::load(dir.path());
        assert!(
            config.is_err(),
            "an existing but broken TOML must be a load error, not a fallback"
        );
    }

    #[test]
    fn resolve_postgres_url_fails_when_helper_not_configured() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            dbname: Some("testdb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: SearchPostgresCredentialHelperConfig { program: None, args: vec![] },
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Reader);
        assert!(matches!(result, Err(ResolvePostgresUrlError::MissingCredentialHelper)));
    }

    #[test]
    fn resolve_postgres_url_fails_when_missing_vault_role_base() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            dbname: Some("testdb".to_owned()),
            port: None,
            schema: Some("public".to_owned()),
            vault_role_base: None,
            credential_helper: SearchPostgresCredentialHelperConfig {
                program: Some("echo".to_owned()),
                args: vec![],
            },
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Reader);
        assert!(matches!(result, Err(ResolvePostgresUrlError::MissingField(_))));
    }

    #[test]
    fn resolve_postgres_url_fails_when_missing_required_target_field() {
        let config = SearchPostgresConfig {
            host: None,
            dbname: Some("testdb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: SearchPostgresCredentialHelperConfig {
                program: Some("echo".to_owned()),
                args: vec![],
            },
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Writer);
        assert!(matches!(result, Err(ResolvePostgresUrlError::MissingField(_))));
    }

    #[test]
    fn resolve_postgres_url_spawn_failure_is_terminal() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            dbname: Some("testdb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: SearchPostgresCredentialHelperConfig {
                program: Some("nonexistent-program-xyz".to_owned()),
                args: vec![],
            },
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Reader);
        assert!(matches!(result, Err(ResolvePostgresUrlError::HelperSpawn { .. })));
    }

    #[test]
    fn resolve_postgres_url_invalid_helper_response_is_protocol_error() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            dbname: Some("testdb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: helper_program("just-a-plain-line", 0),
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Reader);
        assert!(matches!(result, Err(ResolvePostgresUrlError::HelperProtocol { .. })));
    }

    #[test]
    fn resolve_postgres_url_helper_returns_wrong_protocol() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            dbname: Some("testdb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: helper_program(
                r#"{"protocol":"wrong","ok":true,"url":"postgres://localhost:5432/testdb"}"#,
                0,
            ),
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Reader);
        assert!(matches!(result, Err(ResolvePostgresUrlError::HelperProtocol { .. })));
    }

    #[test]
    fn resolve_postgres_url_helper_returns_unsupported_scheme() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            dbname: Some("testdb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: helper_program(
                r#"{"protocol":"bsl-analyzer.postgres-helper.v1","ok":true,"url":"mysql://localhost:5432/testdb"}"#,
                0,
            ),
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Reader);
        assert!(matches!(result, Err(ResolvePostgresUrlError::UnsupportedUrlScheme(_))));
    }

    #[test]
    fn resolve_postgres_url_target_mismatch_host() {
        let config = SearchPostgresConfig {
            host: Some("expected-host".to_owned()),
            dbname: Some("testdb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: helper_program(
                r#"{"protocol":"bsl-analyzer.postgres-helper.v1","ok":true,"url":"postgres://different-host:5432/testdb","lease_id":"l1","renewable":false}"#,
                0,
            ),
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Reader);
        assert!(matches!(result, Err(ResolvePostgresUrlError::TargetMismatch { .. })));
    }

    #[test]
    fn resolve_postgres_url_target_mismatch_dbname() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            port: Some(5433),
            dbname: Some("expected-db".to_owned()),
            schema: Some("public".to_owned()),
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: helper_program(
                r#"{"protocol":"bsl-analyzer.postgres-helper.v1","ok":true,"url":"postgres://localhost:5433/other-db","lease_id":"l1","renewable":false}"#,
                0,
            ),
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Reader);
        assert!(matches!(result, Err(ResolvePostgresUrlError::TargetMismatch { .. })));
    }

    #[test]
    fn resolve_postgres_url_resolves_via_tiny_helper_program() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            port: Some(5433),
            dbname: Some("mydb".to_owned()),
            schema: Some("bsl_search".to_owned()),
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: helper_program(
                r#"{"protocol":"bsl-analyzer.postgres-helper.v1","ok":true,"url":"postgres://user:pass@localhost:5433/mydb","lease_id":"vault/lease/123","expires_at":"2026-06-01T00:00:00Z","renewable":false}"#,
                0,
            ),
        };
        let resolved = resolve_postgres_url(&config, PostgresAccessMode::Reader).unwrap();
        assert_eq!(resolved.url, "postgres://user:pass@localhost:5433/mydb");
        assert_eq!(resolved.lease_id.as_deref(), Some("vault/lease/123"));
        assert!(!resolved.renewable);
    }

    #[test]
    fn resolve_postgres_url_rejected_by_helper_is_terminal() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            dbname: Some("testdb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: helper_program(
                r#"{"protocol":"bsl-analyzer.postgres-helper.v1","ok":false,"error":{"code":"vault_access_denied","message":"role not allowed","retryable":false}}"#,
                1,
            ),
        };
        let result = resolve_postgres_url(&config, PostgresAccessMode::Writer);
        assert!(matches!(result, Err(ResolvePostgresUrlError::HelperRejected { .. })));
        if let Err(ResolvePostgresUrlError::HelperRejected { error, .. }) = result {
            assert_eq!(error.code, "vault_access_denied");
            assert!(!error.retryable);
        }
    }

    #[test]
    fn resolved_target_defaults_port_to_5432() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            dbname: Some("mydb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: None,
            credential_helper: SearchPostgresCredentialHelperConfig::default(),
        };
        let target = config.resolved_target().unwrap();
        assert_eq!(target.host, "localhost");
        assert_eq!(target.port, 5432);
        assert_eq!(target.dbname, "mydb");
        assert_eq!(target.schema, "public");
    }

    #[test]
    fn is_configured_returns_false_for_empty_postgres_config() {
        let config = SearchPostgresConfig::default();
        assert!(!config.is_configured());
    }

    #[test]
    fn is_configured_returns_true_when_helper_present() {
        let config = SearchPostgresConfig {
            host: Some("localhost".to_owned()),
            dbname: Some("mydb".to_owned()),
            schema: Some("public".to_owned()),
            port: None,
            vault_role_base: Some("search/base".to_owned()),
            credential_helper: SearchPostgresCredentialHelperConfig {
                program: Some("helper".to_owned()),
                args: vec![],
            },
        };
        assert!(config.is_configured());
    }

    #[test]
    fn features_config_defaults_type_narrowing_to_true() {
        let features = FeaturesConfig::default();
        assert!(
            features.type_narrowing,
            "narrowing must default to enabled so fresh projects inherit the feature without opt-in"
        );
    }

    #[test]
    fn project_config_defaults_features_to_enabled() {
        let config: ProjectConfig = serde_json::from_str("{}").unwrap();
        assert!(
            config.features.type_narrowing,
            "omitted `features` section must yield `FeaturesConfig::default`"
        );
    }

    #[test]
    fn project_config_deserializes_features_disabled_json() {
        let config: ProjectConfig = serde_json::from_str(
            r#"{
                "features": { "typeNarrowing": false }
            }"#,
        )
        .unwrap();
        assert!(
            !config.features.type_narrowing,
            "explicit `typeNarrowing = false` must propagate through JSON deserialization"
        );
    }

    #[test]
    fn project_config_load_from_toml_disables_narrowing() {
        let dir = tempdir().unwrap();
        let toml_path = dir.path().join("bsl-analyzer.toml");
        fs::write(
            &toml_path,
            r#"
[features]
type_narrowing = false
"#,
        )
        .unwrap();

        let config = ProjectConfig::load(dir.path())
            .expect("ProjectConfig::load must succeed for a well-formed TOML")
            .expect("the written TOML must be found");
        assert!(
            !config.features.type_narrowing,
            "`[features] type_narrowing = false` in TOML must round-trip to FeaturesConfig"
        );
    }

    #[test]
    fn workspace_diagnostics_defaults_off() {
        assert_eq!(FeaturesConfig::default().workspace_diagnostics, WorkspaceDiagnosticsScope::Off);
        let omitted: ProjectConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(
            omitted.features.workspace_diagnostics,
            WorkspaceDiagnosticsScope::Off,
            "an omitted feature must leave the pull provider disabled"
        );
    }

    #[test]
    fn workspace_diagnostics_parses_each_scope_json() {
        for (raw, expected) in [
            ("off", WorkspaceDiagnosticsScope::Off),
            ("extensions", WorkspaceDiagnosticsScope::Extensions),
            ("all", WorkspaceDiagnosticsScope::All),
        ] {
            let config: ProjectConfig = serde_json::from_str(&format!(
                r#"{{ "features": {{ "workspaceDiagnostics": "{raw}" }} }}"#
            ))
            .unwrap();
            assert_eq!(config.features.workspace_diagnostics, expected, "scope {raw}");
        }
    }

    #[test]
    fn workspace_diagnostics_toml_camel_and_snake_aliases() {
        for key in ["workspaceDiagnostics", "workspace_diagnostics"] {
            let toml_str = format!("[features]\n{key} = \"extensions\"\n");
            let config: super::TomlConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                config.features.workspace_diagnostics,
                WorkspaceDiagnosticsScope::Extensions,
                "both camelCase and snake_case keys must resolve ({key})"
            );
        }
    }

    #[test]
    fn workspace_diagnostics_invalid_value_is_rejected_not_silently_enabled() {
        // An unknown scope must NOT deserialize to a value that turns the feature on.
        let err = serde_json::from_str::<ProjectConfig>(
            r#"{ "features": { "workspaceDiagnostics": "everything" } }"#,
        );
        assert!(
            err.is_err(),
            "an invalid scope value must fail deserialization, never degrade to on"
        );
    }
}

/// External data processors and reports (EPF/ERF exports) as source roots of
/// their own kind: a leaf that sees the base and every extension, seen by none.
#[cfg(test)]
mod external_root_tests {
    use super::{
        DiagnosticsBaselinePartitionIdentity, ExtensionDecl, ExternalDecl, ExternalObjectKind,
        NodeKind, Project, ProjectConfig, ProjectError, StructuredExtensionDecl, TopologyError,
    };
    use std::path::Path;
    use tempfile::tempdir;

    fn structured(name: &str, path: &str, deps: &[&str]) -> ExtensionDecl {
        ExtensionDecl::Structured(StructuredExtensionDecl {
            name: name.to_string(),
            path: path.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
        })
    }

    fn external(name: &str, path: &str) -> ExternalDecl {
        ExternalDecl { name: name.to_string(), path: path.to_string(), depends_on: None }
    }

    fn touch_extension(root: &Path, rel: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Configuration.xml"),
            "<Properties><ConfigurationExtensionPurpose>Customization\
             </ConfigurationExtensionPurpose></Properties>",
        )
        .unwrap();
    }

    fn touch_base(root: &Path, rel: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Configuration.xml"), "<Configuration/>").unwrap();
    }

    /// The designer export of an external object: `<Name>.xml` beside a `<Name>/`
    /// directory, with `tag` as the object element under `MetaDataObject`.
    fn touch_external(root: &Path, rel: &str, name: &str, tag: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(dir.join(name).join("Forms")).unwrap();
        std::fs::write(
            dir.join(format!("{name}.xml")),
            format!(
                "\u{feff}<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\" version=\"2.20\">\n\
                 \t<!-- <ExternalReport uuid=\"in-a-comment\"/> -->\n\
                 \t<{tag} uuid=\"3696c164-ad14-4a0d-b659-10e3bf6d6ad2\">\n\
                 \t\t<Properties><Name>{name}</Name></Properties>\n\
                 \t</{tag}>\n\
                 </MetaDataObject>\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn toml_externals_parse_into_named_entries() {
        let toml_str = r#"
[source]
root = "src/cf"
externals = [
  { name = "АРМ", path = "src/epf/АРМПроизводство" },
]
"#;
        let config: super::TomlConfig = toml::from_str(toml_str).unwrap();
        let project = ProjectConfig::from(config);
        assert_eq!(project.externals, Some(vec![external("АРМ", "src/epf/АРМПроизводство")]));

        let json = r#"{ "externals": [ { "name": "АРМ", "path": "src/epf/АРМПроизводство" } ] }"#;
        let config: ProjectConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.externals, Some(vec![external("АРМ", "src/epf/АРМПроизводство")]));
    }

    #[test]
    fn toml_externals_reject_a_bare_path_and_an_unknown_field() {
        let bare = "[source]\nexternals = [\"src/epf/АРМ\"]\n";
        assert!(toml::from_str::<super::TomlConfig>(bare).is_err(), "a bare path has no name");
        let unknown = "[source]\nexternals = [{ name = \"A\", path = \"p\", glob = \"*\" }]\n";
        let err = toml::from_str::<super::TomlConfig>(unknown).unwrap_err().to_string();
        assert!(err.contains("glob"), "the unknown field is named: {err}");
    }

    #[test]
    fn an_external_root_is_a_node_of_its_own_kind_and_a_source_root() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        touch_extension(dir.path(), "src/cfe/Ext");
        touch_external(dir.path(), "src/epf/АРМ", "АРМПроизводство", "ExternalDataProcessor");
        let config = ProjectConfig {
            configuration_root: Some("src/cf".into()),
            extensions: Some(vec![structured("Ext", "src/cfe/Ext", &[])]),
            externals: Some(vec![external("АРМ", "src/epf/АРМ")]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();

        let epf = dir.path().join("src/epf/АРМ");
        assert_eq!(project.external_paths(), &[("АРМ".to_string(), epf.clone())]);
        assert_eq!(
            project.extension_paths().iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["Ext"],
            "an external is not an extension"
        );
        assert!(project.source_roots().contains(&epf), "the external root is scanned");
        let node = project.extension_topology().nodes().iter().find(|n| n.name() == "АРМ").unwrap();
        assert_eq!(node.kind(), NodeKind::External(ExternalObjectKind::DataProcessor));
    }

    #[test]
    fn source_exclusions_dominate_base_extension_and_external_roots_independently() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "base");
        touch_extension(dir.path(), "extensions/hidden");
        touch_extension(dir.path(), "extensions/allowed");
        touch_external(dir.path(), "external/hidden", "HiddenProcessor", "ExternalDataProcessor");

        let declarations = ProjectConfig {
            configuration_root: Some("base".into()),
            extensions: Some(vec![
                structured("HiddenExt", "extensions/hidden", &[]),
                structured("AllowedExt", "extensions/allowed", &[]),
            ]),
            externals: Some(vec![external("HiddenExternal", "external/hidden")]),
            ..Default::default()
        };
        let ordinary = Project::with_config(dir.path(), declarations.clone()).unwrap();
        assert_eq!(ordinary.source_roots().len(), 4, "fixture sanity: every root is declared");

        let scoped = Project::with_config(
            dir.path(),
            ProjectConfig {
                source_exclude: vec!["base".into(), "extensions/hidden".into(), "external".into()],
                ..declarations
            },
        )
        .unwrap();
        assert!(scoped.semantic_base_path().is_none(), "the excluded base stayed semantic");
        assert_eq!(scoped.source_roots(), [dir.path().join("extensions/allowed")]);
        assert_eq!(
            scoped.extension_topology().nodes().iter().map(|node| node.name()).collect::<Vec<_>>(),
            ["AllowedExt"],
            "an excluded extension or external survived in the topology"
        );
    }

    #[test]
    fn source_exclusions_prune_automatic_external_discovery() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        touch_external(dir.path(), "src/epf/Hidden", "HiddenProcessor", "ExternalDataProcessor");
        touch_external(dir.path(), "src/epf/Allowed", "AllowedProcessor", "ExternalDataProcessor");

        let project = Project::with_config(
            dir.path(),
            ProjectConfig {
                configuration_root: Some("src/cf".to_owned()),
                source_exclude: vec!["src/epf/Hidden".to_owned()],
                ..ProjectConfig::default()
            },
        )
        .unwrap();

        assert_eq!(
            project.external_paths(),
            &[("Allowed".to_owned(), dir.path().join("src/epf/Allowed"))]
        );
        assert!(project.source_roots().iter().all(|path| !path.ends_with("src/epf/Hidden")));
    }

    #[test]
    fn an_external_report_is_told_apart_from_a_processor() {
        let dir = tempdir().unwrap();
        touch_external(dir.path(), "src/erf/Отчет", "Отчет", "ExternalReport");
        let config = ProjectConfig {
            externals: Some(vec![external("Отчет", "src/erf/Отчет")]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        let node = project.extension_topology().nodes().first().unwrap();
        assert_eq!(node.kind(), NodeKind::External(ExternalObjectKind::Report));
    }

    #[test]
    fn an_external_root_carrying_configuration_xml_is_a_cfe_declared_under_the_wrong_key() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "src/epf/X");
        let config = ProjectConfig {
            externals: Some(vec![external("X", "src/epf/X")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::ExternalIsAConfiguration { .. })),
            "got: {err}"
        );
    }

    #[test]
    fn an_external_root_with_an_internal_object_is_rejected() {
        let dir = tempdir().unwrap();
        touch_external(dir.path(), "src/epf/X", "X", "DataProcessor");
        let config = ProjectConfig {
            externals: Some(vec![external("X", "src/epf/X")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(
                &err,
                ProjectError::Topology(TopologyError::ExternalNotAnExternalObject { tag, .. })
                    if tag == "DataProcessor"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn an_external_root_without_exactly_one_object_xml_is_rejected() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/epf/Empty")).unwrap();
        let config = ProjectConfig {
            externals: Some(vec![external("E", "src/epf/Empty")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::ExternalNoObjectXml { .. })),
            "got: {err}"
        );

        touch_external(dir.path(), "src/epf/Two", "A", "ExternalDataProcessor");
        touch_external(dir.path(), "src/epf/Two", "B", "ExternalDataProcessor");
        let config = ProjectConfig {
            externals: Some(vec![external("T", "src/epf/Two")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::ExternalAmbiguousObjectXml { .. })),
            "got: {err}"
        );

        let config = ProjectConfig {
            externals: Some(vec![external("M", "src/epf/Missing")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::ExternalPathMissing { .. })),
            "got: {err}"
        );

        // The XML alone is a copy that stopped halfway: the object's directory is
        // where its modules and forms live, and without it there is nothing to analyze.
        touch_external(dir.path(), "src/epf/Half", "H", "ExternalDataProcessor");
        std::fs::remove_dir_all(dir.path().join("src/epf/Half/H")).unwrap();
        let config = ProjectConfig {
            externals: Some(vec![external("H", "src/epf/Half")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(
                &err,
                ProjectError::Topology(TopologyError::ExternalObjectDirMissing { object, .. })
                    if object == "H"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn an_external_is_never_a_depends_on_target() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "src/cfe/X");
        touch_external(dir.path(), "src/epf/АРМ", "АРМ", "ExternalDataProcessor");
        let config = ProjectConfig {
            extensions: Some(vec![structured("X", "src/cfe/X", &["АРМ"])]),
            externals: Some(vec![external("АРМ", "src/epf/АРМ")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(
                &err,
                ProjectError::Topology(TopologyError::ExternalInDependency { from, name })
                    if from == "X" && name == "АРМ"
            ),
            "an edge onto an external must be refused, not resolved: {err}"
        );
    }

    #[test]
    fn an_external_sharing_an_extension_name_is_a_duplicate() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "src/cfe/Same");
        touch_external(dir.path(), "src/epf/Same", "Same", "ExternalDataProcessor");
        let config = ProjectConfig {
            extensions: Some(vec![structured("same", "src/cfe/Same", &[])]),
            externals: Some(vec![external("SAME", "src/epf/Same")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::DuplicateName { .. })),
            "names share one registry, case-folded: {err}"
        );
    }

    #[test]
    fn externals_come_after_every_extension_in_topological_order() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "src/cfe/A");
        touch_extension(dir.path(), "src/cfe/B");
        touch_external(dir.path(), "src/epf/E", "E", "ExternalDataProcessor");
        // Declared first so that declaration-order tie-breaking alone would put
        // it first; the kind must win over declaration order.
        let config = ProjectConfig {
            extensions: Some(vec![
                structured("A", "src/cfe/A", &["B"]),
                structured("B", "src/cfe/B", &[]),
            ]),
            externals: Some(vec![external("E", "src/epf/E")]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        let topology = project.extension_topology();
        let order: Vec<&str> =
            topology.topological_order().iter().map(|id| topology.node(*id).name()).collect();
        assert_eq!(order, ["B", "A", "E"]);
        let external = topology.nodes().iter().find(|n| n.name() == "E").unwrap();
        assert!(external.depends_on().is_empty(), "no key, no edge");
        let closure: Vec<&str> =
            external.closure().iter().map(|id| topology.node(*id).name()).collect();
        assert_eq!(closure, ["B", "A"], "without dependsOn it sees every extension, in order");
        assert!(external.sees_every_extension());
    }

    #[test]
    fn topology_fingerprint_changes_when_an_external_is_added() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "src/cfe/A");
        touch_external(dir.path(), "src/epf/E", "E", "ExternalDataProcessor");
        let fingerprint = |externals: Vec<ExternalDecl>| {
            let config = ProjectConfig {
                extensions: Some(vec![structured("A", "src/cfe/A", &[])]),
                externals: Some(externals),
                ..Default::default()
            };
            Project::with_config(dir.path(), config).unwrap().extension_topology().fingerprint()
        };
        assert_eq!(fingerprint(vec![]), fingerprint(vec![]));
        assert_ne!(fingerprint(vec![]), fingerprint(vec![external("E", "src/epf/E")]));
    }

    /// An external-only project has no base: the workspace root must not be
    /// promoted to an anonymous one, or the `main` partition rooted at "" would
    /// own every file, the external's own partition included.
    #[test]
    fn an_external_only_project_has_no_anonymous_base() {
        let dir = tempdir().unwrap();
        touch_external(dir.path(), "src/epf/E", "E", "ExternalDataProcessor");
        let config = ProjectConfig {
            externals: Some(vec![external("E", "src/epf/E")]),
            diagnostics: super::tests::partitioned_config("baselines", vec![]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        assert!(project.semantic_base_path().is_none());

        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        assert!(plan.project_scope.source_root.is_none());
        assert!(!plan.partitions.iter().any(|p| p.id == "main"), "{:?}", plan.partitions);
        assert_eq!(
            plan.owner_for_project_path("src/epf/E/E/Ext/ObjectModule.bsl"),
            Some("external:E")
        );

        // The control: with externals opted out the anonymous base stays. (Left
        // unset, discovery finds `src/epf/E` and the project is external-only.)
        let opted_out = ProjectConfig { externals: Some(vec![]), ..Default::default() };
        let bare = Project::with_config(dir.path(), opted_out).unwrap();
        assert_eq!(bare.semantic_base_path(), Some(dir.path()));
    }

    #[test]
    fn an_external_without_a_base_is_announced_and_with_one_is_not() {
        let dir = tempdir().unwrap();
        touch_external(dir.path(), "src/epf/E", "E", "ExternalDataProcessor");
        let config = ProjectConfig {
            externals: Some(vec![external("E", "src/epf/E")]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config.clone()).unwrap();
        let notice = project.standalone_external_notice().expect("no base: announced");
        assert!(notice.contains("E"), "names the external: {notice}");
        assert!(project.standalone_extension_notice().is_none(), "it is not an extension");

        touch_base(dir.path(), "src/cf");
        let project = Project::with_config(
            dir.path(),
            ProjectConfig { configuration_root: Some("src/cf".into()), ..config },
        )
        .unwrap();
        assert!(project.standalone_external_notice().is_none(), "with a base: silent");
    }

    /// An export kept inside the base's tree is refused by the partition plan
    /// like a nested extension is: `owner_for_project_path` takes the first
    /// root that contains a file, which is only right while no root nests
    /// another.
    #[test]
    fn an_external_nested_in_the_base_is_a_path_collision() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        touch_external(dir.path(), "src/cf/epf", "X", "ExternalDataProcessor");
        let config = ProjectConfig {
            configuration_root: Some("src/cf".into()),
            externals: Some(vec![external("X", "src/cf/epf")]),
            diagnostics: super::tests::partitioned_config("baselines", vec![]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        let err = project.diagnostics_baseline_partition_plan().unwrap_err();
        assert_eq!(err, super::DiagnosticsBaselineProjectError::PathCollision("src/cf/epf".into()));
    }

    /// An object XML whose `MetaDataObject` carries no element is a truncated
    /// export, not a missing one: the refusal must say the file was found.
    #[test]
    fn an_object_xml_without_an_element_is_refused_as_such() {
        let dir = tempdir().unwrap();
        let epf = dir.path().join("src/epf/E");
        std::fs::create_dir_all(epf.join("E")).unwrap();
        std::fs::write(
            epf.join("E.xml"),
            "<?xml version=\"1.0\"?><MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\">\
             </MetaDataObject>",
        )
        .unwrap();
        let config = ProjectConfig {
            externals: Some(vec![external("E", "src/epf/E")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(
                &err,
                ProjectError::Topology(TopologyError::ExternalObjectElementMissing { name, .. })
                    if name == "E"
            ),
            "got: {err}"
        );
    }

    /// Two exports of one object name are one identity to every surface keyed
    /// by `(MdoType, name)`: the graph and the MCP name dictionary would keep
    /// only the first. The project refuses the pair by name instead.
    #[test]
    fn two_externals_sharing_an_object_name_are_refused() {
        let dir = tempdir().unwrap();
        touch_external(dir.path(), "src/epf/A", "Foo", "ExternalDataProcessor");
        touch_external(dir.path(), "src/epf/B", "Foo", "ExternalDataProcessor");
        let config = ProjectConfig {
            externals: Some(vec![external("A", "src/epf/A"), external("B", "src/epf/B")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(
                &err,
                ProjectError::Topology(TopologyError::ExternalObjectNameDuplicate {
                    object,
                    first,
                    second
                }) if object == "Foo" && first == "A" && second == "B"
            ),
            "got: {err}"
        );

        // Object names are case-insensitive, like every BSL identifier.
        touch_external(dir.path(), "src/epf/D", "FOO", "ExternalDataProcessor");
        let config = ProjectConfig {
            externals: Some(vec![external("A", "src/epf/A"), external("D", "src/epf/D")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), config).unwrap_err();
        assert!(
            matches!(
                &err,
                ProjectError::Topology(TopologyError::ExternalObjectNameDuplicate {
                    first,
                    second,
                    ..
                }) if first == "A" && second == "D"
            ),
            "got: {err}"
        );

        // A report and a processor of one name are different objects.
        touch_external(dir.path(), "src/erf/C", "Foo", "ExternalReport");
        let config = ProjectConfig {
            externals: Some(vec![external("A", "src/epf/A"), external("C", "src/erf/C")]),
            ..Default::default()
        };
        Project::with_config(dir.path(), config).expect("kinds split the namespace");
    }

    #[test]
    fn an_external_lands_in_the_baseline_externals_not_in_extensions() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        touch_extension(dir.path(), "src/cfe/A");
        touch_external(dir.path(), "src/epf/E", "E", "ExternalDataProcessor");
        let config = ProjectConfig {
            configuration_root: Some("src/cf".into()),
            extensions: Some(vec![structured("A", "src/cfe/A", &[])]),
            externals: Some(vec![external("E", "src/epf/E")]),
            diagnostics: super::tests::partitioned_config("baselines", vec![]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();

        let scope = &plan.project_scope;
        assert_eq!(scope.extensions.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["A"]);
        assert_eq!(scope.externals.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["E"]);
        assert_eq!(scope.externals[0].path, "src/epf/E");

        let external = plan.partitions.iter().find(|p| p.id == "external:E").expect("partition");
        assert!(matches!(
            &external.identity,
            DiagnosticsBaselinePartitionIdentity::External { name, path, depends_on: None }
                if name == "E" && path == "src/epf/E"
        ));
        let identity_json = serde_json::to_string(&external.identity).unwrap();
        assert!(!identity_json.contains("depends_on"), "no key, no field: {identity_json}");
        let scope_json = serde_json::to_string(&plan.project_scope).unwrap();
        assert_eq!(
            scope_json,
            r#"{"source_root":"src/cf","extensions":[{"name":"A","path":"src/cfe/A","depends_on":[]}],"externals":[{"name":"E","path":"src/epf/E"}]}"#,
            "an external without dependsOn serializes as it did before the key existed"
        );
        assert_eq!(
            plan.owner_for_project_path("src/epf/E/E/Ext/ObjectModule.bsl"),
            Some("external:E")
        );

        // Serialization keeps the published fingerprints of projects without
        // externals: the empty field is absent, not `[]`.
        let without = super::DiagnosticsBaselineProjectScope {
            source_root: Some("src/cf".into()),
            extensions: vec![],
            externals: vec![],
        };
        let json = serde_json::to_string(&without).unwrap();
        assert!(!json.contains("externals"), "empty externals must not serialize: {json}");
        let back: super::DiagnosticsBaselineProjectScope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, without);
    }

    /// The tempdir of the discovery tests: a base, a processor and a report
    /// under their conventional containers, a report under the processors'
    /// container (its kind must come from its XML, not from where it lies) and a
    /// directory that is no export at all.
    fn discovery_workspace() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        touch_external(dir.path(), "src/epf/А", "А", "ExternalDataProcessor");
        touch_external(dir.path(), "src/epf/ОтчетПодEpf", "ОтчетПодEpf", "ExternalReport");
        std::fs::create_dir_all(dir.path().join("src/epf/НеВыгрузка")).unwrap();
        touch_external(dir.path(), "src/erf/Отчет", "Отчет", "ExternalReport");
        dir
    }

    fn discovered(project: &Project) -> Vec<(String, ExternalObjectKind)> {
        project
            .extension_topology()
            .nodes()
            .iter()
            .filter_map(|node| match node.kind() {
                NodeKind::External(kind) => Some((node.name().to_owned(), kind)),
                NodeKind::Extension => None,
            })
            .collect()
    }

    #[test]
    fn externals_are_discovered_under_src_epf_and_src_erf_when_unset() {
        let dir = discovery_workspace();
        let base =
            ProjectConfig { configuration_root: Some("src/cf".into()), ..Default::default() };
        assert_eq!(base.externals, None, "unset is the discovery default");
        let project = Project::with_config(dir.path(), base.clone()).unwrap();
        assert_eq!(
            discovered(&project),
            [
                ("А".to_owned(), ExternalObjectKind::DataProcessor),
                ("ОтчетПодEpf".to_owned(), ExternalObjectKind::Report),
                ("Отчет".to_owned(), ExternalObjectKind::Report),
            ],
            "both containers, in path order; the kind is the XML's, not the container's"
        );
        assert!(
            project.external_paths().iter().all(|(_, path)| path.starts_with(dir.path())),
            "{:?}",
            project.external_paths()
        );
        assert!(project.extension_topology().nodes().iter().all(|node| node.is_structured()));

        let opted_out = ProjectConfig { externals: Some(vec![]), ..base.clone() };
        assert!(Project::with_config(dir.path(), opted_out).unwrap().external_paths().is_empty());

        touch_external(dir.path(), "elsewhere/Явный", "Явный", "ExternalDataProcessor");
        let explicit = ProjectConfig {
            externals: Some(vec![external("Явный", "elsewhere/Явный")]),
            ..base
        };
        let names: Vec<String> = discovered(&Project::with_config(dir.path(), explicit).unwrap())
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["Явный"], "an explicit list switches discovery off");
    }

    #[test]
    fn discovery_falls_back_to_bare_containers_per_family() {
        let dir = tempdir().unwrap();
        touch_external(dir.path(), "epf/П", "П", "ExternalDataProcessor");
        touch_external(dir.path(), "src/erf/О", "О", "ExternalReport");
        let project = Project::with_config(dir.path(), ProjectConfig::default()).unwrap();
        let names: Vec<String> = discovered(&project).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["П", "О"]);
    }

    /// A collision across families keeps the first by path, not the first
    /// family: with `src/erf` absent the report family falls back to `erf/`,
    /// which sorts before `src/epf/`.
    #[test]
    fn a_cross_family_collision_keeps_the_first_by_path() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        touch_external(dir.path(), "src/epf/X", "X", "ExternalDataProcessor");
        touch_external(dir.path(), "erf/X", "X", "ExternalReport");
        let config =
            ProjectConfig { configuration_root: Some("src/cf".into()), ..Default::default() };
        let project = Project::with_config(dir.path(), config).unwrap();
        assert_eq!(discovered(&project), [("X".to_owned(), ExternalObjectKind::Report)]);
        assert_eq!(project.external_paths()[0].1, dir.path().join("erf/X"));
    }

    #[test]
    fn a_project_without_containers_discovers_nothing() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        let project = Project::with_config(dir.path(), ProjectConfig::default()).unwrap();
        assert!(project.external_paths().is_empty());
        assert!(project.diagnostics_baseline().unwrap().is_none(), "no baseline configured");
    }

    /// Whatever lies under a container must not stop a project that loaded
    /// before the container was looked at: every refusal of a declared external
    /// is a warning and a skip for a discovered one.
    #[test]
    fn discovery_skips_what_a_declaration_would_refuse() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        touch_extension(dir.path(), "src/epf/Расш");
        let broken = dir.path().join("src/epf/Битый");
        std::fs::create_dir_all(broken.join("Битый")).unwrap();
        std::fs::write(
            broken.join("Битый.xml"),
            "<MetaDataObject><DataProcessor uuid=\"1\"/></MetaDataObject>",
        )
        .unwrap();
        let config =
            ProjectConfig { configuration_root: Some("src/cf".into()), ..Default::default() };
        let project = Project::with_config(dir.path(), config.clone()).unwrap();
        assert!(project.external_paths().is_empty(), "{:?}", project.external_paths());

        // The controls: declared, the same directories are refused by name.
        for (path, expect) in [
            ("src/epf/Расш", "ExternalIsAConfiguration"),
            ("src/epf/Битый", "ExternalNotAnExternalObject"),
        ] {
            let declared =
                ProjectConfig { externals: Some(vec![external("X", path)]), ..config.clone() };
            let err = Project::with_config(dir.path(), declared).unwrap_err();
            assert!(format!("{err:?}").contains(expect), "{path}: {err}");
        }
    }

    #[test]
    fn discovery_skips_name_and_object_collisions_instead_of_failing() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        touch_extension(dir.path(), "src/cfe/АРМ");
        touch_external(dir.path(), "src/epf/арм", "АРМПроизводство", "ExternalDataProcessor");
        touch_external(dir.path(), "src/epf/Копия", "X", "ExternalDataProcessor");
        touch_external(dir.path(), "src/epf/X", "X", "ExternalDataProcessor");
        touch_external(dir.path(), "src/erf/x", "X", "ExternalReport");
        touch_external(dir.path(), "src/epf/Свой", "Свой", "ExternalDataProcessor");
        let names_with = |extensions: Option<Vec<ExtensionDecl>>| {
            let config = ProjectConfig {
                configuration_root: Some("src/cf".into()),
                extensions,
                ..Default::default()
            };
            let project = Project::with_config(dir.path(), config).unwrap();
            discovered(&project).into_iter().map(|(name, _)| name).collect::<Vec<_>>()
        };
        // In path order `X` comes first and stays; `Копия` exports the object
        // `X` already exports, `src/erf/x` collides with the discovered name
        // `X`, and `арм` with the extension's name (case-folded): each is skipped.
        assert_eq!(names_with(Some(vec![structured("АРМ", "src/cfe/АРМ", &[])])), ["X", "Свой"]);
        // A legacy extension of that name too: the extension keeps its slot.
        assert_eq!(names_with(Some(vec!["src/cfe/АРМ".into()])), ["X", "Свой"]);

        // The control: declared, the collision with the structured name is the
        // duplicate it always was.
        let declared = ProjectConfig {
            configuration_root: Some("src/cf".into()),
            extensions: Some(vec![structured("АРМ", "src/cfe/АРМ", &[])]),
            externals: Some(vec![external("арм", "src/epf/арм")]),
            ..Default::default()
        };
        let err = Project::with_config(dir.path(), declared).unwrap_err();
        assert!(
            matches!(err, ProjectError::Topology(TopologyError::DuplicateName { .. })),
            "{err}"
        );
    }

    #[test]
    fn a_directory_appearing_under_the_container_changes_the_fingerprint() {
        let dir = discovery_workspace();
        let config =
            ProjectConfig { configuration_root: Some("src/cf".into()), ..Default::default() };
        let before = Project::with_config(dir.path(), config.clone()).unwrap();
        let before = before.extension_topology().fingerprint();
        touch_external(dir.path(), "src/epf/Б", "Б", "ExternalDataProcessor");
        let after = Project::with_config(dir.path(), config).unwrap();
        assert_ne!(before, after.extension_topology().fingerprint());
    }

    #[test]
    fn a_discovered_external_owns_a_baseline_partition() {
        let dir = discovery_workspace();
        touch_extension(dir.path(), "src/cfe/A");
        let config = ProjectConfig {
            configuration_root: Some("src/cf".into()),
            extensions: Some(vec![structured("A", "src/cfe/A", &[])]),
            diagnostics: super::tests::partitioned_config("baselines", vec![]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config.clone()).unwrap();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let external = plan.partitions.iter().find(|p| p.id == "external:А").expect("partition");
        assert!(matches!(
            &external.identity,
            DiagnosticsBaselinePartitionIdentity::External { depends_on: None, .. }
        ));
        assert_eq!(
            plan.owner_for_project_path("src/epf/А/А/Ext/ObjectModule.bsl"),
            Some("external:А")
        );

        // The control: a legacy extension is still what the gate refuses.
        touch_extension(dir.path(), "src/cfe/Legacy");
        let legacy = ProjectConfig {
            extensions: Some(vec![structured("A", "src/cfe/A", &[]), "src/cfe/Legacy".into()]),
            ..config
        };
        let err = Project::with_config(dir.path(), legacy)
            .unwrap()
            .diagnostics_baseline_partition_plan()
            .unwrap_err();
        assert!(
            matches!(err, super::DiagnosticsBaselineProjectError::LegacyExtension(ref name) if name == "Legacy"),
            "{err:?}"
        );
    }

    #[test]
    fn toml_externals_accept_depends_on_in_both_spellings() {
        let toml_str = "[source]\nexternals = [\n  { name = \"A\", path = \"a\", dependsOn = [\"X\"] },\n  { name = \"B\", path = \"b\", depends_on = [] },\n  { name = \"C\", path = \"c\" },\n]\n";
        let config: super::TomlConfig = toml::from_str(toml_str).unwrap();
        let project = ProjectConfig::from(config);
        let externals = project.externals.unwrap();
        assert_eq!(externals[0].depends_on, Some(vec!["X".to_owned()]));
        assert_eq!(externals[1].depends_on, Some(vec![]));
        assert_eq!(externals[2].depends_on, None);
        let empty = "[source]\nexternals = []\n";
        let config: super::TomlConfig = toml::from_str(empty).unwrap();
        assert_eq!(
            ProjectConfig::from(config).externals,
            Some(vec![]),
            "an empty list is an opt-out"
        );
    }

    #[test]
    fn an_external_depends_on_narrows_its_view_and_reaches_the_baseline() {
        let dir = tempdir().unwrap();
        touch_base(dir.path(), "src/cf");
        touch_extension(dir.path(), "src/cfe/A");
        touch_extension(dir.path(), "src/cfe/B");
        touch_extension(dir.path(), "src/cfe/C");
        touch_external(dir.path(), "src/epf/E", "E", "ExternalDataProcessor");
        touch_external(dir.path(), "src/epf/F", "F", "ExternalDataProcessor");
        let mut narrowed = external("E", "src/epf/E");
        narrowed.depends_on = Some(vec!["a".into()]);
        let mut base_only = external("F", "src/epf/F");
        base_only.depends_on = Some(vec![]);
        let config = ProjectConfig {
            configuration_root: Some("src/cf".into()),
            extensions: Some(vec![
                structured("A", "src/cfe/A", &["C"]),
                structured("B", "src/cfe/B", &[]),
                structured("C", "src/cfe/C", &[]),
            ]),
            externals: Some(vec![narrowed, base_only]),
            diagnostics: super::tests::partitioned_config("baselines", vec![]),
            ..Default::default()
        };
        let project = Project::with_config(dir.path(), config).unwrap();
        let topology = project.extension_topology();
        let closure_of = |name: &str| -> Vec<&str> {
            let node = topology.nodes().iter().find(|n| n.name() == name).unwrap();
            node.closure().iter().map(|id| topology.node(*id).name()).collect()
        };
        assert_eq!(closure_of("E"), ["C", "A"], "transitive, B left out");
        assert!(closure_of("F").is_empty(), "an empty list is the base alone");
        let order: Vec<&str> =
            topology.topological_order().iter().map(|id| topology.node(*id).name()).collect();
        assert_eq!(order, ["B", "C", "A", "E", "F"]);

        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let scope = &plan.project_scope;
        assert_eq!(scope.externals[0].depends_on, Some(vec!["A".to_owned()]), "the declared name");
        assert_eq!(scope.externals[1].depends_on, Some(vec![]));
        let json = serde_json::to_string(&scope.externals[1]).unwrap();
        assert_eq!(json, r#"{"name":"F","path":"src/epf/F","depends_on":[]}"#);
        let partition = plan.partitions.iter().find(|p| p.id == "external:E").unwrap();
        assert!(matches!(
            &partition.identity,
            DiagnosticsBaselinePartitionIdentity::External { depends_on: Some(deps), .. } if deps == &["A".to_owned()]
        ));
    }

    #[test]
    fn an_external_depends_on_is_refused_like_an_extensions_when_wrong() {
        let dir = tempdir().unwrap();
        touch_extension(dir.path(), "src/cfe/A");
        touch_external(dir.path(), "src/epf/E", "E", "ExternalDataProcessor");
        touch_external(dir.path(), "src/epf/F", "F", "ExternalDataProcessor");
        let with = |deps: &[&str]| {
            let mut decl = external("E", "src/epf/E");
            decl.depends_on = Some(deps.iter().map(|s| s.to_string()).collect());
            let config = ProjectConfig {
                extensions: Some(vec![structured("A", "src/cfe/A", &[])]),
                externals: Some(vec![decl, external("F", "src/epf/F")]),
                ..Default::default()
            };
            Project::with_config(dir.path(), config).unwrap_err()
        };
        assert!(matches!(
            with(&["F"]),
            ProjectError::Topology(TopologyError::ExternalInDependency { .. })
        ));
        assert!(matches!(
            with(&["e"]),
            ProjectError::Topology(TopologyError::SelfReference { .. })
        ));
        assert!(matches!(
            with(&["nope"]),
            ProjectError::Topology(TopologyError::UnknownDependency { .. })
        ));
    }
}
