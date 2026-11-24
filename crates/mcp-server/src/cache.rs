//! The single owner of per-workspace derived cache paths.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::workspace_lease::{inspect_lease_dir, LeaseSnapshot};

const DAEMON_LOG_FILE: &str = "bsl-analyzer-daemon.log";
/// How long the trail of a rotated daemon log (see [`rotated_log_path`]) is kept. A younger one
/// is what one reads to diagnose a crash that just happened, and may belong to a daemon that is
/// still starting up; past this age no live run can still be reading the superseded copy.
pub const ROTATED_LOG_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// Size above which a log is rotated to its `.prev` trail at startup. Shared by the daemon's cache log
/// and an explicit `BSL_LOG_FILE`, so the two bounds cannot drift apart.
pub const LOG_ROTATE_BYTES: u64 = 50 * 1024 * 1024;
/// Bound on the work of one sweep: the family can hold thousands of directories.
const SWEEP_ENTRY_LIMIT: usize = 10_000;
/// Prefix of a family entry a sweep has claimed for removal. Cache directories are named by a
/// hex digest, so no live one starts with a dot.
const SWEPT_DIR_PREFIX: &str = ".swept-";

/// Lease record file name, next to the caches it governs. The lease writes it through
/// `lease_path` and the sweep reads it in sibling directories, so both take the name from here.
pub(crate) const LEASE_FILE: &str = "writer.lease";
pub(crate) const LEASE_LOCK_FILE: &str = "writer.lease.lock";
pub(crate) const STALL_REPORT_FILE: &str = "bsl-graph-stall-report.txt";
/// Records which workspace a cache directory was derived for; see `write_workspace_marker`.
pub(crate) const WORKSPACE_MARKER_FILE: &str = "workspace.path";
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
            None => default_base(&std::env::temp_dir(), &project.root)?,
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
        roots.extend(project.semantic_dependency_roots());
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
        if self.origin == CacheOrigin::Default && is_temp_family(&self.base) {
            // The temp dir is shared: a base another user created there could let them swap
            // the private namespace underneath it, so only a private base of our own will do.
            if let Some(parent) = self.base.parent() {
                std::fs::create_dir_all(parent)?;
            }
            ensure_private_dir(&self.base)?;
        } else {
            std::fs::create_dir_all(&self.base)?;
        }
        validate_base_ancestor(&self.base_declared, &self.base)?;
        ensure_private_dir(&self.base.join("workspaces"))?;
        ensure_private_dir(&self.base.join("workspaces/v1"))?;
        ensure_private_dir(&self.root)?;
        if self.root.canonicalize()? != self.root {
            return Err(invalid("workspace cache path changed during startup"));
        }
        self.write_workspace_marker();
        self.sweep_abandoned_siblings();
        Ok(())
    }

    /// Record the workspace this directory was derived for, so it stays identifiable without a
    /// lease.
    ///
    /// Identity used to come only from `writer.lease`, which only a process that becomes the
    /// cache writer ever writes. Measured on one machine: of 1080 cache directories (16 GB) just
    /// 5 held a lease, and none of those named a missing tree, so the sweep could prove almost
    /// nothing abandoned. Short CLI runs, tests and degraded runs create the directory here and
    /// never write a lease, which made them unidentifiable rather than abandoned.
    ///
    /// Best effort: a read-only or full disk must not stop a startup. The path is written as
    /// the OS string's encoded bytes, so nothing is lost on a non-UTF-8 name; the reader treats
    /// anything it cannot decode as no evidence at all.
    fn write_workspace_marker(&self) {
        let Some(workspace) = self.workspace.as_deref() else { return };
        let marker = self.root.join(WORKSPACE_MARKER_FILE);
        let content = workspace.as_os_str().as_encoded_bytes();
        if std::fs::read(&marker).is_ok_and(|existing| existing == content) {
            return;
        }
        // Replaced by a rename, never rewritten in place: a sweep in another process reads the
        // marker without a lock, and a torn read would name a shorter path that does not exist.
        let staged = self.root.join(format!("{WORKSPACE_MARKER_FILE}.{}.tmp", std::process::id()));
        if std::fs::write(&staged, content).is_err() || std::fs::rename(&staged, &marker).is_err() {
            let _ = std::fs::remove_file(&staged);
        }
    }

    /// `ensure` can run more than once per process and a scan of thousands of entries is not
    /// free, so only the first call sweeps.
    ///
    /// The sweep runs off the startup path: it checks trees that belong to other caches, and a
    /// `stat` of a tree on a hung network mount can block indefinitely.
    fn sweep_abandoned_siblings(&self) {
        static SWEPT: AtomicBool = AtomicBool::new(false);
        if self.origin == CacheOrigin::Default && !SWEPT.swap(true, Ordering::Relaxed) {
            let layout = self.clone();
            let _ = std::thread::Builder::new()
                .name("cache-sweep".into())
                .spawn(move || layout.sweep_family());
        }
    }

    /// Remove sibling cache directories whose owner is provably gone.
    ///
    /// Nothing evicted these before, and every temporary workspace (test fixtures above all)
    /// leaves one behind. Only the default base is swept: an explicit `--cache-dir` may be
    /// shared or user-owned, and we do not delete inside it.
    ///
    /// Never returns an error and ignores every filesystem failure: a sibling may belong to a
    /// live foreign daemon (a removal can fail, or race with its writes), and a failed sweep
    /// must not stop a startup.
    ///
    /// Liveness is the lease heartbeat, never the recorded pid: the pid is reused by unrelated
    /// processes (measured: every "live" pid in 400 lease files belonged to node, cmd or
    /// python). A stale heartbeat alone is not enough either - an idle cache of a tree that
    /// still exists is wanted, and two digests of one existing tree prove nothing about which
    /// is current - so a directory goes only when the tree it served is gone too.
    fn sweep_family(&self) {
        if self.origin != CacheOrigin::Default {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.family) else { return };
        for entry in entries.take(SWEEP_ENTRY_LIMIT).flatten() {
            let path = entry.path();
            // `file_type` does not follow symlinks, so a link is never descended into.
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if !is_dir || path == self.root {
                continue;
            }
            let name = entry.file_name();
            if name.as_encoded_bytes().starts_with(SWEPT_DIR_PREFIX.as_bytes()) {
                // An earlier sweep claimed it and was stopped mid-removal.
                let _ = std::fs::remove_dir_all(&path);
            } else if is_abandoned(&path) {
                // Claimed by a rename first: a process can exit in the middle of a removal, and a
                // half-removed directory may no longer hold what named its tree, so it would stay
                // forever. A claimed name is finished by whichever sweep sees it next.
                let mut claimed = OsString::from(SWEPT_DIR_PREFIX);
                claimed.push(&name);
                let claimed = self.family.join(claimed);
                if std::fs::rename(&path, &claimed).is_ok() {
                    let _ = std::fs::remove_dir_all(&claimed);
                }
            }
        }
    }

    pub fn graph_db_path(&self) -> PathBuf {
        self.root.join("bsl-graph.db")
    }
    pub fn search_db_path(&self) -> PathBuf {
        self.root.join("bsl-search.db")
    }
    pub fn lease_path(&self) -> PathBuf {
        self.root.join(LEASE_FILE)
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
        daemon_log_in(&self.root)
    }

    fn validate_source_placement(&self, project: &project_model::Project) -> std::io::Result<()> {
        let family = canonicalize_nearest(&self.family)?;
        let workspace = canonicalize_nearest(&project.root)?;
        if family.starts_with(&workspace) && self.origin == CacheOrigin::Default {
            return Err(invalid(format!(
                "default workspace cache {} is inside workspace {}; set --cache-dir or BSL_CACHE_DIR",
                family.display(),
                workspace.display()
            )));
        }
        if workspace.starts_with(&family) {
            return Err(invalid(format!(
                "workspace cache namespace {} contains workspace root {}; choose another base",
                family.display(),
                workspace.display()
            )));
        }
        let mut roots = project.source_roots();
        roots.extend(project.semantic_dependency_roots());
        for root in roots {
            let root = canonicalize_nearest(&root)?;
            if family.starts_with(&root) && self.origin == CacheOrigin::Default {
                return Err(invalid(format!(
                    "default workspace cache {} is inside source root {}; set --cache-dir or BSL_CACHE_DIR",
                    family.display(),
                    root.display()
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

/// Liveness comes from the lease (a fresh one always wins); identity comes from the lease's
/// workspace and, failing that, from the marker `ensure` leaves in every directory.
fn is_abandoned(dir: &Path) -> bool {
    match inspect_lease_dir(dir) {
        LeaseSnapshot::Fresh => false,
        LeaseSnapshot::Stale { workspace: Some(tree) } => tree_is_gone(&tree),
        // A record of an older program names no tree: fall back to the marker.
        LeaseSnapshot::Stale { workspace: None } => marked_tree_is_gone(dir),
        LeaseSnapshot::Unreadable => marked_tree_is_gone(dir) || is_lone_stale_log(dir),
    }
}

/// The tree recorded in `dir`'s marker is provably gone. No marker, an empty or undecodable
/// one, or a relative path proves nothing.
fn marked_tree_is_gone(dir: &Path) -> bool {
    let Ok(bytes) = std::fs::read(dir.join(WORKSPACE_MARKER_FILE)) else { return false };
    let Ok(text) = String::from_utf8(bytes) else { return false };
    !text.is_empty() && tree_is_gone(Path::new(&text))
}

/// The single proof of absence shared by the lease and marker paths, so they cannot drift.
/// An unanswerable existence check (e.g. access denied) is not evidence of absence. Neither is
/// an unreachable volume: on Windows an absent one (detached USB disk, unmapped network
/// drive, unattached VHD) answers `Ok(false)` for EVERY path on it, so without the volume
/// check a detached drive would delete the indexes of all its projects.
///
/// The volume guard protects only on Windows. On unix every absolute path's volume root is
/// `/`, which always exists, so a tree on an unmounted volume (`/mnt`, `/media`, `/Volumes`)
/// reads as gone, and its cache directory goes once its lease is stale or it has none. Only the
/// derived cache is lost: the next run on that tree rebuilds it.
fn tree_is_gone(tree: &Path) -> bool {
    volume_root(tree).is_some_and(|volume| matches!(volume.try_exists(), Ok(true)))
        && matches!(tree.try_exists(), Ok(false))
}

/// Root of the volume `path` lives on: `C:\x\y` -> `C:\`, `\\server\share\x` ->
/// `\\server\share\`; `None` for a path that names no volume (a relative path, or a rooted
/// one on Windows without a drive). On unix there is no prefix and the root is `/` whatever is
/// mounted where, so the volume answers nothing there.
fn volume_root(path: &Path) -> Option<PathBuf> {
    match path.components().next()? {
        Component::Prefix(prefix) => {
            let mut root = prefix.as_os_str().to_os_string();
            root.push(std::path::MAIN_SEPARATOR_STR);
            Some(PathBuf::from(root))
        }
        Component::RootDir if cfg!(unix) => Some(PathBuf::from("/")),
        _ => None,
    }
}

/// A rotated daemon log can outlive everything else in its directory (21 of 21 such
/// directories measured held only a 51 MB `.prev`). Anything else inside, even beside the log,
/// makes the directory unidentifiable, so it stays.
fn is_lone_stale_log(dir: &Path) -> bool {
    let Ok(mut entries) = std::fs::read_dir(dir) else { return false };
    let (Some(Ok(only)), None) = (entries.next(), entries.next()) else { return false };
    if rotated_log_path(&daemon_log_in(dir)).is_none_or(|trail| only.path() != trail) {
        return false;
    }
    only.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > ROTATED_LOG_RETENTION)
}

fn daemon_log_in(dir: &Path) -> PathBuf {
    dir.join(DAEMON_LOG_FILE)
}

/// The trail a log leaves when it is rotated: `<log>.prev`, appended to the whole file name
/// (`with_extension` would truncate a dotted name like `daemon.custom.log`). `None` for a path
/// that names no file: an empty name would put the trail beside whatever directory the path
/// ends in.
pub fn rotated_log_path(log: &Path) -> Option<PathBuf> {
    let mut name = log.file_name()?.to_os_string();
    name.push(".prev");
    Some(log.with_file_name(name))
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
    // Exclusions inside the workspace retain the root spelling they were loaded
    // through; make those relative before hashing the canonical workspace.
    let declared_workspace = lexical_normalize(&std::path::absolute(&project.root)?);
    let mut exclusions = Vec::new();
    for path in project.source_exclusions().declared() {
        let normalized = lexical_normalize(path);
        let spelling = normalized
            .strip_prefix(&declared_workspace)
            .or_else(|_| normalized.strip_prefix(&workspace))
            .unwrap_or(&normalized);
        exclusions.push(spelling.as_os_str().as_encoded_bytes().to_vec());
    }
    exclusions.sort();
    exclusions.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(SCOPE_DOMAIN);
    append_field(&mut hasher, workspace.as_os_str().as_encoded_bytes());
    append_field(&mut hasher, project.extension_topology().fingerprint().as_bytes());
    if project
        .external_configuration_owners()
        .iter()
        .any(|(_, owner)| owner.proven_root().is_some())
    {
        append_field(&mut hasher, project.compilation_fingerprint().as_bytes());
    }
    for exclusion in exclusions {
        append_field(&mut hasher, &exclusion);
    }
    // Patterns are folded in only when declared, so a project without them keeps the
    // stamp it had before patterns existed. The NUL in the tag keeps a pattern apart
    // from a literal exclusion: no file system path holds one.
    for pattern in project.source_exclusions().patterns() {
        append_field(&mut hasher, &[b"pattern\0".as_slice(), pattern.as_bytes()].concat());
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

/// Name of the throwaway cache family used for workspaces that are themselves throwaway.
///
/// On unix the temp dir is usually shared by every user, so the name carries the effective uid:
/// one user's family must neither block nor serve another's.
#[cfg(unix)]
fn temp_workspace_cache_dir() -> String {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    format!("bsl-analyzer-cache-{}", unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn temp_workspace_cache_dir() -> String {
    "bsl-analyzer-cache".to_owned()
}

fn is_temp_family(base: &Path) -> bool {
    base.file_name().is_some_and(|name| *name == *temp_workspace_cache_dir())
}

/// The temp family can be built on: absent, or a real directory that is private to this user.
/// Anything else (another user's directory, a symlink planted in the shared temp dir) means the
/// throwaway base is not ours, and the OS cache directory serves instead of failing startup.
/// This is decided once, when the layout is resolved: a family that turns unusable after that is
/// refused by `ensure`, which never builds under a base it cannot prove private.
#[cfg(unix)]
fn temp_family_is_usable(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => is_private_dir(&meta),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

#[cfg(windows)]
fn temp_family_is_usable(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => crate::cache_windows::verify_private_directory(path, &meta).is_ok(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Elsewhere `ensure_private_dir` checks no ownership either, so there is nothing to decline on.
#[cfg(not(any(unix, windows)))]
fn temp_family_is_usable(_path: &Path) -> bool {
    true
}

/// Pick the default cache base for a workspace nobody named a cache directory for.
///
/// A workspace under the OS temp dir dies with the run that made it, so its derived cache is
/// put under the temp dir too: in the user's real cache it would leave one entry per run that
/// only the sweep ever removes. Anything else keeps the OS cache directory.
fn default_base(temp_dir: &Path, workspace_root: &Path) -> std::io::Result<PathBuf> {
    let os_cache = || {
        dirs::cache_dir()
            .ok_or_else(|| {
                invalid("OS cache directory is unavailable; set --cache-dir or BSL_CACHE_DIR")
            })
            .map(|dir| dir.join("bsl-analyzer"))
    };
    let (Ok(temp), Ok(workspace)) =
        (canonicalize_nearest(temp_dir), canonicalize_nearest(workspace_root))
    else {
        return os_cache();
    };
    // Equality, not just containment: a workspace that IS the temp dir would hold its own
    // cache, which the placement checks rightly refuse for a default base.
    if !workspace.starts_with(&temp) || workspace == temp {
        return os_cache();
    }
    let candidate = temp.join(temp_workspace_cache_dir());
    // A workspace rooted inside the throwaway family itself would again contain its own cache.
    if workspace.starts_with(&candidate) || !temp_family_is_usable(&candidate) {
        return os_cache();
    }
    Ok(candidate)
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

/// The one privacy rule for a cache directory on unix: a real directory (`symlink_metadata`
/// never reports a link as one) owned by the effective user with mode 0700.
#[cfg(unix)]
fn is_private_dir(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    meta.file_type().is_dir()
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        && meta.uid() == unsafe { libc::geteuid() }
        && meta.permissions().mode() & 0o777 == 0o700
}

#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {
            if !is_private_dir(&meta) {
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

        std::fs::write(
            workspace.path().join("bsl-analyzer.toml"),
            "[source]\nexclude = [\"ignored\"]\nexclude_globs = [\"**/Generated.bsl\"]\n",
        )
        .unwrap();
        let patterns_added = project(workspace.path());
        let e = WorkspaceCacheLayout::for_project(
            &patterns_added,
            Some(Path::new("cache")),
            cwd.path(),
            None,
        )
        .unwrap();
        assert_ne!(c.root(), e.root(), "declared exclusion patterns identify the scan topology");

        let stamp = |config: &str| {
            std::fs::write(workspace.path().join("bsl-analyzer.toml"), config).unwrap();
            WorkspaceCacheLayout::for_project(
                &project(workspace.path()),
                Some(Path::new("cache")),
                cwd.path(),
                None,
            )
            .unwrap()
            .root()
            .to_path_buf()
        };
        assert_ne!(
            stamp("[source]\nexclude = [\"a\", \"pattern\", \"q\"]\n"),
            stamp("[source]\nexclude = [\"a\"]\nexclude_globs = [\"q\"]\n"),
            "a pattern is told apart from a literal exclusion spelled like its tag"
        );
    }

    #[test]
    fn workspace_cache_scope_normalizes_toml_and_api_source_declarations() {
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
                    "[source]\nroot = \"src/cf\"\nextensions = [{{ name = \"A\", path = \"src/cfe/A\", scope = \"InfoBase\" }}]\nexclude = [{exclusions}]\n"
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

        let mut api_config = project_model::ProjectConfig::load_from_file(&config_path).unwrap();
        project_model::SourceSetOverride {
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![project_model::ExtensionDecl::Structured(
                project_model::StructuredExtensionDecl {
                    name: "A".to_owned(),
                    path: "src/cfe/A".to_owned(),
                    depends_on: Vec::new(),
                    scope: Some(bsl_metadata::ExtensionScope::InfoBase),
                },
            )]),
            externals: None,
        }
        .apply_to(&mut api_config);
        let api_project =
            project_model::Project::with_config(workspace.path(), api_config).unwrap();
        let api =
            WorkspaceCacheLayout::for_project(&api_project, Some(&base), cwd.path(), None).unwrap();
        assert_eq!(first.scope_stamp(), api.scope_stamp());
        assert_eq!(first.root(), api.root(), "equivalent API and TOML source sets share a leaf");
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
                scope: Some(bsl_metadata::ExtensionScope::InfoBase),
            })
        };
        let legacy =
            project_model::ExtensionDecl::Structured(project_model::StructuredExtensionDecl {
                name: "A".to_owned(),
                path: "src/cfe/A".to_owned(),
                depends_on: Vec::new(),
                scope: None,
            });
        let scoped = |scope| {
            project_model::ExtensionDecl::Structured(project_model::StructuredExtensionDecl {
                name: "A".to_owned(),
                path: "src/cfe/A".to_owned(),
                depends_on: Vec::new(),
                scope: Some(scope),
            })
        };
        let first = layout(vec![independent("A", "src/cfe/A"), independent("B", "src/cfe/B")]);
        let reordered = layout(vec![independent("B", "src/cfe/B"), independent("A", "src/cfe/A")]);
        assert_ne!(first.root(), reordered.root(), "extension order changes cache identity");
        let different_scope = layout(vec![
            scoped(bsl_metadata::ExtensionScope::DataSeparation),
            independent("B", "src/cfe/B"),
        ]);
        assert_ne!(first.root(), different_scope.root(), "extension Scope changes cache identity");
        let legacy = layout(vec![legacy, independent("B", "src/cfe/B")]);
        assert_ne!(first.root(), legacy.root(), "legacy scope has its own cache identity");

        let dependency = layout(vec![
            independent("A", "src/cfe/A"),
            project_model::ExtensionDecl::Structured(project_model::StructuredExtensionDecl {
                name: "B".to_owned(),
                path: "src/cfe/B".to_owned(),
                depends_on: vec!["A".to_owned()],
                scope: Some(bsl_metadata::ExtensionScope::InfoBase),
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
                configuration_root: None,
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
    fn workspace_cache_scope_normalizes_exclusions_across_root_spellings() {
        let workspace = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let alias_parent = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\nexclude = [\"excluded\"]\n",
        )
        .unwrap();

        let declared_root = project(workspace.path());
        #[cfg(unix)]
        let alternate_root = {
            use std::os::unix::fs::symlink;

            let alias = alias_parent.path().join("workspace-link");
            symlink(workspace.path(), &alias).unwrap();
            alias
        };
        #[cfg(windows)]
        let alternate_root = workspace.path().canonicalize().unwrap();
        #[cfg(not(any(unix, windows)))]
        let alternate_root = workspace.path().to_path_buf();
        let alternate_project = project(&alternate_root);
        let declared_layout = WorkspaceCacheLayout::for_project(
            &declared_root,
            Some(cache.path()),
            workspace.path(),
            None,
        )
        .unwrap();
        let alternate_layout = WorkspaceCacheLayout::for_project(
            &alternate_project,
            Some(cache.path()),
            workspace.path(),
            None,
        )
        .unwrap();

        assert_eq!(declared_layout.scope_stamp(), alternate_layout.scope_stamp());
        assert!(declared_layout.verify_project(&alternate_project).is_ok());

        #[cfg(unix)]
        {
            std::fs::write(
                workspace.path().join("bsl-analyzer.toml"),
                "[source]\nroot = \".\"\nexclude = [\"../outside-absent-directory\"]\n",
            )
            .unwrap();
            let declared_external_exclusion = project(workspace.path());
            let alternate_external_exclusion = project(&alternate_root);
            let declared_external_layout = WorkspaceCacheLayout::for_project(
                &declared_external_exclusion,
                Some(cache.path()),
                workspace.path(),
                None,
            )
            .unwrap();
            let alternate_external_layout = WorkspaceCacheLayout::for_project(
                &alternate_external_exclusion,
                Some(cache.path()),
                workspace.path(),
                None,
            )
            .unwrap();
            assert_ne!(
                declared_external_layout.scope_stamp(),
                alternate_external_layout.scope_stamp(),
                "outside exclusions retain their resolved absolute identity"
            );
        }

        std::fs::write(
            workspace.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\nexclude = [\"another-absent-directory\"]\n",
        )
        .unwrap();
        let changed_exclusion = project(workspace.path());
        let changed_layout = WorkspaceCacheLayout::for_project(
            &changed_exclusion,
            Some(cache.path()),
            workspace.path(),
            None,
        )
        .unwrap();
        assert_ne!(declared_layout.scope_stamp(), changed_layout.scope_stamp());
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
        assert!(accepted
            .root()
            .starts_with(workspace.path().canonicalize().unwrap().join("explicit-cache")));
    }

    #[test]
    fn workspace_cache_default_base_for_temp_workspace_stays_in_temp() {
        let workspace = tempfile::tempdir().unwrap();
        let project = project(workspace.path());
        let layout =
            WorkspaceCacheLayout::for_project(&project, None, workspace.path(), None).unwrap();
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert!(layout.base().starts_with(&temp), "{:?} not under {:?}", layout.base(), temp);
        if let Some(os_cache) = dirs::cache_dir() {
            assert!(!layout.base().starts_with(os_cache));
        }
        layout.ensure().unwrap();
        assert!(layout.root().is_dir());
    }

    #[test]
    fn workspace_cache_default_base_control_non_temp_workspace_uses_os_cache() {
        let claimed_temp = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let base = default_base(claimed_temp.path(), workspace.path()).unwrap();
        assert_eq!(base, dirs::cache_dir().unwrap().join("bsl-analyzer"));
    }

    #[test]
    fn workspace_cache_default_base_declines_when_workspace_is_the_temp_root() {
        let dir = tempfile::tempdir().unwrap();
        let base = default_base(dir.path(), dir.path()).unwrap();
        assert_eq!(base, dirs::cache_dir().unwrap().join("bsl-analyzer"));
    }

    #[test]
    fn workspace_cache_default_base_declines_a_workspace_inside_the_cache_family() {
        let temp = tempfile::tempdir().unwrap();
        let inside = temp.path().join(temp_workspace_cache_dir()).join("ws");
        std::fs::create_dir_all(&inside).unwrap();
        let base = default_base(temp.path(), &inside).unwrap();
        assert_eq!(base, dirs::cache_dir().unwrap().join("bsl-analyzer"));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_cache_default_base_declines_a_temp_family_that_is_not_private() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        let family = temp.path().join(temp_workspace_cache_dir());
        std::fs::create_dir(&family).unwrap();
        let os_cache = dirs::cache_dir().unwrap().join("bsl-analyzer");

        std::fs::set_permissions(&family, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(default_base(temp.path(), &workspace).unwrap(), os_cache);

        // Control: the same directory once private is used.
        std::fs::set_permissions(&family, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(default_base(temp.path(), &workspace).unwrap(), family.canonicalize().unwrap());

        std::fs::remove_dir(&family).unwrap();
        std::os::unix::fs::symlink(temp.path().join("elsewhere"), &family).unwrap();
        std::fs::create_dir(temp.path().join("elsewhere")).unwrap();
        assert_eq!(default_base(temp.path(), &workspace).unwrap(), os_cache);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_cache_temp_family_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let layout = WorkspaceCacheLayout::for_project(
            &project(workspace.path()),
            None,
            workspace.path(),
            None,
        )
        .unwrap();
        assert!(is_temp_family(layout.base()), "{:?}", layout.base());
        layout.ensure().unwrap();
        let mode = std::fs::metadata(layout.base()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    mod sweep {
        use super::*;
        use std::time::{SystemTime, UNIX_EPOCH};

        /// A family directory with the layout's own `current` root inside it.
        struct Family {
            dir: tempfile::TempDir,
            layout: WorkspaceCacheLayout,
        }

        fn new_family(origin: CacheOrigin) -> Family {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("current");
            std::fs::create_dir(&root).unwrap();
            let layout = WorkspaceCacheLayout {
                declared: root.clone(),
                family: dir.path().to_path_buf(),
                family_declared: dir.path().to_path_buf(),
                base: dir.path().to_path_buf(),
                base_declared: dir.path().to_path_buf(),
                root,
                workspace: None,
                scope: [1; 32],
                origin,
            };
            Family { dir, layout }
        }

        fn now() -> u64 {
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
        }

        /// A sibling directory with a lease stamped at `heartbeat_secs`, serving `tree`.
        fn sibling(
            family: &Family,
            name: &str,
            heartbeat_secs: u64,
            tree: Option<&Path>,
        ) -> PathBuf {
            let dir = family.dir.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            let mut record = serde_json::json!({
                "generation": 1, "token": 1, "pid": 1, "heartbeat_secs": heartbeat_secs,
            });
            if let Some(tree) = tree {
                record["workspace"] = tree.to_string_lossy().into_owned().into();
            }
            std::fs::write(dir.join(LEASE_FILE), record.to_string()).unwrap();
            dir
        }

        fn gone_tree(family: &Family) -> PathBuf {
            family.dir.path().join("no-such-tree")
        }

        fn lone_log(family: &Family, name: &str, age: Duration) -> PathBuf {
            let dir = family.dir.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            write_log(family, &dir, age);
            dir
        }

        /// The trail is named as the daemon would leave it, from the layout's log path, so the
        /// sweep is checked against the name that is actually written.
        fn write_log(family: &Family, dir: &Path, age: Duration) {
            let trail = rotated_log_path(&family.layout.daemon_log_path()).unwrap();
            let file = std::fs::File::create(dir.join(trail.file_name().unwrap())).unwrap();
            file.set_modified(SystemTime::now() - age).unwrap();
        }

        const DAY: Duration = Duration::from_secs(24 * 60 * 60);

        #[test]
        fn stale_lease_of_a_missing_tree_is_removed() {
            let family = new_family(CacheOrigin::Default);
            let dir = sibling(&family, "a", 1, Some(&gone_tree(&family)));
            family.layout.sweep_family();
            assert!(!dir.exists());
        }

        /// A sibling with NO lease, optionally carrying a marker with the given content.
        fn leaseless(family: &Family, name: &str, marker: Option<&str>) -> PathBuf {
            let dir = family.dir.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            if let Some(content) = marker {
                std::fs::write(dir.join(WORKSPACE_MARKER_FILE), content).unwrap();
            }
            dir
        }

        fn gone_str(family: &Family) -> String {
            gone_tree(family).to_string_lossy().into_owned()
        }

        #[test]
        fn ensure_writes_the_workspace_marker() {
            let workspace = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let project = project(workspace.path());
            let layout = WorkspaceCacheLayout::for_project(
                &project,
                Some(&cache.path().join("c")),
                workspace.path(),
                None,
            )
            .unwrap();
            layout.ensure().unwrap();
            let marker = layout.root().join(WORKSPACE_MARKER_FILE);
            let expected = layout.workspace().unwrap().as_os_str().as_encoded_bytes();
            assert_eq!(std::fs::read(&marker).unwrap(), expected);
            // A second startup leaves it intact.
            layout.ensure().unwrap();
            assert_eq!(std::fs::read(&marker).unwrap(), expected);
        }

        #[test]
        fn leaseless_sibling_whose_marker_names_a_missing_tree_is_removed() {
            let family = new_family(CacheOrigin::Default);
            let dir = leaseless(&family, "a", Some(&gone_str(&family)));
            family.layout.sweep_family();
            assert!(!dir.exists());
        }

        #[test]
        fn stale_lease_without_a_tree_defers_to_the_marker() {
            let family = new_family(CacheOrigin::Default);
            let dir = sibling(&family, "a", 1, None);
            std::fs::write(dir.join(WORKSPACE_MARKER_FILE), gone_str(&family)).unwrap();
            family.layout.sweep_family();
            assert!(!dir.exists());
        }

        #[test]
        fn leaseless_sibling_whose_marker_names_an_existing_tree_is_kept() {
            let family = new_family(CacheOrigin::Default);
            let tree = tempfile::tempdir().unwrap();
            let dir = leaseless(&family, "a", Some(&tree.path().to_string_lossy()));
            family.layout.sweep_family();
            assert!(dir.exists());
        }

        #[test]
        fn leaseless_sibling_without_a_marker_is_kept() {
            let family = new_family(CacheOrigin::Default);
            let dir = leaseless(&family, "a", None);
            family.layout.sweep_family();
            assert!(dir.exists());
        }

        #[test]
        fn fresh_lease_wins_over_a_marker_naming_a_missing_tree() {
            let family = new_family(CacheOrigin::Default);
            let dir = sibling(&family, "a", now(), None);
            std::fs::write(dir.join(WORKSPACE_MARKER_FILE), gone_str(&family)).unwrap();
            family.layout.sweep_family();
            assert!(dir.exists());
        }

        #[test]
        fn marker_that_is_empty_or_relative_is_kept() {
            let family = new_family(CacheOrigin::Default);
            let empty = leaseless(&family, "a", Some(""));
            let relative = leaseless(&family, "b", Some("no-such-relative-tree/x"));
            family.layout.sweep_family();
            assert!(empty.exists(), "an empty marker proves nothing");
            assert!(relative.exists(), "a relative path names no volume");
        }

        #[test]
        fn volume_root_is_parsed_from_the_path_prefix() {
            assert_eq!(volume_root(Path::new("relative/x")), None);
            #[cfg(windows)]
            {
                assert_eq!(volume_root(Path::new(r"C:\x\y")), Some(PathBuf::from(r"C:\")));
                assert_eq!(volume_root(Path::new(r"\\?\D:\x")), Some(PathBuf::from(r"\\?\D:\")));
                assert_eq!(
                    volume_root(Path::new(r"\\server\share\x\y")),
                    Some(PathBuf::from(r"\\server\share\"))
                );
                assert_eq!(volume_root(Path::new(r"\x")), None);
            }
            #[cfg(unix)]
            assert_eq!(volume_root(Path::new("/a/b")), Some(PathBuf::from("/")));
        }

        #[test]
        fn rotated_log_path_appends_to_the_whole_file_name() {
            assert_eq!(
                rotated_log_path(Path::new("logs/daemon.custom.log")),
                Some(PathBuf::from("logs/daemon.custom.log.prev"))
            );
            assert_eq!(rotated_log_path(Path::new("logs/..")), None);
            assert_eq!(rotated_log_path(Path::new("")), None);
        }

        #[cfg(windows)]
        #[test]
        fn stale_lease_on_an_absent_volume_is_kept() {
            let absent = ('D'..='Z')
                .map(|letter| format!(r"{letter}:\"))
                .find(|root| !matches!(Path::new(root).try_exists(), Ok(true)))
                .expect("no absent drive letter was available for this test");
            let family = new_family(CacheOrigin::Default);
            let tree = PathBuf::from(format!(r"{absent}no-such-project"));
            let dir = sibling(&family, "a", 1, Some(&tree));
            family.layout.sweep_family();
            assert!(dir.exists(), "an absent volume is no evidence that the tree is gone");
        }

        #[test]
        fn fresh_lease_of_a_missing_tree_is_kept() {
            let family = new_family(CacheOrigin::Default);
            let dir = sibling(&family, "a", now(), Some(&gone_tree(&family)));
            family.layout.sweep_family();
            assert!(dir.exists());
        }

        #[test]
        fn stale_lease_of_an_existing_tree_is_kept() {
            let family = new_family(CacheOrigin::Default);
            let tree = tempfile::tempdir().unwrap();
            let dir = sibling(&family, "a", 1, Some(tree.path()));
            family.layout.sweep_family();
            assert!(dir.exists());
        }

        #[test]
        fn stale_lease_without_a_named_tree_is_kept() {
            let family = new_family(CacheOrigin::Default);
            let dir = sibling(&family, "a", 1, None);
            family.layout.sweep_family();
            assert!(dir.exists());
        }

        #[test]
        fn directory_without_a_lease_but_with_a_database_is_kept() {
            let family = new_family(CacheOrigin::Default);
            let dir = family.dir.path().join("a");
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("bsl-graph.db"), b"x").unwrap();
            family.layout.sweep_family();
            assert!(dir.exists());
        }

        #[test]
        fn removal_leaves_no_claimed_directory_behind() {
            let family = new_family(CacheOrigin::Default);
            sibling(&family, "a", 1, Some(&gone_tree(&family)));
            family.layout.sweep_family();
            let left: Vec<_> = std::fs::read_dir(family.dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(left, [OsString::from("current")]);
        }

        #[test]
        fn a_claimed_directory_left_by_an_interrupted_sweep_is_finished() {
            let family = new_family(CacheOrigin::Default);
            // Holds a database and no identity: on its own it would be kept.
            let claimed = family.dir.path().join(format!("{SWEPT_DIR_PREFIX}a"));
            std::fs::create_dir(&claimed).unwrap();
            std::fs::write(claimed.join("bsl-graph.db"), b"x").unwrap();
            family.layout.sweep_family();
            assert!(!claimed.exists());
        }

        #[test]
        fn rewriting_the_marker_leaves_no_staging_file() {
            let workspace = tempfile::tempdir().unwrap();
            let family = new_family(CacheOrigin::Default);
            let layout = family.layout.clone().with_workspace(workspace.path().to_path_buf());
            std::fs::write(layout.root.join(WORKSPACE_MARKER_FILE), b"/stale/path").unwrap();
            layout.write_workspace_marker();
            assert_eq!(
                std::fs::read(layout.root.join(WORKSPACE_MARKER_FILE)).unwrap(),
                workspace.path().as_os_str().as_encoded_bytes()
            );
            let names: Vec<_> = std::fs::read_dir(&layout.root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, [OsString::from(WORKSPACE_MARKER_FILE)]);
        }

        #[test]
        fn lone_old_rotated_log_is_removed() {
            let family = new_family(CacheOrigin::Default);
            let dir = lone_log(&family, "a", 2 * DAY);
            family.layout.sweep_family();
            assert!(!dir.exists());
        }

        #[test]
        fn lone_recent_rotated_log_is_kept() {
            let family = new_family(CacheOrigin::Default);
            let dir = lone_log(&family, "a", Duration::from_secs(60));
            family.layout.sweep_family();
            assert!(dir.exists());
        }

        #[test]
        fn old_rotated_log_beside_a_database_is_kept() {
            let family = new_family(CacheOrigin::Default);
            let dir = lone_log(&family, "a", 2 * DAY);
            std::fs::write(dir.join("bsl-search.db"), b"x").unwrap();
            family.layout.sweep_family();
            assert!(dir.exists());
        }

        #[test]
        fn explicit_origin_removes_nothing() {
            let family = new_family(CacheOrigin::Explicit);
            let stale = sibling(&family, "a", 1, Some(&gone_tree(&family)));
            let log = lone_log(&family, "b", 2 * DAY);
            family.layout.sweep_family();
            family.layout.sweep_abandoned_siblings();
            assert!(stale.exists() && log.exists());
        }

        #[test]
        fn own_root_is_never_removed() {
            let family = new_family(CacheOrigin::Default);
            // The root would match both removal rules were it not skipped by identity.
            std::fs::write(
                family.layout.root.join(LEASE_FILE),
                serde_json::json!({
                    "generation": 1, "token": 1, "pid": 1, "heartbeat_secs": 1,
                    "workspace": gone_tree(&family).to_string_lossy(),
                })
                .to_string(),
            )
            .unwrap();
            family.layout.sweep_family();
            assert!(family.layout.root.exists());

            let log_only = new_family(CacheOrigin::Default);
            write_log(&log_only, &log_only.layout.root, 2 * DAY);
            log_only.layout.sweep_family();
            assert!(log_only.layout.root.exists());
        }
    }
}
