use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

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
    pub(super) map: Option<std::collections::BTreeMap<String, (u128, u64)>>,
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
    pub(super) disk_fp: crate::graph_db::GraphFp,
    /// Whether the scan behind `disk_fp` covered the whole tree — the reload
    /// decision needs it to retire a `force_stale` build once the tree heals.
    pub(super) clean: bool,
}

/// Every publication prepares this many independent read handles before becoming ready.
pub(crate) const SNAPSHOT_POOL_CAP: usize = 4;

#[derive(Debug)]
pub(crate) enum BackgroundSnapshotError {
    Changed,
    Operation(anyhow::Error),
}

#[cfg(test)]
type SnapshotOpenHook = Box<dyn FnOnce()>;
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum BackgroundSnapshotFailure {
    Changed,
    Open,
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
}

#[derive(Default)]
pub(super) struct SnapshotPool {
    generation: u64,
    entries: Vec<PooledSnapshotEntry>,
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
    entries: Vec<PooledSnapshotEntry>,
    path_identity: GraphPathIdentity,
    expected_generation: u64,
    expected_fingerprint: crate::graph_db::GraphFp,
    expected_force_stale: bool,
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
    /// Modules this artefact could not read when it was built or last patched.
    pub(crate) fn unread_files(&self) -> usize {
        self.unread_files
    }

    /// The root table, when this snapshot has one.
    pub(crate) fn workspace_roots(&self) -> Option<&bsl_search::WorkspaceRoots> {
        self.workspace_roots.as_ref()
    }
}

