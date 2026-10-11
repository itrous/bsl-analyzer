//! The single owner of per-workspace derived cache paths.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

pub(crate) const LEASE_LOCK_FILE: &str = "writer.lease.lock";
pub(crate) const STALL_REPORT_FILE: &str = "bsl-graph-stall-report.txt";
pub const WORKSPACE_CACHE_SCOPE_ENV: &str = "BSL_MCP_CACHE_SCOPE";
pub const WORKSPACE_CACHE_BASE_ENV: &str = "BSL_CACHE_DIR";
const SCOPE_DOMAIN: &[u8] = b"bsl-analyzer/workspace-derived-cache/v1";

pub fn expected_scope_from_env() -> std::io::Result<Option<String>> {
    std::env::var_os(WORKSPACE_CACHE_SCOPE_ENV)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| invalid("internal workspace cache scope stamp is not valid Unicode"))
        })
        .transpose()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOrigin {
    Default,
    Explicit,
}

impl CacheOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Explicit => "explicit",
        }
    }
}

/// Resolved locations of every cache derived from one validated workspace Project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCacheLayout {
    root: PathBuf,
    declared: PathBuf,
    workspace: Option<PathBuf>,
    family: PathBuf,
    family_declared: PathBuf,
    base: PathBuf,
    base_declared: PathBuf,
    scope: [u8; 32],
    origin: CacheOrigin,
}

impl WorkspaceCacheLayout {
    /// Resolve a namespace using the process cwd only if the selected base is relative.
    pub fn for_project_in_current_dir(
        project: &project_model::Project,
        cli_base: Option<&Path>,
        expected_scope: Option<&str>,
    ) -> std::io::Result<Self> {
        Self::for_project_with_cwd(project, cli_base, std::env::current_dir, expected_scope)
    }

    /// Resolve a lazy namespace from the validated project, applying CLI > env > OS defaults.
    pub fn for_project(
        project: &project_model::Project,
        cli_base: Option<&Path>,
        current_dir: &Path,
        expected_scope: Option<&str>,
    ) -> std::io::Result<Self> {
        Self::for_project_with_cwd(
            project,
            cli_base,
            || Ok(current_dir.to_path_buf()),
            expected_scope,
        )
    }

    fn for_project_with_cwd(
        project: &project_model::Project,
        cli_base: Option<&Path>,
        current_dir: impl FnOnce() -> std::io::Result<PathBuf>,
        expected_scope: Option<&str>,
    ) -> std::io::Result<Self> {
        if let Some(stamp) = expected_scope {
            validate_scope_stamp(stamp)?;
        }
        let inherited_origin =
            expected_scope.and_then(|stamp| stamp.split_once(':').map(|(origin, _)| origin));
        let (selected, origin) = select_base(
            cli_base.map(Path::to_path_buf),
            std::env::var_os(WORKSPACE_CACHE_BASE_ENV),
            inherited_origin,
        )?;
        let base_declared = match selected {
            Some(path) if path.is_absolute() => path,
            Some(path) => current_dir()?.join(path),
            None => dirs::cache_dir()
                .ok_or_else(|| {
                    invalid("OS cache directory is unavailable; set --cache-dir or BSL_CACHE_DIR")
                })?
                .join("bsl-analyzer"),
        };
        let base = canonicalize_nearest(&base_declared)?;
        let scope = project_scope(project)?;
        let stamp = format!("{}:{}", origin.as_str(), hex(&scope));
        if let Some(expected) = expected_scope {
            validate_scope_stamp(expected)?;
            if expected != stamp {
                return Err(invalid(format!(
                    "workspace cache scope changed between launcher and daemon (expected {expected}, resolved {stamp})"
                )));
            }
        }

        let family_declared = base_declared.join("workspaces/v1");
        let family = base.join("workspaces/v1");
        let root = family.join(hex(&scope));
        let layout = Self {
            root: root.clone(),
            declared: family_declared.join(hex(&scope)),
            workspace: Some(project.root.clone()),
            family,
            family_declared,
            base,
            base_declared,
            scope,
            origin,
        };
        if origin == CacheOrigin::Explicit {
            validate_base_ancestor(&layout.base_declared, &layout.base)?;
        }
        layout.validate_source_placement(project)?;
        Ok(layout)
    }

