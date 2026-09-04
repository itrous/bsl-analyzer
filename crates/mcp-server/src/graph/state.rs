//! Graph lifecycle state and publication protocol.

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU8;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bsl_search::SearchEngine;

use crate::change_hub::{SinkCursor, WorkspaceChangeHub};
use crate::state::retry_window::{RetryDecision, RetryWindow};

use super::build::PublishAttemptOutcome;
use super::snapshot::{FpMapState, ScanCache, SnapshotPool};
use super::types::{
    Freshness, FusedStartup, GraphPublishOutcome, GraphPublishSignal, GraphStatus,
    GraphStatusReport, NudgeOutcome, SUPERSEDED_GRAPH_ERROR,
};

/// Minimum time between on-disk drift scans. A scan stats every `.bsl`/`.xml`
/// file under the config roots, so throttling bounds its cost regardless of how
/// fast an agent fires `graph` calls.
const DRIFT_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// State of an in-flight or last-attempted background reload, surfaced to agents
/// so a failed reload is visible rather than leaving them at `stale=true` forever.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ReloadState {
    /// No reload in flight; the published snapshot is the latest.
    Idle,
    /// A reload triggered by detected drift is running in the background.
    Running,
    /// The last reload failed; the previous snapshot is still served.
    Failed(String),
}

impl ReloadState {
    pub(super) fn label(&self) -> &'static str {
        match self {
            ReloadState::Idle => "none",
            ReloadState::Running => "running",
            ReloadState::Failed(_) => "failed",
        }
    }
}

/// The published build's freshness metadata. Publication installs this together with
/// the generation's pre-opened snapshot pool, so request reads never reopen the shared path.
pub(super) struct Published {
    /// Whether this snapshot was published KNOWING it does not reflect current disk
    /// (the boot stale-publish). Any successful build/reload publish replaces it with
    /// a fresh entry. Gates the boot leftover-marks consume: even if the pre-claimed
    /// catch-up fails (`reload` drops to `Failed`, so `drift_pending` no longer
    /// holds), marks must not be cleared against this snapshot.
    pub(super) stale: bool,
    pub(super) generation: u64,
    pub(super) fingerprint: crate::graph_db::GraphFp,
    pub(super) reload: ReloadState,
    /// The published build's coherence marker: it straddled a disk write or was
    /// built over an incomplete scan, so it never was a faithful snapshot. A
    /// fingerprint comparison alone cannot retire it — the incomplete-scan case
    /// leaves the fingerprints EQUAL — so the reload decision must read it.
    pub(super) force_stale: bool,
    /// Search roots paired with this publication: from the build snapshot for a fresh
    /// artifact, or from the fingerprint-verified live project for cached adoption.
    pub(super) search_roots: Option<bsl_search::WorkspaceRoots>,
}

impl Published {
    /// Whether disk state warrants a fresh build: the tree moved past this build's
    /// fingerprint, or this build never was coherent (`force_stale`) and the
    /// current scan is CLEAN — an unclean scan must not trigger the rebuild, or a
    /// chronically unreadable subtree would rebuild in a loop, each build unclean
    /// again. Recovery (the subtree becomes readable) rebuilds exactly once.
    pub(super) fn wants_reload(&self, disk: Option<(crate::graph_db::GraphFp, bool)>) -> bool {
        match disk {
            Some((fp, scan_clean)) => fp != self.fingerprint || (self.force_stale && scan_clean),
            None => false,
        }
    }
}

/// Everything mutable about the published graph, guarded by a single mutex. Locks
/// are only held for brief reads/swaps — the load and the drift scan run without
/// this lock held.
pub(super) struct Inner {
    pub(super) status: GraphStatus,
    pub(super) published: Option<Published>,
}

/// Handle to the workspace call graph. Cheap to clone (shared `Arc`s).
///
/// Loading is lazy: the SQLite graph is built off the workspace on first use, so a
/// server whose user never touches the graph pays nothing. The build is triggered
/// on the first `graph` tool call.
#[derive(Clone)]
pub(crate) struct GraphState {
    pub(super) inner: Arc<Mutex<Inner>>,
    pub(super) scan: Arc<Mutex<Option<ScanCache>>>,
    pub(super) workspace_root: Option<PathBuf>,
    pub(super) cache: Option<crate::cache::WorkspaceCacheLayout>,
    pub(super) drift_interval: Duration,
    /// The daemon's change hub, when this profile has one. The graph does NOT apply
    /// drift in place (its fast path deliberately full-rebuilds on a metadata touch); the
    /// hub only lets a freshness check invalidate its throttled fingerprint cache the
    /// instant a change is delivered, instead of waiting out the drift throttle.
    pub(super) change_hub: Option<WorkspaceChangeHub>,
    /// This graph's cursor into the hub. Subscribed lazily on first freshness check.
    pub(super) hub_cursor: Arc<Mutex<Option<SinkCursor>>>,
    /// Count of actual fingerprint walks (cache misses), so a test can assert an irrelevant
    /// delivered change did NOT invalidate the throttled cache and re-trigger a scan.
    pub(super) scan_count: Arc<AtomicUsize>,
    /// Event-maintained per-file stat map mirroring what a fingerprint walk observes,
    /// so a query-path freshness check can fold ~100k in-memory entries (<1ms) instead
    /// of stat-walking the tree (seconds). Seeded by a real walk, patched per delivered
    /// hub entry, and re-anchored to a real walk every [`WALK_VERIFY_INTERVAL`] — the
    /// hub cannot see everything (events predating its subscribe, writes through paths
    /// outside the watched roots), so the walk stays the periodic source of truth.
    /// Dropped to `None` (next check walks) on hub overflow or a subtree removal.
    pub(super) fp_map: Arc<Mutex<FpMapState>>,
    /// Idle read handles onto the CURRENT published graph file, tagged with the
    /// freshness token they were opened under. Opening the multi-GB SQLite file costs
    /// ~a second on a large configuration; a pooled handle keeps serving the same
    /// coherent snapshot for free. Entries for superseded generations are discarded
    /// lazily at checkout (the tag no longer matches the published generation).
    pub(super) snapshot_pool: Arc<Mutex<SnapshotPool>>,
    #[cfg(test)]
    pub(super) background_snapshot_failure: Arc<AtomicU8>,
    /// Parks the building thread between a publication and the point where the
    /// force-reload obligation is discharged, so a test can sample what an outside
    /// observer could see there. Deliberately invoked with `inner` NOT held: a park
    /// under the lock blocks the observer on that same mutex, so it reads identical
    /// whether or not publication and discharge are one state — and gates nothing.
    #[cfg(test)]
    pub(super) publish_window_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Counts completed passes of [`Self::notify_published`] — every return, including the
    /// one taken when this daemon no longer owns the caches and no hook runs at all.
    ///
    /// `Ready` is a status barrier, never a publish barrier, so a test asserting anything a
    /// publish pass leaves behind needs a barrier of its own. The publish hook is not always
    /// one: the pass does further work AFTER its hook returns (the leftover-marks consume in
    /// the tail), and a test sampling inside that remainder reads a state no real consumer
    /// observes. This counter is the only observable for "the pass finished".
    #[cfg(test)]
    pub(super) publish_passes: Arc<AtomicUsize>,
    /// Invoked on this graph's background thread immediately after each publish/adopt,
    /// once the inner lock is released — the moment the graph "has caught up" and a
    /// consumer (search context re-render) may read the fresh graph. Never called on a
    /// query path. Receives a [`GraphPublishSignal`]: `build_start_seq` bounds which marks
    /// the consumer may clear (correctness), `drift_pending` is a fast-path hint.
    pub(super) on_published:
        Option<Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>>,
    /// The store's monotonic context-dirty mark counter, wired once the search engine
    /// exists (the engine is built after this graph). Read at each build's start to capture
    /// its `build_start_seq`. Absent (never wired, e.g. a disabled/reference graph, or a build
    /// racing the one-time boot wiring) reads as `0` — a consume of NOTHING, so an early build's
    /// publish can never clear a mark against a graph that predates it. Marks left pending
    /// through that window are picked up explicitly once wiring completes (see
    /// [`Self::consume_leftover_marks`]).
    pub(super) mark_seq: Arc<OnceLock<Arc<AtomicI64>>>,
    /// A one-shot armed by [`Self::consume_leftover_marks`] at boot to consume context-dirty
    /// marks left by a prior daemon run. Stores the mark-seq bound captured at the instant the
    /// caller observed those leftovers (before the engine was published): `0` = disarmed,
    /// `> 0` = armed with that bound. The boot build's own publish ran with the unwired (`0`)
    /// bound and cleared nothing; this makes the next publish (or an immediate call, for an
    /// already-published graph) re-run the refresh with the STORED bound — never a later live
    /// read, which could clear a mark a new drift stamped after the capture. Cleared via `swap`
    /// to `0`, so it fires exactly once.
    pub(super) leftover_bound: Arc<AtomicI64>,
    /// A drift observed (via [`Self::nudge_rebuild`]) while a build/reload was already in
    /// flight, so no reload slot could be claimed then. Re-checked at the next publish
    /// ([`Self::notify_published`]): if disk moved past the just-published build, a follow-up
    /// reload is claimed. Without this a drift arriving mid-build is silently lost — the
    /// build's publish would consume the search context marks against a graph built BEFORE
    /// the change.
    pub(super) pending_nudge: Arc<AtomicBool>,
    /// A topology-changed publish whose hook could not run the whole-collection
    /// context refresh (engine not yet published, or deferred behind a fresher
    /// reload). Re-raised on the next publish so the refresh is never lost.
    pub(super) pending_topology_refresh: Arc<AtomicBool>,
    /// A root-table refresh that the publish hook could not apply. Kept separate from
    /// topology context work so retrying roots does not trigger a full context rerender.
    pub(super) pending_roots_refresh: Arc<AtomicBool>,
    /// Monotonic force-project-reload request and the latest request completed by a full
    /// publication. A counter rather than a boolean prevents a request arriving during a
    /// build from being cleared by that older build.
    pub(super) project_reload_epoch: Arc<AtomicUsize>,
    pub(super) completed_project_reload_epoch: Arc<AtomicUsize>,
    /// This daemon's claim on the workspace's derived caches. The graph database is shared
    /// with every other daemon generation over the same workspace, so a superseded daemon
    /// builds and publishes nothing — it serves what it already holds and lets the owner
    /// maintain the file. Unmanaged (always owning) for a disabled graph and in tests.
    pub(super) lease: crate::workspace_lease::WorkspaceLease,
    /// Trigger-driven retry obligation created only by a transient publication refusal.
    pub(super) graph_retry: Arc<Mutex<Option<RetryWindow>>>,
}