/// A read handle checked out of (and returned to) [`GraphState::snapshot_pool`].
/// Dereferences to the underlying [`GraphDb`]; on drop the handle goes back to the
/// pool (up to [`SNAPSHOT_POOL_CAP`]) so the next query skips the multi-GB open.
pub(crate) struct PooledGraphDb {
    entry: Option<PooledSnapshotEntry>,
    pool: Arc<Mutex<SnapshotPool>>,
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
            let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            if pool.generation == entry.generation && pool.len() < SNAPSHOT_POOL_CAP {
                pool.push(entry);
            }
        }
    }
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
                Some(BackgroundSnapshotFailure::Open) => 2,
                Some(BackgroundSnapshotFailure::PrepareSecondChanged) => 3,
            },
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// Open and validate a complete request pool without holding the lease fence.
    pub(super) fn prepare_snapshot_pool(
        &self,
        expected_generation: u64,
        expected_fingerprint: crate::graph_db::GraphFp,
        expected_force_stale: bool,
    ) -> Result<PreparedSnapshotPool, SnapshotPrepareError> {
        let path = self
            .graph_db_path()
            .ok_or_else(|| SnapshotPrepareError::Open(anyhow::anyhow!("graph path unavailable")))?;
        let before = prepare_path_identity(&path)?;
        let mut entries = Vec::with_capacity(SNAPSHOT_POOL_CAP);
        for _index in 0..SNAPSHOT_POOL_CAP {
            #[cfg(test)]
            if _index == 1
                && self.background_snapshot_failure.load(std::sync::atomic::Ordering::SeqCst) == 3
            {
                return Err(SnapshotPrepareError::Changed);
            }
            let db = GraphDb::open(&path).map_err(SnapshotPrepareError::Open)?;
            let (generation, fingerprint, force_stale) =
                db.freshness_token().map_err(SnapshotPrepareError::Open)?;
            if generation != expected_generation
                || fingerprint != expected_fingerprint
                || force_stale != expected_force_stale
            {
                return Err(SnapshotPrepareError::Changed);
            }
            entries.push(PooledSnapshotEntry { generation, fingerprint, force_stale, db });
        }
        let after = prepare_path_identity(&path)?;
        if before != after {
            return Err(SnapshotPrepareError::Changed);
        }
        Ok(PreparedSnapshotPool {
            entries,
            path_identity: after,
            expected_generation,
            expected_fingerprint,
            expected_force_stale,
        })
    }

    /// Revalidate the prepared path under a short ownership fence, then install the
    /// descriptors and readiness metadata while request snapshots are excluded.
    ///
    /// `reload_obligation` names the forced-reload epoch this publication discharges,
    /// or `None` when it discharges none. It is discharged inside the same critical
    /// section that installs the snapshot, so an observer holding `inner` — such as
    /// [`GraphState::claim_reload_slot`] — sees the new publication and the discharged
    /// obligation as ONE state. Discharging it after the section leaves a window in
    /// which the graph reads "reloaded, and still owing a reload", and a claim landing
    /// there starts a second full rebuild of what was just published.
    ///
    /// Only a successful install discharges: a refused lease, a `Changed` revalidation
    /// and a failed build all leave the obligation outstanding, so the forced reload is
    /// retried rather than silently dropped.
    pub(super) fn install_prepared_snapshot(
        &self,
        mut prepared: PreparedSnapshotPool,
        published: Published,
        status: GraphStatus,
        reload_obligation: Option<usize>,
    ) -> crate::workspace_lease::LeaseOperationOutcome<(), SnapshotInstallError> {
        #[cfg(test)]
        SNAPSHOT_OPEN_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        let outcome = self.lease.publish_short(&mut prepared, |prepared| {
            #[cfg(test)]
            if REFUSE_SNAPSHOT_INSTALL.with(|refuse| refuse.replace(false)) {
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
                    return Err(SnapshotInstallError::Changed)
                }
                Err(error) => return Err(SnapshotInstallError::Operation(error.to_string())),
            }
            let actual = prepared
                .entries
                .first()
                .ok_or_else(|| SnapshotInstallError::Operation("empty snapshot pool".to_owned()))?
                .db
                .freshness_token()
                .map_err(|error| SnapshotInstallError::Operation(error.to_string()))?;
            if actual != expected
                || (published.generation, published.fingerprint, published.force_stale) != expected
            {
                return Err(SnapshotInstallError::Changed);
            }
            let mut inner = lock_recover(&self.inner);
            let mut pool = lock_recover(&self.snapshot_pool);
            pool.generation = published.generation;
            pool.entries = std::mem::take(&mut prepared.entries);
            inner.published = Some(published);
            inner.status = status;
            if let Some(epoch) = reload_obligation {
                self.complete_project_reload_through(epoch);
            }
            Ok(())
        });
        if matches!(&outcome, crate::workspace_lease::LeaseOperationOutcome::Applied(())) {
            *lock_recover(&self.graph_retry) = None;
        }
        outcome
    }

    /// Snapshot the graph for a blocking query, if built. The returned
    /// [`GraphSnapshot`] owns a read-only SQLite handle and its freshness token,
    /// and can be moved onto a blocking task without holding the lock during the
    /// query.
    pub(crate) fn snapshot(&self) -> Option<GraphSnapshot> {
        let inner = lock_recover(&self.inner);
        let published = inner.published.as_ref()?;
        // Held through pool checkout in the same inner → pool order as publication. A reader
        // tagged with the old generation can therefore never drain a newly installed pool.
        let published_generation = published.generation;
        let workspace_roots = published.search_roots.clone();
        #[cfg(test)]
        SNAPSHOT_CHECKOUT_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        let mut pool = lock_recover(&self.snapshot_pool);
        while let Some(entry) = pool.pop() {
            if entry.generation == published_generation {
                let (generation, fingerprint, force_stale) =
                    (entry.generation, entry.fingerprint, entry.force_stale);
                let unread_files = entry.db.unread_files();
                return Some(GraphSnapshot {
                    graph: PooledGraphDb {
                        entry: Some(entry),
                        pool: Arc::clone(&self.snapshot_pool),
                    },
                    generation,
                    fingerprint,
                    force_stale,
                    unread_files,
                    workspace_roots,
                });
            }
        }
        None
    }

    /// Acquire a snapshot for the one background consumer that may wait for lease I/O.
    /// Requests use [`Self::snapshot`] and never reach this fallback.
    pub(crate) fn snapshot_blocking(
        &self,
    ) -> crate::workspace_lease::LeaseOperationOutcome<Option<GraphSnapshot>, BackgroundSnapshotError>
    {
        use crate::workspace_lease::{LeaseOperationError, LeaseOperationOutcome};

        if let Some(snapshot) = self.snapshot() {
            return LeaseOperationOutcome::Applied(Some(snapshot));
        }
        let (generation, fingerprint, force_stale, workspace_roots) = {
            let inner = lock_recover(&self.inner);
            let Some(published) = inner.published.as_ref() else {
                return LeaseOperationOutcome::Applied(None);
            };
            (
                published.generation,
                published.fingerprint,
                published.force_stale,
                published.search_roots.clone(),
            )
        };
        let opened = (|| -> anyhow::Result<(PooledSnapshotEntry, GraphPathIdentity)> {
            #[cfg(test)]
            if self.background_snapshot_failure.load(std::sync::atomic::Ordering::SeqCst) == 2 {
                anyhow::bail!("forced background snapshot open failure");
            }
            let path =
                self.graph_db_path().ok_or_else(|| anyhow::anyhow!("graph path unavailable"))?;
            let before = GraphPathIdentity::read(&path)?;
            let db = GraphDb::open(&path)?;
            if db.freshness_token()? != (generation, fingerprint, force_stale) {
                anyhow::bail!("graph changed while opening a background snapshot");
            }
            let after = GraphPathIdentity::read(&path)?;
            if before != after {
                anyhow::bail!("graph path changed while opening a background snapshot");
            }
            Ok((PooledSnapshotEntry { generation, fingerprint, force_stale, db }, after))
        })();
        let (entry, identity) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                return LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                    BackgroundSnapshotError::Operation(error),
                ))
            }
        };

        #[cfg(test)]
        SNAPSHOT_OPEN_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        let mut prepared = Some((entry, identity));
        match self.lease.publish_short(&mut prepared, |prepared| {
            let (_, identity) = prepared
                .as_ref()
                .expect("background snapshot publication keeps its prepared value until commit");
            #[cfg(test)]
            if self.background_snapshot_failure.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                return Err(SnapshotInstallError::Changed);
            }
            let path = self.graph_db_path().ok_or_else(|| {
                SnapshotInstallError::Operation("graph path unavailable".to_owned())
            })?;
            match GraphPathIdentity::read(&path) {
                Ok(current) if current == *identity => {}
                Ok(_) => return Err(SnapshotInstallError::Changed),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SnapshotInstallError::Changed)
                }
                Err(error) => return Err(SnapshotInstallError::Operation(error.to_string())),
            }
            let inner = lock_recover(&self.inner);
            let published = inner.published.as_ref().ok_or(SnapshotInstallError::Changed)?;
            if (published.generation, published.fingerprint, published.force_stale)
                != (generation, fingerprint, force_stale)
            {
                return Err(SnapshotInstallError::Changed);
            }
            Ok(())
        }) {
            LeaseOperationOutcome::Applied(()) => {
                let (entry, _) = prepared.take().expect("successful publication retains entry");
                let unread_files = entry.db.unread_files();
                LeaseOperationOutcome::Applied(Some(GraphSnapshot {
                    graph: PooledGraphDb {
                        entry: Some(entry),
                        pool: Arc::clone(&self.snapshot_pool),
                    },
                    generation,
                    fingerprint,
                    force_stale,
                    unread_files,
                    workspace_roots,
                }))
            }
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                SnapshotInstallError::Changed,
            )) => LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                BackgroundSnapshotError::Changed,
            )),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                SnapshotInstallError::Operation(message),
            )) => LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                BackgroundSnapshotError::Operation(anyhow::anyhow!(message)),
            )),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error)) => {
                LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error))
            }
            LeaseOperationOutcome::TransientRefusal => LeaseOperationOutcome::TransientRefusal,
            LeaseOperationOutcome::Superseded => LeaseOperationOutcome::Superseded,
            LeaseOperationOutcome::Released => LeaseOperationOutcome::Released,
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
        }
    }

    pub(super) fn current_disk_fp(&self) -> Option<(crate::graph_db::GraphFp, bool)> {
        let root = self.workspace_root.as_deref()?;
        self.invalidate_scan_on_hub_drift();
        let mut cache = lock_recover(&self.scan);
        if let Some(c) = cache.as_ref() {
            if c.at.elapsed() < self.drift_interval {
                return Some((c.disk_fp, c.clean));
            }
        }
        // Asked about OUR cursor, not about the hub at large: `invalidate_scan_on_hub_drift`
        // above has just drained it, so any debt left here is the hub's own incompleteness
        // — while a shared verdict would also carry the debt of a consumer that simply
        // stopped draining, and put this one on a full walk for as long as that lasted.
        let hub_healthy = matches!(
            &self.change_hub,
            Some(hub) if matches!(
                hub.health_for(*lock_recover(&self.hub_cursor)),
                crate::change_hub::Health::Healthy
            )
        );
        if !hub_healthy {
            let mut fp_state = lock_recover(&self.fp_map);
            fp_state.map = None;
            fp_state.walked_at = None;
        }
        if hub_healthy {
            let fp_state = lock_recover(&self.fp_map);
            if let (Some(map), Some(walked_at)) = (fp_state.map.as_ref(), fp_state.walked_at) {
                if walked_at.elapsed() < WALK_VERIFY_INTERVAL {
                    let entries: Vec<(String, u128, u64)> =
                        map.iter().map(|(p, (m, l))| (p.clone(), *m, *l)).collect();
                    let fp = crate::graph_db::GraphFp {
                        files: fold_fingerprint_entries(&entries),
                        topology: fp_state.topology,
                    };
                    let clean = fp_state.clean;
                    *cache = Some(ScanCache { at: Instant::now(), disk_fp: fp, clean });
                    return Some((fp, clean));
                }
            }
        }
        self.scan_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // ONE project load serves both components: the roots the stats walk and
        // the topology hash come from the same snapshot, so the fold can never
        // pair one project state's files with another's topology.
        let project = super::input::ProjectSnapshot::load_excluding(root, &self.cache_exclusions());
        let universe = super::universe::ScannedUniverse::scan_excluding(
            &project.scan_roots,
            &project.excluded,
        );
        let clean = universe.clean();
        let mut entries: Vec<(String, u128, u64)> =
            universe.stats.into_iter().map(|s| (s.path, s.mtime, s.len)).collect();
        entries.sort();
        let topology = super::scan::topology_u64(&project.configs);
        let fp = crate::graph_db::GraphFp { files: fold_fingerprint_entries(&entries), topology };
        {
            let mut fp_state = lock_recover(&self.fp_map);
            fp_state.map = Some(entries.into_iter().map(|(p, m, l)| (p, (m, l))).collect());
            fp_state.walked_at = Some(Instant::now());
            fp_state.topology = topology;
            fp_state.clean = clean;
        }
        *cache = Some(ScanCache { at: Instant::now(), disk_fp: fp, clean });
        Some((fp, clean))
    }

    fn invalidate_scan_on_hub_drift(&self) {
        let Some(hub) = &self.change_hub else {
            return;
        };
        let cursor = {
            let mut slot = lock_recover(&self.hub_cursor);
            match *slot {
                Some(cursor) => cursor,
                None => {
                    let cursor = hub.subscribe();
                    *slot = Some(cursor);
                    cursor
                }
            }
        };
        let batch = hub.drain(cursor);
        *lock_recover(&self.hub_cursor) = Some(batch.cursor);
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
        let Some(map) = fp_state.map.as_mut() else {
            return;
        };
        for entry in relevant {
            let key = entry.canonical.to_string_lossy().into_owned();
            match stat_pair(&entry.canonical) {
                Some(pair) => {
                    map.insert(key, pair);
                }
                None => {
                    map.remove(&key);
                }
            }
        }
    }
}

