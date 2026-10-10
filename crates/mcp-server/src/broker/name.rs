//! Backend identity and rendezvous naming.
//!
//! The broker has no registry: a proxy finds its backend by recomputing the same
//! deterministic name from its own launch parameters. Two clients that resolve to
//! the same [`BackendKey`] therefore meet at the same socket; anything that should
//! fork a separate backend (different project, cache directory, profile, binary
//! version, embedding config, extension topology, or served tool surface) lands
//! on a different name by construction.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use crate::McpProfile;

/// The identity of one shared backend. Equal keys ⇒ same socket ⇒ reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendKey {
    /// Canonicalized project root. Callers should pass an already-`canonicalize`d
    /// path; [`BackendKey::new`] canonicalizes defensively so `/p` and `/p/` and a
    /// symlink to either collapse to one key.
    source_dir: PathBuf,
    /// Canonicalized root of the derived workspace state. Sharing a source tree is
    /// safe only when clients also agree where graph/search/lease files live.
    cache_dir: PathBuf,
    profile: McpProfile,
    /// Binary version (`CARGO_PKG_VERSION` of this crate = workspace version). An
    /// upgrade mints a fresh key, so a new proxy never speaks to an old backend on
    /// a possibly-changed protocol or schema; the old one drains by idle.
    version: &'static str,
    /// Fold of the embedding-related environment — see [`embedding_config_fingerprint`].
    config_fp: u64,
    /// Fold of the project's extension topology — see
    /// [`workspace_topology_fingerprint`]. Without it, the same source dir with a
    /// changed dependency graph would rendezvous with (and silently reuse) a daemon
    /// whose caches were built for the old graph.
    topology_fp: u64,
    /// Opt-in tools this launch actually adds to the served surface. A backend serves
    /// every session from one router, so a client that enabled nothing must not land on a
    /// daemon that enabled something: it would be handed a tool it never asked for, and
    /// the default-off promise would hold only for whoever started the daemon first.
    ///
    /// This is the *effective* set (`requested ∩ opt_in`), not the raw flags. Naming a
    /// tool that is already served changes no surface, so it must not fork a daemon —
    /// otherwise, on the release where an opt-in tool becomes a default, configs carrying
    /// the flag and configs without it would never share a backend again.
    enabled_tools: BTreeSet<String>,
}

impl BackendKey {
    /// Build a key for the current process. `config_fp` should come from
    /// [`embedding_config_fingerprint`] so a backend serving one embedding model is
    /// never silently reused by a client expecting another; `topology_fp` from
    /// [`workspace_topology_fingerprint`] so a dependency-graph change forks a fresh
    /// backend (the old one drains by idle).
    pub fn new(
        source_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        profile: McpProfile,
        config_fp: u64,
        topology_fp: u64,
        enabled_tools: BTreeSet<String>,
    ) -> Self {
        let source_dir = source_dir.into();
        let source_dir = std::fs::canonicalize(&source_dir).unwrap_or(source_dir);
        let cache_dir = cache_dir.into();
        let cache_dir = std::fs::canonicalize(&cache_dir).unwrap_or(cache_dir);
        Self {
            source_dir,
            cache_dir,
            profile,
            version: env!("CARGO_PKG_VERSION"),
            config_fp,
            topology_fp,
            enabled_tools,
        }
    }

