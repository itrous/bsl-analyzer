//! The corpus package: the help corpus JSON plus a `manifest.json` that names
//! its platform version, extractor and SHA-256. A legacy bare corpus JSON is
//! accepted too, with an unknown platform version.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST_FILE_NAME: &str = "manifest.json";
pub const DEFAULT_CORPUS_FILE_NAME: &str = "platform_data.json";
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusManifest {
    pub schema_version: u32,
    /// Stable name of this corpus build, e.g. `platform-help-8.3.27.2214`.
    pub corpus_id: String,
    /// Platform version the corpus was extracted from; `None` when unknown.
    #[serde(default)]
    pub platform_version: Option<String>,
    /// Version of the extractor that produced the corpus.
    pub extractor_version: String,
    /// Corpus file next to the manifest: a bare file name.
    #[serde(default = "default_corpus_file")]
    pub corpus_file: String,
    /// Lower-case hex SHA-256 of the corpus file.
    pub sha256: String,
}

fn default_corpus_file() -> String {
    DEFAULT_CORPUS_FILE_NAME.to_owned()
}

/// A corpus that passed the manifest and digest checks; not yet decoded.
#[derive(Debug, Clone)]
pub struct VerifiedCorpus {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub manifest: Option<CorpusManifest>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

impl CorpusManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("malformed corpus manifest: {error}"))?;
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(format!(
                "unsupported corpus manifest schema_version {} (expected {MANIFEST_SCHEMA_VERSION})",
                manifest.schema_version
            ));
        }
        if !is_bare_file_name(&manifest.corpus_file) {
            return Err(format!(
                "corpus manifest names `{}`, not a file next to the manifest",
                manifest.corpus_file
            ));
        }
        if manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("corpus manifest sha256 is not a SHA-256 hex digest".to_owned());
        }
        Ok(manifest)
    }

    /// Checks `bytes` against the manifest digest.
    pub fn verify(self, bytes: Vec<u8>) -> Result<VerifiedCorpus, String> {
        let sha256 = sha256_hex(&bytes);
        if !sha256.eq_ignore_ascii_case(&self.sha256) {
            return Err(format!(
                "corpus `{}` has SHA-256 {sha256}, the manifest declares {}",
                self.corpus_file, self.sha256
            ));
        }
        Ok(VerifiedCorpus { bytes, sha256, manifest: Some(self) })
    }
}

/// A plain file name that resolves next to the manifest both as a path and as
/// a relative URL: no separators of either kind (a URL treats `\\` as `/`), no
/// dot-only names, nothing a URL parser would reinterpret.
fn is_bare_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Reads a package directory, a `manifest.json` path, or a legacy corpus JSON.
pub fn read_local(path: &Path) -> Result<VerifiedCorpus, String> {
    let manifest_path = if path.is_dir() {
        let manifest = path.join(MANIFEST_FILE_NAME);
        if !manifest.is_file() {
            return Err(format!("{} has no {MANIFEST_FILE_NAME}", path.display()));
        }
        Some(manifest)
    } else if path.file_name().is_some_and(|name| name == MANIFEST_FILE_NAME) {
        Some(path.to_path_buf())
    } else {
        None
    };

    let read = |path: &Path| fs::read(path).map_err(|error| format!("{}: {error}", path.display()));
    match manifest_path {
        Some(manifest_path) => {
            let manifest = CorpusManifest::parse(&read(&manifest_path)?)?;
            let dir = manifest_path.parent().unwrap_or(Path::new("."));
            let bytes = read(&dir.join(&manifest.corpus_file))?;
            manifest.verify(bytes)
        }
        None => {
            let bytes = read(path)?;
            Ok(VerifiedCorpus { sha256: sha256_hex(&bytes), bytes, manifest: None })
        }
    }
}

fn manifest_for(
    corpus: &[u8],
    corpus_id: &str,
    platform_version: Option<&str>,
    extractor_version: &str,
) -> CorpusManifest {
    CorpusManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        corpus_id: corpus_id.to_owned(),
        platform_version: platform_version.map(str::to_owned),
        extractor_version: extractor_version.to_owned(),
        corpus_file: DEFAULT_CORPUS_FILE_NAME.to_owned(),
        sha256: sha256_hex(corpus),
    }
}

/// A name no other writer — thread or process — uses at the same time.
fn unique_name(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{count}-{nanos}", std::process::id())
}