fn entry_touches_scan_universe(entry: &ChangeEntry) -> bool {
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
fn entry_is_config_file(entry: &ChangeEntry) -> bool {
    let is_config = |path: &Path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(project_model::is_project_input_file_name)
    };
    is_config(&entry.canonical) || is_config(&entry.raw)
}

pub(super) fn fold_fingerprint_entries(entries: &[(String, u128, u64)]) -> u64 {
    let mut hasher = DefaultHasher::new();
    entries.hash(&mut hasher);
    hasher.finish()
}

fn stat_pair(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((mtime, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::super::scan::workspace_fingerprint;
    use super::super::state::{lock_recover, GraphState};
    use super::super::test_support::{
        sample_workspace, seed_cache, wait_ready, wait_until, wait_until_within, write,
    };
    use super::*;
    use crate::change_hub::WorkspaceChangeHub;
    use std::time::Duration;

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

        lock_recover(&graph.snapshot_pool).clear();
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
            },
            GraphStatus::Ready { files: 0 },
            None,
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
    fn superseded_graph_serves_only_preopened_snapshot() {
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
        assert!(graph.snapshot().is_some(), "a returned preopened descriptor remains readable");
        lock_recover(&graph.snapshot_pool).clear();
        assert!(
            graph.snapshot().is_none(),
            "a cleared pool is never refilled after terminal supersession"
        );

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

        let pool_len = || lock_recover(&graph.snapshot_pool).len();
        assert_eq!(pool_len(), SNAPSHOT_POOL_CAP, "publication prepares the full pool");
        let s1 = graph.snapshot().expect("snapshots");
        assert_eq!(pool_len(), SNAPSHOT_POOL_CAP - 1);
        drop(s1);
        assert_eq!(pool_len(), SNAPSHOT_POOL_CAP, "the dropped handle returns to the pool");
        let s2 = graph.snapshot().expect("snapshots");
        assert_eq!(pool_len(), SNAPSHOT_POOL_CAP - 1);
        drop(s2);
        assert_eq!(pool_len(), SNAPSHOT_POOL_CAP);

        {
            let mut pool = lock_recover(&graph.snapshot_pool);
            let entry = pool.pop().expect("one parked entry");
            pool.push(PooledSnapshotEntry { generation: entry.generation + 100, ..entry });
        }
        let s3 = graph.snapshot().expect("snapshots");
        assert_eq!(s3.generation, 7, "a superseded handle never serves a new request");
        assert_eq!(pool_len(), SNAPSHOT_POOL_CAP - 2, "the stale entry was discarded");
        drop(s3);

        let old = graph.snapshot().expect("old generation checkout");
        {
            let mut pool = lock_recover(&graph.snapshot_pool);
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
        for entry in &mut prepared.entries {
            entry.generation = generation + 1;
        }

        let pending = Arc::new(Mutex::new(Some(prepared)));
        let publisher = Arc::new(Mutex::new(None));
        let publishing_graph = graph.clone();
        let pending_for_hook = Arc::clone(&pending);
        let publisher_for_hook = Arc::clone(&publisher);
        set_snapshot_checkout_hook(Box::new(move || {
            let graph = publishing_graph.clone();
            let publish = move || {
                let prepared = lock_recover(&pending_for_hook).take().unwrap();
                let mut inner = lock_recover(&graph.inner);
                let mut pool = lock_recover(&graph.snapshot_pool);
                pool.generation = generation + 1;
                pool.entries = prepared.entries;
                inner.published.as_mut().unwrap().generation = generation + 1;
            };
            match publishing_graph.inner.try_lock() {
                Ok(guard) => {
                    drop(guard);
                    publish();
                }
                Err(_) => {
                    *lock_recover(&publisher_for_hook) = Some(std::thread::spawn(publish));
                }
            }
        }));

        let old = graph.snapshot().expect("checkout wins before the queued publication");
        lock_recover(&publisher).take().unwrap().join().unwrap();
        drop(old);

        let pool = lock_recover(&graph.snapshot_pool);
        assert_eq!(pool.generation, generation + 1);
        assert_eq!(pool.len(), SNAPSHOT_POOL_CAP, "the complete new pool survives old checkout");
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
        let changed = graph.clone();
        set_snapshot_install_hook(Box::new(move || {
            lock_recover(&changed.inner).published.as_mut().unwrap().generation += 1;
        }));
        assert!(matches!(
            graph.snapshot_blocking(),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(_))
        ));

        let (_dir, graph, _lease) = ready_graph();
        let _occupied = occupy(&graph);
        graph.set_background_snapshot_failure_for_test(Some(BackgroundSnapshotFailure::Open));
        let result = graph.snapshot_blocking();
        graph.set_background_snapshot_failure_for_test(None);
        assert!(matches!(
            result,
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(_))
        ));

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

        let hub = WorkspaceChangeHub::start(vec![root.to_path_buf()]);
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

        let hub = WorkspaceChangeHub::start(vec![root.to_path_buf()]);
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
        wait_until(&graph, "the drift reload to publish generation 2", || {
            graph.snapshot().map(|s| s.generation) == Some(2)
        });

        // The re-armed hub must deliver events under the NEW root. The write is
        // repeated per poll so a delivery is observed even if the ack landed a
        // moment after the generation became visible.
        let cursor = hub.subscribe();
        let file = ext.join("Новый.bsl");
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
            if batch.entries.iter().any(|e| e.raw == file) {
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
        use crate::change_hub::WorkspaceChangeHub;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let hub = WorkspaceChangeHub::start(vec![root.to_path_buf()]);
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

        let hub = WorkspaceChangeHub::start(vec![root.to_path_buf()]);
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

        let hub = WorkspaceChangeHub::start(vec![root.to_path_buf()]);
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
}