impl GraphState {
    /// A disabled graph (reference / shared profiles).
    pub(crate) fn disabled() -> Self {
        Self::with_status(GraphStatus::Disabled, None)
    }

    /// A workspace graph that loads lazily on first use.
    #[cfg(test)]
    pub(crate) fn for_workspace(workspace_root: PathBuf) -> Self {
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(&workspace_root);
        Self::for_workspace_with_cache(workspace_root, cache)
    }

    /// A workspace graph whose derived database lives in `cache`.
    pub(crate) fn for_workspace_with_cache(
        workspace_root: PathBuf,
        cache: crate::cache::WorkspaceCacheLayout,
    ) -> Self {
        let mut state = Self::with_status(GraphStatus::Idle, Some(workspace_root));
        state.cache = Some(cache);
        state
    }

    fn with_status(status: GraphStatus, workspace_root: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner { status, published: None })),
            scan: Arc::new(Mutex::new(None)),
            workspace_root,
            cache: None,
            drift_interval: DRIFT_CHECK_INTERVAL,
            change_hub: None,
            hub_cursor: Arc::new(Mutex::new(None)),
            scan_count: Arc::new(AtomicUsize::new(0)),
            on_published: None,
            pending_nudge: Arc::new(AtomicBool::new(false)),
            pending_topology_refresh: Arc::new(AtomicBool::new(false)),
            pending_roots_refresh: Arc::new(AtomicBool::new(false)),
            project_reload_epoch: Arc::new(AtomicUsize::new(0)),
            completed_project_reload_epoch: Arc::new(AtomicUsize::new(0)),
            mark_seq: Arc::new(OnceLock::new()),
            leftover_bound: Arc::new(AtomicI64::new(0)),
            fp_map: Arc::new(Mutex::new(FpMapState::default())),
            snapshot_pool: Arc::new(Mutex::new(SnapshotPool::default())),
            #[cfg(test)]
            background_snapshot_failure: Arc::new(AtomicU8::new(0)),
            #[cfg(test)]
            publish_window_hook: None,
            #[cfg(test)]
            publish_passes: Arc::new(AtomicUsize::new(0)),
            lease: crate::workspace_lease::WorkspaceLease::unmanaged(),
            graph_retry: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach the daemon's claim on the workspace's derived caches, so this graph stops
    /// building and publishing once a newer daemon generation takes the workspace over.
    pub(crate) fn with_lease(mut self, lease: crate::workspace_lease::WorkspaceLease) -> Self {
        self.lease = lease;
        self
    }

    /// Whether this daemon may write the shared graph database. A superseded one keeps
    /// serving its published snapshot but schedules no builds: the owner maintains the file,
    /// and two processes rebuilding it only race renames and flicker generations.
    #[cfg(test)]
    pub(super) fn may_build(&self) -> bool {
        self.lease.owns_caches()
    }

    /// Refresh the lease from disk and report only the irreversible terminal state.
    pub(crate) fn is_superseded(&self) -> bool {
        if !self.lease.is_superseded() {
            let _ = self.lease.owns_caches_now();
        }
        self.lease.is_superseded()
    }

    /// Request paths may read only the terminal verdict already established by background work.
    pub(crate) fn superseded_latched(&self) -> bool {
        self.lease.is_superseded()
    }

    /// Subtrees every pass driven from this graph must not read as sources.
    ///
    /// Derived from the one layout this state was built with, so the walk sees exactly
    /// the hole the watch does. Empty when this state governs no cache.
    pub(crate) fn cache_exclusions(&self) -> Vec<std::path::PathBuf> {
        self.cache()
            .map(|cache| cache.spellings().iter().map(|path| path.to_path_buf()).collect())
            .unwrap_or_default()
    }

    pub(crate) fn cache(&self) -> Option<&crate::cache::WorkspaceCacheLayout> {
        self.cache.as_ref()
    }

    pub(crate) fn graph_db_path(&self) -> Option<PathBuf> {
        self.cache().map(crate::cache::WorkspaceCacheLayout::graph_db_path)
    }

    /// Number of fingerprint walks performed (cache misses), for asserting that an
    /// irrelevant hub delivery did not invalidate the throttled cache.
    #[cfg(test)]
    pub(super) fn scan_count(&self) -> usize {
        self.scan_count.load(Ordering::SeqCst)
    }

    /// Whether `path` is one of the analyzer config files directly in this workspace root.
    /// Basename-only detection is unsafe: nested projects may carry the same config name but
    /// cannot change this daemon's source-root table.
    pub(crate) fn is_workspace_config_path(&self, path: &Path) -> bool {
        let Some(root) = self.workspace_root.as_deref() else { return false };
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(project_model::is_project_input_file_name)
        {
            return false;
        }
        let Some(parent) = path.parent() else { return false };
        if parent == root {
            return true;
        }
        let canonical_parent =
            std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        canonical_parent == canonical_root
    }

    /// Attach the daemon's change hub so a freshness check invalidates the throttled
    /// fingerprint cache as soon as a change is delivered, without waiting out the throttle.
    pub(crate) fn with_change_hub(mut self, hub: WorkspaceChangeHub) -> Self {
        self.change_hub = Some(hub);
        self
    }

    /// Attach a hook invoked on this graph's background thread after each publish/adopt
    /// (see [`Self::notify_published`]). Used to drive the search context re-render once
    /// the graph has caught up with an `.xml` drift. The hook receives
    /// a [`GraphPublishSignal`]: `build_start_seq` bounds which marks it may clear
    /// (correctness), `drift_pending` is a skip-this-round hint (optimization).
    pub(crate) fn with_publish_hook(
        mut self,
        hook: Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>,
    ) -> Self {
        self.on_published = Some(hook);
        self
    }

    /// Attach the park described by [`Self::publish_window_hook`]: it runs on the
    /// building thread in the one full-build path, after the snapshot is installed and
    /// with `inner` released. Unrelated to [`Self::with_publish_hook`] — it takes no
    /// signal, returns nothing, and fires only there.
    #[cfg(test)]
    pub(super) fn with_publish_window_hook(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.publish_window_hook = Some(hook);
        self
    }

    /// Wire the store's monotonic mark-seq counter, shared with the search engine so a
    /// build reads the same value the store increments. Called once, after the engine is
    /// built (the engine outlives the graph's construction). Setting it again is a no-op —
    /// there is one counter per workspace store.
    pub(crate) fn set_mark_seq_source(&self, mark_seq: Arc<AtomicI64>) {
        let _ = self.mark_seq.set(mark_seq);
    }

    /// The current mark-seq high-water value, captured at a build's start as its
    /// `build_start_seq`. Unwired (disabled/reference graph, or a build racing the one-time
    /// wiring at boot) reads as `0`: a consume of NOTHING. This deliberately makes an early
    /// build's publish clear no marks, rather than an unbounded consume that could clear a
    /// mark stamped after the publish snapshotted disk. Marks stranded across the wiring
    /// window are recovered by [`Self::consume_leftover_marks`].
    ///
    /// Once wired, the bound is taken from the STORE ON DISK rather than from the engine's
    /// in-process mirror, which only reflects the stamps this daemon allocated. A mark another
    /// daemon generation stamped would otherwise sit above every bound this one captures — and
    /// a mark no bound covers is never consumed, leaving that file's context stale for good.
    /// Reading the persisted counter is one small query per build.
    pub(super) fn current_mark_seq(&self) -> i64 {
        let Some(local) = self.mark_seq.get().map(|counter| counter.load(Ordering::SeqCst)) else {
            return 0;
        };
        let persisted = match self.cache() {
            Some(cache) => {
                match bsl_search::Store::persisted_mark_seq(&cache.search_db_path()) {
                    Ok(seq) => seq,
                    Err(e) => {
                        // Falling back to the local mirror is the safe direction — a bound that
                        // is too low leaves marks pending rather than clearing them against a
                        // graph that predates them. It is not free, though: a mark another
                        // daemon stamped stays pending until a later build reads the counter
                        // successfully, so say so rather than degrade in silence.
                        tracing::warn!(
                            error = %e,
                            "could not read the persisted mark counter; this build's context \
                             refresh is bounded by what this process stamped itself"
                        );
                        0
                    }
                }
            }
            None => 0,
        };
        local.max(persisted)
    }

    /// Fire the publish hook, if any. Called after a publish/adopt with no graph lock
    /// held, so the hook may take other locks (e.g. the search engine) without risking a
    /// lock-order inversion against the graph's inner mutex.
    ///
    /// FIRST re-claims a reload for any drift recorded while this build was in flight, so
    /// the hook observes the resulting reload state through [`Self::drift_pending`]: if a
    /// follow-up reload is now catching up, the hook can skip this round and let that
    /// reload's publish consume the marks (against a fresher graph).
    ///
    /// `build_start_seq` is captured by the CALLING build at its start and passed straight
    /// through — never re-read here — so the reclaim's own new build (which captures its own
    /// later seq on another thread) cannot move the bound this publish hands the hook. The
    /// bound is what keeps the consumption correct: only marks at or below it — drifts this
    /// build already reflects — may be cleared.
    pub(super) fn notify_published(&self, build_start_seq: i64, topology_changed: bool) {
        self.notify_published_pass(build_start_seq, topology_changed);
        // Counted here rather than at the end of the pass itself: the pass has several
        // returns, and a barrier that misses one is worse than none — a test would wait on
        // a count that never arrives.
        #[cfg(test)]
        self.publish_passes.fetch_add(1, Ordering::SeqCst);
    }

    /// The pass itself; [`Self::notify_published`] wraps it only to count completions.
    fn notify_published_pass(&self, build_start_seq: i64, topology_changed: bool) {
        if !self.lease.owns_caches_now() {
            return;
        }
        if self.pending_nudge.swap(false, Ordering::SeqCst) {
            self.reclaim_pending_reload();
        }
        // Topology context and search-root refreshes are independent obligations. Every
        // successful publish checks roots, while a previously failed check stays armed until
        // the hook reports it handled. A root-only failure must never manufacture a semantic
        // topology change on retry.
        let pending_topology = self.pending_topology_refresh.swap(false, Ordering::SeqCst);
        let topology = topology_changed || pending_topology;
        self.pending_roots_refresh.swap(false, Ordering::SeqCst);
        let outcome = self.fire_hook(build_start_seq, topology, true);
        if topology && !outcome.topology_handled {
            self.pending_topology_refresh.store(true, Ordering::SeqCst);
        }
        if !outcome.roots_handled {
            self.pending_roots_refresh.store(true, Ordering::SeqCst);
        }
        // A leftover-marks consume was armed at boot (see `consume_leftover_marks`). This build's
        // own publish (above) captured its own `build_start_seq` — for the pre-wiring boot build
        // that is `0`, which clears nothing — so re-run the hook once with the bound captured when
        // the leftovers were observed, picking up marks a prior run left pending. Single-shot via
        // the `swap`. The STORED bound (never a live read) is what keeps the consume from clearing
        // a mark stamped after the capture: that mark is a new drift with its own nudge→publish.
        let leftover_bound = self.leftover_bound.swap(0, Ordering::SeqCst);
        if leftover_bound != 0 && !self.fire_hook(leftover_bound, false, false).topology_handled {
            self.leftover_bound.fetch_max(leftover_bound, Ordering::SeqCst);
        }
    }

    /// Invoke the publish hook, if any, with the given independent obligations.
    fn fire_hook(
        &self,
        build_start_seq: i64,
        topology_changed: bool,
        roots_refresh_requested: bool,
    ) -> GraphPublishOutcome {
        let (topology, workspace_roots) = lock_recover(&self.inner)
            .published
            .as_ref()
            .map(|p| (p.fingerprint.topology, p.search_roots.clone()))
            .unwrap_or_default();
        match &self.on_published {
            Some(hook) => hook(GraphPublishSignal {
                drift_pending: self.drift_pending(),
                build_start_seq,
                topology_changed,
                topology,
                roots_refresh_requested,
                workspace_roots,
            }),
            None => GraphPublishOutcome::HANDLED,
        }
    }

    /// Run a whole-collection context re-render that was requested before the search engine
    /// existed to run it.
    ///
    /// The request is normally raised BY a publish and consumed by that same publish's hook. At
    /// boot the order can invert: the topology mismatch is noticed while deciding what to
    /// publish, and the publish that follows may land before the engine is in the shared handle
    /// — the hook then reports the re-render unhandled and the request is merely re-raised. On
    /// a fused cold build nothing publishes again (the pass wrote the index itself), so files
    /// the build skipped as byte-identical would keep contexts rendered under the old topology
    /// indefinitely. Called once the engine is published, this closes that gap.
    ///
    /// The bound passed is `0`: the re-render marks the whole collection itself and consumes
    /// exactly that batch's own seq, so no OTHER pending mark is swept up by it.
    pub(crate) fn flush_pending_topology_refresh(&self) {
        if !self.lease.owns_caches_now() {
            return;
        }
        if !self.pending_topology_refresh.load(Ordering::SeqCst) {
            return;
        }
        // A graph that is not published cannot re-render anything against itself, and one with
        // a fresher reload already coming leaves the work to that reload's publish.
        if !matches!(self.status(), GraphStatus::Ready { .. }) || self.drift_pending() {
            return;
        }
        if self.pending_topology_refresh.swap(false, Ordering::SeqCst)
            && !self.fire_hook(0, true, false).topology_handled
        {
            self.pending_topology_refresh.store(true, Ordering::SeqCst);
        }
    }

    /// Retry a search-root transition without pretending the graph topology changed.
    pub(crate) fn flush_pending_search_roots_refresh(&self) {
        if !self.lease.owns_caches_now() {
            return;
        }
        if !self.pending_roots_refresh.load(Ordering::SeqCst) {
            return;
        }
        if !matches!(self.status(), GraphStatus::Ready { .. }) || self.drift_pending() {
            return;
        }
        if self.pending_roots_refresh.swap(false, Ordering::SeqCst)
            && !self.fire_hook(0, false, true).roots_handled
        {
            self.pending_roots_refresh.store(true, Ordering::SeqCst);
        }
    }

    /// Recover context-dirty marks a PRIOR daemon run left in the persisted `context_dirty`
    /// table. The boot graph build published BEFORE the mark-seq source was wired, so it ran
    /// with the unwired (`0`) bound and cleared nothing; the persisted marks are still pending.
    /// The caller captures `leftover_bound` — the mark-seq high-water at the instant it observed
    /// these leftovers, before the engine was published — and passes it in. That boot build read
    /// post-restart disk, so a consume against it bounded by `leftover_bound` clears exactly the
    /// leftovers and no more. Correctness: leftover marks predate this daemon run, so any
    /// boot-published graph REFLECTING CURRENT DISK (fresh build, fused, or fingerprint-valid
    /// cached) already reflects their cause — the one exception, a stale boot publish, pre-claims
    /// the reload slot atomically so the `drift_pending` guard below defers this consume to the
    /// catch-up publish; a mark stamped after the capture (seq above the bound) is a new drift
    /// and is guaranteed its own nudge→publish cycle, so it must not be cleared here against the
    /// boot graph that predates it. Handles both post-boot states: an already-published (`Ready`)
    /// graph consumes immediately; an in-flight build (`Loading`, whose own publish captured the
    /// pre-wiring `0` bound and would clear nothing) arms a one-shot so ITS publish runs the
    /// consume with `leftover_bound`. A `Ready` graph with a fresher reload already in flight
    /// (`drift_pending`) leaves the one-shot armed so that reload's publish handles it against
    /// the fresher graph.
    pub(crate) fn consume_leftover_marks(&self, leftover_bound: i64) {
        if !self.lease.owns_caches_now() {
            return;
        }
        // Arm first (store the captured bound), then observe state, so a build publishing
        // concurrently either runs the follow-up itself or leaves it for the immediate consume
        // below — the `swap` to `0` in both paths keeps it single-shot.
        self.leftover_bound.store(leftover_bound, Ordering::SeqCst);
        // `published_stale` outlives `drift_pending`: if the stale boot's pre-claimed
        // catch-up FAILS, the slot drops to `Failed` and `drift_pending` no longer
        // holds — but the snapshot still predates the marks' causes, so the one-shot
        // must stay armed for the next successful publish.
        if matches!(self.status(), GraphStatus::Ready { .. })
            && !self.drift_pending()
            && !self.published_stale()
        {
            let bound = self.leftover_bound.swap(0, Ordering::SeqCst);
            if bound != 0 && !self.fire_hook(bound, false, false).topology_handled {
                self.leftover_bound.fetch_max(bound, Ordering::SeqCst);
            }
        }
    }

    /// Whether a leftover-marks consume is still owed: `consume_leftover_marks` arms the
    /// obligation and a publish discharges it. Tests outside this module drive the consume
    /// through the real publish hook and cannot reach the field itself.
    #[cfg(test)]
    pub(crate) fn leftover_consume_pending(&self) -> bool {
        self.leftover_bound.load(Ordering::SeqCst) != 0
    }

    /// Re-run the reload claim for a drift recorded (as a pending nudge) while a build was in
    /// flight. Runs on the publish thread once the graph is `Ready`: claims a reload if disk
    /// drifted past the just-published build; re-arms the pending flag if a reload is somehow
    /// already running so its own publish re-checks; otherwise the published build already
    /// matches disk and nothing is scheduled.
    fn reclaim_pending_reload(&self) {
        if self.claim_reload_slot() {
            self.spawn_reload();
        } else if self.reload_running() {
            self.pending_nudge.store(true, Ordering::SeqCst);
        }
    }

    /// Whether the published snapshot is the boot stale-publish (known not to reflect
    /// current disk). See [`Published::stale`].
    fn published_stale(&self) -> bool {
        lock_recover(&self.inner).published.as_ref().is_some_and(|p| p.stale)
    }

    /// Whether a published reload is currently `Running`.
    fn reload_running(&self) -> bool {
        matches!(
            lock_recover(&self.inner).published.as_ref().map(|p| &p.reload),
            Some(ReloadState::Running)
        )
    }

    /// Whether a fresher build is already catching up: a nudge was recorded while a build
    /// was in flight, or a reload is currently running. The publish hook uses this only as a
    /// fast-path hint — when a follow-up reload will publish shortly it can skip this round
    /// and let that reload's publish re-render against the fresher graph. It is NOT what
    /// makes consumption correct: the `build_start_seq` bound already prevents clearing a
    /// mark against a graph that predates its drift, whatever this returns.
    pub(crate) fn drift_pending(&self) -> bool {
        self.pending_nudge.load(Ordering::SeqCst) || self.reload_running()
    }

    pub(crate) fn status(&self) -> GraphStatus {
        lock_recover(&self.inner).status.clone()
    }

    /// Request-safe freshness from the publication paired with this pre-opened snapshot.
    pub(crate) fn cached_freshness(&self, snapshot: &super::GraphSnapshot) -> Freshness {
        let (stale, reload, topology) = lock_recover(&self.inner)
            .published
            .as_ref()
            .filter(|published| published.generation == snapshot.generation)
            .map(|published| {
                (
                    published.stale || published.force_stale,
                    published.reload.label(),
                    published.fingerprint.topology,
                )
            })
            .unwrap_or((snapshot.force_stale, "none", snapshot.fingerprint.topology));
        Freshness {
            revision: snapshot.generation,
            stale: stale || snapshot.unread_files() > 0,
            reload,
            topology,
        }
    }

    /// Cached lifecycle snapshot for the `status` action. It uses only process-local state and
    /// the pre-opened descriptor pool; drift detection belongs to background graph owners.
    pub(crate) fn status_report(&self) -> GraphStatusReport {
        let superseded = self.lease.is_superseded();
        let report = |state: &'static str, superseded: Option<bool>| GraphStatusReport {
            state,
            files: None,
            unread_files: None,
            revision: None,
            stale: None,
            reload: None,
            error: None,
            superseded,
        };

        let status = {
            let inner = lock_recover(&self.inner);
            inner.status.clone()
        };

        if superseded {
            if let GraphStatus::Ready { files } = status {
                if let Some(snapshot) = self.snapshot() {
                    let freshness = self.cached_freshness(&snapshot);
                    return GraphStatusReport {
                        files: Some(files),
                        unread_files: Some(snapshot.unread_files()),
                        revision: Some(freshness.revision),
                        stale: Some(freshness.stale),
                        reload: Some(freshness.reload),
                        ..report("ready", Some(true))
                    };
                }
            }
            return GraphStatusReport {
                error: Some(SUPERSEDED_GRAPH_ERROR.to_owned()),
                ..report("failed", Some(true))
            };
        }

        match status {
            GraphStatus::Disabled => report("disabled", None),
            GraphStatus::Idle | GraphStatus::Loading => report("loading", None),
            GraphStatus::Failed(msg) => {
                GraphStatusReport { error: Some(msg), ..report("failed", None) }
            }
            GraphStatus::Ready { files } => match self.snapshot() {
                Some(snapshot) => {
                    let freshness = self.cached_freshness(&snapshot);
                    GraphStatusReport {
                        files: Some(files),
                        unread_files: Some(snapshot.unread_files()),
                        revision: Some(freshness.revision),
                        stale: Some(freshness.stale),
                        reload: Some(freshness.reload),
                        ..report("ready", None)
                    }
                }
                None => report("loading", None),
            },
        }
    }

    /// Trigger the background load if this is the first call. Transitions
    /// `Idle → Loading` and spawns exactly one loader thread; later calls return
    /// immediately. No-op for disabled / already-loading / ready / failed / terminally
    /// superseded graphs. A transient fence refusal may re-arm through `graph_retry`.
    pub(crate) fn ensure_loading(&self) {
        if self.workspace_root.is_none() || self.superseded_latched() {
            return;
        }
        let retry_allowed = lock_recover(&self.graph_retry).as_mut().is_some_and(|retry| {
            matches!(retry.refused(Instant::now(), Duration::ZERO), RetryDecision::RetryAfter(_))
        });
        {
            let mut inner = lock_recover(&self.inner);
            if inner.status != GraphStatus::Idle
                && !(retry_allowed && matches!(inner.status, GraphStatus::Failed(_)))
            {
                return;
            }
            inner.status = GraphStatus::Loading;
        }

        let state = self.clone();
        let spawned = std::thread::Builder::new()
            .name("bsl-graph-init".to_owned())
            .spawn(move || state.run_load(false));
        if let Err(e) = spawned {
            let mut inner = lock_recover(&self.inner);
            inner.status = GraphStatus::Failed(format!("could not spawn loader: {e}"));
        }
    }

    /// Claim the initial build for an external builder (the fused cold-build path).
    /// Transitions `Idle → Loading` like [`Self::ensure_loading`] but spawns no loader
    /// thread — the caller builds and installs the prepared graph itself. Returns
    /// `false` for a disabled graph or one already
    /// loading/ready/failed, in which case the caller must not build (the normal
    /// lifecycle owns it).
    pub(crate) fn try_begin_external_build(&self) -> bool {
        if self.workspace_root.is_none() || !self.lease.owns_caches_now() {
            return false;
        }
        let mut inner = lock_recover(&self.inner);
        if inner.status != GraphStatus::Idle {
            return false;
        }
        inner.status = GraphStatus::Loading;
        true
    }

    #[cfg(test)]
    pub(crate) fn adopt_prebuilt(
        &self,
        generation: u64,
        fingerprint: crate::graph_db::GraphFp,
        files: usize,
        search_roots: Option<bsl_search::WorkspaceRoots>,
    ) {
        let prepared = self.prepare_snapshot_pool(generation, fingerprint, false).unwrap();
        let outcome = self.install_prepared_snapshot(
            prepared,
            Published {
                generation,
                fingerprint,
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots,
            },
            GraphStatus::Ready { files },
            None,
        );
        assert!(matches!(outcome, crate::workspace_lease::LeaseOperationOutcome::Applied(())));
    }

    /// Abandon a claimed external build that did not produce a usable database, so the
    /// normal lazy/eager path can rebuild. Reverts `Loading → Idle`.
    pub(crate) fn abort_external_build(&self) {
        let mut inner = lock_recover(&self.inner);
        if inner.status == GraphStatus::Loading {
            inner.status = GraphStatus::Idle;
        }
    }

    /// Schedule the graph to catch up with a drift another consumer observed (the search
    /// sink), WITHOUT waiting for a `graph` tool freshness check. A user who only calls
    /// `search_code` never triggers a graph rebuild otherwise, so an `.xml` edit would
    /// leave the search chunks' graph context stale forever — the context re-render only
    /// runs on a graph publish. This closes that chain end-to-end: xml drift →
    /// context-dirty marks + this nudge → background rebuild → publish → hook → refresh.
    ///
    /// Single-flight by construction and never blocks (the rebuild runs on a spawned
    /// thread): an unbuilt graph (`Idle`) starts the one initial loader; a published graph
    /// claims the ONE reload slot only when disk drifted since the build and no reload is
    /// already running (so a storm of xml events during a running build queues no extra
    /// rebuilds); `Disabled`/`Loading`/`Failed` schedule nothing. Walks the filesystem for
    /// the drift check, so call from a blocking context (the sink thread), never a query.
    pub(crate) fn nudge_rebuild(&self) -> NudgeOutcome {
        self.nudge_after_recording(false)
    }

    /// Force a full project reload even when the graph fingerprint compares equal.
    /// Root aliases and declared spellings can change search ownership without changing
    /// canonical graph topology, so config delivery cannot use the ordinary fingerprint gate.
    pub(crate) fn nudge_project_reload(&self) -> NudgeOutcome {
        self.project_reload_epoch.fetch_add(1, Ordering::SeqCst);
        self.nudge_after_recording(true)
    }

    fn nudge_after_recording(&self, force_project: bool) -> NudgeOutcome {
        if self.is_superseded() {
            return NudgeOutcome::NoOp;
        }
        if matches!(self.status(), GraphStatus::Failed(_)) {
            if let Some(retry) = lock_recover(&self.graph_retry).as_mut() {
                retry.observe_external_work(true);
            }
        }
        match self.status() {
            GraphStatus::Idle => {
                self.ensure_loading();
                NudgeOutcome::LoadStarted
            }
            // A build is in flight and captured disk at some earlier instant. Record the
            // drift so the build's publish re-checks and reloads if disk moved past what it
            // captured — otherwise the publish would consume the search context marks against
            // a graph built before this change (the drift would be lost).
            GraphStatus::Loading => {
                self.pending_nudge.store(true, Ordering::SeqCst);
                NudgeOutcome::NoOp
            }
            GraphStatus::Ready { .. } => {
                if self.claim_reload_slot() {
                    self.spawn_reload();
                    NudgeOutcome::ReloadClaimed
                } else {
                    // Couldn't claim: either the graph already matches disk (nothing to do)
                    // or a reload is already `Running`. In the latter case the running build
                    // may have started before this drift, so record a pending nudge — its
                    // publish re-checks and reloads again if disk still differs.
                    if self.reload_running() || force_project {
                        self.pending_nudge.store(true, Ordering::SeqCst);
                    }
                    NudgeOutcome::NoOp
                }
            }
            // Fresh external work may start a new epoch after a transient refusal or operation
            // failure. Terminal supersession returned above never resumes.
            GraphStatus::Failed(_) => {
                self.ensure_loading();
                match self.status() {
                    GraphStatus::Loading => NudgeOutcome::LoadStarted,
                    _ => NudgeOutcome::NoOp,
                }
            }
            GraphStatus::Disabled => NudgeOutcome::NoOp,
        }
    }

    /// Whether a forced project reload has been requested and not yet discharged by a
    /// successful full publication. Discharged inside the publishing critical section
    /// (see [`GraphState::install_prepared_snapshot`]), so a reader holding `inner`
    /// never sees a publication whose obligation is still outstanding.
    fn project_reload_pending(&self) -> bool {
        self.project_reload_epoch.load(Ordering::SeqCst)
            > self.completed_project_reload_epoch.load(Ordering::SeqCst)
    }

    pub(super) fn project_reload_epoch(&self) -> usize {
        self.project_reload_epoch.load(Ordering::SeqCst)
    }

    pub(super) fn complete_project_reload_through(&self, epoch: usize) {
        self.completed_project_reload_epoch.fetch_max(epoch, Ordering::SeqCst);
    }

    /// Claim the single background-reload slot iff a reload is owed and none is already
    /// `Running`. Returns whether THIS call won the claim; a caller arriving while a
    /// reload runs (or when nothing is owed) gets `false`.
    ///
    /// A reload is owed on either of two independent grounds, and they are not the same
    /// question: the workspace drifted on disk since the published build, OR a forced
    /// project reload is outstanding ([`Self::project_reload_pending`]) — which is how a
    /// declared-root change gets rebuilt at all, since it moves no canonical input and so
    /// leaves the fingerprint equal.
    ///
    /// [`Self::freshness`] claims the same slot but on the drift ground ALONE; the two
    /// therefore do not share one predicate. What keeps them single-flight is the
    /// `Running` check both make under `inner`, not a common notion of "owed".
    fn claim_reload_slot(&self) -> bool {
        // Ahead of the fingerprint walk: a superseded daemon must not even pay for drift
        // detection it is not allowed to act on.
        if !self.lease.owns_caches_now() {
            return false;
        }
        let disk = self.current_disk_fp();
        let mut inner = lock_recover(&self.inner);
        let Some(published) = inner.published.as_mut() else {
            return false;
        };
        if (self.project_reload_pending() || published.wants_reload(disk))
            && published.reload != ReloadState::Running
        {
            published.reload = ReloadState::Running;
            true
        } else {
            false
        }
    }

    /// Spawn the background reload thread after a successful [`Self::claim_reload_slot`].
    /// On spawn failure the reload slot is marked `Failed` so it is never left stuck
    /// `Running` (which would block every later reload claim).
    pub(super) fn spawn_reload(&self) {
        if self.is_superseded() {
            return;
        }
        let state = self.clone();
        let spawned = std::thread::Builder::new()
            .name("bsl-graph-reload".to_owned())
            .spawn(move || state.run_load(true));
        if let Err(e) = spawned {
            let mut inner = lock_recover(&self.inner);
            if let Some(p) = inner.published.as_mut() {
                p.reload = ReloadState::Failed(format!("could not spawn reload: {e}"));
            }
        }
    }

    /// Drive the SqliteLocal startup graph decision in one place: claim the build,
    /// then either reuse a fresh cached graph, build the graph + search chunks in one
    /// fused pass (when an embedder is available), or fall back to a normal lazy graph
    /// build. Returns whether the fused pass already populated the search index, so
    /// the caller knows whether it still needs the standalone indexer.
    pub(crate) fn start_workspace_graph(
        &self,
        engine: &mut SearchEngine,
        source_path: &Path,
    ) -> FusedStartup {
        let Some(workspace_root) = self.workspace_root.clone() else {
            return FusedStartup::Standalone;
        };
        if !self.try_begin_external_build() {
            // A concurrent path (e.g. a graph tool call) already owns the build; index
            // the search engine the normal way against whatever graph it produces.
            return FusedStartup::Standalone;
        }
        // Capture the mark-seq at build start so the publish consumes only marks this build
        // reflects. At boot this is `0` (unwired): the mark-seq source is wired after the engine
        // is published, so a fused/cached boot build clears nothing here. Marks a prior run left
        // pending are recovered explicitly after wiring (see `consume_leftover_marks`).
        let build_start_seq = self.current_mark_seq();
        match self.try_publish_cached(&workspace_root, build_start_seq) {
            PublishAttemptOutcome::Published => {
                // Warm start: the graph is reused from disk and the persisted search index
                // is reused by the standalone indexer's hash-skip (a near no-op).
                return FusedStartup::Standalone;
            }
            PublishAttemptOutcome::FallBack => {}
            PublishAttemptOutcome::Refused(failure) => {
                self.record_load_failure(false, failure);
                return FusedStartup::Standalone;
            }
        }
        // Cached but drifted: stale answers now beat a fused multi-minute rebuild. The
        // stale publish (Ready) supersedes this path's external claim (Loading), the
        // pre-claimed reload catches the graph up, and the search index still reuses
        // its persisted store through the standalone hash-skip.
        match self.try_publish_stale_and_catch_up(&workspace_root) {
            PublishAttemptOutcome::Published => return FusedStartup::Standalone,
            PublishAttemptOutcome::FallBack => {}
            PublishAttemptOutcome::Refused(failure) => {
                self.record_load_failure(false, failure);
                return FusedStartup::Standalone;
            }
        }
        if !engine.has_semantic() {
            // No embedder → no fused semantic pass; build the graph normally and let
            // the caller build the FTS-only index.
            self.abort_external_build();
            self.ensure_loading();
            return FusedStartup::Standalone;
        }
        match self.run_fused_cold_build(engine, source_path, build_start_seq) {
            Ok(()) => FusedStartup::Fused,
            Err(failure) => {
                tracing::warn!(
                    "fused cold-build failed; falling back to standalone index: {failure}"
                );
                self.record_load_failure(false, failure);
                FusedStartup::Standalone
            }
        }
    }
}