    /// Stable 128-bit identity digest as 32 lowercase hex chars. Short enough to fit
    /// inside the `sun_path` limit when placed in a per-user runtime directory.
    pub fn digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.source_dir.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
        hasher.update(self.cache_dir.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
        hasher.update(self.profile.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(self.version.as_bytes());
        hasher.update(b"\0");
        hasher.update(&self.config_fp.to_le_bytes());
        hasher.update(b"\0");
        hasher.update(&self.topology_fp.to_le_bytes());
        // A `BTreeSet` feeds the names in one order whatever order or repeats the flags
        // arrived in, so two launches asking for the same surface hash the same.
        for name in &self.enabled_tools {
            hasher.update(b"\0");
            hasher.update(name.as_bytes());
        }
        let hex = hasher.finalize().to_hex();
        hex[..32].to_owned()
    }

    /// Filesystem path of the unix-domain socket for this backend. Windows derives
    /// its named-pipe name from the same [`digest`](Self::digest) and applies an
    /// explicit current-user-only security descriptor when binding.
    ///
    /// On unix the result is checked against the `sockaddr_un.sun_path` budget so a
    /// long runtime/temp directory surfaces here as `InvalidInput` rather than a
    /// confusing `EINVAL` when the daemon later `bind`s.
    pub fn socket_path(&self) -> io::Result<PathBuf> {
        let layout = runtime_socket_layout()?;
        let path = layout.dir.join(format!("{}.sock", self.digest()));
        self.validate_runtime_output(&path)?;
        prepare_runtime_socket_dir(&layout)?;
        #[cfg(unix)]
        {
            let len = path.as_os_str().as_encoded_bytes().len();
            if len >= SUN_PATH_MAX {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "broker socket path is {len} bytes, exceeds the sun_path budget \
                         of {SUN_PATH_MAX}: {}",
                        path.display()
                    ),
                ));
            }
        }
        Ok(path)
    }

    /// Runtime log path, gated against selected workspace inputs before creating the
    /// runtime directory. Windows also writes this file even though its rendezvous
    /// endpoint is a named pipe.
    pub fn runtime_log_path(&self) -> io::Result<PathBuf> {
        let layout = runtime_socket_layout()?;
        let path = layout.dir.join(format!("{}.log", self.digest()));
        self.validate_runtime_output(&path)?;
        prepare_runtime_socket_dir(&layout)?;
        Ok(path)
    }

    fn validate_runtime_output(&self, path: &Path) -> io::Result<()> {
        if matches!(self.profile, McpProfile::Workspace) {
            let project = crate::project::at(&self.source_dir)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            validate_runtime_endpoint(&project, path)?;
        }
        Ok(())
    }
}

fn validate_runtime_endpoint(project: &project_model::Project, path: &Path) -> io::Result<()> {
    if let Some(root) = crate::WorkspaceCacheLayout::overlapping_source_root(project, path)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "broker runtime endpoint {} overlaps workspace input root {}",
                path.display(),
                root.display()
            ),
        ));
    }
    Ok(())
}

/// `sun_path` capacity (including the trailing NUL) the kernel accepts for a
/// unix-domain socket address. A path of `>= SUN_PATH_MAX` bytes cannot be bound.
#[cfg(target_os = "linux")]
const SUN_PATH_MAX: usize = 108;
#[cfg(target_os = "macos")]
const SUN_PATH_MAX: usize = 104;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
const SUN_PATH_MAX: usize = 104;

/// Which parent directory a broker socket lives under. The variants are ordered by the
/// precedence in [`socket_dir_source`]; every one but `SharedTemp` is already uid-private,
/// so they share the trusted-parent handling.
enum SocketDirSource {
    /// `$XDG_RUNTIME_DIR` was set.
    Xdg(PathBuf),
    /// `$XDG_RUNTIME_DIR` was unset, but the canonical `/run/user/<euid>` runtime dir exists
    /// and is ours — the same tmpfs the env var would have pointed to.
    CanonicalRunUser(PathBuf),
    /// No per-user runtime dir, but the system temp dir is itself private to our uid — the
    /// shape macOS hands every user through `$TMPDIR`.
    PrivateTemp(PathBuf),
    /// A temp dir shared with other users (`/tmp`): our own tagged directory goes under it.
    SharedTemp(PathBuf),
}

