use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vfs::{Vfs, VfsPath};

use crate::change_hub::{Health, SinkCursor, WorkspaceChangeHub};
use crate::graph::input::{build_source_root, db_for_files_lazy, ProjectSnapshot};

use super::drift::{
    compute_freshness, config_identity, config_identity_now, reconcile_interval, ScanCache,
    DRIFT_CHECK_INTERVAL,
};
use super::resident::{canonical_key, DiagnosticsResident, HoleOrigin, ResidentVfs};
use super::types::{DiagnosticsStatus, ReloadState, ResidentOutcome, StatusReport, WatchReport};

/// Drop the resident database after this long with no `diagnostics file` call, so a
/// standalone server reclaims the ~2.8 GB after a burst. The next call rebuilds.
const IDLE_EVICTION: Duration = Duration::from_secs(600);

/// How often the idle sweeper wakes to check the last-access time.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// One resident build's output: the resident itself, the drift baseline and the
/// config identity (all derived from the SAME project snapshot), plus that
/// snapshot's scan roots for the post-publish hub re-arm.
struct ResidentBuild {
    resident: DiagnosticsResident,
    stats: HashMap<String, u64>,
    config_fp: u64,
    scan_roots: Vec<PathBuf>,
    /// The age of the build snapshot these roots were taken from, for the hub declaration:
    /// the build and its declaration are separated by an arbitrary delay, during which a
    /// newer build may declare first (github#184).
    declaration_epoch: u64,
    /// The build snapshot's portable topology hash, published in the freshness envelope.
    topology: u64,
    /// The physical topology hash, for the hub re-arm supersession guard: the hub watches
    /// directories, and a root retargeted to another directory keeps its portable hash.
    physical_topology: u64,
    /// Whether the walk this resident was built from could speak for the whole tree.
    scan_clean: bool,
}

/// Everything mutable about the resident db, guarded by one `Mutex`. The lock is held
/// for the duration of a per-file query or an incremental/full reload, so the two are
/// mutually exclusive (the db is `!Sync`, so a query cannot run off-thread anyway).
pub(super) struct Inner {
    pub(super) status: DiagnosticsStatus,
    pub(super) resident: Option<DiagnosticsResident>,
    /// Per-file `(path → stat fingerprint)` from the last build/apply, for drift diff.
    pub(super) stats: HashMap<String, u64>,
    /// Folded fingerprint of the analyzer config files, for config drift.
    pub(super) config_fp: u64,
    /// The topology hash of the snapshot this resident was built from, published in
    /// the freshness envelope so an answer names the root set it was computed over.
    ///
    /// Remembering it is safe for the whole life of a generation: a topology change
    /// moves `config_fp` ([`super::drift::config_identity`]) and a moved `config_fp`
    /// reloads the resident, so the stored value cannot outlive the shape it describes.
    pub(super) topology: u64,
    pub(super) generation: u64,
    /// Bumped every time `stats` moves, under this same lock.
    ///
    /// A scan is a snapshot of disk paired with the baseline it was meant to be compared
    /// against; once the baseline moves, the comparison is between two different worlds and
    /// produces a diff that runs BACKWARDS — a file added since the snapshot looks deleted.
    /// The snapshot carries the epoch it was taken at so the apply can refuse it.
    pub(super) baseline_epoch: u64,
    pub(super) reload: ReloadState,
    /// When the current `Loading` build started, for the `status`/`loading` envelope's
    /// `elapsed_ms`. Set on `Idle → Loading`, cleared when the resident becomes `Ready`.
    pub(super) loading_since: Option<Instant>,
}