/// Lock a mutex, recovering the inner value if a prior holder panicked. The graph
/// mutexes guard brief stores/reads (and one throttled scan), so a poisoned guard
/// still carries valid data.
pub(super) fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{sample_workspace, wait_ready, wait_until, wait_until_within};
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A publish hook attached via `with_publish_hook` fires on the graph's background
    /// thread once the build completes and publishes — the seam the search context
    /// re-render hangs on. Without the `notify_published()` call at the publish site the
    /// counter stays zero and this fails.
    #[test]
    fn publish_hook_fires_after_a_build_publishes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let fired = Arc::new(AtomicUsize::new(0));
        let hook = {
            let fired = Arc::clone(&fired);
            Arc::new(move |_signal: GraphPublishSignal| {
                fired.fetch_add(1, Ordering::SeqCst);
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();

        // Waited on, not read once: `Ready` flips under `inner` and the hook runs after the
        // lock is released, so a bare read samples a value the build has not produced yet.
        wait_until(&graph, "the publish hook to fire", || fired.load(Ordering::SeqCst) >= 1);
    }

    /// A daemon whose workspace was taken over by a newer generation must not build the
    /// shared graph database: the owner is maintaining that same file, and a second builder
    /// only races its rename. It says so instead of looking like a build that never finishes —
    /// and it refuses the fused claim too, so the search boot does not hand it the graph.
    /// Remove the ownership gate in `run_load` and the superseded daemon rebuilds happily.
    #[test]
    fn a_superseded_daemon_builds_no_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        let graph = GraphState::for_workspace(root.to_path_buf()).with_lease(lease);
        // A newer daemon generation claims the same workspace.
        let _newer = crate::workspace_lease::WorkspaceLease::claim(root);
        wait_until_within(
            &graph,
            Duration::from_secs(10),
            "the lease verdict to stop allowing this graph to build",
            || !graph.may_build(),
        );

        assert!(!graph.try_begin_external_build(), "a superseded graph refuses the fused claim");

        graph.ensure_loading();
        assert!(matches!(graph.status(), GraphStatus::Idle), "no loader was started");
        assert!(
            !crate::cache::graph_db_path(root).exists(),
            "no graph database was written by the superseded daemon",
        );
        assert_eq!(
            graph.status_report().superseded,
            Some(true),
            "the status says why the graph is not rebuilding",
        );
    }

    #[test]
    fn superseded_status_truth_table() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        graph.ensure_loading();
        wait_ready(&graph);

        let held: Vec<_> = (0..super::super::snapshot::SNAPSHOT_POOL_CAP)
            .map(|_| graph.snapshot().expect("the owner preopens its own descriptors"))
            .collect();
        let own_revision = held[0].generation;
        let _newer = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(graph.is_superseded(), "the foreign token establishes the terminal verdict");

        for lifecycle in [
            GraphStatus::Idle,
            GraphStatus::Loading,
            GraphStatus::Failed("original failure".to_owned()),
            GraphStatus::Ready { files: 2 },
        ] {
            lock_recover(&graph.inner).status = lifecycle.clone();
            if lifecycle == GraphStatus::Idle {
                graph.ensure_loading();
                assert_eq!(graph.status(), GraphStatus::Idle, "terminal status starts no loader");
            }
            let report = graph.status_report();
            assert_eq!(report.state, "failed", "{lifecycle:?}");
            assert_eq!(report.superseded, Some(true), "{lifecycle:?}");
            assert_eq!(report.error.as_deref(), Some(SUPERSEDED_GRAPH_ERROR), "{lifecycle:?}");
        }
        assert!(lease.is_superseded());

        lock_recover(&graph.inner).status = GraphStatus::Ready { files: 2 };
        drop(held);
        let returned = graph.status_report();
        assert_eq!(returned.state, "ready");
        assert_eq!(returned.superseded, Some(true));
        assert_eq!(returned.revision, Some(own_revision));
        assert!(matches!(
            lock_recover(&graph.inner).published.as_ref().map(|p| &p.reload),
            Some(ReloadState::Idle)
        ));

        let transient_dir = tempfile::tempdir().unwrap();
        let transient_root = transient_dir.path();
        sample_workspace(transient_root);
        let transient_cache = crate::cache::WorkspaceCacheLayout::for_workspace(transient_root);
        let transient = GraphState::for_workspace_with_cache(
            transient_root.to_path_buf(),
            transient_cache.clone(),
        );
        transient.ensure_loading();
        wait_ready(&transient);
        drop(transient.snapshot().expect("park one descriptor for read-only status"));

        let holder = crate::workspace_lease::WorkspaceLease::hold_cache_lock_for(
            &transient_cache,
            Duration::from_secs(5),
        );
        let transient_lease = crate::workspace_lease::WorkspaceLease::claim_cache(&transient_cache);
        let transient = transient.with_lease(transient_lease.clone());
        let report = transient.status_report();
        assert_eq!(report.state, "ready");
        assert_eq!(report.superseded, None);
        assert!(!transient_lease.is_superseded());
        holder.join().unwrap();
    }

    /// A whole-collection re-render requested before the search engine existed to run it must
    /// still happen. On a fused cold boot nothing publishes a second time, so a request left
    /// pending would never be picked up and files the build skipped as byte-identical would
    /// keep contexts rendered under the old topology. Drop the flush call and the hook never
    /// receives `topology_changed`.
    #[test]
    fn a_topology_refresh_requested_before_the_engine_existed_is_flushed_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let refreshes = Arc::new(AtomicUsize::new(0));
        let hook = {
            let refreshes = Arc::clone(&refreshes);
            Arc::new(move |signal: GraphPublishSignal| {
                if signal.topology_changed {
                    refreshes.fetch_add(1, Ordering::SeqCst);
                }
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        let after_publish = refreshes.load(Ordering::SeqCst);

        // What the boot's topology mismatch leaves behind, with no publish left to carry it.
        graph.pending_topology_refresh.store(true, Ordering::SeqCst);
        graph.flush_pending_topology_refresh();

        assert_eq!(refreshes.load(Ordering::SeqCst), after_publish + 1, "the request is honoured");
        assert!(
            !graph.pending_topology_refresh.load(Ordering::SeqCst),
            "and cleared once the consumer handled it",
        );
    }

    /// Once a graph observes a live foreign owner it must never adopt, load, or reload the
    /// shared path again, even after that owner exits.
    #[test]
    fn superseded_graph_never_adopts_or_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        let graph = GraphState::for_workspace(root.to_path_buf()).with_lease(lease);
        let newer = crate::workspace_lease::WorkspaceLease::claim(root);
        wait_until_within(
            &graph,
            Duration::from_secs(10),
            "the lease verdict to stop allowing this graph to build",
            || !graph.may_build(),
        );

        let owner_graph = GraphState::for_workspace(root.to_path_buf()).with_lease(newer.clone());
        owner_graph.ensure_loading();
        wait_ready(&owner_graph);
        graph.ensure_loading();
        newer.release();

        assert!(matches!(
            graph.try_publish_cached(root, 0),
            PublishAttemptOutcome::Refused(super::super::build::LoadFailure {
                reason: super::super::build::LoadFailureReason::Superseded,
                ..
            })
        ));
        assert_eq!(graph.nudge_rebuild(), NudgeOutcome::NoOp);
        graph.spawn_reload();
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(graph.status(), GraphStatus::Idle));
        assert!(lock_recover(&graph.inner).published.is_none());
    }

    #[test]
    fn superseded_late_publish_is_fenced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let fired = Arc::new(AtomicUsize::new(0));
        let hook = {
            let fired = Arc::clone(&fired);
            Arc::new(move |_signal: GraphPublishSignal| {
                fired.fetch_add(1, Ordering::SeqCst);
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        let graph =
            GraphState::for_workspace(root.to_path_buf()).with_lease(lease).with_publish_hook(hook);
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        graph.pending_nudge.store(true, Ordering::SeqCst);
        graph.pending_topology_refresh.store(true, Ordering::SeqCst);
        graph.pending_roots_refresh.store(true, Ordering::SeqCst);
        graph.leftover_bound.store(41, Ordering::SeqCst);

        let _newer = crate::workspace_lease::WorkspaceLease::claim(root);
        graph.notify_published(41, true);
        graph.flush_pending_topology_refresh();
        graph.flush_pending_search_roots_refresh();
        graph.consume_leftover_marks(99);

        assert!(graph.is_superseded());
        assert_eq!(fired.load(Ordering::SeqCst), 0, "no late hook may apply its prepared plan");
        assert!(graph.pending_nudge.load(Ordering::SeqCst), "no late reload may start");
        assert!(graph.pending_topology_refresh.load(Ordering::SeqCst));
        assert!(graph.pending_roots_refresh.load(Ordering::SeqCst));
        assert_eq!(graph.leftover_bound.load(Ordering::SeqCst), 41);

        let refusal_dir = tempfile::tempdir().unwrap();
        let refusal_root = refusal_dir.path().to_path_buf();
        let refusal_lease = crate::workspace_lease::WorkspaceLease::claim(&refusal_root);
        let refusal_lease_in_hook = refusal_lease.clone();
        let newer = Arc::new(Mutex::new(None));
        let newer_in_hook = Arc::clone(&newer);
        let root_in_hook = refusal_root.clone();
        let refusal_hook = Arc::new(move |_signal: GraphPublishSignal| {
            *newer_in_hook.lock().unwrap() =
                Some(crate::workspace_lease::WorkspaceLease::claim(&root_in_hook));
            assert!(matches!(
                refusal_lease_in_hook
                    .publish_short(&mut (), |_| { Ok::<_, std::convert::Infallible>(()) }),
                crate::workspace_lease::LeaseOperationOutcome::Superseded
            ));
            GraphPublishOutcome { topology_handled: false, roots_handled: true }
        });
        let refusal_graph = GraphState::for_workspace(refusal_root)
            .with_lease(refusal_lease.clone())
            .with_publish_hook(refusal_hook);
        {
            let mut inner = lock_recover(&refusal_graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        refusal_graph.consume_leftover_marks(99);
        assert!(refusal_lease.is_superseded());
        assert_eq!(
            refusal_graph.leftover_bound.load(Ordering::SeqCst),
            99,
            "a refused consume keeps the captured obligation"
        );
        drop(newer);
    }

    /// A publish re-arms only the obligation its signal actually carried. The hook reports one
    /// outcome for the topology refresh whether or not one was requested, so a refusal reported
    /// for an unrequested refresh must not raise the flag: nothing asked for that work, and a
    /// flag raised here would make every later publish redo a whole-collection re-render.
    /// Dropping the `topology &&` guard makes the refusal below arm it.
    #[test]
    fn a_refusal_cannot_arm_a_topology_refresh_nobody_requested() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let lease = crate::workspace_lease::WorkspaceLease::claim(&root);
        let hook = Arc::new(|_signal: GraphPublishSignal| GraphPublishOutcome {
            topology_handled: false,
            roots_handled: true,
        })
            as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>;
        let graph = GraphState::for_workspace(root).with_lease(lease).with_publish_hook(hook);
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        graph.pending_topology_refresh.store(false, Ordering::SeqCst);

        graph.notify_published(7, false);

        assert!(
            !graph.pending_topology_refresh.load(Ordering::SeqCst),
            "a refusal reported for an unrequested refresh raises no obligation",
        );
        // The control: the SAME refusing hook DOES arm the flag when the refresh was requested,
        // so the assertion above is about the request and not about a flag nothing can set.
        graph.notify_published(7, true);
        assert!(
            graph.pending_topology_refresh.load(Ordering::SeqCst),
            "a refusal reported for a requested refresh keeps the obligation",
        );
    }

    /// The SqliteLocal boot builds the graph and the search chunks in ONE parse pass, and
    /// claims the graph for it through `try_begin_external_build` — which needs the
    /// `Idle → Loading` transition for itself. An eager start that lands first takes that
    /// transition and the claim fails, degrading the fused pass into two. This is why the
    /// boot's eager start is mode-gated and otherwise runs only after the claim.
    #[test]
    fn an_already_started_graph_refuses_the_fused_build_claim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();

        assert!(
            !graph.try_begin_external_build(),
            "a graph already building must refuse the fused claim, not build twice",
        );
        // Let the spawned build finish before the temp workspace goes away.
        wait_ready(&graph);
    }

    /// The mirror image: once the fused build owns the claim, the boot's catch-all start is
    /// inert — it must not spawn a second builder over the one already writing the database.
    /// A spawned loader would publish and fire the hook, so a hook that never fires (while the
    /// claim still reads `Loading`) is what rules a second build out; the status alone would
    /// not, since a second loader leaves it `Loading` too until it publishes.
    #[test]
    fn starting_a_claimed_graph_spawns_no_second_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let published = Arc::new(AtomicUsize::new(0));
        let hook = {
            let published = Arc::clone(&published);
            Arc::new(move |_signal: GraphPublishSignal| {
                published.fetch_add(1, Ordering::SeqCst);
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        assert!(graph.try_begin_external_build(), "an idle graph yields the claim");

        graph.ensure_loading();

        // Long enough for a loader spawned by that call to build this two-module workspace and
        // publish: `publish_hook_fires_after_a_build_publishes` waits for the same build.
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(published.load(Ordering::SeqCst), 0, "no second builder may publish");
        assert_eq!(
            graph.status(),
            GraphStatus::Loading,
            "the external build keeps the claim; nothing else may drive it",
        );
    }

    #[test]
    fn set_mark_seq_source_is_first_writer_wins() {
        let graph = GraphState::disabled();
        let first = Arc::new(AtomicI64::new(7));
        let second = Arc::new(AtomicI64::new(11));

        graph.set_mark_seq_source(Arc::clone(&first));
        graph.set_mark_seq_source(Arc::clone(&second));

        assert_eq!(graph.current_mark_seq(), 7, "the first mark sequence source is retained");
        first.store(13, Ordering::SeqCst);
        assert_eq!(graph.current_mark_seq(), 13, "reads continue using the first source");
    }

    /// A topology refresh the hook cannot run (deferred, engine absent) must be
    /// re-raised on the NEXT publish — otherwise a dependsOn edit landing while
    /// the search engine boots would leave every persisted context stale forever.
    #[test]
    fn an_unhandled_topology_refresh_is_re_raised_on_the_next_publish() {
        use std::sync::atomic::AtomicUsize;

        let seen = Arc::new(AtomicUsize::new(0));
        let handled = Arc::new(AtomicI64::new(0));
        let hook = {
            let seen = Arc::clone(&seen);
            let handled = Arc::clone(&handled);
            Arc::new(move |signal: GraphPublishSignal| {
                let topology_handled = if signal.topology_changed {
                    seen.fetch_add(1, Ordering::SeqCst);
                    // First sighting: report unhandled; second: handled.
                    handled.fetch_add(1, Ordering::SeqCst) > 0
                } else {
                    true
                };
                GraphPublishOutcome { topology_handled, roots_handled: true }
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::disabled().with_publish_hook(hook);

        graph.notify_published(0, true);
        assert_eq!(seen.load(Ordering::SeqCst), 1, "the request reaches the hook");
        graph.notify_published(0, false);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "an unhandled topology refresh is re-raised even though this publish did not change it",
        );
        graph.notify_published(0, false);
        assert_eq!(seen.load(Ordering::SeqCst), 2, "a handled request is not re-raised");
    }

    /// A drift delivered while a build is in flight (`nudge_rebuild` during `Loading`, or while
    /// a reload runs) is recorded, not dropped: the build's publish re-checks and — seeing disk
    /// moved past what the build captured — claims a follow-up reload whose own publish fires
    /// the hook again. Reverting the `pending_nudge` re-claim in `notify_published` leaves the
    /// hook firing only once and this fails.
    #[test]
    fn a_nudge_recorded_during_a_build_reloads_on_publish() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let fired = Arc::new(AtomicUsize::new(0));
        let hook = {
            let fired = Arc::clone(&fired);
            Arc::new(move |_signal: GraphPublishSignal| {
                fired.fetch_add(1, Ordering::SeqCst);
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        // Simulate an initial build that already published (generation 1) with a fingerprint
        // that does NOT match disk, plus a nudge that arrived while that build was in flight.
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        graph.pending_nudge.store(true, Ordering::SeqCst);

        // The publish chain fires the hook once and, seeing the recorded nudge with disk
        // drifted past the faked build, claims a follow-up reload. Pass an explicit unbounded
        // bound (i64::MAX) so the seq bound never gates this test — only the reclaim behavior
        // under test decides how many times the hook fires.
        graph.notify_published(i64::MAX, false);

        wait_until(
            &graph,
            "the recorded nudge to trigger a follow-up reload whose publish fires the hook again",
            || fired.load(Ordering::SeqCst) >= 2,
        );
    }

    /// `drift_pending` reports a drift the context re-render must wait for: a recorded nudge or
    /// a running reload. A clean published graph reports none.
    #[test]
    fn drift_pending_reflects_recorded_nudge_and_running_reload() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("A.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp { files: 1, topology: 1 },
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        assert!(!graph.drift_pending(), "a clean published graph has no pending drift");

        graph.pending_nudge.store(true, Ordering::SeqCst);
        assert!(graph.drift_pending(), "a recorded nudge is a pending drift");
        graph.pending_nudge.store(false, Ordering::SeqCst);

        lock_recover(&graph.inner).published.as_mut().unwrap().reload = ReloadState::Running;
        assert!(graph.drift_pending(), "a running reload is a pending drift");
    }

    /// Even after the stale boot's pre-claimed catch-up FAILS (`reload=Failed`, so
    /// `drift_pending` no longer holds), the leftover-marks one-shot must stay armed:
    /// the stale snapshot predates the marks' causes, and firing the hook against it
    /// would clear them for good.
    #[test]
    fn leftover_consume_stays_armed_while_published_snapshot_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let fired = Arc::new(AtomicUsize::new(0));
        let hook_fired = Arc::clone(&fired);
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(Arc::new(
            move |_signal| {
                hook_fired.fetch_add(1, Ordering::SeqCst);
                GraphPublishOutcome::HANDLED
            },
        ));
        {
            let mut inner = lock_recover(&graph.inner);
            inner.published = Some(Published {
                generation: 7,
                fingerprint: crate::graph_db::GraphFp { files: 1, topology: 1 },
                stale: true,
                reload: ReloadState::Failed("catch-up failed".to_owned()),
                force_stale: false,
                search_roots: None,
            });
            inner.status = GraphStatus::Ready { files: 1 };
        }

        graph.consume_leftover_marks(5);
        assert_eq!(fired.load(Ordering::SeqCst), 0, "no consume against the stale snapshot");
        assert_eq!(
            graph.leftover_bound.load(Ordering::SeqCst),
            5,
            "the one-shot stays armed for the next successful publish"
        );
    }

    /// The single-flight core of the drift nudge: the FIRST claim on a drifted published
    /// graph wins and marks the reload `Running`; a SECOND claim (a storm of xml events
    /// while the build runs) loses, so no extra rebuild is ever queued. Deterministic — no
    /// build thread is spawned, only the claim discipline is exercised.
    #[test]
    fn claim_reload_slot_is_single_flight() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("A.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            // fingerprint 0 can never match the real disk scan → a drift is always seen.
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        assert!(graph.claim_reload_slot(), "the first claim wins on drift");
        assert!(!graph.claim_reload_slot(), "a second claim loses while a reload is Running");
    }

    /// A nudge on an unbuilt (`Idle`) graph starts the one initial load without any `graph`
    /// tool call — the search-only user's path. Asserting the outcome and that the status
    /// left `Idle` (a load never returns to `Idle`; it goes `Loading → Ready`/`Failed`).
    #[test]
    fn nudge_rebuild_from_idle_starts_the_initial_load() {
        let dir = tempfile::tempdir().unwrap();
        sample_workspace(dir.path());
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        assert_eq!(graph.status(), GraphStatus::Idle);

        assert_eq!(graph.nudge_rebuild(), NudgeOutcome::LoadStarted);
        assert_ne!(graph.status(), GraphStatus::Idle, "the nudge scheduled the initial load");
    }

    /// A nudge arriving while a reload is already `Running` schedules nothing (single-flight),
    /// so a storm of xml drift during a build cannot pile up rebuilds. No thread is spawned.
    #[test]
    fn nudge_rebuild_absorbs_a_storm_while_a_reload_runs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("A.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Running,
                force_stale: false,
                search_roots: None,
            });
        }
        assert_eq!(graph.nudge_rebuild(), NudgeOutcome::NoOp);
        assert_eq!(graph.nudge_rebuild(), NudgeOutcome::NoOp);
    }

    #[test]
    fn a_force_request_arriving_during_a_build_survives_the_older_publication() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Running,
                force_stale: false,
                search_roots: None,
            });
        }

        assert_eq!(graph.nudge_project_reload(), NudgeOutcome::NoOp);
        let first_epoch = graph.project_reload_epoch();
        assert_eq!(graph.nudge_project_reload(), NudgeOutcome::NoOp);
        let second_epoch = graph.project_reload_epoch();
        assert!(second_epoch > first_epoch);

        // The running build captured only the first request. Its full publication may complete
        // that request, but not the newer one that arrived after its capture.
        graph.complete_project_reload_through(first_epoch);
        assert!(graph.project_reload_pending());

        graph.pending_nudge.store(false, Ordering::SeqCst);
        lock_recover(&graph.inner).published.as_mut().unwrap().reload = ReloadState::Idle;
        assert!(graph.claim_reload_slot(), "the newer force request claims the follow-up reload");

        graph.complete_project_reload_through(second_epoch);
        assert!(!graph.project_reload_pending());
    }

    #[test]
    fn project_config_detection_is_exactly_workspace_root_level() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        for name in project_model::PROJECT_INPUT_FILE_NAMES {
            assert!(graph.is_workspace_config_path(&dir.path().join(name)));
            assert!(!graph.is_workspace_config_path(&dir.path().join("nested").join(name)));
        }
        let sibling = dir.path().with_extension("other").join("bsl-analyzer.toml");
        assert!(!graph.is_workspace_config_path(&sibling));
    }

    #[test]
    fn topology_and_root_retry_obligations_are_independent() {
        let outcome = Arc::new(Mutex::new(GraphPublishOutcome {
            topology_handled: false,
            roots_handled: true,
        }));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hook = {
            let outcome = Arc::clone(&outcome);
            let seen = Arc::clone(&seen);
            Arc::new(move |signal: GraphPublishSignal| {
                seen.lock()
                    .unwrap()
                    .push((signal.topology_changed, signal.roots_refresh_requested));
                *outcome.lock().unwrap()
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::disabled().with_publish_hook(hook);
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        graph.notify_published(0, true);
        assert!(graph.pending_topology_refresh.load(Ordering::SeqCst));
        assert!(!graph.pending_roots_refresh.load(Ordering::SeqCst));

        *outcome.lock().unwrap() =
            GraphPublishOutcome { topology_handled: true, roots_handled: false };
        graph.flush_pending_topology_refresh();
        assert!(!graph.pending_topology_refresh.load(Ordering::SeqCst));
        graph.notify_published(0, false);
        assert!(graph.pending_roots_refresh.load(Ordering::SeqCst));
        assert!(!graph.pending_topology_refresh.load(Ordering::SeqCst));

        *outcome.lock().unwrap() = GraphPublishOutcome::HANDLED;
        graph.flush_pending_search_roots_refresh();
        assert!(!graph.pending_roots_refresh.load(Ordering::SeqCst));
        assert!(
            seen.lock().unwrap().contains(&(false, true)),
            "root retry must not claim a topology change"
        );
    }

    /// Two symlinks onto ONE configuration directory, so switching the declared
    /// `[source] root` between them changes the resolved search root while leaving
    /// every canonical graph input — and therefore the workspace fingerprint —
    /// byte-identical. Drift detection cannot see this change; only a forced reload can.
    #[cfg(unix)]
    fn forced_reload_workspace(root: &Path) {
        use std::os::unix::fs::symlink;

        let configuration = root.join("cf");
        fs::create_dir_all(&configuration).unwrap();
        fs::write(configuration.join("Configuration.xml"), "<Configuration/>").unwrap();
        sample_workspace(&configuration);
        symlink(&configuration, root.join("alias-a")).unwrap();
        symlink(&configuration, root.join("alias-b")).unwrap();
        declare_source_root(root, "alias-a");
    }

    #[cfg(unix)]
    fn declare_source_root(root: &Path, alias: &str) {
        fs::write(root.join("bsl-analyzer.toml"), format!("[source]\nroot = \"{alias}\"\n"))
            .unwrap();
    }

    #[cfg(unix)]
    fn published_generation(graph: &GraphState) -> u64 {
        lock_recover(&graph.inner).published.as_ref().expect("a ready graph published").generation
    }

    /// The wait a forced-reload test performs. The publication under test is complete
    /// only when a newer generation is published AND the force obligation that
    /// triggered it is discharged: a predicate naming just the generation names a
    /// PRECURSOR of the checked quantity, so a test waiting on it samples the graph
    /// before the value it asserts on exists.
    #[cfg(unix)]
    fn forced_reload_published(graph: &GraphState, since_generation: u64) -> bool {
        let inner = lock_recover(&graph.inner);
        inner.published.as_ref().is_some_and(|published| {
            published.generation > since_generation
                && published.reload == ReloadState::Idle
                && !graph.project_reload_pending()
        })
    }

    /// Only a publication that actually installed discharges the force obligation. A
    /// refused install must leave it outstanding, so the forced reload is retried
    /// rather than dropped: discharging on the attempt would lose the caller's request
    /// silently, and the workspace would keep serving the configuration it was told to
    /// stop serving.
    ///
    /// The refusal half is a regression guard — today's code returns before the
    /// discharge — so the successful half runs in the same test as its positive
    /// control: without it the guard would pass against a build that discharges nothing
    /// at all.
    #[cfg(unix)]
    #[test]
    fn only_an_installed_publication_discharges_the_force_obligation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        forced_reload_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        declare_source_root(root, "alias-b");
        graph.project_reload_epoch.fetch_add(1, Ordering::SeqCst);
        super::super::snapshot::refuse_snapshot_install_for_test();
        graph.run_load(true);
        assert!(
            graph.project_reload_pending(),
            "a refused install must leave the forced reload outstanding: epoch {} completed {}",
            graph.project_reload_epoch.load(Ordering::SeqCst),
            graph.completed_project_reload_epoch.load(Ordering::SeqCst)
        );

        graph.run_load(true);
        assert!(
            !graph.project_reload_pending(),
            "the retry installed and must discharge it: epoch {} completed {}",
            graph.project_reload_epoch.load(Ordering::SeqCst),
            graph.completed_project_reload_epoch.load(Ordering::SeqCst)
        );
    }

    /// A wait that exhausts its ceiling must hand the reader the state it actually
    /// observed. The flake this hardening came from reported a bare left/right and
    /// nothing else, which is why it could not be diagnosed from the CI log at all.
    ///
    /// This gates the shared helper every in-class wait routes through. It does NOT
    /// gate that a newly written wait uses the helper: that is what the census in the
    /// change's own procedure is for, and a text scan over Rust source is not a gate
    /// this repository trusts.
    #[test]
    fn a_wait_that_times_out_reports_the_state_it_observed() {
        let graph = GraphState::disabled();
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 4 };
            inner.published = Some(Published {
                generation: 7,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: true,
                search_roots: None,
            });
        }
        graph.project_reload_epoch.store(3, Ordering::SeqCst);
        graph.completed_project_reload_epoch.store(1, Ordering::SeqCst);

        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_until_within(
                &graph,
                Duration::from_millis(0),
                "a condition that never holds",
                || false,
            );
        }))
        .expect_err("a wait whose condition never holds must fail");
        let reported = failure
            .downcast_ref::<String>()
            .expect("the wait fails with a formatted message")
            .clone();

        for named in [
            "a condition that never holds",
            "Ready",
            "generation 7",
            "reload none",
            "force_stale true",
            "project_reload_epoch 3",
            "completed 1",
        ] {
            assert!(
                reported.contains(named),
                "a timed-out wait must name {named:?}; it reported {reported:?}"
            );
        }
    }

    /// Wait for the forced reload to publish AND discharge its obligation. Waiting on
    /// the generation alone returns on a precursor, so every assertion after it races
    /// the value it reads.
    #[cfg(unix)]
    fn wait_for_forced_reload(graph: &GraphState, since_generation: u64) {
        super::super::test_support::wait_until(
            graph,
            "the forced reload to publish and discharge its force obligation",
            || forced_reload_published(graph, since_generation),
        );
    }

    /// A rendezvous with the building thread parked in the window between a publication
    /// and the post-publication work that follows it — discharging the force obligation,
    /// re-arming the change hub, and the publish pass itself.
    ///
    /// The park happens with `inner` released, which is the whole point: a park taken
    /// under the lock would block the observer on the same mutex and so read identical
    /// against a coherent publication and against a torn one.
    ///
    /// It parks BEFORE `notify_published`, so it cannot expose anything the pass does
    /// internally — the leftover-marks tail included. A test needing that window parks in
    /// the publish hook instead, which the tail calls between its `swap` and `fetch_max`.
    struct PublishWindow {
        armed: Arc<AtomicBool>,
        entered: std::sync::mpsc::Receiver<()>,
        release_tx: std::sync::mpsc::Sender<()>,
        hook: Arc<dyn Fn() + Send + Sync>,
    }

    impl PublishWindow {
        fn new() -> Self {
            let armed = Arc::new(AtomicBool::new(false));
            let (entered_tx, entered) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let release_rx = Arc::new(Mutex::new(release_rx));
            let hook = {
                let armed = Arc::clone(&armed);
                Arc::new(move || {
                    if armed.swap(false, Ordering::SeqCst) {
                        entered_tx.send(()).expect("the test outlives the parked build");
                        lock_recover(&release_rx)
                            .recv_timeout(Duration::from_secs(30))
                            .expect("the test released the parked build");
                    }
                }) as Arc<dyn Fn() + Send + Sync>
            };
            Self { armed, entered, release_tx, hook }
        }

        /// Arm the next publication to reach the window — and only that one, since the
        /// hook disarms itself as it parks.
        ///
        /// Which publication that is belongs to the caller: arming after a workspace is
        /// already `Ready` parks a reload and leaves the initial load unparked, while
        /// arming before `ensure_loading` parks the initial load itself. Both are used.
        fn arm(&self) {
            self.armed.store(true, Ordering::SeqCst);
        }

        /// Fails instead of hanging when no publication reaches the window.
        fn wait_entered(&self) {
            self.entered
                .recv_timeout(Duration::from_secs(30))
                .expect("a publication reached the publish window");
        }

        fn release(&self) {
            let _ = self.release_tx.send(());
        }
    }

    /// Bring a workspace to Ready with the forced-reload fixture, then declare the
    /// other alias and arm the window, leaving the caller holding a parked build.
    #[cfg(unix)]
    fn park_a_forced_reload(root: &Path, window: &PublishWindow) -> (GraphState, u64) {
        forced_reload_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_publish_window_hook(Arc::clone(&window.hook));
        graph.ensure_loading();
        wait_ready(&graph);
        let generation = published_generation(&graph);

        declare_source_root(root, "alias-b");
        assert_eq!(
            super::super::scan::workspace_fingerprint(root),
            lock_recover(&graph.inner).published.as_ref().unwrap().fingerprint,
            "the fixture must change only the declared alias, never a canonical input"
        );
        window.arm();
        assert_eq!(
            graph.nudge_project_reload(),
            NudgeOutcome::ReloadClaimed,
            "a declared-root change claims a forced reload"
        );
        window.wait_entered();
        (graph, generation)
    }

    /// An outside observer must never catch a published generation whose force
    /// obligation is still outstanding: discharging it after the publication leaves a
    /// window in which the graph reads "reloaded, and still owing a reload".
    #[cfg(unix)]
    #[test]
    fn a_publication_and_its_force_obligation_are_one_state_to_an_outside_observer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let window = PublishWindow::new();
        let (graph, generation) = park_a_forced_reload(root, &window);

        let torn = {
            let inner = lock_recover(&graph.inner);
            let published = inner.published.as_ref().expect("the parked build published");
            (published.generation > generation && published.reload == ReloadState::Idle)
                .then(|| (published.generation, graph.project_reload_pending()))
        };
        window.release();

        assert_eq!(
            torn.map(|(_, pending)| pending),
            Some(false),
            "the parked build published generation {:?}, but its force obligation was still \
             outstanding: epoch {} completed {}",
            torn.map(|(generation, _)| generation),
            graph.project_reload_epoch.load(Ordering::SeqCst),
            graph.completed_project_reload_epoch.load(Ordering::SeqCst),
        );
    }

    /// The same window is reachable by `claim_reload_slot`, which reads the force
    /// obligation under `inner`: catching the publication before the obligation is
    /// discharged makes it claim a SECOND full rebuild of what was just published.
    /// On a large configuration that is minutes of work for no change.
    #[cfg(unix)]
    #[test]
    fn a_successful_forced_reload_does_not_claim_a_second_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let window = PublishWindow::new();
        let (graph, generation) = park_a_forced_reload(root, &window);

        let in_window = graph.nudge_rebuild();
        window.release();

        wait_for_forced_reload(&graph, generation);
        // A claim spawns its rebuild off-thread; let it publish before counting.
        if in_window == NudgeOutcome::ReloadClaimed {
            std::thread::sleep(Duration::from_millis(200));
        }

        assert_eq!(
            in_window,
            NudgeOutcome::NoOp,
            "nothing drifted and the forced reload had published, yet a rebuild was claimed"
        );
        assert_eq!(
            published_generation(&graph),
            generation + 1,
            "the forced reload published exactly once"
        );
    }

    /// The wait predicate must name the quantity the test asserts on. Naming only the
    /// generation makes the wait return on a precursor, and every assertion after it
    /// races the value it reads.
    #[cfg(unix)]
    #[test]
    fn the_forced_reload_wait_names_the_epoch_and_not_just_the_generation() {
        let graph = GraphState::disabled();
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 2,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        graph.project_reload_epoch.store(1, Ordering::SeqCst);

        assert!(
            !forced_reload_published(&graph, 1),
            "a newer generation whose force obligation is still outstanding is not the \
             publication this wait is for"
        );

        graph.completed_project_reload_epoch.store(1, Ordering::SeqCst);
        assert!(
            forced_reload_published(&graph, 1),
            "a newer generation with the obligation discharged IS that publication"
        );
    }

    #[cfg(unix)]
    #[test]
    fn forced_project_reload_bypasses_an_equal_graph_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        forced_reload_workspace(root);

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let (generation, before_fp) = {
            let inner = lock_recover(&graph.inner);
            let published = inner.published.as_ref().unwrap();
            (published.generation, published.fingerprint)
        };

        declare_source_root(root, "alias-b");
        assert_eq!(
            super::super::scan::workspace_fingerprint(root),
            before_fp,
            "declared alias changed but canonical graph inputs did not"
        );
        assert_eq!(
            graph.nudge_rebuild(),
            NudgeOutcome::NoOp,
            "an equal fingerprint gives drift detection nothing to claim on: force_stale {}",
            lock_recover(&graph.inner).published.as_ref().unwrap().force_stale
        );
        assert_eq!(
            graph.nudge_project_reload(),
            NudgeOutcome::ReloadClaimed,
            "a declared-root change claims a reload the fingerprint cannot: epoch {}",
            graph.project_reload_epoch.load(Ordering::SeqCst)
        );

        wait_for_forced_reload(&graph, generation);

        let inner = lock_recover(&graph.inner);
        let published = inner.published.as_ref().expect("the forced reload published");
        let roots = published
            .search_roots
            .as_ref()
            .expect("a full publication carries the roots it resolved");
        assert!(
            roots.configuration().is_some_and(|path| path.ends_with("alias-b")),
            "the reload resolved the newly declared alias, got {:?}",
            roots.configuration()
        );
        assert_eq!(
            graph.completed_project_reload_epoch.load(Ordering::SeqCst),
            graph.project_reload_epoch.load(Ordering::SeqCst),
            "only the successful full publication clears the force obligation"
        );
    }

    /// `wait_ready` returns on a PRECURSOR of anything the publish pass leaves behind, and
    /// this pins that down without relying on load to widen the gap: the window hook parks
    /// the building thread between the status flip and the pass, so `Ready` is observable
    /// while the hook provably has not run. A test reading its counter once here — the shape
    /// `publish_hook_fires_after_a_build_publishes` used to have — reads zero every time.
    #[test]
    fn ready_is_observable_before_the_publish_hook_runs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let fired = Arc::new(AtomicUsize::new(0));
        let hook = {
            let fired = Arc::clone(&fired);
            Arc::new(move |_signal: GraphPublishSignal| {
                fired.fetch_add(1, Ordering::SeqCst);
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let window = PublishWindow::new();
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_publish_hook(hook)
            .with_publish_window_hook(Arc::clone(&window.hook));
        window.arm();
        graph.ensure_loading();
        window.wait_entered();

        wait_ready(&graph);
        let parked = fired.load(Ordering::SeqCst);
        window.release();

        assert_eq!(parked, 0, "the status reached Ready while the publish hook had not run");
        wait_until(&graph, "the publish hook to fire", || fired.load(Ordering::SeqCst) >= 1);
    }

    /// The tail of a publish pass takes the leftover bound with a `swap` and only restores it
    /// after its hook reports the consume unhandled. In between, the obligation READS
    /// discharged though nothing consumed it — a state no real consumer can act on, because
    /// no real consumer runs inside the pass.
    ///
    /// So a test asserting the obligation must wait for the pass, not for the hook and not
    /// for `Ready`: both land inside that remainder. The park here is the hook itself, which
    /// the tail calls between its `swap` and its `fetch_max` — the one window
    /// `PublishWindow` cannot reach, since it fires before the pass begins.
    #[test]
    fn a_leftover_obligation_reads_discharged_inside_the_pass_that_re_arms_it() {
        const LEFTOVER_BOUND: i64 = 41;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let hook = Arc::new(move |signal: GraphPublishSignal| {
            // Only the leftover consume carries the stored bound; the build's own fire runs
            // ahead of the tail and must pass straight through.
            if signal.build_start_seq == LEFTOVER_BOUND {
                entered_tx.send(()).expect("the test outlives the parked pass");
                lock_recover(&release_rx)
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the test released the parked pass");
            }
            // Unhandled: the consume could not run, so the tail must re-arm the obligation.
            GraphPublishOutcome { topology_handled: false, roots_handled: false }
        })
            as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>;

        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        graph.leftover_bound.store(LEFTOVER_BOUND, Ordering::SeqCst);

        let publisher = {
            let graph = graph.clone();
            std::thread::spawn(move || graph.notify_published(7, false))
        };
        entered
            .recv_timeout(Duration::from_secs(30))
            .expect("the pass reached its leftover consume");

        assert!(
            matches!(graph.status(), GraphStatus::Ready { .. }),
            "the graph is Ready throughout, so `wait_ready` returns here",
        );
        assert!(
            !graph.leftover_consume_pending(),
            "inside the pass the obligation reads discharged, which is what a test waiting \
             on `Ready` or on the hook samples",
        );
        assert_eq!(
            graph.publish_passes.load(Ordering::SeqCst),
            0,
            "and the pass counter has not moved, so a wait on it does not return here",
        );

        release_tx.send(()).expect("the parked pass is still running");
        publisher.join().expect("the publish pass completed");

        super::super::test_support::wait_publish_pass(&graph, 1);
        assert!(
            graph.leftover_consume_pending(),
            "once the pass finished, the unhandled consume left the obligation armed",
        );
    }
}