/// Pure precedence decision (no filesystem or env access, so it is unit-testable): an explicit
/// `$XDG_RUNTIME_DIR` wins, else the canonical per-user runtime dir if one was found, else the
/// temp dir — as a trusted parent when it is already uid-private, as a shared one otherwise.
/// The `/run/user/<euid>` tier is what lets a spawner that *drops* `$XDG_RUNTIME_DIR`
/// (e.g. Codex launching the backend) still rendezvous with one that keeps it at the standard
/// path, instead of forking a second multi-GB backend under `/tmp`. It does NOT converge a
/// process that deliberately points `$XDG_RUNTIME_DIR` at a *non-standard* path with one that
/// dropped the variable — the explicit env still wins above, which is the right call and an
/// unavoidable split short of propagating the variable.
fn socket_dir_source(
    xdg: Option<PathBuf>,
    canonical_run_user: Option<PathBuf>,
    temp: PathBuf,
    temp_is_uid_private: bool,
) -> SocketDirSource {
    if let Some(base) = xdg {
        return SocketDirSource::Xdg(base);
    }
    if let Some(base) = canonical_run_user {
        return SocketDirSource::CanonicalRunUser(base);
    }
    if temp_is_uid_private {
        return SocketDirSource::PrivateTemp(temp);
    }
    SocketDirSource::SharedTemp(temp)
}

/// Directory name that holds the sockets themselves, under whichever parent
/// [`socket_dir_source`] picked.
const SOCKET_DIR_LEAF: &str = "bsl-mcp";

/// Where the sockets go, and what we have to create and validate ourselves to get there.
struct SocketDirLayout {
    /// Per-user directory that must exist and be ours before the leaf is created. `None`
    /// when the parent is already uid-private: it has no co-tenants to separate, so the
    /// tag would buy nothing and only spend `sun_path` bytes.
    tagged_base: Option<PathBuf>,
    /// The socket directory itself.
    dir: PathBuf,
}

/// Compose the layout for a source. Pure, so both the precedence and the `sun_path` cost of
/// the result are unit-testable without touching the filesystem.
///
/// Tagging a *private* parent is what used to push macOS past its budget: `$TMPDIR` there is
/// already a fixed 49-byte per-user path, and `bsl-mcp-<user>/bsl-mcp/<digest>.sock` on top of
/// it came to 111 bytes against Darwin's 104.
fn socket_dir_layout(source: SocketDirSource, who: &str) -> SocketDirLayout {
    match source {
        SocketDirSource::Xdg(base)
        | SocketDirSource::CanonicalRunUser(base)
        | SocketDirSource::PrivateTemp(base) => {
            SocketDirLayout { tagged_base: None, dir: base.join(SOCKET_DIR_LEAF) }
        }
        SocketDirSource::SharedTemp(base) => {
            let tagged = base.join(format!("bsl-mcp-{who}"));
            let dir = tagged.join(SOCKET_DIR_LEAF);
            SocketDirLayout { tagged_base: Some(tagged), dir }
        }
    }
}

/// Single safe path component naming the user, for the shared-temp layout. Sanitized to
/// alnum/`_`/`-` so a spoofed `USER=../x` cannot escape the temp dir.
fn user_tag() -> String {
    let raw = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
    let mut who: String =
        raw.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    if who.is_empty() {
        who.push_str("default");
    }
    who
}

/// The canonical per-user runtime dir `/run/user/<euid>` — the path `$XDG_RUNTIME_DIR` is set to
/// on a logind system — but only when it is, by its own metadata, a directory we own at mode
/// `0700`. `None` (→ temp fallback) otherwise. The trust is self-contained: `symlink_metadata`
/// (not `metadata`) refuses a symlink standing in for the dir, and the owner+mode check matches
/// the bar [`create_private_dir`] holds our own leaves to — we don't assume "it's systemd's".
/// Keyed on `euid`, never `$USER`/env, so two processes for the same user agree on the path.
#[cfg(unix)]
fn canonical_run_user_dir() -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let base = PathBuf::from(format!("/run/user/{}", current_euid()));
    let meta = std::fs::symlink_metadata(&base).ok()?;
    let ours = meta.is_dir()
        && meta.uid() == current_euid()
        && (meta.permissions().mode() & 0o777) == 0o700;
    ours.then_some(base)
}

/// Select the existing runtime location without creating it. Workspace callers
/// can validate the derived endpoint against project inputs before any mkdir.
fn runtime_socket_layout() -> io::Result<SocketDirLayout> {
    #[cfg(unix)]
    let (canonical, temp_is_uid_private) = (canonical_run_user_dir(), temp_dir_is_uid_private());
    #[cfg(not(unix))]
    let (canonical, temp_is_uid_private) = (None, false);

    let source = socket_dir_source(
        dirs::runtime_dir(),
        canonical,
        std::env::temp_dir(),
        temp_is_uid_private,
    );
    Ok(socket_dir_layout(source, &user_tag()))
}