/// Writes the package files into a fresh staging directory next to `target`.
fn stage(
    target: &Path,
    corpus: &[u8],
    manifest: &CorpusManifest,
    extra: &[(&str, &[u8])],
) -> Result<PathBuf, String> {
    let io = |path: &Path, error: std::io::Error| format!("{}: {error}", path.display());
    let parent = target.parent().ok_or_else(|| format!("{} has no parent", target.display()))?;
    fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    let staging = parent.join(unique_name(".staging"));
    fs::create_dir(&staging).map_err(|error| io(&staging, error))?;
    let write = |name: &str, bytes: &[u8]| {
        let path = staging.join(name);
        fs::write(&path, bytes).map_err(|error| io(&path, error))
    };
    let result = write(DEFAULT_CORPUS_FILE_NAME, corpus)
        .and_then(|()| {
            write(
                MANIFEST_FILE_NAME,
                &serde_json::to_vec_pretty(manifest).expect("manifest serializes"),
            )
        })
        .and_then(|()| extra.iter().try_for_each(|(name, bytes)| write(name, bytes)));
    match result {
        Ok(()) => Ok(staging),
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

/// Writes a new package at `dir`, which must not exist yet: an explicit output
/// never replaces what is already there. Readers never see a half-written
/// package, since it appears in one rename.
pub fn write_package(
    dir: &Path,
    corpus: &[u8],
    corpus_id: &str,
    platform_version: Option<&str>,
    extractor_version: &str,
) -> Result<CorpusManifest, String> {
    write_package_with(dir, corpus, corpus_id, platform_version, extractor_version, &[])
}

/// [`write_package`] with extra files placed next to the corpus.
pub fn write_package_with(
    dir: &Path,
    corpus: &[u8],
    corpus_id: &str,
    platform_version: Option<&str>,
    extractor_version: &str,
    extra: &[(&str, &[u8])],
) -> Result<CorpusManifest, String> {
    if dir.exists() {
        return Err(format!("{} already exists; choose a new package directory", dir.display()));
    }
    let manifest = manifest_for(corpus, corpus_id, platform_version, extractor_version);
    let staging = stage(dir, corpus, &manifest, extra)?;
    if let Err(error) = fs::rename(&staging, dir) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("{}: {error}", dir.display()));
    }
    Ok(manifest)
}

/// A cache slot: packages named by the digest of everything they hold, and a
/// `current` pointer file naming the one that serves. Writers publish under an
/// exclusive lock of the slot and readers read under a shared one; the
/// operating system drops a lock with its process, so neither a crash nor a
/// slow reader can leave the slot inconsistent, and nothing is removed while a
/// reader is on it.
pub struct Slot {
    dir: PathBuf,
}

const CURRENT: &str = "current";
const LOCK: &str = "lock";
/// Counts publications; compared, not timed, so ordering does not depend on
/// file-system clocks or timestamp granularity.
const GENERATION: &str = "generation";

/// The current package of a slot, read whole while the slot is locked.
pub struct SlotPackage {
    pub corpus: VerifiedCorpus,
    extras: Vec<(String, Vec<u8>)>,
}

impl SlotPackage {
    pub fn extra(&self, name: &str) -> Option<&[u8]> {
        self.extras.iter().find(|(n, _)| n == name).map(|(_, bytes)| bytes.as_slice())
    }
}

