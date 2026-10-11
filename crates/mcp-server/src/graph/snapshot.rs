use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::change_hub::{ChangeEntry, ChangeKind};
use crate::graph_query::GraphDb;

#[cfg(test)]
use super::state::ReloadState;
use super::state::{lock_recover, GraphState, Published};
#[cfg(test)]
use super::types::Freshness;
use super::types::GraphStatus;

/// How often the query-path freshness fold must come from a real walk instead of the
/// event-maintained map. Bounds how long a change the hub cannot observe can keep
/// freshness wrong.
pub(super) const WALK_VERIFY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// `canonical path → (mtime nanos, len)` state maintained from hub deliveries and
/// periodically re-anchored by a complete walk. `topology` carries the topology
/// hash observed at the last walk: hub deliveries patch only file stats, and a
/// config-file delivery (which may change the topology) drops the whole map so
/// the next check walks — and re-derives the project — instead of folding a
/// stale topology under fresh file stats.
#[derive(Default)]
pub(super) struct FpMapState {
    /// The same durable file identity and complete byte hash used by the graph database.
    /// Physical paths and metadata are deliberately absent: a moved tree must reuse this map,
    /// while a same-stat byte edit must invalidate it.
    pub(super) map: Option<std::collections::BTreeMap<bsl_search::FileKey, [u8; 32]>>,
    pub(super) roots: Option<bsl_search::WorkspaceRoots>,
    pub(super) walked_at: Option<Instant>,
    pub(super) topology: u64,
    /// Verdict of the walk that anchored `map`: hub deliveries patch stats but
    /// cannot re-judge completeness, so the last walk's verdict rides along.
    pub(super) clean: bool,
}

/// Throttled cache of the last on-disk fingerprint scan. Guarded by its own mutex
/// held across the walk, so concurrent callers serialize onto one scan per window.
pub(super) struct ScanCache {
    pub(super) at: Instant,
    /// The hub position this scan can vouch for: read BEFORE the walk, so every fact at or
    /// below it reached the hub before the disk was looked at. A comparison that answers by a
    /// number read later answers facts its own walk never saw — which is the same defect as a
    /// build publishing a proof wider than its admission.
    pub(super) observed_through: u64,
    pub(super) disk_fp: crate::graph_db::GraphFp,
    /// Whether the scan behind `disk_fp` covered the whole tree — the reload
    /// decision needs it to retire a `force_stale` build once the tree heals.
    pub(super) clean: bool,
}

/// Every publication prepares this many independent read handles before becoming ready.
pub(crate) const SNAPSHOT_POOL_CAP: usize = 4;

/// How long a background reader waits for a pooled handle while requests hold them all.
pub(crate) const BACKGROUND_READ_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) enum BackgroundSnapshotError {
    Changed,
}

#[cfg(test)]
type SnapshotOpenHook = Box<dyn FnOnce()>;
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum BackgroundSnapshotFailure {
    Changed,
    PrepareSecondChanged,
}
#[cfg(test)]
thread_local! {
    static SNAPSHOT_OPEN_HOOK: std::cell::RefCell<Option<SnapshotOpenHook>> =
        const { std::cell::RefCell::new(None) };
    static SNAPSHOT_CHECKOUT_HOOK: std::cell::RefCell<Option<SnapshotOpenHook>> =
        const { std::cell::RefCell::new(None) };
    static REFUSE_SNAPSHOT_INSTALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn set_snapshot_install_hook(hook: SnapshotOpenHook) {
    SNAPSHOT_OPEN_HOOK.with(|slot| slot.replace(Some(hook)));
}

#[cfg(test)]
pub(super) fn refuse_snapshot_install_for_test() {
    REFUSE_SNAPSHOT_INSTALL.with(|refuse| refuse.set(true));
}

#[cfg(test)]
fn set_snapshot_checkout_hook(hook: SnapshotOpenHook) {
    SNAPSHOT_CHECKOUT_HOOK.with(|slot| slot.replace(Some(hook)));
}

/// A pooled idle read handle plus the freshness token it was opened under.
pub(super) struct PooledSnapshotEntry {
    pub(super) generation: u64,
    pub(super) fingerprint: crate::graph_db::GraphFp,
    pub(super) force_stale: bool,
    db: GraphDb,
    unread_files: usize,
}

impl PooledSnapshotEntry {
    /// Open a handle on the graph file at `path` and read the token it serves.
    fn open(path: &Path) -> anyhow::Result<Self> {
        let db = GraphDb::open(path)?;
        let (generation, fingerprint, force_stale) = db.freshness_token()?;
        let unread_files = db.unread_files();
        Ok(Self { generation, fingerprint, force_stale, db, unread_files })
    }

    fn token(&self) -> (u64, crate::graph_db::GraphFp, bool) {
        (self.generation, self.fingerprint, self.force_stale)
    }
}

#[derive(Default)]
pub(super) struct SnapshotPool {
    generation: u64,
    /// Idle handles of the installed publication.
    entries: Vec<PooledSnapshotEntry>,
    /// The root table of the installed publication, handed to every read of it.
    roots: Option<bsl_search::WorkspaceRoots>,
    /// The file the installed publication is served from, and the token every handle opened on
    /// it must read; handles past the first are opened on demand.
    source: Option<InstalledSource>,
    /// Whether any publication has been installed; before that no read is served.
    installed: bool,
    /// Whether this process's ownership lets it lend handles at all.
    admission: Admission,
    /// A replacement is being installed: new reads wait, returned handles close.
    installing: bool,
    /// Handles lent and not yet returned.
    lent: usize,
    /// Handles being opened on demand, outside the lock.
    opening: usize,
    /// Uses of the file outside the handles: inspections, preparations, copies.
    uses: usize,
    /// Why the installed file cannot be trusted, after a replacement whose outcome could not be
    /// established; nothing is served until a publication is installed again.
    unusable: Option<String>,
}

impl SnapshotPool {
    fn handles(&self) -> usize {
        self.entries.len() + self.lent + self.opening
    }

    fn quiet(&self) -> bool {
        self.lent == 0 && self.opening == 0 && self.uses == 0
    }
}

/// The file an installed publication is served from: a handle opened on demand must find the
/// same file — not another one renamed to its name — holding the same publication.
#[derive(Clone)]
struct InstalledSource {
    path: PathBuf,
    identity: GraphPathIdentity,
    token: (u64, crate::graph_db::GraphFp, bool),
}

/// Whether the store lends handles, as this process's ownership of the graph file allows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Admission {
    #[default]
    Open,
    /// Ownership could not be confirmed a moment ago: new reads wait, nothing is closed.
    Paused,
    /// Ownership is gone: no handle is lent again, and each one closes as it comes back.
    Retired,
}

/// Why a read of the published graph was not served. Never an empty answer: a caller that
/// gets one reports it, or keeps its obligation for a later pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GraphReadError {
    /// No publication is installed, or every handle stayed in use for the whole wait.
    Unavailable,
    /// The installed publication is not the generation the caller planned against.
    Changed,
    /// Reads are held back for a moment: a replacement is installed, or the file's owner is
    /// being confirmed or awaited.
    Busy,
    /// Another process owns the graph file now; this one reads it no more.
    OwnerChanged,
}

impl std::fmt::Display for GraphReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "graph is not available for reading",
            Self::Changed => "the published graph generation changed",
            Self::Busy => "the graph is busy; retry shortly",
            Self::OwnerChanged => super::types::SUPERSEDED_GRAPH_ERROR,
        })
    }
}

impl std::error::Error for GraphReadError {}

/// What a database declares in its `meta`, read without validating it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct OnDiskIdentity {
    pub(super) schema_version: Option<u32>,
    pub(super) publication_id: Option<String>,
}

/// What the database at `path` says about its own format and identity, read without
/// validating it. `None`: no database is there. A database whose `meta` will not read declares
/// nothing, which is not a newer format.
pub(super) fn on_disk_identity(path: &Path) -> std::io::Result<Option<OnDiskIdentity>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Ok(Some(OnDiskIdentity::default()));
    };
    let meta = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0)).ok()
    };
    Ok(Some(OnDiskIdentity {
        schema_version: meta("schema_version").and_then(|value| value.parse().ok()),
        publication_id: meta("publication_id"),
    }))
}

/// Finish what an interrupted writer left in the graph file's rollback journal, before any
/// read-only handle opens it: a read-only connection cannot roll a hot journal back, and the
/// journal is never deleted by hand. A connection with write access rolls it back on its first
/// read and removes it.
pub(super) fn recover_hot_journal(path: &Path) -> anyhow::Result<()> {
    let mut journal = path.as_os_str().to_owned();
    journal.push("-journal");
    let hot = std::fs::metadata(Path::new(&journal)).is_ok_and(|metadata| metadata.len() > 0);
    if !hot || !path.try_exists()? {
        return Ok(());
    }
    tracing::warn!(path = %path.display(), "graph file has an interrupted journal; recovering it");
    let conn = rusqlite::Connection::open(path)?;
    conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get::<_, i64>(0))?;
    Ok(())
}

/// One use of the graph file announced to its [`GraphStore`], ended when dropped.
pub(crate) struct FileUse {
    store: GraphStore,
}

impl Drop for FileUse {
    fn drop(&mut self) {
        let mut pool = lock_recover(&self.store.shared.pool);
        pool.uses = pool.uses.saturating_sub(1);
        drop(pool);
        self.store.shared.returned.notify_all();
    }
}

/// The graph file emptied of this process's handles for a replacement to be renamed in. New
/// reads wait while it is held; dropping it without installing a publication lends again from
/// whatever file is in place.
pub(super) struct ReplacementPause {
    store: GraphStore,
}

impl Drop for ReplacementPause {
    fn drop(&mut self) {
        lock_recover(&self.store.shared.pool).installing = false;
        self.store.shared.returned.notify_all();
    }
}

/// What an attempt to empty the graph file of this process's handles came to.
pub(super) enum Pausing {
    Paused(ReplacementPause),
    /// A read or use of the file outlasted the wait.
    ReadersBusy,
    /// This process lost the graph; it replaces nothing.
    Retired,
}

/// What [`GraphStore::status`] can say without opening anything.
pub(crate) struct GraphStoreStatus {
    /// The installed generation, when one is.
    pub(crate) generation: Option<u64>,
    /// `force_stale` of the installed publication.
    pub(crate) idle_force_stale: Option<bool>,
}

/// How long a new read waits to be admitted while a replacement is installed or ownership is
/// confirmed.
const ADMISSION_WAIT: Duration = Duration::from_secs(2);

/// The one owner of the published graph's read handles.
///
/// Every read of the live graph file borrows a handle here and gives it back when the read
/// returns — also on an error or an unwind — so the handles open on the file are always the
/// ones a publication can account for. Handles are opened on the file itself, never on a copy,
/// at most [`SNAPSHOT_POOL_CAP`] of them and only as reads need them.
#[derive(Clone, Default)]
pub(crate) struct GraphStore {
    shared: Arc<StoreShared>,
}

#[derive(Default)]
struct StoreShared {
    pool: Mutex<SnapshotPool>,
    /// Signalled whenever a handle returns, a use ends, or lending is allowed again.
    returned: Condvar,
}

impl GraphStore {
    /// Run `op` against the installed publication and return what it produced.
    ///
    /// `expected` names the generation the caller planned against; another one is
    /// [`GraphReadError::Changed`]. `wait` bounds how long to wait for a handle while all of
    /// them are in use; a read held back by a replacement or an ownership check waits up to
    /// [`ADMISSION_WAIT`] in any case. The borrowed view cannot outlive `op`.
    pub(crate) fn read<R>(
        &self,
        expected: Option<u64>,
        wait: Duration,
        op: impl FnOnce(&GraphSnapshot) -> R,
    ) -> Result<R, GraphReadError> {
        let snapshot = self.checkout(expected, wait)?;
        Ok(op(&snapshot))
    }

    /// The installed generation and whether it reads as stale. Never waits: a pool busy with a
    /// checkout answers `None`.
    pub(crate) fn status(&self) -> Option<GraphStoreStatus> {
        let pool = self.shared.pool.try_lock().ok()?;
        let served =
            pool.installed && pool.unusable.is_none() && pool.admission != Admission::Retired;
        Some(GraphStoreStatus {
            generation: served.then_some(pool.generation),
            idle_force_stale: served
                .then(|| pool.source.as_ref().map(|source| source.token.2))
                .flatten(),
        })
    }

    /// Whether an installed snapshot exists and is servable.
    pub(crate) fn has_installed(&self) -> bool {
        let pool = crate::graph::state::lock_recover(&self.shared.pool);
        pool.installed && pool.unusable.is_none() && pool.admission != Admission::Retired
    }

    /// Why the installed file is not served, after a replacement whose outcome is unknown.
    pub(crate) fn unusable_reason(&self) -> Option<String> {
        lock_recover(&self.shared.pool).unusable.clone()
    }

