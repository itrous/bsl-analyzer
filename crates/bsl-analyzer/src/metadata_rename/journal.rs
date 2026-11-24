//! Durable file operations for an already prepared metadata rename plan.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use ide::metadata_rename_plan::{
    render_metadata_edits, validate_metadata_plan_snapshot, validate_metadata_resource_rename,
    MetadataDirectoryEntryKind, MetadataPlanRoot, MetadataRenamePlan, MetadataResourceRename,
    MetadataRootRole, PreparedMetadataRenameSnapshot,
};
use project_model::{ManagedBaselineDirectory, ManagedDirectoryEntryKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    pub record_bytes: u64,
    pub blob_bytes: u64,
    pub total_blob_bytes: u64,
    pub entries: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Committed,
    RolledBack,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub transaction: String,
    pub status: Status,
    pub reason: Option<String>,
    #[serde(skip)]
    pub known_files: Vec<KnownFile>,
    #[serde(skip)]
    pub resource_paths: Vec<MetadataResourceRename>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KnownFile {
    pub root_id: String,
    pub old_path: PathBuf,
    pub current_path: PathBuf,
    pub hash: [u8; 32],
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingJournal {
    pub transaction: String,
    pub recovery_approved: bool,
    pub publication_only: bool,
}

#[derive(Debug)]
pub(crate) struct JournalStore {
    authority: PathBuf,
    bounds: Bounds,
}

struct Root {
    approved: MetadataPlanRoot,
    directory: ManagedBaselineDirectory,
    identity: Option<(u64, u64)>,
}

/// The common cache authority must be shared by all participating source-root writers.
pub(crate) struct RootLocks {
    cache: ManagedBaselineDirectory,
    roots: BTreeMap<String, Root>,
    bounds: Bounds,
    _locks: Vec<HeldFileLock>,
    defer_publication: bool,
}

struct HeldFileLock(File);

impl HeldFileLock {
    fn acquire(file: File) -> Result<Self> {
        file.try_lock()?;
        Ok(Self(file))
    }
}

impl Drop for HeldFileLock {
    fn drop(&mut self) {
        // A forked child can retain a duplicate descriptor after this handle closes.
        let _ = self.0.unlock();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootBinding {
    id: String,
    source_path: String,
    canonical_path: String,
    role: String,
    writable: bool,
    case_sensitive: bool,
    identity: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    hash: [u8; 32],
    len: u64,
    // Decimal timestamps avoid Serde's unsupported u128 inside tagged operations.
    modified_nanos: Option<String>,
    created_nanos: Option<String>,
    identity: Option<(u64, u64)>,
    mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    root: String,
    path: String,
    version: Option<Version>,
}

type Tree = BTreeMap<(String, String), Option<Version>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Blob {
    hash: [u8; 32],
    len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Text {
        root: String,
        path: String,
        before: Blob,
        after: Blob,
        installed: Option<Box<Version>>,
        restored: Option<Box<Version>>,
    },
    Move {
        root: String,
        from: String,
        to: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    step: usize,
    undo: bool,
    staged: Option<Version>,
    temporary: Option<String>,
    writing: bool,
    contested: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageBoundary {
    Declared,
    CreatedUncheckpointed,
    Created,
    Written,
    Ready,
    Installed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    transaction: String,
    context: [u8; 32],
    roots: Vec<RootBinding>,
    nodes: Vec<Node>,
    observed: Vec<Node>,
    steps: Vec<Step>,
    applied: usize,
    intent: Option<Intent>,
    terminal: Option<Receipt>,
    #[serde(default)]
    publication_pending: bool,
}

impl Record {
    fn pending(&self) -> bool {
        self.publication_pending
            || self
                .terminal
                .as_ref()
                .is_none_or(|receipt| receipt.status == Status::RecoveryRequired)
    }

    fn publication_only(&self) -> bool {
        self.publication_pending
            && self
                .terminal
                .as_ref()
                .is_some_and(|receipt| receipt.status != Status::RecoveryRequired)
    }
}

impl JournalStore {
    /// Opens an existing cache authority without creating anything below source roots.
    pub(crate) fn open(common_cache: &Path, bounds: Bounds) -> Result<Self> {
        ensure!(
            bounds.record_bytes > 0
                && bounds.record_bytes < u64::MAX
                && bounds.blob_bytes > 0
                && bounds.blob_bytes < u64::MAX
                && bounds.entries > 0
        );
        ensure!(bounds.total_blob_bytes >= bounds.blob_bytes);
        Ok(Self { authority: std::fs::canonicalize(common_cache)?, bounds })
    }

    pub(crate) fn lock_roots(&self, approved: &[MetadataPlanRoot]) -> Result<RootLocks> {
        ensure!(!approved.is_empty() && approved.len() <= self.bounds.entries);
        let mut ordered = approved.to_vec();
        ordered.sort_by(|a, b| a.canonical_path.cmp(&b.canonical_path));
        let mut ids = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for root in &ordered {
            ensure!(ids.insert(root.id.clone()) && paths.insert(root.canonical_path.clone()));
            ensure!(!root.id.is_empty() && root.id.len() <= 256);
            ensure!(root.source_path.is_absolute() && root.canonical_path.is_absolute());
            ensure!(root.source_path.to_str().is_some() && root.canonical_path.to_str().is_some());
            ensure!(
                !self.authority.starts_with(&root.canonical_path),
                "journal cache is in source"
            );
            verify_binding(root)?;
        }
        let cache = ManagedBaselineDirectory::open(&self.authority, "metadata-rename", true)?;
        for name in ["objects", "journals", "locks"] {
            ManagedBaselineDirectory::open(
                &self.authority,
                &format!("metadata-rename/{name}"),
                true,
            )?;
        }
        cache.sync_all()?;
        ManagedBaselineDirectory::open_project_root(&self.authority)?.sync_all()?;
        let mut locks = Vec::new();
        // Ancestor roots and physical aliases must share the same participant exclusion.
        let participant =
            HeldFileLock::acquire(cache.open_or_create_file("locks/participants.lock")?)
                .context("metadata rename participant is already active")?;
        locks.push(participant);
        let mut roots = BTreeMap::new();
        for approved in ordered {
            let key = hex(&fingerprint(approved.canonical_path.to_str().unwrap().as_bytes()));
            let lock =
                HeldFileLock::acquire(cache.open_or_create_file(&format!("locks/{key}.lock"))?)
                    .context("metadata source root is already held by another participant")?;
            locks.push(lock);
            verify_binding(&approved)?;
            let identity = root_identity(&approved.canonical_path)?;
            let directory = ManagedBaselineDirectory::open_project_root(&approved.canonical_path)?;
            ensure!(identity == root_identity(&approved.canonical_path)?);
            roots.insert(approved.id.clone(), Root { approved, directory, identity });
        }
        Ok(RootLocks { cache, roots, bounds: self.bounds, _locks: locks, defer_publication: false })
    }

    #[cfg(test)]
    pub(crate) fn recover(
        &self,
        transaction: &str,
        approved: &[MetadataPlanRoot],
        context: [u8; 32],
    ) -> Result<Receipt> {
        self.lock_roots(approved)?.recover(transaction, context)
    }

    pub(crate) fn recover_guarded(
        &self,
        transaction: &str,
        approved: &[MetadataPlanRoot],
        context: [u8; 32],
        before_first_source_write: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Receipt> {
        let mut locks = self.lock_roots(approved)?;
        locks.defer_publication();
        locks.recover_guarded(transaction, context, before_first_source_write)
    }

    pub(crate) fn preview_recovery(
        &self,
        transaction: &str,
        approved: &[MetadataPlanRoot],
        context: [u8; 32],
    ) -> Result<Receipt> {
        let locks = self.lock_roots(approved)?;
        let (mut record, _) = locks.load(transaction)?;
        ensure!(
            record.context == context,
            "live project context does not approve recovery preview"
        );
        if record.publication_only() {
            locks.check_tree(&record)?;
            return locks.with_known_versions(&record, record.terminal.clone().unwrap());
        }
        record.applied = 0;
        record.intent = None;
        locks.with_known_versions(
            &record,
            Receipt {
                transaction: transaction.into(),
                status: Status::RolledBack,
                reason: None,
                known_files: Vec::new(),
                resource_paths: Vec::new(),
            },
        )
    }

    pub(crate) fn pending(
        &self,
        approved: &[MetadataPlanRoot],
        context: [u8; 32],
    ) -> Result<Vec<PendingJournal>> {
        let locks = self.lock_roots(approved)?;
        let mut pending = Vec::new();
        for transaction in locks.journal_ids()? {
            let record = locks.read_record(&transaction)?;
            if !locks.overlaps(&record) || !record.pending() {
                continue;
            }
            pending.push(PendingJournal {
                publication_only: record.publication_only(),
                recovery_approved: record.context == context
                    && locks.validate_record(&record, &transaction).is_ok(),
                transaction,
            });
        }
        Ok(pending)
    }

    /// Clears publication debt only after a terminal model publication has succeeded.
    pub(crate) fn complete_publication(
        &self,
        transaction: &str,
        approved: &[MetadataPlanRoot],
        context: [u8; 32],
    ) -> Result<()> {
        let locks = self.lock_roots(approved)?;
        let (mut record, hash) = locks.load(transaction)?;
        ensure!(record.context == context, "live project context does not approve publication");
        ensure!(
            record
                .terminal
                .as_ref()
                .is_some_and(|receipt| receipt.status != Status::RecoveryRequired),
            "journal has no terminal outcome to publish"
        );
        locks.check_tree(&record)?;
        if record.publication_pending {
            record.publication_pending = false;
            let cleared_hash = fingerprint(&serde_json::to_vec(&record)?);
            if let Err(error) = locks.save(&record, Some(hash)) {
                // A failed parent-directory barrier can follow the atomic replacement.
                // Restore visible debt before propagating a failed completion.
                if let Ok((mut observed, current)) = locks.load(transaction) {
                    if current == cleared_hash && !observed.publication_pending {
                        observed.publication_pending = true;
                        if let Err(restore) = locks.save(&observed, Some(current)) {
                            return Err(error.context(format!(
                                "publication debt restoration failed: {restore}"
                            )));
                        }
                    }
                }
                return Err(error);
            }
        }
        Ok(())
    }
}

impl RootLocks {
    fn bindings(&self) -> Result<Vec<RootBinding>> {
        self.roots
            .values()
            .map(|root| {
                let approved = &root.approved;
                Ok(RootBinding {
                    id: approved.id.clone(),
                    source_path: approved
                        .source_path
                        .to_str()
                        .context("non-Unicode source root")?
                        .into(),
                    canonical_path: approved
                        .canonical_path
                        .to_str()
                        .context("non-Unicode root")?
                        .into(),
                    role: match approved.role {
                        MetadataRootRole::Main => "main",
                        MetadataRootRole::Extension => "extension",
                        MetadataRootRole::Dependency => "dependency",
                        MetadataRootRole::External => "external",
                    }
                    .into(),
                    writable: approved.writable,
                    case_sensitive: approved.case_sensitive,
                    identity: root.identity,
                })
            })
            .collect()
    }

    fn verify_roots(&self) -> Result<()> {
        for root in self.roots.values() {
            verify_binding(&root.approved)?;
            ensure!(
                root.identity == root_identity(&root.approved.canonical_path)?,
                "source root identity changed"
            );
        }
        Ok(())
    }

    fn directory(&self, root: &str) -> Result<&ManagedBaselineDirectory> {
        Ok(&self.roots.get(root).context("unapproved source root")?.directory)
    }

    fn writable(&self, root: &str) -> Result<()> {
        ensure!(
            self.roots.get(root).context("unapproved source root")?.approved.writable,
            "read-only source root"
        );
        Ok(())
    }

    fn read_version(&self, root: &str, path: &str) -> Result<Version> {
        valid_path(path, false)?;
        let directory = self.directory(root)?;
        let mut file = directory.open_file(path)?;
        let metadata = file.metadata()?;
        ensure!(metadata.len() <= self.bounds.blob_bytes, "source input exceeds byte bound");
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(self.bounds.blob_bytes + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= self.bounds.blob_bytes);
        let reading = directory.entry_reading(path)?;
        ensure!(reading.is_regular_file && reading.len == bytes.len() as u64);
        ensure!(metadata.len() == reading.len && file_identity(&metadata) == reading.identity);
        Ok(Version {
            hash: fingerprint(&bytes),
            len: reading.len,
            modified_nanos: reading.modified_nanos.map(|n| n.to_string()),
            created_nanos: metadata
                .created()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos().to_string()),
            identity: reading.identity,
            mode: reading.mode,
        })
    }

    fn current_tree(&self) -> Result<Tree> {
        self.verify_roots()?;
        let mut tree = Tree::new();
        for id in self.roots.keys() {
            let mut pending = vec![String::new()];
            while let Some(parent) = pending.pop() {
                ensure!(tree.len() < self.bounds.entries, "source namespace exceeds entry bound");
                tree.insert((id.clone(), parent.clone()), None);
                for entry in self.directory(id)?.read_directory(&parent)? {
                    ensure!(
                        tree.len() + pending.len() < self.bounds.entries,
                        "source namespace exceeds entry bound"
                    );
                    let name = entry
                        .name
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("non-Unicode source entry"))?;
                    valid_path(&name, false)?;
                    ensure!(Path::new(&name).components().count() == 1);
                    let path = if parent.is_empty() { name } else { format!("{parent}/{name}") };
                    match entry.kind {
                        ManagedDirectoryEntryKind::Directory => pending.push(path),
                        ManagedDirectoryEntryKind::File => {
                            tree.insert(
                                (id.clone(), path.clone()),
                                Some(self.read_version(id, &path)?),
                            );
                        }
                        _ => bail!("symlink or special source entry refused"),
                    }
                }
            }
        }
        ensure!(tree.len() <= self.bounds.entries);
        Ok(tree)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<Blob> {
        ensure!(bytes.len() as u64 <= self.bounds.blob_bytes);
        let blob = Blob { hash: fingerprint(bytes), len: bytes.len() as u64 };
        let path = format!("objects/{}", hex(&blob.hash));
        if let Ok(file) = self.cache.open_file(&path) {
            private_cache_file(&file)?;
            drop(file);
            ensure!(self.read_blob(&blob)? == bytes, "existing content-addressed object changed");
            return Ok(blob);
        }
        let objects = self.cache.read_directory("objects")?;
        ensure!(objects.len() < self.bounds.entries, "journal object count bound exceeded");
        let mut total = bytes.len() as u64;
        for object in objects {
            ensure!(object.kind == ManagedDirectoryEntryKind::File);
            let name =
                object.name.into_string().map_err(|_| anyhow::anyhow!("invalid object name"))?;
            ensure!(name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()));
            let len = self.cache.open_file(&format!("objects/{name}"))?.metadata()?.len();
            ensure!(len <= self.bounds.blob_bytes);
            total = total.checked_add(len).context("journal object budget overflow")?;
        }
        ensure!(total <= self.bounds.total_blob_bytes, "journal object byte budget exceeded");
        match self.cache.create_file_new(&path) {
            Ok(mut file) => {
                private_cache_file(&file)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                self.cache.sync_parent(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    self.read_blob(&blob)? == bytes,
                    "existing content-addressed object changed"
                );
            }
            Err(error) => return Err(error.into()),
        }
        Ok(blob)
    }

    fn read_blob(&self, blob: &Blob) -> Result<Vec<u8>> {
        ensure!(blob.len <= self.bounds.blob_bytes);
        let mut file = self.cache.open_file(&format!("objects/{}", hex(&blob.hash)))?;
        private_cache_file(&file)?;
        ensure!(file.metadata()?.len() == blob.len);
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(blob.len + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == blob.len && fingerprint(&bytes) == blob.hash,
            "journal object hash mismatch"
        );
        Ok(bytes)
    }

    fn record_bytes(&self, transaction: &str) -> Result<Vec<u8>> {
        valid_id(transaction)?;
        let mut file = self.cache.open_file(&format!("journals/{transaction}.json"))?;
        private_cache_file(&file)?;
        ensure!(
            file.metadata()?.len() <= self.bounds.record_bytes,
            "journal record exceeds byte bound"
        );
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(self.bounds.record_bytes + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= self.bounds.record_bytes);
        Ok(bytes)
    }

    fn read_record(&self, transaction: &str) -> Result<Record> {
        let record: Record = serde_json::from_slice(&self.record_bytes(transaction)?)?;
        ensure!(record.schema == 1 && record.transaction == transaction);
        Ok(record)
    }

    fn load(&self, transaction: &str) -> Result<(Record, [u8; 32])> {
        let bytes = self.record_bytes(transaction)?;
        let record: Record = serde_json::from_slice(&bytes)?;
        self.validate_record(&record, transaction)?;
        Ok((record, fingerprint(&bytes)))
    }

    fn save(&self, record: &Record, previous: Option<[u8; 32]>) -> Result<[u8; 32]> {
        self.validate_record(record, &record.transaction)?;
        let bytes = serde_json::to_vec(record)?;
        ensure!(bytes.len() as u64 <= self.bounds.record_bytes);
        let destination = format!("journals/{}.json", record.transaction);
        if let Some(expected) = previous {
            let (_, actual) = self.load(&record.transaction)?;
            ensure!(actual == expected, "journal record changed by another writer");
            let temporary = format!("journals/{}.{}.tmp", record.transaction, uuid::Uuid::new_v4());
            let mut file = self.cache.create_file_new_in_existing_directory(&temporary)?;
            private_cache_file(&file)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            self.cache.replace_file_in_existing_directory(&temporary, &destination)?;
        } else {
            let mut file = self.cache.create_file_new_in_existing_directory(&destination)?;
            private_cache_file(&file)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        self.cache.sync_parent(&destination)?;
        Ok(fingerprint(&bytes))
    }
}

fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    bsl_metadata::rename_xml::metadata_source_fingerprint(bytes)
}

fn private_cache_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

fn valid_id(id: &str) -> Result<()> {
    ensure!(uuid::Uuid::parse_str(id)?.hyphenated().to_string() == id, "invalid transaction id");
    Ok(())
}

fn valid_path(path: &str, root: bool) -> Result<()> {
    ensure!(path.len() <= 4096 && (root || !path.is_empty()));
    ensure!(!path.contains(['\\', ':', '\0']));
    ensure!(
        path.is_empty()
            || path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
    );
    Ok(())
}

fn verify_binding(root: &MetadataPlanRoot) -> Result<()> {
    ensure!(
        std::fs::canonicalize(&root.source_path)? == root.canonical_path,
        "source root binding changed"
    );
    ensure!(std::fs::canonicalize(&root.canonical_path)? == root.canonical_path);
    Ok(())
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn root_identity(path: &Path) -> Result<Option<(u64, u64)>> {
    let metadata = std::fs::metadata(path)?;
    ensure!(metadata.is_dir());
    Ok(file_identity(&metadata))
}

impl RootLocks {
    fn validate_record(&self, record: &Record, transaction: &str) -> Result<()> {
        valid_id(transaction)?;
        ensure!(record.schema == 1 && record.transaction == transaction);
        ensure!(
            record.roots == self.bindings()?,
            "journal roots are not approved by the live project"
        );
        ensure!(
            record.nodes.len() <= self.bounds.entries
                && record.steps.len() <= self.bounds.entries
                && record.observed.len() <= self.bounds.entries
        );
        ensure!(record.applied <= record.steps.len());
        let mut tree = Tree::new();
        for node in &record.nodes {
            self.directory(&node.root)?;
            valid_path(&node.path, true)?;
            ensure!(
                tree.insert((node.root.clone(), node.path.clone()), node.version.clone()).is_none(),
                "duplicate journal namespace entry"
            );
        }
        for ((root, path), version) in &tree {
            if path.is_empty() {
                ensure!(version.is_none());
                continue;
            }
            let parent = parent(path);
            ensure!(
                tree.get(&(root.clone(), parent.to_owned())) == Some(&None),
                "journal namespace has no parent"
            );
            if let Some(version) = version {
                ensure!(version.len <= self.bounds.blob_bytes);
            }
        }
        for root in self.roots.keys() {
            ensure!(tree.get(&(root.clone(), String::new())) == Some(&None));
        }
        let mut total = 0u64;
        for node in record.nodes.iter().chain(&record.observed) {
            self.directory(&node.root)?;
            valid_path(&node.path, true)?;
            if let Some(version) = &node.version {
                ensure!(version.len <= self.bounds.blob_bytes);
                total = total.checked_add(version.len).context("journal blob size overflow")?;
            }
        }
        let mut edited = BTreeSet::new();
        let mut moved = false;
        for step in &record.steps {
            match step {
                Step::Text { root, path, before, after, installed, restored } => {
                    self.writable(root)?;
                    valid_path(path, false)?;
                    ensure!(
                        !moved && edited.insert((root.clone(), path.clone())),
                        "text operations must precede namespace operations"
                    );
                    ensure!(tree.get(&(root.clone(), path.clone())).is_some_and(Option::is_some));
                    let original =
                        tree.get(&(root.clone(), path.clone())).and_then(Option::as_ref).unwrap();
                    ensure!(
                        original.hash == before.hash && original.len == before.len,
                        "journal original bytes do not match its input"
                    );
                    for blob in [before, after] {
                        ensure!(blob.len <= self.bounds.blob_bytes);
                    }
                    total = total.checked_add(after.len).context("journal blob size overflow")?;
                    ensure!(total <= self.bounds.total_blob_bytes);
                    if let Some(version) = installed {
                        ensure!(version.hash == after.hash && version.len == after.len);
                    }
                    if let Some(version) = restored {
                        ensure!(version.hash == before.hash && version.len == before.len);
                    }
                }
                Step::Move { root, from, to } => {
                    moved = true;
                    self.writable(root)?;
                    valid_path(from, false)?;
                    valid_path(to, false)?;
                    ensure!(from != to && parent(from) == parent(to));
                    move_tree(&mut tree, root, from, to)?;
                }
            }
        }
        ensure!(total <= self.bounds.total_blob_bytes);
        if let Some(intent) = &record.intent {
            ensure!(intent.step < record.steps.len());
            ensure!(if intent.undo {
                record.applied == intent.step + 1
            } else {
                record.applied == intent.step
            });
            match &record.steps[intent.step] {
                Step::Text { path, before, after, .. } => {
                    ensure!(
                        intent.temporary.as_deref()
                            == Some(
                                temporary_path(record, intent.step, path, intent.undo).as_str()
                            )
                    );
                    let blob = if intent.undo { before } else { after };
                    if let Some(staged) = &intent.staged {
                        if intent.writing {
                            ensure!(
                                staged.hash == fingerprint(b"")
                                    && staged.len == 0
                                    && !intent.contested
                            );
                        } else {
                            ensure!(
                                staged.hash == blob.hash
                                    && staged.len == blob.len
                                    && !intent.contested
                            );
                        }
                    } else {
                        ensure!(!intent.writing);
                    }
                }
                Step::Move { .. } => ensure!(
                    intent.staged.is_none()
                        && intent.temporary.is_none()
                        && !intent.writing
                        && !intent.contested
                ),
            }
        }
        if let Some(receipt) = &record.terminal {
            ensure!(
                receipt.transaction == transaction
                    && receipt.reason.as_ref().is_none_or(|reason| reason.len() <= 4096)
            );
            if receipt.status == Status::Committed {
                ensure!(record.applied == record.steps.len() && record.intent.is_none());
            }
            if receipt.status == Status::RolledBack {
                ensure!(record.applied == 0 && record.intent.is_none());
            }
        }
        Ok(())
    }

    fn expected_tree(&self, record: &Record) -> Result<Tree> {
        let mut tree: Tree = record
            .nodes
            .iter()
            .map(|node| ((node.root.clone(), node.path.clone()), node.version.clone()))
            .collect();
        for (index, step) in record.steps.iter().enumerate() {
            match step {
                Step::Text { root, path, installed, restored, .. } => {
                    let version = if index < record.applied {
                        installed.as_deref()
                    } else {
                        restored.as_deref()
                    };
                    if let Some(version) = version {
                        tree.insert((root.clone(), path.clone()), Some(version.clone()));
                    } else {
                        ensure!(index >= record.applied, "applied text has no version proof");
                    }
                }
                Step::Move { root, from, to } if index < record.applied => {
                    move_tree(&mut tree, root, from, to)?
                }
                Step::Move { .. } => {}
            }
        }
        Ok(tree)
    }

    fn check_tree(&self, record: &Record) -> Result<()> {
        ensure!(
            self.current_tree()? == self.expected_tree(record)?,
            "source version or namespace changed"
        );
        Ok(())
    }

    fn journal_ids(&self) -> Result<Vec<String>> {
        let entries = self.cache.read_directory("journals")?;
        ensure!(entries.len() <= self.bounds.entries);
        let mut transactions = Vec::new();
        for entry in entries {
            let name = entry
                .name
                .into_string()
                .map_err(|_| anyhow::anyhow!("invalid journal filename"))?;
            if !name.ends_with(".json") {
                continue;
            }
            ensure!(entry.kind == ManagedDirectoryEntryKind::File);
            let transaction = name.strip_suffix(".json").unwrap();
            valid_id(transaction)?;
            transactions.push(transaction.to_owned());
        }
        Ok(transactions)
    }

    fn overlaps(&self, record: &Record) -> bool {
        record.roots.iter().any(|root| {
            self.roots.values().any(|approved| {
                let stored = Path::new(&root.canonical_path);
                stored.starts_with(&approved.approved.canonical_path)
                    || approved.approved.canonical_path.starts_with(stored)
            })
        })
    }

    fn pending_other(&self, excluded: Option<&str>) -> Result<()> {
        for transaction in self.journal_ids()? {
            if excluded == Some(transaction.as_str()) {
                continue;
            }
            let record = self.read_record(&transaction)?;
            if !record.pending() {
                continue;
            }
            if self.overlaps(&record) {
                bail!(
                    "pending journal {} requires recovery before another operation",
                    record.transaction
                );
            }
        }
        Ok(())
    }

    fn prepare(
        &self,
        plan: &MetadataRenamePlan,
        snapshot: &PreparedMetadataRenameSnapshot,
    ) -> Result<Record> {
        ensure!(plan.can_apply(), "metadata rename plan is incomplete");
        ensure!(
            validate_metadata_plan_snapshot(plan, snapshot).is_empty(),
            "metadata plan snapshot is stale"
        );
        ensure!(self.roots.len() == plan.roots.len());
        for approved in &plan.roots {
            ensure!(self.roots.get(&approved.id).is_some_and(|root| root.approved == *approved));
        }
        let tree = self.current_tree()?;
        ensure!(snapshot.files.len() + snapshot.directories.len() == tree.len());
        for input in &snapshot.files {
            let path = portable(&input.relative_path)?;
            let actual = tree
                .get(&(input.root_id.clone(), path))
                .and_then(Option::as_ref)
                .context("missing prepared file")?;
            ensure!(actual.hash == fingerprint(&input.bytes), "unchanged read input changed");
        }
        for directory in &snapshot.directories {
            let path = portable(&directory.relative_path)?;
            ensure!(tree.get(&(directory.root_id.clone(), path.clone())) == Some(&None));
            let mut expected: Vec<_> = directory
                .entries
                .iter()
                .map(|entry| {
                    (entry.name.clone(), entry.kind == MetadataDirectoryEntryKind::Directory)
                })
                .collect();
            let mut actual: Vec<_> = tree
                .iter()
                .filter(|((root, child), _)| {
                    root == &directory.root_id && !child.is_empty() && parent(child) == path
                })
                .map(|((_, child), version)| (filename(child).to_owned(), version.is_none()))
                .collect();
            expected.sort();
            actual.sort();
            ensure!(expected == actual, "source directory changed");
        }
        let transaction = uuid::Uuid::new_v4().hyphenated().to_string();
        let mut steps = Vec::new();
        let mut seen = BTreeSet::new();
        let mut total = 0u64;
        for input in &snapshot.files {
            total = total.checked_add(input.bytes.len() as u64).context("blob bounds overflow")?;
            ensure!(total <= self.bounds.total_blob_bytes);
            self.put_blob(&input.bytes)?;
        }
        for file in &plan.files {
            self.writable(&file.root_id)?;
            let path = portable(&file.relative_path)?;
            ensure!(seen.insert((file.root_id.clone(), path.clone())));
            let input = snapshot
                .files
                .iter()
                .find(|input| {
                    input.root_id == file.root_id && input.relative_path == file.relative_path
                })
                .context("edit not in full read set")?;
            ensure!(
                input.kind != ide::metadata_rename_plan::MetadataPlanFileKind::ProjectInput,
                "project context inputs cannot be edited by metadata rename"
            );
            ensure!(fingerprint(&input.bytes) == file.before_sha256);
            let mut edits = file.edits.clone();
            ensure!(
                render_metadata_edits(&input.bytes, &mut edits)
                    .map_err(|_| anyhow::anyhow!("invalid original byte edits"))?
                    == file.after_bytes
            );
            let handle = self.directory(&file.root_id)?.open_file(&path)?;
            ensure!(
                !handle.metadata()?.permissions().readonly(),
                "planned source file is read-only"
            );
            total =
                total.checked_add(file.after_bytes.len() as u64).context("blob bounds overflow")?;
            ensure!(total <= self.bounds.total_blob_bytes);
            steps.push(Step::Text {
                root: file.root_id.clone(),
                path,
                before: self.put_blob(&input.bytes)?,
                after: self.put_blob(&file.after_bytes)?,
                installed: None,
                restored: None,
            });
        }
        for (index, operation) in plan.resources.iter().enumerate() {
            validate_metadata_resource_rename(snapshot, operation)
                .map_err(|_| anyhow::anyhow!("unsupported resource rename"))?;
            let from = portable(&operation.from_relative)?;
            let to = portable(&operation.to_relative)?;
            let middle = sibling(&from, &format!(".bsl-rename-{transaction}-{index}.move"));
            ensure!(!tree.contains_key(&(operation.root_id.clone(), middle.clone())));
            steps.push(Step::Move { root: operation.root_id.clone(), from, to: middle.clone() });
            steps.push(Step::Move { root: operation.root_id.clone(), from: middle, to });
        }
        let record = Record {
            schema: 1,
            transaction,
            context: plan.context_sha256,
            roots: self.bindings()?,
            nodes: tree
                .into_iter()
                .map(|((root, path), version)| Node { root, path, version })
                .collect(),
            observed: Vec::new(),
            steps,
            applied: 0,
            intent: None,
            terminal: None,
            publication_pending: self.defer_publication,
        };
        self.validate_record(&record, &record.transaction)?;
        Ok(record)
    }

    fn with_known_versions(&self, record: &Record, mut receipt: Receipt) -> Result<Receipt> {
        let tree = self.expected_tree(record)?;
        for node in &record.nodes {
            if node.version.is_none() {
                continue;
            }
            let mut current = node.path.clone();
            for step in record.steps.iter().take(record.applied) {
                if let Step::Move { root, from, to } = step {
                    if *root == node.root && under(&current, from) {
                        current = format!("{to}{}", &current[from.len()..]);
                    }
                }
            }
            let version = tree
                .get(&(node.root.clone(), current.clone()))
                .and_then(Option::as_ref)
                .context("terminal file has no known version")?;
            let bytes = self.read_blob(&Blob { hash: version.hash, len: version.len })?;
            receipt.known_files.push(KnownFile {
                root_id: node.root.clone(),
                old_path: PathBuf::from(&node.path),
                current_path: PathBuf::from(current),
                hash: version.hash,
                bytes: Arc::from(bytes),
            });
        }
        for pair in record
            .steps
            .iter()
            .take(record.applied)
            .filter(|s| matches!(s, Step::Move { .. }))
            .collect::<Vec<_>>()
            .chunks(2)
        {
            let [Step::Move { root, from, to: middle }, Step::Move { root: second_root, from: second_middle, to }] =
                pair
            else {
                bail!("incomplete terminal namespace move");
            };
            ensure!(root == second_root && middle == second_middle);
            let kind = if record
                .nodes
                .iter()
                .find(|n| n.root == *root && n.path == *from)
                .context("missing original resource")?
                .version
                .is_some()
            {
                MetadataDirectoryEntryKind::File
            } else {
                MetadataDirectoryEntryKind::Directory
            };
            receipt.resource_paths.push(MetadataResourceRename {
                root_id: root.clone(),
                from_relative: from.into(),
                to_relative: to.into(),
                kind,
            });
        }
        Ok(receipt)
    }

    fn preserve_observed(&self, record: &mut Record) -> Result<()> {
        let tree = self.current_tree()?;
        let expected = self.expected_tree(record)?;
        let mut total = record
            .nodes
            .iter()
            .chain(&record.observed)
            .filter_map(|n| n.version.as_ref())
            .try_fold(0u64, |n, v| n.checked_add(v.len).context("blob size overflow"))?;
        for step in &record.steps {
            if let Step::Text { after, .. } = step {
                total = total.checked_add(after.len).context("blob size overflow")?;
            }
        }
        for ((root, path), version) in tree {
            let Some(version) = version else {
                continue;
            };
            if expected.get(&(root.clone(), path.clone())) == Some(&Some(version.clone())) {
                continue;
            }
            if record.observed.iter().any(|node| {
                node.root == root && node.path == path && node.version.as_ref() == Some(&version)
            }) {
                continue;
            }
            ensure!(record.observed.len() < self.bounds.entries);
            total = total.checked_add(version.len).context("blob size overflow")?;
            ensure!(
                total <= self.bounds.total_blob_bytes,
                "observed source exceeds journal budget; preserved in source"
            );
            let mut bytes = Vec::new();
            Read::by_ref(&mut self.directory(&root)?.open_file(&path)?)
                .take(self.bounds.blob_bytes + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                fingerprint(&bytes) == version.hash && bytes.len() as u64 == version.len,
                "observed source changed while preserving"
            );
            self.put_blob(&bytes)?;
            record.observed.push(Node { root, path, version: Some(version) });
        }
        Ok(())
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}
fn filename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
fn sibling(path: &str, name: &str) -> String {
    if parent(path).is_empty() {
        name.to_owned()
    } else {
        format!("{}/{name}", parent(path))
    }
}
fn portable(path: &Path) -> Result<String> {
    let mut names = Vec::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            bail!("non-relative source path");
        };
        names.push(name.to_str().context("non-Unicode source path")?);
    }
    let text = names.join("/");
    valid_path(&text, text.is_empty())?;
    Ok(text)
}
fn temporary_path(record: &Record, index: usize, path: &str, undo: bool) -> String {
    sibling(
        path,
        &format!(
            ".bsl-rename-{}-{index}-{}.tmp",
            record.transaction,
            if undo { "undo" } else { "apply" }
        ),
    )
}
fn under(path: &str, prefix: &str) -> bool {
    path == prefix || path.strip_prefix(prefix).is_some_and(|tail| tail.starts_with('/'))
}
fn move_tree(tree: &mut Tree, root: &str, from: &str, to: &str) -> Result<()> {
    ensure!(tree.contains_key(&(root.to_owned(), from.to_owned())));
    ensure!(!tree.contains_key(&(root.to_owned(), to.to_owned())));
    let moved: Vec<_> = tree
        .iter()
        .filter(|((id, path), _)| id == root && under(path, from))
        .map(|(key, version)| (key.clone(), version.clone()))
        .collect();
    for ((id, old), _) in &moved {
        tree.remove(&(id.clone(), old.clone()));
    }
    for ((id, old), version) in moved {
        tree.insert((id, format!("{to}{}", &old[from.len()..])), version);
    }
    Ok(())
}

impl RootLocks {
    /// Keeps the physical outcome recoverable until its model publication is acknowledged.
    pub(crate) fn defer_publication(&mut self) {
        self.defer_publication = true;
    }

    #[cfg(test)]
    pub(crate) fn apply(
        &mut self,
        plan: &MetadataRenamePlan,
        snapshot: &PreparedMetadataRenameSnapshot,
        cancelled: &AtomicBool,
    ) -> Result<Receipt> {
        self.apply_observed(plan, snapshot, cancelled, &mut |_| Ok(()))
    }

    pub(crate) fn apply_guarded(
        &mut self,
        plan: &MetadataRenamePlan,
        snapshot: &PreparedMetadataRenameSnapshot,
        cancelled: &AtomicBool,
        violated: &AtomicBool,
        before_first_source_write: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Receipt> {
        let mut started = false;
        self.apply_with_observers(
            plan,
            snapshot,
            cancelled,
            violated,
            &mut || {
                if !started {
                    before_first_source_write()?;
                    started = true;
                }
                Ok(())
            },
            &mut |_| Ok(()),
        )
    }

    #[cfg(test)]
    fn apply_observed(
        &mut self,
        plan: &MetadataRenamePlan,
        snapshot: &PreparedMetadataRenameSnapshot,
        cancelled: &AtomicBool,
        observer: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<Receipt> {
        self.apply_guarded_observed(plan, snapshot, cancelled, &AtomicBool::new(false), observer)
    }

    #[cfg(test)]
    fn apply_guarded_observed(
        &mut self,
        plan: &MetadataRenamePlan,
        snapshot: &PreparedMetadataRenameSnapshot,
        cancelled: &AtomicBool,
        violated: &AtomicBool,
        observer: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<Receipt> {
        self.apply_with_observers(plan, snapshot, cancelled, violated, &mut || Ok(()), observer)
    }

    pub(super) fn apply_with_observers(
        &mut self,
        plan: &MetadataRenamePlan,
        snapshot: &PreparedMetadataRenameSnapshot,
        cancelled: &AtomicBool,
        violated: &AtomicBool,
        before_source_write: &mut dyn FnMut() -> Result<()>,
        observer: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<Receipt> {
        ensure!(!cancelled.load(Ordering::Acquire), "metadata rename cancelled before writes");
        ensure!(!violated.load(Ordering::Acquire), "exclusive session changed before writes");
        self.pending_other(None)?;
        let mut record = self.prepare(plan, snapshot)?;
        self.check_tree(&record)?;
        let mut hash = self.save(&record, None)?;
        let transaction = record.transaction.clone();
        let work = (|| -> Result<()> {
            while record.applied < record.steps.len() {
                ensure!(!violated.load(Ordering::Acquire), "exclusive session changed");
                ensure!(!cancelled.load(Ordering::Acquire), "metadata rename cancelled");
                self.check_tree(&record)?;
                self.execute_guarded(
                    &mut record,
                    &mut hash,
                    false,
                    Some(violated),
                    before_source_write,
                )?;
                observer(record.applied)?;
            }
            ensure!(!cancelled.load(Ordering::Acquire), "metadata rename cancelled");
            ensure!(!violated.load(Ordering::Acquire), "exclusive session changed");
            self.check_tree(&record)?;
            Ok(())
        })();
        if let Err(error) = work {
            if violated.load(Ordering::Acquire) {
                return Ok(self.recovery_required(&transaction, error.to_string()));
            }
            return Ok(self.rollback_result(
                &transaction,
                plan.context_sha256,
                Some(error.to_string()),
                before_source_write,
            ));
        }
        let receipt = Receipt {
            transaction,
            status: Status::Committed,
            reason: None,
            known_files: Vec::new(),
            resource_paths: Vec::new(),
        };
        record.terminal = Some(receipt.clone());
        if let Err(error) = self.save(&record, Some(hash)) {
            return Ok(self.rollback_result(
                &record.transaction,
                plan.context_sha256,
                Some(error.to_string()),
                before_source_write,
            ));
        }
        self.with_known_versions(&record, receipt)
    }

    #[cfg(test)]
    pub(crate) fn recover(&mut self, transaction: &str, context: [u8; 32]) -> Result<Receipt> {
        self.recover_guarded(transaction, context, &mut || Ok(()))
    }

    pub(crate) fn recover_guarded(
        &mut self,
        transaction: &str,
        context: [u8; 32],
        before_first_source_write: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Receipt> {
        let (mut record, hash) = self.load(transaction)?;
        ensure!(
            record.context == context,
            "live project context does not approve journal recovery"
        );
        if self.defer_publication && !record.publication_pending {
            record.publication_pending = true;
            self.save(&record, Some(hash))?;
        }
        if record.publication_only() {
            self.check_tree(&record)?;
            return self.with_known_versions(&record, record.terminal.clone().unwrap());
        }
        if let Some(receipt) = &record.terminal {
            if receipt.status != Status::RecoveryRequired {
                return self.with_known_versions(&record, receipt.clone());
            }
        }
        let mut started = false;
        Ok(self.rollback_result(transaction, context, None, &mut || {
            if !started {
                before_first_source_write()?;
                started = true;
            }
            Ok(())
        }))
    }

    fn rollback_result(
        &self,
        transaction: &str,
        context: [u8; 32],
        reason: Option<String>,
        before_source_write: &mut dyn FnMut() -> Result<()>,
    ) -> Receipt {
        let result = (|| -> Result<Receipt> {
            let (mut record, mut hash) = self.load(transaction)?;
            ensure!(record.context == context);
            self.resolve_intent(&mut record, &mut hash, before_source_write)?;
            record.terminal = None;
            while record.applied > 0 {
                self.check_tree(&record)?;
                self.execute_guarded(&mut record, &mut hash, true, None, before_source_write)?;
            }
            self.check_tree(&record)?;
            let receipt = Receipt {
                transaction: transaction.to_owned(),
                status: Status::RolledBack,
                reason: reason.clone(),
                known_files: Vec::new(),
                resource_paths: Vec::new(),
            };
            record.terminal = Some(receipt.clone());
            self.save(&record, Some(hash))?;
            self.with_known_versions(&record, receipt)
        })();
        match result {
            Ok(receipt) => receipt,
            Err(error) => self.recovery_required(transaction, error.to_string()),
        }
    }

    fn recovery_required(&self, transaction: &str, mut message: String) -> Receipt {
        if let Ok((mut record, hash)) = self.load(transaction) {
            if let Err(error) = self.preserve_observed(&mut record) {
                message.push_str(&format!("; observed bytes remain in source: {error}"));
            }
            while message.len() > 4096 {
                message.pop();
            }
            let receipt = Receipt {
                transaction: transaction.to_owned(),
                status: Status::RecoveryRequired,
                reason: Some(message.clone()),
                known_files: Vec::new(),
                resource_paths: Vec::new(),
            };
            record.terminal = Some(receipt.clone());
            if self.save(&record, Some(hash)).is_ok() {
                return receipt;
            }
            message.push_str("; recovery outcome could not be persisted");
        }
        while message.len() > 4096 {
            message.pop();
        }
        Receipt {
            transaction: transaction.to_owned(),
            status: Status::RecoveryRequired,
            reason: Some(message),
            known_files: Vec::new(),
            resource_paths: Vec::new(),
        }
    }

    #[cfg(test)]
    fn execute(&self, record: &mut Record, hash: &mut [u8; 32], undo: bool) -> Result<()> {
        self.execute_guarded(record, hash, undo, None, &mut || Ok(()))
    }

    fn execute_guarded(
        &self,
        record: &mut Record,
        hash: &mut [u8; 32],
        undo: bool,
        violated: Option<&AtomicBool>,
        before_source_write: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        self.execute_with_observers(record, hash, undo, violated, before_source_write, &mut |_| {
            Ok(())
        })
    }

    #[cfg(test)]
    fn execute_observed(
        &self,
        record: &mut Record,
        hash: &mut [u8; 32],
        undo: bool,
        violated: Option<&AtomicBool>,
        observer: &mut dyn FnMut(StageBoundary) -> Result<()>,
    ) -> Result<()> {
        self.execute_with_observers(record, hash, undo, violated, &mut || Ok(()), observer)
    }

    fn execute_with_observers(
        &self,
        record: &mut Record,
        hash: &mut [u8; 32],
        undo: bool,
        violated: Option<&AtomicBool>,
        before_source_write: &mut dyn FnMut() -> Result<()>,
        observer: &mut dyn FnMut(StageBoundary) -> Result<()>,
    ) -> Result<()> {
        ensure!(record.intent.is_none());
        ensure!(!violated.is_some_and(|v| v.load(Ordering::Acquire)), "exclusive session changed");
        let index = if undo {
            record.applied.checked_sub(1).context("empty rollback")?
        } else {
            record.applied
        };
        let step = record.steps.get(index).context("step index out of bounds")?.clone();
        match step {
            Step::Text { root, path, before, after, .. } => {
                let blob = if undo { before } else { after };
                let bytes = self.read_blob(&blob)?;
                let temporary = temporary_path(record, index, &path, undo);
                let directory = self.directory(&root)?;
                let destination_metadata = directory.open_file(&path)?.metadata()?;
                let permissions = destination_metadata.permissions();
                ensure!(!permissions.readonly(), "source target became read-only");
                record.intent = Some(Intent {
                    step: index,
                    undo,
                    staged: None,
                    temporary: Some(temporary.clone()),
                    writing: false,
                    contested: false,
                });
                *hash = self.save(record, Some(*hash))?;
                observer(StageBoundary::Declared)?;
                before_source_write()?;
                let mut file = match directory.create_file_new_in_existing_directory(&temporary) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        record.intent.as_mut().unwrap().contested = true;
                        *hash = self.save(record, Some(*hash))?;
                        return Err(error.into());
                    }
                    Err(error) => return Err(error.into()),
                };
                observer(StageBoundary::CreatedUncheckpointed)?;
                file.set_permissions(permissions.clone())?;
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileTimesExt;
                    // Rename tunneling preserves the destination's creation time; checkpoint it before replacing.
                    file.set_times(
                        std::fs::FileTimes::new().set_created(destination_metadata.created()?),
                    )?;
                }
                file.sync_all()?;
                let created = self.read_version(&root, &temporary)?;
                ensure!(created.len == 0 && created.hash == fingerprint(b""));
                record.intent.as_mut().unwrap().staged = Some(created.clone());
                record.intent.as_mut().unwrap().writing = true;
                *hash = self.save(record, Some(*hash))?;
                ensure!(
                    created.identity.is_some() || created.created_nanos.is_some(),
                    "filesystem cannot identify an incomplete staging file"
                );
                observer(StageBoundary::Created)?;
                file.write_all(&bytes)?;
                file.set_permissions(permissions)?;
                file.sync_all()?;
                observer(StageBoundary::Written)?;
                let staged = self.read_version(&root, &temporary)?;
                ensure!(staged.hash == blob.hash && staged.len == blob.len);
                record.intent = Some(Intent {
                    step: index,
                    undo,
                    staged: Some(staged.clone()),
                    temporary: Some(temporary.clone()),
                    writing: false,
                    contested: false,
                });
                *hash = self.save(record, Some(*hash))?;
                observer(StageBoundary::Ready)?;
                let mut expected = self.expected_tree(record)?;
                expected.insert((root.clone(), temporary.clone()), Some(staged));
                ensure!(self.current_tree()? == expected, "source changed after staging");
                ensure!(
                    !violated.is_some_and(|v| v.load(Ordering::Acquire)),
                    "exclusive session changed"
                );
                directory.replace_file_in_existing_directory(&temporary, &path)?;
                observer(StageBoundary::Installed)?;
                directory.sync_parent(&path)?;
            }
            Step::Move { root, from, to } => {
                record.intent = Some(Intent {
                    step: index,
                    undo,
                    staged: None,
                    temporary: None,
                    writing: false,
                    contested: false,
                });
                *hash = self.save(record, Some(*hash))?;
                let (source, target) = if undo { (&to, &from) } else { (&from, &to) };
                ensure!(
                    !violated.is_some_and(|v| v.load(Ordering::Acquire)),
                    "exclusive session changed"
                );
                before_source_write()?;
                self.directory(&root)?.rename_entry_new(source, target)?;
                self.directory(&root)?.sync_parent(target)?;
            }
        }
        self.resolve_intent(record, hash, before_source_write)
    }

    fn resolve_intent(
        &self,
        record: &mut Record,
        hash: &mut [u8; 32],
        before_source_write: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let Some(intent) = record.intent.clone() else {
            return Ok(());
        };
        let before = self.expected_tree(record)?;
        let mut after = before.clone();
        let step = record.steps[intent.step].clone();
        if let Step::Text { root, before: original, after: proposed, .. } = &step {
            if intent.staged.is_none() || intent.writing {
                let temporary = intent.temporary.as_ref().context("missing staging path")?;
                let mut actual = self.current_tree()?;
                let staged = actual.remove(&(root.clone(), temporary.clone()));
                ensure!(
                    actual == before,
                    "foreign source version or namespace prevents safe staging recovery"
                );
                if let Some(version) = staged {
                    let version = version.context("staging path is a foreign directory")?;
                    ensure!(!intent.contested, "another writer occupied the declared staging path");
                    if let Some(created) = &intent.staged {
                        ensure!(created.identity.is_some() || created.created_nanos.is_some());
                        ensure!(
                            created.identity == version.identity
                                && created.created_nanos == version.created_nanos
                                && created.mode == version.mode,
                            "staging file was replaced by another writer"
                        );
                    } else {
                        ensure!(
                            version.len == 0 && version.hash == fingerprint(b""),
                            "uncheckpointed staging path has foreign bytes"
                        );
                    }
                    let expected = self.read_blob(if intent.undo { original } else { proposed })?;
                    ensure!(version.len <= expected.len() as u64);
                    ensure!(
                        version.hash == fingerprint(&expected[..version.len as usize]),
                        "incomplete staging bytes do not match the planned prefix"
                    );
                    ensure!(
                        self.read_version(root, temporary)? == version,
                        "staging version changed during recovery"
                    );
                    before_source_write()?;
                    self.directory(root)?.remove_file(temporary)?;
                    self.directory(root)?.sync_parent(temporary)?;
                }
                record.intent = None;
                *hash = self.save(record, Some(*hash))?;
                return Ok(());
            }
        }
        match &step {
            Step::Text { root, path, .. } => {
                after.insert((root.clone(), path.clone()), intent.staged.clone());
            }
            Step::Move { root, from, to } => {
                let (source, target) = if intent.undo { (to, from) } else { (from, to) };
                move_tree(&mut after, root, source, target)?;
            }
        }
        let actual = self.current_tree()?;
        let mut staged_before = before.clone();
        if let (Step::Text { root, .. }, Some(temporary), Some(version)) =
            (&step, &intent.temporary, &intent.staged)
        {
            staged_before.insert((root.clone(), temporary.clone()), Some(version.clone()));
        }
        if actual == after {
            if let Step::Text { installed, restored, .. } = &mut record.steps[intent.step] {
                if intent.undo {
                    *restored = intent.staged.clone().map(Box::new);
                } else {
                    *installed = intent.staged.clone().map(Box::new);
                }
            }
            if intent.undo {
                record.applied -= 1;
            } else {
                record.applied += 1;
            }
        } else if actual == staged_before || actual == before {
            if actual == staged_before {
                if let (Step::Text { root, .. }, Some(temporary), Some(version)) =
                    (&step, &intent.temporary, &intent.staged)
                {
                    ensure!(&self.read_version(root, temporary)? == version);
                    before_source_write()?;
                    self.directory(root)?.remove_file(temporary)?;
                    self.directory(root)?.sync_parent(temporary)?;
                }
            }
        } else {
            bail!("foreign source version or namespace prevents safe recovery");
        }
        record.intent = None;
        *hash = self.save(record, Some(*hash))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hir::metadata_symbol::{MetadataSymbolIdentity, MetadataSymbolPath};
    use ide::metadata_rename_plan::{
        MetadataDirectoryEntry, MetadataDirectorySnapshot, MetadataFileRead, MetadataOpenDocument,
        MetadataPlanFileKind, MetadataPlannedFile, MetadataRawEdit, MetadataResourceRename,
        PreparedMetadataRenameFile,
    };
    use std::sync::Arc;
    use vfs::FileId;

    struct Fixture {
        source: tempfile::TempDir,
        cache: tempfile::TempDir,
        snapshot: PreparedMetadataRenameSnapshot,
        plan: MetadataRenamePlan,
    }

    fn fixture_store_at(cache: &Path) -> JournalStore {
        JournalStore::open(
            cache,
            Bounds {
                record_bytes: 1 << 20,
                blob_bytes: 1 << 20,
                total_blob_bytes: 4 << 20,
                entries: 1000,
            },
        )
        .unwrap()
    }

    impl Fixture {
        fn new() -> Self {
            let source = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            std::fs::create_dir(source.path().join("Partners")).unwrap();
            std::fs::write(source.path().join("Partners.xml"), b"\xef\xbb\xbf<Catalog uuid=\"00000000-0000-0000-0000-000000000001\"><Name>Partners</Name></Catalog>\r\n").unwrap();
            std::fs::write(
                source.path().join("Partners/Module.bsl"),
                b"Procedure Check()\r\nResult = Catalogs.Partners;\r\nEndProcedure\r\n",
            )
            .unwrap();
            std::fs::write(source.path().join("Partners/Picture.bin"), [0, 255, 42]).unwrap();
            std::fs::write(source.path().join("Unused.xml"), b"<Other/>\r\n").unwrap();
            let canonical = std::fs::canonicalize(source.path()).unwrap();
            let root = MetadataPlanRoot {
                id: "main".into(),
                source_path: canonical.clone(),
                canonical_path: canonical,
                role: MetadataRootRole::Main,
                writable: true,
                case_sensitive: cfg!(not(windows)),
            };
            let mut snapshot = PreparedMetadataRenameSnapshot {
                roots: vec![root],
                files: Vec::new(),
                directories: Vec::new(),
                context_sha256: [1; 32],
                unindexed_paths: Vec::new(),
            };
            let mut pending = vec![PathBuf::new()];
            while let Some(relative) = pending.pop() {
                let mut entries = Vec::new();
                let mut children: Vec<_> = std::fs::read_dir(source.path().join(&relative))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                children.sort_by_key(|child| child.file_name());
                for child in children {
                    let name = child.file_name().into_string().unwrap();
                    let path = relative.join(&name);
                    let kind = if child.file_type().unwrap().is_dir() {
                        pending.push(path);
                        MetadataDirectoryEntryKind::Directory
                    } else {
                        let bytes = std::fs::read(child.path()).unwrap();
                        let kind = match child.path().extension().and_then(|v| v.to_str()) {
                            Some("xml") => MetadataPlanFileKind::Xml,
                            Some("bsl") => MetadataPlanFileKind::Bsl,
                            _ => MetadataPlanFileKind::OpaqueResource,
                        };
                        let open_document =
                            (kind == MetadataPlanFileKind::Bsl).then(|| MetadataOpenDocument {
                                version: 7,
                                document_text_sha256: fingerprint(&bytes),
                                dirty: false,
                            });
                        snapshot.files.push(PreparedMetadataRenameFile {
                            file_id: Some(FileId(snapshot.files.len() as u32 + 1)),
                            root_id: "main".into(),
                            relative_path: path,
                            kind,
                            bytes: Arc::from(bytes),
                            open_document,
                            effective_form_attributes: None,
                        });
                        MetadataDirectoryEntryKind::File
                    };
                    entries.push(MetadataDirectoryEntry { name, kind });
                }
                snapshot.directories.push(MetadataDirectorySnapshot {
                    root_id: "main".into(),
                    relative_path: relative,
                    entries,
                });
            }
            let files = snapshot
                .files
                .iter()
                .filter(|file| {
                    file.kind != MetadataPlanFileKind::OpaqueResource
                        && file.relative_path.file_name().unwrap() != "Unused.xml"
                })
                .map(|file| {
                    let start = file
                        .bytes
                        .windows(b"Partners".len())
                        .position(|v| v == b"Partners")
                        .unwrap();
                    let edits = vec![MetadataRawEdit {
                        range: start..start + 8,
                        expected_bytes: b"Partners".to_vec(),
                        replacement_bytes: b"Customers".to_vec(),
                    }];
                    let after_bytes =
                        render_metadata_edits(&file.bytes, &mut edits.clone()).unwrap();
                    MetadataPlannedFile {
                        root_id: "main".into(),
                        relative_path: file.relative_path.clone(),
                        before_sha256: fingerprint(&file.bytes),
                        edits,
                        after_bytes,
                    }
                })
                .collect();
            let plan = MetadataRenamePlan {
                target: MetadataSymbolIdentity {
                    declaration_file: FileId(1),
                    owner_uuid: bsl_metadata::Uuid::from_u128(1),
                    declaration_uuid: bsl_metadata::Uuid::from_u128(1),
                },
                path: MetadataSymbolPath {
                    owner_kind: bsl_metadata::MdoType::Catalog,
                    owner_name: hir::Name::new("Partners"),
                    members: Vec::new(),
                },
                new_name: "Customers".into(),
                roots: snapshot.roots.clone(),
                context_sha256: snapshot.context_sha256,
                read_set: snapshot
                    .files
                    .iter()
                    .map(|file| MetadataFileRead {
                        file_id: file.file_id,
                        root_id: file.root_id.clone(),
                        relative_path: file.relative_path.clone(),
                        before_sha256: fingerprint(&file.bytes),
                        open_document: file.open_document.clone(),
                    })
                    .collect(),
                directories: snapshot.directories.clone(),
                files,
                resources: vec![
                    MetadataResourceRename {
                        root_id: "main".into(),
                        from_relative: "Partners.xml".into(),
                        to_relative: "Customers.xml".into(),
                        kind: MetadataDirectoryEntryKind::File,
                    },
                    MetadataResourceRename {
                        root_id: "main".into(),
                        from_relative: "Partners".into(),
                        to_relative: "Customers".into(),
                        kind: MetadataDirectoryEntryKind::Directory,
                    },
                ],
                problems: Vec::new(),
            };
            Self { source, cache, snapshot, plan }
        }
        fn store(&self) -> JournalStore {
            fixture_store_at(self.cache.path())
        }
        fn assert_original(&self) {
            for file in &self.snapshot.files {
                assert_eq!(
                    std::fs::read(self.source.path().join(&file.relative_path)).unwrap(),
                    file.bytes.as_ref()
                );
            }
            assert!(!self.source.path().join("Customers.xml").exists());
            assert!(!self.source.path().join("Customers").exists());
        }
        fn transaction(&self) -> String {
            let path = std::fs::read_dir(self.cache.path().join("metadata-rename/journals"))
                .unwrap()
                .map(Result::unwrap)
                .find(|entry| entry.path().extension().is_some_and(|v| v == "json"))
                .unwrap()
                .path();
            path.file_stem().unwrap().to_str().unwrap().to_owned()
        }
    }

    #[test]
    fn commit_preserves_uuid_raw_bytes_and_moves_source_container() {
        let f = Fixture::new();
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        let result = locks.apply(&f.plan, &f.snapshot, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.status, Status::Committed);
        assert!(!f.source.path().join("Partners.xml").exists());
        assert!(!f.source.path().join("Partners").exists());
        for edited in &f.plan.files {
            let old = portable(&edited.relative_path).unwrap();
            let new = if old == "Partners.xml" {
                "Customers.xml".to_owned()
            } else {
                old.replacen("Partners/", "Customers/", 1)
            };
            assert_eq!(std::fs::read(f.source.path().join(new)).unwrap(), edited.after_bytes);
        }
        assert_eq!(
            std::fs::read(f.source.path().join("Customers/Picture.bin")).unwrap(),
            [0, 255, 42]
        );
        let journal = std::fs::read(
            f.cache.path().join(format!("metadata-rename/journals/{}.json", result.transaction)),
        )
        .unwrap();
        assert!(!String::from_utf8(journal).unwrap().contains("Catalog uuid"));
        drop(locks);
        assert_eq!(
            store.recover(&result.transaction, &f.plan.roots, f.plan.context_sha256).unwrap(),
            result
        );
    }

    #[test]
    fn stale_read_input_document_context_and_namespace_refuse_before_writes() {
        for variant in 0..5 {
            let mut f = Fixture::new();
            match variant {
                0 => std::fs::write(f.source.path().join("Unused.xml"), b"foreign").unwrap(),
                1 => {
                    f.snapshot
                        .files
                        .iter_mut()
                        .find(|v| v.open_document.is_some())
                        .unwrap()
                        .open_document
                        .as_mut()
                        .unwrap()
                        .version += 1
                }
                2 => f.snapshot.context_sha256 = [2; 32],
                3 => std::fs::write(f.source.path().join("New.xml"), b"new").unwrap(),
                _ => std::fs::write(f.source.path().join("Partners.xml"), b"new target").unwrap(),
            }
            let before: Vec<_> = f
                .plan
                .files
                .iter()
                .map(|v| {
                    (
                        v.relative_path.clone(),
                        std::fs::read(f.source.path().join(&v.relative_path)).unwrap(),
                    )
                })
                .collect();
            let store = f.store();
            let mut locks = store.lock_roots(&f.plan.roots).unwrap();
            assert!(locks.apply(&f.plan, &f.snapshot, &AtomicBool::new(false)).is_err());
            for (path, bytes) in before {
                assert_eq!(std::fs::read(f.source.path().join(path)).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn readonly_target_refuses_before_any_replacement() {
        let f = Fixture::new();
        let path = f.source.path().join("Partners.xml");
        let original = std::fs::metadata(&path).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&path, readonly).unwrap();
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        assert!(locks.apply(&f.plan, &f.snapshot, &AtomicBool::new(false)).is_err());
        f.assert_original();
        std::fs::set_permissions(path, original).unwrap();
    }

    #[test]
    fn project_context_inputs_cannot_be_text_replacements() {
        let mut f = Fixture::new();
        f.snapshot
            .files
            .iter_mut()
            .find(|file| file.relative_path == Path::new("Partners.xml"))
            .unwrap()
            .kind = MetadataPlanFileKind::ProjectInput;
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        assert!(locks.apply(&f.plan, &f.snapshot, &AtomicBool::new(false)).is_err());
        f.assert_original();
    }

    #[test]
    fn participating_writers_share_one_root_lock() {
        let f = Fixture::new();
        let first = f.store();
        let second = f.store();
        let held = first.lock_roots(&f.plan.roots).unwrap();
        assert!(second.lock_roots(&f.plan.roots).is_err());
        drop(held);
        assert!(second.lock_roots(&f.plan.roots).is_ok());
    }

    #[test]
    fn duplicate_descriptors_do_not_extend_ownership_or_unlock_a_new_holder() {
        let f = Fixture::new();
        let store = f.store();
        let held = store.lock_roots(&f.plan.roots).unwrap();
        let duplicates: Vec<_> =
            held._locks.iter().map(|lock| lock.0.try_clone().unwrap()).collect();
        assert!(store.lock_roots(&f.plan.roots).is_err());
        drop(held);
        let next = store.lock_roots(&f.plan.roots).unwrap();
        for duplicate in &duplicates {
            let _ = duplicate.unlock();
        }
        drop(duplicates);
        assert!(store.lock_roots(&f.plan.roots).is_err());
        drop(next);
        assert!(store.lock_roots(&f.plan.roots).is_ok());
    }

    #[test]
    fn partial_root_lock_failure_releases_every_lock_already_acquired() {
        let f = Fixture::new();
        let other = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(other.path()).unwrap();
        let mut other_root = f.plan.roots[0].clone();
        other_root.id = "second".into();
        other_root.source_path = canonical.clone();
        other_root.canonical_path = canonical;
        let mut approved = vec![f.plan.roots[0].clone(), other_root];
        approved.sort_by(|a, b| a.canonical_path.cmp(&b.canonical_path));
        let store = f.store();
        drop(store.lock_roots(&approved).unwrap());
        let cache =
            ManagedBaselineDirectory::open(f.cache.path(), "metadata-rename", false).unwrap();
        let root_lock = |root: &MetadataPlanRoot| {
            let key = hex(&fingerprint(root.canonical_path.to_str().unwrap().as_bytes()));
            cache.open_or_create_file(&format!("locks/{key}.lock")).unwrap()
        };
        let blocked = root_lock(&approved[1]);
        blocked.try_lock().unwrap();
        let error = store.lock_roots(&approved).err().expect("the last root is held");
        assert!(error.to_string().contains("metadata source root is already held"));
        let participant = cache.open_or_create_file("locks/participants.lock").unwrap();
        participant.try_lock().expect("the partial participant lock was released");
        let first = root_lock(&approved[0]);
        first.try_lock().expect("the first source root was released");
        first.unlock().unwrap();
        participant.unlock().unwrap();
        blocked.unlock().unwrap();
        assert!(store.lock_roots(&approved).is_ok());
    }

    #[test]
    fn fault_and_cancellation_after_first_write_restore_all_original_bytes() {
        for cancel in [false, true] {
            let f = Fixture::new();
            let store = f.store();
            let mut locks = store.lock_roots(&f.plan.roots).unwrap();
            let cancelled = AtomicBool::new(false);
            let receipt = locks
                .apply_observed(&f.plan, &f.snapshot, &cancelled, &mut |count| {
                    if count == 1 {
                        if cancel {
                            cancelled.store(true, Ordering::Release);
                        } else {
                            bail!("injected failure");
                        }
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(receipt.status, Status::RolledBack);
            f.assert_original();
        }
    }

    #[test]
    fn external_version_is_preserved_and_pending_recovery_blocks_another_apply() {
        let f = Fixture::new();
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        let receipt = locks
            .apply_observed(&f.plan, &f.snapshot, &AtomicBool::new(false), &mut |count| {
                if count == 1 {
                    std::fs::write(f.source.path().join("Partners.xml"), b"foreign").unwrap();
                    bail!("foreign writer");
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(receipt.status, Status::RecoveryRequired);
        assert_eq!(std::fs::read(f.source.path().join("Partners.xml")).unwrap(), b"foreign");
        assert!(locks.apply(&f.plan, &f.snapshot, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn process_break_then_reopen_recovers_and_terminal_receipt_is_idempotent() {
        let f = Fixture::new();
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            locks.apply_observed(&f.plan, &f.snapshot, &AtomicBool::new(false), &mut |count| {
                if count == 1 {
                    panic!("simulated process break");
                }
                Ok(())
            })
        }));
        assert!(result.is_err());
        drop(locks);
        drop(store);
        let reopened = f.store();
        let id = f.transaction();
        let receipt = reopened.recover(&id, &f.plan.roots, f.plan.context_sha256).unwrap();
        assert_eq!(receipt.status, Status::RolledBack);
        f.assert_original();
        assert_eq!(reopened.recover(&id, &f.plan.roots, f.plan.context_sha256).unwrap(), receipt);
    }

    #[test]
    fn corrupted_paths_and_unapproved_roots_are_never_opened_for_restore() {
        let f = Fixture::new();
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        let mut record = locks.prepare(&f.plan, &f.snapshot).unwrap();
        let id = record.transaction.clone();
        record.nodes[0].path = "../outside".into();
        assert!(locks.save(&record, None).is_err());
        record.nodes[0].path.clear();
        record.roots[0].canonical_path = f.cache.path().to_string_lossy().into_owned();
        assert!(locks.save(&record, None).is_err());
        let oversized = vec![b' '; (1 << 20) + 1];
        let path = f.cache.path().join(format!("metadata-rename/journals/{id}.json"));
        std::fs::write(path, oversized).unwrap();
        assert!(locks.recover(&id, f.plan.context_sha256).is_err());
        f.assert_original();
    }

    #[test]
    fn portable_journal_strings_cannot_alias_another_managed_path() {
        for path in ["../outside", "a/../b", "a//b", "a/./b", "a/", "C:/outside", "a\\b", ""] {
            assert!(valid_path(path, false).is_err(), "{path}");
        }
        assert!(valid_path("", true).is_ok());
        assert_eq!(portable(&Path::new("a").join("b")).unwrap(), "a/b");
    }

    #[test]
    fn volatile_exclusivity_violation_preserves_current_bytes_for_recovery() {
        let f = Fixture::new();
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        let violated = AtomicBool::new(false);
        let receipt = locks
            .apply_guarded_observed(
                &f.plan,
                &f.snapshot,
                &AtomicBool::new(false),
                &violated,
                &mut |count| {
                    if count == 1 {
                        violated.store(true, Ordering::Release);
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(receipt.status, Status::RecoveryRequired);
        assert_eq!(
            std::fs::read(f.source.path().join("Partners.xml")).unwrap(),
            f.plan.files[0].after_bytes
        );
        assert!(locks.apply(&f.plan, &f.snapshot, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn foreign_versions_are_archived_alongside_original_and_proposed_bytes() {
        let f = Fixture::new();
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        let receipt = locks
            .apply_observed(&f.plan, &f.snapshot, &AtomicBool::new(false), &mut |count| {
                if count == 1 {
                    std::fs::write(f.source.path().join("Unused.xml"), b"foreign").unwrap();
                    bail!("foreign writer");
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(receipt.status, Status::RecoveryRequired);
        for bytes in [b"foreign".as_slice(), b"<Other/>\r\n".as_slice()] {
            assert_eq!(
                locks
                    .read_blob(&Blob { hash: fingerprint(bytes), len: bytes.len() as u64 })
                    .unwrap(),
                bytes
            );
        }
        let (record, _) = locks.load(&receipt.transaction).unwrap();
        assert!(record.observed.iter().any(|node| node.path == "Unused.xml"
            && node.version.as_ref().is_some_and(|v| v.hash == fingerprint(b"foreign"))));
    }

    #[test]
    fn pending_journals_require_exact_live_approval() {
        let f = Fixture::new();
        let store = f.store();
        let locks = store.lock_roots(&f.plan.roots).unwrap();
        let record = locks.prepare(&f.plan, &f.snapshot).unwrap();
        locks.save(&record, None).unwrap();
        drop(locks);
        assert_eq!(
            store.pending(&f.plan.roots, f.plan.context_sha256).unwrap(),
            vec![PendingJournal {
                transaction: record.transaction.clone(),
                recovery_approved: true,
                publication_only: false,
            }]
        );
        assert_eq!(
            store.pending(&f.plan.roots, [2; 32]).unwrap(),
            vec![PendingJournal {
                transaction: record.transaction,
                recovery_approved: false,
                publication_only: false
            }]
        );
    }

    #[test]
    fn interruption_after_each_text_and_namespace_operation_recovers() {
        for boundary in 1..=6 {
            let f = Fixture::new();
            let store = f.store();
            let mut locks = store.lock_roots(&f.plan.roots).unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                locks.apply_observed(&f.plan, &f.snapshot, &AtomicBool::new(false), &mut |count| {
                    if count == boundary {
                        panic!("simulated interruption");
                    }
                    Ok(())
                })
            }));
            assert!(result.is_err());
            drop(locks);
            let receipt =
                store.recover(&f.transaction(), &f.plan.roots, f.plan.context_sha256).unwrap();
            assert_eq!(receipt.status, Status::RolledBack, "{boundary}: {:?}", receipt.reason);
            f.assert_original();
        }
    }

    #[test]
    fn malformed_step_history_cannot_replace_an_unrelated_original() {
        let f = Fixture::new();
        let store = f.store();
        let locks = store.lock_roots(&f.plan.roots).unwrap();
        let mut record = locks.prepare(&f.plan, &f.snapshot).unwrap();
        let Step::Text { before, .. } = &mut record.steps[0] else { panic!() };
        before.hash = [9; 32];
        assert!(locks.save(&record, None).is_err());
        let mut record = locks.prepare(&f.plan, &f.snapshot).unwrap();
        let Step::Move { from, .. } = &mut record.steps[2] else { panic!() };
        *from = "Missing.xml".into();
        assert!(locks.save(&record, None).is_err());
        f.assert_original();
    }

    #[test]
    fn crash_with_persisted_operation_intent_recovers_both_sides_of_atomic_move() {
        for text in [false, true] {
            for installed in [false, true] {
                let f = Fixture::new();
                let store = f.store();
                let locks = store.lock_roots(&f.plan.roots).unwrap();
                let mut record = locks.prepare(&f.plan, &f.snapshot).unwrap();
                let mut hash = locks.save(&record, None).unwrap();
                let index = if text { 0 } else { 2 };
                while record.applied < index {
                    locks.execute(&mut record, &mut hash, false).unwrap();
                }
                match record.steps[index].clone() {
                    Step::Text { root, path, after, .. } => {
                        let temporary = temporary_path(&record, index, &path, false);
                        let mut file = locks
                            .directory(&root)
                            .unwrap()
                            .create_file_new_in_existing_directory(&temporary)
                            .unwrap();
                        file.write_all(&locks.read_blob(&after).unwrap()).unwrap();
                        file.sync_all().unwrap();
                        record.intent = Some(Intent {
                            step: index,
                            undo: false,
                            staged: Some(locks.read_version(&root, &temporary).unwrap()),
                            temporary: Some(temporary.clone()),
                            writing: false,
                            contested: false,
                        });
                        locks.save(&record, Some(hash)).unwrap();
                        if installed {
                            locks
                                .directory(&root)
                                .unwrap()
                                .replace_file_in_existing_directory(&temporary, &path)
                                .unwrap();
                        }
                    }
                    Step::Move { root, from, to } => {
                        record.intent = Some(Intent {
                            step: index,
                            undo: false,
                            staged: None,
                            temporary: None,
                            writing: false,
                            contested: false,
                        });
                        locks.save(&record, Some(hash)).unwrap();
                        if installed {
                            locks.directory(&root).unwrap().rename_entry_new(&from, &to).unwrap();
                        }
                    }
                }
                drop(locks);
                let receipt = store
                    .recover(&record.transaction, &f.plan.roots, f.plan.context_sha256)
                    .unwrap();
                assert_eq!(
                    receipt.status,
                    Status::RolledBack,
                    "{text}/{installed}: {:?}",
                    receipt.reason
                );
                f.assert_original();
            }
        }
    }

    #[test]
    fn bounds_refuse_preparation_without_modifying_sources() {
        for variant in 0..4 {
            let f = Fixture::new();
            let mut bounds = f.store().bounds;
            match variant {
                0 => bounds.entries = 2,
                1 => bounds.blob_bytes = 8,
                2 => bounds.total_blob_bytes = bounds.blob_bytes,
                _ => bounds.record_bytes = 10,
            }
            let store = JournalStore::open(f.cache.path(), bounds).unwrap();
            let mut locks = store.lock_roots(&f.plan.roots).unwrap();
            if variant == 2 {
                // Small originals and proposed contents together exceed this transaction budget.
                locks.bounds.total_blob_bytes = 10;
            }
            assert!(locks.apply(&f.plan, &f.snapshot, &AtomicBool::new(false)).is_err());
            f.assert_original();
        }
    }

    #[test]
    fn directory_replacement_and_same_content_file_replacement_are_foreign_versions() {
        for same_bytes in [false, true] {
            let f = Fixture::new();
            let store = f.store();
            let mut locks = store.lock_roots(&f.plan.roots).unwrap();
            let receipt = locks
                .apply_observed(&f.plan, &f.snapshot, &AtomicBool::new(false), &mut |count| {
                    if count == 1 {
                        let path = f.source.path().join("Partners.xml");
                        if same_bytes {
                            let bytes = std::fs::read(&path).unwrap();
                            std::fs::rename(&path, f.cache.path().join("moved.xml")).unwrap();
                            std::fs::write(&path, bytes).unwrap();
                        } else {
                            std::fs::rename(
                                f.source.path().join("Partners"),
                                f.cache.path().join("moved-directory"),
                            )
                            .unwrap();
                            std::fs::create_dir(f.source.path().join("Partners")).unwrap();
                        }
                        bail!("foreign namespace change");
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(receipt.status, Status::RecoveryRequired, "{same_bytes}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn own_staging_preserves_destination_creation_time_before_checkpoint_and_replace() {
        use std::os::windows::fs::FileTimesExt;
        use std::time::{Duration, UNIX_EPOCH};

        let f = Fixture::new();
        let destination_created = UNIX_EPOCH + Duration::from_secs(946_684_800);
        let staging_created = UNIX_EPOCH + Duration::from_secs(978_307_200);
        let store = f.store();
        let locks = store.lock_roots(&f.plan.roots).unwrap();
        let destination = locks.directory("main").unwrap().open_file("Partners.xml").unwrap();
        let destination_path = f.source.path().join("Partners.xml");
        let writable = std::fs::OpenOptions::new().write(true).open(&destination_path).unwrap();
        writable.set_times(std::fs::FileTimes::new().set_created(destination_created)).unwrap();
        assert_eq!(destination.metadata().unwrap().created().unwrap(), destination_created);
        drop(writable);
        drop(destination);
        let mut record = locks.prepare(&f.plan, &f.snapshot).unwrap();
        let mut hash = locks.save(&record, None).unwrap();
        let Step::Text { root, path, .. } = record.steps[0].clone() else { panic!("text step") };
        let created = locks.read_version(&root, &path).unwrap().created_nanos;

        for undo in [false, true] {
            let temporary = temporary_path(&record, 0, &path, undo);
            let mut ready = None;
            locks
                .execute_observed(&mut record, &mut hash, undo, None, &mut |boundary| {
                    if boundary == StageBoundary::CreatedUncheckpointed {
                        let file = std::fs::OpenOptions::new()
                            .write(true)
                            .open(f.source.path().join(&temporary))?;
                        file.set_times(std::fs::FileTimes::new().set_created(staging_created))?;
                        assert_eq!(file.metadata()?.created()?, staging_created);
                        assert_ne!(staging_created, destination_created);
                    }
                    if matches!(boundary, StageBoundary::Created | StageBoundary::Ready) {
                        let staged = locks.read_version(&root, &temporary)?;
                        assert_eq!(staged.created_nanos, created);
                        if boundary == StageBoundary::Ready {
                            ready = Some(staged);
                        }
                    }
                    if boundary == StageBoundary::Installed {
                        assert_eq!(&locks.read_version(&root, &path)?, ready.as_ref().unwrap());
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(locks.current_tree().unwrap(), locks.expected_tree(&record).unwrap());
        }
        f.assert_original();
    }

    #[test]
    fn own_incomplete_staging_recovers_after_each_checkpoint_and_partial_write() {
        for boundary in [
            StageBoundary::Declared,
            StageBoundary::CreatedUncheckpointed,
            StageBoundary::Created,
            StageBoundary::Written,
            StageBoundary::Ready,
            StageBoundary::Installed,
        ] {
            for crash in [false, true] {
                for undo in [false, true] {
                    let f = Fixture::new();
                    let store = f.store();
                    let locks = store.lock_roots(&f.plan.roots).unwrap();
                    let mut record = locks.prepare(&f.plan, &f.snapshot).unwrap();
                    let mut hash = locks.save(&record, None).unwrap();
                    if undo {
                        locks.execute(&mut record, &mut hash, false).unwrap();
                    }
                    let interrupted =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            locks.execute_observed(
                                &mut record,
                                &mut hash,
                                undo,
                                None,
                                &mut |point| {
                                    if point == boundary {
                                        if point == StageBoundary::Created {
                                            let intent = recordless_temporary(&f.cache);
                                            let mut file = std::fs::OpenOptions::new()
                                                .write(true)
                                                .open(f.source.path().join(intent.0))
                                                .unwrap();
                                            file.write_all(&intent.1[..3]).unwrap();
                                            file.sync_all().unwrap();
                                        }
                                        if crash {
                                            panic!("simulated staging interruption");
                                        }
                                        bail!("simulated staging fault");
                                    }
                                    Ok(())
                                },
                            )
                        }));
                    assert!(interrupted.is_err() || interrupted.unwrap().is_err());
                    drop(locks);
                    let receipt = store
                        .recover(&record.transaction, &f.plan.roots, f.plan.context_sha256)
                        .unwrap();
                    assert_eq!(
                        receipt.status,
                        Status::RolledBack,
                        "{boundary:?}/crash={crash}/undo={undo}: {:?}",
                        receipt.reason
                    );
                    f.assert_original();
                    let locks = store.lock_roots(&f.plan.roots).unwrap();
                    assert_eq!(
                        locks.current_tree().unwrap().len(),
                        f.snapshot.files.len() + f.snapshot.directories.len()
                    );
                }
            }
        }
    }

    fn recordless_temporary(cache: &tempfile::TempDir) -> (String, Vec<u8>) {
        let path = std::fs::read_dir(cache.path().join("metadata-rename/journals"))
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.path().extension().is_some_and(|v| v == "json"))
            .unwrap()
            .path();
        let record: Record = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let intent = record.intent.unwrap();
        let Step::Text { before, after, .. } = &record.steps[intent.step] else { panic!() };
        let blob = if intent.undo { before } else { after };
        (
            intent.temporary.unwrap(),
            std::fs::read(
                cache.path().join(format!("metadata-rename/objects/{}", hex(&blob.hash))),
            )
            .unwrap(),
        )
    }

    #[test]
    fn foreign_staging_bytes_or_creation_collision_are_preserved() {
        for boundary in [
            StageBoundary::Declared,
            StageBoundary::CreatedUncheckpointed,
            StageBoundary::Created,
            StageBoundary::Ready,
        ] {
            let f = Fixture::new();
            let store = f.store();
            let locks = store.lock_roots(&f.plan.roots).unwrap();
            let mut record = locks.prepare(&f.plan, &f.snapshot).unwrap();
            let mut hash = locks.save(&record, None).unwrap();
            let result =
                locks.execute_observed(&mut record, &mut hash, false, None, &mut |point| {
                    if point == boundary {
                        let (temporary, _) = recordless_temporary(&f.cache);
                        std::fs::write(f.source.path().join(temporary), b"foreign").unwrap();
                        if boundary != StageBoundary::Declared {
                            bail!("foreign staging writer");
                        }
                    }
                    Ok(())
                });
            assert!(result.is_err());
            let temporary = recordless_temporary(&f.cache).0;
            drop(locks);
            let receipt =
                store.recover(&record.transaction, &f.plan.roots, f.plan.context_sha256).unwrap();
            assert_eq!(receipt.status, Status::RecoveryRequired, "{boundary:?}");
            assert_eq!(std::fs::read(f.source.path().join(temporary)).unwrap(), b"foreign");
            f.assert_original();
        }
    }

    #[test]
    fn write_boundary_runs_once_after_preflight_and_before_first_source_mutation() {
        for stale in [false, true] {
            let mut f = Fixture::new();
            if stale {
                f.snapshot.context_sha256 = [3; 32];
            }
            let store = f.store();
            let mut locks = store.lock_roots(&f.plan.roots).unwrap();
            let mut calls = 0;
            let result = locks.apply_guarded(
                &f.plan,
                &f.snapshot,
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                &mut || {
                    calls += 1;
                    f.assert_original();
                    let (temporary, _) = recordless_temporary(&f.cache);
                    assert!(!f.source.path().join(temporary).exists());
                    Ok(())
                },
            );
            if stale {
                assert!(result.is_err());
                assert_eq!(calls, 0);
            } else {
                assert_eq!(result.unwrap().status, Status::Committed);
                assert_eq!(calls, 1);
            }
        }
    }

    #[test]
    fn rejected_write_boundary_leaves_original_namespace_and_no_recovery_lock() {
        let f = Fixture::new();
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        let receipt = locks
            .apply_guarded(
                &f.plan,
                &f.snapshot,
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                &mut || bail!("write boundary refused"),
            )
            .unwrap();
        assert_eq!(receipt.status, Status::RolledBack);
        f.assert_original();
        assert_eq!(
            locks.current_tree().unwrap().len(),
            f.snapshot.files.len() + f.snapshot.directories.len()
        );
        drop(locks);
        assert!(store.pending(&f.plan.roots, f.plan.context_sha256).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn archived_source_bytes_and_records_are_private_to_the_current_user() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        let receipt = locks.apply(&f.plan, &f.snapshot, &AtomicBool::new(false)).unwrap();
        assert_eq!(receipt.status, Status::Committed);
        for directory in ["objects", "journals"] {
            for entry in
                std::fs::read_dir(f.cache.path().join("metadata-rename").join(directory)).unwrap()
            {
                let entry = entry.unwrap();
                assert_eq!(
                    entry.metadata().unwrap().permissions().mode() & 0o777,
                    0o600,
                    "{}",
                    entry.path().display()
                );
            }
        }
    }

    #[test]
    fn recovery_preview_reads_original_bytes_without_altering_pending_source_or_journal() {
        let f = Fixture::new();
        let store = f.store();
        let locks = store.lock_roots(&f.plan.roots).unwrap();
        let mut record = locks.prepare(&f.plan, &f.snapshot).unwrap();
        let mut hash = locks.save(&record, None).unwrap();
        locks.execute(&mut record, &mut hash, false).unwrap();
        let (before, bytes_hash) = locks.load(&record.transaction).unwrap();
        let before_tree = locks.current_tree().unwrap();
        drop(locks);
        let receipt = store
            .preview_recovery(&record.transaction, &f.plan.roots, f.plan.context_sha256)
            .unwrap();
        assert_eq!(receipt.status, Status::RolledBack);
        for file in receipt.known_files {
            let original =
                f.snapshot.files.iter().find(|v| v.relative_path == file.current_path).unwrap();
            assert_eq!(file.bytes, original.bytes);
            assert_eq!(file.current_path, file.old_path);
        }
        let locks = store.lock_roots(&f.plan.roots).unwrap();
        assert_eq!(locks.load(&record.transaction).unwrap().1, bytes_hash);
        assert_eq!(locks.current_tree().unwrap(), before_tree);
        assert_eq!(before.applied, 1);
        drop(locks);
        let mut calls = 0;
        let receipt = store
            .recover_guarded(&record.transaction, &f.plan.roots, f.plan.context_sha256, &mut || {
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(receipt.status, Status::RolledBack);
        f.assert_original();
    }

    fn deferred_terminal(f: &Fixture, rollback: bool) -> Receipt {
        let store = f.store();
        let mut locks = store.lock_roots(&f.plan.roots).unwrap();
        locks.defer_publication();
        let mut source_callbacks = 0;
        let receipt = locks
            .apply_with_observers(
                &f.plan,
                &f.snapshot,
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                &mut || {
                    let record: Record = serde_json::from_slice(
                        &std::fs::read(
                            f.cache
                                .path()
                                .join(format!("metadata-rename/journals/{}.json", f.transaction())),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    assert!(record.publication_pending, "debt precedes the first source mutation");
                    source_callbacks += 1;
                    Ok(())
                },
                &mut |applied| {
                    if rollback && applied == 1 {
                        bail!("rollback before model publication");
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert!(source_callbacks > 0);
        assert_eq!(receipt.status, if rollback { Status::RolledBack } else { Status::Committed });
        receipt
    }

    #[test]
    fn deferred_terminal_publication_survives_drop_and_reopens_without_source_undo() {
        for rollback in [false, true] {
            let f = Fixture::new();
            let receipt = deferred_terminal(&f, rollback);
            let store = f.store();
            let pending = store.pending(&f.plan.roots, f.plan.context_sha256).unwrap();
            assert_eq!(
                pending,
                vec![PendingJournal {
                    transaction: receipt.transaction.clone(),
                    recovery_approved: true,
                    publication_only: true,
                }]
            );
            let preview = store
                .preview_recovery(&receipt.transaction, &f.plan.roots, f.plan.context_sha256)
                .unwrap();
            assert_eq!(
                preview, receipt,
                "publication preview is the final tree, never original undo bytes"
            );
            let before = store.lock_roots(&f.plan.roots).unwrap().current_tree().unwrap();
            let recovered = store
                .recover_guarded(
                    &receipt.transaction,
                    &f.plan.roots,
                    f.plan.context_sha256,
                    &mut || panic!("terminal publication recovery must not mutate source"),
                )
                .unwrap();
            assert_eq!(recovered, receipt);
            assert_eq!(store.lock_roots(&f.plan.roots).unwrap().current_tree().unwrap(), before);
            assert_eq!(store.pending(&f.plan.roots, f.plan.context_sha256).unwrap(), pending);
            {
                let mut locks = store.lock_roots(&f.plan.roots).unwrap();
                let error = locks.apply(&f.plan, &f.snapshot, &AtomicBool::new(false)).unwrap_err();
                assert!(error.to_string().contains("pending journal"));
            }
            assert!(store
                .complete_publication(&receipt.transaction, &f.plan.roots, [9; 32])
                .is_err());
            assert_eq!(store.pending(&f.plan.roots, f.plan.context_sha256).unwrap(), pending);
            store
                .complete_publication(&receipt.transaction, &f.plan.roots, f.plan.context_sha256)
                .unwrap();
            store
                .complete_publication(&receipt.transaction, &f.plan.roots, f.plan.context_sha256)
                .unwrap();
            assert!(store.pending(&f.plan.roots, f.plan.context_sha256).unwrap().is_empty());
            assert_eq!(
                store.recover(&receipt.transaction, &f.plan.roots, f.plan.context_sha256).unwrap(),
                receipt
            );
            if rollback {
                f.assert_original();
            }
        }
    }

    #[test]
    fn deferred_terminal_foreign_versions_never_turn_commit_into_undo_recovery() {
        for rollback in [false, true] {
            let f = Fixture::new();
            let receipt = deferred_terminal(&f, rollback);
            let path =
                f.source.path().join(if rollback { "Partners.xml" } else { "Customers.xml" });
            std::fs::write(&path, b"foreign after physical terminal").unwrap();
            let store = f.store();
            assert!(store
                .preview_recovery(&receipt.transaction, &f.plan.roots, f.plan.context_sha256)
                .is_err());
            assert!(store
                .recover_guarded(
                    &receipt.transaction,
                    &f.plan.roots,
                    f.plan.context_sha256,
                    &mut || panic!("foreign terminal bytes cannot be undone"),
                )
                .is_err());
            assert!(store
                .complete_publication(&receipt.transaction, &f.plan.roots, f.plan.context_sha256)
                .is_err());
            let locks = store.lock_roots(&f.plan.roots).unwrap();
            let (record, _) = locks.load(&receipt.transaction).unwrap();
            assert!(record.publication_pending);
            assert_eq!(record.terminal.unwrap().status, receipt.status);
            assert_eq!(std::fs::read(path).unwrap(), b"foreign after physical terminal");
            drop(locks);
            assert!(
                store.pending(&f.plan.roots, f.plan.context_sha256).unwrap()[0].publication_only
            );
        }
    }

    #[test]
    fn runtime_recovery_of_a_legacy_incomplete_record_defers_its_rollback_publication() {
        let f = Fixture::new();
        let store = f.store();
        let locks = store.lock_roots(&f.plan.roots).unwrap();
        let mut record = locks.prepare(&f.plan, &f.snapshot).unwrap();
        let mut hash = locks.save(&record, None).unwrap();
        locks.execute(&mut record, &mut hash, false).unwrap();
        let path =
            f.cache.path().join(format!("metadata-rename/journals/{}.json", record.transaction));
        let mut legacy = serde_json::to_value(&record).unwrap();
        legacy.as_object_mut().unwrap().remove("publication_pending");
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        drop(locks);
        assert!(!store.pending(&f.plan.roots, f.plan.context_sha256).unwrap()[0].publication_only);
        let mut writes = 0;
        let receipt = store
            .recover_guarded(&record.transaction, &f.plan.roots, f.plan.context_sha256, &mut || {
                let persisted: Record =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                assert!(
                    persisted.publication_pending,
                    "runtime rollback debt precedes source writes"
                );
                writes += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(writes, 1);
        assert_eq!(receipt.status, Status::RolledBack);
        f.assert_original();
        assert!(
            f.store().pending(&f.plan.roots, f.plan.context_sha256).unwrap()[0].publication_only
        );
        store
            .complete_publication(&record.transaction, &f.plan.roots, f.plan.context_sha256)
            .unwrap();
        assert!(store.pending(&f.plan.roots, f.plan.context_sha256).unwrap().is_empty());
    }

    #[test]
    fn failed_publication_completion_keeps_the_durable_terminal_pending() {
        let f = Fixture::new();
        let receipt = deferred_terminal(&f, false);
        let path =
            f.cache.path().join(format!("metadata-rename/journals/{}.json", receipt.transaction));
        let before = std::fs::read(&path).unwrap();
        let mut constrained = f.store();
        constrained.bounds.record_bytes = before.len() as u64;
        // Serializing false needs one more byte than true; the clear cannot fit its record budget.
        assert!(constrained
            .complete_publication(&receipt.transaction, &f.plan.roots, f.plan.context_sha256)
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            f.store().pending(&f.plan.roots, f.plan.context_sha256).unwrap()[0].publication_only
        );
        f.store()
            .complete_publication(&receipt.transaction, &f.plan.roots, f.plan.context_sha256)
            .unwrap();
        assert!(f.store().pending(&f.plan.roots, f.plan.context_sha256).unwrap().is_empty());
    }

    #[test]
    fn generic_terminal_records_without_publication_context_remain_idempotent() {
        let f = Fixture::new();
        let store = f.store();
        let receipt = store
            .lock_roots(&f.plan.roots)
            .unwrap()
            .apply(&f.plan, &f.snapshot, &AtomicBool::new(false))
            .unwrap();
        let path =
            f.cache.path().join(format!("metadata-rename/journals/{}.json", receipt.transaction));
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("publication_pending");
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(store.pending(&f.plan.roots, f.plan.context_sha256).unwrap().is_empty());
        assert_eq!(
            store.recover(&receipt.transaction, &f.plan.roots, f.plan.context_sha256).unwrap(),
            receipt
        );
        assert!(store.pending(&f.plan.roots, f.plan.context_sha256).unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "subprocess entry point; parent owns both disposable roots"]
    fn abort_after_first_installed_source_operation_child() {
        let Some(source) = std::env::var_os("BSL_TEST_METADATA_ABORT_SOURCE") else { return };
        let cache = std::env::var_os("BSL_TEST_METADATA_ABORT_CACHE").unwrap();
        let transaction = std::env::var("BSL_TEST_METADATA_ABORT_TRANSACTION").unwrap();
        valid_id(&transaction).unwrap();
        // SAFETY: PR_SET_DUMPABLE with 0 changes only this dedicated child's core-dump policy.
        assert_eq!(
            unsafe {
                libc::prctl(
                    libc::PR_SET_DUMPABLE,
                    0 as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                )
            },
            0
        );
        let source = std::fs::canonicalize(source).unwrap();
        let approved = MetadataPlanRoot {
            id: "main".into(),
            source_path: source.clone(),
            canonical_path: source,
            role: MetadataRootRole::Main,
            writable: true,
            case_sensitive: true,
        };
        let store = fixture_store_at(Path::new(&cache));
        let locks = store.lock_roots(&[approved]).unwrap();
        let (mut record, mut hash) = locks.load(&transaction).unwrap();
        assert_eq!(record.applied, 0);
        locks.execute(&mut record, &mut hash, false).unwrap();
        assert_eq!(record.applied, 1);
        std::process::abort();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn os_process_abort_after_first_installed_source_operation_recovers_full_original_tree() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let f = Fixture::new();
        let store = f.store();
        let locks = store.lock_roots(&f.plan.roots).unwrap();
        let record = locks.prepare(&f.plan, &f.snapshot).unwrap();
        locks.save(&record, None).unwrap();
        let transaction = record.transaction;
        drop(locks);
        drop(store);
        let stderr_path = f.cache.path().join("abort-child.stderr");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "metadata_rename::journal::tests::abort_after_first_installed_source_operation_child", "--ignored", "--nocapture"])
            .env("BSL_TEST_METADATA_ABORT_SOURCE", &f.plan.roots[0].source_path)
            .env("BSL_TEST_METADATA_ABORT_CACHE", f.cache.path())
            .env("BSL_TEST_METADATA_ABORT_TRANSACTION", &transaction)
            .current_dir(f.cache.path())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .spawn().unwrap();
        let deadline = Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(30));
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("abort child did not terminate: {result:?}");
                }
            }
        };
        assert_eq!(
            status.signal(),
            Some(libc::SIGABRT),
            "child failed before its durable checkpoint: {}",
            std::fs::read_to_string(stderr_path).unwrap()
        );
        assert!(!status.core_dumped());
        let reopened = f.store();
        let locks = reopened.lock_roots(&f.plan.roots).unwrap();
        let (interrupted, _) = locks.load(&transaction).unwrap();
        assert_eq!(
            interrupted.applied, 1,
            "the child performed and durably recorded one source operation"
        );
        assert!(interrupted.terminal.is_none());
        let Step::Text { root, path, before, after, .. } = &interrupted.steps[0] else {
            panic!("text first")
        };
        let actual = std::fs::read(f.source.path().join(path)).unwrap();
        assert_eq!(actual, locks.read_blob(after).unwrap());
        assert_ne!(actual, locks.read_blob(before).unwrap());
        assert_eq!(root, "main");
        drop(locks);
        let receipt = reopened.recover(&transaction, &f.plan.roots, f.plan.context_sha256).unwrap();
        assert_eq!(receipt.status, Status::RolledBack);
        f.assert_original();
        let expected_paths: BTreeSet<_> = f
            .snapshot
            .files
            .iter()
            .map(|file| &file.relative_path)
            .chain(f.snapshot.directories.iter().map(|directory| &directory.relative_path))
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let locks = reopened.lock_roots(&f.plan.roots).unwrap();
        let actual_paths: BTreeSet<_> =
            locks.current_tree().unwrap().into_keys().map(|(_, path)| path).collect();
        assert_eq!(
            actual_paths, expected_paths,
            "no staging or resource names remain after rollback"
        );
        drop(locks);
        assert!(reopened.pending(&f.plan.roots, f.plan.context_sha256).unwrap().is_empty());
        assert_eq!(
            reopened.recover(&transaction, &f.plan.roots, f.plan.context_sha256).unwrap(),
            receipt
        );
    }
}