    /// Test fixture layout. Production callers must resolve a validated Project.
    #[cfg(test)]
    pub fn for_workspace(workspace_root: &Path) -> Self {
        let root = workspace_root.join(".build");
        Self {
            declared: root.clone(),
            family: root.clone(),
            family_declared: root.clone(),
            base: root.clone(),
            base_declared: root.clone(),
            root,
            workspace: Some(workspace_root.to_path_buf()),
            scope: [0; 32],
            origin: CacheOrigin::Default,
        }
    }

    /// Test fixture layout. Production callers must resolve a validated Project.
    #[cfg(test)]
    pub fn from_root(root: PathBuf) -> Self {
        Self {
            declared: root.clone(),
            family: root.clone(),
            family_declared: root.clone(),
            base: root.clone(),
            base_declared: root.clone(),
            root,
            workspace: None,
            scope: [0; 32],
            origin: CacheOrigin::Default,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_workspace(mut self, workspace: PathBuf) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn declared_base(&self) -> &Path {
        &self.base_declared
    }

    pub fn origin(&self) -> CacheOrigin {
        self.origin
    }

    pub fn scope_stamp(&self) -> String {
        format!("{}:{}", self.origin.as_str(), hex(&self.scope))
    }

    pub fn verify_project(&self, project: &project_model::Project) -> std::io::Result<()> {
        #[cfg(test)]
        if self.scope == [0; 32] {
            return Ok(());
        }
        if project_scope(project)? != self.scope {
            return Err(invalid("workspace cache scope no longer matches the validated Project"));
        }
        self.validate_source_placement(project)
    }

    pub fn overlapping_source_root(
        project: &project_model::Project,
        output: &Path,
    ) -> std::io::Result<Option<PathBuf>> {
        let output = canonicalize_nearest(output)?;
        let mut roots = project.source_roots();
        roots.push(project.root.clone());
        for root in roots {
            let root = canonicalize_nearest(&root)?;
            if output.starts_with(&root) || root.starts_with(&output) {
                return Ok(Some(root));
            }
        }
        Ok(None)
    }

    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }

    pub fn spellings(&self) -> [&Path; 2] {
        [self.declared.as_path(), self.root.as_path()]
    }

    /// Exact owned namespace family plus the existing service and legacy soft exclusions.
    pub fn exclusions(&self, workspace_root: &Path) -> Vec<PathBuf> {
        let mut exclusions = self.placement_exclusions(workspace_root);
        let legacy = workspace_root.join(".build");
        exclusions.push(legacy.clone());
        if let Ok(canonical) = std::fs::canonicalize(&legacy) {
            exclusions.push(canonical);
        }
        for service in service_directories(workspace_root) {
            if !exclusions.contains(&service) {
                exclusions.push(service);
            }
        }
        exclusions
    }

    /// The output-family and service paths used for pre-write placement validation.
    pub fn placement_exclusions(&self, workspace_root: &Path) -> Vec<PathBuf> {
        let mut exclusions = vec![self.family_declared.clone(), self.family.clone()];
        for service in service_directories(workspace_root) {
            if !exclusions.contains(&service) {
                exclusions.push(service);
            }
        }
        exclusions
    }

    pub fn is_service_directory(workspace_root: &Path, path: &Path) -> bool {
        service_directories(workspace_root).any(|service| service == path)
    }

    /// Create only the app-owned namespace with private permissions and verify its resolved path.
    pub fn ensure(&self) -> std::io::Result<()> {
        #[cfg(test)]
        if self.scope == [0; 32] {
            return std::fs::create_dir_all(&self.root);
        }
        // The resolver may have observed a missing suffix. Re-resolve it before
        // the first write so a newly inserted symlink cannot redirect mkdirs
        // into an undeclared tree; repeat after creation to cover replacement
        // during the operation itself.
        validate_base_ancestor(&self.base_declared, &self.base)?;
        std::fs::create_dir_all(&self.base)?;
        validate_base_ancestor(&self.base_declared, &self.base)?;
        ensure_private_dir(&self.base.join("workspaces"))?;
        ensure_private_dir(&self.base.join("workspaces/v1"))?;
        ensure_private_dir(&self.root)?;
        if self.root.canonicalize()? != self.root {
            return Err(invalid("workspace cache path changed during startup"));
        }
        Ok(())
    }

    pub fn graph_db_path(&self) -> PathBuf {
        self.root.join("bsl-graph.db")
    }
    pub fn search_db_path(&self) -> PathBuf {
        self.root.join("bsl-search.db")
    }
    pub fn lease_path(&self) -> PathBuf {
        self.root.join("writer.lease")
    }
    pub fn lease_lock_path(&self) -> PathBuf {
        self.root.join(LEASE_LOCK_FILE)
    }
    pub(crate) fn graph_candidate_path(&self) -> PathBuf {
        self.root.join("bsl-graph.pending.db")
    }
    pub(crate) fn graph_candidate_lock_path(&self) -> PathBuf {
        self.root.join("bsl-graph.replacement.lock")
    }
    pub(crate) fn graph_access_lock_path(&self) -> PathBuf {
        self.root.join("bsl-graph.access.lock")
    }
    pub fn stall_report_path(&self) -> PathBuf {
        self.root.join(STALL_REPORT_FILE)
    }
    pub fn daemon_log_path(&self) -> PathBuf {
        self.root.join("bsl-analyzer-daemon.log")
    }

    fn validate_source_placement(&self, project: &project_model::Project) -> std::io::Result<()> {
        let family = canonicalize_nearest(&self.family)?;
        let workspace = canonicalize_nearest(&project.root)?;
        if family.starts_with(&workspace) && self.origin == CacheOrigin::Default {
            return Err(invalid(format!(
                "default workspace cache {} is inside workspace {}; set --cache-dir or BSL_CACHE_DIR",
                family.display(), workspace.display()
            )));
        }
        if workspace.starts_with(&family) {
            return Err(invalid(format!(
                "workspace cache namespace {} contains workspace root {}; choose another base",
                family.display(),
                workspace.display()
            )));
        }
        let roots = project.source_roots();
        for root in roots {
            let root = canonicalize_nearest(&root)?;
            if family.starts_with(&root) && self.origin == CacheOrigin::Default {
                return Err(invalid(format!(
                    "default workspace cache {} is inside source root {}; set --cache-dir or BSL_CACHE_DIR",
                    family.display(), root.display()
                )));
            }
            if root.starts_with(&family) {
                return Err(invalid(format!(
                    "workspace cache namespace {} contains source root {}; choose another base",
                    family.display(),
                    root.display()
                )));
            }
        }
        Ok(())
    }
}

pub fn validate_scope_stamp(stamp: &str) -> std::io::Result<()> {
    let Some((origin, digest)) = stamp.split_once(':') else {
        return Err(invalid("invalid internal workspace cache scope stamp"));
    };
    if !matches!(origin, "default" | "explicit")
        || digest.len() != 64
        || !digest.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(invalid("invalid internal workspace cache scope stamp"));
    }
    Ok(())
}