    pub(super) fn checkout(
        &self,
        expected: Option<u64>,
        wait: Duration,
    ) -> Result<GraphSnapshot, GraphReadError> {
        #[cfg(test)]
        SNAPSHOT_CHECKOUT_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        let started = Instant::now();
        let handle_deadline = started.checked_add(wait);
        let admission_deadline = started.checked_add(wait.max(ADMISSION_WAIT));
        let mut pool = lock_recover(&self.shared.pool);
        loop {
            if pool.admission == Admission::Retired {
                return Err(GraphReadError::OwnerChanged);
            }
            let held = pool.admission == Admission::Paused || pool.installing;
            if !held {
                if !pool.installed || pool.unusable.is_some() {
                    return Err(GraphReadError::Unavailable);
                }
                let generation = pool.generation;
                if expected.is_some_and(|expected| expected != generation) {
                    return Err(GraphReadError::Changed);
                }
                // Entries of a superseded generation are dropped here, never served.
                while let Some(entry) = pool.entries.pop() {
                    if entry.generation == generation {
                        pool.lent += 1;
                        let workspace_roots = pool.roots.clone();
                        return Ok(GraphSnapshot::lent(entry, self.clone(), workspace_roots));
                    }
                }
                if pool.handles() < SNAPSHOT_POOL_CAP {
                    if let Some(source) = pool.source.clone() {
                        let (path, token) = (source.path.clone(), source.token);
                        pool.opening += 1;
                        drop(pool);
                        let opened = std::panic::catch_unwind(|| {
                            GraphPathIdentity::read(&path).map_err(anyhow::Error::from).and_then(
                                |identity| {
                                    anyhow::ensure!(
                                        identity == source.identity,
                                        "another file is at the graph's path"
                                    );
                                    PooledSnapshotEntry::open(&path)
                                },
                            )
                        });
                        pool = lock_recover(&self.shared.pool);
                        pool.opening -= 1;
                        self.shared.returned.notify_all();
                        let opened = match opened {
                            Ok(opened) => opened,
                            Err(panic) => {
                                drop(pool);
                                std::panic::resume_unwind(panic);
                            }
                        };
                        if pool.source.as_ref().is_none_or(|served| served.token != token) {
                            // Another publication was installed meanwhile: ask again.
                            continue;
                        }
                        match opened {
                            Ok(entry)
                                if entry.token() == token
                                    && pool
                                        .source
                                        .as_ref()
                                        .is_some_and(|served| served.token == token) =>
                            {
                                if pool.admission == Admission::Open && !pool.installing {
                                    pool.lent += 1;
                                    let workspace_roots = pool.roots.clone();
                                    return Ok(GraphSnapshot::lent(
                                        entry,
                                        self.clone(),
                                        workspace_roots,
                                    ));
                                }
                                continue;
                            }
                            Ok(entry) => {
                                tracing::warn!(
                                    path = %path.display(),
                                    found = ?entry.token(),
                                    expected = ?token,
                                    "the graph file no longer holds the installed publication"
                                );
                                return Err(GraphReadError::Changed);
                            }
                            Err(error) => {
                                tracing::warn!(path = %path.display(), %error, "could not open a graph read handle");
                                return Err(GraphReadError::Unavailable);
                            }
                        }
                    }
                }
            }
            let deadline = if held { admission_deadline } else { handle_deadline };
            let left = deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::MAX);
            if left.is_zero() {
                return Err(if held { GraphReadError::Busy } else { GraphReadError::Unavailable });
            }
            pool = self
                .shared
                .returned
                .wait_timeout(pool, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn give_back(&self, entry: PooledSnapshotEntry) {
        let mut pool = lock_recover(&self.shared.pool);
        let keep = pool.admission != Admission::Retired
            && !pool.installing
            && pool.generation == entry.generation
            && pool.entries.len() < SNAPSHOT_POOL_CAP;
        if keep {
            pool.entries.push(entry);
        } else {
            // Closed while it still counts as lent: a replacement waiting for the count to reach
            // zero must not rename the file while this handle is open on it.
            drop(pool);
            drop(entry);
            pool = lock_recover(&self.shared.pool);
        }
        pool.lent = pool.lent.saturating_sub(1);
        drop(pool);
        // Every waiter re-checks: one whose deadline has just passed must not swallow the
        // wake-up another could use, and a replacement or retirement waits for the last return.
        self.shared.returned.notify_all();
    }

    /// Announce a use of the graph file outside the lent handles — an inspection, a pool being
    /// prepared, a copy for a patch. A retirement waits for it as for a lent read, and none
    /// starts once the store is retired.
    pub(crate) fn use_file(&self) -> Result<FileUse, GraphReadError> {
        let mut pool = lock_recover(&self.shared.pool);
        if pool.admission == Admission::Retired {
            return Err(GraphReadError::OwnerChanged);
        }
        pool.uses += 1;
        Ok(FileUse { store: self.clone() })
    }

    /// Take the file at `path` for the one this store serves after this process rewrote it in
    /// place and rolled the write back: its modification time moved, its contents did not.
    pub(super) fn accept_rewritten_file(&self, path: &Path, _pause: &ReplacementPause) {
        let Ok(identity) = GraphPathIdentity::read(path) else {
            self.mark_unusable("graph unavailable: the graph file cannot be looked at".to_owned());
            return;
        };
        let mut pool = lock_recover(&self.shared.pool);
        if let Some(source) = pool.source.as_mut().filter(|source| source.path == path) {
            source.identity = identity;
        }
    }

    /// Whether the store is retired and every handle and use of the file has come back.
    pub(crate) fn retired_and_returned(&self) -> bool {
        let pool = lock_recover(&self.shared.pool);
        pool.admission == Admission::Retired && pool.quiet()
    }

    /// Allow, hold back or end lending. Retirement is final: the idle handles close at once,
    /// and the lent ones as they return.
    pub(crate) fn set_admission(&self, admission: Admission) {
        let mut pool = lock_recover(&self.shared.pool);
        if pool.admission == Admission::Retired {
            return;
        }
        pool.admission = admission;
        let closing = if admission == Admission::Retired {
            std::mem::take(&mut pool.entries)
        } else {
            Vec::new()
        };
        drop(pool);
        drop(closing);
        self.shared.returned.notify_all();
    }

    /// Wait until every lent handle and use of the file has come back. No deadline: an active
    /// read is never cut.
    pub(crate) fn wait_until_returned(&self) {
        let mut pool = lock_recover(&self.shared.pool);
        while !pool.quiet() {
            pool =
                self.shared.returned.wait(pool).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Hold new reads back and wait, up to `readers`, for every lent handle and every use of
    /// the file to end; then close the idle handles, so this process has nothing open on the file
    /// a replacement is renamed over. A read that outlasts the wait resumes lending with nothing
    /// closed.
    pub(super) fn pause_for_replacement(&self, readers: Duration) -> Pausing {
        let deadline = Instant::now().checked_add(readers);
        let mut pool = lock_recover(&self.shared.pool);
        if pool.admission == Admission::Retired {
            return Pausing::Retired;
        }
        pool.installing = true;
        while !pool.quiet() {
            let left = deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::MAX);
            if left.is_zero() {
                pool.installing = false;
                drop(pool);
                self.shared.returned.notify_all();
                return Pausing::ReadersBusy;
            }
            pool = self
                .shared
                .returned
                .wait_timeout(pool, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        let closing = std::mem::take(&mut pool.entries);
        drop(pool);
        drop(closing);
        Pausing::Paused(ReplacementPause { store: self.clone() })
    }

    /// Serve `entry`'s publication from the file at `path` from now on; further handles open on
    /// demand. Returns the unread-module count the new publication declares.
    fn install(
        &self,
        path: PathBuf,
        identity: GraphPathIdentity,
        entry: PooledSnapshotEntry,
        roots: Option<bsl_search::WorkspaceRoots>,
    ) -> Option<usize> {
        let mut pool = lock_recover(&self.shared.pool);
        if pool.admission == Admission::Retired {
            // A process that lost the graph serves no new publication: the handle closes here.
            return None;
        }
        let unread_files = entry.unread_files;
        let closing = std::mem::take(&mut pool.entries);
        pool.generation = entry.generation;
        pool.source = Some(InstalledSource { path, identity, token: entry.token() });
        pool.entries.push(entry);
        pool.roots = roots;
        pool.installed = true;
        pool.unusable = None;
        drop(pool);
        drop(closing);
        self.shared.returned.notify_all();
        Some(unread_files)
    }

    /// Serve nothing until a publication is installed again: a replacement left the file in a
    /// state nobody could establish.
    pub(super) fn mark_unusable(&self, reason: String) {
        let mut pool = lock_recover(&self.shared.pool);
        pool.unusable = Some(reason);
        let closing = std::mem::take(&mut pool.entries);
        drop(pool);
        drop(closing);
        self.shared.returned.notify_all();
    }

    #[cfg(test)]
    pub(super) fn lock_pool(&self) -> std::sync::MutexGuard<'_, SnapshotPool> {
        lock_recover(&self.shared.pool)
    }

    /// A store serving the graph file at `path` as it stands, for a test that drives a search
    /// consumer against a database it built by hand.
    #[cfg(test)]
    pub(crate) fn serving_file_for_test(
        path: &Path,
        roots: Option<bsl_search::WorkspaceRoots>,
    ) -> anyhow::Result<Self> {
        let identity = GraphPathIdentity::read(path)?;
        let entry = PooledSnapshotEntry::open(path)?;
        let store = Self::default();
        store.install(path.to_path_buf(), identity, entry, roots);
        Ok(store)
    }
}

impl std::ops::Deref for SnapshotPool {
    type Target = Vec<PooledSnapshotEntry>;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl std::ops::DerefMut for SnapshotPool {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct WindowsFileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct WindowsFileInformation {
    attributes: u32,
    creation_time: WindowsFileTime,
    last_access_time: WindowsFileTime,
    last_write_time: WindowsFileTime,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "GetFileInformationByHandle"]
    fn get_file_information_by_handle(
        file: std::os::windows::io::RawHandle,
        information: *mut WindowsFileInformation,
    ) -> i32;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GraphPathIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume_serial_number: u32,
    #[cfg(windows)]
    file_index: u64,
}

impl GraphPathIdentity {
    fn read(path: &Path) -> std::io::Result<Self> {
        #[cfg(windows)]
        let (metadata, volume_serial_number, file_index) = {
            use std::os::windows::io::AsRawHandle;

            let file = std::fs::File::open(path)?;
            let metadata = file.metadata()?;
            let mut info = WindowsFileInformation::default();
            // SAFETY: `file` owns a live handle and `info` is valid writable storage.
            if unsafe { get_file_information_by_handle(file.as_raw_handle(), &mut info) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            (
                metadata,
                info.volume_serial_number,
                (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low),
            )
        };
        #[cfg(not(windows))]
        let metadata = std::fs::metadata(path)?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(windows)]
            volume_serial_number,
            #[cfg(windows)]
            file_index,
        })
    }
}

pub(super) struct PreparedSnapshotPool {
    /// The one handle opened and checked before the publication is installed; the rest of the
    /// pool opens on demand once it is.
    entry: Option<PooledSnapshotEntry>,
    path: PathBuf,
    /// The artefact's own unread set, read STRICTLY and bound to the generation validated
    /// above. `None` when the metadata would not read: that is a failure to look, and it says
    /// nothing at all about what this publication owes.
    declared_unread: Option<Vec<bsl_search::FileKey>>,
    path_identity: GraphPathIdentity,
    expected_generation: u64,
    expected_fingerprint: crate::graph_db::GraphFp,
    expected_force_stale: bool,
    /// The file stays in use while this handle is prepared and not yet installed.
    _file_use: FileUse,
    /// Reads held back since a replacement was renamed in, until this pool is installed.
    pause: Option<ReplacementPause>,
}

impl PreparedSnapshotPool {
    /// Keep new reads waiting until this pool is installed: the file under it was just
    /// replaced, and nothing of the previous publication may be lent in between.
    pub(super) fn hold_reads(&mut self, pause: ReplacementPause) {
        self.pause = Some(pause);
    }

    /// What the artefact this pool holds declares unread, read strictly at prepare time.
    pub(super) fn declared_unread(&self) -> Option<&[bsl_search::FileKey]> {
        self.declared_unread.as_deref()
    }

    /// The publication id of the prepared candidate, read from its meta table if opened.
    pub(super) fn publication_id(&self) -> Option<String> {
        self.entry.as_ref().and_then(|e| e.db.publication_id().ok())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SnapshotInstallError {
    Changed,
    Operation(String),
}

#[derive(Debug)]
pub(super) enum SnapshotPrepareError {
    Changed,
    Open(anyhow::Error),
}

fn prepare_path_identity(path: &Path) -> Result<GraphPathIdentity, SnapshotPrepareError> {
    GraphPathIdentity::read(path).map_err(classify_prepare_identity_error)
}

fn classify_prepare_identity_error(error: std::io::Error) -> SnapshotPrepareError {
    if error.kind() == std::io::ErrorKind::NotFound {
        SnapshotPrepareError::Changed
    } else {
        SnapshotPrepareError::Open(error.into())
    }
}

/// A served graph handle plus the freshness token it was built at. Capturing the
/// generation/fingerprint at snapshot time (not at response time) keeps the
/// envelope's `revision`/`stale` consistent with the data actually returned, even
/// if a reload publishes a newer generation while the query runs. The handle is an
/// own read-only connection opened against the on-disk SQLite graph.
pub(crate) struct GraphSnapshot {
    pub graph: PooledGraphDb,
    pub(super) generation: u64,
    pub(super) fingerprint: crate::graph_db::GraphFp,
    pub(super) force_stale: bool,
    /// Modules this artefact was built without being able to read. Makes `stale`
    /// true — the graph is missing their nodes and edges — WITHOUT making
    /// `wants_reload` true, since rebuilding cannot read them either.
    unread_files: usize,
    /// The root table this workspace publishes, for turning a node's stored file path into
    /// a `(root_id, path)` pair. `None` on a boot that published a cached graph before the
    /// project was loaded — a real serving state, not a test-only one.
    workspace_roots: Option<bsl_search::WorkspaceRoots>,
}

impl GraphSnapshot {
    fn lent(
        entry: PooledSnapshotEntry,
        store: GraphStore,
        workspace_roots: Option<bsl_search::WorkspaceRoots>,
    ) -> Self {
        Self {
            generation: entry.generation,
            fingerprint: entry.fingerprint,
            force_stale: entry.force_stale,
            unread_files: entry.unread_files,
            graph: PooledGraphDb { entry: Some(entry), store },
            workspace_roots,
        }
    }

    /// The generation this view reads.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Modules this artefact could not read when it was built or last patched.
    pub(crate) fn unread_files(&self) -> usize {
        self.unread_files
    }

    /// The root table, when this snapshot has one.
    pub(crate) fn workspace_roots(&self) -> Option<&bsl_search::WorkspaceRoots> {
        self.workspace_roots.as_ref()
    }
}

/// A read handle checked out of (and returned to) a [`GraphStore`].
/// Dereferences to the underlying [`GraphDb`]; on drop the handle goes back to the
/// pool (up to [`SNAPSHOT_POOL_CAP`]) so the next query skips the multi-GB open.
pub(crate) struct PooledGraphDb {
    entry: Option<PooledSnapshotEntry>,
    store: GraphStore,
}

impl std::ops::Deref for PooledGraphDb {
    type Target = GraphDb;

    fn deref(&self) -> &GraphDb {
        &self.entry.as_ref().expect("pooled handle is present until drop").db
    }
}

impl Drop for PooledGraphDb {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.store.give_back(entry);
        }
    }
}

/// What a publication actually did, for the obligations that were outstanding when it was
/// prepared. Not a verdict — the raw facts the three positive proofs are read off.
pub(super) enum RecoveryCoverage<'a> {
    /// A build that enumerated the scope itself: every required address is either in its
    /// universe or absent from it, and the walk says whether it may speak for the whole tree.
    #[cfg(test)]
    Walked {
        scope: super::debt::RecoveryScope,
        /// Every address the walk listed, borrowed from the walk — the universe is not cloned
        /// to say what a handful of obligations were covered by.
        enumerated: &'a std::collections::HashSet<&'a str>,
        complete: bool,
        /// Whether the tree moved while this build ran. Such a build enumerated a world that
        /// no longer stands, so what it did not list is not thereby gone.
        straddled: bool,
    },
    /// The production form. Membership is compared by the same durable `(root_id, path)`
    /// identity used by the graph; physical spellings are obtained only through the roots of
    /// the candidate publication. The string form above remains a test adapter for synthetic
    /// recovery traces that predate the portable key contract.
    WalkedKeys {
        scope: super::debt::RecoveryScope,
        enumerated: &'a std::collections::HashSet<bsl_search::FileKey>,
        complete: bool,
        straddled: bool,
    },
    /// A patch that re-projected exactly these addresses. It proves nothing about absence:
    /// a point rewrite never looked at what it did not touch.
    #[cfg(test)]
    Patched { rewritten: &'a std::collections::HashSet<&'a str> },
    /// A patch's production coverage, expressed in durable file keys.
    PatchedKeys { rewritten: &'a std::collections::HashSet<bsl_search::FileKey> },
    /// No fresh coverage authority at all — a cache served as it stands, or a test adapter.
    None,
}

/// Input address accepted by the test-only compatibility wrapper and by the production
/// `FileKey` path. Production metadata always supplies the structured form; the legacy physical
/// spelling exists only for old in-process recovery traces.
pub(super) trait RecoveryDeclaredKey {
    fn file_key(&self) -> bsl_search::FileKey;
    fn legacy_physical_path(&self) -> Option<String>;
}

impl RecoveryDeclaredKey for bsl_search::FileKey {
    fn file_key(&self) -> bsl_search::FileKey {
        self.clone()
    }

    fn legacy_physical_path(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
impl RecoveryDeclaredKey for String {
    fn file_key(&self) -> bsl_search::FileKey {
        bsl_search::FileKey::configuration(self.clone())
    }

    fn legacy_physical_path(&self) -> Option<String> {
        Some(self.clone())
    }
}

/// The scope descriptor of one actually loaded project, carried with whatever that project
/// produced. `scan_roots`, never `search_roots`: what a walk can reach is what a walk was told
/// to walk.
pub(super) fn recovery_scope_of(
    project: &super::input::ProjectSnapshot,
) -> super::debt::RecoveryScope {
    super::debt::RecoveryScope::of(&project.scan_roots, &project.excluded, project.validated)
}

/// What a recovery probe learned.
///
/// Three outcomes, not two: a probe that could not take a snapshot at all learned NOTHING, and
/// reading that as "nothing has healed" pushes the next probe twice as far out for an
/// observation that never happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ProbeOutcome {
    /// The probe looked, and these are the levels it measured. Whether any of them is NEWS is
    /// the schedule's question, not the prober's: only the schedule knows what was measured
    /// last time, and a capability that was already positive is a repeat, not a healing.
    Looked {
        levels: Vec<(super::debt::Capability, super::debt::Level)>,
        /// The scope the walk actually covered, when one was owed — from the same traversal
        /// that reached the verdict beside it.
        scope: Option<super::debt::RecoveryScope>,
    },
    /// The probe could not look: the lease was held, the handle would not open, the
    /// generation moved. The pause stays where it is and the probe is owed again.
    CouldNotLook,
}

impl GraphState {
    #[cfg(test)]
    pub(crate) fn set_background_snapshot_failure_for_test(
        &self,
        failure: Option<BackgroundSnapshotFailure>,
    ) {
        self.background_snapshot_failure.store(
            match failure {
                None => 0,
                Some(BackgroundSnapshotFailure::Changed) => 1,
                Some(BackgroundSnapshotFailure::PrepareSecondChanged) => 3,
            },
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// Look at the database on disk before a publication of it is installed — whether a
    /// cached build can be served at all. The handle is closed before this returns, and it is
    /// the only open of the shared path outside the pool this graph prepares and serves.
    pub(super) fn inspect_unpublished<R>(
        &self,
        op: impl FnOnce(&GraphDb) -> R,
    ) -> anyhow::Result<R> {
        let path = self.graph_db_path().ok_or_else(|| anyhow::anyhow!("graph path unavailable"))?;
        let _use = self.store.use_file()?;
        let db = GraphDb::open(&path)?;
        Ok(op(&db))
    }

    /// Open and validate a complete request pool without holding the lease fence.
    pub(super) fn prepare_snapshot_pool(
        &self,
        expected_generation: u64,
        expected_fingerprint: crate::graph_db::GraphFp,
        expected_force_stale: bool,
    ) -> Result<PreparedSnapshotPool, SnapshotPrepareError> {
        if !self.validate_workspace_scope() {
            return Err(SnapshotPrepareError::Changed);
        }
        let path = self
            .graph_db_path()
            .ok_or_else(|| SnapshotPrepareError::Open(anyhow::anyhow!("graph path unavailable")))?;
        let file_use = self
            .store
            .use_file()
            .map_err(|error| SnapshotPrepareError::Open(anyhow::Error::from(error)))?;
        let before = prepare_path_identity(&path)?;
        #[cfg(test)]
        if self.background_snapshot_failure.load(std::sync::atomic::Ordering::SeqCst) == 3 {
            return Err(SnapshotPrepareError::Changed);
        }
        let entry = PooledSnapshotEntry::open(&path).map_err(SnapshotPrepareError::Open)?;
        if entry.token() != (expected_generation, expected_fingerprint, expected_force_stale) {
            return Err(SnapshotPrepareError::Changed);
        }
        let after = prepare_path_identity(&path)?;
        if before != after {
            return Err(SnapshotPrepareError::Changed);
        }
        // Read here, where the generation has just been checked against the expectation and
        // the path identity brackets the read: a list taken later could belong to another
        // artefact at the same name.
        let declared_unread =
            Some(entry.db.unread_keys_strict().map_err(SnapshotPrepareError::Open)?);
        Ok(PreparedSnapshotPool {
            entry: Some(entry),
            path,
            declared_unread,
            path_identity: after,
            expected_generation,
            expected_fingerprint,
            expected_force_stale,
            _file_use: file_use,
            pause: None,
        })
    }

    /// Revalidate the prepared path under a short ownership fence, then install the
    /// descriptors and readiness metadata while request snapshots are excluded.
    ///
    /// `forced_through` is the fact the ticket carried — the demand this publication is
    /// entitled to discharge — and `None` when it ran as an ordinary catch-up. It comes from
    /// the ticket fixed at the admission, never from re-reading the debts here: a builder that
    /// asks what is owed NOW answers for demands it was never admitted for.
    ///
    /// Two different numbers travel with a publication and they are not interchangeable. The
    /// SPONSOR CUTOFF (`forced_through`) says which demand paid for this build; the
    /// OBSERVATION (`Published::observed_through`) says how far the installed graph is proven
    /// to cover: initially its scan cutoff, later possibly an exact clean comparison against
    /// the same fingerprint. Marks are consumed against that frontier. A build admitted for
    /// one fact may observe no further, and it must not claim to have answered anything above
    /// either line.
    ///
    /// The debts are discharged inside the same critical section that installs the snapshot,
    /// so an observer holding `inner` — such as [`GraphState::try_claim_reload`] — sees the
    /// new publication and the discharged demand as ONE state. Discharging after the section
    /// leaves a window in which the graph reads "reloaded, and still owing a reload", and a
    /// claim landing there starts a second full rebuild of what was just published.
    ///
    /// Lock order, which this call sits inside and never inverts:
    /// publication gate → lease → `inner` → debt.
    ///
    /// Only a successful install discharges: a refused lease, a `Changed` revalidation
    /// and a failed build all leave the obligation outstanding, so the forced reload is
    /// retried rather than silently dropped.
    pub(super) fn install_prepared_snapshot(
        &self,
        mut prepared: PreparedSnapshotPool,
        published: Published,
        status: GraphStatus,
        forced_through: Option<u64>,
        recovery_through: Option<u64>,
        recovery: super::debt::RecoveryPublicationProof,
    ) -> crate::workspace_lease::LeaseOperationOutcome<(), SnapshotInstallError> {
        if !self.validate_workspace_scope() {
            return crate::workspace_lease::LeaseOperationOutcome::Released;
        }
        #[cfg(test)]
        SNAPSHOT_OPEN_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        // The install is the other half of the critical section `consume_observed_marks` and
        // `notify_published_pass` hold: they charge marks against whatever is published when
        // their hook runs, so a publication swapped in midway would take marks that were
        // cleared against another — and an unsound one would take marks no publication may
        // consume at all. Taken BEFORE the fence, never inside it: a hook running under the
        // gate publishes through the lease, so gate → lease is the only order both sides can
        // agree on.
        let _gate = crate::graph::state::lock_recover(&self.publication_gate);
        // What the ledger hands back to be thrown away. Filled inside the section and dropped
        // below it: freeing the answered obligations and the consumed proof is O(P) of
        // allocator work, and the gate, the lease and `inner` are all held in there.
        let retired = std::cell::RefCell::new(super::debt::RetiredPayload::default());
        let discard = &retired;
        let outcome = self.lease.publish_short(&mut prepared, move |prepared| {
            #[cfg(test)]
            #[allow(deprecated, reason = "test fault injection retains Rust 1.91 compatibility")]
            if REFUSE_SNAPSHOT_INSTALL.with(|refuse| refuse.replace(false))
                || self
                    .refused_installs
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |left| left.checked_sub(1),
                    )
                    .is_ok()
            {
                return Err(SnapshotInstallError::Changed);
            }
            let expected = (
                prepared.expected_generation,
                prepared.expected_fingerprint,
                prepared.expected_force_stale,
            );
            let path = self.graph_db_path().ok_or_else(|| {
                SnapshotInstallError::Operation("graph path unavailable".to_owned())
            })?;
            match GraphPathIdentity::read(&path) {
                Ok(identity) if identity == prepared.path_identity => {}
                Ok(_) => return Err(SnapshotInstallError::Changed),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SnapshotInstallError::Changed);
                }
                Err(error) => return Err(SnapshotInstallError::Operation(error.to_string())),
            }
            let validation = &prepared
                .entry
                .as_ref()
                .ok_or_else(|| SnapshotInstallError::Operation("empty snapshot pool".to_owned()))?
                .db;
            let actual = validation
                .freshness_token()
                .map_err(|error| SnapshotInstallError::Operation(error.to_string()))?;
            if actual != expected
                || (published.generation, published.fingerprint, published.force_stale) != expected
            {
                return Err(SnapshotInstallError::Changed);
            }
            let observed_through = published.observed_through;
            // What only a probe can heal: a scan that could not vouch for itself, and files
            // whose bytes could not be read. Nothing on the fact stream announces either.
            //
            // Staleness by itself is neither. A knowingly stale publication is a cache served
            // on purpose while the build that replaces it is already claimed, and its
            // fingerprint differs from disk — the ordinary comparison owes that rebuild and
            // will make it, so being behind disk asks for no probe of its own.
            //
            // What such a publication DECLARES UNREAD is a different thing and keeps every
            // obligation it always had: those are the paths its own metadata names, opened
            // here as for any other artefact, and a probe answers them one path at a time.
            // Not a walk of the whole tree — an episode over strict unread carries no scan
            // obligation, and reading it as one would refuse recovery to exactly the
            // publication that needs it.
            // Whether this artefact can vouch for itself travels WITH the proof, prepared
            // outside every lock from the strict reader. What was here instead — a fresh
            // lenient count of unread rows, read under the publication gate — answered
            // "nothing left unread" for a database that would not answer at all.
            let mut recovery = recovery;
            recovery.straddled |= published.force_stale;
            let mut inner = lock_recover(&self.inner);
            let entry =
                prepared.entry.take().expect("a prepared pool holds its handle until installed");
            inner.indexing_unread_files = self.store.install(
                prepared.path.clone(),
                prepared.path_identity.clone(),
                entry,
                published.search_roots.clone(),
            );
            inner.published = Some(published);
            inner.status = status;
            // Inside the publishing critical section, under `inner`: a reader holding it never
            // sees a publication whose debts still name what it has just discharged.
            *discard.borrow_mut() = lock_recover(&self.debt).record_publication(
                std::time::Instant::now(),
                observed_through,
                forced_through.is_some(),
                recovery_through,
                recovery,
            );
            #[cfg(test)]
            if let Some(hook) = &self.install_section_hook {
                // Still inside: the gate, the lease and `inner` are all held here.
                hook("under-locks");
            }
            Ok(())
        });
        drop(_gate);
        if prepared.pause.is_some()
            && !matches!(outcome, crate::workspace_lease::LeaseOperationOutcome::Applied(()))
        {
            // The file was already replaced: the generation in memory is no longer on disk.
            self.store.mark_unusable(
                "graph unavailable: a replaced graph file could not be installed; it is \
                 rebuilt"
                    .to_owned(),
            );
        }
        // Reads resume here, once the publication is in memory: the pause goes with the pool.
        drop(prepared);
        #[cfg(test)]
        if let Some(hook) = &self.install_section_hook {
            // Outside every lock, and before the payload goes: what the next line frees is
            // what the section would otherwise have freed with the gate held.
            hook("gate-released");
        }
        drop(retired);
        outcome
    }

    /// Borrow a handle of the published graph without waiting, if one is idle. Tests hold
    /// several at once to exhaust the pool; production reads go through [`Self::read`].
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Option<GraphSnapshot> {
        self.store.checkout(None, Duration::ZERO).ok()
    }

    /// Run a request's read against the published graph. A request never waits for a handle:
    /// with every one in use it answers "loading" at once.
    pub(crate) fn read<R>(
        &self,
        op: impl FnOnce(&GraphSnapshot) -> R,
    ) -> Result<R, GraphReadError> {
        self.store.read(None, Duration::ZERO, op)
    }

    /// Whether an installed publication is available in the store.
    pub(crate) fn has_installed_snapshot(&self) -> bool {
        self.store.has_installed()
    }

    /// Run a read for which the graph is enrichment: `op` receives `None` when no handle can
    /// be lent, and reports that state as its own rather than as an empty graph.
    pub(crate) fn read_optional<R>(&self, op: impl FnOnce(Option<&GraphSnapshot>) -> R) -> R {
        match self.store.checkout(None, Duration::ZERO) {
            Ok(snapshot) => op(Some(&snapshot)),
            Err(_) => op(None),
        }
    }

    /// Run a background consumer's read, which may wait for lease I/O to open a handle of its
    /// own when every pooled one is in use. `Applied(None)`: nothing is published.
    pub(crate) fn read_blocking<R>(
        &self,
        op: impl FnOnce(&GraphSnapshot) -> R,
    ) -> crate::workspace_lease::LeaseOperationOutcome<Option<R>, BackgroundSnapshotError> {
        use crate::workspace_lease::LeaseOperationOutcome;

        match self.snapshot_blocking() {
            LeaseOperationOutcome::Applied(snapshot) => {
                LeaseOperationOutcome::Applied(snapshot.map(|snapshot| op(&snapshot)))
            }
            LeaseOperationOutcome::OperationError(error) => {
                LeaseOperationOutcome::OperationError(error)
            }
            LeaseOperationOutcome::TransientRefusal => LeaseOperationOutcome::TransientRefusal,
            LeaseOperationOutcome::Superseded => LeaseOperationOutcome::Superseded,
            LeaseOperationOutcome::Released => LeaseOperationOutcome::Released,
        }
    }

    fn snapshot_blocking(
        &self,
    ) -> crate::workspace_lease::LeaseOperationOutcome<Option<GraphSnapshot>, BackgroundSnapshotError>
    {
        use crate::workspace_lease::{LeaseOperationError, LeaseOperationOutcome};

        if let Ok(snapshot) = self.store.checkout(None, Duration::ZERO) {
            return LeaseOperationOutcome::Applied(Some(snapshot));
        }
        if lock_recover(&self.inner).published.is_none() {
            return LeaseOperationOutcome::Applied(None);
        }
        // Each outcome keeps its own name: a released lease is not a superseded one, and the
        // caller's handling differs. Asked of the record itself — a background reader may wait
        // for lease I/O — so an owner that took over is seen before the wait for a free handle.
        if self.lease.is_released() {
            return LeaseOperationOutcome::Released;
        }
        if self.is_superseded() {
            return LeaseOperationOutcome::Superseded;
        }
        #[cfg(test)]
        if self.background_snapshot_failure.load(std::sync::atomic::Ordering::SeqCst) == 1 {
            return LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                BackgroundSnapshotError::Changed,
            ));
        }
        // No handle of its own past the pool: a background reader waits for one like any other,
        // so the handles on the file stay the ones a replacement drains.
        match self.store.checkout(None, BACKGROUND_READ_WAIT) {
            Ok(snapshot) => LeaseOperationOutcome::Applied(Some(snapshot)),
            Err(GraphReadError::OwnerChanged) => LeaseOperationOutcome::Superseded,
            Err(GraphReadError::Changed) => LeaseOperationOutcome::OperationError(
                LeaseOperationError::Operation(BackgroundSnapshotError::Changed),
            ),
            Err(GraphReadError::Busy | GraphReadError::Unavailable) => {
                LeaseOperationOutcome::TransientRefusal
            }
        }
    }

    /// Test-only legacy freshness path. Production request handlers use
    /// `cached_freshness` and never walk disk or start a reload.
    #[cfg(test)]
    pub(crate) fn freshness(&self, snapshot: &GraphSnapshot) -> Freshness {
        let disk = self.current_disk_fp();
        let stale = snapshot.force_stale
            || snapshot.unread_files > 0
            || disk.map(|(fp, _)| fp != snapshot.fingerprint).unwrap_or(false);
        let may_build = self.may_build();

        let mut inner = lock_recover(&self.inner);
        let Some(published) = inner.published.as_mut() else {
            return Freshness {
                revision: snapshot.generation,
                stale,
                reload: "none",
                topology: snapshot.fingerprint.topology,
                drift_watch: self.drift_watch(),
            };
        };
        let mut reload = published.reload.label();
        let claim_reload =
            published.wants_reload(disk) && published.reload != ReloadState::Running && may_build;
        if claim_reload {
            published.reload = ReloadState::Running;
            reload = "running";
        }
        drop(inner);

        if claim_reload {
            let state = self.clone();
            let spawned = std::thread::Builder::new()
                .name("bsl-graph-reload".to_owned())
                .spawn(move || state.run_load(true));
            if let Err(e) = spawned {
                let mut inner = lock_recover(&self.inner);
                if let Some(p) = inner.published.as_mut() {
                    p.reload = ReloadState::Failed(format!("could not spawn reload: {e}"));
                }
                reload = "failed";
            }
        }

        Freshness {
            revision: snapshot.generation,
            stale,
            reload,
            topology: snapshot.fingerprint.topology,
            drift_watch: self.drift_watch(),
        }
    }

    /// Look at what the published build could not read: is the tree whole again, and can the
    /// modules it recorded as unread be opened now?
    ///
    /// The only owner a publication that cannot vouch for itself can have. A restored
    /// permission is not a file change, so nothing on the fact stream will ever announce it;
    /// without this the graph stays `stale` for the life of the daemon. Runs on the watcher's
    /// thread, never on a request — a request may only ask for it sooner.
    pub(super) fn recovery_probe(&self, plan: &super::debt::ProbePlan) -> ProbeOutcome {
        // Ask what we can SEE before paying for the walk. The walk drops the cached
        // fingerprint and the event-maintained map and then stats the whole tree; doing it
        // first meant a probe that could not take a snapshot at all — a held lease, a database
        // that will not open — paid for the walk every time before discovering it had nothing
        // to compare against, and threw the caches away for nothing besides.
        use crate::workspace_lease::LeaseOperationOutcome;
        match self.read_blocking(|snapshot| snapshot.generation) {
            // And it must be a snapshot of the publication these obligations are the gaps OF.
            // Acquired without asking, a walk could measure the tree against one artefact
            // while reporting about another's gaps: the receipt is refused at completion for
            // the same reason, and paying for the whole pass first is the part that is
            // avoidable.
            LeaseOperationOutcome::Applied(Some(generation))
                if generation != plan.basis.generation() =>
            {
                return ProbeOutcome::CouldNotLook;
            }
            LeaseOperationOutcome::Applied(_) => {}
            // A failure to LOOK, not a look that found nothing.
            _ => return ProbeOutcome::CouldNotLook,
        }

        // The BACKGROUND acquire, not the request pool's — and P, not the newest unread list.
        // An obligation the latest enumeration did not mention is still owed an observation:
        // a short scan that stopped naming it answered nothing about it, and dropping it from
        // the walk would leave it with memory and no observer. One level per path, BY path:
        // an aggregate is true for the rest of the episode as soon as any one path opens, and
        // the next path to heal is then invisible. A path that is GONE is told apart from one
        // that will not open — a removal is a measured change of composition, not an open to
        // retry for ever.
        let mut levels: Vec<(super::debt::Capability, super::debt::Level)> = plan
            .open
            .iter()
            .map(|path| {
                let level = match std::fs::File::open(path) {
                    Ok(_) => super::debt::Level::Granted,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        super::debt::Level::Absent
                    }
                    Err(_) => super::debt::Level::Denied,
                };
                (super::debt::Capability::Open(path.clone()), level)
            })
            .collect();

        // The walk is a capability only while one is owed. Scope and verdict come back from
        // the SAME traversal: read as three separate questions of three mutable caches, a
        // verdict could be paired with the roots of a walk that never produced it.
        let walked = plan.scan.as_ref().and_then(|_| {
            *lock_recover(&self.scan) = None;
            {
                let mut fp_state = lock_recover(&self.fp_map);
                fp_state.map = None;
                fp_state.walked_at = None;
            }
            self.walk_scan_receipt()
        });
        if let Some((scope, complete)) = &walked {
            levels.push((
                super::debt::Capability::ScanRoots,
                if *complete { super::debt::Level::Granted } else { super::debt::Level::Denied },
            ));
            let _ = scope;
        }
        ProbeOutcome::Looked { levels, scope: walked.map(|(scope, _)| scope) }
    }

