//! Help of an installed platform: locating its two help archives, extracting the
//! corpus in-process, and the on-disk cache that spares a re-extraction while
//! the archives stay the same.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::package::{self, Slot, VerifiedCorpus};

pub const SHCNTX: &str = "shcntx_ru.hbk";
pub const SHLANG: &str = "shlang_ru.hbk";
/// Overrides the cache directory; tests and acceptance runs use it to start from
/// a clean cache without touching the user's one.
pub const CACHE_DIR_ENV: &str = "BSL_PLATFORM_HELP_CACHE_DIR";
pub const PLATFORM_PATH_ENV: &str = "BSL_PLATFORM_PATH";

const BIN_DIR: &str = "bin";

/// The two help archives of one platform installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbkPair {
    /// The installation directory, the one named after the platform version;
    /// the archives themselves may sit in its `bin` subdirectory.
    pub dir: PathBuf,
    pub shcntx: PathBuf,
    pub shlang: PathBuf,
}

impl HbkPair {
    /// The archives of the installation `dir`: next to it first, then in its
    /// `bin`. A `dir` that is itself an installation's `bin` stands for that
    /// installation.
    fn in_dir(dir: &Path) -> Option<Self> {
        let holding = |holder: &Path, installation: &Path| {
            let pair = Self {
                dir: installation.to_path_buf(),
                shcntx: holder.join(SHCNTX),
                shlang: holder.join(SHLANG),
            };
            (pair.shcntx.is_file() && pair.shlang.is_file()).then_some(pair)
        };
        let installation = match dir.file_name() {
            Some(name) if name.eq_ignore_ascii_case(BIN_DIR) => dir.parent().unwrap_or(dir),
            _ => dir,
        };
        holding(dir, installation).or_else(|| holding(&dir.join(BIN_DIR), dir))
    }

    /// The platform version named by the installation directory, when it is one.
    pub fn platform_version(&self) -> Option<String> {
        let name = self.dir.file_name()?.to_str()?;
        name.parse::<bsl_platform::PlatformVersion>().ok().map(|_| name.to_owned())
    }
}

/// Where loading reads and writes: injected so tests never see the user's cache
/// or installations.
#[derive(Debug, Clone)]
pub struct LoadContext {
    pub cache_dir: PathBuf,
    /// Directories whose subdirectories are candidate installations.
    pub discovery_roots: Vec<PathBuf>,
    pub platform_path_env: Option<OsString>,
}

impl LoadContext {
    pub fn from_environment() -> Self {
        let cache_dir = std::env::var_os(CACHE_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::cache_dir().map(|dir| dir.join("bsl-analyzer").join("platform-help")))
            .unwrap_or_else(|| std::env::temp_dir().join("bsl-analyzer-platform-help"));
        Self {
            cache_dir,
            discovery_roots: default_discovery_roots(),
            platform_path_env: std::env::var_os(PLATFORM_PATH_ENV)
                .filter(|value| !value.is_empty()),
        }
    }
}

fn default_discovery_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if cfg!(target_os = "linux") {
        roots.push(PathBuf::from("/opt/1cv8/x86_64"));
    }
    if cfg!(target_os = "macos") {
        roots.push(PathBuf::from("/opt/1cv8"));
    }
    if cfg!(target_os = "windows") {
        let program_files =
            std::env::var_os("ProgramFiles").unwrap_or_else(|| OsString::from("C:\\Program Files"));
        roots.push(PathBuf::from(program_files).join("1cv8"));
    }
    roots
}

/// The installation to read: an explicit directory, else `BSL_PLATFORM_PATH`,
/// else discovery. An explicit choice that holds no archives is an error, never
/// a reason to look elsewhere.
pub fn locate(path: Option<&Path>, context: &LoadContext) -> Result<HbkPair, String> {
    if let Some(path) = path {
        return pair_in(path, "platform_help.path");
    }
    if let Some(env) = &context.platform_path_env {
        return pair_in(Path::new(env), PLATFORM_PATH_ENV);
    }
    discover(&context.discovery_roots)
}

/// The archive pair in `dir`; `what` names where the directory was given.
pub fn pair_in(dir: &Path, what: &str) -> Result<HbkPair, String> {
    HbkPair::in_dir(dir)
        .ok_or_else(|| format!("{what} {} has no {SHCNTX} and {SHLANG}", dir.display()))
}