/// Handle to the workspace diagnostics database. Cheap to clone (shared `Arc`s).
#[derive(Clone)]
pub(crate) struct DiagnosticsState {
    pub(super) inner: Arc<Mutex<Inner>>,
    pub(super) scan: Arc<Mutex<Option<ScanCache>>>,
    pub(super) last_access: Arc<Mutex<Instant>>,
    pub(super) shutdown: Arc<AtomicBool>,
    /// Released together with the flag above: the sweeper parks between ticks, and a flag it
    /// only reads on waking would keep it asleep for a whole interval after the daemon has
    /// gone.
    pub(super) shutdown_wake: Arc<(Mutex<bool>, std::sync::Condvar)>,
    /// Guards spawning exactly one idle sweeper for the lifetime of the handle.
    pub(super) sweeper_started: Arc<AtomicBool>,
    pub(super) workspace_root: Option<PathBuf>,
    /// Subtrees this profile must not read as sources — the server's derived cache.
    ///
    /// Held here because the diagnostics passes walk the tree on their own and are the
    /// two places that have no other way to learn where the cache is. Empty for the
    /// profiles and tests that own no cache.
    pub(super) excluded: Vec<PathBuf>,
    pub(super) drift_interval: Duration,
    pub(super) eviction_after: Duration,
    /// The daemon's filesystem change hub, when this profile has one. Drift is served
    /// event-driven (drain-on-read) while the hub is healthy, falling back to the
    /// throttled scan otherwise. `None` for reference/shared profiles and tests, which
    /// keep the pure scan path.
    pub(super) change_hub: Option<WorkspaceChangeHub>,
    /// This state's cursor into the hub. Subscribed when a resident is (re)built and
    /// dropped on eviction, so an idle diagnostics profile never pins the accumulator.
    pub(super) hub_cursor: Arc<Mutex<Option<SinkCursor>>>,
    /// Set by [`Self::force_rescan`]: forces the next poll onto the scan path even when
    /// the hub is healthy (the `metadata object` miss escape hatch).
    pub(super) force_scan: Arc<AtomicBool>,
    /// When the hole retry list was last walked. Throttled on `drift_interval` like
    /// the scan, because a retry reads each hole WHOLE off disk (`read_to_string`
    /// fails UTF-8 only after reading), and `poll_drift` runs on every request.
    pub(super) last_hole_retry: Arc<Mutex<Option<Instant>>>,
    /// When each hole was last re-read on a request's account. A file that stays unreadable
    /// is the steady state a hole exists for, and re-reading it whole under the resident's
    /// write lock on every request about it is the very cost the sweep above is paced to
    /// avoid — so the named retry is paced too, per file.
    pub(super) last_named_hole_retry: Arc<Mutex<HashMap<String, Instant>>>,
    /// When the reconciler last ran a watchdog scan.
    pub(super) last_reconcile: Arc<Mutex<Instant>>,
    pub(super) reconcile_interval: Duration,
    /// Count of actual workspace walks (not cache hits), so a test can assert the
    /// event-driven hot path performs no scan.
    pub(super) scan_count: Arc<AtomicUsize>,
    /// When the drift poll last compared the scope's resolved git refs, so the
    /// ref-only-movement check stays off the per-request hot path.
    pub(super) scope_ref_check_at: Arc<Mutex<Option<Instant>>>,
    /// Whether the hub's stream can be trusted to carry everything since this resident read
    /// disk: the watch was already armed when the resident subscribed, or a scan has since
    /// run under an armed one. False puts the drift back on its own walk — the window
    /// between a build that raced the arming and the arming itself is in no event.
    pub(super) hub_armed: Arc<AtomicBool>,
    /// Whether the idle sweeper thread is running right now. Raised before the thread starts
    /// and lowered by a guard it owns, so every way out — a return, a panic, a thread that
    /// never started — lowers it. A test cannot otherwise see a detached thread leave.
    #[cfg(test)]
    pub(super) sweeper_live: Arc<AtomicBool>,
    /// Full rebuilds STARTED — the operation itself, not one of its effects. A rebuild
    /// that is declined at the swap leaves no trace in the resident, so nothing else can
    /// tell "asked for and thrown away" from "never asked for".
    #[cfg(test)]
    pub(super) rebuilds_started: Arc<AtomicUsize>,
    /// Forces asked of the storm guard — the ASK, not its effect. Counting the arms instead
    /// would make the count depend on the age of the scan cache, because the guard declines
    /// to arm within its floor: the same call would be counted or not according to time.
    ///
    /// One consultation of the stale-miss hatch spends one force or two, so this cannot
    /// answer HOW MANY TIMES a caller consulted; [`Self::stale_miss_consultations`] is that
    /// number.
    #[cfg(test)]
    pub(super) forced_rescans: Arc<AtomicUsize>,
    /// Consultations of the stale-miss rescan hatch: one per read that met a stale miss and
    /// decided to ask, whatever the storm guard then made of the ask.
    ///
    /// This is the number a handler-level gate states — "this tool asks on its own miss and
    /// on nothing else" is a claim about consultations, and a second consultation is exactly
    /// what a count of forces cannot tell from one consultation the guard declined once.
    #[cfg(test)]
    pub(super) stale_miss_consultations: Arc<AtomicUsize>,
    /// One-shot test seam fired between the reconciler's first drain and its scan.
    #[cfg(test)]
    pub(super) reconcile_probe: ReconcileProbe,
    /// One-shot test seam fired between the reconciler's scan and its second drain — the
    /// only window in which an event can arrive that the scan's snapshot cannot contain.
    #[cfg(test)]
    pub(super) post_scan_probe: ReconcileProbe,
    /// One-shot test seam fired between the read path's health decision and its drain.
    /// A reconcile debt raised inside that window is the only way the drain returns one:
    /// a debt visible earlier sends the read down the scan path instead.
    #[cfg(test)]
    pub(super) pre_drain_probe: ReconcileProbe,
    /// One-shot test seam fired immediately before the stale-miss retry consults the storm
    /// guard. The guard's answer depends on the AGE of the scan cache at that instant, and
    /// the read standing between a stand's setup and this point is unbounded on a loaded
    /// machine — so a stand that wants a particular answer has to state the age here, where
    /// it is read, rather than earlier and hope it survives.
    #[cfg(test)]
    pub(super) pre_force_probe: ReconcileProbe,
}

/// A one-shot callback the reconciler fires between its first drain and its scan.
#[cfg(test)]
pub(super) type ReconcileProbe = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

impl DiagnosticsState {
    /// A disabled handle (reference / shared profiles).
    pub(crate) fn disabled() -> Self {
        Self::with_status(DiagnosticsStatus::Disabled, None)
    }

    /// A workspace handle that builds its resident db lazily on first use.
    /// Count the resident as in use without reading it.
    ///
    /// Idle eviction measures the time since the last READ, which is the right measure for
    /// callers that read and leave. A caller waiting for the build to finish is using the
    /// resident just as much, and has nothing to read yet: without this, the sweeper sees an
    /// untouched resident, evicts it the moment it is published, and the waiter goes on
    /// waiting for a build nobody will start again.
    pub(crate) fn mark_in_use(&self) {
        *lock_recover(&self.last_access) = Instant::now();
    }

    /// How long since the resident was last used, for gates whose subject is that measure.
    #[cfg(test)]
    pub(crate) fn access_age(&self) -> Duration {
        lock_recover(&self.last_access).elapsed()
    }

    /// Publish a build failure without running a build, for tests that need the state a
    /// failed load leaves behind and cannot provoke one from outside.
    #[cfg(test)]
    pub(crate) fn fail_for_test(&self, message: &str) {
        let mut inner = lock_recover(&self.inner);
        inner.loading_since = None;
        inner.status = DiagnosticsStatus::Failed(message.to_owned());
    }

    pub(crate) fn for_workspace(workspace_root: PathBuf) -> Self {
        Self::with_status(DiagnosticsStatus::Idle, Some(workspace_root))
    }

    /// Take the subtrees this profile must not read as sources (see [`Self::excluded`]).
    #[must_use]
    pub(crate) fn with_excluded(mut self, excluded: Vec<PathBuf>) -> Self {
        self.excluded = excluded;
        self
    }