fn prepare_runtime_socket_dir(layout: &SocketDirLayout) -> io::Result<()> {
    // Every level we create must be ours — otherwise an attacker who owns an ancestor could
    // swap our socket dir after it is validated. So validate the tagged base (rejecting an
    // attacker-pre-created one) before creating the leaf, never descending recursively through
    // an unvalidated parent. `/tmp`'s sticky bit then prevents anyone from renaming a base we
    // own. A trusted parent contributes no level of its own.
    if let Some(base) = &layout.tagged_base {
        create_private_dir(base)?;
    }
    create_private_dir(&layout.dir)?;
    Ok(())
}

/// The path with any trailing separators removed.
///
/// `lstat` on a path that ends in a separator must resolve to a directory, so POSIX makes it
/// follow the final component — and a check written to see a symlink would read straight
/// through the one it exists to reject. A path that is nothing but separators IS the root and
/// is returned unchanged.
#[cfg(unix)]
fn without_trailing_separators(path: &Path) -> &Path {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let end = bytes.iter().rposition(|byte| *byte != b'/').map_or(0, |last| last + 1);
    if end == 0 {
        return path;
    }
    Path::new(OsStr::from_bytes(&bytes[..end]))
}

/// Whether the system temp dir is itself private to our uid — the shape macOS gives every user
/// through `$TMPDIR` (`/var/folders/<…>/T`, mode `0700` and owned by us). Held to the same bar as
/// [`canonical_run_user_dir`]: `symlink_metadata` so a symlink cannot stand in for the dir, plus
/// owner and mode. `/tmp` is world-writable and so fails it, keeping the tagged layout there.
///
/// The separator is trimmed first because launchd hands `$TMPDIR` over WITH one, and
/// `std::env::temp_dir` passes the spelling through untouched — asking about the path as
/// given would follow the very link the check rejects.
#[cfg(unix)]
fn temp_dir_is_uid_private() -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let temp_dir = std::env::temp_dir();
    let Ok(meta) = std::fs::symlink_metadata(without_trailing_separators(&temp_dir)) else {
        return false;
    };
    meta.is_dir() && meta.uid() == current_euid() && (meta.permissions().mode() & 0o777) == 0o700
}

/// Current effective uid. `geteuid()` has no failure mode or preconditions.
#[cfg(unix)]
pub(crate) fn current_euid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {
            // A pre-existing dir must be private AND owned by us; otherwise a co-tenant
            // could have pre-created a 0700 dir they own to host (and observe or
            // pre-empt) our socket. This matters for the shared-/tmp fallback;
            // `$XDG_RUNTIME_DIR` is already kernel-private to our uid.
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o700 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "broker runtime dir {} has mode {mode:#o}, expected 0o700",
                        dir.display()
                    ),
                ));
            }
            if meta.uid() != current_euid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "broker runtime dir {} is owned by uid {}, not the current user {}",
                        dir.display(),
                        meta.uid(),
                        current_euid()
                    ),
                ));
            }
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("broker runtime path {} exists and is not a directory", dir.display()),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Non-recursive: the parent must already exist and be trusted (the caller
            // validates each level top-down), so we never create or descend through an
            // unvalidated ancestor.
            std::fs::DirBuilder::new().mode(0o700).create(dir)
        }
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Fold the embedding-relevant environment into a stable 64-bit fingerprint. Only
/// variables that change what an index *means* are included (endpoint, model,
/// dimension, provider); auth (`EMBEDDING_API_KEY`) and perf knobs are excluded so
/// key rotation and tuning do not fork the backend — and no secret-derived bytes
/// land in a socket name.
pub fn embedding_config_fingerprint() -> u64 {
    let prefixes = crate::EmbeddingPrefixes {
        query: std::env::var("EMBEDDING_QUERY_PREFIX").unwrap_or_default(),
        document: std::env::var("EMBEDDING_DOCUMENT_PREFIX").unwrap_or_default(),
        token_profile: None,
    };
    embedding_config_fingerprint_with_prefixes(&prefixes)
}