fn project_scope(project: &project_model::Project) -> std::io::Result<[u8; 32]> {
    let workspace = project.root.canonicalize().map_err(|error| {
        std::io::Error::new(error.kind(), format!("failed to canonicalize workspace: {error}"))
    })?;
    let mut exclusions = Vec::new();
    for path in project.source_exclusions().declared() {
        let normalized = lexical_normalize(path);
        let spelling = normalized.strip_prefix(&workspace).unwrap_or(&normalized);
        exclusions.push(spelling.as_os_str().as_encoded_bytes().to_vec());
    }
    exclusions.sort();
    exclusions.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(SCOPE_DOMAIN);
    append_field(&mut hasher, workspace.as_os_str().as_encoded_bytes());
    append_field(&mut hasher, project.extension_topology().fingerprint().as_bytes());
    for exclusion in exclusions {
        append_field(&mut hasher, &exclusion);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn append_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn select_base(
    cli: Option<PathBuf>,
    environment: Option<OsString>,
    inherited_origin: Option<&str>,
) -> std::io::Result<(Option<PathBuf>, CacheOrigin)> {
    let explicit_origin = if inherited_origin == Some("default") {
        CacheOrigin::Default
    } else {
        CacheOrigin::Explicit
    };
    let selected = cli.or_else(|| environment.map(PathBuf::from));
    if selected.as_ref().is_some_and(|path| path.as_os_str().is_empty()) {
        return Err(invalid("selected cache base is empty"));
    }
    let origin = if selected.is_some() { explicit_origin } else { CacheOrigin::Default };
    Ok((selected, origin))
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn canonicalize_nearest(path: &Path) -> std::io::Result<PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let component = ancestor
            .components()
            .next_back()
            .filter(|component| matches!(component, Component::Normal(_) | Component::ParentDir))
            .ok_or_else(|| invalid("cache path has no existing ancestor"))?;
        suffix.push(component.as_os_str().to_os_string());
        ancestor =
            ancestor.parent().ok_or_else(|| invalid("cache path has no existing ancestor"))?;
    }
    // Resolve existing symlinks before folding parents in a not-yet-created suffix.
    let mut resolved = ancestor.canonicalize()?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(lexical_normalize(&resolved))
}

fn validate_base_ancestor(declared: &Path, resolved: &Path) -> std::io::Result<()> {
    if canonicalize_nearest(declared)? != resolved {
        return Err(invalid("cache base ancestry changed before startup"));
    }
    let mut ancestor = declared;
    while !ancestor.exists() {
        ancestor =
            ancestor.parent().ok_or_else(|| invalid("cache path has no existing ancestor"))?;
    }
    if !std::fs::metadata(ancestor)?.is_dir() {
        return Err(invalid("cache base has a non-directory ancestor"));
    }
    Ok(())
}

fn service_directories(workspace_root: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    [".git", "target", "node_modules"].into_iter().map(|name| workspace_root.join(name))
}

fn invalid(message: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.to_string())
}