    fn with_status(status: DiagnosticsStatus, workspace_root: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                status,
                resident: None,
                stats: HashMap::new(),
                config_fp: 0,
                topology: 0,
                generation: 0,
                baseline_epoch: 0,
                reload: ReloadState::Idle,
                loading_since: None,
            })),
            scan: Arc::new(Mutex::new(None)),
            last_access: Arc::new(Mutex::new(Instant::now())),
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_wake: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
            sweeper_started: Arc::new(AtomicBool::new(false)),
            workspace_root,
            excluded: Vec::new(),
            drift_interval: DRIFT_CHECK_INTERVAL,
            eviction_after: IDLE_EVICTION,
            change_hub: None,
            hub_cursor: Arc::new(Mutex::new(None)),
            force_scan: Arc::new(AtomicBool::new(false)),
            last_hole_retry: Arc::new(Mutex::new(None)),
            last_named_hole_retry: Arc::new(Mutex::new(HashMap::new())),
            last_reconcile: Arc::new(Mutex::new(Instant::now())),
            reconcile_interval: reconcile_interval(),
            scan_count: Arc::new(AtomicUsize::new(0)),
            scope_ref_check_at: Arc::new(Mutex::new(None)),
            hub_armed: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            sweeper_live: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            rebuilds_started: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            forced_rescans: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            stale_miss_consultations: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            reconcile_probe: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            post_scan_probe: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            pre_drain_probe: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            pre_force_probe: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach the daemon's change hub so drift is served event-driven (drain-on-read)
    /// while the watcher is healthy, with the scan as the reconcile/fallback oracle.
    pub(crate) fn with_change_hub(mut self, hub: WorkspaceChangeHub) -> Self {
        self.change_hub = Some(hub);
        self
    }

    pub(crate) fn status(&self) -> DiagnosticsStatus {
        lock_recover(&self.inner).status.clone()
    }

    /// A lifecycle snapshot for the `status` action and the enriched `loading` envelope.
    pub(crate) fn status_report(&self) -> StatusReport {
        let inner = lock_recover(&self.inner);
        let (state, files) = match &inner.status {
            DiagnosticsStatus::Disabled => ("disabled", None),
            DiagnosticsStatus::Idle => ("idle", None),
            DiagnosticsStatus::Loading => ("loading", None),
            DiagnosticsStatus::Ready { files } => ("ready", Some(*files)),
            DiagnosticsStatus::Failed(_) => ("failed", None),
        };
        let error = match &inner.status {
            DiagnosticsStatus::Failed(msg) => Some(msg.clone()),
            _ => None,
        };
        let watch = self.change_hub.as_ref().map(|hub| {
            let health = hub.health();
            WatchReport {
                mode: if matches!(health, Health::Healthy) {
                    "event-driven"
                } else {
                    "scan-fallback"
                },
                health: health.label(),
                events_seen: hub.events_seen(),
            }
        });
        StatusReport {
            state,
            generation: inner.generation,
            files,
            unread_files: inner.resident.as_ref().map(|r| r.unread_count()),
            reload: inner.reload.label(),
            error,
            elapsed_ms: inner.loading_since.map(|t| t.elapsed().as_millis() as u64),
            watch,
        }
    }

    /// Whether the idle sweeper thread is running right now.
    #[cfg(test)]
    pub(crate) fn sweeper_running(&self) -> bool {
        self.sweeper_live.load(Ordering::SeqCst)
    }

    /// Start the idle sweeper without a resident build to hang it on, for a test whose
    /// subject is the sweeper's own lifetime.
    #[cfg(test)]
    pub(crate) fn spawn_sweeper_for_test(&self) {
        self.spawn_sweeper();
    }

    /// Stop the idle sweeper (called on server shutdown).
    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let (stopped, wake) = &*self.shutdown_wake;
        *lock_recover(stopped) = true;
        wake.notify_all();
    }

    /// Trigger the background build if this is the first call (or the db was evicted).
    /// Transitions `Idle → Loading` and spawns exactly one loader thread; later calls
    /// return immediately. No-op for disabled / loading / ready / failed states.
    pub(crate) fn ensure_loading(&self) {
        if self.workspace_root.is_none() {
            return;
        }
        {
            let mut inner = lock_recover(&self.inner);
            if inner.status != DiagnosticsStatus::Idle {
                return;
            }
            inner.status = DiagnosticsStatus::Loading;
            inner.loading_since = Some(Instant::now());
        }
        // Spawn the single idle sweeper on first use (it outlives evict→rebuild cycles).
        if !self.sweeper_started.swap(true, Ordering::SeqCst) {
            self.spawn_sweeper();
        }
        let state = self.clone();
        let spawned = std::thread::Builder::new()
            .name("bsl-diag-init".to_owned())
            .spawn(move || state.run_load());
        if let Err(e) = spawned {
            let mut inner = lock_recover(&self.inner);
            inner.loading_since = None;
            inner.status = DiagnosticsStatus::Failed(format!("could not spawn loader: {e}"));
        }
    }

    /// Run `f` against the resident db under the lock, after refreshing the workspace
    /// on disk (throttled). Bumps the last-access time so the idle sweeper does not
    /// evict an actively-used db. The closure's borrow cannot outlive the guard, so a
    /// concurrent writer (reload / eviction) never aliases a live read.
    ///
    /// `f` MUST NOT call back into `&self` methods that lock `inner` (`status`,
    /// `generation`, `freshness`, …): the lock is held across `f` and is non-reentrant,
    /// so doing so self-deadlocks. The current generation is passed to `f` so it can
    /// stamp a result id consistent with the resident state it reads — never call
    /// `generation()` inside `f`. The freshness verdict is returned alongside the
    /// result, computed under the same lock, so the caller need not (and must not)
    /// re-sample it.
    /// The resident read, reachable only through [`ResidentSession`] in production.
    ///
    /// Narrowed on purpose: a tool that reads the resident directly reads it on the
    /// master handle, outside any request's cancellation registry — which is exactly the
    /// shape every cancellation defect on this path has had. Tests keep direct access:
    /// they assert on the resident itself, and routing them through a request would test
    /// the door instead of the subject.
    #[cfg(not(test))]
    pub(super) fn read<F, R>(&self, f: F) -> ResidentOutcome<R>
    where
        F: FnOnce(&DiagnosticsResident, u64) -> R,
    {
        self.read_impl(f)
    }

    #[cfg(test)]
    pub(crate) fn read<F, R>(&self, f: F) -> ResidentOutcome<R>
    where
        F: FnOnce(&DiagnosticsResident, u64) -> R,
    {
        self.read_impl(f)
    }

    fn read_impl<F, R>(&self, f: F) -> ResidentOutcome<R>
    where
        F: FnOnce(&DiagnosticsResident, u64) -> R,
    {
        self.read_mut_impl(|resident, generation| f(&*resident, generation))
    }

    /// [`Self::read`] for a reader that trims the resident's own memo caches as it
    /// goes (the workspace sweep): the same lock, the same drift reconciliation and
    /// the same freshness verdict, with the resident borrowed mutably.
    pub(crate) fn read_mut<F, R>(&self, f: F) -> ResidentOutcome<R>
    where
        F: FnOnce(&mut DiagnosticsResident, u64) -> R,
    {
        self.read_mut_impl(f)
    }

    fn read_mut_impl<F, R>(&self, f: F) -> ResidentOutcome<R>
    where
        F: FnOnce(&mut DiagnosticsResident, u64) -> R,
    {
        *lock_recover(&self.last_access) = Instant::now();
        // Freshness is handled before taking the lock: an incremental apply needs the
        // same mutex and it is non-reentrant. A full rebuild runs off-thread and this
        // read serves the current (stale) resident meanwhile. After `poll_drift`, the
        // generation read under the lock matches the resident content `f` will query.
        self.poll_drift();
        self.refresh_diagnostics_baseline();

        // Snapshot the drift scan BEFORE taking the inner lock (scan → inner order,
        // matching `freshness`), so the freshness verdict and the result are computed
        // from one consistent resident state. Without this, a reload finishing between
        // the read and a separate freshness sample could report `stale: false` for an
        // already-superseded generation.
        // When the hub is healthy, `poll_drift` above has already reconciled the resident
        // to disk through the drain path, so freshness needs no scan — staleness reduces
        // to an in-flight reload. Only fall back to a freshness scan with no hub or a
        // degraded one (the scan path), keeping the healthy hot path free of a walk.
        // Asked about OUR cursor, like the drain decision above it: a debt belongs to the
        // consumer that owes it, and a shared verdict would spend a walk here on somebody
        // else's silence.
        let cursor = *lock_recover(&self.hub_cursor);
        let hub_healthy = matches!(
            &self.change_hub,
            Some(hub) if matches!(hub.health_for(cursor), Health::Healthy)
        );
        let scan = if matches!(self.status(), DiagnosticsStatus::Ready { .. }) && !hub_healthy {
            self.workspace_root.as_deref().and_then(|root| self.throttled_scan(root))
        } else {
            None
        };

        let mut inner = lock_recover(&self.inner);
        let inner: &mut Inner = &mut inner;
        let generation = inner.generation;
        match &inner.status {
            DiagnosticsStatus::Ready { .. } => {
                let freshness = compute_freshness(inner, scan.as_ref());
                match inner.resident.as_mut() {
                    Some(resident) => {
                        let result = f(resident, generation);
                        // After the read: its database clone is gone, so the trim never
                        // waits on this request's own handle.
                        resident.trim_after_read();
                        ResidentOutcome::Ready(result, freshness)
                    }
                    None => ResidentOutcome::Loading,
                }
            }
            DiagnosticsStatus::Idle | DiagnosticsStatus::Loading => ResidentOutcome::Loading,
            DiagnosticsStatus::Disabled => ResidentOutcome::Disabled,
            DiagnosticsStatus::Failed(msg) => ResidentOutcome::Failed(msg.clone()),
        }
    }

    fn refresh_diagnostics_baseline(&self) {
        let mut inner = lock_recover(&self.inner);
        let Some(resident) = inner.resident.as_mut() else { return };
        if !resident.diagnostics_baseline.moved_since_load(&resident.project) {
            return;
        }
        resident.diagnostics_baseline =
            ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load_reusing(
                &resident.project,
                &resident.diagnostics_baseline,
            );
    }

    /// The resident's current generation, bumped on every build / reload / incremental
    /// apply. Test-only observation point; production reads it under the lock via
    /// [`Self::read`], which returns it folded into the freshness verdict.
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.poll_drift();
        lock_recover(&self.inner).generation
    }

    /// Number of actual workspace walks performed (cache misses), for asserting the
    /// event-driven hot path does no scanning.
    /// Whether a hub cursor is currently held (dropped on eviction).
    /// Drain this state's cursor and throw the entries away, advancing past them without
    /// applying — simulating a lossy sink so the reconciler has an undelivered change to
    /// catch.
    /// Arm the one-shot reconciler probe (fired between its first drain and its scan).
    /// Detect and handle on-disk drift since the last build/apply. Reconciled in place
    /// (under the resident mutex) for `.bsl` body edits and any `.xml` add/remove/edit —
    /// the latter through a metadata-substrate point-refresh. Only a non-`.xml` add/remove
    /// or an analyzer-config change forces a full off-thread rebuild. Throttled and a
    /// no-op unless Ready.
    /// The throttled full-scan drift path: at most one workspace walk per drift window,
    /// diffed against the last-applied stats and reconciled through the same rules the
    /// hub-driven path feeds. Returns whether any drift was found (so the reconciler can
    /// tell a lossy-backend miss from an in-sync workspace). This is the parity path used
    /// when there is no hub or the hub is degraded.
    /// Apply already-classified full-scan drift: a full rebuild for structural/config
    /// drift, else an in-place metadata + body apply. Returns whether any drift was
    /// handled. Shared by the scan path and the reconciler (which classifies once, then
    /// re-checks late-delivered events before deciding to degrade).
    /// The event-driven drift path: drain this state's cursor and reconcile only the
    /// changed paths. Empty drain → nothing to do (crucially, NO scan on the hot path).
    /// A hub overflow (`rescan_required`) → fall back to the full scan, today's path.
    /// Reconcile the paths a drain reported. Events are hints, stats are truth: each path
    /// is re-stat'd and classified through the SAME fingerprint diff the scan uses, then
    /// fed the identical downstream — a full rebuild for structural/config drift, else an
    /// in-place metadata + body apply. Only the affected paths are stat'd, never the tree.
    /// Apply already-classified event-driven drift to the resident and advance the drift
    /// baseline incrementally (only the drained paths change). Shares the exact resident
    /// mutation — substrate point-refresh + body re-key + `Running`-guard — with the scan
    /// path via [`apply_resident_changes`]; only the stats update differs (delta vs full
    /// rebase), because the drain has no whole-workspace scan to rebase onto.
    #[allow(
        clippy::too_many_arguments,
        reason = "one bucket per drift class, same as the classifier"
    )]
    /// Reconcile metadata-only structural drift in place, without a whole-db rebuild:
    /// point-refresh the metadata substrate for the drifted `.xml` (re-discovering the
    /// affected roots and re-reading only changed/new composing files) and re-key the
    /// drifted `.bsl` bodies to their on-disk revision. Everything runs under ONE hold of
    /// the resident mutex — the same discipline as a body-only apply — so a concurrent
    /// full rebuild (which also takes the lock) can never swap the resident mid-apply. A
    /// modified `.bsl` with no resident FileId means the file universe moved, so we bail
    /// to a full rebuild. The drift baseline is rebased to `scan` once reconciled, and the
    /// generation is bumped only when a Salsa input actually moved.
    /// Spawn a full rebuild (at most one in flight) that replaces the resident db.
    /// Peak RAM is bounded: the new db is built, then swapped under the write lock and
    /// the old one dropped — the brief overlap is the price of a structural change.
    pub(super) fn kick_full_reload(&self) {
        {
            let mut inner = lock_recover(&self.inner);
            if inner.reload == ReloadState::Running {
                return;
            }
            inner.reload = ReloadState::Running;
        }
        let state = self.clone();
        let spawned = std::thread::Builder::new()
            .name("bsl-diag-reload".to_owned())
            .spawn(move || state.run_reload());
        if let Err(e) = spawned {
            let mut inner = lock_recover(&self.inner);
            inner.reload = ReloadState::Failed(format!("could not spawn reload: {e}"));
        }
    }

    /// The initial resident build: load every `.bsl` text into a fresh database with
    /// the LSP-equivalent inputs, index path→FileId, record the drift baseline, and
    /// publish `Ready`. On success spawns the idle sweeper.
    fn run_load(&self) {
        let Some(root) = self.workspace_root.clone() else {
            return;
        };
        // Snapshot the hub cursor at build start: the build's baseline scan captures the
        // disk as of now, so only events landing after this point need replaying onto the
        // published resident.
        self.resubscribe_cursor();
        tracing::info!(?root, "diagnostics resident db build started");
        match Self::catch_build(|| Self::build_resident(&root, &self.excluded)) {
            Ok(built) => {
                let files = built.resident.file_count();
                {
                    let mut inner = lock_recover(&self.inner);
                    inner.resident = Some(built.resident);
                    inner.stats = built.stats;
                    inner.baseline_epoch += 1;
                    inner.config_fp = built.config_fp;
                    inner.topology = built.topology;
                    inner.generation += 1;
                    inner.reload = ReloadState::Idle;
                    inner.loading_since = None;
                    inner.status = DiagnosticsStatus::Ready { files };
                }
                *lock_recover(&self.scan) = None;
                tracing::info!(files, "diagnostics resident db ready");
                self.ensure_hub_roots(
                    &built.scan_roots,
                    built.physical_topology,
                    built.declaration_epoch,
                );
                self.recheck_config_identity_after_publish();
            }
            Err(msg) => {
                tracing::warn!("diagnostics resident db build failed: {msg}");
                let mut inner = lock_recover(&self.inner);
                inner.loading_since = None;
                inner.status = DiagnosticsStatus::Failed(msg);
            }
        }
    }

    /// A config/topology edit landing while a resident build ran is invisible to the
    /// published baseline — baseline and resident deliberately derive from the build's
    /// own project snapshot. One cheap re-derivation (config-file stats plus a fresh
    /// project load; no tree walk) right after publication closes that window by
    /// kicking the reload again when the identity moved mid-build.
    fn recheck_config_identity_after_publish(&self) {
        let Some(root) = self.workspace_root.as_deref() else {
            return;
        };
        let current = config_identity_now(root);
        if current != lock_recover(&self.inner).config_fp {
            tracing::info!("project config/topology changed during the resident build; reloading");
            self.kick_full_reload();
        }
    }

    /// A full rebuild triggered by structural drift: build a fresh resident off-thread,
    /// then swap it in under the write lock. Keeps the old resident served until the
    /// swap; on failure the old one stays and `reload` is flagged failed.
    fn run_reload(&self) {
        let Some(root) = self.workspace_root.clone() else {
            return;
        };
        #[cfg(test)]
        self.rebuilds_started.fetch_add(1, Ordering::SeqCst);
        // Fresh cursor snapshot at rebuild start; events during the rebuild replay onto
        // the new resident, events before it are covered by the rebuild's baseline scan.
        self.resubscribe_cursor();
        match Self::catch_build(|| Self::build_resident(&root, &self.excluded)) {
            Ok(built) => {
                // A swap states outright which files exist, so a build that could not read
                // the whole tree may not perform one: it would retire a serving resident in
                // favour of a shorter universe, dropping live files — the same loss the
                // incremental path refuses, arriving by the one route that bypasses it.
                // The drift that asked for this rebuild is left unanswered on purpose; it
                // is still on disk, so the next window asks again, and the request is
                // granted the first time the tree can be read whole. A first build has no
                // resident to protect and publishes regardless: half a universe beats none.
                if !built.scan_clean && lock_recover(&self.inner).resident.is_some() {
                    tracing::info!(
                        "declining to swap in a resident built over a partially unreadable \
                         tree; the pending drift is retried once the tree reads whole"
                    );
                    {
                        let mut inner = lock_recover(&self.inner);
                        inner.reload = ReloadState::Idle;
                    }
                    // This rebuild's cursor was resubscribed at its start, which advances
                    // past every event the build was assumed to cover. Declining the build
                    // makes that assumption false: those events are now recorded nowhere.
                    // A scan re-derives them from disk, so route the next poll through one
                    // instead of leaving them to the watchdog a minute and a half later.
                    // What that scan may then apply is its own question — body text yes,
                    // structure not until the tree reads whole.
                    *lock_recover(&self.scan) = None;
                    self.force_scan.store(true, Ordering::SeqCst);
                    return;
                }
                let files = built.resident.file_count();
                let mut inner = lock_recover(&self.inner);
                inner.resident = Some(built.resident);
                inner.stats = built.stats;
                inner.baseline_epoch += 1;
                inner.config_fp = built.config_fp;
                inner.topology = built.topology;
                inner.generation += 1;
                inner.reload = ReloadState::Idle;
                inner.status = DiagnosticsStatus::Ready { files };
                drop(inner);
                *lock_recover(&self.scan) = None;
                tracing::info!(files, "diagnostics resident db reloaded");
                self.ensure_hub_roots(
                    &built.scan_roots,
                    built.physical_topology,
                    built.declaration_epoch,
                );
                self.recheck_config_identity_after_publish();
            }
            Err(msg) => {
                tracing::warn!("diagnostics resident db reload failed: {msg}");
                {
                    let mut inner = lock_recover(&self.inner);
                    inner.reload = ReloadState::Failed(msg);
                }
                // The SERVING resident is the old one, and this rebuild already moved the
                // cursor past every event it was assumed to cover and raised `hub_armed` for
                // a resident that never arrived. Both of those describe a build that did not
                // happen: the old resident may well have been derived while the watch was
                // still arming, and the window between arming and its baseline is carried by
                // no event at all. A scan re-derives it from disk — the same repair the
                // declined-swap branch above makes, for the same reason.
                *lock_recover(&self.scan) = None;
                self.hub_armed.store(false, Ordering::SeqCst);
                self.force_scan.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Run a resident-build closure with panic isolation. A panic in the build thread must
    /// NOT leave the status pinned at `Loading` forever (the agent would see "still
    /// building" with no recovery, and the idle sweeper only evicts from `Ready`). Folding
    /// both an `Err` and a panic into `Err(String)` lets the caller publish `Failed`, which
    /// is visible and retryable. Generic over the closure so the fold itself is testable.
    pub(super) fn catch_build<T>(build: impl FnOnce() -> anyhow::Result<T>) -> Result<T, String> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(build)) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(e.to_string()),
            Err(panic) => {
                let detail = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_owned());
                Err(format!("diagnostics build panicked: {detail}"))
            }
        }
    }

    /// Build a resident db over every workspace `.bsl`, with the same Salsa inputs the
    /// LSP server registers (source root, per-file source root + text, all config
    /// paths). Returns the resident, the per-file drift baseline, and the config fp.
    fn build_resident(root: &Path, excluded: &[PathBuf]) -> anyhow::Result<ResidentBuild> {
        // The config-file stat is captured BEFORE the project load: the published
        // identity must describe the config state the resident was built FROM. A
        // stat taken after the build could pair a mid-build config edit's mtime
        // with the old snapshot's settings — the post-publish recheck would then
        // see its own mixture as current and never reload.
        let config_files_fp = super::drift::config_files_fingerprint(root);
        // Load the project ONCE: the scan universe, the config roots (with their
        // dependency closures) and the `[diagnostics]` settings all derive from
        // this single snapshot, so a reload can never mix two project states.
        let project = crate::project::at(root)
            .map_err(|e| anyhow::anyhow!("invalid project at {}: {e}", root.display()))?;
        let snapshot = ProjectSnapshot::from_project_excluding(&project, excluded);
        // Taken BEFORE the walk, because `WorkspaceRoots::build` canonicalises against the disk
        // rather than reading the already-loaded project: built afterwards, it could describe a
        // retargeted symlink the walk never saw, and then the table and `by_path` would be two
        // snapshots instead of one.
        let (workspace_roots, _rejected) = crate::project::workspace_roots(&project, excluded);
        // ONE scan serves both the resident's file set and the drift baseline
        // below: two walks here could disagree (a file deleted between them would
        // sit in the resident forever, invisible to every later drift scan,
        // because the baseline never contained it).
        let universe = crate::graph::universe::ScannedUniverse::scan_project(&snapshot);
        let scan_clean = universe.clean();
        let files = &universe.files;
        // `ProjectSnapshot` already registers canonical roots, matching the
        // canonical `.bsl` universe the scan produces.
        let configs = snapshot.configs.clone();
        let diagnostics = project.config.diagnostics.rules_json();
        let mut config = ide::DiagnosticsConfig::from_project_json(
            &diagnostics,
            project.config.output.resolve_locale().unwrap_or_default(),
        );
        // `[analysis].diff_base`: restrict diagnostics to the vendor diff. Computed
        // synchronously — the resident build is already the heavy bootstrap phase —
        // and refreshed on body drift so the filter tracks the working copy.
        let diff_base = project.config.analysis.diff_base.clone();
        let mut scope_identity = None;
        if let Some(base) = diff_base.as_deref() {
            let (scope, identity) = super::resident::build_scope(root, base);
            config.scope = scope;
            scope_identity = identity;
        }
        // `[analysis].ignored_authors`: blame-backed line filter, pinned to the
        // current HEAD; the drift poll rebuilds it when the refs move.
        let ignored_authors = project.config.analysis.ignored_authors.clone();
        let author_filter = super::resident::build_author_filter(root, &ignored_authors);
        let source_root = build_source_root(files);
        // Disk-backed: register each file's content revision and drop its text, so the
        // whole-workspace resident is not pinned as salsa inputs (which OOMs on a large
        // config). `file_text_query` re-reads on demand under its LRU cap — the same
        // model the LSP server and CLI `analyze` use.
        let crate::graph::input::BatchLoad { mut db, unread } =
            db_for_files_lazy(&source_root, files, &configs, None);
        // A file whose bytes could not be read is NOT served: it is held out of
        // `by_path` and answered as "known but unreadable", and the retry list
        // re-reads it every window. Its baseline fingerprint stays — the scan did
        // parse this state of the disk, so re-adding it every window would storm.
        // Build holes are `Admitted`: the build IS the admission for the configuration
        // it was started for.
        let holes: HashMap<String, HoleOrigin> =
            unread.iter().map(|p| (canonical_key(p), HoleOrigin::Admitted)).collect();

        // Pre-seed the VFS with the SAME FileIds the source root uses for each `.bsl`,
        // in enumerate order, so the interner assigns id `i` to `files[i]`. The metadata
        // bootstrap resolves common-module / service module back-links through
        // `vfs.file_id(<Module.bsl>)`; without these ids present it drops them silently.
        // Bootstrap then allocates only the metadata-XML ids on top of this id space.
        let vfs = ResidentVfs(RefCell::new(Vfs::default()));
        {
            let mut guard = vfs.0.borrow_mut();
            for (file_id, path) in files {
                let allocated = guard.alloc_file_id(VfsPath::new(path.clone()));
                // A hard check, not `debug_assert`: this is a one-time O(n) pass at
                // resident build (not a hot path), and a release-mode misalignment
                // would silently scatter every metadata back-link with no signal.
                assert_eq!(
                    allocated, *file_id,
                    "resident VFS must mirror enumerate_bsl_files ids for back-link resolution",
                );
            }
        }
        // Runs AFTER every file is registered, so the unreadable ones already carry
        // their mark: the substrate asks the database per body, and a body asked too
        // early would get a back-link to text nobody could read.
        ide_host_core::bootstrap_metadata_substrate(&mut db, &vfs);

        let mut by_path = HashMap::with_capacity(files.len());
        for (file_id, path) in files {
            let key = canonical_key(path);
            if holes.contains_key(&key) {
                continue;
            }
            by_path.insert(key, *file_id);
        }
        // The drift baseline derives from the SAME scan that gave the resident its
        // files (and the config identity from the same snapshot) — a fresh walk
        // here could reflect a change that landed mid-build, and comparing later
        // scans against that newer baseline would hide the drift forever.
        let stats: HashMap<String, u64> =
            universe.stats.iter().map(|s| (s.path.clone(), s.fingerprint())).collect();
        let config_fp = config_identity(config_files_fp, &snapshot.configs);

        let topology = snapshot.portable_topology;
        let diagnostics_baseline =
            ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load(&project);
        Ok(ResidentBuild {
            resident: DiagnosticsResident {
                db,
                #[cfg(test)]
                read_trims: 0,
                sweep_pool: std::sync::OnceLock::new(),
                vfs,
                by_path,
                holes,
                config,
                workspace_root: root.to_path_buf(),
                workspace_roots,
                diff_base,
                scope_identity,
                ignored_authors,
                author_filter,
                diagnostics_baseline,
                project,
            },
            stats,
            config_fp,
            scan_roots: snapshot.scan_roots,
            declaration_epoch: snapshot.declaration_epoch,
            topology,
            physical_topology: crate::graph::scan::topology_u64(&snapshot.configs),
            scan_clean,
        })
    }

    /// After a successful (re)build, re-point the daemon's change hub at the build
    /// snapshot's scan roots (no-op when unchanged): a topology reload that added
    /// an extension root must start receiving its events instead of leaving that
    /// subtree to the periodic reconciler.
    fn ensure_hub_roots(
        &self,
        scan_roots: &[PathBuf],
        built_topology: u64,
        declaration_epoch: u64,
    ) {
        let (Some(hub), Some(root)) = (&self.change_hub, self.workspace_root.as_deref()) else {
            return;
        };
        // A slow build finishing after a newer topology reload must not roll the
        // shared hub back onto its older root set (see the graph-side twin). That
        // check is a barrier, not a proof: overtaken between its re-read and the
        // declaration, this build is caught by the epoch the declaration carries
        // (github#184).
        let live = crate::graph::input::ProjectSnapshot::load_excluding(root, &self.excluded);
        if crate::graph::scan::topology_u64(&live.configs) != built_topology {
            tracing::info!("skipping hub re-arm: the built snapshot's topology is superseded");
            return;
        }
        if !hub.ensure_scope(
            &crate::change_hub::watch_targets_for(root, scan_roots),
            &live.user_excluded,
            declaration_epoch,
        ) {
            tracing::warn!("resident rebuild could not re-arm the change hub onto new roots");
        }
    }

    /// Drop the throttled scan cache so the next [`Self::read`] re-scans immediately, even
    /// inside the drift window. Used when a `metadata object` lookup misses, in case the
    /// object was added since the last scan. Storm-guarded: it only drops the cache when
    /// the last scan is older than [`FORCE_RESCAN_FLOOR`], so a loop of genuinely-absent
    /// lookups cannot stat-walk the workspace faster than that floor (the D2 discipline).
    /// (Re)subscribe this state's hub cursor, dropping any prior one. Called at the start
    /// of a (re)build so the fresh cursor is snapshotted at that moment: events BEFORE the
    /// build are captured by the build's own baseline scan, events DURING/AFTER it stay
    /// pending and apply to the freshly-published resident.
    /// The canonical paths of the analyzer config files at the workspace root — the exact
    /// set [`config_files_fingerprint`] folds. A drained path counts as config drift only if it
    /// is one of these, matching the scan path (which fingerprints only `root.join(name)`)
    /// so an identically-named file elsewhere in the tree is not a spurious rebuild trigger.
    /// Drop this state's hub cursor so an evicted (or never-built) resident does not pin
    /// the accumulator against reclamation.
    /// Drain this state's cursor once, apply the delivered changes, and return the set of
    /// canonical paths the drain reported (empty when there is no cursor). A hub overflow
    /// (`rescan_required`) reconciles via a full scan and reports no paths. The path set
    /// lets the reconciler tell a late-but-delivered edit from a genuinely-missed one.
    /// The reconciler/watchdog: a periodic full scan that catches any drift the hub's
    /// event stream failed to deliver (a lossy backend). Runs on the idle sweeper thread.
    /// Applies everything the hub delivered, then scans for the residue; a change that a
    /// second drain shows was merely late (delivered DURING the scan, not missed) does not
    /// degrade — only genuinely-undelivered file drift does.
    /// A one-shot test seam fired between the reconciler's first drain and its scan, so a
    /// test can inject an edit that lands DURING the scan window (delivered, not missed).
    /// The throttled disk scan: at most one walk per drift interval, its result shared
    /// by concurrent callers. Returns a borrowed view valid until the next scan.
    /// The single idle sweeper, spawned once per handle and living until shutdown. Each
    /// tick it drops the resident db (→ `Idle`) when it has been `Ready` and untouched
    /// for `eviction_after`; otherwise it sleeps on. It does NOT exit after evicting, so
    /// a later rebuild (via `ensure_loading`/`run_reload`) is monitored again without
    /// needing a replacement sweeper.
    fn spawn_sweeper(&self) {
        let state = self.clone();
        #[cfg(test)]
        let live = SweeperLive::raise(&self.sweeper_live);
        let _ = std::thread::Builder::new().name("bsl-diag-sweep".to_owned()).spawn(move || {
            #[cfg(test)]
            let _live = live;
            loop {
                let tick = SWEEP_INTERVAL.min(state.eviction_after).min(state.reconcile_interval);
                {
                    // Parked on the shutdown's own signal rather than sleeping it out: the
                    // difference between leaving in a moment and leaving in half a minute.
                    let (stopped, wake) = &*state.shutdown_wake;
                    let guard = lock_recover(stopped);
                    let (guard, _) = wake
                        .wait_timeout_while(guard, tick, |stopped| !*stopped)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if *guard {
                        return;
                    }
                }
                if state.shutdown.load(Ordering::SeqCst) {
                    return;
                }

                // Watchdog: catch drift the hub's event stream may have missed. Runs on this
                // existing thread (no new one), independent of eviction, at its own cadence.
                if state.change_hub.is_some()
                    && lock_recover(&state.last_reconcile).elapsed() >= state.reconcile_interval
                    && matches!(state.status(), DiagnosticsStatus::Ready { .. })
                {
                    *lock_recover(&state.last_reconcile) = Instant::now();
                    state.reconcile_tick();
                }

                state.evict_if_idle();
            }
        });
    }

    /// Drop the resident db (→ `Idle`) when it is `Ready` and has gone untouched for
    /// `eviction_after`. Reports whether the eviction happened.
    ///
    /// The sweeper is only the clock above this step: the schedule lives there, the
    /// decision and the act live here. A test whose subject is the eviction itself
    /// calls this directly and so does not depend on when the clock gets round to it.
    pub(super) fn evict_if_idle(&self) -> bool {
        if !matches!(self.status(), DiagnosticsStatus::Ready { .. }) {
            return false;
        }
        if lock_recover(&self.last_access).elapsed() < self.eviction_after {
            return false;
        }
        let mut inner = lock_recover(&self.inner);
        // Re-check under the lock: a read between the idle check and the lock bumps
        // last_access, and a reload may be in flight — never evict either.
        if lock_recover(&self.last_access).elapsed() < self.eviction_after
            || !matches!(inner.status, DiagnosticsStatus::Ready { .. })
            || inner.reload == ReloadState::Running
        {
            return false;
        }
        inner.resident = None;
        inner.stats.clear();
        inner.baseline_epoch += 1;
        inner.status = DiagnosticsStatus::Idle;
        // Release the hub cursor while `inner` is still held: `status` reads under the
        // same lock, so no observer can catch an evicted resident that still pins the
        // accumulator. Safe in this order because no path holds the cursor and then
        // asks for `inner` — unlike `scan`, which `throttled_scan` holds across its
        // own `inner` read and which therefore stays outside.
        self.drop_cursor();
        drop(inner);
        *lock_recover(&self.scan) = None;
        tracing::info!("diagnostics resident db evicted after idle period");
        true
    }

    /// Leave the idle sweeper unstarted, so `ensure_loading` raises no clock.
    ///
    /// A test whose subject is the act of eviction does not want the schedule: with a
    /// sweeper armed before the resident exists, every assertion about the state BEFORE
    /// eviction races the eviction itself — the resident may vanish between the
    /// publication of `Ready` and the observation of it.
    #[cfg(test)]
    pub(super) fn without_idle_sweeper(&self) {
        self.sweeper_started.store(true, Ordering::SeqCst);
    }
}