pub fn embedding_config_fingerprint_with_prefixes(prefixes: &crate::EmbeddingPrefixes) -> u64 {
    const KEYS: [&str; 4] =
        ["EMBEDDING_URL", "EMBEDDING_MODEL", "EMBEDDING_DIM", "EMBEDDING_PROVIDER"];
    let mut hasher = blake3::Hasher::new();
    for key in KEYS {
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        if let Ok(value) = std::env::var(key) {
            hasher.update(value.as_bytes());
        }
        hasher.update(b"\0");
    }
    if !prefixes.query.is_empty() || !prefixes.document.is_empty() {
        hasher.update(b"input-prefixes-v1");
        for prefix in [&prefixes.query, &prefixes.document] {
            hasher.update(&(prefix.len() as u64).to_le_bytes());
            hasher.update(prefix.as_bytes());
        }
    }
    if let Some(profile) = &prefixes.token_profile {
        hasher.update(b"token-policy-profile-v1\0");
        hasher.update(&profile.max_input_tokens.to_le_bytes());
        hasher.update(bsl_search::SEGMENTATION_VERSION.as_bytes());
        hasher.update(profile.tokenizer_file.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
        hasher.update(profile.tokenizer_sha256.as_bytes());
    }
    if prefixes.token_profile.is_some() {
        hasher.update(b"request-bytes=");
        match bsl_search::EmbedderConfig::request_bytes_from_env() {
            Ok(max_request_bytes) => hasher.update(&max_request_bytes.to_le_bytes()),
            Err(_) => hasher.update(
                std::env::var("EMBEDDING_MAX_REQUEST_BYTES").unwrap_or_default().as_bytes(),
            ),
        };
        hasher.update(b"\0");
    }
    let bytes = hasher.finalize();
    u64::from_le_bytes(bytes.as_bytes()[..8].try_into().expect("blake3 yields >= 8 bytes"))
}

pub const EMBEDDING_QUERY_PREFIX_ENV: &str = "BSL_MCP_EFFECTIVE_EMBEDDING_QUERY_PREFIX";
pub const EMBEDDING_DOCUMENT_PREFIX_ENV: &str = "BSL_MCP_EFFECTIVE_EMBEDDING_DOCUMENT_PREFIX";
pub const EMBEDDING_MAX_INPUT_TOKENS_ENV: &str = "BSL_MCP_EFFECTIVE_EMBEDDING_MAX_INPUT_TOKENS";
pub const EMBEDDING_TOKENIZER_FILE_ENV: &str = "BSL_MCP_EFFECTIVE_EMBEDDING_TOKENIZER_FILE";
pub const EMBEDDING_TOKENIZER_SHA256_ENV: &str = "BSL_MCP_EFFECTIVE_EMBEDDING_TOKENIZER_SHA256";

/// Env var carrying the spawning proxy's frozen topology fingerprint to the daemon
/// child, so both sides key the SAME rendezvous even if the config changes between
/// their respective project reads.
pub const TOPOLOGY_FP_ENV: &str = "BSL_MCP_TOPOLOGY_FP";

/// Stable 64-bit identity of the project's extension topology at `source_dir`, for
/// backend keying: a daemon whose graph/search/diagnostics caches were built for
/// one dependency graph must not be reused by a client whose config now declares
/// another. Proxy and daemon both derive it through this one function (from the
/// same `--source-dir`), so they agree by construction. An invalid or unloadable
/// project folds as `0` on both sides — still one rendezvous, just untagged.
pub fn workspace_topology_fingerprint(source_dir: &Path) -> u64 {
    match crate::project::at(source_dir) {
        Ok(project) => crate::graph::scan::topology_hex_u64(
            &project.extension_topology().fingerprint().to_hex(),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "broker key: project topology unavailable, folding as 0");
            0
        }
    }
}