#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {
            if meta.uid() != unsafe { libc::geteuid() }
                || meta.permissions().mode() & 0o777 != 0o700
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "cache directory {} must be owned by this user with mode 0700",
                        path.display()
                    ),
                ));
            }
            Ok(())
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("cache path {} is not a directory", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure_private_dir(path)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    crate::cache_windows::private_directory(path)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("cache path {} is not a directory", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(path),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub fn workspace_cache_dir(workspace_root: &Path) -> PathBuf {
    WorkspaceCacheLayout::for_workspace(workspace_root).root
}
#[cfg(test)]
pub fn ensure_workspace_cache_dir(workspace_root: &Path) -> std::io::Result<PathBuf> {
    let layout = WorkspaceCacheLayout::for_workspace(workspace_root);
    layout.ensure()?;
    Ok(layout.root)
}
#[cfg(test)]
pub fn graph_db_path(workspace_root: &Path) -> PathBuf {
    WorkspaceCacheLayout::for_workspace(workspace_root).graph_db_path()
}
#[cfg(test)]
pub fn search_db_path(workspace_root: &Path) -> PathBuf {
    WorkspaceCacheLayout::for_workspace(workspace_root).search_db_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(root: &Path) -> project_model::Project {
        crate::project::at(root).unwrap()
    }

    #[test]
    fn workspace_cache_absolute_base_does_not_require_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        let cache_parent = tempfile::tempdir().unwrap();
        let project = project(workspace.path());
        let base = cache_parent.path().join("cache");
        let missing_cwd = || Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        let layout =
            WorkspaceCacheLayout::for_project_with_cwd(&project, Some(&base), missing_cwd, None)
                .unwrap();
        layout.ensure().unwrap();
        assert_eq!(layout.base(), base.canonicalize().unwrap());
        let error = WorkspaceCacheLayout::for_project_with_cwd(
            &project,
            Some(Path::new("relative-cache")),
            missing_cwd,
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_cache_resolves_symlink_before_parent_components() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let cache_parent = tempfile::tempdir().unwrap();
        let target = cache_parent.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = workspace.path().join("link");
        symlink(&target, &link).unwrap();
        let project = project(workspace.path());
        for suffix in ["link/../cache", "link/missing/../../cache"] {
            let declared = workspace.path().join(suffix);
            let layout = WorkspaceCacheLayout::for_project(
                &project,
                Some(&declared),
                workspace.path(),
                None,
            )
            .unwrap();
            assert_eq!(layout.base(), cache_parent.path().canonicalize().unwrap().join("cache"));
            layout.ensure().unwrap();
            assert!(layout.root().is_dir());
            assert!(!workspace.path().join("cache").exists());
            assert!(!target.join("missing").exists(), "resolution must remain lazy");
        }
    }

    #[test]
    fn workspace_cache_scope_layout_is_lazy_private_and_namespaces_topology() {
        let workspace = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let project = project(workspace.path());
        let layout =
            WorkspaceCacheLayout::for_project(&project, Some(Path::new("cache")), cwd.path(), None)
                .unwrap();
        assert!(layout
            .root()
            .starts_with(cwd.path().canonicalize().unwrap().join("cache/workspaces/v1")));
        assert!(!layout.root().exists());
        assert_eq!(layout.scope_stamp().split(':').nth(1).unwrap().len(), 64);
        layout.ensure().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(layout.root()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn workspace_cache_scope_ignores_bodies_but_hashes_declared_exclusions() {
        let workspace = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let source = workspace.path().join("module.bsl");
        std::fs::write(&source, "Function A() EndFunction").unwrap();
        let first = project(workspace.path());
        let a =
            WorkspaceCacheLayout::for_project(&first, Some(Path::new("cache")), cwd.path(), None)
                .unwrap();
        std::fs::write(&source, "Function A() Return 1; EndFunction").unwrap();
        let body_changed = project(workspace.path());
        let b = WorkspaceCacheLayout::for_project(
            &body_changed,
            Some(Path::new("cache")),
            cwd.path(),
            None,
        )
        .unwrap();
        assert_eq!(a.root(), b.root(), "source bodies do not identify a cache namespace");

        std::fs::write(
            workspace.path().join("bsl-analyzer.toml"),
            "[source]\nexclude = [\"ignored\"]\n",
        )
        .unwrap();
        let exclusions_changed = project(workspace.path());
        let c = WorkspaceCacheLayout::for_project(
            &exclusions_changed,
            Some(Path::new("cache")),
            cwd.path(),
            None,
        )
        .unwrap();
        assert_ne!(a.root(), c.root(), "declared source exclusions identify the scan topology");

        std::fs::create_dir_all(workspace.path().join("ignored")).unwrap();
        let exclusion_target_created = project(workspace.path());
        let d = WorkspaceCacheLayout::for_project(
            &exclusion_target_created,
            Some(Path::new("cache")),
            cwd.path(),
            None,
        )
        .unwrap();
        assert_eq!(
            c.root(),
            d.root(),
            "creating an excluded directory is not a declaration change"
        );
    }

    #[test]
    fn workspace_cache_scope_normalizes_toml_and_cli_source_declarations() {
        let workspace = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("src/cf")).unwrap();
        std::fs::create_dir_all(workspace.path().join("src/cfe/A")).unwrap();
        std::fs::write(workspace.path().join("src/cf/Configuration.xml"), "<Configuration/>")
            .unwrap();
        std::fs::write(workspace.path().join("src/cfe/A/Configuration.xml"), "<Configuration/>")
            .unwrap();
        let config_path = workspace.path().join("bsl-analyzer.toml");
        let write_config = |exclusions: &str| {
            std::fs::write(
                &config_path,
                format!(
                    "[source]\nroot = \"src/cf\"\nextensions = [\"src/cfe/A\"]\nexclude = [{exclusions}]\n"
                ),
            )
            .unwrap();
            project(workspace.path())
        };

        let declared = write_config("\"ignored\", \"./ignored\", \"ignored\"");
        let normalized = write_config("\"./ignored\", \"ignored\"");
        let base = PathBuf::from("cache");
        let first =
            WorkspaceCacheLayout::for_project(&declared, Some(&base), cwd.path(), None).unwrap();
        let second =
            WorkspaceCacheLayout::for_project(&normalized, Some(&base), cwd.path(), None).unwrap();
        assert_eq!(first.scope_stamp(), second.scope_stamp());
        assert_eq!(first.root(), second.root(), "ordering and duplicates are normalized");

        let mut cli_config = project_model::ProjectConfig::load_from_file(&config_path).unwrap();
        project_model::SourceSetOverride {
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![project_model::ExtensionDecl::from("src/cfe/A")]),
            externals: None,
        }
        .apply_to(&mut cli_config);
        let cli_project =
            project_model::Project::with_config(workspace.path(), cli_config).unwrap();
        let cli =
            WorkspaceCacheLayout::for_project(&cli_project, Some(&base), cwd.path(), None).unwrap();
        assert_eq!(first.scope_stamp(), cli.scope_stamp());
        assert_eq!(first.root(), cli.root(), "equivalent CLI and TOML source sets share a leaf");
    }

    #[test]
    fn workspace_cache_scope_leaf_tracks_individual_topology_dimensions() {
        let workspace = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        for path in ["src/cf", "src/cfe/A", "src/cfe/B"] {
            std::fs::create_dir_all(workspace.path().join(path)).unwrap();
            std::fs::write(
                workspace.path().join(path).join("Configuration.xml"),
                "<Configuration/>",
            )
            .unwrap();
        }
        let base = PathBuf::from("cache");
        let layout = |extensions: Vec<project_model::ExtensionDecl>| {
            let config = project_model::ProjectConfig {
                configuration_root: Some("src/cf".to_owned()),
                extensions: Some(extensions),
                ..Default::default()
            };
            let project = project_model::Project::with_config(workspace.path(), config).unwrap();
            WorkspaceCacheLayout::for_project(&project, Some(&base), cwd.path(), None).unwrap()
        };
        let independent = |name: &str, path: &str| {
            project_model::ExtensionDecl::Structured(project_model::StructuredExtensionDecl {
                name: name.to_owned(),
                path: path.to_owned(),
                depends_on: Vec::new(),
            })
        };
        let first = layout(vec![independent("A", "src/cfe/A"), independent("B", "src/cfe/B")]);
        let reordered = layout(vec![independent("B", "src/cfe/B"), independent("A", "src/cfe/A")]);
        assert_ne!(first.root(), reordered.root(), "extension order changes cache identity");

        let dependency = layout(vec![
            independent("A", "src/cfe/A"),
            project_model::ExtensionDecl::Structured(project_model::StructuredExtensionDecl {
                name: "B".to_owned(),
                path: "src/cfe/B".to_owned(),
                depends_on: vec!["A".to_owned()],
            }),
        ]);
        assert_ne!(first.root(), dependency.root(), "dependency visibility changes cache identity");
    }

    #[test]
    fn workspace_cache_scope_leaf_tracks_workspace_and_external_scope() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        for root in [first_root.path(), second_root.path()] {
            std::fs::create_dir_all(root.join("src/cf")).unwrap();
            std::fs::write(root.join("src/cf/Configuration.xml"), "<Configuration/>").unwrap();
        }
        let base = PathBuf::from("cache");
        let layout = |root: &Path, externals: Option<Vec<project_model::ExternalDecl>>| {
            let config = project_model::ProjectConfig {
                configuration_root: Some("src/cf".to_owned()),
                externals,
                ..Default::default()
            };
            let project = project_model::Project::with_config(root, config).unwrap();
            WorkspaceCacheLayout::for_project(&project, Some(&base), cwd.path(), None).unwrap()
        };
        let first = layout(first_root.path(), Some(Vec::new()));
        let other_workspace = layout(second_root.path(), Some(Vec::new()));
        assert_ne!(first.root(), other_workspace.root(), "workspace identity changes the leaf");

        let external = first_root.path().join("src/epf/Processor");
        std::fs::create_dir_all(external.join("Processor/Forms")).unwrap();
        std::fs::write(
            external.join("Processor.xml"),
            "<MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\" version=\"2.20\"><ExternalDataProcessor uuid=\"3696c164-ad14-4a0d-b659-10e3bf6d6ad2\"><Properties><Name>Processor</Name></Properties></ExternalDataProcessor></MetaDataObject>",
        )
        .unwrap();
        let with_external = layout(
            first_root.path(),
            Some(vec![project_model::ExternalDecl {
                name: "Processor".to_owned(),
                path: "src/epf/Processor".to_owned(),
                depends_on: None,
            }]),
        );
        assert_ne!(first.root(), with_external.root(), "external scope changes the leaf");
    }

    #[cfg(unix)]
    #[test]
    fn workspace_cache_scope_normalizes_workspace_aliases() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let alias_parent = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let alias = alias_parent.path().join("workspace-link");
        symlink(workspace.path(), &alias).unwrap();
        let first = project(workspace.path());
        let through_alias = project(&alias);
        let a =
            WorkspaceCacheLayout::for_project(&first, Some(Path::new("cache")), cwd.path(), None)
                .unwrap();
        let b = WorkspaceCacheLayout::for_project(
            &through_alias,
            Some(Path::new("cache")),
            cwd.path(),
            None,
        )
        .unwrap();
        assert_eq!(a.scope_stamp(), b.scope_stamp());
        assert_eq!(a.root(), b.root());
    }

    #[test]
    fn workspace_cache_scope_stamp_rejects_malformed_and_drifted_child_scope() {
        assert!(validate_scope_stamp(
            "explicit:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        )
        .is_ok());
        assert!(validate_scope_stamp("default:AAAA").is_err());
        let workspace = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let project = project(workspace.path());
        let err = WorkspaceCacheLayout::for_project(
            &project,
            Some(Path::new("cache")),
            cwd.path(),
            Some("default:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("scope changed"));
    }

    #[test]
    fn workspace_cache_base_selection_prefers_cli_and_rejects_selected_empty_value() {
        let (selected, origin) =
            select_base(Some(PathBuf::from("cli")), Some(OsString::from("environment")), None)
                .unwrap();
        assert_eq!(selected.as_deref(), Some(Path::new("cli")));
        assert_eq!(origin, CacheOrigin::Explicit);

        let (selected, origin) =
            select_base(None, Some(OsString::from("environment")), None).unwrap();
        assert_eq!(selected.as_deref(), Some(Path::new("environment")));
        assert_eq!(origin, CacheOrigin::Explicit);
        assert!(select_base(Some(PathBuf::new()), Some(OsString::from("fallback")), None).is_err());
        assert_eq!(select_base(None, None, Some("default")).unwrap().1, CacheOrigin::Default);
    }

    #[test]
    fn workspace_cache_admission_rejects_a_namespace_that_contains_the_project() {
        let parent = tempfile::tempdir().unwrap();
        let base = parent.path().join("cache-base");
        let workspace = base.join("workspaces/v1/project");
        std::fs::create_dir_all(&workspace).unwrap();
        let project = project(&workspace);
        let err = WorkspaceCacheLayout::for_project(&project, Some(&base), parent.path(), None)
            .expect_err("cache family must not contain a source root");
        assert!(err.to_string().contains("contains workspace root"));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_cache_scope_ensure_rejects_symlink_drift_before_creating_target() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let cache_parent = tempfile::tempdir().unwrap();
        let project = project(workspace.path());
        let missing = cache_parent.path().join("missing");
        let base = missing.join("cache");
        let layout =
            WorkspaceCacheLayout::for_project(&project, Some(&base), cache_parent.path(), None)
                .unwrap();
        assert!(!missing.exists());

        symlink(workspace.path(), &missing).unwrap();
        let error = layout.ensure().expect_err("a changed cache ancestry is refused");
        assert!(error.to_string().contains("ancestry changed"));
        assert!(!workspace.path().join("cache").exists());
    }

    #[test]
    fn workspace_cache_scope_resolver_rejects_file_ancestor_without_creation() {
        let workspace = tempfile::tempdir().unwrap();
        let cache_parent = tempfile::tempdir().unwrap();
        let project = project(workspace.path());
        let file = cache_parent.path().join("cache-file");
        std::fs::write(&file, b"preserve").unwrap();

        let error =
            WorkspaceCacheLayout::for_project(&project, Some(&file), cache_parent.path(), None)
                .expect_err("a file cannot own a cache namespace");
        assert!(error.to_string().contains("non-directory ancestor"));
        assert_eq!(std::fs::read(&file).unwrap(), b"preserve");
        assert!(!cache_parent.path().join("cache-file/workspaces").exists());
    }

    #[test]
    fn workspace_cache_scope_default_type_collision_degrades_but_explicit_fails() {
        let _env_lock = crate::state::test_support::env_lock();
        let workspace = tempfile::tempdir().unwrap();
        let cache_parent = tempfile::tempdir().unwrap();
        let project = project(workspace.path());
        let base_file = cache_parent.path().join("cache-file");
        std::fs::write(&base_file, b"preserve").unwrap();

        let explicit_error = WorkspaceCacheLayout::for_project(
            &project,
            Some(&base_file),
            cache_parent.path(),
            None,
        )
        .expect_err("an explicit non-directory base fails during resolution");
        assert!(explicit_error.to_string().contains("non-directory ancestor"));
        assert_eq!(std::fs::read(&base_file).unwrap(), b"preserve");

        let scope = format!("default:{}", hex(&project_scope(&project).unwrap()));
        let _base = crate::state::test_support::EnvVarGuard::set(
            WORKSPACE_CACHE_BASE_ENV,
            base_file.to_str().unwrap(),
        );
        let default =
            WorkspaceCacheLayout::for_project(&project, None, cache_parent.path(), Some(&scope))
                .expect("default resolution stays lazy despite an unusable cache ancestor");
        assert_eq!(default.origin(), CacheOrigin::Default);
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&default);
        assert!(lease.coordination_failed(), "failed default cache creation is degraded");
        assert!(!lease.owns_caches(), "degraded defaults never become unmanaged writers");
        assert!(crate::workspace_lease::WorkspaceLease::unmanaged().owns_caches());
        assert_eq!(std::fs::read(&base_file).unwrap(), b"preserve");
        assert!(!cache_parent.path().join("cache-file/workspaces").exists());
    }

    #[test]
    fn workspace_cache_automatic_output_overlap_is_detected_before_creation() {
        let workspace = tempfile::tempdir().unwrap();
        let project = project(workspace.path());
        let output = workspace.path().join("runtime/process-record.jsonl");
        assert_eq!(
            WorkspaceCacheLayout::overlapping_source_root(&project, &output).unwrap(),
            Some(workspace.path().canonicalize().unwrap())
        );
        assert!(!output.exists());
    }

    #[test]
    fn workspace_cache_default_base_inside_nested_workspace_is_rejected_before_creation() {
        let workspace = tempfile::tempdir().unwrap();
        let configuration = workspace.path().join("src/cf");
        std::fs::create_dir_all(&configuration).unwrap();
        std::fs::write(configuration.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(workspace.path().join("bsl-analyzer.toml"), "[source]\nroot = \"src/cf\"\n")
            .unwrap();
        let project = project(workspace.path());
        let scope = format!("default:{}", hex(&project_scope(&project).unwrap()));
        // Pass the resolved default as a launcher does, rather than redirecting unrelated
        // background cache writers through a process-wide XDG_CACHE_HOME change.
        let default_base = workspace.path().join("os-cache");
        let error = WorkspaceCacheLayout::for_project(
            &project,
            Some(&default_base),
            workspace.path(),
            Some(&scope),
        )
        .expect_err("forwarded default base inside a nested workspace must be refused");
        assert!(error.to_string().contains("inside workspace"));
        assert!(!workspace.path().join("os-cache").exists(), "rejection precedes mkdir");

        let explicit = workspace.path().join("explicit-cache");
        let accepted =
            WorkspaceCacheLayout::for_project(&project, Some(&explicit), workspace.path(), None)
                .expect("an explicit cache inside the workspace remains supported");
        assert!(accepted.root().starts_with(workspace.path().join("explicit-cache")));
    }
}