/// Raised while the idle sweeper thread runs; lowered on every way out of it.
#[cfg(test)]
struct SweeperLive(Arc<AtomicBool>);

#[cfg(test)]
impl SweeperLive {
    fn raise(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(Arc::clone(flag))
    }
}

#[cfg(test)]
impl Drop for SweeperLive {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// An owned snapshot of one drift scan, decoupled from the cache lock.
/// The freshness verdict for `inner` against an optional drift `scan`. Computed by the
/// caller while holding the inner lock, so the verdict (`revision`/`stale`/`reload`) is
/// atomic with the generation a read serves. `scan` is `None` when the db is not Ready
/// (nothing to compare), making `stale` depend only on an in-flight reload.
/// A fold of the analyzer config files' `(presence, len, mtime)`. Any change forces a
/// full rebuild because it can alter the project's extension set and thus the db inputs.
pub(crate) fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics_state::test_support::sample_workspace;

    /// The sweeper parks between ticks, and the shutdown wakes it out of that park rather
    /// than leaving it to time out. The tick is half a minute: slept out, a daemon told to
    /// stop keeps a thread walking its workspace for the rest of it.
    #[test]
    fn a_shutdown_wakes_the_idle_sweeper_out_of_its_tick() {
        use crate::change_hub::test_support::eventually;
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let state = DiagnosticsState::for_workspace(dir.path().to_path_buf());
        state.spawn_sweeper();
        assert!(
            eventually(Duration::from_secs(5), || state.sweeper_running()),
            "the sweeper never started, so its exit below would prove nothing"
        );

        // Parked: its next tick is SWEEP_INTERVAL away and nothing else wakes it.
        std::thread::sleep(Duration::from_millis(100));
        assert!(state.sweeper_running());
        assert!(
            SWEEP_INTERVAL >= Duration::from_secs(5),
            "the tick is short enough that timing out would pass for being woken"
        );

        let asked = Instant::now();
        state.shutdown();

        assert!(
            eventually(Duration::from_secs(1), || !state.sweeper_running()),
            "the sweeper slept through the shutdown"
        );
        assert!(asked.elapsed() < Duration::from_secs(1));
    }