    /// Pair what this publication did with what is outstanding. The short wrapper is retained
    /// for synthetic recovery tests that use physical strings; production callers use
    /// [`Self::recovery_proof_with_roots`] and pass the candidate generation's roots.
    #[cfg(test)]
    pub(super) fn recovery_proof<K: RecoveryDeclaredKey>(
        &self,
        generation: u64,
        declared_unread: Option<&[K]>,
        coverage: RecoveryCoverage<'_>,
    ) -> super::debt::RecoveryPublicationProof {
        self.recovery_proof_with_roots(generation, declared_unread, coverage, None)
    }

    /// Build recovery authority from structured unread keys. A key is resolved with
    /// `resolve_walked`, because the scan and its coverage enumerate canonical paths. An
    /// unknown root, malformed path, or unavailable roots yields no physical declaration and
    /// therefore no recovery authority.
    pub(super) fn recovery_proof_with_roots<K: RecoveryDeclaredKey>(
        &self,
        generation: u64,
        declared_unread: Option<&[K]>,
        coverage: RecoveryCoverage<'_>,
        roots: Option<&bsl_search::WorkspaceRoots>,
    ) -> super::debt::RecoveryPublicationProof {
        let super::debt::OutstandingRecovery { keys: outstanding_keys, captured_seq } =
            lock_recover(&self.debt).outstanding_recovery();
        let declared_unread_paths = declared_unread.map(|unread| {
            unread
                .iter()
                .map(|item| {
                    roots
                        .and_then(|roots| roots.resolve_walked(&item.file_key()))
                        .map(|path| path.to_string_lossy().into_owned())
                        .or_else(|| {
                            roots.is_none().then_some(()).and_then(|_| item.legacy_physical_path())
                        })
                })
                .collect::<Option<Vec<_>>>()
        });
        let resolved_unread = declared_unread_paths.flatten();
        let still_unread: Option<std::collections::HashSet<&str>> =
            resolved_unread.as_ref().map(|unread| unread.iter().map(String::as_str).collect());
        let mut proof = super::debt::RecoveryPublicationProof {
            generation,
            captured_seq,
            declared_unread: resolved_unread.clone(),
            ..Default::default()
        };
        // Without a strict unread list, or when any structured key cannot resolve through the
        // current roots, this publication cannot say what it managed to read.

        let process_walk = |scope: super::debt::RecoveryScope,
                            enumerated: &dyn Fn(&str) -> bool,
                            complete: bool,
                            straddled: bool| {
            let mut read_covered = Vec::new();
            let mut absent_covered = Vec::new();
            let mut out_of_scope_covered = Vec::new();
            for (key, occurrence) in outstanding_keys.iter().cloned() {
                let Some(unread) = still_unread.as_ref() else { continue };
                if unread.contains(key.as_str()) {
                    continue;
                }
                let listed = enumerated(&key);
                if listed {
                    read_covered.push((key, occurrence));
                } else if scope.speaks_for_removal() && !scope.requires(&key) {
                    out_of_scope_covered.push((key, occurrence));
                } else if complete
                    && !straddled
                    && scope.speaks_for_removal()
                    && scope.requires(&key)
                {
                    absent_covered.push((key, occurrence));
                }
            }
            (scope, complete, straddled, read_covered, absent_covered, out_of_scope_covered)
        };

        match coverage {
            #[cfg(test)]
            RecoveryCoverage::Walked { scope, enumerated, complete, straddled } => {
                let (scope, complete, straddled, read, absent, out_of_scope) =
                    process_walk(scope, &|path| enumerated.contains(path), complete, straddled);
                proof.read_covered.extend(read);
                proof.absent_covered.extend(absent);
                proof.out_of_scope_covered.extend(out_of_scope);
                proof.scan_complete = Some(complete);
                proof.straddled = straddled;
                proof.scope = Some(scope);
            }
            RecoveryCoverage::WalkedKeys { scope, enumerated, complete, straddled } => {
                let (scope, complete, straddled, read, absent, out_of_scope) = process_walk(
                    scope,
                    &|path| {
                        roots
                            .and_then(|roots| roots.key_of_path(Path::new(path)))
                            .is_some_and(|key| enumerated.contains(&key))
                    },
                    complete,
                    straddled,
                );
                proof.read_covered.extend(read);
                proof.absent_covered.extend(absent);
                proof.out_of_scope_covered.extend(out_of_scope);
                proof.scan_complete = Some(complete);
                proof.straddled = straddled;
                proof.scope = Some(scope);
            }
            #[cfg(test)]
            RecoveryCoverage::Patched { rewritten } => {
                for (key, occurrence) in outstanding_keys.iter().cloned() {
                    if rewritten.contains(key.as_str())
                        && still_unread
                            .as_ref()
                            .is_some_and(|unread| !unread.contains(key.as_str()))
                    {
                        proof.read_covered.push((key, occurrence));
                    }
                }
            }
            RecoveryCoverage::PatchedKeys { rewritten } => {
                for (key, occurrence) in outstanding_keys.iter().cloned() {
                    let listed = roots
                        .and_then(|roots| roots.key_of_path(Path::new(&key)))
                        .is_some_and(|file_key| rewritten.contains(&file_key));
                    if listed
                        && still_unread
                            .as_ref()
                            .is_some_and(|unread| !unread.contains(key.as_str()))
                    {
                        proof.read_covered.push((key, occurrence));
                    }
                }
            }
            RecoveryCoverage::None => {}
        }
        proof
    }