impl Slot {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock_file(&self) -> Result<fs::File, String> {
        let io = |path: &Path, error: std::io::Error| format!("{}: {error}", path.display());
        fs::create_dir_all(&self.dir).map_err(|error| io(&self.dir, error))?;
        let path = self.dir.join(LOCK);
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| io(&path, error))
    }

    fn current_name(&self) -> Option<String> {
        let name = fs::read_to_string(self.dir.join(CURRENT)).ok()?;
        let name = name.trim();
        (name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())).then(|| name.to_owned())
    }

    /// The current package with the extra files named in `extras`.
    pub fn current(&self, extras: &[&str]) -> Result<SlotPackage, String> {
        if !self.dir.join(CURRENT).is_file() {
            return Err(format!("{} holds no package", self.dir.display()));
        }
        let lock = self.lock_file()?;
        lock.lock_shared().map_err(|error| format!("{}: {error}", self.dir.display()))?;
        let name = self
            .current_name()
            .ok_or_else(|| format!("{} holds no package", self.dir.display()))?;
        let package = self.dir.join(name);
        let corpus = read_local(&package)?;
        let extras = extras
            .iter()
            .filter_map(|extra| fs::read(package.join(extra)).ok().map(|b| (extra.to_string(), b)))
            .collect();
        Ok(SlotPackage { corpus, extras })
    }

    /// The publication count of the slot, to hand back to
    /// [`Self::publish_unless_superseded`].
    pub fn generation(&self) -> u64 {
        let Ok(lock) = self.lock_file() else { return 0 };
        if lock.lock_shared().is_err() {
            return 0;
        }
        self.read_generation()
    }

    fn read_generation(&self) -> u64 {
        fs::read_to_string(self.dir.join(GENERATION))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    /// The corpus of the current package.
    pub fn read(&self) -> Result<VerifiedCorpus, String> {
        self.current(&[]).map(|package| package.corpus)
    }

    /// Publishes a package, makes it current and removes every other package
    /// and any leftover of an interrupted writer. Returns the corpus as written.
    pub fn publish(
        &self,
        corpus: &[u8],
        corpus_id: &str,
        platform_version: Option<&str>,
        extractor_version: &str,
        extra: &[(&str, &[u8])],
    ) -> Result<VerifiedCorpus, String> {
        self.publish_unless_superseded(
            corpus,
            corpus_id,
            platform_version,
            extractor_version,
            extra,
            None,
        )
    }

    /// [`Self::publish`], except that a slot published again since it was at
    /// generation `observed` is left as it is: a slow writer that started earlier
    /// must not roll back a newer publication. The corpus as written by this
    /// call is returned either way.
    pub fn publish_unless_superseded(
        &self,
        corpus: &[u8],
        corpus_id: &str,
        platform_version: Option<&str>,
        extractor_version: &str,
        extra: &[(&str, &[u8])],
        observed: Option<u64>,
    ) -> Result<VerifiedCorpus, String> {
        let io = |path: &Path, error: std::io::Error| format!("{}: {error}", path.display());
        let manifest = manifest_for(corpus, corpus_id, platform_version, extractor_version);
        let name = publication_name(&manifest, extra);
        let package = self.dir.join(&name);
        let written = VerifiedCorpus {
            bytes: corpus.to_vec(),
            sha256: manifest.sha256.clone(),
            manifest: Some(manifest.clone()),
        };

        let lock = self.lock_file()?;
        lock.lock().map_err(|error| io(&self.dir, error))?;
        let generation = self.read_generation();
        if observed.is_some_and(|observed| observed != generation) {
            return Ok(written);
        }
        if read_local(&package).is_err() {
            let _ = fs::remove_dir_all(&package);
            let staging = stage(&package, corpus, &manifest, extra)?;
            fs::rename(&staging, &package).map_err(|error| {
                let _ = fs::remove_dir_all(&staging);
                io(&package, error)
            })?;
        }
        // The generation moves first: should the pointer then fail to change, a
        // slower writer still sees the slot as published again and leaves it
        // alone, so no failure between the two steps can lead to a rollback.
        let counter = self.dir.join(format!(".{GENERATION}.new"));
        fs::write(&counter, (generation + 1).to_string()).map_err(|error| io(&counter, error))?;
        fs::rename(&counter, self.dir.join(GENERATION)).map_err(|error| {
            let _ = fs::remove_file(&counter);
            io(&self.dir, error)
        })?;
        let pointer = self.dir.join(format!(".{CURRENT}.new"));
        fs::write(&pointer, &name).map_err(|error| io(&pointer, error))?;
        fs::rename(&pointer, self.dir.join(CURRENT)).map_err(|error| {
            let _ = fs::remove_file(&pointer);
            io(&self.dir, error)
        })?;
        // Readers and writers are excluded by the lock: nothing else is in use.
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let entry_name = entry.file_name();
                if entry_name == name.as_str()
                    || entry_name == CURRENT
                    || entry_name == LOCK
                    || entry_name == GENERATION
                {
                    continue;
                }
                let path = entry.path();
                let _ =
                    if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
            }
        }
        Ok(written)
    }
}