/// Among installations under `roots`, the highest numeric version; equal or
/// non-numeric names are ordered by path.
pub fn discover(roots: &[PathBuf]) -> Result<HbkPair, String> {
    let mut found: Vec<HbkPair> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            let dir = entry.path();
            let name = entry.file_name();
            if name == "common" || name == "conf" {
                continue;
            }
            if let Some(pair) = HbkPair::in_dir(&dir) {
                found.push(pair);
            }
        }
    }
    found.sort_by(|a, b| match (numeric_version(&a.dir), numeric_version(&b.dir)) {
        (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.dir.cmp(&b.dir)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.dir.cmp(&b.dir),
    });
    let roots_list = roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ");
    let chosen = found.into_iter().next().ok_or_else(|| {
        format!("no installed platform with {SHCNTX} and {SHLANG} under [{roots_list}]")
    })?;
    tracing::info!(path = %chosen.dir.display(), "discovered installed platform help");
    Ok(chosen)
}

fn numeric_version(dir: &Path) -> Option<Vec<u32>> {
    let name = dir.file_name()?.to_str()?;
    let parts: Option<Vec<u32>> = name.split('.').map(|part| part.parse().ok()).collect();
    parts.filter(|parts| parts.len() >= 2)
}

/// Identity of an installation's archives: paths and contents.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveIdentity {
    pub shcntx: PathBuf,
    pub shcntx_sha256: String,
    pub shlang: PathBuf,
    pub shlang_sha256: String,
    pub extractor_version: String,
}

impl ArchiveIdentity {
    pub fn of(pair: &HbkPair) -> Result<Self, String> {
        let digest = |path: &Path| {
            fs::read(path)
                .map(|bytes| package::sha256_hex(&bytes))
                .map_err(|error| format!("{}: {error}", path.display()))
        };
        let canonical = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Ok(Self {
            shcntx: canonical(&pair.shcntx),
            shcntx_sha256: digest(&pair.shcntx)?,
            shlang: canonical(&pair.shlang),
            shlang_sha256: digest(&pair.shlang)?,
            extractor_version: html_parser::EXTRACTOR_VERSION.to_owned(),
        })
    }

    fn cache_key(&self) -> String {
        let text = serde_json::to_string(self).expect("identity serializes");
        package::sha256_hex(text.as_bytes())[..32].to_owned()
    }
}

/// Extracts the corpus of `pair` and publishes it into `slot`, with `extra`
/// files stored alongside. The extraction works in a private directory under
/// the cache that it removes afterwards.
pub fn extract_into_slot(
    pair: &HbkPair,
    slot: &Slot,
    extra: &[(&str, &[u8])],
    context: &LoadContext,
) -> Result<VerifiedCorpus, String> {
    let _span = tracing::info_span!("extract_platform_help", path = %pair.dir.display()).entered();
    let scratch = context.cache_dir.join("work");
    let data = html_parser::extract_corpus_from_hbk(&pair.shcntx, &pair.shlang, &scratch).map_err(
        |error| format!("help extraction from {} failed: {error:#}", pair.dir.display()),
    )?;
    let json = data.to_json().map_err(|error| format!("{error:#}"))?;
    let corpus_id = format!(
        "installed-{}",
        pair.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    );
    slot.publish(
        &json,
        &corpus_id,
        pair.platform_version().as_deref(),
        html_parser::EXTRACTOR_VERSION,
        extra,
    )
}

/// The corpus of an installation: the cached package when its archives are
/// unchanged, a fresh extraction otherwise.
pub fn installed_corpus(pair: &HbkPair, context: &LoadContext) -> Result<VerifiedCorpus, String> {
    let identity = ArchiveIdentity::of(pair)?;
    let slot = Slot::new(context.cache_dir.join("installed").join(identity.cache_key()));
    if let Ok(corpus) = slot.read() {
        tracing::info!(cache = %slot.dir().display(), "reusing extracted platform help");
        return Ok(corpus);
    }
    extract_into_slot(pair, &slot, &[], context)
}

/// The snapshot `auto` keeps for itself, stored with the identity of the
/// archives it came from.
pub struct AutoSnapshot {
    slot: Slot,
}

const AUTO_SOURCE: &str = "source.json";

impl AutoSnapshot {
    pub fn new(context: &LoadContext) -> Self {
        Self { slot: Slot::new(context.cache_dir.join("auto")) }
    }

    pub fn location(&self) -> &Path {
        self.slot.dir()
    }

    /// The saved corpus with the identity of the archives it came from, read
    /// from one package so the two always belong together.
    pub fn read_with_identity(&self) -> Option<(VerifiedCorpus, Option<ArchiveIdentity>)> {
        let package = self.slot.current(&[AUTO_SOURCE]).ok()?;
        let identity =
            package.extra(AUTO_SOURCE).and_then(|bytes| serde_json::from_slice(bytes).ok());
        Some((package.corpus, identity))
    }