    /// One traversal, one receipt: the scope actually covered and whether the walk behind it
    /// may speak for the whole tree.
    ///
    /// The two belong together. Asked separately — the verdict from one cache, the roots from
    /// another — a verdict can be paired with a composition that never produced it, and the
    /// pairing is exactly what says an obligation has been answered.
    pub(super) fn walk_scan_receipt(&self) -> Option<(super::debt::RecoveryScope, bool)> {
        let root = self.workspace_root.as_deref()?;
        // Counted where the tree is actually walked. A walk is the expensive half of a
        // recovery pass, and a cost nothing counts is a cost nobody can measure.
        self.scan_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let project = super::input::ProjectSnapshot::load_excluding(root, &self.cache_exclusions());
        let universe = super::universe::ScannedUniverse::scan_project(&project);
        #[cfg(test)]
        if let Some(hook) = self.scan_receipt_hook.clone() {
            // AFTER the walk and before the receipt is made: the window another owner's
            // replacement lands in, and the one a receipt assembled from two reads would
            // straddle without saying so.
            hook(self);
        }
        Some((recovery_scope_of(&project), universe.clean()))
    }

    /// Give back the cursor the comparison keeps for its own scan cache.
    ///
    /// It is subscribed lazily, by the first comparison, and it is the graph's — not the
    /// watcher's. When the observation ends nothing compares again for this generation, and a
    /// cursor nobody drains makes the hub hold entries for a consumer that has left.
    pub(super) fn release_scan_cursor(&self) {
        let Some(hub) = &self.change_hub else { return };
        let cursor = lock_recover(&self.hub_cursor).close();
        if let Some(cursor) = cursor {
            hub.unsubscribe(cursor);
        }
    }