/// The package name: the digest of the whole manifest and of every extra file,
/// so a package of that name always holds exactly that content.
fn publication_name(manifest: &CorpusManifest, extra: &[(&str, &[u8])]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(manifest).expect("manifest serializes"));
    for (name, bytes) in extra {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_round_trips_and_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("pkg");
        let manifest =
            write_package(&package, b"{\"types\":[]}", "fixture", Some("8.3.27.1"), "test")
                .unwrap();
        let read = read_local(&package).unwrap();
        assert_eq!(read.bytes, b"{\"types\":[]}");
        assert_eq!(read.manifest.as_ref(), Some(&manifest));
        assert_eq!(read_local(&package.join(MANIFEST_FILE_NAME)).unwrap().sha256, manifest.sha256);

        fs::write(package.join(DEFAULT_CORPUS_FILE_NAME), b"{\"types\":[1]}").unwrap();
        assert!(read_local(&package).unwrap_err().contains("SHA-256"));
    }

    #[test]
    fn explicit_output_never_replaces_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let taken = dir.path().join("taken");
        fs::create_dir_all(&taken).unwrap();
        fs::write(taken.join("keep.txt"), b"keep").unwrap();
        assert!(write_package(&taken, b"{}", "x", None, "t")
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(fs::read(taken.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn slot_serves_the_latest_publication_and_concurrent_writers_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let slot = std::sync::Arc::new(Slot::new(dir.path().join("slot")));
        assert!(slot.read().is_err());
        slot.publish(b"{\"a\":1}", "a", None, "t", &[("source.json", b"one")]).unwrap();
        assert_eq!(slot.read().unwrap().bytes, b"{\"a\":1}");
        assert_eq!(slot.current(&["source.json"]).unwrap().extra("source.json"), Some(&b"one"[..]));
        // The same corpus with another extra file or manifest is another package.
        slot.publish(b"{\"a\":1}", "a", None, "t", &[("source.json", b"two")]).unwrap();
        assert_eq!(slot.current(&["source.json"]).unwrap().extra("source.json"), Some(&b"two"[..]));
        slot.publish(b"{\"a\":1}", "a", Some("8.3.2"), "t", &[]).unwrap();
        let manifest = slot.read().unwrap().manifest.unwrap();
        assert_eq!(manifest.platform_version.as_deref(), Some("8.3.2"));

        let writers: Vec<_> = (0..8)
            .map(|i| {
                let slot = slot.clone();
                std::thread::spawn(move || {
                    let corpus = format!("{{\"w\":{}}}", i % 2);
                    slot.publish(corpus.as_bytes(), "w", None, "t", &[]).map(|c| c.bytes)
                })
            })
            .collect();
        let readers: Vec<_> = (0..8)
            .map(|_| {
                let slot = slot.clone();
                std::thread::spawn(move || (0..20).all(|_| slot.read().is_ok()))
            })
            .collect();
        for (i, writer) in writers.into_iter().enumerate() {
            let written = writer.join().unwrap().expect("every concurrent publication succeeds");
            assert_eq!(written, format!("{{\"w\":{}}}", i % 2).into_bytes());
        }
        for reader in readers {
            assert!(reader.join().unwrap(), "a reader never meets a removed package");
        }
        let current = slot.read().unwrap().bytes;
        assert!(current == b"{\"w\":0}" || current == b"{\"w\":1}");
        let entries = fs::read_dir(slot.dir()).unwrap().count();
        assert_eq!(entries, 4, "only the lock, the pointer, the counter and the package remain");
    }

    #[test]
    fn a_publication_whose_generation_cannot_move_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let slot = Slot::new(dir.path().join("slot"));
        slot.publish(b"{\"v\":1}", "v", None, "t", &[]).unwrap();
        // The generation file cannot be replaced: a directory stands in its way.
        fs::remove_file(slot.dir().join(GENERATION)).unwrap();
        fs::create_dir(slot.dir().join(GENERATION)).unwrap();
        let observed = slot.generation();
        assert!(slot
            .publish_unless_superseded(b"{\"v\":2}", "v", None, "t", &[], Some(observed))
            .is_err());
        assert_eq!(slot.read().unwrap().bytes, b"{\"v\":1}", "the pointer did not move either");
    }

    #[test]
    fn publish_removes_superseded_packages_and_crash_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let slot = Slot::new(dir.path().join("slot"));
        slot.publish(b"{\"v\":0}", "v", None, "t", &[]).unwrap();
        let first = fs::read_to_string(slot.dir().join(CURRENT)).unwrap();
        fs::create_dir_all(slot.dir().join(".staging-crashed")).unwrap();
        slot.publish(b"{\"v\":1}", "v", None, "t", &[]).unwrap();
        assert!(!slot.dir().join(first).exists());
        assert!(!slot.dir().join(".staging-crashed").exists());
        assert_eq!(slot.read().unwrap().bytes, b"{\"v\":1}");
    }

    #[test]
    fn manifest_rejects_unknown_schema_and_escaping_corpus_file() {
        let base = |schema: u32, file: &str| {
            format!(
                r#"{{"schema_version":{schema},"corpus_id":"x","extractor_version":"1","corpus_file":"{file}","sha256":"{}"}}"#,
                "0".repeat(64)
            )
        };
        assert!(CorpusManifest::parse(base(1, "platform_data.json").as_bytes()).is_ok());
        assert!(CorpusManifest::parse(base(2, "platform_data.json").as_bytes())
            .unwrap_err()
            .contains("schema_version"));
        for escaping in
            ["../x.json", "/etc/passwd", "a/b.json", "..", "a\\\\..\\\\b.json", "x?y", "%2e%2e"]
        {
            assert!(CorpusManifest::parse(base(1, escaping).as_bytes()).is_err(), "{escaping}");
        }
    }

    #[test]
    fn directory_without_manifest_is_refused_and_legacy_json_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_local(dir.path()).unwrap_err().contains(MANIFEST_FILE_NAME));
        let legacy = dir.path().join("corpus.json");
        fs::write(&legacy, b"{}").unwrap();
        let read = read_local(&legacy).unwrap();
        assert!(read.manifest.is_none());
        assert_eq!(read.sha256, sha256_hex(b"{}"));
    }
}