/// Cross-platform rendezvous name for a backend.
///
/// Unix uses the filesystem socket path ([`BackendKey::socket_path`]) so we own
/// stale-file recovery. Windows uses a namespaced named pipe derived from the
/// same digest, with pipe security applied by the daemon listener.
pub fn backend_name(key: &BackendKey) -> io::Result<interprocess::local_socket::Name<'static>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        key.socket_path()?.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        // Windows rendezvous does not use a filesystem socket, but the detached child
        // still writes its runtime log there. Validate that automatic output before the
        // directory is created, just as the Unix socket path does.
        let _ = key.runtime_log_path()?;
        format!("bsl-mcp-{}.sock", key.digest()).to_ns_name::<GenericNamespaced>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(dir: &str, profile: McpProfile, fp: u64) -> BackendKey {
        keyed(dir, profile, fp, BTreeSet::new())
    }

    fn keyed(
        dir: &str,
        profile: McpProfile,
        fp: u64,
        enabled_tools: BTreeSet<String>,
    ) -> BackendKey {
        // Bypass `new`'s canonicalize (the path need not exist in unit tests).
        BackendKey {
            source_dir: PathBuf::from(dir),
            cache_dir: PathBuf::from(dir).join(".build"),
            profile,
            version: "test",
            config_fp: fp,
            topology_fp: 0,
            enabled_tools,
        }
    }

    #[test]
    fn digest_is_deterministic_and_32_hex() {
        let k = key("/srv/erp", McpProfile::Workspace, 7);
        let d = k.digest();
        assert_eq!(d.len(), 32);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(d, k.digest());
    }

    #[test]
    fn each_identity_axis_forks_the_digest() {
        let base = key("/srv/erp", McpProfile::Workspace, 7).digest();
        assert_ne!(base, key("/srv/other", McpProfile::Workspace, 7).digest(), "source_dir");
        assert_ne!(base, key("/srv/erp", McpProfile::Reference, 7).digest(), "profile");
        assert_ne!(base, key("/srv/erp", McpProfile::Workspace, 8).digest(), "config_fp");

        let mut moved_cache = key("/srv/erp", McpProfile::Workspace, 7);
        moved_cache.cache_dir = PathBuf::from("/var/cache/erp-next");
        assert_ne!(base, moved_cache.digest(), "cache_dir");

        let mut bumped = key("/srv/erp", McpProfile::Workspace, 7);
        bumped.version = "test-next";
        assert_ne!(base, bumped.digest(), "version");

        let mut retopologized = key("/srv/erp", McpProfile::Workspace, 7);
        retopologized.topology_fp = 1;
        assert_ne!(
            base,
            retopologized.digest(),
            "same dir with a different dependency graph must land on a different socket"
        );

        assert_ne!(
            base,
            keyed("/srv/erp", McpProfile::Workspace, 7, set(&["references"])).digest(),
            "a launch serving an extra tool must not share a backend with one that does not"
        );
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// The identity must follow the served surface, not the spelling of the flags. Two
    /// launches asking for the same tools in different order, or with a repeat, are the
    /// same backend — otherwise clients would fork daemons over argv cosmetics.
    #[test]
    fn tool_set_is_order_and_duplicate_insensitive() {
        let one = keyed("/srv/erp", McpProfile::Workspace, 7, set(&["references", "outline"]));
        let other =
            keyed("/srv/erp", McpProfile::Workspace, 7, set(&["outline", "references", "outline"]));
        assert_eq!(one.digest(), other.digest());
    }

    /// Asking for a tool the profile already serves changes no surface, so it must not
    /// change the rendezvous either. This is what keeps configs carrying the flag and
    /// configs without it on one daemon after an opt-in tool becomes a default; hashing
    /// the raw flags instead of the effective set would split them forever.
    #[test]
    fn enabling_a_default_tool_does_not_fork_the_backend() {
        let default_tool = crate::contract::declared_tools(McpProfile::Workspace)
            .next()
            .expect("the workspace profile declares tools")
            .to_owned();
        let asked_for_it = crate::contract::effective_opt_in(
            McpProfile::Workspace,
            std::slice::from_ref(&default_tool),
        );
        assert_eq!(
            key("/srv/erp", McpProfile::Workspace, 7).digest(),
            keyed("/srv/erp", McpProfile::Workspace, 7, asked_for_it).digest(),
            "--enable-tool {default_tool} serves the same surface and must reuse the backend"
        );
    }

    /// A different set of tools is a different surface, and the digest must say so even
    /// when one set contains the other.
    #[test]
    fn a_wider_tool_set_forks_the_digest() {
        let narrow = keyed("/srv/erp", McpProfile::Workspace, 7, set(&["references"]));
        let wide = keyed("/srv/erp", McpProfile::Workspace, 7, set(&["references", "outline"]));
        assert_ne!(narrow.digest(), wide.digest());
    }

    #[test]
    fn socket_path_sits_under_runtime_dir_and_is_named_by_digest() {
        let k = key("/srv/erp", McpProfile::Reference, 7);
        let path = k.socket_path().expect("runtime dir resolvable");
        let runtime_dir = runtime_socket_layout().unwrap().dir;
        assert_eq!(path.parent(), Some(runtime_dir.as_path()));
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some(format!("{}.sock", k.digest()).as_str())
        );
    }

    #[test]
    fn workspace_cache_scope_runtime_endpoint_and_log_overlap_fails_before_creation() {
        let root = tempfile::tempdir().unwrap();
        let key = BackendKey::new(
            root.path(),
            root.path().join("cache"),
            McpProfile::Workspace,
            0,
            0,
            BTreeSet::new(),
        );
        let runtime = root.path().join("runtime").join(SOCKET_DIR_LEAF);
        for endpoint in [runtime.join("derived.sock"), runtime.join("derived.log")] {
            let error = key
                .validate_runtime_output(&endpoint)
                .expect_err("runtime endpoint under project input is refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert!(!runtime.exists(), "the preflight must not create the overlapping path");
    }

    #[test]
    fn fingerprint_tracks_only_identity_env() {
        // No assertion on the absolute value (depends on ambient env); just that the
        // function is callable and stable within one environment.
        assert_eq!(embedding_config_fingerprint(), embedding_config_fingerprint());
    }

    #[test]
    fn effective_prefix_pair_partitions_profile_without_credentials() {
        let empty = crate::EmbeddingPrefixes::default();
        let query = crate::EmbeddingPrefixes {
            query: "q: ".into(),
            document: String::new(),
            token_profile: None,
        };
        let document = crate::EmbeddingPrefixes {
            query: String::new(),
            document: "d: ".into(),
            token_profile: None,
        };
        assert_ne!(
            embedding_config_fingerprint_with_prefixes(&empty),
            embedding_config_fingerprint_with_prefixes(&query)
        );
        assert_ne!(
            embedding_config_fingerprint_with_prefixes(&query),
            embedding_config_fingerprint_with_prefixes(&document)
        );
    }

    #[test]
    fn token_profile_changes_broker_identity() {
        let base = crate::EmbeddingPrefixes::default();
        let profile = |max_input_tokens, tokenizer_file: &str, tokenizer_sha256: &str| {
            crate::EmbeddingPrefixes {
                token_profile: Some(crate::EmbeddingTokenProfile {
                    max_input_tokens,
                    tokenizer_file: PathBuf::from(tokenizer_file),
                    tokenizer_sha256: tokenizer_sha256.to_owned(),
                    token_policy: None,
                }),
                ..base.clone()
            }
        };
        let a =
            embedding_config_fingerprint_with_prefixes(&profile(8192, "/a/tokenizer.json", "abc"));
        assert_ne!(
            a,
            embedding_config_fingerprint_with_prefixes(&profile(8193, "/a/tokenizer.json", "abc"))
        );
        assert_ne!(
            a,
            embedding_config_fingerprint_with_prefixes(&profile(8192, "/b/tokenizer.json", "abc"))
        );
        assert_ne!(
            a,
            embedding_config_fingerprint_with_prefixes(&profile(8192, "/a/tokenizer.json", "def"))
        );
    }

    #[test]
    fn socket_dir_source_falls_back_to_canonical_when_xdg_is_dropped() {
        use super::{socket_dir_source, SocketDirSource};
        let xdg = PathBuf::from("/run/user/1000");
        let canonical = PathBuf::from("/run/user/1000");
        let tmp = PathBuf::from("/tmp");

        // $XDG_RUNTIME_DIR set → it wins outright.
        assert!(matches!(
            socket_dir_source(Some(xdg.clone()), Some(canonical.clone()), tmp.clone(), false),
            SocketDirSource::Xdg(p) if p == xdg
        ));
        // The regression this fix targets: a spawner (e.g. Codex) dropped $XDG_RUNTIME_DIR but
        // the canonical /run/user/<euid> is there — use it, NOT the /tmp fallback, so both
        // processes meet at the same socket instead of forking a second backend.
        assert!(matches!(
            socket_dir_source(None, Some(canonical.clone()), tmp.clone(), false),
            SocketDirSource::CanonicalRunUser(p) if p == canonical
        ));
        // Neither available, and the temp dir is shared → tag it with the user.
        assert!(matches!(
            socket_dir_source(None, None, tmp.clone(), false),
            SocketDirSource::SharedTemp(p) if p == tmp
        ));
    }

    /// Darwin's `sun_path`, the tightest budget of any supported platform. Asserted by name so
    /// the rule below still holds when the test happens to run on a roomier host.
    const DARWIN_SUN_PATH_MAX: usize = 104;

    /// A private per-user temp dir carries no co-tenants, so the socket dir hangs directly off
    /// it. macOS depends on that: `$TMPDIR` there is a fixed 49-byte path, and inserting a
    /// `bsl-mcp-<user>` level took the socket to 111 bytes — past Darwin's `sun_path`, so the
    /// broker could not bind at all under the platform's own default temp dir.
    #[test]
    fn a_private_temp_dir_keeps_the_socket_inside_the_sun_path_budget() {
        use super::{socket_dir_layout, socket_dir_source, SocketDirSource};
        // The shape Darwin builds for every user: `/var/folders/<2>/<30>/T/`, 49 bytes fixed.
        let tmp = PathBuf::from("/var/folders/ab/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/T/");
        let source = socket_dir_source(None, None, tmp.clone(), true);
        assert!(matches!(source, SocketDirSource::PrivateTemp(_)));

        let leaf = format!("{}.sock", key("/srv/erp", McpProfile::Workspace, 7).digest());
        let layout = socket_dir_layout(source, "user");
        assert!(layout.tagged_base.is_none(), "a uid-private parent needs no user tag");

        let socket = layout.dir.join(&leaf);
        let len = socket.as_os_str().as_encoded_bytes().len();
        assert!(
            len < DARWIN_SUN_PATH_MAX,
            "socket path is {len} bytes, over the {DARWIN_SUN_PATH_MAX}-byte budget: {}",
            socket.display()
        );

        // The tag is what overflowed: the same temp dir treated as shared does not fit.
        let tagged = socket_dir_layout(SocketDirSource::SharedTemp(tmp), "user");
        assert!(tagged.dir.join(&leaf).as_os_str().as_encoded_bytes().len() >= DARWIN_SUN_PATH_MAX);
    }

    /// The privacy check must see the path it was given, not what the path points at.
    ///
    /// launchd hands `$TMPDIR` over with a trailing separator, and `lstat` on such a path
    /// resolves its final component — so the anti-symlink half of the check would never fire
    /// on the one platform it was written for.
    #[cfg(unix)]
    #[test]
    fn a_trailing_separator_does_not_let_a_link_stand_in_for_the_dir() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let with_separator = PathBuf::from(format!("{}/", link.display()));
        assert!(
            !std::fs::symlink_metadata(&with_separator).unwrap().file_type().is_symlink(),
            "the hazard being trimmed for: asked with the separator, lstat follows the link",
        );
        assert_eq!(
            without_trailing_separators(&with_separator),
            link,
            "the trimmed path is the link itself",
        );
        assert!(
            std::fs::symlink_metadata(without_trailing_separators(&with_separator))
                .unwrap()
                .file_type()
                .is_symlink(),
            "trimmed, the check sees the link it exists to reject",
        );

        // A path that is nothing but separators is the root, and keeps every one of them.
        assert_eq!(without_trailing_separators(Path::new("/")), Path::new("/"));
        assert_eq!(without_trailing_separators(Path::new("/tmp")), Path::new("/tmp"));
    }
}