    /// Compatibility wrapper for the test-only freshness path.
    #[cfg(test)]
    pub(super) fn current_disk_fp(&self) -> Option<(crate::graph_db::GraphFp, bool)> {
        self.current_disk_fp_with_watermark().map(|(fingerprint, clean, _)| (fingerprint, clean))
    }

    /// The exact hub cutoff paired with the fingerprint returned by this scan. A caller may
    /// use it to answer only facts present before the walk represented by that fingerprint.
    pub(super) fn current_disk_fp_with_watermark(
        &self,
    ) -> Option<(crate::graph_db::GraphFp, bool, u64)> {
        let root = self.workspace_root.as_deref()?;
        self.invalidate_scan_on_hub_drift();
        let mut cache = lock_recover(&self.scan);
        if let Some(c) = cache.as_ref() {
            if c.at.elapsed() < self.drift_interval {
                return Some((c.disk_fp, c.clean, c.observed_through));
            }
        }
        // Asked about OUR cursor, not about the hub at large: `invalidate_scan_on_hub_drift`
        // above has just drained it, so any debt left here is the hub's own incompleteness
        // — while a shared verdict would also carry the debt of a consumer that simply
        // stopped draining, and put this one on a full walk for as long as that lasted.
        let hub_healthy = matches!(
            &self.change_hub,
            Some(hub) if matches!(
                hub.health_for(lock_recover(&self.hub_cursor).peek()),
                crate::change_hub::Health::Healthy
            )
        );
        // Before any look at disk, cached or fresh.
        let observed_through = self.observation();
        if !hub_healthy {
            let mut fp_state = lock_recover(&self.fp_map);
            fp_state.map = None;
            fp_state.walked_at = None;
        }
        if hub_healthy {
            let fp_state = lock_recover(&self.fp_map);
            if let (Some(map), Some(roots), Some(walked_at)) =
                (fp_state.map.as_ref(), fp_state.roots.as_ref(), fp_state.walked_at)
            {
                if walked_at.elapsed() < WALK_VERIFY_INTERVAL {
                    let fp = fold_portable_fingerprint(map, fp_state.topology);
                    let clean = fp_state.clean && !roots.is_empty();
                    *cache = Some(ScanCache {
                        at: Instant::now(),
                        disk_fp: fp,
                        clean,
                        observed_through,
                    });
                    return Some((fp, clean, observed_through));
                }
            }
        }
        self.scan_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // ONE project load serves both components: the roots the stats walk and
        // the topology hash come from the same snapshot, so the fold can never
        // pair one project state's files with another's topology.
        let project = super::input::ProjectSnapshot::load_excluding(root, &self.cache_exclusions());
        let universe = super::universe::ScannedUniverse::scan_project(&project);
        let clean = universe.clean();
        #[cfg(test)]
        if let Some(hook) = self.scan_window_hook.clone() {
            // The tree has been enumerated and the position this look may vouch for was read
            // before it: a change delivered here belongs to neither.
            hook(self);
        }
        let keys_complete = project.search_roots.as_ref().is_some_and(|roots| {
            universe
                .stats
                .iter()
                .all(|stat| stat.key(roots).is_some() && stat.content_hash().is_some())
        });
        let topology = project.portable_topology;
        let fp = super::scan::fingerprint_of_project(&universe.stats, &project);
        let portable_complete = keys_complete && fp.is_some();
        let clean = clean && portable_complete;
        let fp = fp.unwrap_or(crate::graph_db::GraphFp { files: 0, topology });
        let map = portable_complete.then(|| {
            project.search_roots.as_ref().expect("portable_complete implies roots").clone()
        });
        let map = map.map(|roots| {
            universe
                .stats
                .iter()
                .filter_map(|stat| stat.key(&roots).zip(stat.content_hash()))
                .collect::<std::collections::BTreeMap<_, _>>()
        });
        {
            let mut fp_state = lock_recover(&self.fp_map);
            fp_state.map = map;
            fp_state.roots = project.search_roots.clone();
            fp_state.walked_at = Some(Instant::now());
            fp_state.topology = topology;
            fp_state.clean = clean;
        }
        *cache = Some(ScanCache { at: Instant::now(), disk_fp: fp, clean, observed_through });
        Some((fp, clean, observed_through))
    }

    /// Forget every cached look at disk, so the next one walks the tree again.
    ///
    /// A statement about what is ON disk cannot be read through the throttle: the cached
    /// answer is kept for pacing, and whether it still describes the tree depends on a
    /// watcher having delivered the edit — a race, not an oracle.
    #[cfg(test)]
    pub(super) fn forget_the_disk_look(&self) {
        *lock_recover(&self.scan) = None;
        let mut fp_state = lock_recover(&self.fp_map);
        fp_state.map = None;
        fp_state.walked_at = None;
    }

    /// The hub position the comparison's current fingerprint can vouch for.
    ///
    /// A comparison answers what its own walk saw and no more. Answering by a number read at
    /// the moment of the verdict retires facts delivered after the walk — including one the
    /// walk could not possibly have covered, whose only record is that debt.
    pub(super) fn scan_watermark(&self) -> u64 {
        lock_recover(&self.scan)
            .as_ref()
            .map_or_else(|| self.observation(), |cache| cache.observed_through)
    }

    fn invalidate_scan_on_hub_drift(&self) {
        let Some(hub) = &self.change_hub else {
            return;
        };
        // Taken with the epoch that names this slot's current life. The drain below runs with
        // the lock released; the epoch is what tells a write-back that the cursor it is
        // holding is still the slot's, and not one a release has already ended.
        let Some((cursor, epoch)) = lock_recover(&self.hub_cursor).open(hub) else {
            // Closed: the observation is over and nothing subscribes again.
            return;
        };
        let batch = hub.drain(cursor);
        if !lock_recover(&self.hub_cursor).advance(epoch, batch.cursor) {
            // The slot moved on or closed while this drain ran. Whoever ends a cursor's life
            // unsubscribes it, and that is this drain: storing it would resurrect an id the
            // hub has already forgotten.
            hub.unsubscribe(batch.cursor);
            return;
        }
        if batch.rescan_required {
            *lock_recover(&self.scan) = None;
            let mut fp_state = lock_recover(&self.fp_map);
            fp_state.map = None;
            fp_state.walked_at = None;
            return;
        }
        let relevant: Vec<&ChangeEntry> =
            batch.entries.iter().filter(|e| entry_touches_scan_universe(e)).collect();
        if relevant.is_empty() {
            return;
        }
        *lock_recover(&self.scan) = None;
        let mut fp_state = lock_recover(&self.fp_map);
        // A subtree removal invalidates paths the entry list cannot enumerate; a
        // config-file change may alter the topology AND the scan-root universe.
        // Either way the patched map would lie — drop it so the next check walks
        // (and re-derives the project).
        if relevant.iter().any(|e| e.kind == ChangeKind::SubtreeRemoved || entry_is_config_file(e))
        {
            fp_state.map = None;
            fp_state.walked_at = None;
            return;
        }
        let Some(roots) = fp_state.roots.clone() else {
            fp_state.map = None;
            fp_state.walked_at = None;
            fp_state.clean = false;
            return;
        };
        if fp_state.map.is_none() {
            return;
        }
        let mut updates = Vec::with_capacity(relevant.len());
        for entry in relevant {
            let Some(key) = roots.root_of(&entry.raw, &entry.canonical) else {
                fp_state.map = None;
                fp_state.walked_at = None;
                fp_state.clean = false;
                return;
            };
            match std::fs::read(&entry.canonical) {
                Ok(bytes) => updates.push((key, Some(*blake3::hash(&bytes).as_bytes()))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    updates.push((key, None));
                }
                Err(_) => {
                    fp_state.map = None;
                    fp_state.walked_at = None;
                    fp_state.clean = false;
                    return;
                }
            }
        }
        let Some(map) = fp_state.map.as_mut() else { return };
        for (key, hash) in updates {
            if let Some(hash) = hash {
                map.insert(key, hash);
            } else {
                map.remove(&key);
            }
        }
    }
}

pub(super) fn entry_touches_scan_universe(entry: &ChangeEntry) -> bool {
    if entry.kind == ChangeKind::SubtreeRemoved {
        return true;
    }
    let is_scan_ext = |path: &Path| {
        bsl_conventions::has_extension(path, bsl_conventions::BSL_EXTENSION)
            || bsl_conventions::has_extension(path, bsl_conventions::XML_EXTENSION)
    };
    is_scan_ext(&entry.canonical) || is_scan_ext(&entry.raw) || entry_is_config_file(entry)
}

/// Whether a delivered change is one of the analyzer config files — an edit there
/// can change the extension topology (and with it the scan-root universe) without
/// touching a single `.bsl`/`.xml`.
pub(super) fn entry_is_config_file(entry: &ChangeEntry) -> bool {
    let is_config = |path: &Path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(project_model::is_project_input_file_name)
    };
    is_config(&entry.canonical) || is_config(&entry.raw)
}

fn fold_portable_fingerprint(
    entries: &std::collections::BTreeMap<bsl_search::FileKey, [u8; 32]>,
    topology: u64,
) -> crate::graph_db::GraphFp {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bsl-analyzer-portable-files-v1\0");
    for (key, hash) in entries {
        hasher.update(&(key.root_id.len() as u64).to_le_bytes());
        hasher.update(key.root_id.as_bytes());
        hasher.update(&(key.path.len() as u64).to_le_bytes());
        hasher.update(key.path.as_bytes());
        hasher.update(hash);
    }
    let digest = hasher.finalize();
    crate::graph_db::GraphFp {
        files: u64::from_le_bytes(
            digest.as_bytes()[..8].try_into().expect("blake3 yields >= 8 bytes"),
        ),
        topology,
    }
}

#[cfg(test)]
mod tests {