    /// One resident build is exactly ONE traversal: the file set and the drift
    /// baseline are projections of the same scan. Historically this path walked
    /// twice, and a file deleted between the walks stayed in the resident forever —
    /// the baseline never contained it, so no later drift scan could evict it.
    #[test]
    fn a_resident_build_walks_the_tree_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let before = project_model::source_set::scans_performed_on_thread();
        let built = DiagnosticsState::build_resident(root, &[]).expect("resident builds");
        let walks = project_model::source_set::scans_performed_on_thread() - before;

        assert!(walks > 0, "a zero count means the instrumentation broke");
        assert_eq!(walks, 1, "files and drift baseline come from the one scan");
        assert!(
            built.stats.keys().any(|k| k.ends_with("Module.bsl")),
            "the baseline describes the scanned universe"
        );
    }

    /// A rebuild walks the tree the workspace path names NOW, not the one it named earlier.
    ///
    /// The root is kept as the project DECLARED it and read again on every build — this entry
    /// point is the only one a rebuild has — so a workspace reached through a link follows that
    /// link wherever it currently points. Resolving the root once, when the daemon takes it in,
    /// would freeze which physical tree is served: every later rebuild would enumerate the
    /// directory the link named at startup and answer about a workspace that has since moved.
    #[cfg(unix)]
    #[test]
    fn a_rebuild_walks_the_tree_the_workspace_points_at_now() {
        let dir = tempfile::tempdir().unwrap();
        let served = dir.path().join("served");
        let moved_to = dir.path().join("moved-to");
        for (tree, module) in [(&served, "Альфа"), (&moved_to, "Бета")] {
            let ext = tree.join(format!("CommonModules/{module}/Ext"));
            std::fs::create_dir_all(&ext).unwrap();
            std::fs::write(ext.join("Module.bsl"), "Процедура П() Экспорт КонецПроцедуры\n")
                .unwrap();
        }
        let workspace = dir.path().join("workspace");
        std::os::unix::fs::symlink(&served, &workspace).unwrap();

        // Which tree was walked, read off the module directory each scanned body sits in — the
        // absolute spellings differ per run, the owning module name does not.
        let modules = |stats: &HashMap<String, u64>| -> Vec<String> {
            let mut names: Vec<String> = stats
                .keys()
                .filter(|key| key.ends_with("Module.bsl"))
                .filter_map(|key| {
                    let ext = Path::new(key).parent()?;
                    ext.parent()?.file_name()?.to_str().map(str::to_owned)
                })
                .collect();
            names.sort();
            names
        };

        let served_build = DiagnosticsState::build_resident(&workspace, &[]).expect("builds");
        assert_eq!(
            modules(&served_build.stats),
            ["Альфа"],
            "control: the workspace names the tree it was pointed at",
        );

        std::fs::remove_file(&workspace).unwrap();
        std::os::unix::fs::symlink(&moved_to, &workspace).unwrap();

        let moved_build = DiagnosticsState::build_resident(&workspace, &[]).expect("builds");
        assert_eq!(
            modules(&moved_build.stats),
            ["Бета"],
            "and a rebuild goes where it points now, rather than where it pointed before",
        );
    }
}