    pub fn read(&self) -> Result<VerifiedCorpus, String> {
        self.slot.read()
    }

    /// Extracts `pair` and saves it as the `auto` snapshot of `identity`.
    pub fn save(
        &self,
        identity: &ArchiveIdentity,
        pair: &HbkPair,
        context: &LoadContext,
    ) -> Result<VerifiedCorpus, String> {
        let source = serde_json::to_vec_pretty(identity).expect("identity serializes");
        extract_into_slot(pair, &self.slot, &[(AUTO_SOURCE, &source)], context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installation(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SHCNTX), b"x").unwrap();
        fs::write(dir.join(SHLANG), b"y").unwrap();
        dir
    }

    fn context(cache: &Path, roots: Vec<PathBuf>, env: Option<&Path>) -> LoadContext {
        LoadContext {
            cache_dir: cache.to_path_buf(),
            discovery_roots: roots,
            platform_path_env: env.map(|p| p.as_os_str().to_owned()),
        }
    }

    #[test]
    fn discovery_prefers_the_highest_numeric_version_then_the_path() {
        let root = tempfile::tempdir().unwrap();
        installation(root.path(), "8.3.9.100");
        let newest = installation(root.path(), "8.3.27.2214");
        installation(root.path(), "custom");
        fs::create_dir_all(root.path().join("8.3.30.1")).unwrap(); // no archives
        assert_eq!(discover(&[root.path().to_path_buf()]).unwrap().dir, newest);

        let only_named = tempfile::tempdir().unwrap();
        let alpha = installation(only_named.path(), "alpha");
        installation(only_named.path(), "beta");
        assert_eq!(discover(&[only_named.path().to_path_buf()]).unwrap().dir, alpha);

        let empty = tempfile::tempdir().unwrap();
        assert!(discover(&[empty.path().to_path_buf()]).unwrap_err().contains("no installed"));
    }

    #[test]
    fn explicit_locations_are_never_replaced_by_discovery() {
        let root = tempfile::tempdir().unwrap();
        let discovered = installation(root.path(), "8.3.27.1");
        let elsewhere = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_path_buf()];

        let ctx = context(elsewhere.path(), roots.clone(), None);
        assert_eq!(locate(None, &ctx).unwrap().dir, discovered);
        let wrong = elsewhere.path().join("missing");
        assert!(locate(Some(&wrong), &ctx).unwrap_err().contains("platform_help.path"));

        let with_env = context(elsewhere.path(), roots, Some(&wrong));
        assert!(locate(None, &with_env).unwrap_err().contains(PLATFORM_PATH_ENV));
        let env_ok = context(elsewhere.path(), vec![], Some(&discovered));
        assert_eq!(locate(None, &env_ok).unwrap().dir, discovered);
    }

    #[test]
    fn archives_in_the_bin_subdirectory_belong_to_the_installation() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("8.3.27.1786");
        let bin = install.join(BIN_DIR);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join(SHCNTX), b"x").unwrap();
        fs::write(bin.join(SHLANG), b"y").unwrap();

        let discovered = discover(&[root.path().to_path_buf()]).unwrap();
        assert_eq!(discovered.dir, install);
        assert_eq!(discovered.shcntx, bin.join(SHCNTX));
        assert_eq!(discovered.platform_version().as_deref(), Some("8.3.27.1786"));

        for given in [&install, &bin] {
            let pair = pair_in(given, "platform_help.path").unwrap();
            assert_eq!(pair.dir, install);
            assert_eq!(pair.platform_version().as_deref(), Some("8.3.27.1786"));
        }
    }

    #[test]
    fn archives_beside_the_version_directory_win_over_bin() {
        let root = tempfile::tempdir().unwrap();
        let install = installation(root.path(), "8.3.27.1786");
        let bin = install.join(BIN_DIR);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join(SHCNTX), b"x").unwrap();
        fs::write(bin.join(SHLANG), b"y").unwrap();
        assert_eq!(pair_in(&install, "p").unwrap().shcntx, install.join(SHCNTX));
    }

    #[test]
    fn version_comes_from_a_numeric_installation_directory() {
        let root = tempfile::tempdir().unwrap();
        let pair = HbkPair::in_dir(&installation(root.path(), "8.3.27.2214")).unwrap();
        assert_eq!(pair.platform_version().as_deref(), Some("8.3.27.2214"));
        let pair = HbkPair::in_dir(&installation(root.path(), "help")).unwrap();
        assert_eq!(pair.platform_version(), None);
    }
}