    /// The three interleavings, run concurrently with real barriers rather than in sequence.
    ///
    /// The drain is what makes them races: it runs with the slot lock RELEASED, so a release
    /// can land inside it. A — a copy written back after that release. B — a comparison
    /// starting after it and subscribing a cursor nobody would unsubscribe. C — two drains
    /// overlapping the release.
    #[test]
    fn a_closing_cursor_races_its_own_drain_and_a_second_comparison() {
        use crate::graph::state::lock_recover;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        for round in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            crate::graph::test_support::sample_workspace(root);
            let hub = crate::graph::test_support::workspace_hub(root);
            assert!(hub.wait_until_watching(std::time::Duration::from_secs(5)));
            let before = hub.active_cursor_count();
            let graph = Arc::new(
                crate::graph::state::GraphState::for_workspace(root.to_path_buf())
                    .with_change_hub(hub.clone()),
            );
            graph.invalidate_scan_on_hub_drift();

            // A: a drain already holding a copy when the release lands.
            let opened = lock_recover(&graph.hub_cursor).open(&hub).expect("an open slot");
            let gate = Arc::new(Barrier::new(3));
            let refused = Arc::new(AtomicUsize::new(0));

            let drainer = {
                let (graph, hub, gate, refused) =
                    (Arc::clone(&graph), hub.clone(), Arc::clone(&gate), Arc::clone(&refused));
                std::thread::spawn(move || {
                    let (cursor, epoch) = opened;
                    gate.wait();
                    let batch = hub.drain(cursor);
                    if !lock_recover(&graph.hub_cursor).advance(epoch, batch.cursor) {
                        // Whoever ends a cursor's life unsubscribes it, and that is this drain.
                        hub.unsubscribe(batch.cursor);
                        refused.fetch_add(1, Ordering::SeqCst);
                    }
                })
            };
            // B and C: a release, and a second comparison starting beside it.
            let releaser = {
                let (graph, gate) = (Arc::clone(&graph), Arc::clone(&gate));
                std::thread::spawn(move || {
                    gate.wait();
                    graph.release_scan_cursor();
                })
            };
            let second = {
                let (graph, gate) = (Arc::clone(&graph), Arc::clone(&gate));
                std::thread::spawn(move || {
                    gate.wait();
                    graph.invalidate_scan_on_hub_drift();
                })
            };
            drainer.join().unwrap();
            releaser.join().unwrap();
            second.join().unwrap();

            let slot = lock_recover(&graph.hub_cursor);
            assert!(slot.is_closed(), "round {round}: close is final");
            assert!(
                slot.holds().is_none(),
                "round {round}: a closed slot holds an id the hub has already forgotten",
            );
            drop(slot);
            assert_eq!(
                hub.active_cursor_count(),
                before,
                "round {round}: a cursor outlived the observation — {} write-backs refused",
                refused.load(Ordering::SeqCst),
            );
            hub.shutdown();
        }
    }

    /// A cursor's life ends where the observation ends, and the three ways that end could be
    /// raced all had the same shape: the drain runs with the slot lock released.
    ///
    /// A — a copied cursor written back after a release resurrected an id the hub had already
    /// forgotten. B — a release that emptied the slot let the next comparison subscribe one
    /// nobody would ever unsubscribe. C — two comparisons overlapping a release left the slot
    /// holding whichever finished last.
    #[test]
    fn closing_a_scan_cursor_prevents_resubscription_and_stale_writeback() {
        use crate::graph::state::lock_recover;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::graph::test_support::sample_workspace(root);
        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(std::time::Duration::from_secs(5)));
        let before = hub.active_cursor_count();

        let graph = crate::graph::state::GraphState::for_workspace(root.to_path_buf())
            .with_change_hub(hub.clone());

        // The comparison subscribes on its own.
        graph.invalidate_scan_on_hub_drift();
        assert_eq!(hub.active_cursor_count(), before + 1, "the comparison subscribed one cursor");

        // A: the copy a drain is holding, taken before the release.
        let (held, epoch) = lock_recover(&graph.hub_cursor).open(&hub).expect("an open slot");
        graph.release_scan_cursor();
        assert_eq!(hub.active_cursor_count(), before, "the release took the cursor with it");
        assert!(
            !lock_recover(&graph.hub_cursor).advance(epoch, held),
            "a closed slot accepted a write-back and resurrected a dead cursor",
        );

        // B and C: nothing subscribes again, however many comparisons come through.
        for _ in 0..3 {
            graph.invalidate_scan_on_hub_drift();
        }
        assert!(lock_recover(&graph.hub_cursor).is_closed(), "the slot stays closed");
        assert_eq!(
            hub.active_cursor_count(),
            before,
            "a comparison after the release subscribed a cursor nobody will unsubscribe",
        );
        hub.shutdown();
    }
    use super::super::scan::workspace_fingerprint;
    use super::super::state::{lock_recover, GraphState};
    use super::super::test_support::{
        sample_workspace, seed_cache, wait_ready, wait_until, wait_until_within, write,
    };
    use super::*;
    use std::time::Duration;

    fn ready_graph(root: &Path) -> GraphState {
        sample_workspace(root);
        seed_cache(root, workspace_fingerprint(root));
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        graph
    }

    #[test]
    fn a_read_returns_its_handle_on_error_and_on_unwind() {
        let dir = tempfile::tempdir().unwrap();
        let graph = ready_graph(dir.path());
        let idle = || graph.store.lock_pool().len();
        let before = idle();
        assert!(before >= 1, "an installed publication keeps its checked handle");

        let failed: Result<anyhow::Result<()>, _> =
            graph.read(|_| Err(anyhow::anyhow!("the operation failed")));
        assert!(matches!(failed, Ok(Err(_))), "the operation's own error is its result");
        assert_eq!(idle(), before, "a failed read gives its handle back");

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            graph.read(|_| -> u64 { panic!("the reader unwinds") })
        }));
        assert!(unwound.is_err());
        assert_eq!(idle(), before, "an unwinding read gives its handle back");
        assert!(graph.read(|snapshot| snapshot.generation()).is_ok());
    }

    #[test]
    fn a_read_planned_against_another_generation_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let graph = ready_graph(dir.path());
        let generation = graph.read(|snapshot| snapshot.generation()).unwrap();

        assert_eq!(
            graph.store.read(Some(generation + 1), Duration::ZERO, |_| ()),
            Err(GraphReadError::Changed)
        );
        assert_eq!(graph.store.read(Some(generation), Duration::ZERO, |_| ()), Ok(()));
        assert_eq!(
            GraphStore::default().read(None, Duration::ZERO, |_| ()),
            Err(GraphReadError::Unavailable),
            "nothing is served before a publication is installed"
        );
    }

    #[test]
    fn a_waiting_read_is_served_by_the_next_returned_handle() {
        let dir = tempfile::tempdir().unwrap();
        let graph = ready_graph(dir.path());
        let mut held: Vec<_> =
            (0..SNAPSHOT_POOL_CAP).map(|_| graph.snapshot().expect("idle handle")).collect();

        assert_eq!(
            graph.read(|_| ()),
            Err(GraphReadError::Unavailable),
            "a request never waits for a handle"
        );
        let returned = held.pop().unwrap();
        let giver = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(returned);
        });
        assert_eq!(
            graph.store.read(None, Duration::from_secs(10), |snapshot| snapshot.generation()),
            Ok(held[0].generation),
            "a background read is served by the handle that came back"
        );
        giver.join().unwrap();
    }

    #[test]
    fn the_context_provider_holds_no_handle_and_refuses_a_newer_generation() {
        use bsl_search::GraphContextProvider as _;

        let dir = tempfile::tempdir().unwrap();
        let graph = ready_graph(dir.path());
        let (generation, fingerprint, force_stale, roots) = graph
            .read(|snapshot| {
                (
                    snapshot.generation,
                    snapshot.fingerprint,
                    snapshot.force_stale,
                    snapshot.workspace_roots().cloned(),
                )
            })
            .unwrap();
        assert!(roots.is_some(), "the fixture publishes its root table");
        let provider = crate::graph_query::GraphDbContextProvider::new(
            graph.store.clone(),
            generation,
            roots.as_ref(),
            Some(graph.owed_context_marks()),
        );
        let module = "CommonModules/Клиент/Ext/Module.bsl";
        let rendered = provider.try_graph_context(module, "Главная", "procedure");
        assert!(matches!(rendered, Ok(Some(_))), "{rendered:?}");
        assert_eq!(graph.store.lock_pool().lent, 0, "the provider owns no handle");

        let mut prepared =
            graph.prepare_snapshot_pool(generation, fingerprint, force_stale).unwrap();
        let mut entry = prepared.entry.take().unwrap();
        entry.generation = generation + 1;
        graph.store.install(prepared.path.clone(), prepared.path_identity.clone(), entry, roots);
        assert!(
            provider.try_graph_context(module, "Главная", "procedure").is_err(),
            "a render against a newer publication fails, so its mark is kept"
        );
        assert_eq!(
            provider.graph_context(module, "Главная", "procedure"),
            None,
            "the infallible form is the same refusal, never another generation's answer"
        );
    }

    /// A render the provider failed is owed, and the owing reaches the graph without waiting on
    /// anything: the engine that reports it is held while it does.
    #[test]
    fn marks_owed_by_the_provider_wait_for_the_watcher_to_register_them() {
        use bsl_search::GraphContextProvider as _;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        let graph = ready_graph(dir.path());
        let provider = crate::graph_query::GraphDbContextProvider::new(
            graph.store.clone(),
            0,
            None,
            Some(graph.owed_context_marks()),
        );
        let alarms = graph.alarms.load(Ordering::SeqCst);
        provider.context_marks_owed(5);
        provider.context_marks_owed(3);
        assert_eq!(graph.owed_context_marks.load(Ordering::SeqCst), 5, "the highest mark is kept");
        assert!(graph.alarms.load(Ordering::SeqCst) > alarms, "the watcher is woken");

        graph.register_owed_context_marks();
        assert_eq!(graph.owed_context_marks.load(Ordering::SeqCst), 0, "registered once");
        assert!(!graph.marks_pending(), "an observing publication consumes the marks at once");
    }

    #[test]
    fn prepare_identity_errors_preserve_operation_provenance() {
        assert!(matches!(
            classify_prepare_identity_error(std::io::Error::from(std::io::ErrorKind::NotFound)),
            SnapshotPrepareError::Changed
        ));
        assert!(matches!(
            classify_prepare_identity_error(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            )),
            SnapshotPrepareError::Open(_)
        ));
    }

    #[test]
    fn path_identity_detects_equal_size_and_time_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("graph.db");
        let replacement = dir.path().join("replacement.db");
        let old = dir.path().join("old.db");
        std::fs::write(&current, b"old").unwrap();
        let modified = std::fs::metadata(&current).unwrap().modified().unwrap();
        let before = GraphPathIdentity::read(&current).unwrap();

        std::fs::write(&replacement, b"new").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::rename(&current, old).unwrap();
        std::fs::rename(replacement, &current).unwrap();
        let after = GraphPathIdentity::read(&current).unwrap();

        assert_eq!(before.len, after.len);
        assert_eq!(before.modified, after.modified);
        assert_ne!(before, after, "stable file identity detects the replacement");
    }

    #[test]
    fn a_case_variant_module_still_touches_the_scan_universe() {
        let path = std::path::PathBuf::from("/w/CommonModules/X/Ext/Module.BSL");
        let entry = ChangeEntry {
            canonical: path.clone(),
            raw: path,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };
        assert!(
            entry_touches_scan_universe(&entry),
            "Module.BSL входит во вселенную скана — хаб обязан сбросить кэш отпечатка"
        );
    }

    /// Every file the project is derived from shapes the extension topology, so a
    /// change to any of them must touch the scan universe. Enumerated from the
    /// shared list rather than spelled out here: a point that stops recognising one
    /// of them reddens this test instead of going unnoticed.
    #[test]
    fn every_project_input_touches_the_scan_universe() {
        for name in project_model::PROJECT_INPUT_FILE_NAMES {
            let path = std::path::PathBuf::from("/w").join(name);
            let entry = ChangeEntry {
                canonical: path.clone(),
                raw: path,
                kind: ChangeKind::MaybeChanged,
                seq: 1,
            };
            assert!(
                entry_touches_scan_universe(&entry),
                "a change to {name} must touch the scan universe"
            );
        }
    }

    /// A request miss never reopens a replaced shared file, even when its token looks compatible.
    #[test]
    fn a_replaced_graph_file_is_not_opened_on_request_miss() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, workspace_fingerprint(root));

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        graph.store.lock_pool().clear();
        seed_cache(root, workspace_fingerprint(root));
        assert!(
            graph.snapshot().is_none(),
            "request miss is pool-only and never opens the replacement",
        );
    }

    #[test]
    fn final_install_rechecks_token_through_the_preopened_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, workspace_fingerprint(root));
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let (generation, fingerprint, force_stale) = {
            let inner = lock_recover(&graph.inner);
            let published = inner.published.as_ref().unwrap();
            (published.generation, published.fingerprint, published.force_stale)
        };
        let prepared = graph.prepare_snapshot_pool(generation, fingerprint, force_stale).unwrap();
        let path = graph.graph_db_path().unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        set_snapshot_install_hook(Box::new(move || {
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute(
                    "UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'revision'",
                    [],
                )
                .unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }));
        let outcome = graph.install_prepared_snapshot(
            prepared,
            Published {
                generation,
                fingerprint,
                stale: false,
                reload: ReloadState::Idle,
                force_stale,
                search_roots: None,
                observed_through: Some(0),
            },
            GraphStatus::Ready { files: 0 },
            None,
            None,
            super::super::debt::RecoveryPublicationProof::without_coverage(generation),
        );
        assert!(matches!(
            outcome,
            crate::workspace_lease::LeaseOperationOutcome::OperationError(
                crate::workspace_lease::LeaseOperationError::Operation(
                    SnapshotInstallError::Changed
                )
            )
        ));
    }

    #[test]
    fn a_request_miss_never_consults_the_lease() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let old = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(old.clone());
        graph.ensure_loading();
        wait_ready(&graph);

        let held: Vec<_> = (0..SNAPSHOT_POOL_CAP)
            .map(|_| graph.snapshot().expect("the prepared descriptor is available"))
            .collect();
        let newer = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(graph.snapshot().is_none(), "an empty pool never consults the foreign owner");
        newer.release();
        assert!(graph.snapshot().is_none(), "owner release cannot refill the empty pool");

        drop(held);
        assert!(graph.snapshot().is_some(), "a returned handle serves again");

        let transient_dir = tempfile::tempdir().unwrap();
        let transient_root = transient_dir.path();
        sample_workspace(transient_root);
        let transient_cache = crate::cache::WorkspaceCacheLayout::for_workspace(transient_root);
        transient_cache.ensure().unwrap();
        let transient_lease = crate::workspace_lease::WorkspaceLease::claim_cache(&transient_cache);
        let transient = GraphState::for_workspace_with_cache(
            transient_root.to_path_buf(),
            transient_cache.clone(),
        )
        .with_lease(transient_lease.clone());
        transient.ensure_loading();
        wait_ready(&transient);
        let occupied: Vec<_> = (0..SNAPSHOT_POOL_CAP)
            .map(|_| transient.snapshot().expect("prepared descriptor"))
            .collect();
        let held_lock = transient_lease.hold_file_lock_for_test();
        let started = Instant::now();
        assert!(transient.snapshot().is_none(), "the fifth request gets an immediate miss");
        assert!(started.elapsed() < Duration::from_millis(100));
        drop(held_lock);
        drop(occupied);
    }

    #[test]
    fn snapshot_pool_reuses_and_discards_superseded_handles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, workspace_fingerprint(root));

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        let pool_len = || graph.store.lock_pool().len();
        assert_eq!(pool_len(), 1, "publication opens and checks one handle, no more");
        let s1 = graph.snapshot().expect("snapshots");
        assert_eq!(pool_len(), 0);
        drop(s1);
        assert_eq!(pool_len(), 1, "the dropped handle returns to the pool");

        let held: Vec<_> =
            (0..SNAPSHOT_POOL_CAP).map(|_| graph.snapshot().expect("opened on demand")).collect();
        assert!(graph.snapshot().is_none(), "never more handles than the cap");
        drop(held);
        assert_eq!(pool_len(), SNAPSHOT_POOL_CAP, "every returned handle is kept for reuse");

        {
            let mut pool = graph.store.lock_pool();
            let entry = pool.pop().expect("one parked entry");
            pool.push(PooledSnapshotEntry { generation: entry.generation + 100, ..entry });
        }
        let s3 = graph.snapshot().expect("snapshots");
        assert_eq!(s3.generation, 7, "a superseded handle never serves a new request");
        assert_eq!(pool_len(), SNAPSHOT_POOL_CAP - 2, "the stale entry was discarded");
        drop(s3);

        let old = graph.snapshot().expect("old generation checkout");
        {
            let mut pool = graph.store.lock_pool();
            pool.clear();
            pool.generation += 1;
        }
        drop(old);
        assert_eq!(pool_len(), 0, "a returned old-generation handle cannot poison a new pool");
    }

    #[test]
    fn checkout_cannot_discard_a_concurrently_published_pool() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, workspace_fingerprint(root));

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let (generation, fingerprint, force_stale) = {
            let inner = lock_recover(&graph.inner);
            let published = inner.published.as_ref().unwrap();
            (published.generation, published.fingerprint, published.force_stale)
        };
        let mut prepared =
            graph.prepare_snapshot_pool(generation, fingerprint, force_stale).unwrap();
        let mut entry = prepared.entry.take().unwrap();
        entry.generation = generation + 1;
        let (path, identity) = (prepared.path.clone(), prepared.path_identity.clone());

        let old = graph.snapshot().expect("the published pool lends a handle");
        let publishing = graph.store.clone();
        set_snapshot_checkout_hook(Box::new(move || {
            publishing.install(path, identity, entry, None);
        }));
        let raced = graph.snapshot().expect("a checkout racing the publication is served");
        assert_eq!(raced.generation, generation + 1, "the race is served by the new pool");
        drop(raced);
        drop(old);

        let pool = graph.store.lock_pool();
        assert_eq!(pool.generation, generation + 1);
        assert_eq!(pool.len(), 1, "the new handle survives the old checkout's return");
        assert!(pool.iter().all(|entry| entry.generation == generation + 1));
    }

    #[test]
    fn background_snapshot_preserves_all_typed_outcomes() {
        use crate::workspace_lease::{LeaseOperationError, LeaseOperationOutcome};

        let ready_graph = || {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
            cache.ensure().unwrap();
            let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
            let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache)
                .with_lease(lease.clone());
            graph.ensure_loading();
            wait_ready(&graph);
            (dir, graph, lease)
        };
        let occupy = |graph: &GraphState| -> Vec<_> {
            (0..SNAPSHOT_POOL_CAP)
                .map(|_| graph.snapshot().expect("published descriptor"))
                .collect()
        };

        let (_dir, graph, lease) = ready_graph();
        let held_lock = lease.hold_file_lock_for_test();
        let started = Instant::now();
        assert!(matches!(graph.snapshot_blocking(), LeaseOperationOutcome::Applied(Some(_))));
        assert!(started.elapsed() < Duration::from_millis(100), "pool checkout skips preflight");
        drop(held_lock);

        let _occupied = occupy(&graph);
        let held_lock = lease.hold_file_lock_for_test();
        assert!(matches!(graph.snapshot_blocking(), LeaseOperationOutcome::TransientRefusal));
        drop(held_lock);

        let (_dir, graph, _lease) = ready_graph();
        let _occupied = occupy(&graph);
        graph.set_background_snapshot_failure_for_test(Some(BackgroundSnapshotFailure::Changed));
        assert!(matches!(
            graph.snapshot_blocking(),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(_))
        ));
        graph.set_background_snapshot_failure_for_test(None);

        let missing = GraphState::for_workspace(tempfile::tempdir().unwrap().path().to_path_buf());
        assert!(matches!(missing.snapshot_blocking(), LeaseOperationOutcome::Applied(None)));

        let (_dir, graph, lease) = ready_graph();
        let _occupied = occupy(&graph);
        lease.release();
        assert!(matches!(graph.snapshot_blocking(), LeaseOperationOutcome::Released));

        let (_dir, graph, old) = ready_graph();
        let _occupied = occupy(&graph);
        let cache = graph.cache().unwrap().clone();
        let newer = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(matches!(graph.snapshot_blocking(), LeaseOperationOutcome::Superseded));
        old.release();
        newer.release();
    }

    #[test]
    fn publication_without_a_complete_descriptor_pool_cannot_be_ready() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.set_background_snapshot_failure_for_test(Some(
            BackgroundSnapshotFailure::PrepareSecondChanged,
        ));
        graph.ensure_loading();
        wait_until_within(&graph, Duration::from_secs(5), "the publication to fail", || {
            matches!(graph.status(), GraphStatus::Failed { .. })
        });
        assert!(graph.snapshot().is_none());
        graph.set_background_snapshot_failure_for_test(None);
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(graph.snapshot().is_some(), "the failed publication remains retryable");
    }

    #[test]
    fn drift_marks_stale_and_async_reload_bumps_generation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut graph = GraphState::for_workspace(root.to_path_buf());
        graph.drift_interval = Duration::ZERO;
        graph.ensure_loading();
        wait_ready(&graph);

        let snap1 = graph.snapshot().expect("ready graph snapshots");
        let old_token = snap1.graph.freshness_token().unwrap();
        let fresh = graph.freshness(&snap1);
        assert_eq!(fresh.revision, 1);
        assert!(!fresh.stale);
        assert_eq!(fresh.reload, "none");

        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт Возврат 42; КонецФункции",
        );
        let drifted = graph.freshness(&snap1);
        assert!(drifted.stale, "an on-disk edit must read as stale");
        assert_eq!(drifted.revision, 1, "the stale response still serves the old generation");
        assert!(matches!(drifted.reload, "running" | "failed"));
        // The read in flight stays whole for as long as it runs; the reload's installation
        // waits for it to return rather than replacing the file under it.
        assert_eq!(snap1.graph.freshness_token().unwrap(), old_token);
        drop(snap1);

        wait_until_within(
            &graph,
            Duration::from_secs(2),
            "the reload to publish generation 2",
            || graph.snapshot().is_some_and(|snap| snap.generation == 2),
        );
        let settled = graph.freshness(&graph.snapshot().expect("the reload published"));
        assert!(!settled.stale);
        assert_eq!(settled.revision, 2);
        assert_eq!(settled.reload, "none");
    }

    #[test]
    fn graph_freshness_invalidates_on_hub_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        sample_workspace(root);

        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let mut graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        graph.drift_interval = Duration::from_secs(120);
        graph.ensure_loading();
        wait_ready(&graph);

        let snap = graph.snapshot().expect("ready");
        assert!(!graph.freshness(&snap).stale, "a freshly built graph is not stale");

        let mut observer = hub.subscribe();
        std::thread::sleep(Duration::from_millis(10));
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции",
        );
        // Waits on the hub's delivery queue, not on graph state: a graph-state summary
        // would say nothing about whether inotify delivered.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut delivered = false;
        while Instant::now() < deadline {
            let batch = hub.drain(observer);
            observer = batch.cursor;
            if batch.entries.iter().any(|e| e.raw.to_string_lossy().contains("Module.bsl")) {
                delivered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(delivered, "the hub delivered the edit");
        assert!(
            graph.freshness(&snap).stale,
            "a hub-delivered edit is seen without waiting out the drift throttle",
        );
    }

    /// The full live-daemon chain for a topology-only change: a served graph must
    /// read stale after a `dependsOn`-only config edit (no `.bsl`/`.xml` touched),
    /// and the kicked reload must publish a fresh generation that reads clean.
    #[test]
    fn a_depends_on_only_edit_marks_a_served_graph_stale_and_reloads() {
        use super::super::test_support::{write_extension_config, write_extension_workspace};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_extension_workspace(root, false);

        let mut graph = GraphState::for_workspace(root.to_path_buf());
        graph.drift_interval = Duration::ZERO;
        graph.ensure_loading();
        wait_ready(&graph);

        let snap = graph.snapshot().expect("ready graph snapshots");
        assert!(!graph.freshness(&snap).stale, "a freshly built graph is not stale");

        write_extension_config(root, true);
        let drifted = graph.freshness(&snap);
        assert!(drifted.stale, "a dependsOn-only edit must read as stale");
        assert!(matches!(drifted.reload, "running" | "failed"));
        // Returned, so the reload's installation need not wait for it.
        drop(snap);

        wait_until_within(
            &graph,
            Duration::from_secs(5),
            "the topology-triggered reload to publish generation 2",
            || graph.snapshot().is_some_and(|snap| snap.generation == 2),
        );
        let settled = graph.freshness(&graph.snapshot().expect("the reload published"));
        assert!(!settled.stale, "the reloaded graph reflects the new topology");
    }

    /// End-to-end root re-arm: an extension root added by a topology reload lies
    /// OUTSIDE the hub's original coverage, and after the reload publishes, events
    /// under that root must be hub-delivered — proof the rebuild re-pointed the
    /// live watcher instead of leaving the new subtree to the reconciler.
    #[test]
    fn a_topology_reload_rearms_the_hub_onto_the_new_extension_root() {
        use super::super::test_support::write;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ext_dir = tempfile::tempdir().unwrap();
        let ext = ext_dir.path();
        super::super::test_support::sample_workspace(root);
        write(root, "Configuration.xml", "<Configuration/>");
        write(ext, "Configuration.xml", "<Configuration/>");

        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let mut graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        graph.drift_interval = Duration::ZERO;
        graph.ensure_loading();
        super::super::test_support::wait_ready(&graph);
        let snap = graph.snapshot().expect("ready");
        assert!(!graph.freshness(&snap).stale);

        // Declare the out-of-tree extension: a topology-only reload trigger.
        std::fs::write(
            root.join("bsl-analyzer.toml"),
            format!(
                "[source]\nroot = \".\"\nextensions = [{{ name = \"a\", path = {:?} }}]\n",
                ext.to_string_lossy()
            ),
        )
        .unwrap();
        // Staleness lands once the hub delivers the config event (the throttled
        // fast path deliberately serves the cached topology until then).
        wait_until_within(
            &graph,
            Duration::from_secs(5),
            "the new extension root to read as drift",
            || graph.freshness(&snap).stale,
        );
        drop(snap);
        wait_until(&graph, "the drift reload to publish generation 2", || {
            graph.snapshot().map(|s| s.generation) == Some(2)
        });

        // The re-armed hub must deliver events under the NEW root. The write is
        // repeated per poll so a delivery is observed even if the ack landed a
        // moment after the generation became visible.
        let cursor = hub.subscribe();
        let file = ext.join("Новый.bsl");
        // The watcher reports the spelling the platform hands it, which need not be the
        // one written through (macOS resolves the temp dir's link before delivering). The
        // delivery is therefore recognised by `canonical`, the key the hub guarantees
        // against the scan universe.
        let delivered = ext.canonicalize().expect("the extension root exists").join("Новый.bsl");
        // Waits on the hub's delivery queue, not on graph state: a graph-state summary
        // would say nothing about whether inotify delivered.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut cursor = cursor;
        let mut seen = false;
        while Instant::now() < deadline {
            std::fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();
            std::thread::sleep(Duration::from_millis(50));
            let batch = hub.drain(cursor);
            cursor = batch.cursor;
            if batch.entries.iter().any(|e| e.canonical == delivered) {
                seen = true;
                break;
            }
        }
        assert!(seen, "the hub must deliver events under the newly-added extension root");
    }

    /// Another consumer that stopped draining owes its own reconcile. Answering that debt
    /// here used to drop the graph's fingerprint map and buy a full tree walk on every
    /// freshness check — for as long as the other consumer stayed silent, which is
    /// forever if its thread is gone.
    #[test]
    fn a_foreign_cursors_debt_does_not_cost_the_graph_a_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let mut graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        // Without this the throttled scan cache answers before the health question is ever
        // asked, and this test would pass no matter what the answer would have been.
        graph.drift_interval = Duration::ZERO;
        graph.ensure_loading();
        wait_ready(&graph);
        // The graph's own cursor exists and is clean from here on: `current_disk_fp`
        // drains it before it asks anything. Two calls settle the map and its own debt.
        let _ = graph.current_disk_fp();
        let _ = graph.current_disk_fp();

        // A stranger subscribes and never drains; then everyone is asked to reconcile.
        // The graph answers for ITSELF with one walk — that debt is genuinely its own —
        // and the stranger's stays outstanding for ever after.
        let _stranger = hub.subscribe();
        hub.degrade_external();
        let _ = graph.current_disk_fp();

        let walks = graph.scan_count.load(std::sync::atomic::Ordering::SeqCst);
        let _ = graph.current_disk_fp();
        assert_eq!(
            graph.scan_count.load(std::sync::atomic::Ordering::SeqCst),
            walks,
            "somebody else's outstanding reconcile is not the graph's to pay for"
        );
    }

    /// The other half, and the one that keeps the first honest: when the HUB cannot
    /// deliver, the graph must keep walking however clean its own cursor is. Without this
    /// leg, replacing the health question with an unconditional fast path passes every
    /// other gate here while going quietly blind.
    ///
    /// The carrier is a hub whose thread never started, not a blind root: the graph
    /// re-declares the hub's targets as it builds, which would take an unwatched root out
    /// of the declaration and leave the hub honestly healthy — a stand that proves nothing.
    #[test]
    fn a_hub_that_cannot_deliver_still_sends_the_graph_back_to_a_walk() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hub = WorkspaceChangeHub::start_with_unstartable_thread(vec![WatchTarget::recursive(
            root.to_path_buf(),
        )]);

        let mut graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub);
        graph.drift_interval = Duration::ZERO;
        graph.ensure_loading();
        wait_ready(&graph);
        let _ = graph.current_disk_fp();
        let walks = graph.scan_count.load(std::sync::atomic::Ordering::SeqCst);

        let _ = graph.current_disk_fp();
        assert!(
            graph.scan_count.load(std::sync::atomic::Ordering::SeqCst) > walks,
            "a hub that will never deliver leaves the graph nothing to trust"
        );
    }

    /// A config-file change delivered by the hub must invalidate the throttled
    /// fingerprint cache AND the event-maintained stat map immediately — the map
    /// can only patch file stats, not the topology, so serving its fold after a
    /// config edit would keep a stale topology fresh for up to the walk interval.
    #[test]
    fn graph_freshness_sees_a_config_edit_through_the_hub() {
        use super::super::test_support::{write_extension_config, write_extension_workspace};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_extension_workspace(root, false);

        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let mut graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        graph.drift_interval = Duration::from_secs(120);
        graph.ensure_loading();
        wait_ready(&graph);

        let snap = graph.snapshot().expect("ready");
        assert!(!graph.freshness(&snap).stale, "a freshly built graph is not stale");

        let mut observer = hub.subscribe();
        std::thread::sleep(Duration::from_millis(10));
        // Re-written per poll: under a fully parallel test run the inotify queue
        // can lag well past a single write's event window.
        // Waits on the hub's delivery queue, not on graph state: a graph-state summary
        // would say nothing about whether inotify delivered.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut delivered = false;
        while Instant::now() < deadline {
            write_extension_config(root, true);
            std::thread::sleep(Duration::from_millis(20));
            let batch = hub.drain(observer);
            observer = batch.cursor;
            if batch.entries.iter().any(|e| e.raw.to_string_lossy().contains("bsl-analyzer.toml")) {
                delivered = true;
                break;
            }
        }
        assert!(
            delivered,
            "the hub delivered the config edit (events_seen={}, health={:?})",
            hub.events_seen(),
            hub.health(),
        );
        assert!(
            graph.freshness(&snap).stale,
            "a hub-delivered config edit is seen without waiting out the drift throttle",
        );
    }

    #[test]
    fn graph_freshness_ignores_non_scan_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        sample_workspace(root);

        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let mut graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        graph.drift_interval = Duration::from_secs(120);
        graph.ensure_loading();
        wait_ready(&graph);

        let snap = graph.snapshot().expect("ready");
        assert!(!graph.freshness(&snap).stale, "a freshly built graph is not stale");
        let scans_after_prime = graph.scan_count();

        let mut observer = hub.subscribe();
        std::thread::sleep(Duration::from_millis(10));
        write(root, "CommonModules/Сервер/Ext/Module.bsl.tmp", "editor swap file");
        // Waits on the hub's delivery queue, not on graph state: a graph-state summary
        // would say nothing about whether inotify delivered.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut delivered = false;
        while Instant::now() < deadline {
            let batch = hub.drain(observer);
            observer = batch.cursor;
            if batch.entries.iter().any(|e| e.raw.to_string_lossy().contains(".tmp")) {
                delivered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(delivered, "the hub delivered the .tmp file");
        assert!(!graph.freshness(&snap).stale, "a temp file does not make the graph stale");
        assert_eq!(
            graph.scan_count(),
            scans_after_prime,
            "an irrelevant temp file must not invalidate the cache and re-trigger a scan",
        );
    }

    /// The three positive proofs, and everything that is NOT one of them.
    ///
    /// This is the producer, where the policy lives: what the debt applies is whatever arrives
    /// in these lists, so the lists are where "a complete walk of a validated scope" has to be
    /// told apart from a short one, a restricted fallback, and a patch that never looked.
    #[test]
    fn only_three_positives_retire_an_obligation() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        // Real places on disk, because a descriptor whose roots do not resolve may not say
        // that an address has gone away — and a test that asserted removal against a spelling
        // nothing answers to would prove the opposite of what it claims.
        let root = dir.path().join("ws");
        let excluded = root.join("Исключено");
        let other_root = dir.path().join("другое");
        std::fs::create_dir_all(&excluded).unwrap();
        std::fs::create_dir_all(&other_root).unwrap();
        let validated = super::super::debt::RecoveryScope::of(
            std::slice::from_ref(&root),
            std::slice::from_ref(&excluded),
            true,
        );
        let fallback =
            super::super::debt::RecoveryScope::of(std::slice::from_ref(&root), &[], false);
        let module = root.join("Модуль.bsl");
        let another = root.join("Другой.bsl");
        std::fs::write(&module, "").unwrap();
        std::fs::write(&another, "").unwrap();
        let module = module.canonicalize().unwrap().to_string_lossy().into_owned();
        let another = another.canonicalize().unwrap().to_string_lossy().into_owned();
        let required: &str = &module;
        let other: &str = &another;

        // Declare the obligation the way an unsound publication does.
        lock_recover(&graph.debt).record_publication(
            std::time::Instant::now(),
            Some(1),
            false,
            None,
            super::super::debt::RecoveryPublicationProof {
                generation: 1,
                declared_unread: Some(vec![required.to_owned()]),
                ..Default::default()
            },
        );

        let enumerated_with: std::collections::HashSet<&str> =
            [required, other].into_iter().collect();
        let enumerated_without: std::collections::HashSet<&str> = [other].into_iter().collect();
        let unread_none: Vec<String> = Vec::new();
        let unread_it = vec![required.to_owned()];

        let read = graph.recovery_proof(
            2,
            Some(&unread_none),
            RecoveryCoverage::Walked {
                scope: validated.clone(),
                enumerated: &enumerated_with,
                complete: true,
                straddled: false,
            },
        );
        assert_eq!(read.read_covered.len(), 1, "a build that enumerated and read it proves so");
        assert!(read.absent_covered.is_empty() && read.out_of_scope_covered.is_empty());

        let still_unread = graph.recovery_proof(
            2,
            Some(&unread_it),
            RecoveryCoverage::Walked {
                scope: validated.clone(),
                enumerated: &enumerated_with,
                complete: true,
                straddled: false,
            },
        );
        assert!(
            still_unread.read_covered.is_empty(),
            "a build that listed it and could not read it proved it read it",
        );

        let absent = graph.recovery_proof(
            2,
            Some(&unread_none),
            RecoveryCoverage::Walked {
                scope: validated.clone(),
                enumerated: &enumerated_without,
                complete: true,
                straddled: false,
            },
        );
        assert_eq!(absent.absent_covered.len(), 1, "a complete walk that did not list it");

        let short = graph.recovery_proof(
            2,
            Some(&unread_none),
            RecoveryCoverage::Walked {
                scope: validated.clone(),
                enumerated: &enumerated_without,
                complete: false,
                straddled: false,
            },
        );
        assert!(short.absent_covered.is_empty(), "a SHORT walk proved an absence");

        let restricted = graph.recovery_proof(
            2,
            Some(&unread_none),
            RecoveryCoverage::Walked {
                scope: fallback,
                enumerated: &enumerated_without,
                complete: true,
                straddled: false,
            },
        );
        assert!(
            restricted.absent_covered.is_empty() && restricted.out_of_scope_covered.is_empty(),
            "a restricted fallback declared a path gone",
        );

        // A narrower validated declaration no longer asks for it — that IS a proof.
        let narrowed =
            super::super::debt::RecoveryScope::of(std::slice::from_ref(&other_root), &[], true);
        let out_of_scope = graph.recovery_proof(
            2,
            Some(&unread_none),
            RecoveryCoverage::Walked {
                scope: narrowed,
                enumerated: &enumerated_without,
                complete: true,
                straddled: false,
            },
        );
        assert_eq!(out_of_scope.out_of_scope_covered.len(), 1, "the roots no longer cover it");

        // A point patch proves only what it rewrote, and never an absence.
        let rewritten: std::collections::HashSet<&str> = [required].into_iter().collect();
        let patched = graph.recovery_proof(
            2,
            Some(&unread_none),
            RecoveryCoverage::Patched { rewritten: &rewritten },
        );
        assert_eq!(patched.read_covered.len(), 1, "the patch re-projected it");
        assert!(patched.absent_covered.is_empty(), "a patch proved an absence");

        let untouched: std::collections::HashSet<&str> = [other].into_iter().collect();
        let elsewhere = graph.recovery_proof(
            2,
            Some(&unread_none),
            RecoveryCoverage::Patched { rewritten: &untouched },
        );
        assert!(elsewhere.read_covered.is_empty(), "a patch that never touched it covered it");
        assert!(
            elsewhere.absent_covered.is_empty() && elsewhere.out_of_scope_covered.is_empty(),
            "a patch proved something about an address it never looked at",
        );

        // A walk that straddled a write enumerated a world that has already been replaced.
        let straddled = graph.recovery_proof(
            2,
            Some(&unread_none),
            RecoveryCoverage::Walked {
                scope: validated.clone(),
                enumerated: &enumerated_without,
                complete: true,
                straddled: true,
            },
        );
        assert!(
            straddled.absent_covered.is_empty(),
            "a build whose tree moved under it proved an absence",
        );
        assert_eq!(straddled.scan_complete, Some(true), "the walk itself was still whole");
        assert!(straddled.straddled, "and the publication still cannot vouch for it");

        // A root that could not be resolved keeps its declared spelling, under which the
        // canonical addresses beneath it match nothing. That is not a removal.
        let gone = dir.path().join("ушёл");
        let unresolved =
            super::super::debt::RecoveryScope::of(std::slice::from_ref(&gone), &[], true);
        let denied_root = graph.recovery_proof(
            2,
            Some(&unread_none),
            RecoveryCoverage::Walked {
                scope: unresolved,
                enumerated: &enumerated_without,
                complete: true,
                straddled: false,
            },
        );
        assert!(
            denied_root.out_of_scope_covered.is_empty() && denied_root.absent_covered.is_empty(),
            "a root that would not resolve declared its subtree gone",
        );

        // An address the walk LISTED is required, whatever spelling the roots canonicalise
        // to: a descendant reached through a symlink is enumerated under the walked name and
        // canonicalises elsewhere.
        let alias: std::collections::HashSet<&str> = [required, other].into_iter().collect();
        let aliased = graph.recovery_proof(
            2,
            Some(&unread_it),
            RecoveryCoverage::Walked {
                scope: super::super::debt::RecoveryScope::of(
                    std::slice::from_ref(&other_root),
                    &[],
                    true,
                ),
                enumerated: &alias,
                complete: true,
                straddled: false,
            },
        );
        assert!(
            aliased.out_of_scope_covered.is_empty(),
            "an address the walk listed and the artefact could not read was retired anyway",
        );

        // No strict unread list at all: the database would not say what it managed to read,
        // so nothing it did proves anything about an obligation.
        let blind_metadata = graph.recovery_proof(
            2,
            None::<&[String]>,
            RecoveryCoverage::Walked {
                scope: validated.clone(),
                enumerated: &enumerated_with,
                complete: true,
                straddled: false,
            },
        );
        assert!(
            blind_metadata.read_covered.is_empty()
                && blind_metadata.absent_covered.is_empty()
                && blind_metadata.out_of_scope_covered.is_empty(),
            "a publication that could not read its own metadata retired an obligation",
        );

        // And a cache, which did neither.
        let cached = graph.recovery_proof(2, Some(&unread_none), RecoveryCoverage::None);
        assert!(
            cached.read_covered.is_empty()
                && cached.absent_covered.is_empty()
                && cached.out_of_scope_covered.is_empty(),
            "a cache served as it stands proved something",
        );
    }

    /// Unreadable metadata is a failure to LOOK, and it answers nothing.
    ///
    /// `read_unread_paths` returns an empty list for a database that will not read — the
    /// lenient form the serving API has always used. Taken as authority it says "this build
    /// read everything", which would retire every outstanding obligation on the strength of an
    /// error.
    #[test]
    fn strict_unread_errors_never_become_an_empty_answer() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        lock_recover(&graph.debt).record_publication(
            std::time::Instant::now(),
            Some(1),
            false,
            None,
            super::super::debt::RecoveryPublicationProof {
                generation: 1,
                declared_unread: Some(vec!["/ws/Модуль.bsl".to_owned()]),
                ..Default::default()
            },
        );
        let enumerated: std::collections::HashSet<&str> = ["/ws/Модуль.bsl"].into_iter().collect();
        let blind = graph.recovery_proof(
            2,
            None::<&[String]>,
            RecoveryCoverage::Walked {
                scope: super::super::debt::RecoveryScope::of(
                    &[std::path::PathBuf::from("/ws")],
                    &[],
                    true,
                ),
                enumerated: &enumerated,
                complete: true,
                straddled: false,
            },
        );
        assert!(
            blind.read_covered.is_empty(),
            "a publication whose metadata would not read claimed it had read the file",
        );
        assert!(
            blind.declared_unread.is_none(),
            "and it claimed authority over what is still unread",
        );

        // The strict reader itself: a missing key is an incomplete schema-21 artefact,
        // an explicit empty array is valid, and a broken payload is an error.
        let path = dir.path().join("meta.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", []).unwrap();
        assert!(
            crate::graph_db::read_unread_keys_strict(&conn).is_err(),
            "schema 21 requires unread_paths metadata even when the set is empty",
        );
        conn.execute("INSERT INTO meta (key, value) VALUES ('unread_paths', '[]')", []).unwrap();
        assert_eq!(
            crate::graph_db::read_unread_keys_strict(&conn).unwrap(),
            Vec::<bsl_search::FileKey>::new(),
            "an explicit empty unread set is valid metadata",
        );
        conn.execute("UPDATE meta SET value = 'not json' WHERE key = 'unread_paths'", []).unwrap();
        assert!(
            crate::graph_db::read_unread_keys_strict(&conn).is_err(),
            "a payload that will not decode was read as an empty answer",
        );
        assert!(
            crate::graph_db::read_unread_paths(&conn).is_empty(),
            "control: the lenient reader still answers empty, which is why it is not authority",
        );
        conn.execute(
            "UPDATE meta SET value = '[\"/ws/legacy.bsl\"]' WHERE key = 'unread_paths'",
            [],
        )
        .unwrap();
        assert!(
            crate::graph_db::read_unread_keys_strict(&conn).is_err(),
            "schema 21 requires structured root/path unread keys",
        );
    }

    #[test]
    fn prepare_snapshot_rejects_corrupt_unread_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let fingerprint = workspace_fingerprint(root);
        seed_cache(root, fingerprint);

        let path = crate::cache::graph_db_path(root);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("UPDATE meta SET value = 'not json' WHERE key = 'unread_paths'", []).unwrap();
        drop(conn);

        let graph = GraphState::for_workspace(root.to_path_buf());
        match graph.prepare_snapshot_pool(7, fingerprint, false) {
            Err(SnapshotPrepareError::Open(error)) => assert!(
                error.to_string().contains("structured unread_paths"),
                "corrupt unread metadata must retain its cause: {error:#}"
            ),
            Err(SnapshotPrepareError::Changed) => {
                panic!("corrupt unread metadata must be an operation error")
            }
            Ok(_) => panic!("corrupt unread metadata must prevent snapshot preparation"),
        }
    }
}
