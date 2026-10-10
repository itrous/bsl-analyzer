//! Graph lifecycle state and publication protocol.

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU8;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bsl_search::SearchEngine;

use crate::change_hub::{SinkCursor, WorkspaceChangeHub};

use super::debt::{BuildKind, BuildStart, Facts, FailureKind, GraphDebt, HookDebt};

use super::build::PublishAttemptOutcome;
use super::snapshot::{FpMapState, GraphStore, ScanCache};
use super::types::{
    Freshness, FusedStartup, GraphPublishOutcome, GraphPublishSignal, GraphStatus,
    GraphStatusReport, SUPERSEDED_GRAPH_ERROR,
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
#[cfg_attr(test, derive(Clone))]
pub(super) struct Published {
    /// Whether this snapshot was published KNOWING it does not reflect current disk
    /// (the boot stale-publish). Any successful build/reload publish replaces it with
    /// a fresh entry. No mark is ever consumed against it: even if the pre-claimed
    /// catch-up fails (`reload` drops to `Failed`, so `drift_pending` no longer
    /// holds), this snapshot predates the marks' causes.
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
    /// The highest hub position known to be reflected by this publication. A successful
    /// build initializes it from the position read before its disk scan; a complete clean
    /// comparison against the same fingerprint may advance it under `publication_gate`.
    /// `None` for a publication that scanned nothing — the boot's stale cache — which
    /// therefore consumes no marks.
    pub(super) observed_through: Option<u64>,
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
    pub(super) indexing_unread_files: Option<usize>,
    pub(super) build_ticker: Option<Arc<ide::GraphBuildTicker>>,
    /// The ticket of the build holding the slot. Written with the grant and cleared with the
    /// outcome, so a slot that reads as taken always names what it was taken for.
    pub(super) claimed: Option<super::debt::BuildTicket>,
    /// The ticket the running builder is CARRYING, from the moment it takes the grant to the
    /// moment its work ends.
    ///
    /// Taking the grant is what starts the build, and an outcome reported afterwards found
    /// nothing left to read: it invented a primary-only sponsorship and closed an account that
    /// had not paid for anything, while the lane that did pay went on believing its build was
    /// still to come.
    pub(super) building: Option<super::debt::BuildTicket>,
    /// Which carry `building` belongs to: the guard that set it, or `0` while the ticket has
    /// been taken and no guard holds it yet. A hand-over puts the same admission in a second
    /// builder's hands before the first one has returned, so "clear the slot" at the first
    /// one's exit would end the second one's mandate.
    pub(super) carrier: u64,
    /// The last carry identity handed out.
    pub(super) carries: u64,
}

/// The builder's hold on its own mandate. While one of these is alive the ticket it carries is
/// what every outcome of that build reports against.
pub(super) struct CarriedTicket<'a> {
    graph: &'a GraphState,
    carrier: u64,
}

impl Drop for CarriedTicket<'_> {
    fn drop(&mut self) {
        let mut inner = lock_recover(&self.graph.inner);
        // Only what THIS carry still holds. A successor that has taken the slot since is
        // carrying a mandate of its own, however alike the tickets look.
        if inner.carrier == self.carrier {
            inner.building = None;
            inner.carrier = 0;
        }
    }
}

/// The barrier a test parks the building thread on, between the accepted claim and the
/// pre-scan that follows it.
#[cfg(test)]
pub(super) type PostClaimHook = Arc<dyn Fn(&GraphState) + Send + Sync>;

#[cfg(test)]
pub(crate) type BuildCandidateHook = Arc<dyn Fn(&GraphState) + Send + Sync>;

/// The barrier a test runs on the watcher's own thread, between taking the continuation latch
/// and sampling the alarm counter.
#[cfg(test)]
pub(super) type LatchWindowHook = Arc<dyn Fn(&GraphState) + Send + Sync>;

/// The comparison's own cursor into the hub, and its lifetime.
///
/// A cursor is not a value that can be put back wherever it came from. It is subscribed with
/// the hub, and once the observation ends it is unsubscribed — after which the id names
/// nothing. The drain that reads it runs with this lock RELEASED, because it can take a while,
/// and that window is where the three races lived: a copied cursor written back after a
/// release resurrected a dead id; a release that emptied the slot let a concurrent comparison
/// subscribe one nobody would ever clean up; two comparisons overlapping a release left the
/// slot holding whichever finished last.
///
/// So the slot is a lifecycle, not an `Option`. `Closed` is terminal: nothing subscribes
/// again, and a drain that comes back to a closed slot unsubscribes what it was holding
/// instead of storing it. `epoch` distinguishes a cursor this slot still owns from one that
/// has been replaced under it.
#[derive(Default)]
pub(super) struct ScanCursorSlot {
    cursor: Option<SinkCursor>,
    epoch: u64,
    closed: bool,
}

impl ScanCursorSlot {
    /// The cursor to drain, subscribing one if this slot is open and has none, plus the epoch
    /// the caller must present to write back.
    pub(super) fn open(&mut self, hub: &WorkspaceChangeHub) -> Option<(SinkCursor, u64)> {
        if self.closed {
            return None;
        }
        let cursor = match self.cursor {
            Some(cursor) => cursor,
            None => {
                let cursor = hub.subscribe();
                self.cursor = Some(cursor);
                self.epoch += 1;
                cursor
            }
        };
        Some((cursor, self.epoch))
    }

    /// Store the position a drain reached. Refused — and the cursor unsubscribed by the caller
    /// — when the slot was closed or moved on while that drain ran.
    #[must_use]
    pub(super) fn advance(&mut self, epoch: u64, cursor: SinkCursor) -> bool {
        if self.closed || self.epoch != epoch {
            return false;
        }
        self.cursor = Some(cursor);
        true
    }

    /// End this slot's life and hand back what it holds. Terminal.
    pub(super) fn close(&mut self) -> Option<SinkCursor> {
        self.closed = true;
        self.cursor.take()
    }

    pub(super) fn peek(&self) -> Option<SinkCursor> {
        self.cursor
    }

    #[cfg(test)]
    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }

    /// What the slot is holding, for a test asserting that a closed one holds nothing: an id
    /// the hub has forgotten, sitting where the next reader looks, is the resurrection this
    /// lifecycle exists to forbid.
    #[cfg(test)]
    pub(super) fn holds(&self) -> Option<SinkCursor> {
        self.cursor
    }
}

/// What a start did, for the one caller that must know whether anything actually began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartOutcome {
    /// A build began, or the slot was refused and its own outcome will decide next.
    Settled,
    /// No build: the comparison found disk already matching and answered the debts instead,
    /// which can leave whatever queued behind them ripe this very instant.
    AnsweredWithoutBuilding,
}

/// Who is driving, and therefore what they may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Executor {
    /// The boot, a request, or a build reporting its own outcome: everything owed.
    Any,
    /// The drift watcher: everything owed except the first build of an idle graph.
    NotTheFirstBuild,
}

/// Why a reload was, or was not, claimed.
enum ReloadClaim {
    Claimed,
    /// Disk matches the published build and no project reload is outstanding.
    NotOwed {
        comparison: Option<ComparisonProof>,
    },
    /// A difference WAS measured, and every account that could have paid for the attempt has
    /// run out. Nothing is answered by this: the debts stand, their explanation stands, and
    /// only fresh work opens a budget that can act on them.
    Unsponsored,
    /// Owed, but not startable now: the backoff holds, or the lease could not be confirmed.
    Held,
    /// A reload is running; its own outcome settles what is owed.
    Running,
    /// The daemon has asked its owners to leave. Not a refusal and not a failure: nothing is
    /// dropped, nothing is retried, and no debt is recorded — this generation is going.
    Stopping,
}

/// A clean disk comparison of the exact published generation and fingerprint. The proof is
/// revalidated under `publication_gate` before it advances that publication's fact frontier.
#[derive(Clone, Copy)]
struct ComparisonProof {
    generation: u64,
    fingerprint: crate::graph_db::GraphFp,
    through: u64,
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
    pub(super) hub_cursor: Arc<Mutex<ScanCursorSlot>>,
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
    /// The owner of every read handle onto the CURRENT published graph file. Opening the
    /// multi-GB SQLite file costs ~a second on a large configuration; a pooled handle keeps
    /// serving the same coherent snapshot for free, and a handle of a superseded generation
    /// is discarded instead of served.
    pub(super) store: GraphStore,
    #[cfg(test)]
    pub(super) background_snapshot_failure: Arc<AtomicU8>,
    /// Parks the building thread between the ACCEPTED CLAIM and the pre-scan that follows it,
    /// so a test can deliver a fact into exactly that window and no other. It is the window a
    /// live re-read of the hub position would silently absorb: the build was admitted for one
    /// set of facts and would publish a proof covering a later one.
    ///
    /// A barrier, not a sleep: it runs once, on the building thread, at one named point.
    #[cfg(test)]
    pub(super) post_claim_hook: Option<PostClaimHook>,
    #[cfg(test)]
    pub(super) build_candidate_hook: Option<BuildCandidateHook>,
    /// Runs on the thread handing a stale publication's catch-up over, after the successor has
    /// been spawned and before the hand-over returns — the window in which two builds hold one
    /// mandate between them.
    #[cfg(test)]
    pub(super) handover_hook: Option<PostClaimHook>,
    /// Runs on the thread reporting a failed build, after the lifecycle already reads the build
    /// as over and before the failure is recorded — the window another claim can land in.
    #[cfg(test)]
    pub(super) outcome_window_hook: Option<PostClaimHook>,
    /// Runs on a watcherless kick's thread after it has taken the ask and before it lets go of
    /// the kick — the window a request's ask can land in with nobody left to carry it.
    #[cfg(test)]
    pub(super) kick_release_hook: Option<PostClaimHook>,
    /// Parks the building thread between a publication and the point where the
    /// force-reload obligation is discharged, so a test can sample what an outside
    /// observer could see there. Deliberately invoked with `inner` NOT held: a park
    /// under the lock blocks the observer on that same mutex, so it reads identical
    /// whether or not publication and discharge are one state — and gates nothing.
    #[cfg(test)]
    pub(super) publish_window_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Fired at the two points of a publication that only look alike from outside it: once
    /// INSIDE the critical section, with the gate, the lease and `inner` all held, and once
    /// after the gate has been given up, immediately before what the section handed out is
    /// thrown away.
    ///
    /// Measured from outside the call, work done inside the section and work done after it are
    /// indistinguishable — which is exactly the difference the contract is about.
    #[cfg(test)]
    pub(super) install_section_hook: Option<Arc<dyn Fn(&'static str) + Send + Sync>>,
    /// Runs on the watcher's own thread inside the window between taking the continuation
    /// latch and sampling the alarm counter, so a test can hand work over in exactly that
    /// window and no other. A latch raised there is the one the counter cannot report: the
    /// producer's bump is already in the sample the wait compares against.
    ///
    /// A barrier, not a sleep: it runs on every turn, at one named point, and what it does
    /// there is the test's business.
    #[cfg(test)]
    pub(super) latch_window_hook: Option<LatchWindowHook>,
    /// Runs on the probing thread with the reservation already taken and no IO started, so a
    /// test can put a whole publication — or a second caller — into exactly that window.
    #[cfg(test)]
    pub(super) probe_window_hook: Option<LatchWindowHook>,
    /// Runs inside [`GraphState::walk_scan_receipt`], before the declaration is read and the
    /// tree is walked, so a test can move the workspace under exactly that window.
    ///
    /// What it is for: the verdict and the scope have to be two halves of ONE walk. Read from
    /// two caches instead, a verdict can be paired with a composition that never produced it,
    /// and nothing in the result would say so.
    #[cfg(test)]
    pub(super) scan_receipt_hook: Option<LatchWindowHook>,
    /// Runs inside a comparison, after it has read the position its answer may cover and
    /// before it claims anything, so a test can have a real fact delivered into exactly that
    /// window — the one where "the graph already matches disk" and "something changed" are
    /// both true, of two different moments.
    #[cfg(test)]
    pub(super) comparison_window_hook: Option<LatchWindowHook>,
    /// Runs inside the fingerprint walk itself, after the position this look may vouch for has
    /// been read and after the tree has been enumerated, but before the verdict is cached.
    ///
    /// The one window where "the graph matches disk" and "something changed" are both true, of
    /// two different moments: a change landing here is one this walk cannot have seen, and
    /// answering it with a number read at the verdict is how it disappears.
    #[cfg(test)]
    pub(super) scan_window_hook: Option<LatchWindowHook>,
    /// How many probe walks have actually started. A second caller that found the work owned
    /// does not move it.
    #[cfg(test)]
    pub(super) probe_walks: Arc<AtomicUsize>,
    /// What the point-patch path decided, in order, for a test that must tell a real point
    /// rewrite from a full rebuild standing in for one. Written by the production gates,
    /// consulted by none of them.
    #[cfg(test)]
    pub(super) incremental_decisions: Arc<Mutex<Vec<&'static str>>>,
    /// Counts completed passes of [`Self::notify_published`] — every return, including the
    /// one taken when this daemon no longer owns the caches and no hook runs at all.
    ///
    /// `Ready` is a status barrier, never a publish barrier, so a test asserting anything a
    /// publish pass leaves behind needs a barrier of its own. The publish hook is not always
    /// one: the pass does further work AFTER its hook returns (the ledger prune and the
    /// obligation it settles), and a test sampling inside that remainder reads a state no real
    /// consumer observes. This counter is the only observable for "the pass finished".
    #[cfg(test)]
    pub(super) publish_passes: Arc<AtomicUsize>,
    /// Snapshot installs still to be refused, on whatever thread this graph builds: a test
    /// reaches the watcher's own reload thread through it.
    #[cfg(test)]
    pub(super) refused_installs: Arc<AtomicUsize>,
    /// Invoked on this graph's background thread immediately after each publish/adopt,
    /// once the inner lock is released — the moment the graph "has caught up" and a
    /// consumer (search context re-render) may read the fresh graph. Never called on a
    /// query path. Receives a [`GraphPublishSignal`]: `mark_bound` bounds which marks the
    /// consumer may clear (correctness), `drift_pending` is a fast-path hint.
    pub(super) on_published:
        Option<Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>>,
    /// Everything the graph owes and the only thing that decides what it does about it (see
    /// [`super::debt`]). One state, one decision: a debt that lives in a flag of its own is a
    /// debt whose owner has to be argued about, and three review rounds found one each time.
    pub(super) debt: Arc<Mutex<GraphDebt>>,
    /// Serialises installing a publication against consuming marks against one. The hook the
    /// consumption fires renders context from the graph that is published right now, so the
    /// publication it is charged against has to be the one it read — the two cannot interleave.
    /// Order: this gate, then `inner`, then `debt`; never the other way round.
    pub(super) publication_gate: Arc<Mutex<()>>,
    /// This daemon's claim on the workspace's derived caches. The graph database is shared
    /// with every other daemon generation over the same workspace, so a superseded daemon
    /// builds and publishes nothing — it serves what it already holds and lets the owner
    /// maintain the file. Unmanaged (always owning) for a disabled graph and in tests.
    pub(super) lease: crate::workspace_lease::WorkspaceLease,
    /// The drift watcher's phase and the cursor it reads (see [`super::watcher`]).
    pub(super) watch: Arc<Mutex<(super::watcher::WatchPhase, Option<SinkCursor>)>>,
    /// Moved whenever owed work is armed from another thread, so a watcher asleep on a
    /// deadline computed before it wakes and computes a new one.
    pub(super) alarms: Arc<AtomicUsize>,
    /// A request asked for the build a request may ask for: the first one, and the retry the
    /// schedule says is due.
    ///
    /// A FACT, recorded and left for the owner, because deciding it needs the lease and the
    /// lease is not a request's to read. Nothing about what happens next changes: the same
    /// schedule, in the same executor, decides whether anything starts at all.
    pub(super) first_build_asked: Arc<std::sync::atomic::AtomicBool>,
    /// Raised while the fallback kick below is in flight, so a burst of requests over a
    /// watcherless graph asks once rather than once each.
    pub(super) first_build_kick: Arc<std::sync::atomic::AtomicBool>,
    /// How many of those fallbacks have been started. A test proving the watcher answered the
    /// ask has to be able to say the fallback did not.
    #[cfg(test)]
    pub(super) first_build_kicks: Arc<AtomicUsize>,
    /// Admissions granted in this generation, so each has an identity of its own.
    pub(super) claims: Arc<AtomicUsize>,
    /// The highest context-dirty mark the search engine placed for a render this graph could
    /// not serve, not yet registered. See [`OwedContextMarks`].
    pub(super) owed_context_marks: Arc<std::sync::atomic::AtomicI64>,
    /// Highest hub fact observed while those marks were reported. Written before the mark
    /// high-water and never reset, so a concurrent registration can only pair marks with an
    /// equal or later fact.
    pub(super) owed_context_fact: Arc<AtomicU64>,
    /// This process's hold on the graph file, shared with the lease observer that lets it go.
    pub(super) access: Arc<Mutex<GraphAccess>>,
    /// Who publishes through this graph, in every `publication_id` it writes.
    pub(super) publication_owner: u64,
    /// How many publication identities this graph has handed out.
    pub(super) publications: Arc<std::sync::atomic::AtomicU64>,
    /// Owed work a bounded turn could not finish. Read by the owner before its wait, so the
    /// yield hands the work on instead of sleeping on it.
    pub(super) continuation: Arc<std::sync::atomic::AtomicBool>,
    /// Makes this graph's loader thread refuse to start, so a test can see what a build that
    /// never began leaves behind. Per graph, never global: these tests run beside others that
    /// need a loader that starts.
    #[cfg(test)]
    pub(super) loader_cannot_spawn: Arc<std::sync::atomic::AtomicBool>,
    /// Builder threads this graph has actually started. A test that injects a spawn failure
    /// has to be able to say that no builder ran, rather than infer it from a debt that would
    /// read the same either way.
    #[cfg(test)]
    pub(super) builders_started: Arc<AtomicUsize>,
    /// Whole graph builders actually entered, independently of their loader thread.
    #[cfg(test)]
    pub(super) full_builds_started: Arc<AtomicUsize>,
    /// How many times the SQL of one patch plan has outlasted its budget in this process.
    pub(super) patch_overruns: Arc<Mutex<Option<(super::build::PatchPlan, u32)>>>,
    /// Reconciles delivered to this ledger by a consumer that records without deciding — the
    /// watcher. A stand waits on this to know a delivery has happened at all, which a ledger
    /// that correctly recognises the loss as one it already acted on cannot show by itself.
    #[cfg(test)]
    pub(super) quiet_losses: Arc<Mutex<Vec<Option<u64>>>>,
    /// The daemon's request that every background owner leave.
    ///
    /// The graph holds it because the STOP IS PART OF THE DECISION: every path that records a
    /// debt drives from inside itself, so a check placed around those calls closes only the
    /// half of the window it can see. Read without a lock (an atomic), because it is read while
    /// other locks are held. Never-stopped by default, which is what a disabled graph and a
    /// test without a daemon want.
    pub(super) stop: crate::state::OwnerStop,
    /// Makes the next reload claim answer `Held`, the way a lease that cannot be confirmed
    /// between the decision and the claim does. That window is one instruction wide in
    /// production, and what happens in it — whether a budget is spent for a build nobody
    /// started — is what a test needs to be able to see.
    #[cfg(test)]
    pub(super) claim_is_held: Arc<std::sync::atomic::AtomicBool>,
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
            inner: Arc::new(Mutex::new(Inner {
                status,
                published: None,
                claimed: None,
                building: None,
                carrier: 0,
                carries: 0,
                indexing_unread_files: None,
                build_ticker: None,
            })),
            scan: Arc::new(Mutex::new(None)),
            workspace_root,
            cache: None,
            drift_interval: DRIFT_CHECK_INTERVAL,
            change_hub: None,
            hub_cursor: Arc::new(Mutex::new(ScanCursorSlot::default())),
            scan_count: Arc::new(AtomicUsize::new(0)),
            on_published: None,
            debt: Arc::new(Mutex::new(GraphDebt::default())),
            publication_gate: Arc::new(Mutex::new(())),
            fp_map: Arc::new(Mutex::new(FpMapState::default())),
            store: GraphStore::default(),
            #[cfg(test)]
            background_snapshot_failure: Arc::new(AtomicU8::new(0)),
            #[cfg(test)]
            post_claim_hook: None,
            #[cfg(test)]
            build_candidate_hook: None,
            #[cfg(test)]
            handover_hook: None,
            #[cfg(test)]
            outcome_window_hook: None,
            #[cfg(test)]
            kick_release_hook: None,
            #[cfg(test)]
            latch_window_hook: None,
            #[cfg(test)]
            probe_window_hook: None,
            #[cfg(test)]
            scan_receipt_hook: None,
            #[cfg(test)]
            comparison_window_hook: None,
            #[cfg(test)]
            scan_window_hook: None,
            #[cfg(test)]
            probe_walks: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            incremental_decisions: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            publish_window_hook: None,
            #[cfg(test)]
            install_section_hook: None,
            #[cfg(test)]
            publish_passes: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            refused_installs: Arc::new(AtomicUsize::new(0)),
            lease: crate::workspace_lease::WorkspaceLease::unmanaged(),
            stop: crate::state::OwnerStop::default(),
            watch: Arc::new(Mutex::new((super::watcher::WatchPhase::Unwatched, None))),
            alarms: Arc::new(AtomicUsize::new(0)),
            first_build_asked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            first_build_kick: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            first_build_kicks: Arc::new(AtomicUsize::new(0)),
            claims: Arc::new(AtomicUsize::new(0)),
            owed_context_marks: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            owed_context_fact: Arc::new(AtomicU64::new(0)),
            access: Arc::new(Mutex::new(GraphAccess::NotHeld)),
            publication_owner: crate::workspace_lease::new_token(),
            publications: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            continuation: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            loader_cannot_spawn: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            builders_started: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            full_builds_started: Arc::new(AtomicUsize::new(0)),
            patch_overruns: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            quiet_losses: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            claim_is_held: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// A fresh identity for the next database this graph publishes.
    pub(super) fn next_publication_id(&self) -> String {
        let number = self.publications.fetch_add(1, Ordering::SeqCst) + 1;
        format!("{:016x}-{number}", self.publication_owner)
    }

    /// Where a context provider of this graph reports the marks it owes.
    pub(crate) fn owed_context_marks(&self) -> OwedContextMarks {
        OwedContextMarks {
            high: Arc::clone(&self.owed_context_marks),
            fact: Arc::clone(&self.owed_context_fact),
            alarms: Arc::clone(&self.alarms),
            hub: self.change_hub.clone(),
        }
    }

    /// Register the marks reported through [`OwedContextMarks`] as marks this graph consumes.
    ///
    /// Use the hub frontier captured by the provider report, not the later fact at registration:
    /// a deferred duplicate mark must not become fresh merely because another event arrived.
    /// The provider stores the fact before the mark high-water; swapping the high-water before
    /// loading the monotone fact can over-cover a concurrent report, but cannot under-cover it.
    pub(super) fn register_owed_context_marks(&self) {
        let high = self.owed_context_marks.swap(0, Ordering::SeqCst);
        if high > 0 {
            let fact = self.owed_context_fact.load(Ordering::SeqCst);
            self.marks_placed(high, fact);
        }
    }

    /// Tell the watcher its alarms may have moved.
    pub(super) fn wake_watcher(&self) {
        self.alarms.fetch_add(1, Ordering::SeqCst);
        if let Some(hub) = &self.change_hub {
            hub.wake_waiters();
        }
    }

    pub(super) fn set_watch(&self, phase: super::watcher::WatchPhase, cursor: Option<SinkCursor>) {
        *lock_recover(&self.watch) = (phase, cursor);
    }

    /// The drift watcher's phase, and the cursor whose health says how it watches.
    pub(crate) fn watch_state(&self) -> (super::watcher::WatchPhase, Option<SinkCursor>) {
        *lock_recover(&self.watch)
    }

    /// Put the graph in the state a throttled failure leaves: `Failed`, a build owed, and the
    /// retry budget holding the next attempt off until `at`.
    #[cfg(test)]
    pub(crate) fn fail_with_retry_held_until(&self, at: Instant) {
        let now = Instant::now();
        lock_recover(&self.debt).fail_held_until(now, at);
        lock_recover(&self.inner).status = GraphStatus::Failed("forced".to_owned());
        lock_recover(&self.debt).record_change(now, self.observation());
    }

    /// This generation lost the workspace for good, or handed it back.
    pub(super) fn lease_is_terminal(&self) -> bool {
        self.lease.is_superseded() || self.lease.is_released()
    }

    /// The graph's lifecycle as the decision reads it. Copied out before the debt lock is
    /// taken: deciding never reaches back into the graph.
    fn facts(&self) -> Facts {
        let terminal = self.lease_is_terminal();
        // Asked once, and not at all when the answer cannot matter: a terminal lease is
        // latched, and confirming ownership can go to disk.
        let owns = !terminal && self.lease.owns_caches_now();
        let (status, reload) = {
            let inner = lock_recover(&self.inner);
            (inner.status.clone(), inner.published.as_ref().map(|p| p.reload.clone()))
        };
        Facts {
            ready: matches!(status, GraphStatus::Ready { .. }),
            failed: matches!(status, GraphStatus::Failed(_)),
            idle: matches!(status, GraphStatus::Idle),
            in_flight: matches!(status, GraphStatus::Loading)
                || matches!(reload, Some(ReloadState::Running)),
            owns,
            terminal,
            stopping: self.stop.is_stopped(),
        }
    }

    /// What the graph owes right now, for a test that must see a debt rather than a flag.
    #[cfg(test)]
    pub(crate) fn owes_failed(&self) -> bool {
        lock_recover(&self.debt).owes_failed()
    }

    #[cfg(test)]
    pub(crate) fn owes_forced(&self) -> Option<u64> {
        lock_recover(&self.debt).owes_forced()
    }

    /// Reconciles delivered by a recording-only consumer, by the identity each carried. A
    /// delivery is listed once the ledger has it, so a stand waiting here is waiting for a
    /// delivery that is DONE, not for one that has started.
    #[cfg(test)]
    pub(crate) fn quiet_loss_deliveries(&self) -> Vec<Option<u64>> {
        lock_recover(&self.quiet_losses).clone()
    }

    /// The losses the ledger has acted on, by identity.
    #[cfg(test)]
    pub(crate) fn acted_losses(&self) -> Vec<u64> {
        lock_recover(&self.debt).acted_losses()
    }

    /// The highest fact a forced build of this graph has answered.
    #[cfg(test)]
    pub(crate) fn answered_forced(&self) -> u64 {
        lock_recover(&self.debt).answered_forced()
    }

    #[cfg(test)]
    pub(crate) fn owes_change(&self) -> Option<u64> {
        lock_recover(&self.debt).owes_change()
    }

    #[cfg(test)]
    pub(crate) fn owes_marks(&self) -> bool {
        lock_recover(&self.debt).owes_marks()
    }

    #[cfg(test)]
    pub(crate) fn owes_recovery(&self) -> bool {
        lock_recover(&self.debt).owes_recovery()
    }

    /// What the publish hook still owes, for a test that must see the obligation rather than
    /// the flag it used to live in.
    #[cfg(test)]
    pub(crate) fn hook_debt(&self) -> HookDebt {
        lock_recover(&self.debt).hook_debt()
    }

    /// Arm a hook obligation the way a publication whose hook refused would.
    #[cfg(test)]
    pub(crate) fn record_hook_debt(&self, owed: HookDebt) {
        lock_recover(&self.debt).record_hook(owed);
    }

    /// Record a delivered change WITHOUT deciding on it.
    ///
    /// The watcher's first observation uses this: its drain runs before the boot's fused build
    /// has claimed anything, and a decision taken there takes that claim — one full parse of
    /// the workspace instead of the fused one. The debt is recorded all the same, and the
    /// watcher drives once at the end of the observation.
    /// Record where the fact stream STANDS, without claiming anything was delivered.
    ///
    /// The distinction the budget rests on: an observation is not external work. A level may
    /// leave a comparison owed — a build that scanned before this point has not seen it — but
    /// it issues no credit and opens no retry epoch.
    pub(crate) fn observe_current_level(&self, level: u64) {
        lock_recover(&self.debt).observe_current_level(level);
        self.wake_watcher();
    }

    pub(crate) fn record_change_quietly(&self, fact: u64) {
        lock_recover(&self.debt).record_change(Instant::now(), fact);
        self.wake_watcher();
    }

    /// [`Self::record_change_quietly`] for a change no fingerprint comparison can answer.
    /// A reconcile reached this graph: the hub lost detail, so the next build re-reads the
    /// project. Identified by the loss itself, not by the fact number it happens to stand at.
    pub(crate) fn record_loss_quietly(&self, token: Option<u64>, fact: u64) {
        let horizon = self.loss_horizon(token);
        lock_recover(&self.debt).record_loss(Instant::now(), token, fact, horizon);
        // Listed once the ledger HAS it: what a stand reads after this is the ledger this
        // delivery reached, not the one it was still waiting for.
        #[cfg(test)]
        lock_recover(&self.quiet_losses).push(token);
        self.wake_watcher();
    }

    /// What the hub can still deliver of its losses, read before the ledger is locked: the
    /// hub's lock and the ledger's are never held together. Read while the consumer recording
    /// `token` still holds its batch, so the loss being recorded is always among the live ones.
    fn loss_horizon(&self, token: Option<u64>) -> Option<crate::change_hub::LossHorizon> {
        token.and(self.change_hub.as_ref()).map(WorkspaceChangeHub::loss_horizon)
    }

    pub(crate) fn record_forced_quietly(&self, fact: u64) {
        lock_recover(&self.debt).record_forced(Instant::now(), fact);
        self.wake_watcher();
    }

    /// What every debt says about itself right now, for a test that must assert a debt has an
    /// owner rather than merely that some alarm exists. A debt whose turn is NOW has no alarm
    /// — it is the executor's — so reading `wake_at` alone cannot tell "owned" from "lost".
    #[cfg(test)]
    pub(super) fn debt_standing(&self, now: Instant) -> crate::graph::debt::Standing {
        let facts = self.facts();
        lock_recover(&self.debt).standing(now, facts)
    }

    /// When the graph's owed work next comes due, for the watcher's alarm.
    pub(super) fn wake_at(&self, now: Instant) -> Option<Instant> {
        let facts = self.facts();
        lock_recover(&self.debt).decide(now, facts).wake_at
    }

    /// A change of the scan universe was delivered under `fact`.
    pub(crate) fn record_change(&self, fact: u64) {
        lock_recover(&self.debt).record_change(Instant::now(), fact);
        self.wake_watcher();
        self.drive();
    }

    /// A change no fingerprint comparison can answer — a config edit, a reconcile — was
    /// delivered under `fact`.
    /// [`Self::record_loss_quietly`] for a caller that also drives.
    pub(crate) fn record_loss(&self, token: Option<u64>, fact: u64) {
        let horizon = self.loss_horizon(token);
        lock_recover(&self.debt).record_loss(Instant::now(), token, fact, horizon);
        self.wake_watcher();
        self.drive();
    }

    pub(crate) fn record_forced(&self, fact: u64) {
        lock_recover(&self.debt).record_forced(Instant::now(), fact);
        self.wake_watcher();
        self.drive();
    }

    /// The lease could not be confirmed, so the decision waits — and the watcher has to be told.
    ///
    /// A hold moves the next decision to two seconds out, which is a new standing like any
    /// other. Every other mutation wakes the watcher; this one did not, and it is written from
    /// the publishing thread, so a watcher already asleep on its slice kept sleeping and the
    /// retry HELD_RETRY promises in two seconds arrived in thirty.
    fn hold_decision(&self, now: Instant) {
        lock_recover(&self.debt).hold(now);
        self.wake_watcher();
    }

    /// A request asked about this graph. It reads no disk: all it may do is pull a recovery
    /// probe forward, and the watcher is what runs it.
    pub(crate) fn note_request(&self) {
        if lock_recover(&self.debt).note_request(Instant::now()) {
            self.wake_watcher();
        }
    }

    /// Record a build failure as the debt it is. `Spawn` is the one that used to be lost:
    /// nothing calls back into a thread that never started.
    /// The comparison answered the failure: the publication on record already describes the
    /// disk. The reload SLOT has to hear that too — left `Failed`, it reports a failed catch-up
    /// for a graph that is perfectly current, and nothing is owed that would ever clear it.
    fn clear_failed_reload_slot(&self) {
        let mut inner = lock_recover(&self.inner);
        if let Some(published) = inner.published.as_mut() {
            if matches!(published.reload, ReloadState::Failed(_)) {
                published.reload = ReloadState::Idle;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn record_failure(&self, kind: FailureKind) -> bool {
        self.record_admission_failure(kind, |_| {})
    }

    /// Record how one admission failed: `end` writes the lifecycle transition that ends it.
    ///
    /// The transition, the sponsors and the grant are settled in ONE hold. From the moment the
    /// lifecycle reads the build as over another claim is legal, so sponsors read in a later
    /// hold could name that claim — closing the lanes that paid for a build which has not
    /// started. And a grant no builder took ends here with its outcome: left in the slot it
    /// reads as a build in flight, and the retry this failure schedules could never start.
    pub(super) fn record_admission_failure(
        &self,
        kind: FailureKind,
        end: impl FnOnce(&mut Inner),
    ) -> bool {
        let sponsors = {
            let mut inner = lock_recover(&self.inner);
            end(&mut inner);
            // The outcome of a specific admission. Without its ticket a failure closes whatever
            // account happens to be there — and mints one that never paid.
            inner
                .claimed
                .take()
                .or(inner.building)
                .map_or(super::debt::Sponsors { primary: true, marks: false }, |ticket| {
                    ticket.sponsors
                })
        };
        #[cfg(test)]
        if let Some(hook) = self.outcome_window_hook.clone() {
            hook(self);
        }
        let rearmed = lock_recover(&self.debt).record_failure(Instant::now(), kind, sponsors);
        self.wake_watcher();
        rearmed
    }

    /// The one executor. Every intake records and then calls this; the watcher calls it on its
    /// batch and on its alarm; the build thread calls it on its outcome. Single-flight by the
    /// lifecycle itself: a build in flight decides nothing, and its own end decides again.
    ///
    /// Never holds the debt lock across a disk walk, a spawn or the publish hook.
    pub(crate) fn drive(&self) {
        self.drive_as(Executor::Any);
    }

    /// [`Self::drive`] for the watcher, which may run every owed thing EXCEPT the very first
    /// build of an idle graph.
    ///
    /// That one belongs to the boot's fused pass, which parses the workspace once for the
    /// graph and the search index together, and to a request, which is what a lazy graph waits
    /// for. A watcher that takes it turns one parse into two. Deferring the decision is not
    /// enough on its own: a debt that is ripe NOW names no moment, the watcher's wait falls
    /// back to its slice, and at the end of that slice it takes the claim anyway — later, and
    /// on exactly the slow cold boot the fused pass exists for.
    pub(super) fn drive_without_the_first_build(&self) {
        self.drive_as(Executor::NotTheFirstBuild);
    }

    fn drive_as(&self, executor: Executor) {
        if self.workspace_root.is_none() {
            return;
        }
        let now = Instant::now();
        let facts = self.facts();
        if facts.terminal {
            lock_recover(&self.debt).abandon();
            return;
        }
        if facts.stopping {
            // Leaving is neither a refusal nor a failure: nothing is dropped, nothing is held
            // for a retry this generation will not make.
            return;
        }
        if !facts.owns {
            // Not a takeover, just an answer the lease could not give right now: nothing is
            // dropped and the decision is taken again on the schedule.
            //
            // Only while something IS owed. A hold is a promise to come back and decide, and
            // on a graph that owes nothing there is nothing to come back for: the hold would
            // be the earliest moment for ever, the watcher would wake on it every two seconds,
            // and every one of those turns would re-read the lease from disk and write the
            // hold again. `unowned_wake` already says it — "nothing at all when nothing is
            // owed" — and this is the caller that was making that untrue.
            let stale = lock_recover(&self.debt).stale();
            if stale {
                self.hold_decision(now);
            }
            return;
        }
        // ONE runner, bounded. Each turn takes a single action and then re-reads the state
        // that action changed, because a decision taken before an action is a decision about a
        // state that no longer exists — which is how the same hook revision came to be flushed
        // twice, and how a comparison that answered its own debt left a newly delivered fact
        // with nobody to run it until the watcher's whole slice ran out.
        const QUANTUM: usize = 8;
        for _ in 0..QUANTUM {
            let facts = self.facts();
            if facts.terminal || facts.stopping || facts.in_flight || !facts.owns {
                return;
            }
            let decision = lock_recover(&self.debt).decide(Instant::now(), facts);
            if let Some(start) = decision.start {
                tracing::info!(
                    action = "build",
                    trigger = start.trigger,
                    executor = ?executor,
                    "graph debt decision"
                );
            }
            if decision.check {
                tracing::info!(
                    action = "compare_disk",
                    trigger = "disk_event",
                    executor = ?executor,
                    "graph debt decision"
                );
            }
            if decision.probe {
                tracing::info!(
                    action = "probe_recovery",
                    trigger = "recovery",
                    executor = ?executor,
                    "graph debt decision"
                );
            }
            if decision.flush_hook.any() {
                let trigger = if decision.flush_hook.marks {
                    "owed_context_marks"
                } else {
                    "published_context_refresh"
                };
                tracing::info!(
                    action = "refresh_contexts",
                    trigger,
                    executor = ?executor,
                    "graph debt decision"
                );
            }
            let mut acted = false;
            if let Some(start) = decision.start {
                // The first build of an idle graph is not this executor's to take. Nothing is
                // dropped: the debt stands, and the boot or the first request answers it.
                if !(executor == Executor::NotTheFirstBuild && facts.idle) {
                    acted = true;
                    if self.start_build(start) == StartOutcome::Settled {
                        // A build is on its way, or the slot is held: either has an outcome of
                        // its own to decide on.
                        return;
                    }
                }
            } else if decision.check {
                acted = true;
                self.check_against_disk();
            } else if decision.probe {
                // The probe decides again itself once it has measured.
                self.probe_recovery();
                return;
            }
            if decision.flush_hook.any() {
                // Offered once. The next turn asks the debt again, and a revision the hook
                // took is no longer on it.
                //
                // The mask the decision carried is NOT what is offered: it was read before
                // this turn owned anything, and a publication that landed in between may have
                // taken it. The offer is claimed again under its owner, or not made at all.
                //
                // Counted as an action only when it WAS one: the flush declines while a build
                // or a comparison is pending, while another owner holds the offer, and when
                // the hook takes nothing — and a decline that still counted kept the turn busy
                // until the quantum ran out and then latched a continuation for work nobody
                // could do.
                acted |= self.flush_hook_debt();
            }
            if !acted {
                return;
            }
        }
        // The quantum ran out with work still ripe. Latching it is what keeps a cooperative
        // yield from turning into a thirty-second sleep: the owner reads the latch BEFORE it
        // samples the alarm counter, so the hand-off cannot be lost between the two.
        self.latch_continuation();
    }

    /// Say that owed work was left ripe when a turn yielded, and wake the owner to take it.
    pub(super) fn latch_continuation(&self) {
        self.continuation.store(true, Ordering::SeqCst);
        self.wake_watcher();
    }

    /// Take the continuation latch, if one was left. Read by the owner before it decides how
    /// long to wait.
    pub(super) fn take_continuation(&self) -> bool {
        self.continuation.swap(false, Ordering::SeqCst)
    }

    /// Whether a continuation is latched, without taking it. The owner's WAIT reads this: the
    /// latch can be raised between the owner taking it and sampling the alarm counter, and a
    /// predicate that watches only the counter then sleeps out a whole slice over work that
    /// was already handed to it.
    pub(super) fn continuation_pending(&self) -> bool {
        self.continuation.load(Ordering::SeqCst)
    }

    /// Hand back a reload slot this generation claimed and will not use.
    ///
    /// Only for a decline BEFORE any thread exists: a slot released while a loader runs would
    /// let a second one start beside it.
    fn release_reload_slot(&self) {
        // A released slot owns no mandate.
        lock_recover(&self.inner).claimed = None;
        let mut inner = lock_recover(&self.inner);
        if let Some(published) = inner.published.as_mut() {
            if published.reload == ReloadState::Running {
                published.reload = ReloadState::Idle;
            }
        }
    }

    /// Start the build a decision asked for. A reload takes the single slot under `inner`, so
    /// two deciders cannot both start one.
    fn start_build(&self, start: BuildStart) -> StartOutcome {
        // The marks' budget pays for an ATTEMPT that goes and reads disk, so it is spent
        // where one is claimed and nowhere else. Charged before the claim, a decision that
        // ends in `Held` (ownership not confirmed this moment) or `Running` (another drive
        // got there first) would burn the budget without a build — and a handful of those
        // leaves the marks owed with no schedule left to answer them.
        let claimed = |graph: &Self| {
            if start.forced {
                lock_recover(&graph.debt).spend_mark_attempt(Instant::now());
            }
        };
        // Read before the claim — and then taken from the SCAN the comparison actually used,
        // which may be a throttled one older still. Either way the answer covers what was
        // looked at and no more: a fact delivered after that walk has not been compared, and
        // answering it erases the only record of it.
        let read_before_claim = self.observation();
        match start.kind {
            BuildKind::Initial => {
                // The mode the DECISION fixed, carried into the ticket. Re-deriving it from
                // the forced field alone dropped a marks-sponsored forced build back to an
                // ordinary one — and an ordinary first build may answer with a cached graph
                // over a same-stat edit, which is the one thing marks exist to prevent.
                if self.ensure_loading_claimed(start.forced) {
                    claimed(self);
                }
                StartOutcome::Settled
            }
            BuildKind::Reload => match self.try_claim_reload(start.forced) {
                ReloadClaim::Stopping => StartOutcome::Settled,
                ReloadClaim::Claimed => {
                    claimed(self);
                    self.spawn_reload();
                    StartOutcome::Settled
                }
                // A forced claim is never refused for want of drift, so this is the ordinary
                // comparison answering the change: disk still matches the published build.
                ReloadClaim::NotOwed { comparison } => {
                    let mut debt = lock_recover(&self.debt);
                    debt.change_answered(read_before_claim.min(self.scan_watermark()));
                    // The retry this start was owed has now run: it compared and found the
                    // publication already describes the disk.
                    debt.failure_answered();
                    drop(debt);
                    self.clear_failed_reload_slot();
                    self.record_comparison_and_consume_marks(comparison, read_before_claim);
                    StartOutcome::AnsweredWithoutBuilding
                }
                // Measured, unpayable. Nothing is answered and nothing is retried: the debt
                // and its exhausted account stand, and the standing they produce is what the
                // status reports until fresh work revives it.
                ReloadClaim::Unsponsored => StartOutcome::Settled,
                ReloadClaim::Running => StartOutcome::Settled,
                ReloadClaim::Held => {
                    self.hold_decision(Instant::now());
                    StartOutcome::Settled
                }
            },
        }
    }

    /// Answer a delivered change by comparing the published build against disk. Read before
    /// the walk: every fact at or below it is one this comparison covers.
    fn check_against_disk(&self) {
        let read_before_claim = self.observation();
        #[cfg(test)]
        if let Some(hook) = self.comparison_window_hook.clone() {
            hook(self);
        }
        match self.try_claim_reload(false) {
            ReloadClaim::Stopping => {}
            ReloadClaim::Claimed => self.spawn_reload(),
            ReloadClaim::NotOwed { comparison } => {
                let mut debt = lock_recover(&self.debt);
                debt.change_answered(read_before_claim.min(self.scan_watermark()));
                debt.failure_answered();
                drop(debt);
                self.clear_failed_reload_slot();
                self.record_comparison_and_consume_marks(comparison, read_before_claim);
            }
            // See the same arm in `start_build`: a refusal for want of a sponsor answers
            // nothing about disk.
            ReloadClaim::Unsponsored => {}
            ReloadClaim::Running => {}
            ReloadClaim::Held => self.hold_decision(Instant::now()),
        }
    }

    /// Look at what the published build could not read. Nothing on the fact stream announces a
    /// restored permission, so this is the only owner such a publication can have.
    fn probe_recovery(&self) {
        // The lease, read where it may go to disk: never under a lock.
        let facts = self.facts();
        // The reservation and the basis are taken TOGETHER, under `inner` → debt, before any
        // disk is touched: what this walk is measuring against is fixed here, and a second
        // caller reaching this point finds the work owned rather than opening the tree beside
        // it.
        let plan = {
            let inner = lock_recover(&self.inner);
            // The lifecycle as it stands NOW, not as the decision read it. Between deciding to
            // look and reserving the walk a build can be admitted, a stop can land, and the
            // publication whose gaps these are can be replaced — and a walk reserved after any
            // of those is a walk of a world this caller is no longer the owner of.
            let live = super::debt::Facts {
                ready: matches!(inner.status, GraphStatus::Ready { .. }),
                failed: matches!(inner.status, GraphStatus::Failed(_)),
                idle: matches!(inner.status, GraphStatus::Idle),
                in_flight: matches!(inner.status, GraphStatus::Loading)
                    || matches!(
                        inner.published.as_ref().map(|published| &published.reload),
                        Some(ReloadState::Running)
                    )
                    || inner.claimed.is_some()
                    || inner.building.is_some(),
                stopping: self.stop.is_stopped(),
                ..facts
            };
            let mut debt = lock_recover(&self.debt);
            if !debt.decide(Instant::now(), live).probe {
                return;
            }
            debt.reserve_probe()
        };
        let Some(plan) = plan else { return };
        #[cfg(test)]
        if let Some(hook) = self.probe_window_hook.clone() {
            hook(self);
        }
        #[cfg(test)]
        self.probe_walks.fetch_add(1, Ordering::SeqCst);
        // The IO itself holds no lock of the graph's.
        let outcome = self.recovery_probe(&plan);
        // Read before the locks, for the same reason as above.
        let terminal = self.lease_is_terminal();
        let result = match outcome {
            super::snapshot::ProbeOutcome::Looked { levels, scope } => {
                let _inner = lock_recover(&self.inner);
                let mut debt = lock_recover(&self.debt);
                if terminal || self.stop.is_stopped() {
                    // Measured, but by an owner who may no longer act: the whole batch is
                    // refused, the token given back, and the revisit stays finite. A claim is
                    // NOT such a case — a walk whose basis still stands may finish while a
                    // build runs, and its credits simply land after that build's cutoff.
                    debt.release_probe(Instant::now(), plan.token, false);
                    super::debt::ProbeResult::Obsolete
                } else {
                    debt.finish_probe(
                        Instant::now(),
                        super::debt::ProbeReceipt {
                            token: plan.token,
                            basis: plan.basis,
                            levels,
                            scope,
                        },
                    )
                }
            }
            // Nothing was observed, so nothing is recorded about the workspace — but the
            // attempt is paced all the same, or the watcher asks again every turn for as long
            // as the obstacle lasts.
            super::snapshot::ProbeOutcome::CouldNotLook => {
                lock_recover(&self.debt).release_probe(Instant::now(), plan.token, false);
                super::debt::ProbeResult::CouldNotLook
            }
        };
        // What the schedule made of it, said outright. Read off `owes_forced_fact()` instead,
        // this asked the wrong question: several healings can share one observation, and a
        // number that does not move is not the same as authority that did not arrive.
        if result == super::debt::ProbeResult::NewEvidence {
            self.wake_watcher();
            self.drive();
        }
    }

    /// Offer the publish hook the refreshes it could not take. The hook may legitimately
    /// refuse again — the engine is not published yet, a fresher reload is coming — and then
    /// the debt simply stands.
    /// Offer the hook what it could not take. Says whether the offer was actually made: a
    /// decline is not an action, and counting it as one lets a turn spin out its whole quantum
    /// on work that cannot move.
    fn flush_hook_debt(&self) -> bool {
        // The offer has ONE owner, and this is it. Claimed WITHOUT waiting, for two reasons:
        // the owner holds it across the hook itself, so waiting would park the watcher inside
        // a consumer's callback; and the hook is entitled to drive the graph back, which on a
        // wait is not a duplicate offer but a self-deadlock. An offer somebody else is making
        // is not this turn's work — the debt stands, and its owner's reply decides again.
        let Some(_owner) = self.claim_hook_offer() else { return false };
        self.flush_hook_offer()
    }

    /// Take the publication gate without waiting, or report that somebody else holds it.
    ///
    /// A poisoned gate is a held gate that will never be given back, so the guard is taken out
    /// of the poison exactly as [`lock_recover`] does: the state it protects is rebuilt from
    /// the debt on every turn, and refusing to flush for the life of the daemon is the worse
    /// answer.
    fn claim_hook_offer(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        match self.publication_gate.try_lock() {
            Ok(owner) => Some(owner),
            Err(std::sync::TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// Make the offer, for a caller that already owns it.
    fn flush_hook_offer(&self) -> bool {
        if self.drift_pending() {
            return false;
        }
        // The offer this flush answers for, read HERE — under the owner — so the revision and
        // the mask are the same claim. A publication landing meanwhile raises a new one, and a
        // reply carrying the old number clears nothing: otherwise an offer made before that
        // publication would report ITS bits as taken.
        let (revision, owed) = lock_recover(&self.debt).claim_hook();
        let observed = self.consuming_observation();
        let bound = observed.and_then(|observed| lock_recover(&self.debt).marks.bound(observed));
        if !owed.any() && bound.is_none() {
            return false;
        }
        let outcome = self.fire_hook(bound.unwrap_or(0), owed.topology, owed.roots);
        let handled = HookDebt {
            topology: owed.topology && outcome.topology_handled,
            roots: owed.roots && outcome.roots_handled,
            marks: bound.is_some() && outcome.topology_handled,
        };
        let mut debt = lock_recover(&self.debt);
        debt.hook_handled(revision, handled);
        if let (Some(observed), Some(bound)) = (observed, bound) {
            if outcome.topology_handled {
                debt.marks.consumed(observed, bound);
            }
        }
        if !handled.any() {
            // Offered and took nothing. That is not an action, and asking again this turn is
            // how a consumer that refuses for as long as it lives — a search engine that never
            // came up — turned the executor into a zero-wait loop.
            debt.hook_refused(Instant::now(), revision);
            return false;
        }
        true
    }

    /// Attach the daemon's claim on the workspace's derived caches, so this graph stops
    /// building and publishing once a newer daemon generation takes the workspace over — and
    /// stops reading: a lost workspace retires the store, waits for the reads in flight, closes
    /// every handle and gives the file's access lock to the next owner. A check that could not
    /// answer only holds new reads back until one does.
    pub(crate) fn with_lease(mut self, lease: crate::workspace_lease::WorkspaceLease) -> Self {
        let store = self.store.clone();
        let access = Arc::clone(&self.access);
        lease.observe(Arc::new(move |check| {
            use super::snapshot::Admission;
            use crate::workspace_lease::OwnershipCheck;
            match check {
                OwnershipCheck::Owned => store.set_admission(Admission::Open),
                OwnershipCheck::Unknown => store.set_admission(Admission::Paused),
                OwnershipCheck::Lost => {
                    store.set_admission(Admission::Retired);
                    let (store, access) = (store.clone(), Arc::clone(&access));
                    let spawned = std::thread::Builder::new()
                        .name("bsl-graph-retire".to_owned())
                        .spawn(move || {
                            store.wait_until_returned();
                            release_access(&store, &access);
                        });
                    if let Err(error) = spawned {
                        // `released` completes the hand-over when its transport next asks.
                        tracing::warn!(%error, "could not start the graph retirement thread");
                    }
                }
            }
        }));
        self.lease = lease;
        self
    }

    /// Why this graph is not served, when that is so: the file in place could not be
    /// established after a replacement, or this process may not open the graph at all.
    pub(crate) fn unavailable_reason(&self) -> Option<String> {
        self.store.unusable_reason().or_else(|| self.ownership_refusal())
    }

    /// Why this process may not open the graph at all: its cache directory is held by another
    /// live process, or it could not coordinate over it. Unlike an unusable file, which a
    /// rebuild replaces, nothing this process does lifts it.
    pub(super) fn ownership_refusal(&self) -> Option<String> {
        let cache_dir =
            || self.cache().map(|cache| cache.root().display().to_string()).unwrap_or_default();
        if self.lease.coordination_failed() {
            return Some(format!(
                "graph unavailable: this process could not claim the cache directory {} and \
                 does not open a graph it cannot coordinate over; restart it once the directory \
                 is writable",
                cache_dir()
            ));
        }
        self.lease.busy_owner().map(|owner| {
            if let Some((mine, theirs)) = self.lease.foreign_workspace() {
                return format!(
                    "graph unavailable: the cache directory {} serves workspace {} — this \
                     process analyzes {}; give this process a separate --cache-dir",
                    cache_dir(),
                    theirs,
                    mine
                );
            }
            format!(
                "graph busy: the cache directory {} is used by another live process (pid {}, \
                 version {}); give this process a separate --cache-dir",
                cache_dir(),
                owner.pid,
                owner.version.as_deref().unwrap_or("unknown")
            )
        })
    }

    /// Whether this process has let the graph go for good: ownership was lost, every read in
    /// flight has finished and the file's access lock is released.
    pub(crate) fn released(&self) -> bool {
        release_access(&self.store, &self.access)
    }

    /// Since when this process has been waiting for the previous owner to release the graph
    /// file, while it waits.
    pub(crate) fn waiting_for_access_since(&self) -> Option<Instant> {
        match &*lock_recover(&self.access) {
            GraphAccess::Waiting { since } => Some(*since),
            _ => None,
        }
    }

    /// Take the graph file's access lock before this process opens the published graph, and
    /// keep it for as long as it may. Only a managed lease coordinates across processes; any
    /// other graph has nobody to wait for.
    ///
    /// Waits without a deadline when `wait`, re-trying the lock and never holding the lease's
    /// own lock meanwhile, and says so in the log at a bounded rate. `false`: the lock is not
    /// held — not yet, when not waiting, or never again, once ownership is gone.
    pub(super) fn acquire_graph_access(&self, wait: bool) -> bool {
        let Some(path) = self
            .cache()
            .filter(|_| self.lease.is_managed())
            .map(crate::cache::WorkspaceCacheLayout::graph_access_lock_path)
        else {
            return true;
        };
        let mut warned_at = None;
        loop {
            let taken = {
                let mut access = lock_recover(&self.access);
                match &*access {
                    GraphAccess::Held(_) => return true,
                    GraphAccess::Released => return false,
                    GraphAccess::NotHeld | GraphAccess::Waiting { .. } => {}
                }
                match crate::workspace_lease::ExclusiveFileLock::try_acquire(&path) {
                    Ok(Some(lock)) => Some(lock),
                    Ok(None) => {
                        if matches!(*access, GraphAccess::NotHeld) {
                            *access = GraphAccess::Waiting { since: Instant::now() };
                        }
                        None
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            path = %path.display(),
                            "could not take the graph access lock; the graph stays closed"
                        );
                        *access = GraphAccess::NotHeld;
                        return false;
                    }
                }
            };
            if let Some(lock) = taken {
                // Confirmed again once the file is ours — the lease may have moved on while this
                // process waited — and asked with the state unlocked: the answer can take the
                // lease's own lock, and a status request reads the state meanwhile.
                let owner = self.lease.owns_caches_now();
                let mut access = lock_recover(&self.access);
                if matches!(*access, GraphAccess::Released) {
                    return false;
                }
                if !owner {
                    *access = GraphAccess::NotHeld;
                    return false;
                }
                if let GraphAccess::Waiting { since } = &*access {
                    tracing::info!(
                        waited_secs = since.elapsed().as_secs(),
                        "the previous owner released the graph file; opening it"
                    );
                }
                *access = GraphAccess::Held(lock);
                return true;
            }
            if !wait || self.stop.is_stopped() || self.lease_is_terminal() {
                return false;
            }
            if warned_at.is_none_or(|at: Instant| at.elapsed() >= ACCESS_WAIT_WARNING) {
                warned_at = Some(Instant::now());
                tracing::warn!(
                    path = %path.display(),
                    waited_secs = self
                        .waiting_for_access_since()
                        .map_or(0, |since| since.elapsed().as_secs()),
                    "waiting for the previous owner of the graph to finish its reads and release \
                     the file"
                );
            }
            std::thread::sleep(ACCESS_RETRY);
        }
    }

    /// Wire the daemon's stop into the graph, so the decision reads it.
    pub(crate) fn with_owner_stop(mut self, stop: crate::state::OwnerStop) -> Self {
        self.stop = stop;
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
    /// the holes the watch does: the cache directory and the service directories of the
    /// workspace root. Empty when this state governs no cache.
    pub(crate) fn cache_exclusions(&self) -> Vec<std::path::PathBuf> {
        match (self.cache(), self.workspace_root.as_deref()) {
            (Some(cache), Some(root)) => cache.exclusions(root),
            (Some(cache), None) => {
                cache.spellings().iter().map(|path| path.to_path_buf()).collect()
            }
            (None, _) => Vec::new(),
        }
    }

    pub(crate) fn cache(&self) -> Option<&crate::cache::WorkspaceCacheLayout> {
        self.cache.as_ref()
    }

    /// The owner of this graph's read handles, for a consumer that reads the published graph
    /// long after this call — the search context provider.
    pub(crate) fn store(&self) -> &GraphStore {
        &self.store
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
    /// a [`GraphPublishSignal`]: `mark_bound` bounds which marks it may clear
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

    /// Attach the sample described by [`Self::install_section_hook`].
    #[cfg(test)]
    pub(super) fn with_install_section_hook(
        mut self,
        hook: Arc<dyn Fn(&'static str) + Send + Sync>,
    ) -> Self {
        self.install_section_hook = Some(hook);
        self
    }

    /// Attach the barrier described by [`Self::post_claim_hook`].
    #[cfg(test)]
    pub(super) fn with_post_claim_hook(mut self, hook: PostClaimHook) -> Self {
        self.post_claim_hook = Some(hook);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_build_candidate_hook_for_test(mut self, hook: BuildCandidateHook) -> Self {
        self.build_candidate_hook = Some(hook);
        self
    }

    #[cfg(test)]
    pub(super) fn enter_build_candidate_hook(&self) {
        if let Some(hook) = self.build_candidate_hook.clone() {
            hook(self);
        }
    }

    /// Attach the barrier described by [`Self::handover_hook`].
    #[cfg(test)]
    pub(super) fn with_handover_hook(mut self, hook: PostClaimHook) -> Self {
        self.handover_hook = Some(hook);
        self
    }

    /// Attach the barrier described by [`Self::outcome_window_hook`].
    #[cfg(test)]
    pub(super) fn with_outcome_window_hook(mut self, hook: PostClaimHook) -> Self {
        self.outcome_window_hook = Some(hook);
        self
    }

    /// Attach the barrier described by [`Self::kick_release_hook`].
    #[cfg(test)]
    pub(super) fn with_kick_release_hook(mut self, hook: PostClaimHook) -> Self {
        self.kick_release_hook = Some(hook);
        self
    }

    /// Attach the barrier described by [`Self::latch_window_hook`].
    #[cfg(test)]
    pub(super) fn with_latch_window_hook(mut self, hook: LatchWindowHook) -> Self {
        self.latch_window_hook = Some(hook);
        self
    }

    /// Attach the barrier described by [`Self::probe_window_hook`].
    #[cfg(all(test, unix))]
    pub(super) fn with_probe_window_hook(mut self, hook: LatchWindowHook) -> Self {
        self.probe_window_hook = Some(hook);
        self
    }

    /// Attach the barrier described by [`Self::scan_receipt_hook`].
    #[cfg(all(test, unix))]
    pub(super) fn with_scan_receipt_hook(mut self, hook: LatchWindowHook) -> Self {
        self.scan_receipt_hook = Some(hook);
        self
    }

    /// Attach the barrier described by [`Self::comparison_window_hook`].
    #[cfg(test)]
    pub(super) fn with_comparison_window_hook(mut self, hook: LatchWindowHook) -> Self {
        self.comparison_window_hook = Some(hook);
        self
    }

    /// Attach the barrier described by [`Self::scan_window_hook`].
    #[cfg(test)]
    pub(super) fn with_scan_window_hook(mut self, hook: LatchWindowHook) -> Self {
        self.scan_window_hook = Some(hook);
        self
    }

    /// The watcher is inside the window between taking the latch and sampling the alarms.
    #[cfg(test)]
    pub(super) fn enter_latch_window(&self) {
        if let Some(hook) = self.latch_window_hook.clone() {
            hook(self);
        }
    }

    /// The next admission's identity. Monotone within this generation, and never reused: an
    /// outcome names the claim it belongs to, so a late one cannot be mistaken for the current
    /// build's.
    fn next_claim_id(&self) -> u64 {
        self.claims.fetch_add(1, Ordering::SeqCst) as u64 + 1
    }

    /// A mandate for a build that reached the loader without an admission point of its own.
    ///
    /// Fixed here, before any disk read, so even this path has ONE reading of what it was
    /// admitted for rather than a live re-read later. It charges nothing: no slot was granted
    /// through the accounts, and inventing a charge would bill a lane that bought nothing.
    pub(super) fn mint_direct_ticket(&self, is_reload: bool) -> super::debt::BuildTicket {
        let scan_cutoff = self.observation();
        let mut debt = lock_recover(&self.debt);
        let forced_through = debt.forced_fact();
        let forced = forced_through.is_some() || debt.owes_recovery_build();
        let recovery_cutoff = debt.capture_recovery();
        let sponsors = super::debt::Sponsors { primary: true, marks: false };
        drop(debt);
        super::debt::BuildTicket {
            claim: self.next_claim_id(),
            kind: if is_reload { BuildKind::Reload } else { BuildKind::Initial },
            forced,
            scan_cutoff,
            forced_through,
            recovery_cutoff,
            sponsors,
        }
    }

    /// Issue a mandate for a build taking over a slot this generation already claimed.
    ///
    /// The slot was granted and paid for once; the work is being handed to another thread, and
    /// that thread must carry a ticket rather than re-derive its mandate. No second charge:
    /// the admission already happened.
    pub(super) fn issue_handover_ticket(&self) {
        let scan_cutoff = self.observation();
        let mut inner = lock_recover(&self.inner);
        // Still in the slot on the boot's path; already TAKEN by the builder that is handing
        // over on the lazy one. Either way it is the admission that was paid for, and reading
        // an empty slot as "nothing was admitted" minted a second one for free.
        if let Some(paid) = inner.claimed.or(inner.building) {
            // The admission that is being handed over, carried across unchanged. Rebuilt from
            // whatever the debts say now, it attached sponsors that never paid for this build,
            // captured recovery origins measured after the claim, and widened the cutoff its
            // proof may cover past the one the charge covered — a second, free admission
            // wearing the first one's name.
            inner.claimed = Some(super::debt::BuildTicket { kind: BuildKind::Reload, ..paid });
            return;
        }
        let mut debt = lock_recover(&self.debt);
        let forced_through = debt.forced_fact();
        let forced = forced_through.is_some() || debt.owes_recovery_build();
        let recovery_cutoff = debt.capture_recovery();
        // No admission anywhere: a hand-over nothing was granted for, and the primary lane is
        // what such a path reports under. The marks are NOT attached here — nothing charged
        // them for an attempt, and a sponsor that never paid must not be closed by this build's
        // outcome.
        let sponsors = super::debt::Sponsors { primary: true, marks: false };
        drop(debt);
        inner.claimed = Some(super::debt::BuildTicket {
            claim: self.next_claim_id(),
            kind: BuildKind::Reload,
            forced,
            scan_cutoff,
            forced_through,
            recovery_cutoff,
            sponsors,
        });
    }

    /// TAKE the ticket the admission granted: exactly one builder is admitted per claim, and
    /// exactly one reads its mandate. Taking rather than peeking is what keeps a finished
    /// build's ticket from standing in for the next one that never went through a claim.
    pub(super) fn take_claimed_ticket(&self) -> Option<super::debt::BuildTicket> {
        let mut inner = lock_recover(&self.inner);
        // Taken OUT of the slot and carried in the same hold. Between the two there is no
        // moment where the graph holds no ticket for a build that is under way — which is the
        // moment an outcome used to land in.
        let ticket = inner.claimed.take();
        if ticket.is_some() {
            inner.building = ticket;
            // Taken, and not yet carried: no earlier carry may let go of it.
            inner.carrier = 0;
        }
        ticket
    }

    /// Carry `ticket` for as long as the builder runs. Dropped at every exit of that work —
    /// publication, failure or panic — so a ticket is readable exactly while its build is the
    /// one in flight.
    pub(super) fn carry_ticket(
        &self,
        ticket: Option<super::debt::BuildTicket>,
    ) -> CarriedTicket<'_> {
        let mut inner = lock_recover(&self.inner);
        inner.carries += 1;
        let carrier = inner.carries;
        inner.building = ticket;
        inner.carrier = carrier;
        CarriedTicket { graph: self, carrier }
    }

    /// Give back a grant no builder will take.
    ///
    /// The claim is single-flight by itself: while it stands, every decision reads a build in
    /// flight and returns early. A path that finishes its own work without handing the ticket
    /// on has to give it back, or the graph spends the rest of its generation looking busy.
    ///
    /// Only the grant named: once the path's work is published another claim is legal, and
    /// the slot may by then hold somebody else's.
    pub(super) fn release_unused_claim(&self, claim: Option<u64>) {
        let mut inner = lock_recover(&self.inner);
        if claim.is_some() && inner.claimed.map(|ticket| ticket.claim) == claim {
            inner.claimed = None;
        }
    }

    /// The ticket currently held, for a test asserting what an admission granted.
    #[cfg(test)]
    pub(super) fn claimed_ticket(&self) -> Option<super::debt::BuildTicket> {
        lock_recover(&self.inner).claimed
    }

    /// Whether a build holds this graph's slot right now — granted, or already taken by the
    /// builder carrying it.
    #[cfg(test)]
    pub(crate) fn build_in_flight(&self) -> bool {
        let inner = lock_recover(&self.inner);
        inner.claimed.is_some() || inner.building.is_some()
    }

    /// The hub position a build reads BEFORE its pre-scan, recorded on its publication as
    /// [`Published::observed_through`]: every fact at or below it reached the hub before the
    /// scan read disk, so the published graph reflects it.
    ///
    /// A graph without a hub has no fact stream to fall behind; it reads as having observed
    /// every fact there could be.
    pub(super) fn observation(&self) -> u64 {
        self.change_hub.as_ref().map_or(u64::MAX, WorkspaceChangeHub::seq)
    }

    /// The observation of the current publication, if marks may be consumed against it: a
    /// ready graph with a proven coverage frontier (`observed_through` is set) and is neither
    /// the boot's stale cache nor a build that straddled a write.
    pub(super) fn consuming_observation(&self) -> Option<u64> {
        let inner = lock_recover(&self.inner);
        if !matches!(inner.status, GraphStatus::Ready { .. }) {
            return None;
        }
        let published = inner.published.as_ref()?;
        if published.stale || published.force_stale {
            return None;
        }
        // The other half of "unsound": modules whose bytes the build could not read. It is
        // not in `Published`, but the debt holds it — an unsound publication owes a probe
        // until one that read everything replaces it — and marks charged against such a
        // graph would be cleared against a rendering that never saw the files they were
        // placed for.
        if lock_recover(&self.debt).owes_recovery() {
            return None;
        }
        published.observed_through
    }

    /// Context-dirty marks up to `mark_high` were placed for hub facts up to `fact`.
    ///
    /// Marks are consumed by the first publication that observed their fact, whatever order
    /// the change, the marks and the publications arrive in: if the current publication
    /// already observed `fact`, the marks are consumed against it right away; otherwise they
    /// wait for one that does, and if no build is on its way to publish one, a build is owed.
    /// Marks a prior run left behind are fact `0`.
    pub(crate) fn marks_placed(&self, mark_high: i64, fact: u64) {
        if self.lease_is_terminal() {
            // Gone for good: this generation will never publish again, and the next one reads
            // the same marks out of the store.
            return;
        }
        // Recorded even when ownership cannot be confirmed right now. A mark dropped here is
        // a re-render nobody will ever ask for again; a mark kept costs one decision.
        lock_recover(&self.debt).place_marks(Instant::now(), mark_high, fact);
        self.wake_watcher();
        self.consume_observed_marks();
        self.settle_mark_obligation();
        self.drive();
    }

    /// Consume, against the current publication, every placed mark it observed.
    ///
    /// Under the publication gate from end to end: the hook renders context out of whatever
    /// graph is published when it runs, so the publication the marks are charged against has
    /// to be the one it read. Without the gate a `force_stale` publication installed midway
    /// would take marks that were cleared against a coherent one.
    fn consume_observed_marks(&self) {
        let _gate = lock_recover(&self.publication_gate);
        let Some(observed) = self.consuming_observation() else { return };
        self.consume_marks_through_while_published(observed);
    }

    /// An unchanged, complete disk comparison can extend the installed graph's fact coverage
    /// without publishing or projecting it again. Persist that proof on the exact publication
    /// it compared so marks placed later can use it too; failed context refreshes retain the
    /// frontier and are retried by the ordinary hook-debt path.
    fn record_comparison_and_consume_marks(&self, proof: Option<ComparisonProof>, cutoff: u64) {
        let Some(proof) = proof else { return };
        let _gate = lock_recover(&self.publication_gate);
        let through = {
            let mut inner = lock_recover(&self.inner);
            if !matches!(inner.status, GraphStatus::Ready { .. }) {
                return;
            }
            let Some(published) = inner.published.as_mut() else { return };
            if published.generation != proof.generation
                || published.fingerprint != proof.fingerprint
                || published.stale
                || published.force_stale
            {
                return;
            }
            let debt = lock_recover(&self.debt);
            if debt.owes_recovery() {
                return;
            }
            let compared_through = proof.through.min(cutoff);
            let observed_through = published.observed_through.get_or_insert(compared_through);
            *observed_through = (*observed_through).max(compared_through);
            let through = *observed_through;
            drop(debt);
            through
        };
        // A recovery obligation or a stale transition may have appeared while the comparison
        // was finishing. Recheck the existing consumption guards before calling the hook.
        if self.consuming_observation().is_none() {
            return;
        }
        self.consume_marks_through_while_published(through);
        // This comparison can make every placed mark covered; clear its build schedule even
        // when the context hook refuses, leaving only the independent hook retry debt.
        self.settle_mark_obligation();
    }

    /// The publication gate is held by the caller; the caller has verified the current
    /// publication is suitable for refreshing these contexts.
    fn consume_marks_through_while_published(&self, through: u64) {
        let Some(bound) = lock_recover(&self.debt).marks.bound(through) else { return };
        let (revision, _) = lock_recover(&self.debt).claim_hook();
        let outcome = self.fire_hook(bound, false, false);
        if outcome.topology_handled {
            let mut debt = lock_recover(&self.debt);
            debt.marks.consumed(through, bound);
            // A prior publication may have refused these already-covered marks and left
            // hook debt behind. This successful render consumed those same placements;
            // clear only that mark bit, and only if the offer revision did not change
            // while the consumer ran. Topology/root refresh debt is independent.
            debt.hook_handled(revision, HookDebt { topology: false, roots: false, marks: true });
        } else {
            let now = Instant::now();
            let mut debt = lock_recover(&self.debt);
            debt.record_hook(HookDebt { topology: false, roots: false, marks: true });
            debt.pace_hook_refusal(now);
        }
    }

    /// Owe a build to marks nothing is on its way to consume, or drop the obligation once
    /// none are left. A build in flight will publish and settle again, so nothing is owed
    /// while one runs.
    pub(super) fn settle_mark_obligation(&self) {
        let in_flight = self.drift_pending();
        let observed = self.consuming_observation();
        if lock_recover(&self.debt).settle_marks_through(Instant::now(), in_flight, observed) {
            self.wake_watcher();
        }
    }

    /// Fire the publish hook, if any. Called after a publish/adopt with no graph lock
    /// held, so the hook may take other locks (e.g. the search engine) without risking a
    /// lock-order inversion against the graph's inner mutex.
    ///
    /// The marks this publication may consume are the ones whose facts it observed (see
    /// [`Self::marks_placed`]); what is left afterwards is either owed a build or waits for
    /// one already running.
    pub(super) fn notify_published(&self, topology_changed: bool) {
        self.notify_published_pass(topology_changed);
        // Counted here rather than at the end of the pass itself: the pass has several
        // returns, and a barrier that misses one is worse than none — a test would wait on
        // a count that never arrives.
        #[cfg(test)]
        self.publish_passes.fetch_add(1, Ordering::SeqCst);
    }

    /// The pass itself; [`Self::notify_published`] wraps it only to count completions.
    fn notify_published_pass(&self, topology_changed: bool) {
        if !self.lease.owns_caches_now() {
            // No hook, no consumption — those need the caches. But the marks' OBLIGATION is
            // bookkeeping about what is owed, not an act of ownership, and skipping it left
            // placed marks with no schedule at all: `stale` reads the obligation, not the
            // placements, so the graph answered "caught up" while marks nobody had consumed
            // sat unowned. The same reasoning already makes `marks_placed` record regardless
            // of ownership.
            self.settle_mark_obligation();
            return;
        }
        {
            // The gate spans the whole charge against this publication, exactly as it spans a
            // consumption driven from the consumer's thread.
            let _gate = lock_recover(&self.publication_gate);
            // Topology context and search-root refreshes are independent obligations. Every
            // successful publish checks roots, while a previously failed check stays armed
            // until the hook reports it handled. A root-only failure must never manufacture a
            // semantic topology change on retry.
            let (revision, owed) = lock_recover(&self.debt).claim_hook();
            let topology = topology_changed || owed.topology;
            let observed = self.consuming_observation();
            let bound =
                observed.and_then(|observed| lock_recover(&self.debt).marks.bound(observed));
            let outcome = self.fire_hook(bound.unwrap_or(0), topology, true);
            let mut debt = lock_recover(&self.debt);
            debt.hook_handled(
                revision,
                HookDebt {
                    topology: outcome.topology_handled,
                    roots: outcome.roots_handled,
                    marks: bound.is_some() && outcome.topology_handled,
                },
            );
            debt.record_hook(HookDebt {
                topology: topology && !outcome.topology_handled,
                roots: !outcome.roots_handled,
                marks: bound.is_some() && !outcome.topology_handled,
            });
            if topology && !outcome.topology_handled
                || !outcome.roots_handled
                || bound.is_some() && !outcome.topology_handled
            {
                // Offered this publication's refreshes and took none of them. A refusal earns
                // the same finite pause here as on an executor's turn: without one the pass
                // records what was refused, the executor it drives at the end finds that debt
                // ripe NOW, and the consumer that has just said no is asked again in the same
                // breath. Paced after the record, so it is the debt now standing that waits.
                debt.pace_hook_refusal(Instant::now());
            }
            if let (Some(observed), Some(bound)) = (observed, bound) {
                if outcome.topology_handled {
                    debt.marks.consumed(observed, bound);
                }
            }
        }
        self.settle_mark_obligation();
        // Whatever this publication did not discharge is decided again right here, on this
        // thread: the drift that arrived while the build ran, the forced reload it did not
        // answer, the recovery it could not prove.
        self.drive();
    }

    /// Invoke the publish hook, if any, with the given independent obligations.
    fn fire_hook(
        &self,
        mark_bound: i64,
        topology_changed: bool,
        roots_refresh_requested: bool,
    ) -> GraphPublishOutcome {
        let (revision, fingerprint, workspace_roots) = lock_recover(&self.inner)
            .published
            .as_ref()
            .map(|p| (p.generation, p.fingerprint, p.search_roots.clone()))
            .unwrap_or((0, crate::graph_db::GraphFp::default(), None));
        match &self.on_published {
            Some(hook) => hook(GraphPublishSignal {
                drift_pending: self.drift_pending(),
                mark_bound,
                topology_changed,
                topology: fingerprint.topology,
                revision,
                fingerprint,
                roots_refresh_requested,
                workspace_roots,
            }),
            None => GraphPublishOutcome::HANDLED,
        }
    }

    /// Run the refreshes a publish hook could not take, now that the engine exists to run
    /// them. The request is normally raised BY a publish and consumed by that same publish's
    /// hook; at boot the order can invert, and on a fused cold build nothing publishes again,
    /// so files the build skipped as byte-identical would keep contexts rendered under the old
    /// topology indefinitely.
    ///
    /// One entry for both obligations, because they are one debt with two halves and the
    /// decision that owns them is the same.
    pub(crate) fn flush_hook_obligations(&self) {
        if !self.lease.owns_caches_now() {
            return;
        }
        if !matches!(self.status(), GraphStatus::Ready { .. }) {
            return;
        }
        // One owner per offer. This entry WAITS for it: a consumer that has just come up is
        // asking for the refresh it exists to run, not taking an executor's turn that can be
        // taken again in a moment.
        let _owner = lock_recover(&self.publication_gate);
        self.flush_hook_offer();
    }

    /// Recover context-dirty marks a PRIOR daemon run left in the persisted `context_dirty`
    /// table. `leftover_bound` is the mark high-water the caller read when it observed them,
    /// before anything of this run could stamp a mark; they predate every fact this run's hub
    /// delivers, so they are fact `0` and any publication that scanned disk consumes them.
    pub(crate) fn consume_leftover_marks(&self, leftover_bound: i64) {
        self.marks_placed(leftover_bound, 0);
    }

    /// Whether any placed mark is still waiting to be consumed.
    #[cfg(test)]
    pub(crate) fn marks_pending(&self) -> bool {
        lock_recover(&self.debt).marks.has_placed()
    }

    /// Whether a published reload is currently `Running`.
    fn reload_running(&self) -> bool {
        matches!(
            lock_recover(&self.inner).published.as_ref().map(|p| &p.reload),
            Some(ReloadState::Running)
        )
    }

    /// Whether a fresher build is already on its way: one is running, or one is loading. The
    /// publish hook uses this only as a fast-path hint — when a follow-up reload will publish
    /// shortly it can skip this round and let that reload's publish re-render against the
    /// fresher graph. It is NOT what makes consumption correct: the `mark_bound` already
    /// prevents clearing a mark against a graph that predates its drift.
    pub(crate) fn drift_pending(&self) -> bool {
        matches!(self.status(), GraphStatus::Loading) || self.reload_running()
    }

    /// A bounded owner sample; no descriptor checkout, SQL, drift scan or blocking lock.
    pub(crate) fn indexing_snapshot(&self) -> crate::indexing::Target {
        self.status_report_with_indexing().1
    }

    fn indexing_from_inner(
        &self,
        inner: &Inner,
        superseded: bool,
        released: bool,
        debt_stale: Option<bool>,
        observed_stale: bool,
    ) -> crate::indexing::Target {
        use crate::indexing::{Kind, Reason, State, Target};
        if superseded {
            return Target::new(Kind::Graph, State::Superseded, Some(Reason::Superseded));
        }
        if released {
            return Target::new(Kind::Graph, State::Cancelled, Some(Reason::Cancelled));
        }
        let (state, reason) = match &inner.status {
            GraphStatus::Idle => (State::Waiting, Some(Reason::Initializing)),
            GraphStatus::Disabled => (State::Unknown, Some(Reason::SnapshotUnavailable)),
            GraphStatus::Loading => (State::Running, None),
            GraphStatus::Failed(_) => (State::Failed, Some(Reason::NativeFailure)),
            GraphStatus::Ready { .. } => {
                let Some(published) = &inner.published else { return Target::unknown(Kind::Graph) };
                match &published.reload {
                    ReloadState::Running => (State::Running, None),
                    ReloadState::Failed(_) => (State::Failed, Some(Reason::NativeFailure)),
                    ReloadState::Idle => {
                        let Some(unread) = inner.indexing_unread_files else {
                            return Target::unknown(Kind::Graph);
                        };
                        let Some(debt_stale) = debt_stale else {
                            return Target::unknown(Kind::Graph);
                        };
                        if published.stale
                            || published.force_stale
                            || unread > 0
                            || debt_stale
                            || observed_stale
                        {
                            (State::Waiting, Some(Reason::StaleGeneration))
                        } else {
                            (State::Ready, None)
                        }
                    }
                }
            }
        };
        Target::new(Kind::Graph, state, reason)
    }

    pub(crate) fn status(&self) -> GraphStatus {
        lock_recover(&self.inner).status.clone()
    }

    /// A bounded read for the cold-loading response. A ready snapshot that is reloading never
    /// exposes a build estimate, and a busy lifecycle lock simply leaves it unknown.
    pub(crate) fn cold_build_eta_seconds(&self) -> Option<u64> {
        let inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        if inner.status != GraphStatus::Loading || inner.published.is_some() {
            return None;
        }
        inner.build_ticker.as_ref()?.eta_seconds()
    }

    pub(super) fn start_cold_build_ticker(&self, ticker: Arc<ide::GraphBuildTicker>) {
        let mut inner = lock_recover(&self.inner);
        if inner.status == GraphStatus::Loading && inner.published.is_none() {
            inner.build_ticker = Some(ticker);
        }
    }

    /// Finish the final measured interval only after the publication is installed.
    pub(super) fn finish_cold_build_ticker(&self) {
        let ticker = { lock_recover(&self.inner).build_ticker.take() };
        if let Some(ticker) = ticker {
            ticker.note("published", 0, 0, "");
        }
    }

    pub(super) fn clear_cold_build_ticker(&self) {
        lock_recover(&self.inner).build_ticker = None;
    }

    /// Whether a generation is being rebuilt while the installed snapshot remains
    /// answerable. Readers use this to label cross-cache results consistently.
    pub(crate) fn is_reloading(&self) -> bool {
        let inner = lock_recover(&self.inner);
        inner.published.as_ref().is_some_and(|published| {
            inner.status == GraphStatus::Loading || published.reload == ReloadState::Running
        })
    }

    #[cfg(test)]
    pub(crate) fn set_reload_running_for_test(&self) {
        if let Some(published) = lock_recover(&self.inner).published.as_mut() {
            published.reload = ReloadState::Running;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_loading_for_test(&self) {
        let mut inner = lock_recover(&self.inner);
        inner.status = GraphStatus::Loading;
        inner.build_ticker = None;
    }

    /// Who watches this graph's drift, and how. A graph whose watcher never started or has
    /// left is unobserved: nothing would notice it falling behind.
    pub(crate) fn drift_watch(&self) -> crate::tools::location::DriftWatch {
        use crate::tools::location::DriftWatch;
        match self.watch_state().0 {
            super::watcher::WatchPhase::Unwatched | super::watcher::WatchPhase::Stopped => {
                DriftWatch::Unobserved
            }
            super::watcher::WatchPhase::Starting => DriftWatch::Starting,
            super::watcher::WatchPhase::Running => match &self.change_hub {
                Some(hub) if hub.is_polling() || hub.is_partially_blind() => DriftWatch::Polling,
                Some(_) => DriftWatch::Watching,
                None => DriftWatch::Unobserved,
            },
        }
    }

    /// Whether this graph's watch cannot vouch for disk right now: it has not finished its
    /// first look, it has left, or the poll that stands in for it is overdue.
    fn watch_vouches_for_nothing(&self, watch: crate::tools::location::DriftWatch) -> bool {
        use crate::tools::location::DriftWatch;
        match watch {
            DriftWatch::Starting | DriftWatch::Unobserved => true,
            DriftWatch::Polling => self.change_hub.as_ref().is_some_and(|hub| hub.poll_overdue()),
            DriftWatch::Watching => false,
        }
    }

    /// Request-safe freshness from the publication paired with this pre-opened snapshot.
    pub(crate) fn cached_freshness(&self, snapshot: &super::GraphSnapshot) -> Freshness {
        let (stale, reload, topology, building) = {
            let inner = lock_recover(&self.inner);
            let publication = inner
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
                // No publication carries this snapshot's generation any more: a newer revision
                // replaced it while this request was in flight, so the data being returned is by
                // construction behind. Reporting the snapshot's own `force_stale` here would call
                // an obsolete revision fresh.
                .unwrap_or((true, "none", snapshot.fingerprint.topology));
            (
                publication.0,
                publication.1,
                publication.2,
                matches!(&inner.status, GraphStatus::Loading)
                    || inner.published.as_ref().is_some_and(|published| {
                        published.generation == snapshot.generation
                            && published.reload == ReloadState::Running
                    }),
            )
        };
        let drift_watch = self.drift_watch();
        Freshness {
            revision: snapshot.generation,
            // A catch-up that is owed but not finished is staleness the consumer can act on,
            // and it costs an atomic to say so. Without it the envelope reports a graph that
            // is known to be behind as fresh — the request path no longer walks disk, so this
            // is the only place the fact can still surface.
            stale: stale
                || building
                || snapshot.force_stale
                || snapshot.unread_files() > 0
                || self.drift_pending()
                // Every debt the graph carries, read from the one state that holds them: a
                // delivered change not yet answered, a forced reload owed, a build that failed
                // and is owed again, a publication that cannot vouch for itself, marks no
                // publication has observed. One term, because it is one question.
                || lock_recover(&self.debt).stale()
                || self.watch_vouches_for_nothing(drift_watch),
            reload,
            topology,
            drift_watch,
        }
    }

    #[cfg(test)]
    pub(crate) fn status_report(&self) -> GraphStatusReport {
        self.status_report_with_indexing().0
    }

    /// Legacy report and telemetry share one publication/revision sample.
    pub(crate) fn status_report_with_indexing(
        &self,
    ) -> (GraphStatusReport, crate::indexing::Target) {
        use crate::indexing::{Kind, Target};
        use crate::tools::location::DriftWatch;
        let watch_sample = (|| {
            let phase = self.watch.try_lock().ok()?.0;
            let (polling, cycle, overdue) = match &self.change_hub {
                Some(hub) => hub.try_poll_status()?,
                None => (false, None, false),
            };
            let watch = match phase {
                super::watcher::WatchPhase::Unwatched | super::watcher::WatchPhase::Stopped => {
                    DriftWatch::Unobserved
                }
                super::watcher::WatchPhase::Starting => DriftWatch::Starting,
                super::watcher::WatchPhase::Running if self.change_hub.is_none() => {
                    DriftWatch::Unobserved
                }
                super::watcher::WatchPhase::Running if polling => DriftWatch::Polling,
                super::watcher::WatchPhase::Running => DriftWatch::Watching,
            };
            let stale = matches!(watch, DriftWatch::Unobserved | DriftWatch::Starting) || overdue;
            Some((watch, cycle, stale))
        })();
        let drift_watch = self
            .workspace_root
            .as_ref()
            .and(watch_sample.as_ref())
            .map(|(watch, _, _)| watch.as_str());
        let report = |state, superseded| GraphStatusReport {
            state,
            files: None,
            unread_files: None,
            revision: None,
            stale: None,
            reload: None,
            error: None,
            superseded,
            drift_watch,
            poll_cycle_secs: watch_sample.and_then(|(_, cycle, _)| cycle),
        };
        if let Some(reason) = self.unavailable_reason() {
            return (
                GraphStatusReport { error: Some(reason), ..report("failed", None) },
                Target::new(Kind::Graph, crate::indexing::State::Failed, None),
            );
        }
        if let Some(since) = self.waiting_for_access_since() {
            return (
                GraphStatusReport {
                    error: Some(format!(
                        "waiting {}s for the previous owner to finish its reads and release the \
                         graph file",
                        since.elapsed().as_secs()
                    )),
                    ..report("loading", None)
                },
                Target::new(Kind::Graph, crate::indexing::State::Waiting, None),
            );
        }
        let superseded = self.lease.is_superseded();
        // A busy owner reads as loading, but a superseded one is terminal and never loads.
        let busy = || {
            if superseded {
                (
                    GraphStatusReport {
                        error: Some(SUPERSEDED_GRAPH_ERROR.to_owned()),
                        ..report("failed", Some(true))
                    },
                    Target::new(
                        Kind::Graph,
                        crate::indexing::State::Superseded,
                        Some(crate::indexing::Reason::Superseded),
                    ),
                )
            } else {
                (report("loading", None), Target::unknown(Kind::Graph))
            }
        };
        let Some((_, _, watch_stale)) = watch_sample else {
            return busy();
        };
        let Ok(inner) = self.inner.try_lock() else {
            return busy();
        };
        let debt_stale = self.debt.try_lock().ok().map(|debt| debt.stale());
        let snapshot_stale = inner.published.as_ref().and_then(|published| {
            self.store
                .status()
                .filter(|status| status.generation == Some(published.generation))
                .and_then(|status| status.idle_force_stale)
        });
        let target = self.indexing_from_inner(
            &inner,
            superseded,
            self.lease.is_released(),
            debt_stale,
            snapshot_stale.unwrap_or(false) || (self.workspace_root.is_some() && watch_stale),
        );
        if let GraphStatus::Ready { files } = &inner.status {
            if let (Some(published), Some(unread)) = (&inner.published, inner.indexing_unread_files)
            {
                // Preserve legacy read availability without checking out a descriptor or doing I/O.
                if let (Some(snapshot_stale), Some(debt_stale)) = (snapshot_stale, debt_stale) {
                    return (
                        GraphStatusReport {
                            files: Some(*files),
                            unread_files: Some(unread),
                            revision: Some(published.generation),
                            stale: Some(
                                published.stale
                                    || published.force_stale
                                    || snapshot_stale
                                    || unread > 0
                                    || matches!(published.reload, ReloadState::Running)
                                    || debt_stale
                                    || watch_stale,
                            ),
                            reload: Some(published.reload.label()),
                            ..report("ready", superseded.then_some(true))
                        },
                        target,
                    );
                }
            }
        }
        let legacy = if superseded {
            GraphStatusReport {
                error: Some(SUPERSEDED_GRAPH_ERROR.to_owned()),
                ..report("failed", Some(true))
            }
        } else {
            match &inner.status {
                GraphStatus::Disabled => report("disabled", None),
                GraphStatus::Failed(msg) => {
                    GraphStatusReport { error: Some(msg.clone()), ..report("failed", None) }
                }
                _ => report("loading", None),
            }
        };
        (legacy, target)
    }

    /// Start the initial load: `Idle → Loading`, one loader thread, nothing else.
    ///
    /// The lifecycle transition only. Whether a load is DUE — the retry budget, the backoff,
    /// the drift that justifies it — is decided in [`super::debt`] and executed by
    /// [`Self::drive`]; this is what that decision calls, and what the boot calls for the very
    /// first build. A spawn that fails is recorded as the debt it is: nothing else would ever
    /// call back into a thread that never started.
    pub(crate) fn ensure_loading(&self) {
        // The slot first, and the lease only if this call could start something at all.
        //
        // Everything below asks `facts`, and `facts` asks the lease — a file read, and a file
        // LOCK when the record has to be re-claimed. On a published graph no build can be
        // claimed here whatever the lease says, so on every request after the first that read
        // was pure cost on the request thread, waiting out another daemon's lock for an answer
        // that could change nothing. The lifecycle is unchanged: an idle or failed graph still
        // reaches the decision below, which is where the lease question belongs.
        {
            let inner = lock_recover(&self.inner);
            let startable = matches!(inner.status, GraphStatus::Idle | GraphStatus::Failed(_));
            if !startable || inner.claimed.is_some() || inner.building.is_some() {
                return;
            }
        }
        // A graph that has FAILED is the debt's business, not this call's. The boot reaches
        // here on its own failure paths, and restarting a failed build from each of them would
        // spend the retry budget outside the schedule that exists to bound it — including a
        // budget already exhausted, or a backoff that has not elapsed. An idle graph has no
        // such schedule: its first build is exactly what this call is for.
        if matches!(lock_recover(&self.inner).status, GraphStatus::Failed(_))
            && !matches!(self.debt_ripeness_of_failure(), Some(crate::graph::debt::Ripeness::Now))
        {
            return;
        }
        // The boot and a request enter here without a decision of their own, so the mode is
        // asked of the schedule: marks owed in Idle or Failed make the first build a forced
        // one, and losing that here is the same defect as losing it on the decided path.
        // `facts` FIRST, and then the debt. It reads `inner`, and every other path in this
        // module takes `inner` before `debt`; taking them the other way round here inverted
        // the order against the claim and deadlocked the graph outright.
        let facts = self.facts();
        if !facts.owns {
            // Ownership could not be confirmed — a peer holds the lease lock, or the startup
            // claim could not be written — and an unanswered question is not an answer. A build
            // admitted here would scan and write a whole database for its own publication fence
            // to throw away. The ask this call answers stays owed to the owner, and the hold is
            // what paces the next look; a lease that is gone for good leaves nothing to ask.
            if !facts.terminal && !facts.stopping {
                self.first_build_asked.store(true, Ordering::SeqCst);
                self.hold_decision(Instant::now());
            }
            return;
        }
        let forced = lock_recover(&self.debt)
            .decide(Instant::now(), facts)
            .start
            .is_some_and(|start| start.forced);
        self.ensure_loading_claimed(forced);
    }

    /// What the failure debt says about itself right now, for the one caller that must not
    /// restart a failed build the schedule is still holding off.
    fn debt_ripeness_of_failure(&self) -> Option<crate::graph::debt::Ripeness> {
        let facts = self.facts();
        lock_recover(&self.debt).standing(Instant::now(), facts).failed
    }

    /// The build a REQUEST may ask for: the first one, and the retry the schedule says is due.
    ///
    /// ASKS, and does not decide. A graph that has failed is not idle: when it is built again,
    /// and how often, is the debt's decision — it holds the failure's schedule and what is left
    /// of its budget — and a request that started one here would run the same failing build
    /// again on every call, which is the loop that schedule exists to prevent. A workspace
    /// whose user only ever asks for the graph still gets its first build from the asking.
    ///
    /// The asking is all that happens on this thread. Deciding reads the lifecycle FACTS, and
    /// those include ownership, which is a file — and, when the record has to be re-claimed, a
    /// file lock another daemon may hold for seconds. A published graph was already answered
    /// from the slot alone; the two states where a build really could be claimed, idle and
    /// failed-and-due, are what this leaves to the owner.
    pub(crate) fn ensure_first_build(&self) {
        {
            // The slot, and nothing that can go to disk. `claimed`/`building` are the
            // single-flight the decision would find anyway.
            let inner = lock_recover(&self.inner);
            let startable = matches!(inner.status, GraphStatus::Idle | GraphStatus::Failed(_));
            if !startable || inner.claimed.is_some() || inner.building.is_some() {
                return;
            }
        }
        self.first_build_asked.store(true, Ordering::SeqCst);
        // Latched, not merely counted. The owner reads the latch BEFORE it samples the alarm
        // counter, so an ask landing in the window between that sample and the wait cannot be
        // lost — and a lost one is not a late build but a thirty-second sleep over a workspace
        // the caller is waiting on. The same hand-off a yielded turn uses, for the same reason.
        self.latch_continuation();
        self.kick_first_build_without_a_watcher();
    }

    /// The same ask where no watcher will ever come for it.
    ///
    /// A watcher that never started — no cursor, or a spawn that failed — leaves the graph with
    /// no executor at all, and then "a workspace whose user only ever asks for the graph gets
    /// its first build from the asking" would simply stop being true. So the ask is carried by
    /// one short-lived thread instead, single-flight: a burst of requests raises the same flag,
    /// and whichever kick is in flight reads it. It decides nothing itself — it calls the same
    /// entry the boot and the watcher call — and it never starts while the daemon is leaving.
    fn kick_first_build_without_a_watcher(&self) {
        if self.stop.is_stopped() || self.watch_state().0 == super::watcher::WatchPhase::Running {
            return;
        }
        if self.first_build_kick.swap(true, Ordering::SeqCst) {
            return;
        }
        #[cfg(test)]
        self.first_build_kicks.fetch_add(1, Ordering::SeqCst);
        let graph = self.clone();
        let spawned =
            std::thread::Builder::new().name("bsl-graph-first-build".to_owned()).spawn(move || {
                loop {
                    if graph.first_build_asked.swap(false, Ordering::SeqCst) {
                        graph.ensure_loading();
                    }
                    #[cfg(test)]
                    if let Some(hook) = graph.kick_release_hook.clone() {
                        hook(&graph);
                    }
                    graph.first_build_kick.store(false, Ordering::SeqCst);
                    // Read AFTER letting go. An ask that landed while this kick still held the
                    // flag was left to it, and it is the only carrier that ask has: whoever
                    // asked saw the flag taken and returned.
                    if !graph.first_build_asked.load(Ordering::SeqCst) {
                        break;
                    }
                    // An ask the decision put back under a hold is not due before the hold
                    // runs out. Waited out stop-aware, so a lease that stays unconfirmed is
                    // looked at on the hold's cadence and never in a loop.
                    let held = lock_recover(&graph.debt).held_until(Instant::now());
                    if let Some(until) = held {
                        if graph.stop.sleep(until.saturating_duration_since(Instant::now())) {
                            break;
                        }
                    }
                    if graph.stop.is_stopped()
                        || graph.watch_state().0 == super::watcher::WatchPhase::Running
                        || graph.first_build_kick.swap(true, Ordering::SeqCst)
                    {
                        break;
                    }
                }
            });
        if spawned.is_err() {
            // Nothing was started and nothing is in flight: the flag stays raised for the next
            // caller, and the ask is still on the graph for a watcher that may yet arrive.
            self.first_build_kick.store(false, Ordering::SeqCst);
        }
    }

    /// Take the ask a request left, for the executor that answers it.
    pub(super) fn take_first_build_ask(&self) -> bool {
        self.first_build_asked.swap(false, Ordering::SeqCst)
    }

    /// [`Self::ensure_loading`] that says whether THIS call claimed the build. A caller that
    /// pays for an attempt needs to know it got one.
    fn ensure_loading_claimed(&self, forced: bool) -> bool {
        if self.workspace_root.is_none() || self.superseded_latched() {
            return false;
        }
        let scan_cutoff = self.observation();
        {
            let mut inner = lock_recover(&self.inner);
            // The same admission point as the reload slot, for the same reason.
            if self.stop.is_stopped() {
                return false;
            }
            if !matches!(inner.status, GraphStatus::Idle | GraphStatus::Failed(_)) {
                return false;
            }
            inner.build_ticker = None;
            inner.status = GraphStatus::Loading;
            let mut debt = lock_recover(&self.debt);
            let forced_through = debt.forced_fact();
            let mode = forced || forced_through.is_some() || debt.owes_recovery_build();
            let sponsors = debt.charge_admission(scan_cutoff, Instant::now(), mode);
            let recovery_cutoff = debt.capture_recovery();
            let ticket = super::debt::BuildTicket {
                claim: self.next_claim_id(),
                kind: BuildKind::Initial,
                forced: mode,
                scan_cutoff,
                forced_through,
                recovery_cutoff,
                sponsors,
            };
            drop(debt);
            inner.claimed = Some(ticket);
        }
        let state = self.clone();
        #[cfg(test)]
        if self.loader_cannot_spawn.load(Ordering::SeqCst) {
            self.record_admission_failure(FailureKind::Spawn, |inner| {
                inner.status =
                    GraphStatus::Failed("could not spawn loader: refused by test".to_owned());
            });
            // Claimed all the same: the attempt happened, and it failed where it stands.
            return true;
        }
        let spawned = std::thread::Builder::new()
            .name("bsl-graph-init".to_owned())
            .spawn(move || state.run_load(false));
        #[cfg(test)]
        if spawned.is_ok() {
            self.builders_started.fetch_add(1, Ordering::SeqCst);
        }
        if let Err(e) = spawned {
            self.record_admission_failure(FailureKind::Spawn, |inner| {
                inner.status = GraphStatus::Failed(format!("could not spawn loader: {e}"));
            });
        }
        true
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
        let scan_cutoff = self.observation();
        let mut inner = lock_recover(&self.inner);
        // The THIRD admission point, and it needs the stop for the same reason as the other
        // two: the boot reads the stop once, early, and then spends minutes opening the store
        // and indexing before it gets here. A stop landing in that window would otherwise
        // admit a whole cold build after the daemon had asked every owner to leave — and the
        // lease is released only after that asking, so ownership still reads true.
        if self.stop.is_stopped() {
            return false;
        }
        if inner.status != GraphStatus::Idle {
            return false;
        }
        inner.build_ticker = None;
        inner.status = GraphStatus::Loading;
        let mut debt = lock_recover(&self.debt);
        let forced_through = debt.forced_fact();
        let mode = forced_through.is_some() || debt.owes_recovery_build();
        let sponsors = debt.charge_admission(scan_cutoff, Instant::now(), mode);
        let recovery_cutoff = debt.capture_recovery();
        let ticket = super::debt::BuildTicket {
            claim: self.next_claim_id(),
            kind: BuildKind::Initial,
            forced: mode,
            scan_cutoff,
            forced_through,
            recovery_cutoff,
            sponsors,
        };
        drop(debt);
        inner.claimed = Some(ticket);
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
                observed_through: Some(self.observation()),
            },
            GraphStatus::Ready { files },
            None,
            None,
            // A test adapter, not a production ingress: it adopts a database somebody else
            // built, so it proves nothing about what that build read or walked.
            super::debt::RecoveryPublicationProof::without_coverage(generation),
        );
        assert!(matches!(outcome, crate::workspace_lease::LeaseOperationOutcome::Applied(())));
    }

    /// Abandon a claimed external build that did not produce a usable database, so the
    /// normal lazy/eager path can rebuild. Reverts `Loading → Idle`.
    pub(crate) fn abort_external_build(&self) {
        let mut inner = lock_recover(&self.inner);
        if inner.status == GraphStatus::Loading {
            inner.status = GraphStatus::Idle;
            inner.claimed = None;
            inner.build_ticker = None;
        }
    }

    /// Record a delivered change of the scan universe and decide what it is worth. The fact
    /// number is the hub position it was delivered under; a caller that has no batch of its
    /// own — the boot, a test — reads the hub's current position, which covers everything
    /// delivered so far.
    ///
    /// Kept as a name because it is what the consumer and the watcher call; what changed is
    /// that it records into one state and lets one decision act, instead of deciding here.
    #[cfg(test)]
    pub(crate) fn nudge_rebuild(&self) {
        self.record_change(self.observation());
    }

    /// Record a change no fingerprint comparison can answer: root aliases and declared
    /// spellings move search ownership without moving canonical graph topology, and a
    /// reconcile means the detail itself was lost.
    #[cfg(test)]
    pub(crate) fn nudge_project_reload(&self) {
        self.record_forced(self.observation());
    }

    /// Claim the single background-reload slot. `force` says the caller's debt cannot be
    /// answered by a fingerprint comparison, so the claim is owed whatever disk says.
    ///
    /// Returns why, not just whether: a claim that cannot be made because the lease could not
    /// be confirmed is a debt still owed, while one that finds disk unchanged is a debt
    /// answered — and the two must not read alike.
    fn try_claim_reload(&self, force: bool) -> ReloadClaim {
        #[cfg(test)]
        if self.claim_is_held.swap(false, Ordering::SeqCst) {
            return ReloadClaim::Held;
        }
        // Ahead of the fingerprint walk: a superseded daemon must not even pay for drift
        // detection it is not allowed to act on.
        if !self.lease.owns_caches_now() {
            return ReloadClaim::Held;
        }
        let disk_receipt = (!force).then(|| self.current_disk_fp_with_watermark()).flatten();
        let disk = disk_receipt.map(|(fingerprint, clean, _)| (fingerprint, clean));
        // Read BEFORE the slot is granted, never after: this is the cutoff the build's proof
        // may cover, and reading it early can only under-claim. `observation` takes the hub's
        // own lock, which is why it is not taken under `inner`.
        let scan_cutoff = self.observation();
        let mut inner = lock_recover(&self.inner);
        // THE admission point, and it is here rather than at the decision because the walk
        // above takes seconds on a large tree: a stop that lands inside it must still be
        // answered. Read under the same lock that grants the slot, and `OwnerStop::stop` writes
        // its atomic first of all, so the two are ordered against each other.
        if self.stop.is_stopped() {
            return ReloadClaim::Stopping;
        }
        let ready = matches!(inner.status, GraphStatus::Ready { .. });
        let Some(published) = inner.published.as_mut() else {
            return ReloadClaim::NotOwed { comparison: None };
        };
        if published.reload == ReloadState::Running {
            return ReloadClaim::Running;
        }
        if force || published.wants_reload(disk) {
            let before_claim = published.reload.clone();
            published.reload = ReloadState::Running;
            let mut debt = lock_recover(&self.debt);
            let forced_through = force.then(|| debt.forced_fact()).flatten();
            // The charge is part of the grant, not a step after it: a slot handed out without
            // spending what paid for it is a free attempt, and free attempts are what the
            // whole account exists to make impossible. It can REFUSE — the walk above takes
            // seconds, and the only sponsor's deadline can pass inside it.
            let sponsors = debt.charge_admission(scan_cutoff, Instant::now(), force);
            if !sponsors.any() {
                drop(debt);
                // Back exactly where it was. Reporting this as an ordinary "nothing to do"
                // retired a change this call never compared away and deleted the failure
                // account whose exhaustion IS the explanation — and left the slot reading
                // `none` over a reload that had failed.
                published.reload = before_claim;
                return ReloadClaim::Unsponsored;
            }
            // Captured in the same hold that grants the slot: every recovery origin measured
            // so far is what this build is admitted to answer, and one measured after it is
            // not.
            let recovery_cutoff = debt.capture_recovery();
            let ticket = super::debt::BuildTicket {
                claim: self.next_claim_id(),
                kind: BuildKind::Reload,
                forced: force,
                scan_cutoff,
                forced_through,
                recovery_cutoff,
                sponsors,
            };
            drop(debt);
            inner.claimed = Some(ticket);
            ReloadClaim::Claimed
        } else {
            let comparison = if !force
                && ready
                && !published.stale
                && !published.force_stale
                && disk_receipt.is_some_and(|(_, clean, _)| clean)
                && !lock_recover(&self.debt).owes_recovery()
            {
                disk_receipt.map(|(_, _, through)| ComparisonProof {
                    generation: published.generation,
                    fingerprint: published.fingerprint,
                    through,
                })
            } else {
                None
            };
            ReloadClaim::NotOwed { comparison }
        }
    }

    /// Spawn the background reload thread after a successful [`Self::try_claim_reload`].
    /// On spawn failure the reload slot is marked `Failed` so it is never left stuck
    /// `Running` (which would block every later reload claim).
    pub(super) fn spawn_reload(&self) {
        // The claim already happened, so declining here MUST give the slot back. A `Running`
        // slot with no thread behind it reads as a build in flight for ever: the decision sees
        // `in_flight` and returns early every time, so nothing retries, probes or flushes for
        // the rest of this generation, and the graph reports "catching up" on a snapshot
        // nothing will replace. Leaving is not a failure either, so no debt is recorded — the
        // debts stand as they were.
        if self.is_superseded() || self.stop.is_stopped() {
            self.release_reload_slot();
            return;
        }
        let state = self.clone();
        // The same seam the Initial path carries, on the path a published graph actually
        // takes. Without it the reload's spawn failure — the branch right below — had no test
        // that could reach it, and a stand that asked for one got an ordinary build instead.
        #[cfg(test)]
        if self.loader_cannot_spawn.load(Ordering::SeqCst) {
            self.record_load_failure(
                true,
                super::build::LoadFailure::refused("could not spawn reload: refused by test"),
            );
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("bsl-graph-reload".to_owned())
            .spawn(move || state.run_load(true));
        #[cfg(test)]
        if spawned.is_ok() {
            self.builders_started.fetch_add(1, Ordering::SeqCst);
        }
        if let Err(e) = spawned {
            self.record_load_failure(
                true,
                super::build::LoadFailure::refused(format!("could not spawn reload: {e}")),
            );
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
        // The boot does not wait for another process's reads: the loader waits for the file
        // on its own thread, and the search index is built standalone meanwhile.
        if self.ownership_refusal().is_some() || !self.acquire_graph_access(false) {
            self.ensure_loading();
            return FusedStartup::Standalone;
        }
        // Every read below is read-only and cannot roll back a journal an interrupted patch
        // left: unrecovered, the cached graph looks unreadable and the publication it carries
        // looks absent. A journal that will not recover is the loader's to report.
        if let Some(path) = self.graph_db_path() {
            if super::snapshot::recover_hot_journal(&path).is_err() {
                self.ensure_loading();
                return FusedStartup::Standalone;
            }
        }
        if !self.try_begin_external_build() {
            // A concurrent path (e.g. a graph tool call) already owns the build; index
            // the search engine the normal way against whatever graph it produces.
            return FusedStartup::Standalone;
        }
        // Read before any disk read below: see `observation`.
        let observed_through = self.observation();
        match self.try_publish_cached(&workspace_root, observed_through) {
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
        match self.run_fused_cold_build(engine, source_path, observed_through) {
            Ok(()) => FusedStartup::Fused,
            // Already recorded by the build itself, against the ticket it was carrying.
            Err(failure) => {
                tracing::warn!(
                    "fused cold-build failed; falling back to standalone index: {failure}"
                );
                FusedStartup::Standalone
            }
        }
    }
}

/// Let the graph file's access lock go once the store is retired and every read and use of the
/// file has come back. Says whether it is let go.
fn release_access(store: &GraphStore, access: &Mutex<GraphAccess>) -> bool {
    let mut access = lock_recover(access);
    if !matches!(*access, GraphAccess::Released) && store.retired_and_returned() {
        if matches!(*access, GraphAccess::Held(_)) {
            tracing::info!(
                "graph ownership passed on: reads finished, handles closed, access to the graph \
                 file released"
            );
        }
        *access = GraphAccess::Released;
    }
    matches!(*access, GraphAccess::Released)
}

/// How often a process waiting for the graph file tries its access lock again.
const ACCESS_RETRY: Duration = Duration::from_millis(250);
/// How often a process still waiting for the graph file says so in the log.
const ACCESS_WAIT_WARNING: Duration = Duration::from_secs(60);

/// This process's hold on the graph file's access lock.
pub(super) enum GraphAccess {
    NotHeld,
    /// The previous owner still holds the file.
    Waiting {
        since: Instant,
    },
    Held(
        #[allow(dead_code, reason = "held for its drop, which releases the lock")]
        crate::workspace_lease::ExclusiveFileLock,
    ),
    /// Ownership is gone; the lock was let go and is never taken again by this process.
    Released,
}

/// Context-dirty marks the search engine placed for renders the published graph could not
/// serve. The context provider reports them while the engine is held, so reporting only raises
/// a high-water and wakes the watcher; the watcher registers them with the graph, which
/// consumes them against a publication like any other mark.
#[derive(Clone)]
pub(crate) struct OwedContextMarks {
    high: Arc<std::sync::atomic::AtomicI64>,
    fact: Arc<AtomicU64>,
    alarms: Arc<AtomicUsize>,
    hub: Option<WorkspaceChangeHub>,
}

impl OwedContextMarks {
    pub(crate) fn record(&self, mark_high: i64) {
        // The search engine calls this while holding its own lock. Reading the hub's short-lived
        // accumulator mutex follows the same engine -> hub order as the existing wake below; it
        // never waits for graph publication or search-engine work.
        let fact = self.hub.as_ref().map_or(u64::MAX, WorkspaceChangeHub::seq);
        self.fact.fetch_max(fact, Ordering::SeqCst);
        self.high.fetch_max(mark_high, Ordering::SeqCst);
        self.alarms.fetch_add(1, Ordering::SeqCst);
        if let Some(hub) = &self.hub {
            hub.wake_waiters();
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
    #[cfg(unix)]
    use super::super::test_support::wait_until_driving;
    use super::super::test_support::{
        drive_until, published_report, sample_workspace, wait_ready, wait_until, wait_until_within,
    };
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A cold build has no publication to be stale against: answers from other caches
    /// stay fresh until a graph is installed and then rebuilt.
    #[test]
    fn only_a_published_graph_reports_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cold = GraphState::for_workspace(root.to_path_buf());
        cold.set_loading_for_test();
        assert_eq!(cold.status(), GraphStatus::Loading);
        assert!(!cold.is_reloading(), "a cold build is not a reload");

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(!graph.is_reloading());
        graph.set_loading_for_test();
        assert!(graph.is_reloading(), "loading over an installed publication is a reload");
    }

    /// A delivery is listed when the LEDGER has it, not when the consumer set out to make it.
    ///
    /// A stand that waits for a delivery is waiting to read what the delivery did. Listed on the
    /// way in, the wait ends while the ledger is still somebody else's — the reading that
    /// follows is then of a ledger the delivery has not reached, and a stand asserting "this
    /// loss was already acted on" would be asserting it against the state before the fact.
    #[test]
    fn a_quiet_delivery_is_listed_only_once_the_ledger_has_it() {
        use crate::change_hub::test_support::eventually;
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        let ledger = lock_recover(&graph.debt);

        let (calling, called) = std::sync::mpsc::channel();
        let delivering = {
            let graph = graph.clone();
            std::thread::spawn(move || {
                calling.send(()).unwrap();
                graph.record_loss_quietly(Some(7), 3);
            })
        };
        called.recv_timeout(Duration::from_secs(10)).expect("the delivery never set out");
        let listed_while_the_ledger_was_busy =
            eventually(Duration::from_secs(2), || !graph.quiet_loss_deliveries().is_empty());
        drop(ledger);
        delivering.join().expect("the delivering thread");

        assert!(
            !listed_while_the_ledger_was_busy,
            "a delivery was listed while the ledger it had not reached was held elsewhere",
        );
        assert_eq!(
            graph.quiet_loss_deliveries(),
            vec![Some(7)],
            "the delivery is listed once, under the identity it carried",
        );
        assert_eq!(graph.acted_losses(), vec![7], "and the ledger has exactly that loss");
    }

    /// Every retry owner in this workflow gets a deadline AND a bounded delay. The graph's
    /// delay cannot be slept — the retry rides on whatever call comes next — so it is held as
    /// an earliest-next instant. Without it the whole 600-second budget goes on restarting a
    /// rebuild that takes minutes, back to back.
    #[test]
    fn a_backed_off_failure_holds_the_next_rebuild_off() {
        let mut debt = GraphDebt::default();
        let facts = Facts { failed: true, owns: true, ..Facts::default() };
        let now = Instant::now();

        debt.record_failure(
            now,
            FailureKind::Transient,
            crate::graph::debt::Sponsors { primary: true, marks: false },
        );
        assert!(
            debt.decide(now, facts).start.is_some(),
            "the first retry after a refusal is not made to wait",
        );

        debt.record_failure(
            now,
            FailureKind::Transient,
            crate::graph::debt::Sponsors { primary: true, marks: false },
        );
        let held_off = debt.decide(now, facts).wake_at.expect("a refusal records the next start");
        assert_eq!(
            held_off.saturating_duration_since(now),
            crate::state::overlay_retry::retry_delay(1),
            "the delay must come from the refusal count, not from the questions asked about it",
        );
        assert!(held_off > now, "a repeated refusal restarted the rebuild at once");
        assert!(
            debt.decide(now, facts).start.is_none(),
            "the rebuild started before its own backoff elapsed",
        );
        assert!(
            debt.decide(held_off, facts).start.is_some(),
            "the budget is still open once the backoff elapsed",
        );
    }

    /// A change that arrives while the retry window is holding the next rebuild off is the one
    /// delivery of that change: the hub batch carrying it is acknowledged, and nothing else
    /// will hand it over again. Dropped here, the graph stays on the pre-edit revision and
    /// answers as if nothing were owed.
    #[test]
    fn a_change_that_arrives_while_the_retry_is_throttled_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        let now = Instant::now();
        {
            let mut debt = lock_recover(&graph.debt);
            debt.record_failure(
                now,
                FailureKind::Transient,
                crate::graph::debt::Sponsors { primary: true, marks: false },
            );
            debt.record_failure(
                now,
                FailureKind::Transient,
                crate::graph::debt::Sponsors { primary: true, marks: false },
            );
            let facts = Facts { failed: true, owns: true, ..Facts::default() };
            assert!(
                debt.decide(now, facts).start.is_none(),
                "the fixture needs a window that is holding off",
            );
        }
        lock_recover(&graph.inner).status = GraphStatus::Failed("forced".to_owned());

        graph.nudge_rebuild();

        assert!(
            graph.owes_change().is_some(),
            "the change was dropped, and nothing will deliver it again",
        );
        assert!(
            lock_recover(&graph.debt).stale(),
            "and a freshness check must see the rebuild is owed",
        );
    }

    /// The alarm is a projection of the standing, and every mutation of the standing wakes the
    /// watcher to re-read it. A hold is a mutation like any other — it moves the next decision
    /// to two seconds out — and it was the one exception: written from the publishing thread
    /// while the watcher slept on a thirty-second slice, it was not seen until that slice ran
    /// out. Enforced structurally, because the three sites are on three different paths and a
    /// fourth is one edit away.
    #[test]
    fn every_hold_wakes_the_watcher() {
        let source = include_str!("state.rs");
        let production = crate::inventory::production_source(source);
        let sites = hold_sites(&production);
        assert!(!sites.is_empty(), "the scan found no hold at all — it stopped discriminating");
        for site in &sites {
            assert!(
                site.contains("self.hold_decision(")
                    || site.starts_with("lock_recover(&self.debt).hold("),
                "a hold written outside `hold_decision` does not wake the watcher: {site}",
            );
        }
        assert_eq!(
            production.matches("fn hold_decision(").count(),
            1,
            "the one place a hold may be written",
        );
        assert_eq!(sites.len(), 1, "every hold goes through `hold_decision`: {sites:#?}");
    }

    /// The lines of `production` that write a hold: every call of `hold(`, whatever the
    /// receiver is spelled as. A needle on the receiver saw a hold written through a guard
    /// bound to another name as no hold at all.
    fn hold_sites(production: &str) -> Vec<String> {
        let code = mask_non_code(production);
        let needle = [".", "hold("].concat();
        code.match_indices(&needle)
            .map(|(at, _)| {
                let line_start = code[..at].rfind('\n').map_or(0, |nl| nl + 1);
                code[line_start..].lines().next().unwrap_or("").trim().to_owned()
            })
            .collect()
    }

    /// The hold gate is worth its scan only if a hold written any other way is a site it sees:
    /// through a bound guard, or on a debt reached by another name.
    #[test]
    fn the_hold_gate_sees_a_hold_written_through_a_bound_guard() {
        let decision = "    fn hold_decision(&self, now: Instant) {\n        \
                        lock_recover(&self.debt).hold(now);\n        \
                        self.wake_watcher();\n    }\n";
        assert_eq!(hold_sites(decision).len(), 1, "the one legitimate hold is a site");
        for (shape, extra) in [
            (
                "a bound guard",
                "        let mut debt = lock_recover(&self.debt);\n        debt.hold(now);\n",
            ),
            ("another graph", "        lock_recover(&graph.debt).hold(now);\n"),
        ] {
            let injected =
                format!("{decision}    fn quiet(&self, now: Instant) {{\n{extra}    }}\n");
            assert_eq!(
                hold_sites(&injected).len(),
                2,
                "a hold written through {shape} is invisible to the gate",
            );
        }
    }

    /// A graph whose first build failed transiently, with no watcher to ring its alarm and no
    /// search consumer to deliver a change, has exactly one executor left: the request that
    /// asks about it. Refusing there left the retry with an alarm nobody comes to — the graph
    /// answered `failed` and `stale` for ever over a workspace that would have built.
    ///
    /// What the request may NOT do is re-arm a spent budget; that is why this asks the
    /// schedule rather than the status.
    #[test]
    fn a_request_runs_a_retry_that_is_due_and_none_that_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        let now = Instant::now();

        // Due now: one transient failure retries at once.
        lock_recover(&graph.debt).record_failure(
            now,
            FailureKind::Transient,
            crate::graph::debt::Sponsors { primary: true, marks: false },
        );
        lock_recover(&graph.inner).status = GraphStatus::Failed("transient".to_owned());
        assert!(
            matches!(graph.debt_standing(now).failed, Some(crate::graph::debt::Ripeness::Now)),
            "the fixture needs a retry the schedule allows now",
        );
        graph.ensure_first_build();
        // The request ASKS, and the owner answers: with no watcher for this graph the ask is
        // carried by one short-lived kick, so the retry starts on that thread rather than on
        // this one. What it may not do is fail to start at all.
        wait_until(&graph, "the retry the request asked for to start", || {
            !matches!(graph.status(), GraphStatus::Failed(_))
        });
        assert!(
            matches!(
                lock_recover(&graph.inner).status,
                GraphStatus::Loading | GraphStatus::Ready { .. }
            ),
            "the request is the only executor left and its ask ran no retry: {:?}",
            graph.status(),
        );

        // Spent: an operation error stops the budget, and no number of requests re-opens it.
        let spent = GraphState::for_workspace(dir.path().to_path_buf());
        lock_recover(&spent.debt).record_failure(
            now,
            FailureKind::Operation,
            crate::graph::debt::Sponsors { primary: true, marks: false },
        );
        lock_recover(&spent.inner).status = GraphStatus::Failed("operation".to_owned());
        for _ in 0..8 {
            spent.ensure_first_build();
        }
        // Answered asynchronously now, so the statement is given time to become false before
        // it is believed: a spent budget must still be spent after every kick has run.
        assert!(
            !crate::change_hub::test_support::eventually(Duration::from_secs(2), || !matches!(
                lock_recover(&spent.inner).status,
                GraphStatus::Failed(_)
            )),
            "a request restarted a build the budget had already stopped: {:?}",
            spent.status(),
        );
    }

    /// A build is admitted for the facts that were on the table WHEN IT WAS ADMITTED. A fact
    /// delivered after that claim — while the builder is still on its way to the disk — is not
    /// one this build was paid to answer, and the proof it publishes does not cover it.
    ///
    /// The window is real and narrow: the claim grants the slot, and only then does the
    /// builder read the hub position it will publish. A live re-read there silently widens the
    /// proof to facts nobody funded, retires their credits, and leaves the next failure to be
    /// paid for by work that was never done.
    ///
    /// Barrier, not a sleep: `post_claim_hook` runs once, on the building thread, between the
    /// accepted claim and the pre-scan.
    #[test]
    fn post_claim_fact_survives_publication() {
        // Both the demand that BUYS the admission and the one delivered after it are varied:
        // a late Change, a late ProjectForced and late marks are three different mandates, and
        // a test that always injects the first proves nothing about the other two.
        for (lane, late) in
            [("change", "change"), ("forced", "forced"), ("forced", "marks"), ("change", "forced")]
        {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            let hub = crate::graph::test_support::workspace_hub(root);
            assert!(hub.wait_until_watching(Duration::from_secs(5)));

            let late_fact = Arc::new(AtomicI64::new(-1));
            let fired = Arc::new(AtomicUsize::new(0));
            let armed = Arc::new(AtomicBool::new(false));
            let barrier_armed = Arc::clone(&armed);
            let barrier_hub = hub.clone();
            let barrier_fact = Arc::clone(&late_fact);
            let barrier_fired = Arc::clone(&fired);
            let barrier_root = root.to_path_buf();
            let ticket_at_barrier = Arc::new(std::sync::Mutex::new(None));
            let barrier_ticket = Arc::clone(&ticket_at_barrier);
            // Sampled ON the publishing thread, in the window the publication opens — not by
            // polling for a generation. A later build legitimately answers the late work, and
            // a poll that arrives after it reads a state belonging to a different admission;
            // at ten milliseconds a turn it can also miss the moment entirely.
            //
            // Armed BEFORE the loader starts, because a clone taken earlier carries the hook
            // field it was cloned with: installed afterwards, the initial loader's own clone
            // has no hook, and the follow-up build it may start would publish outside the
            // window this test is watching. Which generation to sample is what arrives late.
            type PublicationSample = (Option<u64>, (Option<u64>, Option<u64>, bool));
            let sampled: Arc<std::sync::Mutex<Option<PublicationSample>>> =
                Arc::new(std::sync::Mutex::new(None));
            let barrier_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let window_graph: Arc<std::sync::Mutex<Option<GraphState>>> =
                Arc::new(std::sync::Mutex::new(None));
            let window = {
                let (sampled, window_graph, wanted) = (
                    Arc::clone(&sampled),
                    Arc::clone(&window_graph),
                    Arc::clone(&barrier_generation),
                );
                Arc::new(move || {
                    let wanted = wanted.load(Ordering::SeqCst);
                    if wanted == 0 {
                        return;
                    }
                    let Some(graph) = lock_recover(&window_graph).clone() else { return };
                    let inner = lock_recover(&graph.inner);
                    let Some(published) = inner.published.as_ref() else { return };
                    if published.generation != wanted {
                        return;
                    }
                    let observed = published.observed_through;
                    drop(inner);
                    let debt = lock_recover(&graph.debt);
                    let (change, forced) = (debt.owes_change(), debt.owes_forced());
                    drop(debt);
                    // The PLACEMENTS, not the obligation armed for them: the obligation is
                    // settled after this window, and the question here is whether this
                    // publication consumed marks it was never admitted to answer.
                    let owed = (change, forced, graph.marks_pending());
                    let mut slot = lock_recover(&sampled);
                    if slot.is_none() {
                        *slot = Some((observed, owed));
                    }
                }) as Arc<dyn Fn() + Send + Sync>
            };
            let graph = GraphState::for_workspace(root.to_path_buf())
                .with_change_hub(hub.clone())
                .with_publish_window_hook(window)
                .with_post_claim_hook(Arc::new(move |graph: &GraphState| {
                    if !barrier_armed.load(Ordering::SeqCst)
                        || barrier_fired.fetch_add(1, Ordering::SeqCst) > 0
                    {
                        return;
                    }
                    *barrier_ticket.lock().unwrap() = graph.claimed_ticket();
                    let observed = barrier_hub.seq();
                    super::super::test_support::write(
                        &barrier_root,
                        "CommonModules/Поздний/Ext/Module.bsl",
                        "Функция Поздняя() Экспорт Возврат 9; КонецФункции",
                    );
                    let seq =
                        crate::graph::test_support::wait_for_hub_seq_above(&barrier_hub, observed);
                    barrier_fact.store(seq as i64, Ordering::SeqCst);
                    match late {
                        "change" => graph.record_change_quietly(seq),
                        "forced" => graph.record_forced_quietly(seq),
                        _ => graph.marks_placed(11, seq),
                    }
                }));
            graph.set_watch(super::super::watcher::WatchPhase::Running, None);
            graph.ensure_loading();
            wait_ready(&graph);
            let first_generation =
                lock_recover(&graph.inner).published.as_ref().map(|p| p.generation).unwrap_or(0);
            barrier_generation.store(first_generation + 1, Ordering::SeqCst);
            *lock_recover(&window_graph) = Some(graph.clone());
            armed.store(true, Ordering::SeqCst);

            let observed = hub.seq();
            // A body-only BSL edit would be accepted by the incremental path, which
            // deliberately has no full-build post-claim barrier. Change the module's
            // XML execution context instead: this is a real unsupported local topology
            // delta and must enter the full-build admission this test observes.
            let descriptor = root.join("CommonModules/Сервер.xml");
            let xml = std::fs::read_to_string(&descriptor).unwrap();
            assert!(xml.contains("<Global>false</Global>"), "fixture starts non-global");
            super::super::test_support::write(
                root,
                "CommonModules/Сервер.xml",
                &xml.replace("<Global>false</Global>", "<Global>true</Global>"),
            );
            let admitted_fact = crate::graph::test_support::wait_for_hub_seq_above(&hub, observed);
            match lane {
                "change" => graph.record_change_quietly(admitted_fact),
                _ => graph.record_forced_quietly(admitted_fact),
            }

            let mut covered_by_barrier_build = None;
            let mut owed_at_that_publication = None;
            graph.drive();
            wait_until_within(
                &graph,
                Duration::from_secs(60),
                "the build admitted at the barrier to publish",
                || lock_recover(&sampled).is_some(),
            );
            if let Some((observed, owed)) = lock_recover(&sampled).take() {
                covered_by_barrier_build = Some(observed);
                owed_at_that_publication = Some(owed);
            }
            *lock_recover(&window_graph) = None;
            assert!(fired.load(Ordering::SeqCst) >= 1, "{lane}/{late}: the barrier never ran");
            let late_fact = late_fact.load(Ordering::SeqCst);
            assert!(late_fact >= 0, "{lane}/{late}: the barrier delivered nothing");
            let late_fact = late_fact as u64;

            // The mandate, as the admission fixed it.
            let ticket = ticket_at_barrier.lock().unwrap().expect("the barrier saw a ticket");
            assert_eq!(
                ticket.forced,
                lane == "forced",
                "{lane}/{late}: the mode must be the one the admission fixed, and a demand
                 arriving after it cannot change it",
            );
            assert!(
                ticket.scan_cutoff >= admitted_fact && ticket.scan_cutoff < late_fact,
                "{lane}/{late}: the cutoff must cover what was admitted and nothing after it",
            );
            assert!(ticket.claim > 0, "{lane}/{late}: every admission has an identity");
            assert!(
                ticket.sponsors.any(),
                "{lane}/{late}: an admission names the lanes that paid for it",
            );

            // The proof covers exactly its cutoff.
            let covered = covered_by_barrier_build
                .flatten()
                .expect("the build admitted at the barrier published a scan");
            assert!(
                covered < late_fact,
                "{lane}/{late}: the proof of the build admitted BEFORE fact {late_fact} claims \
                 to cover through {covered}",
            );

            // The late work is still owed, and it is what pays for the NEXT admission — once.
            let (change, forced, marks) =
                owed_at_that_publication.expect("the barrier build's own moment");
            // What the RESULT did not prove answered stays owed. A late ordinary change is the
            // one kind a following fingerprint comparison may legitimately answer — the file
            // was on disk before this build's scan read it, so the publication does describe
            // it — and the contract has never asked otherwise. A forced demand and placed
            // marks are exactly the kinds no comparison answers, which is why they are the
            // ones asserted here.
            if late != "change" {
                assert!(
                    forced.is_some_and(|fact| fact >= late_fact) || marks,
                    "{lane}/{late}: post-claim work no comparison can answer was retired \
                     anyway — change {change:?}, forced {forced:?}, marks {marks}",
                );
            }
            let before =
                lock_recover(&graph.debt).charge_admission(late_fact, Instant::now(), true);
            assert!(before.any(), "{lane}/{late}: the retained work financed no next claim");
            assert!(
                !lock_recover(&graph.debt).record_failure(
                    Instant::now(),
                    FailureKind::Operation,
                    before,
                ),
                "{lane}/{late}: the retained work financed its own failure as well as its claim",
            );
        }
    }

    /// A turn offers each hook revision once. The decision that carried it was taken before
    /// the comparison ran; running the SAME decision again afterwards fired the consumer's
    /// handler a second time for a revision it had already taken.
    #[test]
    fn one_comparison_turn_flushes_each_hook_revision_once() {
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
        let before = refreshes.load(Ordering::SeqCst);

        // A comparison that will answer without building — disk matches what is published —
        // standing beside a hook revision nobody has taken.
        graph.record_change(graph.observation());
        graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
        graph.drive();

        assert_eq!(
            refreshes.load(Ordering::SeqCst),
            before + 1,
            "the same hook revision was offered more than once in one turn",
        );
        assert!(!graph.hook_debt().topology, "and it was taken");
    }

    /// One offer has one owner — and the executor's own turn is where that was missing.
    ///
    /// The publish pass and the consumer's direct flush take the publication gate; the
    /// executor's turn took nothing, and claiming the offer only READ the revision and the
    /// mask. Two turns — the watcher's alarm and the thread a build finished on — therefore
    /// read the same revision and both fired the hook for it, and whoever replied second
    /// reported bits the first had already taken.
    #[test]
    fn two_executor_turns_offer_one_hook_revision_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let fires = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let hook = {
            let (fires, armed, parked, release) =
                (Arc::clone(&fires), Arc::clone(&armed), Arc::clone(&parked), Arc::clone(&release));
            Arc::new(move |signal: GraphPublishSignal| {
                if !armed.load(Ordering::SeqCst) || !signal.topology_changed {
                    return GraphPublishOutcome::HANDLED;
                }
                fires.fetch_add(1, Ordering::SeqCst);
                if !parked.swap(true, Ordering::SeqCst) {
                    // A barrier, not a sleep: the first flusher stays inside the hook until
                    // the second turn has run to completion and says so.
                    assert!(
                        crate::change_hub::test_support::eventually(
                            Duration::from_secs(30),
                            || release.load(Ordering::SeqCst)
                        ),
                        "the second turn never finished",
                    );
                }
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        armed.store(true, Ordering::SeqCst);

        graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
        let first = {
            let graph = graph.clone();
            std::thread::spawn(move || graph.drive())
        };
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(30), || parked
                .load(Ordering::SeqCst)),
            "the first turn never reached the hook",
        );

        // The second turn, while the first one's offer is still out.
        graph.drive();
        release.store(true, Ordering::SeqCst);
        first.join().expect("the parked turn finished");

        assert_eq!(
            fires.load(Ordering::SeqCst),
            1,
            "two executor turns fired the hook for one revision",
        );
    }

    /// The flush offers the mask it CLAIMED, never the one a decision handed it.
    ///
    /// The decision's mask is read before the turn owns anything, and a publication landing in
    /// between answers it. Offering that mask anyway tells the consumer its topology changed
    /// when the refresh it names has already run.
    #[test]
    fn an_offer_nobody_owes_is_not_made() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let fires = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let hook = {
            let (fires, armed) = (Arc::clone(&fires), Arc::clone(&armed));
            Arc::new(move |_: GraphPublishSignal| {
                if armed.load(Ordering::SeqCst) {
                    fires.fetch_add(1, Ordering::SeqCst);
                }
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        // Ready precedes the initial publish hook; count only offers after that pass.
        super::super::test_support::wait_publish_pass_within(
            &graph,
            super::super::test_support::WAIT_CEILING,
            1,
        );
        armed.store(true, Ordering::SeqCst);
        assert!(!graph.hook_debt().any(), "the fixture needs a graph that owes the hook nothing");

        graph.flush_hook_obligations();
        graph.drive();

        assert_eq!(fires.load(Ordering::SeqCst), 0, "an offer was made for a debt nobody owes",);

        // The control, so the assertion is not about a hook that never fires at all.
        graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
        graph.flush_hook_obligations();
        assert_eq!(fires.load(Ordering::SeqCst), 1, "the offer that IS owed was never made");
    }

    /// A revision raised while an offer is out is a NEW offer, and the reply carrying the old
    /// number may not answer it.
    ///
    /// The mask cannot tell the two apart — a second topology change sets the same bit — so
    /// only the revision can. Clearing that bit on the strength of a hook that took the
    /// PREVIOUS change loses the new obligation outright, and nothing else in the graph will
    /// ever raise it again.
    #[test]
    fn a_revision_raised_during_an_offer_survives_that_offers_reply() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let fires = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let hook = {
            let (fires, armed, parked, release) =
                (Arc::clone(&fires), Arc::clone(&armed), Arc::clone(&parked), Arc::clone(&release));
            Arc::new(move |signal: GraphPublishSignal| {
                if !armed.load(Ordering::SeqCst) || !signal.topology_changed {
                    return GraphPublishOutcome::HANDLED;
                }
                fires.fetch_add(1, Ordering::SeqCst);
                if !parked.swap(true, Ordering::SeqCst) {
                    assert!(
                        crate::change_hub::test_support::eventually(
                            Duration::from_secs(30),
                            || release.load(Ordering::SeqCst)
                        ),
                        "the new revision was never raised",
                    );
                }
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        armed.store(true, Ordering::SeqCst);

        graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
        let flushing = {
            let graph = graph.clone();
            std::thread::spawn(move || graph.drive())
        };
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(30), || parked
                .load(Ordering::SeqCst)),
            "the flush never reached the hook",
        );

        // A SECOND topology change, while the first offer is still out with the hook.
        graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
        release.store(true, Ordering::SeqCst);
        flushing.join().expect("the parked flush finished");

        assert_eq!(
            fires.load(Ordering::SeqCst),
            2,
            "the reply to the old offer cleared the change raised after it, and the new \
             revision was never offered at all",
        );
        assert!(!graph.hook_debt().topology, "and the new revision was answered in its turn");
    }

    /// A hook that drives the graph back is not offered the same revision again — and does not
    /// hang waiting for the offer it is itself holding.
    ///
    /// Reachable: the hook belongs to the search engine, and an engine that publishes while it
    /// refreshes drives the graph from inside its own callback. Whoever owns the offer must
    /// therefore own it without blocking, or this entry is a self-deadlock rather than a
    /// duplicate fire.
    #[test]
    fn a_hook_that_drives_the_graph_back_is_not_offered_the_same_revision_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let fires = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let reentered = Arc::new(AtomicBool::new(false));
        let slot: Arc<Mutex<Option<GraphState>>> = Arc::new(Mutex::new(None));
        let hook = {
            let (fires, armed, reentered, slot) =
                (Arc::clone(&fires), Arc::clone(&armed), Arc::clone(&reentered), Arc::clone(&slot));
            Arc::new(move |signal: GraphPublishSignal| {
                if !armed.load(Ordering::SeqCst) || !signal.topology_changed {
                    return GraphPublishOutcome::HANDLED;
                }
                fires.fetch_add(1, Ordering::SeqCst);
                if !reentered.swap(true, Ordering::SeqCst) {
                    let graph = lock_recover(&slot).clone();
                    if let Some(graph) = graph {
                        graph.drive();
                    }
                }
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        *lock_recover(&slot) = Some(graph.clone());
        armed.store(true, Ordering::SeqCst);

        graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
        graph.drive();

        assert_eq!(
            fires.load(Ordering::SeqCst),
            1,
            "the hook was offered the revision it was already answering",
        );
        // The graph is held by its own hook; drop that edge before the test ends.
        *lock_recover(&slot) = None;
    }

    /// A publication whose hook refuses everything it was offered earns the same finite pause
    /// an executor's refusal earns.
    ///
    /// Without it the refusal is free: the pass records what the hook would not take, the
    /// executor it drives at the end finds that debt ripe NOW, and the same consumer that has
    /// just said no is asked again in the same breath.
    #[test]
    fn a_publication_whose_hook_refuses_paces_the_next_offer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let fires = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let hook = {
            let (fires, armed) = (Arc::clone(&fires), Arc::clone(&armed));
            Arc::new(move |_: GraphPublishSignal| {
                if !armed.load(Ordering::SeqCst) {
                    return GraphPublishOutcome::HANDLED;
                }
                fires.fetch_add(1, Ordering::SeqCst);
                GraphPublishOutcome { topology_handled: false, roots_handled: false }
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        armed.store(true, Ordering::SeqCst);

        graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
        graph.drive();
        graph.drive();
        assert_eq!(
            fires.load(Ordering::SeqCst),
            1,
            "the executor asked a refusing hook again inside its own pause",
        );

        // The publication makes its own offer — that one is legitimate — and its refusal must
        // leave the same pause behind.
        graph.notify_published(false);
        assert_eq!(
            fires.load(Ordering::SeqCst),
            2,
            "the publication's refusal was free: the executor it drives asked again at once",
        );
    }

    /// A fact delivered while a comparison is walking is ripe the instant that comparison
    /// answers. A debt that is ripe NOW names no moment — it is the executor's turn, not the
    /// alarm's — so a runner that stops after one action leaves it for the owner's next full
    /// slice. The turn either runs it or latches it; it never simply drops it.
    ///
    /// The second fact here is a NUMBER recorded straight into the ledger, not a delivery: what
    /// it pins is the arithmetic of the cutoff. The test below it does the same thing with a
    /// real write, a real hub and the watcher's own drain, and neither stands in for the other.
    #[test]
    fn a_new_fact_during_comparison_runs_before_idle_wait() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        graph.set_watch(super::super::watcher::WatchPhase::Running, None);
        graph.ensure_loading();
        wait_ready(&graph);

        // Two facts, the second above the first: the comparison answers only what its cutoff
        // covered, and the rest is owed the moment it returns.
        let first = graph.observation();
        graph.record_change_quietly(first);
        lock_recover(&graph.debt).record_change(Instant::now(), first + 5);
        graph.drive();

        // The fact above the cutoff is still owed — a comparison answers what its own walk
        // covered and no more. Accepting "not owed" as one way to pass made the first disjunct
        // true in exactly the case this test exists to catch: the fact dropped, with nothing
        // left to run it and nothing left to name it.
        let owed = graph.owes_change();
        assert_eq!(
            owed,
            Some(first + 5),
            "the comparison answered a fact its own cutoff never covered",
        );
        assert!(
            graph.take_continuation() || graph.wake_at(Instant::now()).is_some(),
            "a fact left ripe after the comparison has neither an executor nor an alarm: \
             owed {owed:?}",
        );
    }

    /// A change delivered INSIDE a comparison is run, not slept through.
    ///
    /// The comparison answers the position it read before it started; a fact the watcher
    /// delivers while it walks disk is above that line. Injected as a number before the drive,
    /// the same assertion passes on a graph that never looked at all — so the change is
    /// written to disk here, delivered by the real hub, and recorded through the path the
    /// watcher's drain uses, all inside that window.
    #[test]
    fn a_real_fact_delivered_inside_a_comparison_runs_before_any_idle_wait() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));

        let delivered = Arc::new(AtomicI64::new(-1));
        let walks_at_delivery = Arc::new(AtomicUsize::new(0));
        let hook = {
            let (delivered, walks, hub, root) = (
                Arc::clone(&delivered),
                Arc::clone(&walks_at_delivery),
                hub.clone(),
                root.to_path_buf(),
            );
            Arc::new(move |graph: &GraphState| {
                if delivered.load(Ordering::SeqCst) >= 0 {
                    return;
                }
                let floor = graph.observation();
                walks.store(graph.scan_count(), Ordering::SeqCst);
                // Delivered by the watcher and deliberately NOT part of the scanned universe:
                // this comparison will find disk unchanged and answer, which is the moment a
                // fact above its cutoff can be answered along with it.
                super::super::test_support::write(
                    &root,
                    "CommonModules/Сервер/Ext/Module.bsl.tmp",
                    "editor swap file",
                );
                // Bounded, and never an assertion from inside the graph's own thread: a
                // barrier that panics here unwinds through the executor being tested.
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    let seq = hub.seq();
                    if seq > floor {
                        // Exactly what the watcher's drain does with a delivered batch.
                        graph.record_change_quietly(seq);
                        delivered.store(seq as i64, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }) as LatchWindowHook
        };
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_change_hub(hub.clone())
            .with_comparison_window_hook(hook);
        graph.set_watch(super::super::watcher::WatchPhase::Running, None);
        graph.ensure_loading();
        wait_ready(&graph);

        // A real post-publication fact makes a comparison owed, and running it is what opens
        // the window above. The file is outside the scan universe: it changes the hub frontier
        // without changing the build fingerprint.
        let watermark = graph
            .consuming_observation()
            .expect("the fixture needs a publication that observed the fact stream");
        let issued_after = hub.seq().max(watermark);
        std::fs::write(root.join("unscanned.txt"), "a change no scan reads").unwrap();
        let fact = super::super::test_support::wait_for_hub_seq_above(&hub, issued_after);
        graph.record_change_quietly(fact);
        graph.drive();

        let late = delivered.load(Ordering::SeqCst);
        assert!(late >= 0, "the barrier never had a change delivered to it");
        let late = late as u64;

        // Three ways that fact can be in hand, and one way it can be lost. Lost is: nobody
        // owes it, no publication covered it, and nothing looked at disk after it arrived —
        // which is a change slept through, whatever the schedule says afterwards.
        let covered = lock_recover(&graph.inner)
            .published
            .as_ref()
            .and_then(|published| published.observed_through)
            .is_some_and(|observed| observed >= late);
        let owed = graph.owes_change();
        let facts = graph.facts();
        let decision = lock_recover(&graph.debt).decide(Instant::now(), facts);
        let walked_since = graph.scan_count() > walks_at_delivery.load(Ordering::SeqCst);
        assert!(
            covered
                || (owed.is_some_and(|owed| owed >= late)
                    && (facts.in_flight || decision.does_work() || graph.take_continuation()))
                || (owed.is_none() && walked_since),
            "a fact delivered inside the comparison was neither covered, nor owned, nor \
             answered by a look that happened after it: late {late}, owed {owed:?}, covered \
             {covered}, walked since {walked_since}, decision {decision:?}",
        );
    }

    /// A change delivered while the comparison was walking is not answered by that comparison.
    ///
    /// The window is inside the walk: the position this look may vouch for has been read, the
    /// tree has been enumerated, and only then does the change land. This look cannot have
    /// seen it — its fingerprint is the one it just computed — so answering it with a number
    /// read at the verdict retires a real edit whose only record is that debt.
    ///
    /// Exactly ONE comparison runs here, by calling it rather than driving: a second, later
    /// comparison would legitimately cover the same fact and hide whether the first one had
    /// any business answering it.
    #[test]
    fn a_change_delivered_during_the_walk_outlives_that_comparison() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));

        let delivered = Arc::new(AtomicI64::new(-1));
        let hook = {
            let (delivered, hub, root) = (Arc::clone(&delivered), hub.clone(), root.to_path_buf());
            Arc::new(move |graph: &GraphState| {
                if delivered.load(Ordering::SeqCst) >= 0 {
                    return;
                }
                // Read BEFORE the edit is issued, and never below what the walk has already
                // covered: a position the hub reaches afterwards is a fact this walk could
                // not have enumerated. Which entry carries it is the watcher's business —
                // the fixture's oracles below do not depend on that one.
                let floor = hub.seq().max(graph.observation());
                super::super::test_support::write(
                    &root,
                    "CommonModules/Сервер/Ext/Module.bsl",
                    "&НаСервере\nФункция Считать() Экспорт Возврат 42; КонецФункции",
                );
                // Bounded, and never an assertion from inside the graph's own walk: a barrier
                // that panics here unwinds through the code under test.
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    let seq = hub.seq();
                    if seq > floor {
                        graph.record_change_quietly(seq);
                        delivered.store(seq as i64, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }) as LatchWindowHook
        };
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_change_hub(hub.clone())
            .with_scan_window_hook(hook);
        graph.set_watch(super::super::watcher::WatchPhase::Running, None);
        graph.ensure_loading();
        wait_ready(&graph);

        // A comparison is owed, and running it is what opens the window above.
        //
        // Owed only by a fact the publication has not answered: a delivery at or below its own
        // watermark is a repeat, and recording one records nothing. Whether the watcher has
        // already delivered anything above it is its own timing, so the fixture makes such a
        // change itself — in a file no scan reads, so the fingerprint is untouched and the
        // edit inside the walk stays the only change a comparison could see — and waits for
        // the hub to report a position past it.
        let watermark = graph
            .consuming_observation()
            .expect("the fixture needs a publication that observed the fact stream");
        let issued_after = hub.seq().max(watermark);
        std::fs::write(root.join("unscanned.txt"), "a change no scan reads").unwrap();
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(10), || hub.seq()
                > issued_after),
            "the hub never reported the change this fixture made",
        );
        graph.record_change_quietly(graph.observation());
        let owed_before = graph.owes_change().expect("the comparison this test runs is owed");
        graph.check_against_disk();

        let late = delivered.load(Ordering::SeqCst);
        assert!(late >= 0, "the barrier never had a change delivered into the walk");
        let late = late as u64;
        assert!(late > owed_before, "the fixture needs the late fact above the one being answered");

        let owed = graph.owes_change();
        assert_eq!(
            owed,
            Some(late),
            "a comparison answered an edit that landed after its own walk had enumerated the \
             tree — the debt was the only record of it",
        );
        // And the edit really is on disk, unseen by the publication that stands.
        let published = lock_recover(&graph.inner)
            .published
            .as_ref()
            .map(|published| published.fingerprint)
            .expect("the build published");
        // Asked of the tree, not of the throttled answer the comparison just cached: that one
        // is only refreshed when the watcher's delivery happens to have landed first, so
        // reading it here would make this fixture's premise a race against the watcher.
        graph.forget_the_disk_look();
        let (disk, _) = graph.current_disk_fp().expect("a workspace walks");
        assert_ne!(
            disk, published,
            "the fixture needs an edit the published graph does not describe",
        );
    }

    /// The graph's own owners — the boot, the executor, a request and the alarm — run over one
    /// graph at once and every one of them finishes.
    ///
    /// A lock cycle does not fail, it HANGS, and a suite that hangs reports nothing at all —
    /// the live one took thirty-six minutes of a commit hook at zero CPU before anybody
    /// looked. So the work runs on children and this thread only waits, with a deadline: a
    /// cycle reachable from these four owners is then a failed assertion that names it,
    /// instead of a run that never ends.
    ///
    /// A child that never returns is left where it is rather than joined. It owns nothing
    /// outside this test — its own graph over its own workspace — and joining it would turn
    /// the hang being watched for into the hung suite this exists to prevent.
    ///
    /// What this is NOT: a reproducer of the historic inversion on the boot claim. That one
    /// needs the two critical sections to overlap, and no schedule here forces them to; it is
    /// held by the structural gate below, which is exhaustive over the module, and by the same
    /// deadline around the integration run that first exposed it.
    #[test]
    fn the_graphs_own_owners_all_finish_within_a_watchdog() {
        let dir = tempfile::tempdir().unwrap();
        sample_workspace(dir.path());
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        // Ready, because the claim is the other half of the pair: it holds the lifecycle lock
        // ACROSS taking the debt, and a boot claim holding the debt across reading the
        // lifecycle is what closes the cycle. An idle graph never reaches it.
        graph.ensure_loading();
        wait_ready(&graph);
        let (tx, rx) = std::sync::mpsc::channel();
        let owners: Vec<_> = (0..4)
            .map(|owner| {
                let graph = graph.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for _ in 0..40 {
                        match owner {
                            // The boot's claim: the one that asks the schedule for its mode.
                            0 => graph.ensure_loading(),
                            // A reload claim, over and over: forced work and the executor's
                            // turn that takes it.
                            1 => {
                                lock_recover(&graph.debt)
                                    .record_forced(Instant::now(), graph.observation());
                                graph.drive();
                            }
                            // A request, which reads the lifecycle.
                            2 => drop(graph.status()),
                            // The alarm, which reads the debt.
                            _ => drop(graph.wake_at(Instant::now())),
                        }
                    }
                    let _ = tx.send(owner);
                })
            })
            .collect();
        drop(tx);

        let deadline = Instant::now() + Duration::from_secs(90);
        for done in 0..owners.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero() && rx.recv_timeout(left).is_ok(),
                "the graph's owners deadlocked: {done} of {} finished",
                owners.len(),
            );
        }
        for owner in owners {
            owner.join().expect("every owner that reported in also returned");
        }
    }

    /// The lock order of this module is `inner` → `debt`, everywhere, and it is not a
    /// preference: `facts()` reads `inner`, the claim takes `inner` and then `debt`, and a
    /// single place taking them the other way round deadlocks the graph outright. One did —
    /// `lock_recover(&self.debt).decide(now, self.facts())` evaluates the guard first — and it
    /// was found only because a whole integration suite hung.
    ///
    /// Structural, because the failure is a hang: a test that exercises it either passes or
    /// never returns, and "never returns" is what a gate has to stop being possible.
    #[test]
    fn the_debt_lock_is_never_taken_before_the_lifecycle_lock() {
        let production = crate::inventory::production_source(include_str!("state.rs"));
        let offenders = lifecycle_locks_taken_under_the_debt(&production);
        assert!(
            offenders.is_empty(),
            "the debt guard is held while the lifecycle lock is taken — the order every other \
             path uses is the reverse, and mixing them deadlocks:\n{offenders:#?}",
        );
    }

    /// Where a debt guard is alive while `facts()` or the lifecycle lock is taken.
    ///
    /// For a guard used in one expression that is the expression; for a BOUND one it is the
    /// whole life of the binding — to its `drop`, or to the end of the block it was taken in.
    /// Stopping at the first semicolon, as this did, read only the `let` line and saw nothing
    /// of the very shape the order exists to forbid: a guard bound here and a lifecycle lock
    /// taken three lines below it.
    fn lifecycle_locks_taken_under_the_debt(production: &str) -> Vec<String> {
        const NEEDLE: &str = "lock_recover(&self.debt)";
        let source = mask_non_code(production);
        let mut offenders = Vec::new();
        let mut at = 0usize;
        while let Some(found) = source[at..].find(NEEDLE) {
            let start = at + found;
            let statement_start = source[..start].rfind('\n').map_or(0, |line| line + 1);
            let tail = &source[start..];
            let statement_end = tail.find(";\n").map_or(tail.len(), |stop| stop + 1);
            let bound = source[statement_start..start]
                .trim_start()
                .strip_prefix("let ")
                .map(|rest| rest.trim_start().trim_start_matches("mut ").trim_start())
                .and_then(|rest| rest.split_once('=').map(|(name, _)| name.trim()))
                .filter(|name| {
                    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                });
            // Either way the guard cannot outlive the block it was taken in — and an unbound
            // one in a TAIL expression has no semicolon at all, so without that bound the slice
            // ran on into the next function and read its locks as this one's.
            let block_end = end_of_block(tail);
            let region_end = match bound {
                // A bound guard lives until it is dropped, or until that block ends.
                Some(name) => {
                    let after = &tail[statement_end..];
                    let ends = end_of_block(after);
                    statement_end + after.find(&format!("drop({name})")).unwrap_or(ends).min(ends)
                }
                None => statement_end,
            }
            .min(block_end);
            let region = &tail[..region_end];
            if region.contains("self.facts()") || region.contains("lock_recover(&self.inner)") {
                let line = source[..start].matches('\n').count() + 1;
                offenders.push(format!("line {line}: {}", region.replace('\n', " ")));
            }
            at = start + NEEDLE.len();
        }
        offenders
    }

    /// The offset of the `}` that closes the block this text is inside.
    fn end_of_block(source: &str) -> usize {
        let mut depth = 0usize;
        for (at, ch) in source.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' if depth == 0 => return at,
                '}' => depth -= 1,
                _ => {}
            }
        }
        source.len()
    }

    /// `source` with the inside of every comment and literal blanked, lines kept.
    ///
    /// Comments carry the names these gates look for — this is a module about lock order — and
    /// a comment is not a lock; a brace inside a string, a character or a raw string does not
    /// end the block a guard lives in. Not a Rust parser: just enough of the lexer to know
    /// where code is — nested block comments, escapes, raw strings of any `#` depth, byte
    /// literals, and a lifetime told apart from a character.
    fn mask_non_code(source: &str) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let blank = |out: &mut String, ch: char| out.push(if ch == '\n' { '\n' } else { ' ' });
        let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            let next = chars.get(i + 1).copied();
            if ch == '/' && next == Some('/') {
                while i < chars.len() && chars[i] != '\n' {
                    blank(&mut out, chars[i]);
                    i += 1;
                }
            } else if ch == '/' && next == Some('*') {
                let mut depth = 0usize;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        blank(&mut out, chars[i]);
                        blank(&mut out, '*');
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        blank(&mut out, '*');
                        blank(&mut out, '/');
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        blank(&mut out, chars[i]);
                        i += 1;
                    }
                }
            } else if (ch == 'r' || ch == 'b') && !(i > 0 && ident(chars[i - 1])) && {
                let mut j = i + 1;
                if ch == 'b' && chars.get(j) == Some(&'r') {
                    j += 1;
                }
                let raw = ch == 'r' || j > i + 1;
                while raw && chars.get(j) == Some(&'#') {
                    j += 1;
                }
                raw && chars.get(j) == Some(&'"')
            } {
                let mut j = i + 1;
                if ch == 'b' {
                    j += 1;
                }
                let mut hashes = 0;
                while chars.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                j += 1;
                loop {
                    if j >= chars.len() {
                        break;
                    }
                    if chars[j] == '"' && (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')) {
                        j += 1 + hashes;
                        break;
                    }
                    j += 1;
                }
                for &c in &chars[i..j.min(chars.len())] {
                    blank(&mut out, c);
                }
                i = j;
            } else if ch == '"'
                || (ch == 'b' && next == Some('"') && !(i > 0 && ident(chars[i - 1])))
            {
                let mut j = if ch == 'b' { i + 2 } else { i + 1 };
                while j < chars.len() && chars[j] != '"' {
                    j += if chars[j] == '\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(chars.len());
                for &c in &chars[i..end] {
                    blank(&mut out, c);
                }
                i = end;
            } else if ch == '\''
                || (ch == 'b' && next == Some('\'') && !(i > 0 && ident(chars[i - 1])))
            {
                let start = if ch == 'b' { i + 1 } else { i };
                let close = match chars.get(start + 1) {
                    Some('\\') => (start + 3..chars.len()).find(|&k| chars[k] == '\''),
                    Some(_) if chars.get(start + 2) == Some(&'\'') => Some(start + 2),
                    _ => None,
                };
                match close {
                    Some(end) => {
                        for &c in &chars[i..=end] {
                            blank(&mut out, c);
                        }
                        i = end + 1;
                    }
                    // A lifetime or a label: code, left as it is.
                    None => {
                        out.push(ch);
                        i += 1;
                    }
                }
            } else {
                out.push(ch);
                i += 1;
            }
        }
        out
    }

    /// The gate is worth its scan only if it can fail, and on the shape that slipped past it:
    /// a debt guard BOUND, and the lifecycle read under it further down the block.
    #[test]
    fn the_lock_order_gate_sees_a_lifecycle_read_under_a_bound_debt_guard() {
        let injected = "    fn inverted(&self) {\n        \
                        let mut debt = lock_recover(&self.debt);\n        \
                        let facts = self.facts();\n        \
                        debt.decide(Instant::now(), facts);\n    }\n";
        let offenders = lifecycle_locks_taken_under_the_debt(injected);
        assert_eq!(offenders.len(), 1, "a bound debt guard over a lifecycle read is invisible");

        // And the shape that is NOT an inversion: the guard is given back first.
        let released = "    fn ordered(&self) {\n        \
                        let mut debt = lock_recover(&self.debt);\n        \
                        debt.abandon();\n        \
                        drop(debt);\n        \
                        let _ = self.facts();\n    }\n";
        assert!(
            lifecycle_locks_taken_under_the_debt(released).is_empty(),
            "a lifecycle read after the guard was dropped is not an inversion",
        );
    }

    /// The lock-order gate reads code, not the text inside literals and comments: a brace in a
    /// string, a character, a raw string or a block comment does not end the block a guard
    /// lives in, and a lifetime is not a character literal.
    #[test]
    fn the_lock_order_gate_is_not_fooled_by_braces_in_literals_or_comments() {
        for (shape, noise) in [
            ("a string", "let s = \"a } b\";"),
            ("an escaped string", "let s = \"q\\\" } \";"),
            ("a character", "let c = '}';"),
            ("a raw string", "let s = r#\"a } \"} b\"#;"),
            ("a byte string", "let s = b\"}\";"),
            ("a block comment", "/* } */"),
            ("a nested block comment", "/* /* } */ } */"),
            ("a line comment", "// }"),
        ] {
            let injected = format!(
                "    fn inverted<'a>(&'a self) {{\n        \
                 let mut debt = lock_recover(&self.debt);\n        \
                 {noise}\n        \
                 let facts = self.facts();\n        \
                 debt.decide(Instant::now(), facts);\n    }}\n"
            );
            assert_eq!(
                lifecycle_locks_taken_under_the_debt(&injected).len(),
                1,
                "a brace in {shape} hid the inversion from the gate",
            );
        }
        // And text that only NAMES the locks is not a lock.
        let named = "    fn named(&self) {\n        \
                     let mut debt = lock_recover(&self.debt);\n        \
                     let s = \"self.facts()\";\n        \
                     /* lock_recover(&self.inner) */\n    }\n";
        assert!(
            lifecycle_locks_taken_under_the_debt(named).is_empty(),
            "a lock named inside a literal or a comment was read as taken",
        );
    }

    /// Nesting a held first build is refused where it starts, not discovered by the outer
    /// boot as a missing reservation.
    #[test]
    #[should_panic(expected = "does not nest")]
    fn holding_the_first_build_refuses_to_nest() {
        let _ = super::super::test_support::holding_the_first_build(|| {
            super::super::test_support::holding_the_first_build(|| ())
        });
    }

    /// BUD-03. The mode the DECISION fixed reaches the ticket on the Initial path too. Marks
    /// owed on an idle or failed graph make its first build a forced one, and dropping that to
    /// an ordinary build lets a cached graph answer a same-stat edit — the one thing marks
    /// exist to prevent.
    #[test]
    fn an_initial_claim_keeps_the_marks_sponsored_mode() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // The ticket is read on the builder's own thread, before it takes it: a refused spawn
        // gives its grant back with the failure, so there is nothing left in the slot to read.
        let admitted: Arc<Mutex<Option<super::super::debt::BuildTicket>>> =
            Arc::new(Mutex::new(None));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_post_claim_hook({
            let admitted = Arc::clone(&admitted);
            Arc::new(move |graph: &GraphState| {
                lock_recover(&admitted).get_or_insert_with(|| {
                    graph.claimed_ticket().expect("the admission issued a ticket")
                });
            })
        });
        // Idle, with marks owed and no forced fact at all.
        let now = Instant::now();
        {
            let mut debt = lock_recover(&graph.debt);
            debt.place_marks(now, 5, 1);
            debt.settle_marks(now, false);
            assert!(debt.owes_forced_fact().is_none(), "the stand needs no forced fact");
        }
        let start = lock_recover(&graph.debt)
            .decide(
                now + crate::graph::debt::OWED_MARKS_GRACE,
                Facts { idle: true, owns: true, ..Facts::default() },
            )
            .start
            .expect("the marks owe a first build");
        assert!(start.forced, "the stand needs a marks-sponsored forced decision");

        // Claim it the way the executor does — through the real Initial admission point — and
        // read back what that admission fixed.
        assert!(graph.ensure_loading_claimed(start.forced), "the claim must be granted");
        wait_until(&graph, "the builder to reach its mandate", || {
            lock_recover(&admitted).is_some()
        });
        let ticket = lock_recover(&admitted).expect("the admission issued a ticket");
        assert!(
            ticket.forced,
            "the Initial admission dropped the mode the decision fixed: {ticket:?}",
        );
        assert!(ticket.sponsors.marks, "and it must name the lane that paid");
        wait_ready(&graph);
    }

    /// E1. A consumer may refuse for as long as it lives — a search engine that never came up
    /// refuses every time — and the executor must not ask it again on every turn. A refusal is
    /// not an action, it earns a finite pause, and a turn that spends its whole quantum on one
    /// hands the owner a zero-length wait and comes straight back.
    #[test]
    fn a_refusing_hook_is_paced_and_does_not_spin_the_executor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let offers = Arc::new(AtomicUsize::new(0));
        let hook = {
            let offers = Arc::clone(&offers);
            Arc::new(move |_signal: GraphPublishSignal| {
                offers.fetch_add(1, Ordering::SeqCst);
                // The real shape of a refusal: nothing taken.
                GraphPublishOutcome { topology_handled: false, roots_handled: false }
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        // Ready precedes the initial hook's refusal and the pause it records.
        super::super::test_support::wait_publish_pass_within(
            &graph,
            super::super::test_support::WAIT_CEILING,
            1,
        );
        let before = offers.load(Ordering::SeqCst);

        graph.record_hook_debt(HookDebt { topology: true, roots: true, marks: false });
        graph.drive();

        let asked = offers.load(Ordering::SeqCst) - before;
        assert_eq!(asked, 1, "one turn offered a refusing hook {asked} times");
        assert!(
            !graph.take_continuation(),
            "a refusal latched a continuation, so the owner comes back with a zero wait",
        );
        assert!(graph.hook_debt().any(), "and the work is still owed");

        // Asked again at once: still paced, not offered.
        graph.drive();
        assert_eq!(offers.load(Ordering::SeqCst) - before, 1, "the refusal earned no pause at all",);
    }

    /// O1. The level the watcher reports is not authority, in either order. The alias fixed
    /// first was `failure → observe`; this is `observe while Loading → failure`, which the
    /// production bootstrap reaches because it starts the watcher before the first build and
    /// does not wait for its first observation.
    #[test]
    fn an_observed_level_is_not_authority_for_a_later_failure() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        let now = Instant::now();

        // A first build is admitted at the level the hub stood at.
        lock_recover(&graph.debt).charge_admission(7, now, false);
        lock_recover(&graph.inner).status = GraphStatus::Loading;

        // The hub has moved on for a write this graph does not scan, and the watcher's first
        // observation reports where the stream stands.
        graph.observe_current_level(12);

        // That build then fails for good.
        graph.record_failure(FailureKind::Operation);
        lock_recover(&graph.inner).status = GraphStatus::Failed("operation".to_owned());

        let standing = graph.debt_standing(Instant::now()).failed;
        assert!(
            matches!(standing, Some(crate::graph::debt::Ripeness::Exhausted(_))),
            "an observation financed a retry epoch: {standing:?}",
        );
    }

    /// A stopped budget waits for fresh external work — a delivered change — and a request is
    /// not one. Every `graph` tool call reaches the request path, so a request that re-armed
    /// the budget would hand a workspace that cannot build a new epoch per call.
    #[test]
    fn a_request_spends_neither_the_budget_nor_a_debt() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        let now = Instant::now();
        lock_recover(&graph.debt).record_failure(
            now,
            FailureKind::Operation,
            crate::graph::debt::Sponsors { primary: true, marks: false },
        );
        lock_recover(&graph.inner).status = GraphStatus::Ready { files: 1 };
        let facts = Facts { ready: true, owns: true, ..Facts::default() };
        assert!(
            lock_recover(&graph.debt).decide(now, facts).start.is_none(),
            "the fixture needs a budget that has stopped",
        );

        graph.note_request();
        graph.drive();
        // And the door the request path actually goes through, on a graph that has failed:
        // the build it could start is the FIRST one, never a retry the schedule is holding.
        lock_recover(&graph.inner).status = GraphStatus::Failed("the last build failed".to_owned());
        graph.ensure_first_build();
        assert!(
            matches!(graph.status(), GraphStatus::Failed(_)),
            "a request restarted a failed build, bypassing its schedule"
        );
        lock_recover(&graph.inner).status = GraphStatus::Ready { files: 1 };

        assert_eq!(graph.status(), GraphStatus::Ready { files: 1 }, "a request started a build");
        assert!(
            lock_recover(&graph.debt).decide(Instant::now(), facts).start.is_none(),
            "a request re-armed the stopped budget",
        );
        assert!(graph.owes_failed(), "and the failure it could not act on is still owed");
    }

    /// A publish hook attached via `with_publish_hook` fires on the graph's background
    /// thread once the build completes and publishes — the seam the search context
    /// re-render hangs on. Without the `notify_published()` call at the publish site the
    /// counter stays zero and this fails.
    #[test]
    fn indexing_owner_lifecycles_graph() {
        use crate::indexing::{Reason, State};
        let mut graph = GraphState::with_status(GraphStatus::Idle, None);
        assert_eq!(graph.indexing_snapshot().state, State::Waiting);
        {
            let _held = graph.watch.lock().unwrap();
            assert_eq!(graph.indexing_snapshot().state, State::Unknown);
        }
        {
            let _guard = graph.inner.lock().unwrap();
            let (report, target) = graph.status_report_with_indexing();
            assert_eq!(target.state, State::Unknown);
            assert_eq!(report.state, "loading");
        }
        graph.inner.lock().unwrap().status = GraphStatus::Loading;
        assert_eq!(graph.indexing_snapshot().state, State::Running);
        {
            let mut inner = graph.inner.lock().unwrap();
            inner.status = GraphStatus::Ready { files: 0 };
            inner.indexing_unread_files = Some(0);
            inner.published = Some(Published {
                generation: 7,
                observed_through: Some(0),
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
            });
        }
        let (report, target) = graph.status_report_with_indexing();
        assert_eq!(target.state, State::Ready);
        let workspace = tempfile::tempdir().unwrap();
        graph.workspace_root = Some(workspace.path().to_path_buf());
        assert_eq!(
            graph.indexing_snapshot().state,
            State::Waiting,
            "an unwatched workspace cannot report a fresh graph"
        );
        graph.workspace_root = None;
        assert_eq!(report.revision, None); // no pre-opened descriptor in this owner-only fixture
        assert_eq!(report.stale, None);
        graph.inner.lock().unwrap().indexing_unread_files = Some(1);
        let (report, target) = graph.status_report_with_indexing();
        assert_eq!(target.reason_code, Some(Reason::StaleGeneration));
        assert_eq!(report.revision, None); // no pre-opened descriptor in this owner-only fixture
        assert_eq!(report.stale, None);
        graph.inner.lock().unwrap().indexing_unread_files = Some(0);
        graph.inner.lock().unwrap().published.as_mut().unwrap().reload = ReloadState::Running;
        assert_eq!(graph.indexing_snapshot().state, State::Running);
        graph.inner.lock().unwrap().published.as_mut().unwrap().reload =
            ReloadState::Failed("private failure".to_owned());
        assert_eq!(graph.indexing_snapshot().state, State::Failed);
        let json = serde_json::to_value(graph.indexing_snapshot()).unwrap();
        for field in ["phase", "progress", "pass_id"] {
            assert!(json[field].is_null());
        }
        assert!(!json.to_string().contains("private failure"));
        assert_eq!(graph.scan_count(), 0);
    }

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

    /// A request that picked up a snapshot before a reload published must not be told the data
    /// it is carrying is current: the revision it names is gone.
    #[test]
    fn a_snapshot_from_a_replaced_revision_is_never_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let (graph, _hub, _stop) = super::super::test_support::watched_graph(root);
        graph.ensure_loading();
        wait_ready(&graph);
        wait_until(&graph, "the watcher's boot nudge to settle", || !graph.drift_pending());
        let snapshot = graph.snapshot().expect("the published revision serves a descriptor");
        assert!(!graph.cached_freshness(&snapshot).stale, "a current snapshot is fresh");

        // The publication moves on while the request still holds the older snapshot.
        lock_recover(&graph.inner).published.as_mut().expect("a published graph").generation += 1;

        assert!(
            graph.cached_freshness(&snapshot).stale,
            "an obsolete revision was reported as current",
        );
    }

    /// `stale` is what an agent reads to decide whether the answer it just got reflects the
    /// tree it is looking at. Once a catch-up is owed, answering `false` is not "cached", it
    /// is wrong — and it is exactly what a request-path check used to prevent.
    #[test]
    fn a_pending_rebuild_is_not_reported_as_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let (graph, _hub, _stop) = super::super::test_support::watched_graph(root);
        graph.ensure_loading();
        wait_ready(&graph);
        wait_until(&graph, "the watcher's boot nudge to settle", || !graph.drift_pending());
        assert_eq!(published_report(&graph).stale, Some(false), "a fresh build is not stale");

        graph.record_change_quietly(graph.observation() + 1);

        assert_eq!(
            published_report(&graph).stale,
            Some(true),
            "a graph with a catch-up owed reports itself fresh",
        );
    }

    /// A new owner opens nothing while the previous one still holds the graph file: it waits,
    /// says since when, and loads once the file is let go.
    #[test]
    fn a_new_owner_waits_for_the_graph_file_before_opening_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let previous =
            crate::workspace_lease::ExclusiveFileLock::try_acquire(&cache.graph_access_lock_path())
                .unwrap()
                .expect("the previous owner's lock");
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease);
        graph.ensure_loading();
        wait_until(&graph, "the new owner to wait for the file", || {
            graph.waiting_for_access_since().is_some()
        });
        let report = graph.status_report();
        assert_eq!(report.state, "loading");
        assert!(report.error.as_deref().is_some_and(|error| error.contains("previous owner")));
        assert!(!cache.graph_db_path().exists(), "nothing was built or opened meanwhile");

        drop(previous);
        wait_ready(&graph);
        assert!(graph.waiting_for_access_since().is_none());
    }

    /// A check that cannot answer holds new reads back without retiring anything: once the
    /// owner is confirmed again, the same handles serve.
    #[test]
    fn an_unanswered_ownership_check_pauses_reads_without_retiring_the_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        graph.ensure_loading();
        wait_ready(&graph);

        // The record gone and its lock held: the check can neither read an owner nor claim one.
        let held = lease.hold_file_lock_for_test();
        std::fs::remove_file(cache.lease_path()).unwrap();
        assert!(!lease.owns_caches_now());
        assert!(!lease.is_superseded(), "an unanswered check is not a lost workspace");
        assert_eq!(
            graph.store.read(None, Duration::from_millis(50), |_| ()),
            Err(super::super::snapshot::GraphReadError::Busy)
        );

        drop(held);
        assert!(lease.owns_caches_now(), "the free workspace is claimed again");
        assert!(graph.read(|snapshot| snapshot.generation()).is_ok(), "the same pool serves");
        assert!(!graph.released());
    }

    /// Ownership passes between two graphs of one process in the order two processes need: the
    /// superseded one lends nothing new, lets its read in flight finish, closes its handles and
    /// lets the file go, and only then does its successor open it.
    #[test]
    fn a_superseded_owner_lets_the_file_go_once_its_reads_finish() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let first = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(crate::workspace_lease::WorkspaceLease::claim_cache(&cache));
        first.ensure_loading();
        wait_ready(&first);
        let in_flight = first.snapshot().expect("a read in flight");

        let second = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(crate::workspace_lease::WorkspaceLease::claim_cache(&cache));
        assert!(first.is_superseded());
        second.ensure_loading();
        wait_until(&second, "the successor to wait for the file", || {
            second.waiting_for_access_since().is_some()
        });
        assert_eq!(
            first.read(|snapshot| snapshot.generation()),
            Err(super::super::snapshot::GraphReadError::OwnerChanged)
        );
        assert!(in_flight.graph.freshness_token().is_ok(), "the read in flight is not cut");
        assert!(!first.released());

        drop(in_flight);
        wait_ready(&second);
        assert!(first.released(), "the file went to the successor");
    }

    /// A use of the graph file outside the lent handles — a build inspecting or copying it —
    /// keeps the file from the next owner exactly like a read in flight, and none starts once
    /// the workspace is lost.
    #[test]
    fn a_retiring_graph_waits_for_uses_of_its_file_outside_the_pool() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        graph.ensure_loading();
        wait_ready(&graph);
        let copying = graph.store.use_file().expect("a use of the file");

        lease.release();
        assert!(graph.store.use_file().is_err(), "no use starts after the workspace is lost");
        std::thread::sleep(Duration::from_millis(200));
        assert!(!graph.released(), "the file is still in use");
        drop(copying);
        wait_until(&graph, "the file to be let go", || graph.released());
    }

    /// Each cache directory has its own access lock: a directory whose file is held elsewhere
    /// keeps only its own graph waiting, and the other five load.
    #[test]
    fn independent_cache_directories_do_not_wait_for_each_other() {
        let dirs: Vec<_> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
        let caches: Vec<_> = dirs
            .iter()
            .map(|dir| {
                sample_workspace(dir.path());
                crate::cache::WorkspaceCacheLayout::for_workspace(dir.path())
            })
            .collect();
        caches[0].ensure().unwrap();
        let held = crate::workspace_lease::ExclusiveFileLock::try_acquire(
            &caches[0].graph_access_lock_path(),
        )
        .unwrap()
        .expect("a stranger holds the first directory's file");
        let graphs: Vec<_> = dirs
            .iter()
            .zip(&caches)
            .map(|(dir, cache)| {
                let graph =
                    GraphState::for_workspace_with_cache(dir.path().to_path_buf(), cache.clone())
                        .with_lease(crate::workspace_lease::WorkspaceLease::claim_cache(cache));
                graph.ensure_loading();
                graph
            })
            .collect();
        for graph in &graphs[1..] {
            wait_ready(graph);
        }
        assert!(graphs[0].waiting_for_access_since().is_some(), "only the held one waits");
        drop(held);
        wait_ready(&graphs[0]);
    }

    /// A process that could not claim its cache directory at all does not open the graph over
    /// it, and says why.
    #[test]
    fn an_uncoordinated_graph_is_not_opened() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        sample_workspace(&root);
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"file").unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::from_root(blocker.join("cache"));
        let graph = GraphState::for_workspace_with_cache(root, cache.clone())
            .with_lease(crate::workspace_lease::WorkspaceLease::claim_cache(&cache));
        assert!(graph
            .unavailable_reason()
            .is_some_and(|reason| reason.contains("could not claim")));
        graph.ensure_loading();
        wait_until(&graph, "the load to be refused", || {
            matches!(graph.status_report().state, "failed")
        });
        assert!(graph.snapshot().is_none());
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
        {
            let _busy = graph.inner.lock().unwrap();
            let (report, target) = graph.status_report_with_indexing();
            assert_eq!(report.state, "failed", "a superseded owner never reports loading");
            assert_eq!(report.superseded, Some(true));
            assert_eq!(target.state, crate::indexing::State::Superseded);
        }

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
        assert!(!graph.released(), "the reads in flight keep the file until they finish");
        drop(held);
        wait_until(&graph, "the superseded graph to let its file go", || graph.released());
        let returned = graph.status_report();
        assert_eq!(returned.state, "failed", "a returned handle is closed, never lent again");
        assert_eq!(returned.superseded, Some(true));
        assert!(graph.snapshot().is_none());
        assert_eq!(
            graph.read(|snapshot| snapshot.generation()),
            Err(super::super::snapshot::GraphReadError::OwnerChanged),
            "revision {own_revision} is not served by a process that lost the workspace"
        );
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
        graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
        graph.flush_hook_obligations();

        assert_eq!(refreshes.load(Ordering::SeqCst), after_publish + 1, "the request is honoured");
        assert!(!graph.hook_debt().topology, "and cleared once the consumer handled it",);
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
        graph.nudge_rebuild();
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
                observed_through: Some(0),
            });
        }
        graph.record_change_quietly(graph.observation());
        graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
        graph.record_hook_debt(HookDebt { topology: false, roots: true, marks: false });
        lock_recover(&graph.debt).place_marks(Instant::now(), 41, 0);

        let _newer = crate::workspace_lease::WorkspaceLease::claim(root);
        graph.notify_published(true);
        graph.flush_hook_obligations();
        graph.flush_hook_obligations();
        graph.consume_leftover_marks(99);

        assert!(graph.is_superseded());
        assert_eq!(fired.load(Ordering::SeqCst), 0, "no late hook may apply its prepared plan");
        assert!(graph.owes_change().is_some(), "no late reload may start");
        assert!(graph.hook_debt().topology);
        assert!(graph.hook_debt().roots);
        assert!(graph.marks_pending(), "a superseded graph consumes nothing");

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
                observed_through: Some(0),
            });
        }
        refusal_graph.consume_leftover_marks(99);
        assert!(refusal_lease.is_superseded());
        assert!(refusal_graph.marks_pending(), "a refused consume keeps the placed marks");
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
                observed_through: Some(0),
            });
        }
        assert!(!graph.hook_debt().topology, "the fixture starts with no topology owed");

        graph.notify_published(false);

        assert!(
            !graph.hook_debt().topology,
            "a refusal reported for an unrequested refresh raises no obligation",
        );
        // The control: the SAME refusing hook DOES arm the flag when the refresh was requested,
        // so the assertion above is about the request and not about a flag nothing can set.
        graph.notify_published(true);
        assert!(
            graph.hook_debt().topology,
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

        graph.notify_published(true);
        assert_eq!(seen.load(Ordering::SeqCst), 1, "the request reaches the hook");
        graph.notify_published(false);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "an unhandled topology refresh is re-raised even though this publish did not change it",
        );
        graph.notify_published(false);
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
                observed_through: Some(0),
            });
        }
        graph.record_change_quietly(graph.observation());

        // The publish chain fires the hook once and, seeing the recorded nudge with disk
        // drifted past the faked build, claims a follow-up reload. Pass an explicit unbounded
        // bound (i64::MAX) so the seq bound never gates this test — only the reclaim behavior
        // under test decides how many times the hook fires.
        graph.notify_published(false);

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
                observed_through: Some(0),
            });
        }
        assert!(!graph.drift_pending(), "a clean published graph has no pending drift");

        lock_recover(&graph.inner).status = GraphStatus::Loading;
        assert!(graph.drift_pending(), "a build in flight is a pending drift");
        lock_recover(&graph.inner).status = GraphStatus::Ready { files: 0 };

        lock_recover(&graph.inner).published.as_mut().unwrap().reload = ReloadState::Running;
        assert!(graph.drift_pending(), "a running reload is a pending drift");
    }

    /// Even after the stale boot's pre-claimed catch-up FAILS (`reload=Failed`, so
    /// `drift_pending` no longer holds), the leftover marks must stay placed: the stale
    /// snapshot predates their causes, and consuming them against it would clear them for
    /// good.
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
                observed_through: Some(0),
            });
            inner.status = GraphStatus::Ready { files: 1 };
        }

        graph.consume_leftover_marks(5);
        assert_eq!(fired.load(Ordering::SeqCst), 0, "no consume against the stale snapshot");
        assert!(graph.marks_pending(), "the marks wait for the next successful publish");
    }

    /// The single-flight core of the drift nudge: the FIRST claim on a drifted published
    /// A hand-over carries the admission that was paid for, not a new one wearing its name.
    ///
    /// The slot was granted and charged once; the work is only moving to another thread. Built
    /// again from whatever the debts say at that moment, the ticket picked up sponsors that
    /// never paid for this build and a demand admitted after it — so the outcome would close
    /// accounts nobody had charged and answer work this build was never admitted for.
    #[test]
    fn a_handover_carries_the_ticket_the_claim_paid_for() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        {
            let mut debt = lock_recover(&graph.debt);
            debt.place_marks(Instant::now(), 5, 1);
            debt.settle_marks(Instant::now(), false);
        }
        assert!(graph.try_begin_external_build(), "the boot takes the claim");
        let paid = graph.claimed_ticket().expect("the claim granted a mandate");

        // Work that arrives AFTER the claim: a demand this build was never admitted for.
        lock_recover(&graph.debt).record_forced(Instant::now(), 42);

        graph.issue_handover_ticket();
        let handover = graph.claimed_ticket().expect("the hand-over granted a mandate");

        assert_eq!(
            handover.sponsors, paid.sponsors,
            "the hand-over named sponsors that never paid for this build",
        );
        assert_eq!(
            handover.forced_through, paid.forced_through,
            "the hand-over adopted a demand admitted after the claim it continues",
        );
        assert_eq!(
            handover.scan_cutoff, paid.scan_cutoff,
            "the hand-over widened the cutoff its proof may cover",
        );
        assert_eq!(
            handover.recovery_cutoff, paid.recovery_cutoff,
            "the hand-over captured recovery origins the charge never covered",
        );
        assert_eq!(handover.kind, BuildKind::Reload, "the hand-over is a catch-up");
    }

    /// A claim refused for want of a sponsor is not a comparison that matched.
    ///
    /// The walk before the admission line takes seconds, and the only account that could pay
    /// can run out inside it. What the caller has then is a measured DIFFERENCE it may not act
    /// on — and reporting that as "disk already matches" retired the change nobody compared,
    /// deleted the failure account with its explanation, and cleared the failed reload slot,
    /// so the next caller found a graph that looked answered and a budget that looked fresh.
    ///
    /// The budget is closed here BEFORE the call rather than inside the walk; the claim reads
    /// it at the same line either way, and what this fixes is what the caller does with the
    /// refusal, not how the deadline came to pass.
    #[test]
    fn a_claim_refused_for_want_of_a_sponsor_answers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("A.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 0 };
            // fingerprint 0 can never match the real disk scan → the comparison really does
            // see a difference.
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Failed("the last reload could not open the store".to_owned()),
                force_stale: false,
                search_roots: None,
                observed_through: Some(0),
            });
        }
        let now = Instant::now();
        {
            let mut debt = lock_recover(&graph.debt);
            debt.record_change(now, 7);
            let sponsors = debt.charge_admission(7, now, false);
            assert!(sponsors.primary, "the stand needs a real primary admission first");
            // The operation error closes the retry budget for good: from here nothing can pay
            // for an attempt until fresh work arrives.
            debt.record_failure(now, FailureKind::Operation, sponsors);
            assert!(debt.owes_change().is_some(), "the stand needs an unanswered change");
            assert!(
                !debt.charge_admission(7, now, false).any(),
                "the stand needs an admission nothing can pay for",
            );
        }

        // The caller that answers a delivered change, on the path it really takes.
        graph.check_against_disk();

        let debt = lock_recover(&graph.debt);
        assert!(
            debt.owes_change().is_some(),
            "the change nobody compared was retired by a claim that never ran",
        );
        assert!(debt.owes_failed(), "the failure account and its explanation were deleted");
        drop(debt);
        assert!(
            matches!(
                lock_recover(&graph.inner).published.as_ref().map(|p| p.reload.clone()),
                Some(ReloadState::Failed(_))
            ),
            "the refused claim erased the last reload's own explanation",
        );
    }

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
                observed_through: Some(0),
            });
        }
        assert!(
            matches!(graph.try_claim_reload(false), ReloadClaim::Claimed),
            "the first claim wins on drift",
        );
        assert!(
            matches!(graph.try_claim_reload(false), ReloadClaim::Running),
            "a second claim loses while a reload is Running",
        );
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

        graph.nudge_rebuild();
        assert_ne!(graph.status(), GraphStatus::Idle, "the change scheduled the initial load");
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
                observed_through: Some(0),
            });
        }
        graph.nudge_rebuild();
        graph.nudge_rebuild();
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
                observed_through: Some(0),
            });
        }

        graph.record_forced(7);
        let first = graph.owes_forced().expect("a forced reload is owed");
        graph.record_forced(9);
        let second = graph.owes_forced().expect("the later request is the one owed");
        assert!(second > first, "a later fact must not be swallowed by an earlier one");

        // The running build captured only the first request. Its publication may discharge
        // that fact, but not the newer one that arrived after its capture.
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(first),
            true,
            None,
            super::super::debt::RecoveryPublicationProof::without_coverage(first),
        );
        assert_eq!(
            graph.owes_forced(),
            Some(second),
            "a publication that observed the older fact discharged the newer request",
        );

        lock_recover(&graph.inner).published.as_mut().unwrap().reload = ReloadState::Idle;
        assert!(
            matches!(graph.try_claim_reload(true), ReloadClaim::Claimed),
            "the newer force request claims the follow-up reload",
        );

        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(second),
            true,
            None,
            super::super::debt::RecoveryPublicationProof::without_coverage(second),
        );
        assert_eq!(graph.owes_forced(), None, "the observing publication discharged it");
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
                observed_through: Some(0),
            });
        }
        graph.notify_published(true);
        assert!(graph.hook_debt().topology);
        assert!(!graph.hook_debt().roots);

        *outcome.lock().unwrap() =
            GraphPublishOutcome { topology_handled: true, roots_handled: false };
        graph.flush_hook_obligations();
        assert!(!graph.hook_debt().topology);
        graph.notify_published(false);
        assert!(graph.hook_debt().roots);
        assert!(!graph.hook_debt().topology);

        *outcome.lock().unwrap() = GraphPublishOutcome::HANDLED;
        graph.flush_hook_obligations();
        assert!(!graph.hook_debt().roots);
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
                && graph.owes_forced().is_none()
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
        // Recorded without deciding: this test drives the builds itself, and a reload spawned
        // from the intake would race the refusal it is about to install.
        lock_recover(&graph.debt).record_forced(Instant::now(), graph.observation());
        super::super::snapshot::refuse_snapshot_install_for_test();
        graph.run_load(true);
        assert!(
            graph.owes_forced().is_some(),
            "a refused install must leave the forced reload outstanding",
        );

        graph.run_load(true);
        assert_eq!(graph.owes_forced(), None, "the retry installed and must discharge it",);
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
                observed_through: Some(0),
            });
        }
        graph.record_forced(3);

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
            "forced Some(3)",
        ] {
            assert!(
                reported.contains(named),
                "a timed-out wait must name {named:?}; it reported {reported:?}"
            );
        }
        lock_recover(&graph.inner).published.as_mut().unwrap().reload =
            ReloadState::Failed("snapshot replacement refused".to_owned());
        let summary = super::super::test_support::graph_state_summary(&graph);
        assert!(summary.contains("reload failed"));
        assert!(summary.contains("snapshot replacement refused"));
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
    /// internally. A test needing a window inside the pass parks in the publish hook
    /// instead.
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
        graph.nudge_project_reload();
        assert!(
            graph.drift_pending(),
            "a declared-root change claims a forced reload the fingerprint cannot",
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
                .then(|| (published.generation, graph.owes_forced().is_some()))
        };
        window.release();

        assert_eq!(
            torn.map(|(_, pending)| pending),
            Some(false),
            "the parked build published generation {:?}, but its force obligation was still \
             outstanding: {:?}",
            torn.map(|(generation, _)| generation),
            graph.owes_forced(),
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

        graph.nudge_rebuild();
        window.release();

        wait_for_forced_reload(&graph, generation);
        // A claim would spawn its rebuild off-thread; give one time to publish before counting.
        std::thread::sleep(Duration::from_millis(200));

        assert_eq!(
            published_generation(&graph),
            generation + 1,
            "nothing drifted and the forced reload had published, yet a second rebuild ran"
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
                observed_through: Some(0),
            });
        }
        graph.record_forced(1);

        assert!(
            !forced_reload_published(&graph, 1),
            "a newer generation whose force obligation is still outstanding is not the \
             publication this wait is for"
        );

        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(1),
            true,
            None,
            super::super::debt::RecoveryPublicationProof::without_coverage(1),
        );
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
        graph.nudge_rebuild();
        graph.nudge_project_reload();
        assert!(
            graph.owes_forced().is_some() || graph.drift_pending(),
            "a declared-root change claims a reload the fingerprint cannot",
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
            graph.owes_forced(),
            None,
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

    /// Placed marks leave the ledger only once a hook reports them handled. Inside a pass whose
    /// hook will refuse, and after it, they still read pending — there is no window in which
    /// an obligation nothing discharged reads as discharged.
    #[test]
    fn unhandled_marks_read_pending_inside_and_after_the_pass() {
        const LEFTOVER_BOUND: i64 = 41;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let hook = Arc::new(move |signal: GraphPublishSignal| {
            if signal.mark_bound == LEFTOVER_BOUND {
                entered_tx.send(()).expect("the test outlives the parked pass");
                lock_recover(&release_rx)
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the test released the parked pass");
            }
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
                observed_through: Some(0),
            });
        }
        lock_recover(&graph.debt).place_marks(Instant::now(), LEFTOVER_BOUND, 0);

        let publisher = {
            let graph = graph.clone();
            std::thread::spawn(move || graph.notify_published(false))
        };
        entered.recv_timeout(Duration::from_secs(30)).expect("the pass reached its consume");
        assert!(graph.marks_pending(), "inside the pass the marks already read consumed");

        release_tx.send(()).expect("the parked pass is still running");
        publisher.join().expect("the publish pass completed");
        assert!(graph.marks_pending(), "an unhandled consume dropped the marks");
        assert!(graph.hook_debt().marks, "the refused covered refresh remains owed to the hook");
        assert!(
            !graph.owes_marks(),
            "the publication already observed these facts; a refused refresh does not need a graph build"
        );
    }

    /// Consuming marks against a publication and installing the next one are one critical
    /// section. The hook renders context out of whatever graph is published when it runs, so a
    /// publication swapped in midway would take marks that were cleared against another —
    /// and an unsound publication would take marks no publication may consume at all.
    #[test]
    fn a_publication_cannot_be_installed_while_marks_are_being_consumed() {
        const BOUND: i64 = 41;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let hook = Arc::new(move |signal: GraphPublishSignal| {
            if signal.mark_bound == BOUND {
                entered_tx.send(()).expect("the test outlives the parked consume");
                lock_recover(&release_rx)
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the test released the parked consume");
            }
            GraphPublishOutcome::HANDLED
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
                observed_through: Some(9),
            });
        }

        // The consumer's thread: marks whose fact this publication observed, consumed against
        // it — and parked inside the hook.
        let consuming = {
            let graph = graph.clone();
            std::thread::spawn(move || graph.marks_placed(BOUND, 9))
        };
        entered.recv_timeout(Duration::from_secs(30)).expect("the consume reached its hook");

        // Meanwhile a build tries to install an unsound publication of its own.
        let installing = {
            let graph = graph.clone();
            std::thread::spawn(move || {
                let _gate = lock_recover(&graph.publication_gate);
                lock_recover(&graph.inner).published.as_mut().unwrap().force_stale = true;
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !installing.is_finished(),
            "a publication was installed while marks were being consumed against another",
        );

        release_tx.send(()).expect("the parked consume is still running");
        consuming.join().expect("the consume completed");
        installing.join().expect("the install completed once the gate was free");
        assert!(!graph.marks_pending(), "the consume that held the gate cleared its own marks");
    }

    /// The other half of that critical section, counted rather than acted out: the only
    /// production writer of `published` is `install_prepared_snapshot`, and a unit test can
    /// only fake an install — a real one needs a built pool and a lease fence. What it can
    /// prove is that the install takes the gate, and takes it BEFORE the fence: a hook
    /// running under the gate publishes through the lease, so the reverse order would let the
    /// two sides deadlock instead of serialising.
    #[test]
    fn the_publication_install_takes_the_gate_before_its_fence() {
        let source = crate::inventory::production_source(include_str!("snapshot.rs"));
        let install = source
            .split_once("fn install_prepared_snapshot(")
            .expect("the install is still there")
            .1;
        let fence = install.find(".publish_short(").expect("the install still fences its write");
        let head = &install[..fence];
        assert!(
            head.contains("publication_gate"),
            "the install lands without the gate that serialises it against a consume"
        );
        // And nowhere else: a gate taken inside the fence is the lock-order inversion.
        let body_end = install.find("\n    /// Snapshot the graph").unwrap_or(install.len());
        assert_eq!(
            install[..body_end].matches("publication_gate").count(),
            1,
            "the install takes the gate more than once, or takes it inside its own fence"
        );
    }

    /// A subtree the build could not read makes the publication unsound: no fingerprint
    /// comparison can retire it, and no file event announces a restored permission. The probe
    /// is its owner, and a healing is what pays for the rebuild that clears it.
    #[cfg(unix)]
    #[test]
    fn a_healed_subtree_is_found_by_the_probe_and_rebuilt() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hidden = root.join("CommonModules").join("Скрытый");
        fs::create_dir_all(hidden.join("Ext")).unwrap();
        fs::write(hidden.join("Ext").join("Module.bsl"), "Функция Ф() Экспорт КонецФункции")
            .unwrap();
        let open = fs::metadata(&hidden).unwrap().permissions();
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&hidden).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        let snapshot = graph.snapshot().expect("a ready graph publishes a snapshot");
        assert!(
            snapshot.force_stale,
            "a build that could not see the whole tree must not pass for a faithful one",
        );
        assert!(
            lock_recover(&graph.debt).stale(),
            "and the graph must read behind while that publication stands",
        );
        assert!(graph.owes_recovery(), "the publication it left is owed a probe");
        // A read still held would keep the rebuild from replacing the file under it.
        drop(snapshot);

        fs::set_permissions(&hidden, open).unwrap();
        lock_recover(&graph.debt).probe_now(Instant::now());
        graph.drive();

        wait_until(&graph, "the healed subtree to be rebuilt", || !graph.owes_recovery());
        let snapshot = graph.snapshot().expect("the rebuild published");
        assert!(!snapshot.force_stale, "the rebuild over a whole tree is still not faithful");
        assert!(!lock_recover(&graph.debt).stale(), "and nothing is owed once it has published",);
    }

    /// A module whose bytes could not be read is the same debt in the small: `stat` needs no
    /// read permission, so the fingerprint is equal and only opening the file can tell.
    #[cfg(unix)]
    #[test]
    fn a_module_that_becomes_readable_again_is_rebuilt() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let open = fs::metadata(&module).unwrap().permissions();
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let snapshot = graph.snapshot().expect("a ready graph publishes a snapshot");
        assert!(snapshot.unread_files() > 0, "the fixture needs a module the build could not read");
        assert!(graph.owes_recovery(), "an unread module owes a probe");
        // A read still held would keep the rebuild from replacing the file under it.
        drop(snapshot);

        // Restored WITHOUT touching the bytes: mtime and length are what they were, so no
        // fingerprint comparison can see this.
        fs::set_permissions(&module, open).unwrap();

        // And the probe runs with the request pool DRAINED, which is the ordinary state under
        // a handful of concurrent graph calls. A background reader takes a handle like any
        // other, so it gets none: that must read as "not measured" and leave the debt owed,
        // never as "this publication left nothing unread", which would double its pause for a
        // workspace that is now perfectly readable.
        let mut held = Vec::new();
        while let Some(snapshot) = graph.snapshot() {
            held.push(snapshot);
            assert!(held.len() <= 64, "the pool never ran out; the fixture cannot drain it");
        }
        assert!(!held.is_empty(), "the fixture needs a pool with handles to drain");

        lock_recover(&graph.debt).probe_now(Instant::now());
        graph.drive();
        assert!(graph.owes_recovery(), "a probe that got no handle measured nothing");
        drop(held);

        wait_until_driving(
            &graph,
            "the readable module to be rebuilt",
            || {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.drive();
            },
            || graph.snapshot().is_some_and(|snapshot| snapshot.unread_files() == 0),
        );
        assert!(!graph.owes_recovery(), "the rebuild that read everything left the debt behind");
    }

    /// What an install records for a build that left a module unread: the gap that makes the
    /// publication unsound, declared by the publication itself.
    fn left_unread(
        generation: u64,
        unread: &[&str],
    ) -> super::super::debt::RecoveryPublicationProof {
        super::super::debt::RecoveryPublicationProof {
            generation,
            declared_unread: Some(unread.iter().map(|path| (*path).to_owned()).collect()),
            ..Default::default()
        }
    }

    /// What the install records for a build that read every address it was owed: nothing left
    /// unread, a walk that covered the whole of its scope, and the proof of each obligation it
    /// answered.
    fn read_everything(
        graph: &GraphState,
        generation: u64,
    ) -> super::super::debt::RecoveryPublicationProof {
        let outstanding = lock_recover(&graph.debt).outstanding_recovery();
        super::super::debt::RecoveryPublicationProof {
            generation,
            captured_seq: outstanding.captured_seq,
            declared_unread: Some(Vec::new()),
            scan_complete: Some(true),
            read_covered: outstanding.keys,
            ..Default::default()
        }
    }

    /// Restores the modes a test changed, whatever happens to it.
    ///
    /// Not tidiness: a directory left at `0o000` is one the temp dir cannot remove, so a panic
    /// without this leaves the fixture behind on the machine that ran it.
    #[cfg(unix)]
    struct RestoredModes(Vec<(std::path::PathBuf, fs::Permissions)>);

    #[cfg(unix)]
    impl RestoredModes {
        fn of(paths: &[&Path]) -> Self {
            Self(
                paths
                    .iter()
                    .map(|path| ((*path).to_path_buf(), fs::metadata(path).unwrap().permissions()))
                    .collect(),
            )
        }
    }

    #[cfg(unix)]
    impl Drop for RestoredModes {
        fn drop(&mut self) {
            for (path, mode) in &self.0 {
                let _ = fs::set_permissions(path, mode.clone());
            }
        }
    }

    /// Every unread path heals on its own account.
    ///
    /// A witness of two booleans cannot say this: "something unread opened" is true for the
    /// rest of the episode as soon as ANY path opens, so the second path becoming readable
    /// measures nothing new, no rebuild is armed, and that module stays missing from the graph
    /// for as long as the daemon lives. Nothing else can rescue it either — a restored
    /// permission writes nothing, so no fact ever announces it, which is asserted here.
    #[cfg(unix)]
    #[test]
    fn each_unread_path_heals_on_its_own_account() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let first = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let second = root.join("CommonModules").join("Клиент").join("Ext").join("Module.bsl");
        let restore = RestoredModes::of(&[&first, &second]);
        for path in [&first, &second] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::read(path).is_ok() {
                eprintln!("skipping: mode 0o000 is not an obstacle for this user");
                return;
            }
        }

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert_eq!(
            graph.snapshot().expect("a ready graph publishes a snapshot").unread_files(),
            2,
            "the fixture needs two modules the build could not read",
        );
        let stream = graph.observation();

        // The first one opens. Nothing is written, so nothing on the fact stream says so.
        fs::set_permissions(&first, restore.0[0].1.clone()).unwrap();
        wait_until_driving(
            &graph,
            "the first healing to be rebuilt",
            || {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.drive();
            },
            || graph.snapshot().is_some_and(|snapshot| snapshot.unread_files() == 1),
        );
        assert!(graph.owes_recovery(), "a publication still missing a module owes a probe");

        // And now the second, before the next probe — the case an aggregate cannot see,
        // because the level it reports was already true when the first one healed.
        fs::set_permissions(&second, restore.0[1].1.clone()).unwrap();
        wait_until_driving(
            &graph,
            "the second healing to be rebuilt",
            || {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.drive();
            },
            || graph.snapshot().is_some_and(|snapshot| snapshot.unread_files() == 0),
        );
        assert!(!graph.owes_recovery(), "the rebuild that read everything left the debt behind");
        assert_eq!(
            graph.observation(),
            stream,
            "the fact stream moved: this was not the permission-only healing it claims to be",
        );
    }

    /// A path the publication could not read and that is now GONE is measured as absent, by
    /// name — not as one more open to retry for ever.
    #[cfg(unix)]
    #[test]
    fn a_removed_unread_path_is_measured_as_absent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert_eq!(
            graph.snapshot().expect("a ready graph publishes a snapshot").unread_files(),
            1,
            "the fixture needs a module the build could not read",
        );

        // It is not unreadable any more. It is not there.
        drop(restore);
        fs::remove_file(&module).unwrap();

        let plan = lock_recover(&graph.debt)
            .reserve_probe()
            .expect("an unread module is an outstanding obligation");
        let outcome = graph.recovery_probe(&plan);
        let super::super::snapshot::ProbeOutcome::Looked { levels, .. } = outcome else {
            panic!("the probe could not look at a workspace it has just built over");
        };
        let measured = levels
            .iter()
            .find(|(capability, _)| {
                matches!(capability, super::super::debt::Capability::Open(path)
                    if path.ends_with("Module.bsl"))
            })
            .map(|(_, level)| *level);
        assert_eq!(
            measured,
            Some(super::super::debt::Level::Absent),
            "a path that is gone was reported as one more file that will not open: {levels:?}",
        );
    }

    /// The walk is a capability named by the SCOPE it walked, and its transition is measured
    /// like any other.
    ///
    /// Naming it matters for the same reason naming a path does: a walk over another set of
    /// declared roots is not a re-measurement of this one, and reading it as one would report
    /// a scope that was never short as having healed.
    #[cfg(unix)]
    #[test]
    fn the_walks_capability_is_named_by_the_scope_it_walked() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hidden = root.join("CommonModules").join("Скрытый");
        fs::create_dir_all(hidden.join("Ext")).unwrap();
        fs::write(hidden.join("Ext").join("Module.bsl"), "Функция Ф() Экспорт КонецФункции")
            .unwrap();
        let restore = RestoredModes::of(&[&hidden]);
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&hidden).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(
            graph.snapshot().expect("a ready graph publishes a snapshot").force_stale,
            "the fixture needs a publication whose walk came up short",
        );
        // One receipt: the verdict and the scope it is about come back from the same walk.
        let walked = |graph: &GraphState| {
            let plan = lock_recover(&graph.debt)
                .reserve_probe()
                .expect("a short publication owes a verdict about its walk");
            let outcome = graph.recovery_probe(&plan);
            let super::super::snapshot::ProbeOutcome::Looked { levels, scope } = outcome else {
                panic!("the probe could not look at a workspace just built over");
            };
            let level = levels
                .into_iter()
                .find(|(capability, _)| {
                    matches!(capability, super::super::debt::Capability::ScanRoots)
                })
                .map(|(_, level)| level)
                .expect("the walk that was owed reports its verdict");
            lock_recover(&graph.debt).release_probe(Instant::now(), plan.token, true);
            (scope.expect("a walk names the scope it covered"), level)
        };

        let (short, level) = walked(&graph);
        assert_eq!(level, super::super::debt::Level::Denied, "the walk is still short");

        drop(restore);
        let (whole, level) = walked(&graph);
        assert_eq!(level, super::super::debt::Level::Granted, "the walk now completes");
        assert_eq!(
            whole, short,
            "the same scope, walked again, is not the same scope — so its healing reads as \
             news about something else",
        );
    }

    /// Two callers reaching the probe together share ONE reservation, one walk and one
    /// completion.
    ///
    /// The barrier is real: the second caller runs while the first is inside the window
    /// between taking the reservation and touching the disk, which is exactly where a second
    /// walk of the whole tree would start.
    #[cfg(unix)]
    #[test]
    fn concurrent_probe_callers_share_one_reservation() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        let inside = Arc::new(AtomicBool::new(false));
        let second_done = Arc::new(AtomicBool::new(false));
        let hook = {
            let (inside, second_done) = (Arc::clone(&inside), Arc::clone(&second_done));
            Arc::new(move |_: &GraphState| {
                if !inside.swap(true, Ordering::SeqCst) {
                    assert!(
                        crate::change_hub::test_support::eventually(
                            Duration::from_secs(30),
                            || second_done.load(Ordering::SeqCst)
                        ),
                        "the second caller never finished",
                    );
                }
            }) as LatchWindowHook
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_probe_window_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(graph.owes_recovery(), "the fixture needs an outstanding obligation");
        // Due, because a walk is only ever reserved for work the schedule says is ripe.
        lock_recover(&graph.debt).probe_now(Instant::now());
        let before = graph.probe_walks.load(Ordering::SeqCst);

        let first = {
            let graph = graph.clone();
            std::thread::spawn(move || graph.probe_recovery())
        };
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(30), || inside
                .load(Ordering::SeqCst)),
            "the first probe never reached its window",
        );

        // The second caller, while the reservation is out.
        graph.probe_recovery();
        second_done.store(true, Ordering::SeqCst);
        first.join().expect("the parked probe finished");

        assert_eq!(
            graph.probe_walks.load(Ordering::SeqCst) - before,
            1,
            "two callers opened the tree twice for one obligation",
        );
        // And the reservation is given back: the next turn may look again.
        drop(restore);
        lock_recover(&graph.debt).probe_now(Instant::now());
        graph.probe_recovery();
        assert_eq!(
            graph.probe_walks.load(Ordering::SeqCst) - before,
            2,
            "the reservation was never released",
        );
    }

    /// The outcome of a build closes the account that paid for it — the real one.
    ///
    /// Admission and outcome are two ends of one transaction, and between them the builder
    /// takes the grant out of the slot. Read from the slot afterwards, the outcome found it
    /// empty and reported the primary lane: the account that actually bought the build kept
    /// its budget and its next moment, and a lane that bought nothing was closed instead.
    #[test]
    fn a_failed_build_closes_the_account_that_paid_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        // Marks nothing has consumed, and the obligation they arm.
        {
            let mut debt = lock_recover(&graph.debt);
            debt.place_marks(Instant::now(), 11, 1);
            debt.settle_marks(Instant::now(), false);
        }
        // And a retry lane whose window is long spent: it cannot sponsor anything, so the
        // marks are the only account left that can buy this build.
        let spent = Instant::now() - Duration::from_secs(700);
        lock_recover(&graph.debt).record_failure(
            spent,
            super::super::debt::FailureKind::Transient,
            super::super::debt::Sponsors { primary: true, marks: false },
        );

        assert!(
            matches!(graph.try_claim_reload(true), ReloadClaim::Claimed),
            "the marks buy the slot",
        );
        let ticket = graph.take_claimed_ticket().expect("the admission granted a mandate");
        assert_eq!(
            ticket.sponsors,
            super::super::debt::Sponsors { primary: false, marks: true },
            "the fixture needs a build the marks alone paid for",
        );

        // ...and the builder carrying it cannot finish.
        graph.record_load_failure(
            true,
            super::super::build::LoadFailure::operation("the builder could not finish"),
        );

        assert!(
            !lock_recover(&graph.debt).marks_are_eligible(Instant::now() + Duration::from_secs(60)),
            "the operation closed some other account and left the marks buying attempts",
        );
    }

    /// A build admitted between the decision and the walk takes the walk with it.
    ///
    /// The decision to look is taken under the debt alone and acted on afterwards. In that
    /// window a slot can be granted, and a walk reserved after it observes a world the build
    /// is already replacing — while the build itself is the owner that will answer these
    /// obligations. Re-read under `inner` → debt, where the grant is written.
    #[cfg(unix)]
    #[test]
    fn a_walk_is_not_reserved_beside_an_admitted_build() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(graph.owes_recovery(), "the fixture needs an outstanding obligation");
        lock_recover(&graph.debt).probe_now(Instant::now());
        let before = graph.probe_walks.load(Ordering::SeqCst);

        // A slot granted in the window between deciding to look and looking.
        assert!(matches!(graph.try_claim_reload(true), ReloadClaim::Claimed), "the slot is taken");
        graph.probe_recovery();

        assert_eq!(
            graph.probe_walks.load(Ordering::SeqCst),
            before,
            "a walk opened the tree beside the build that was already admitted",
        );
        assert!(
            lock_recover(&graph.debt).reserve_probe().is_some(),
            "the walk was reserved after all, and the reservation was left behind",
        );
        drop(restore);
    }

    /// A stop that lands while the walk is out refuses what it measured.
    ///
    /// Completion is the second half of the reservation, and it has to ask the same questions:
    /// an owner told to leave may not write a new level, hand out a credit, or arm a build on
    /// the way out. Refusing the whole batch is what the basis check does for a replaced
    /// publication, and a stop is no different.
    #[cfg(unix)]
    #[test]
    fn a_stop_during_the_walk_refuses_what_it_measured() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let stop = crate::state::OwnerStop::default();
        let healed = module.clone();
        let hook = {
            let stop = stop.clone();
            Arc::new(move |_: &GraphState| {
                // The obstacle really goes away, so the walk below measures a genuine
                // healing — and the stop lands in the same window.
                fs::set_permissions(&healed, fs::Permissions::from_mode(0o755)).unwrap();
                stop.stop();
            }) as LatchWindowHook
        };
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_owner_stop(stop.clone())
            .with_probe_window_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(graph.owes_recovery(), "the fixture needs an outstanding obligation");
        lock_recover(&graph.debt).probe_now(Instant::now());

        graph.probe_recovery();

        assert!(
            !lock_recover(&graph.debt).owes_recovery_build(),
            "an owner told to leave measured a healing and armed a build on its way out",
        );
        // And the walk is given back, so the next owner may look again.
        assert!(
            lock_recover(&graph.debt).reserve_probe().is_some(),
            "the refused walk kept the reservation",
        );
        drop(restore);
    }

    /// A walk that outlives its episode is still the only walk.
    ///
    /// The consumer doing the I/O outlives the publication that sent it: a coherent proof can
    /// land, close the chain, and a new gap open a new one, all while the first walker is
    /// still opening files. Owned by the episode, that walker was forgotten and a second one
    /// went out beside it.
    #[cfg(unix)]
    #[test]
    fn a_walk_that_outlived_its_episode_is_still_the_only_walk() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let inside = Arc::new(AtomicBool::new(false));
        let hook = {
            let inside = Arc::clone(&inside);
            Arc::new(move |graph: &GraphState| {
                if inside.swap(true, Ordering::SeqCst) {
                    return;
                }
                // The chain ends and another one begins while this walk is out.
                let sound = read_everything(graph, 1_000);
                let mut debt = lock_recover(&graph.debt);
                debt.record_publication(Instant::now(), Some(1), false, None, sound);
                debt.record_publication(
                    Instant::now(),
                    Some(2),
                    false,
                    None,
                    left_unread(1_001, &["/ws/Другой.bsl"]),
                );
                debt.probe_now(Instant::now());
                drop(debt);
                // A second caller, while the first walk is still out.
                graph.probe_recovery();
            }) as LatchWindowHook
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_probe_window_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(graph.owes_recovery(), "the fixture needs an outstanding obligation");
        lock_recover(&graph.debt).probe_now(Instant::now());
        let before = graph.probe_walks.load(Ordering::SeqCst);

        graph.probe_recovery();

        assert!(inside.load(Ordering::SeqCst), "the barrier never ran");
        assert_eq!(
            graph.probe_walks.load(Ordering::SeqCst) - before,
            1,
            "a second walker opened the tree while the first was still working",
        );
        drop(restore);
    }

    /// A publication landing while a walk is out invalidates its basis, and the receipt that
    /// comes back is rejected whole — on the real path, with a real rebuild in the window.
    #[cfg(unix)]
    #[test]
    fn a_publication_during_a_walk_rejects_the_receipt_it_outran() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        // A walk reserves against the publication that stands.
        let plan = lock_recover(&graph.debt)
            .reserve_probe()
            .expect("an unread module is an outstanding obligation");
        let levels = match graph.recovery_probe(&plan) {
            super::super::snapshot::ProbeOutcome::Looked { levels, .. } => levels,
            super::super::snapshot::ProbeOutcome::CouldNotLook => {
                panic!("the probe could not look at a workspace it has just built over")
            }
        };

        // A REAL rebuild lands while that receipt is in hand: the module opens again, so the
        // publication that follows is a different world.
        drop(restore);
        lock_recover(&graph.debt).record_forced(Instant::now(), graph.observation());
        drive_until(&graph, "the rebuild to publish", || {
            graph.snapshot().is_some_and(|snapshot| snapshot.unread_files() == 0)
        });

        // The straggler's receipt names a healing — and is rejected all the same.
        let result = lock_recover(&graph.debt).finish_probe(
            Instant::now(),
            super::super::debt::ProbeReceipt {
                token: plan.token,
                basis: plan.basis,
                levels,
                scope: None,
            },
        );
        assert_eq!(
            result,
            super::super::debt::ProbeResult::Obsolete,
            "a receipt measured against a replaced publication was believed",
        );
    }

    /// A healing drives the work it created without any hub event, and without the observation
    /// moving at all.
    ///
    /// Read off `owes_forced_fact()`, this was invisible: several healings share one
    /// observation, and on a graph with no hub every number is the same sentinel. The typed
    /// outcome is what says authority arrived.
    #[cfg(unix)]
    #[test]
    fn same_observation_new_healing_drives_without_hub() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let observation = graph.observation();
        let forced_fact = lock_recover(&graph.debt).owes_forced_fact();

        fs::set_permissions(&module, restore.0[0].1.clone()).unwrap();
        let plan = lock_recover(&graph.debt).reserve_probe().expect("the obligation stands");
        let levels = match graph.recovery_probe(&plan) {
            super::super::snapshot::ProbeOutcome::Looked { levels, .. } => levels,
            super::super::snapshot::ProbeOutcome::CouldNotLook => {
                panic!("the probe could not look")
            }
        };
        let result = lock_recover(&graph.debt).finish_probe(
            Instant::now(),
            super::super::debt::ProbeReceipt {
                token: plan.token,
                basis: plan.basis,
                levels,
                scope: None,
            },
        );

        assert_eq!(
            result,
            super::super::debt::ProbeResult::NewEvidence,
            "a module that opened again was not measured as news",
        );
        assert_eq!(
            lock_recover(&graph.debt).owes_forced_fact(),
            forced_fact,
            "the healing was addressed by a hub number after all",
        );
        assert_eq!(graph.observation(), observation, "the fact stream moved");
        assert!(
            lock_recover(&graph.debt).owes_recovery_build(),
            "the measured healing owes the build that proves it",
        );
        // And the work it created runs without waiting for any event.
        drive_until(&graph, "the healed module to be rebuilt", || {
            graph.snapshot().is_some_and(|snapshot| snapshot.unread_files() == 0)
        });
    }

    /// Every way a build is admitted captures the healings measured before it, and none of
    /// the ones measured after.
    ///
    /// The cutoff is fixed with the grant, at each of the entries that can grant one. A
    /// ledger test proves the arithmetic; this proves the adapters actually carry it, which
    /// is where a mandate gets re-derived from whatever is owed at publication time.
    #[cfg(unix)]
    #[test]
    fn every_admission_carries_the_healings_measured_before_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let first = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let second = root.join("CommonModules").join("Общий").join("Ext").join("Module.bsl");
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&second, "Функция Взять() Экспорт Возврат 1; КонецФункции").unwrap();
        let restore = RestoredModes::of(&[&first, &second]);
        for module in [&first, &second] {
            fs::set_permissions(module, fs::Permissions::from_mode(0o000)).unwrap();
        }
        if fs::read(&first).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(graph.owes_recovery(), "the fixture needs outstanding obligations");

        // One healing, measured through the real pass.
        fs::set_permissions(&first, fs::Permissions::from_mode(0o755)).unwrap();
        let measured = measure_one_pass(&graph);
        assert_eq!(
            measured,
            super::super::debt::ProbeResult::NewEvidence,
            "the healed module was not measured as news",
        );
        let captured = lock_recover(&graph.debt).outstanding_recovery().captured_seq;
        assert!(captured > 0, "the healing was issued no origin of its own");

        // Each entry that can grant a mandate carries that origin.
        let direct = graph.mint_direct_ticket(true);
        assert_eq!(direct.recovery_cutoff, captured, "the direct ticket carried no cutoff");
        assert!(direct.forced, "a measured healing is answered by a forced reload");

        graph.issue_handover_ticket();
        let handover = graph.take_claimed_ticket().expect("the handover granted a mandate");
        assert_eq!(handover.recovery_cutoff, captured, "the handover ticket carried no cutoff");
        assert!(handover.forced, "the handover lost the mode the healing demands");

        assert!(
            matches!(graph.try_claim_reload(true), ReloadClaim::Claimed),
            "the slot is granted"
        );
        let claimed = graph.take_claimed_ticket().expect("the claim granted a mandate");
        assert_eq!(claimed.recovery_cutoff, captured, "the claim captured no origin at all");

        // A healing measured AFTER that claim belongs to nobody's cutoff yet.
        fs::set_permissions(&second, fs::Permissions::from_mode(0o755)).unwrap();
        lock_recover(&graph.debt).probe_now(Instant::now());
        assert_eq!(
            measure_one_pass(&graph),
            super::super::debt::ProbeResult::NewEvidence,
            "the second healing was not measured",
        );

        // The claimed build publishes, proving it read the FIRST module only.
        let outstanding = lock_recover(&graph.debt).outstanding_recovery();
        let read: Vec<(String, u64)> = outstanding
            .keys
            .iter()
            .filter(|(key, _)| std::path::Path::new(key) == first.canonicalize().unwrap())
            .cloned()
            .collect();
        assert_eq!(read.len(), 1, "the fixture needs the first module outstanding");
        let still_unread: Vec<String> = outstanding
            .keys
            .iter()
            .map(|(key, _)| key.clone())
            .filter(|key| !read.iter().any(|(covered, _)| covered == key))
            .collect();
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(graph.observation()),
            true,
            Some(claimed.recovery_cutoff),
            super::super::debt::RecoveryPublicationProof {
                generation: 99,
                captured_seq: claimed.recovery_cutoff,
                declared_unread: Some(still_unread),
                read_covered: read,
                ..Default::default()
            },
        );

        assert!(
            lock_recover(&graph.debt).owes_recovery_build(),
            "the healing measured after the claim was answered by a build admitted before it",
        );
        drop(restore);
    }

    /// One real pass of the probe, with its reservation taken and given back the way the
    /// executor takes it.
    #[cfg(unix)]
    fn measure_one_pass(graph: &GraphState) -> super::super::debt::ProbeResult {
        let plan = lock_recover(&graph.debt).reserve_probe().expect("an obligation stands");
        let (levels, scope) = match graph.recovery_probe(&plan) {
            super::super::snapshot::ProbeOutcome::Looked { levels, scope } => (levels, scope),
            super::super::snapshot::ProbeOutcome::CouldNotLook => {
                panic!("the probe could not look at a workspace it owns")
            }
        };
        lock_recover(&graph.debt).finish_probe(
            Instant::now(),
            super::super::debt::ProbeReceipt {
                token: plan.token,
                basis: plan.basis,
                levels,
                scope,
            },
        )
    }

    /// A second healing runs its own build, with no event and no caller driving the graph.
    ///
    /// The pass reports what it made of the receipt, and the owner acts on that. Read off
    /// `Option<forced_fact>` instead, a second healing measured while the first was still the
    /// number on record looked exactly like nothing having happened — and a graph nobody
    /// watches has no other owner to notice.
    #[cfg(unix)]
    #[test]
    fn a_second_healing_drives_its_own_build_without_any_event() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let first = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let second = root.join("CommonModules").join("Общий").join("Ext").join("Module.bsl");
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&second, "Функция Взять() Экспорт Возврат 1; КонецФункции").unwrap();
        let restore = RestoredModes::of(&[&first, &second]);
        for module in [&first, &second] {
            fs::set_permissions(module, fs::Permissions::from_mode(0o000)).unwrap();
        }
        if fs::read(&first).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        // A snapshot is visible before its builder releases the carried ticket. A probe
        // offered in that window is correctly refused while a build is still in flight.
        wait_until(&graph, "the initial builder to release its ticket", || {
            !graph.build_in_flight()
        });
        let observation = graph.observation();
        assert_eq!(
            graph.snapshot().map(|snapshot| snapshot.unread_files()),
            Some(2),
            "the fixture needs both modules unread",
        );

        // The first heals, and the PASS itself runs the build it owes.
        fs::set_permissions(&first, fs::Permissions::from_mode(0o755)).unwrap();
        wait_until_driving(
            &graph,
            "the first healed module to be read",
            || {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.probe_recovery();
            },
            || {
                !graph.build_in_flight()
                    && graph.snapshot().is_some_and(|snapshot| snapshot.unread_files() == 1)
            },
        );
        assert!(
            !lock_recover(&graph.debt).owes_recovery_build(),
            "the build that read the module left its credit unspent",
        );

        // And the second, with the credit for the first already spent and nothing on the
        // fact stream to announce either.
        fs::set_permissions(&second, fs::Permissions::from_mode(0o755)).unwrap();
        wait_until_driving(
            &graph,
            "the second healed module to be read",
            || {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.probe_recovery();
            },
            || {
                !graph.build_in_flight()
                    && graph.snapshot().is_some_and(|snapshot| snapshot.unread_files() == 0)
            },
        );
        assert_eq!(graph.observation(), observation, "the fact stream moved");
        drop(restore);
    }

    /// The verdict and the scope are two halves of ONE walk, under a real replacement.
    ///
    /// The declaration is rewritten and re-read by another owner while this walk is between
    /// reading the project and reporting what it found. Assembled from two caches, the result
    /// could pair the verdict of the walk that ran with the roots of the declaration that
    /// replaced it — and nothing in the receipt would say so.
    #[cfg(unix)]
    #[test]
    fn scan_verdict_and_scope_are_one_receipt() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for part in ["src", "ext/a", "ext/b"] {
            fs::create_dir_all(root.join(part)).unwrap();
            fs::write(root.join(part).join("Configuration.xml"), "<Configuration/>").unwrap();
        }
        let declare = |extensions: &str| {
            fs::write(
                root.join("bsl-analyzer.toml"),
                format!("[source]\nroot = \"src\"\nextensions = [\n{extensions}]\n"),
            )
            .unwrap();
        };
        let both = "  { name = \"a\", path = \"ext/a\" },\n  { name = \"b\", path = \"ext/b\" },\n";
        let only_a = "  { name = \"a\", path = \"ext/a\" },\n";
        declare(both);

        // A subtree that cannot be walked, so the verdict of a complete walk and of a short
        // one are actually different answers.
        let closed = root.join("ext/b").join("Закрытое");
        fs::create_dir_all(&closed).unwrap();
        fs::write(closed.join("Module.bsl"), "Функция Ф() Экспорт КонецФункции").unwrap();
        let restore = RestoredModes::of(&[&closed]);
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&closed).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        let plain = GraphState::for_workspace(root.to_path_buf());
        let (with_b, clean_with_b) = plain.walk_scan_receipt().expect("a workspace walks");
        declare(only_a);
        let (without_b, clean_without_b) = plain.walk_scan_receipt().expect("a workspace walks");
        declare(both);
        assert_ne!(with_b, without_b, "the fixture needs two different declarations");
        assert!(clean_without_b, "the narrowed declaration no longer walks the closed subtree");

        // Now the replacement happens INSIDE one walk.
        let swapped = Arc::new(AtomicBool::new(false));
        let hook = {
            let (swapped, root) = (Arc::clone(&swapped), root.to_path_buf());
            Arc::new(move |graph: &GraphState| {
                if swapped.swap(true, Ordering::SeqCst) {
                    return;
                }
                fs::write(
                    root.join("bsl-analyzer.toml"),
                    "[source]\nroot = \"src\"\nextensions = [\n  { name = \"a\", path = \"ext/a\" },\n]\n",
                )
                .unwrap();
                // Another owner reads the new declaration and refreshes every cache a
                // receipt could be assembled from.
                let _ = graph.current_disk_fp();
            }) as LatchWindowHook
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_scan_receipt_hook(hook);
        let (scope, complete) = graph.walk_scan_receipt().expect("a workspace walks");

        assert!(swapped.load(Ordering::SeqCst), "the barrier never ran");
        assert_eq!(
            scope, with_b,
            "the receipt names the declaration that replaced the one this walk read",
        );
        assert_eq!(
            complete, clean_with_b,
            "the verdict belongs to the walk that produced the scope beside it",
        );
        assert_ne!(
            clean_with_b, clean_without_b,
            "the fixture needs the two declarations to walk differently",
        );
        drop(restore);
    }

    /// Metadata a real database will not answer for is a failure to look, not an empty answer.
    ///
    /// Four ways the strict reader can come back with nothing — a row that will not decode, a
    /// table that is not there, a row of the wrong type, and a handle that cannot be opened at
    /// all — against the publication path that consumes it. None of them may retire anything,
    /// and the positive control beside them is a database that really did read everything.
    #[cfg(unix)]
    #[test]
    fn strict_metadata_failures_never_become_an_empty_answer() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hidden = root.join("CommonModules").join("Скрытый").join("Ext");
        fs::create_dir_all(&hidden).unwrap();
        let module = hidden.join("Module.bsl");
        fs::write(&module, "Функция Ф() Экспорт КонецФункции").unwrap();
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let outstanding = lock_recover(&graph.debt).outstanding_recovery();
        assert_eq!(outstanding.keys.len(), 1, "the unreadable module is an obligation");
        let published = lock_recover(&graph.inner)
            .published
            .as_ref()
            .map(|published| (published.generation, published.fingerprint, published.force_stale))
            .expect("the build published");
        let db_path = graph.graph_db_path().expect("a workspace graph has a cache layout");

        // The positive control first, on the artefact as it stands: it names the module it
        // could not read, and that is an answer.
        let prepared = graph
            .prepare_snapshot_pool(published.0, published.1, published.2)
            .expect("the published artefact prepares");
        assert_eq!(
            prepared.declared_unread().map(|keys| keys.len()),
            Some(1),
            "the strict reader returns what the build actually could not read",
        );
        drop(prepared);

        let corrupt = |sql: &str| {
            let conn = rusqlite::Connection::open(&db_path).expect("the published graph opens");
            conn.execute_batch(sql).expect("the metadata is rewritten");
        };
        for (what, sql) in [
            (
                "a row that will not decode",
                "UPDATE meta SET value = '{не json' WHERE key = 'unread_paths'",
            ),
            (
                "a row of the wrong shape",
                "UPDATE meta SET value = '{\"paths\":1}' WHERE key = 'unread_paths'",
            ),
            (
                "a row that is not a string",
                "UPDATE meta SET value = X'00ff' WHERE key = 'unread_paths'",
            ),
            ("no metadata table at all", "DROP TABLE meta"),
        ] {
            corrupt(sql);
            let prepared = graph.prepare_snapshot_pool(published.0, published.1, published.2);
            let declared = match &prepared {
                Ok(prepared) => prepared.declared_unread().map(|keys| keys.to_vec()),
                // A pool that will not prepare at all is the same answer: nothing known.
                Err(_) => None,
            };
            assert!(declared.is_none(), "{what}: a broken artefact answered for what it read");

            // And what the producer makes of it: nothing it could retire an obligation with.
            let enumerated: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let proof = graph.recovery_proof(
                published.0 + 1,
                declared.as_deref(),
                super::super::snapshot::RecoveryCoverage::Walked {
                    scope: super::super::snapshot::recovery_scope_of(
                        &super::super::input::ProjectSnapshot::load(root),
                    ),
                    enumerated: &enumerated,
                    complete: true,
                    straddled: false,
                },
            );
            assert!(
                proof.read_covered.is_empty()
                    && proof.absent_covered.is_empty()
                    && proof.out_of_scope_covered.is_empty(),
                "{what}: a proof built on unreadable metadata retired an obligation",
            );

            // Installed, it changes nothing about what is outstanding.
            lock_recover(&graph.debt).record_publication(
                Instant::now(),
                Some(graph.observation()),
                false,
                None,
                proof,
            );
            assert_eq!(
                lock_recover(&graph.debt).outstanding_recovery().keys,
                outstanding.keys,
                "{what}: the obligation moved",
            );
            assert!(graph.owes_recovery(), "{what}: the chain ended on an error");
        }
        drop(restore);
    }

    /// An install that did not happen answers nothing, whichever way it was refused.
    ///
    /// Preparing is not publishing. The proof is built before the gate — that is what keeps
    /// the section short — so the retirement it carries must take effect only where the
    /// publication actually lands. Three real refusals, on the production sink itself: the
    /// path moved under it, the lease could not be taken, and the lease is gone for good.
    #[cfg(unix)]
    #[test]
    fn a_refused_install_retires_nothing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hidden = root.join("CommonModules").join("Скрытый").join("Ext");
        fs::create_dir_all(&hidden).unwrap();
        let module = hidden.join("Module.bsl");
        fs::write(&module, "Функция Ф() Экспорт КонецФункции").unwrap();
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        graph.ensure_loading();
        wait_ready(&graph);
        let owed = lock_recover(&graph.debt).outstanding_recovery();
        assert_eq!(owed.keys.len(), 1, "the unreadable module is an obligation");
        let published =
            lock_recover(&graph.inner).published.as_ref().cloned().expect("the build published");

        // A proof that WOULD retire it, prepared exactly as a build prepares one.
        let proof = || super::super::debt::RecoveryPublicationProof {
            generation: published.generation + 1,
            captured_seq: u64::MAX,
            declared_unread: Some(Vec::new()),
            scan_complete: Some(true),
            read_covered: owed.keys.clone(),
            ..Default::default()
        };
        let install = |graph: &GraphState| {
            let prepared = graph
                .prepare_snapshot_pool(
                    published.generation,
                    published.fingerprint,
                    published.force_stale,
                )
                .expect("the published artefact prepares");
            graph.install_prepared_snapshot(
                prepared,
                published.clone(),
                GraphStatus::Ready { files: 1 },
                None,
                Some(0),
                proof(),
            )
        };

        // 1. The artefact moved under the install.
        graph.refused_installs.store(1, Ordering::SeqCst);
        let refused = install(&graph);
        assert!(
            !matches!(refused, crate::workspace_lease::LeaseOperationOutcome::Applied(())),
            "the fixture needs this install to be refused",
        );
        assert_eq!(
            lock_recover(&graph.debt).outstanding_recovery().keys,
            owed.keys,
            "an install refused for a changed artefact retired an obligation",
        );

        // 2. The lease could not be taken.
        let held = lease.hold_file_lock_for_test();
        let refused = install(&graph);
        assert!(
            !matches!(refused, crate::workspace_lease::LeaseOperationOutcome::Applied(())),
            "the fixture needs the held lease to refuse this install",
        );
        drop(held);
        assert_eq!(
            lock_recover(&graph.debt).outstanding_recovery().keys,
            owed.keys,
            "an install refused by the lease retired an obligation",
        );

        // 3. The lease is gone for good: another owner took the workspace.
        let _newer = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let refused = install(&graph);
        assert!(
            !matches!(refused, crate::workspace_lease::LeaseOperationOutcome::Applied(())),
            "the fixture needs the superseded lease to refuse this install",
        );
        assert_eq!(
            lock_recover(&graph.debt).outstanding_recovery().keys,
            owed.keys,
            "an install refused for a lost workspace retired an obligation",
        );
        assert!(graph.owes_recovery(), "the chain ended on installs that never happened");
        drop(restore);
    }

    /// Real declarations, changed and changed back, with real builds installing each one.
    ///
    /// Nothing on the fact stream announces a rewritten project file, so the probe is the only
    /// owner such a change has: it measures the composition, buys ONE build, and that build's
    /// own result is what may answer an address the new declaration no longer asks for. The
    /// anchor beside it is never answered by any of that, and coming back to the first
    /// declaration is a new transition rather than an old witness resurrected.
    #[cfg(unix)]
    #[test]
    fn actual_roots_churn_installs_each_declaration_and_answers_only_what_it_served() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for part in ["src", "ext/a", "ext/b"] {
            fs::create_dir_all(root.join(part)).unwrap();
            fs::write(root.join(part).join("Configuration.xml"), "<Configuration/>").unwrap();
        }
        let declare = |extensions: &str| {
            fs::write(
                root.join("bsl-analyzer.toml"),
                format!("[source]\nroot = \"src\"\nextensions = [\n{extensions}]\n"),
            )
            .unwrap();
        };
        let both = "  { name = \"a\", path = \"ext/a\" },\n  { name = \"b\", path = \"ext/b\" },\n";
        let only_a = "  { name = \"a\", path = \"ext/a\" },\n";

        // One module under the root that is always declared — the anchor — and one under the
        // root that comes and goes. Both unreadable, so both are obligations.
        let anchor = root.join("src").join("CommonModules").join("Якорь").join("Ext");
        let visiting = root.join("ext/b").join("CommonModules").join("Гость").join("Ext");
        for dir in [&anchor, &visiting] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("Module.bsl"), "Функция Ф() Экспорт КонецФункции").unwrap();
        }
        // And a subtree under the always-declared root that cannot be walked at all, so every
        // publication here is one that cannot vouch for its own scan — which is what a walk is
        // owed for, and the only way a rewritten project file has an owner without an event.
        let closed = root.join("src").join("CommonModules").join("Закрытая");
        fs::create_dir_all(closed.join("Ext")).unwrap();
        fs::write(closed.join("Ext").join("Module.bsl"), "Функция Ф() Экспорт КонецФункции")
            .unwrap();
        let anchor_module = anchor.join("Module.bsl");
        let visiting_module = visiting.join("Module.bsl");
        let restore = RestoredModes::of(&[&anchor_module, &visiting_module, &closed]);
        for module in [&anchor_module, &visiting_module] {
            fs::set_permissions(module, fs::Permissions::from_mode(0o000)).unwrap();
        }
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&anchor_module).is_ok() || fs::read_dir(&closed).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        declare(both);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let outstanding = |graph: &GraphState| -> Vec<String> {
            let mut keys: Vec<String> = lock_recover(&graph.debt)
                .outstanding_recovery()
                .keys
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            keys.sort();
            keys
        };
        assert_eq!(outstanding(&graph).len(), 2, "both unreadable modules are obligations");
        let generation = |graph: &GraphState| {
            lock_recover(&graph.inner).published.as_ref().map(|p| p.generation).unwrap_or(0)
        };

        // The declaration narrows. No event announces it: the probe is the only owner.
        let before = generation(&graph);
        declare(only_a);
        wait_until_driving(
            &graph,
            "the narrowed declaration to be built",
            || {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.probe_recovery();
            },
            || generation(&graph) > before,
        );
        let after_narrowing = outstanding(&graph);
        assert_eq!(
            after_narrowing.len(),
            1,
            "the build that served the new declaration answered what it no longer asks for: {after_narrowing:?}",
        );
        assert!(
            after_narrowing[0].contains("Якорь"),
            "the anchor is the obligation that survives: {after_narrowing:?}",
        );

        // Standing still buys nothing: the same declaration, measured again, is not news.
        let quiet = generation(&graph);
        for _ in 0..3 {
            lock_recover(&graph.debt).probe_now(Instant::now());
            graph.probe_recovery();
        }
        assert_eq!(generation(&graph), quiet, "an unchanged declaration bought another build");

        // And back. The returning root is a NEW obligation, not the old one resurrected.
        declare(both);
        wait_until_driving(
            &graph,
            "the restored declaration to be built",
            || {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.probe_recovery();
            },
            || generation(&graph) > quiet,
        );
        let after_return = outstanding(&graph);
        assert_eq!(
            after_return.len(),
            2,
            "the root that came back is required again: {after_return:?}",
        );

        // What the memory holds is the workload, not its history: two obligations, and one
        // descriptor each for what is installed and what was last walked.
        let plan = lock_recover(&graph.debt).reserve_probe().expect("obligations stand");
        assert_eq!(plan.open.len(), 2, "the walk observes exactly what is outstanding");
        lock_recover(&graph.debt).release_probe(Instant::now(), plan.token, true);

        // Churn: the declaration goes back and forth, with a real build installing each one.
        // What is outstanding never grows past what is actually required, and the builds are
        // bounded by the transitions rather than by the number of passes.
        let mut builds = 0;
        let mut peak = outstanding(&graph).len();
        for round in 0..4 {
            let at = generation(&graph);
            declare(if round % 2 == 0 { only_a } else { both });
            wait_until_driving(
                &graph,
                "the churned declaration to be built",
                || {
                    lock_recover(&graph.debt).probe_now(Instant::now());
                    graph.probe_recovery();
                },
                || generation(&graph) > at,
            );
            builds += 1;
            peak = peak.max(outstanding(&graph).len());
            // The same declaration again is not another transition.
            for _ in 0..2 {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.probe_recovery();
            }
            assert_eq!(generation(&graph), at + 1, "round {round}: standing still bought a build",);
        }
        assert_eq!(builds, 4, "one build per real transition");
        assert_eq!(peak, 2, "the memory grew past what was required at once: {peak}");

        // And it shrinks when the work is really answered: the anchor becomes readable, one
        // build reads it, and nothing is left outstanding under the narrow declaration.
        declare(only_a);
        drop(restore);
        let at = generation(&graph);
        wait_until_driving(
            &graph,
            "the healed anchor to be read",
            || {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.probe_recovery();
            },
            || generation(&graph) > at && outstanding(&graph).is_empty(),
        );
    }

    /// A module reached through a real symlink is required by the walk that listed it.
    ///
    /// The durable recovery key keeps the walked spelling from the same traversal. A descendant
    /// reached through a link therefore remains under the declared source root even when its
    /// canonical target is outside that root. Recovery must ask coverage about that walked key,
    /// or the obligation the very same artefact declares unread would be retired as a root that
    /// had gone away, re-registered from the same list, and paid for again on the next pass.
    #[cfg(unix)]
    #[test]
    fn a_module_reached_through_a_symlink_is_not_a_root_that_went_away() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        let elsewhere = dir.path().join("вне");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("Configuration.xml"), "<Configuration/>").unwrap();
        fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \"src\"\n").unwrap();
        let target = elsewhere.join("CommonModules").join("Связь").join("Ext");
        fs::create_dir_all(&target).unwrap();
        let linked_module = target.join("Module.bsl");
        fs::write(&linked_module, "Функция Ф() Экспорт КонецФункции").unwrap();
        fs::create_dir_all(root.join("src").join("CommonModules")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.join("CommonModules").join("Связь"),
            root.join("src").join("CommonModules").join("Связь"),
        )
        .unwrap();

        let restore = RestoredModes::of(&[&linked_module]);
        fs::set_permissions(&linked_module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&linked_module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        let graph = GraphState::for_workspace(root.clone());
        graph.ensure_loading();
        wait_ready(&graph);
        let owed = lock_recover(&graph.debt).outstanding_recovery().keys;
        assert_eq!(owed.len(), 1, "the module behind the link is an obligation: {owed:?}");
        let key = owed[0].0.clone();
        let declared = super::super::snapshot::recovery_scope_of(
            &super::super::input::ProjectSnapshot::load(&root),
        );
        let expected = root.join("src/CommonModules/Связь/Ext/Module.bsl");
        assert!(
            std::path::Path::new(&key) == expected.as_path(),
            "the recovery key must keep the walked alias: {key}",
        );
        assert!(
            declared.requires(&key),
            "the declared source root must cover the walked alias: {key}",
        );

        // A real rebuild: the walk lists it, the artefact declares it unread, and it is
        // still owed afterwards — with the SAME occurrence, so looking again is not news.
        let before = lock_recover(&graph.inner).published.as_ref().map(|p| p.generation).unwrap();
        graph.nudge_project_reload();
        wait_until(&graph, "the forced rebuild to publish", || {
            lock_recover(&graph.inner).published.as_ref().is_some_and(|p| p.generation > before)
        });
        assert_eq!(
            lock_recover(&graph.debt).outstanding_recovery().keys,
            owed,
            "the address the walk listed was answered as a root that had gone away",
        );

        // ...and the same positive, measured again, buys nothing.
        lock_recover(&graph.debt).probe_now(Instant::now());
        drop(restore);
        let plan = lock_recover(&graph.debt).reserve_probe().expect("the obligation stands");
        let (levels, scope) = match graph.recovery_probe(&plan) {
            super::super::snapshot::ProbeOutcome::Looked { levels, scope } => (levels, scope),
            super::super::snapshot::ProbeOutcome::CouldNotLook => {
                panic!("the probe could not look")
            }
        };
        assert_eq!(
            lock_recover(&graph.debt).finish_probe(
                Instant::now(),
                super::super::debt::ProbeReceipt {
                    token: plan.token,
                    basis: plan.basis,
                    levels,
                    scope,
                },
            ),
            super::super::debt::ProbeResult::NewEvidence,
            "the module behind the link became readable, and that is its healing",
        );
    }

    /// The two admissions no test had ever reached carry the same mandate as the others.
    ///
    /// Five entries can grant a build a mandate, and a ledger test proves the arithmetic of
    /// none of them: the cutoff has to be fixed where the slot is granted, in the same hold,
    /// or a build answers origins it was never admitted for. The boot's own entry and the
    /// external claim were the two nothing had ever called with a healing standing.
    #[test]
    fn the_boot_and_the_external_claim_carry_the_healings_measured_before_them() {
        for entry in ["ensure_loading_claimed", "try_begin_external_build"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
            cache.ensure().unwrap();
            let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
            // The loader `ensure_loading_claimed` spawns parks before it takes its ticket, so the
            // grant is still in the slot to be read and nothing but this test writes to the debt
            // while it is being read.
            let release = Arc::new(AtomicBool::new(false));
            let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
                .with_lease(lease)
                .with_post_claim_hook({
                    let release = Arc::clone(&release);
                    Arc::new(move |_: &GraphState| {
                        let deadline = Instant::now() + Duration::from_secs(30);
                        while !release.load(Ordering::SeqCst) && Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    })
                });

            // An unsound publication on record, and a healing measured against it — before
            // anything is admitted.
            {
                let mut debt = lock_recover(&graph.debt);
                debt.record_publication(
                    Instant::now(),
                    Some(1),
                    false,
                    None,
                    left_unread(1, &["/ws/Модуль.bsl"]),
                );
                let plan = debt.reserve_probe().expect("the declared gap is owed a look");
                assert_eq!(
                    debt.finish_probe(
                        Instant::now(),
                        super::super::debt::ProbeReceipt {
                            token: plan.token,
                            basis: plan.basis,
                            levels: vec![(
                                super::super::debt::Capability::Open("/ws/Модуль.bsl".to_owned(),),
                                super::super::debt::Level::Granted,
                            )],
                            scope: None,
                        },
                    ),
                    super::super::debt::ProbeResult::NewEvidence,
                    "{entry}: the fixture needs a measured healing",
                );
            }
            let captured = lock_recover(&graph.debt).outstanding_recovery().captured_seq;
            assert!(captured > 0, "{entry}: the healing was issued no origin");
            assert!(
                lock_recover(&graph.debt).owes_recovery_build(),
                "{entry}: and it owes a build",
            );

            // Nothing is spent by deciding, only by granting.
            graph.drive_without_the_first_build();
            assert!(
                lock_recover(&graph.debt).owes_recovery_build(),
                "{entry}: a decision spent the credit",
            );

            let granted = match entry {
                "ensure_loading_claimed" => graph.ensure_loading_claimed(false),
                _ => graph.try_begin_external_build(),
            };
            assert!(granted, "{entry}: the slot was not granted");
            let ticket = graph.claimed_ticket().expect("{entry}: the grant carries a mandate");
            assert_eq!(ticket.recovery_cutoff, captured, "{entry}: the mandate carried no cutoff");
            assert!(ticket.forced, "{entry}: a measured healing is answered by a forced reload");

            // ...and a healing measured AFTER the grant keeps its own demand.
            {
                let mut debt = lock_recover(&graph.debt);
                debt.record_publication(
                    Instant::now(),
                    Some(1),
                    false,
                    None,
                    left_unread(2, &["/ws/Второй.bsl"]),
                );
                let plan = debt.reserve_probe().expect("the second gap is owed a look");
                debt.finish_probe(
                    Instant::now(),
                    super::super::debt::ProbeReceipt {
                        token: plan.token,
                        basis: plan.basis,
                        levels: vec![(
                            super::super::debt::Capability::Open("/ws/Второй.bsl".to_owned()),
                            super::super::debt::Level::Granted,
                        )],
                        scope: None,
                    },
                );
            }
            let proof = super::super::debt::RecoveryPublicationProof {
                generation: 3,
                captured_seq: ticket.recovery_cutoff,
                declared_unread: Some(vec!["/ws/Второй.bsl".to_owned()]),
                read_covered: vec![("/ws/Модуль.bsl".to_owned(), 1)],
                ..Default::default()
            };
            lock_recover(&graph.debt).record_publication(
                Instant::now(),
                Some(1),
                true,
                Some(ticket.recovery_cutoff),
                proof,
            );
            assert!(
                lock_recover(&graph.debt).owes_recovery_build(),
                "{entry}: the healing measured after the grant was answered by it",
            );
            release.store(true, Ordering::SeqCst);
            wait_until(&graph, "the parked loader, if any, to finish", || {
                let inner = lock_recover(&graph.inner);
                inner.building.is_none() && inner.claimed.is_none()
                    || entry == "try_begin_external_build"
            });
            graph.abort_external_build();
        }
    }

    /// A real builder's failure closes the account that really paid for it.
    ///
    /// The same rule as the ticket-level case, through the whole path: a build admitted on the
    /// marks' account alone, spawned, run, and refused at the install. Its outcome must not
    /// reach for a lane that bought nothing — here a retry budget that an operation error had
    /// already stopped, which a mis-reported sponsorship revives and puts back on the clock.
    #[cfg(unix)]
    #[test]
    fn a_real_builders_failure_closes_the_account_that_paid_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        // Marks nobody has consumed, and a primary retry lane an operation error stopped for
        // good: the marks are the only account that can buy the next build.
        let placed = Instant::now() - Duration::from_secs(60);
        {
            let mut debt = lock_recover(&graph.debt);
            debt.place_marks(placed, 11, 1);
            debt.settle_marks(placed + Duration::from_secs(30), false);
            debt.record_failure(
                Instant::now() - Duration::from_secs(700),
                super::super::debt::FailureKind::Operation,
                super::super::debt::Sponsors { primary: true, marks: false },
            );
        }
        let stopped = graph.debt_standing(Instant::now()).failed;
        assert!(
            matches!(stopped, Some(super::super::debt::Ripeness::Exhausted(_))),
            "the fixture needs a retry lane that is stopped, not merely paused: {stopped:?}",
        );
        assert_eq!(
            graph.debt_standing(Instant::now()).marks,
            Some(super::super::debt::Ripeness::Now),
            "and marks that are due, so the build below is theirs to buy",
        );

        // The build is admitted, spawned, and runs for real — into a cache directory it
        // cannot write, which is an operation error rather than a refusal to try again.
        use std::os::unix::fs::PermissionsExt;
        let cache_dir = graph.graph_db_path().expect("a workspace graph has a cache layout");
        let cache_dir = cache_dir.parent().expect("the graph lives in a directory").to_path_buf();
        let restore_cache = RestoredModes::of(&[&cache_dir]);
        fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o500)).unwrap();
        if fs::write(cache_dir.join("проба"), b"x").is_ok() {
            eprintln!("skipping: a read-only directory is not an obstacle for this user");
            return;
        }
        let generation =
            lock_recover(&graph.inner).published.as_ref().map(|p| p.generation).unwrap_or(0);
        drive_until(&graph, "the builder to report the operation error", || {
            matches!(
                lock_recover(&graph.inner).published.as_ref().map(|p| p.reload.clone()),
                Some(ReloadState::Failed(_))
            )
        });
        drop(restore_cache);
        assert_eq!(
            lock_recover(&graph.inner).published.as_ref().map(|p| p.generation),
            Some(generation),
            "the failed build must not have published",
        );

        assert_eq!(
            graph.debt_standing(Instant::now()).failed,
            stopped,
            "the outcome revived a retry lane that never paid for this build",
        );
        // An operation error closes the account that paid, and it is the marks' account here.
        // Reported against a lane that bought nothing, the marks keep their budget and go on
        // buying attempts for a build that cannot be made.
        assert!(
            !lock_recover(&graph.debt).marks_are_eligible(Instant::now() + Duration::from_secs(60)),
            "the account that paid for the build never heard how it ended",
        );
    }

    /// The same rule on every platform, through a failure that needs no file permissions: a
    /// real builder admitted on the marks' account alone, spawned, run and refused at its
    /// install. The refusal is its outcome, and a lane that bought nothing must not come out of
    /// it holding a retry budget.
    #[test]
    fn a_real_builders_refused_install_answers_for_the_account_that_paid_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        let placed = Instant::now() - Duration::from_secs(60);
        {
            let mut debt = lock_recover(&graph.debt);
            debt.place_marks(placed, 11, 1);
            debt.settle_marks(placed + Duration::from_secs(30), false);
        }
        assert_eq!(
            graph.debt_standing(Instant::now()).marks,
            Some(super::super::debt::Ripeness::Now),
            "the fixture needs marks that are due, so the build below is theirs to buy",
        );
        assert!(!lock_recover(&graph.debt).owes_failed(), "and no retry lane of any kind");

        let generation =
            lock_recover(&graph.inner).published.as_ref().map(|p| p.generation).unwrap_or(0);
        let started = graph.builders_started.load(Ordering::SeqCst);
        graph.refused_installs.store(1, Ordering::SeqCst);
        drive_until(&graph, "the builder to report the refused install", || {
            matches!(
                lock_recover(&graph.inner).published.as_ref().map(|p| p.reload.clone()),
                Some(ReloadState::Failed(_))
            )
        });
        assert_eq!(
            graph.builders_started.load(Ordering::SeqCst),
            started + 1,
            "the fixture needs a real builder to have run",
        );
        assert_eq!(graph.refused_installs.load(Ordering::SeqCst), 0, "and its install refused");
        assert_eq!(
            lock_recover(&graph.inner).published.as_ref().map(|p| p.generation),
            Some(generation),
            "the refused build must not have published",
        );
        assert!(
            !lock_recover(&graph.debt).owes_failed(),
            "the refusal of a marks-sponsored build minted a retry budget for a lane that paid nothing",
        );
    }

    /// A walk looks at the publication whose gaps it is walking for.
    ///
    /// The plan fixes what is being measured AND what it is being measured against. Acquiring
    /// whatever snapshot happens to be published instead, a pass stats the whole tree against
    /// one artefact and reports about another's gaps — the receipt is refused at completion
    /// for exactly that reason, and the walk it paid for on the way there is the avoidable
    /// part.
    #[cfg(unix)]
    #[test]
    fn a_walk_refuses_a_snapshot_of_another_publication() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // A subtree that cannot be walked: the publication owes a walk, so a pass has one to
        // pay for.
        let closed = root.join("CommonModules").join("Закрытая");
        fs::create_dir_all(closed.join("Ext")).unwrap();
        fs::write(closed.join("Ext").join("Module.bsl"), "Функция Ф() Экспорт КонецФункции")
            .unwrap();
        let restore = RestoredModes::of(&[&closed]);
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&closed).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(graph.owes_recovery(), "the fixture needs an outstanding walk");

        // A plan against the publication that stands...
        let plan = lock_recover(&graph.debt).reserve_probe().expect("the walk is owed");
        assert!(plan.scan.is_some(), "the fixture needs the plan to carry a walk");
        let walks = graph.scan_count();

        // ...and a real rebuild lands under it before the pass starts.
        drop(restore);
        let before = lock_recover(&graph.inner).published.as_ref().map(|p| p.generation).unwrap();
        graph.nudge_project_reload();
        wait_until(&graph, "the rebuild to publish", || {
            lock_recover(&graph.inner).published.as_ref().is_some_and(|p| p.generation > before)
        });

        let outcome = graph.recovery_probe(&plan);
        assert!(
            matches!(outcome, super::super::snapshot::ProbeOutcome::CouldNotLook),
            "a walk measured the tree against an artefact that is not the one it walks for",
        );
        assert_eq!(
            graph.scan_count(),
            walks,
            "and it paid for a walk of the whole tree before finding that out",
        );
        lock_recover(&graph.debt).release_probe(Instant::now(), plan.token, false);
    }

    /// A complete walk that no longer finds an address is what answers it, not a probe.
    ///
    /// The fourth positive, on a real build: the module that could not be read is deleted, and
    /// the publication whose walk covered the whole of the scope that required it is the thing
    /// that retires the obligation. Before that build, a probe finding the file gone is an
    /// observation and nothing more.
    #[cfg(unix)]
    #[test]
    fn a_complete_build_answers_an_address_that_is_gone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");
        let restore = RestoredModes::of(&[&module]);
        fs::set_permissions(&module, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&module).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let owed = lock_recover(&graph.debt).outstanding_recovery().keys;
        assert_eq!(owed.len(), 1, "the unreadable module is an obligation");

        // Gone from disk. A probe sees that, and seeing it is not an answer.
        drop(restore);
        fs::remove_file(&module).unwrap();
        lock_recover(&graph.debt).probe_now(Instant::now());
        let plan = lock_recover(&graph.debt).reserve_probe().expect("the obligation stands");
        let (levels, scope) = match graph.recovery_probe(&plan) {
            super::super::snapshot::ProbeOutcome::Looked { levels, scope } => (levels, scope),
            super::super::snapshot::ProbeOutcome::CouldNotLook => {
                panic!("the probe could not look")
            }
        };
        assert!(
            levels.iter().any(|(_, level)| *level == super::super::debt::Level::Absent),
            "the fixture needs the walk to find it gone",
        );
        lock_recover(&graph.debt).finish_probe(
            Instant::now(),
            super::super::debt::ProbeReceipt {
                token: plan.token,
                basis: plan.basis,
                levels,
                scope,
            },
        );
        assert_eq!(
            lock_recover(&graph.debt).outstanding_recovery().keys,
            owed,
            "a probe that found the address gone retired it by itself",
        );

        // And the build that walked the whole of the scope is what answers it.
        let before = lock_recover(&graph.inner).published.as_ref().map(|p| p.generation).unwrap();
        graph.nudge_project_reload();
        wait_until(&graph, "the rebuild over the deleted module to publish", || {
            lock_recover(&graph.inner).published.as_ref().is_some_and(|p| p.generation > before)
        });
        assert!(
            lock_recover(&graph.debt).outstanding_recovery().keys.is_empty(),
            "a complete walk of the scope that required it did not answer an address that is gone",
        );
    }

    /// Deciding, holding and failing to start spend no recovery origin.
    ///
    /// A pending origin is spent where a slot is actually granted and nowhere else. The three
    /// ways a build can fail to happen — the decision that takes no slot, the claim the lease
    /// could not confirm, and the thread that never started — must leave the healing exactly
    /// as demanding as it was, or the one event that can revive this graph arrives already
    /// spent.
    #[test]
    fn a_build_that_never_started_spends_no_healing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        // A healing on record, measured through the production pass.
        {
            let mut debt = lock_recover(&graph.debt);
            debt.record_publication(
                Instant::now(),
                Some(graph.observation()),
                false,
                None,
                left_unread(99, &["/ws/Модуль.bsl"]),
            );
            let plan = debt.reserve_probe().expect("the declared gap is owed a look");
            debt.finish_probe(
                Instant::now(),
                super::super::debt::ProbeReceipt {
                    token: plan.token,
                    basis: plan.basis,
                    levels: vec![(
                        super::super::debt::Capability::Open("/ws/Модуль.bsl".to_owned()),
                        super::super::debt::Level::Granted,
                    )],
                    scope: None,
                },
            );
        }
        let demanded = lock_recover(&graph.debt).outstanding_recovery().captured_seq;
        assert!(demanded > 0, "the fixture needs a measured healing");
        let unspent = |graph: &GraphState, what: &str| {
            assert!(
                lock_recover(&graph.debt).owes_recovery_build(),
                "{what}: the healing was spent by a build that never happened",
            );
        };

        // Deciding is not claiming.
        graph.debt_standing(Instant::now());
        lock_recover(&graph.debt).decide(Instant::now(), graph.facts());
        unspent(&graph, "a decision");

        // A claim the lease could not confirm.
        graph.claim_is_held.store(true, Ordering::SeqCst);
        graph.drive();
        unspent(&graph, "a held claim");

        // And a builder that could not be spawned at all: charged for its admission, so the
        // demand it was admitted for is answered by that build's outcome and not before it.
        //
        // Asserted as three separate statements, because the disjunction they used to be was
        // satisfied by the healing alone — on a published graph the drive dispatches a RELOAD,
        // whose spawn had no seam at all, so the leg that was supposed to prove what a refused
        // thread leaves behind quietly ran an ordinary build instead.
        let started = graph.builders_started.load(Ordering::SeqCst);
        graph.loader_cannot_spawn.store(true, Ordering::SeqCst);
        graph.drive();
        graph.loader_cannot_spawn.store(false, Ordering::SeqCst);
        assert_eq!(
            graph.builders_started.load(Ordering::SeqCst),
            started,
            "a builder started where the stand asked for a refused spawn",
        );
        assert!(
            matches!(
                lock_recover(&graph.inner).published.as_ref().map(|p| p.reload.clone()),
                Some(ReloadState::Failed(_))
            ),
            "the refused spawn left no failure on the slot it had claimed: {:?}",
            lock_recover(&graph.inner).published.as_ref().map(|p| p.reload.clone()),
        );
        assert!(graph.owes_failed(), "a builder that never started left no retry behind it");
        unspent(&graph, "a refused spawn");
    }

    /// What a real point patch can and cannot answer, on the path that actually runs it.
    ///
    /// The producer has a branch for it: an address the patch REWROTE and the artefact no
    /// longer lists unread is read-covered. This drives the whole native path — a real
    /// database, a real body-only edit, the real eligibility gates — and records which branch
    /// the build took, because a full rebuild quietly standing in for a patch would answer the
    /// same obligations and prove nothing about the patch.
    ///
    /// The eligibility gates are production's own and are not bent here. What the run below
    /// actually shows: the ledger starts with two synthetic unread obligations after a fully
    /// readable initial publication, and a body edit of one of them takes the point path — the
    /// patch rewrites it and answers its obligation. What does NOT reach this path is a healing
    /// measured by a probe: that makes the reload forced, and a forced reload never looks at the
    /// point path at all. Which branch ran is recorded rather than assumed.
    #[test]
    fn a_point_patch_answers_what_it_rewrote_and_nothing_else() {
        use super::super::test_support::{wait_publish_pass_within, WAIT_CEILING};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // Enough modules that a caller-delta fan-out is not "most of the config".
        for i in 0..6 {
            super::super::test_support::write_common_module(
                root,
                &format!("Полный{i}"),
                true,
                "&НаСервере
Функция Взять() Экспорт Возврат 1; КонецФункции",
            );
        }
        let hidden = root.join("CommonModules").join("Клиент").join("Ext").join("Module.bsl");
        let other = root.join("CommonModules").join("Полный0").join("Ext").join("Module.bsl");
        let edited = root.join("CommonModules").join("Полный1").join("Ext").join("Module.bsl");

        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        graph.set_watch(super::super::watcher::WatchPhase::Running, None);
        graph.ensure_loading();
        wait_ready(&graph);
        wait_publish_pass_within(&graph, WAIT_CEILING, 1);
        let generation = |graph: &GraphState| {
            lock_recover(&graph.inner).published.as_ref().map(|p| p.generation).unwrap_or(0)
        };
        let initial_generation = generation(&graph);
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(graph.observation()),
            false,
            None,
            super::super::debt::RecoveryPublicationProof {
                generation: initial_generation,
                declared_unread: Some(vec![
                    hidden.to_string_lossy().into_owned(),
                    other.to_string_lossy().into_owned(),
                ]),
                ..Default::default()
            },
        );
        let outstanding = |graph: &GraphState| -> Vec<String> {
            let mut keys: Vec<String> = lock_recover(&graph.debt)
                .outstanding_recovery()
                .keys
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            keys.sort();
            keys
        };
        assert_eq!(outstanding(&graph).len(), 2, "two synthetic unread obligations");

        // A body-only edit of a module nobody is owed anything about: the point path is
        // eligible, and this is what a real patch looks like.
        let before = generation(&graph);
        let passes = graph.publish_passes.load(Ordering::SeqCst);
        let observed = hub.seq();
        lock_recover(&graph.incremental_decisions).clear();
        fs::write(&edited, "&НаСервере\nФункция Взять() Экспорт Возврат 2; КонецФункции").unwrap();
        crate::graph::test_support::wait_for_hub_seq_above(&hub, observed);
        graph.nudge_rebuild();
        wait_until(&graph, "the body-only edit to be published", || generation(&graph) > before);
        wait_publish_pass_within(&graph, WAIT_CEILING, passes + 1);
        let decisions = lock_recover(&graph.incremental_decisions).clone();
        assert_eq!(
            decisions.last().copied(),
            Some("published"),
            "the fixture needs a REAL point patch, not a full rebuild standing in for one: \
             {decisions:?}",
        );
        assert_eq!(
            outstanding(&graph).len(),
            2,
            "a patch that rewrote something else answered an obligation it never looked at",
        );

        // And what it CAN answer: an obligated address it actually rewrote. An ordinary drift
        // — not a measured healing — brings it into the patch's own rewritten set.
        lock_recover(&graph.incremental_decisions).clear();
        let at = generation(&graph);
        let passes = graph.publish_passes.load(Ordering::SeqCst);
        let observed = hub.seq();
        fs::write(&other, "&НаСервере\nФункция Взять() Экспорт Возврат 3; КонецФункции").unwrap();
        crate::graph::test_support::wait_for_hub_seq_above(&hub, observed);
        graph.nudge_rebuild();
        wait_until(&graph, "the rewritten module to be published", || generation(&graph) > at);
        wait_publish_pass_within(&graph, WAIT_CEILING, passes + 1);
        let decisions = lock_recover(&graph.incremental_decisions).clone();
        assert_eq!(
            decisions.last().copied(),
            Some("published"),
            "the obligation was answered by a full rebuild, not by the patch under test: \
             {decisions:?}",
        );
        let left = outstanding(&graph);
        assert_eq!(
            left.len(),
            1,
            "the patch answered exactly what it rewrote and nothing else: {left:?}",
        );
        assert!(
            left[0].contains("Клиент"),
            "the address the patch never looked at survives it: {left:?}",
        );
        assert!(
            graph.owes_recovery(),
            "the chain stands while an address nobody has read is still outstanding",
        );
    }

    /// The marks' cap and the recovery workload are two different bounds, and retirement
    /// moves neither.
    ///
    /// One is a CAP: past 1024 placements the ledger merges its two oldest conservatively, so
    /// a mark is consumed late at worst. The other must not be a cap at all — every address a
    /// publication could not read is owed an observation, however many there are. Run
    /// together, because the thing to prove is that neither one starts standing in for the
    /// other: retiring recovery obligations must not consume a mark, expand a consumption
    /// watermark, or tidy the ledger; and the marks' cap must not bound what recovery
    /// remembers.
    #[test]
    fn a_saturated_mark_ledger_and_a_retiring_recovery_set_bound_each_other_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // A real hub, so the facts the marks are placed for are real positions rather than
        // the no-stream sentinel a workspace without one reports.
        let hub = crate::graph::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        graph.set_watch(super::super::watcher::WatchPhase::Running, None);
        graph.ensure_loading();
        wait_ready(&graph);
        let observed = graph.observation();

        // More outstanding recovery obligations than the marks' cap, declared the way an
        // unsound publication declares them.
        const OUTSTANDING: usize = 1200;
        let unread: Vec<String> =
            (0..OUTSTANDING).map(|i| format!("/ws/CommonModules/М{i:05}/Ext/Module.bsl")).collect();
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(observed),
            false,
            None,
            super::super::debt::RecoveryPublicationProof {
                generation: 2,
                declared_unread: Some(unread.clone()),
                ..Default::default()
            },
        );
        // ...and more placements than the cap, through the caller that places them. Each is
        // for a fact NO publication has observed, so none of them may be consumed here.
        const PLACED: usize = 1100;
        for i in 0..PLACED {
            graph.marks_placed(i as i64 + 1, observed + 1 + i as u64);
        }

        let before = {
            let debt = lock_recover(&graph.debt);
            (debt.marks.placements(), debt.marks.demanded(), debt.outstanding_recovery().keys.len())
        };
        assert_eq!(
            before.0,
            super::super::debt::MARK_LEDGER_CAP,
            "the marks ledger is a cap and holds at it: {} placements",
            before.0,
        );
        assert_eq!(
            before.2, OUTSTANDING,
            "every address the publication could not read is remembered, cap or no cap",
        );
        assert!(
            before.2 > before.0,
            "the fixture needs more obligations than the marks' cap, or it proves nothing",
        );
        assert!(graph.marks_pending(), "the marks are placed and unconsumed");

        // A publication that answers 700 of the obligations — and observes nothing new.
        let outstanding = lock_recover(&graph.debt).outstanding_recovery();
        let answered: Vec<(String, u64)> =
            outstanding.keys.iter().take(700).cloned().collect::<Vec<_>>();
        let still_unread: Vec<String> =
            outstanding.keys.iter().skip(700).map(|(key, _)| key.clone()).collect();
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(observed),
            false,
            None,
            super::super::debt::RecoveryPublicationProof {
                generation: 3,
                captured_seq: u64::MAX,
                declared_unread: Some(still_unread),
                read_covered: answered,
                ..Default::default()
            },
        );

        let after = {
            let debt = lock_recover(&graph.debt);
            (debt.marks.placements(), debt.marks.demanded(), debt.outstanding_recovery().keys.len())
        };
        assert_eq!(
            after.2,
            OUTSTANDING - 700,
            "the retirement answered exactly what the proof covered",
        );
        assert_eq!(
            after.2,
            OUTSTANDING - 700,
            "what is left is the workload minus what was answered — 500 here, which is BELOW \
             the marks' cap of 1024 and not meant to clear it: the separation this asserts is \
             that 1200 were remembered while the ledger held 1024, and that the retirement \
             moved the recovery count by exactly what it covered",
        );
        assert_eq!(
            after.0, before.0,
            "retiring recovery obligations consumed or merged a placement",
        );
        assert_eq!(after.1, before.1, "and moved the demand the marks are owed for");
        assert!(graph.marks_pending(), "a recovery retirement consumed marks it never observed");

        // The consumption watermark is still what the marks themselves say: nothing here
        // observed the facts they were placed for.
        assert_eq!(
            lock_recover(&graph.debt).marks.bound(observed),
            None,
            "a publication that observed none of the placed facts was given a bound",
        );
        assert_eq!(
            lock_recover(&graph.debt).marks.bound(observed + 1 + PLACED as u64),
            Some(PLACED as i64),
            "and a publication that observed all of them may consume up to the highest",
        );
    }

    /// Real declared roots, changed and changed back: A → B → A.
    ///
    /// Each transition is a measured change of composition. The return to A is a NEW
    /// transition, not an old witness resurrected — and what a narrowed declaration no longer
    /// asks for is answered only when the declaration was actually read.
    #[test]
    fn actual_roots_a_b_a_retire_only_answered_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Extensions BESIDE the configuration root, so dropping one from the declaration
        // really drops a root instead of leaving it covered by the workspace root.
        for part in ["src", "ext/a", "ext/b"] {
            fs::create_dir_all(root.join(part)).unwrap();
            fs::write(root.join(part).join("Configuration.xml"), "<Configuration/>").unwrap();
        }
        let declare = |extensions: &str| {
            fs::write(
                root.join("bsl-analyzer.toml"),
                format!("[source]\nroot = \"src\"\nextensions = [\n{extensions}]\n"),
            )
            .unwrap();
        };
        let both = "  { name = \"a\", path = \"ext/a\" },\n  { name = \"b\", path = \"ext/b\" },\n";
        let only_a = "  { name = \"a\", path = \"ext/a\" },\n";
        declare(both);
        let graph = GraphState::for_workspace(root.to_path_buf());

        let (scope_a, _) = graph.walk_scan_receipt().expect("a workspace walks");
        assert!(scope_a.is_validated(), "the fixture declares a project that reads");

        // B: the same workspace with one extension taken out of the declaration.
        declare(only_a);
        let (scope_b, _) = graph.walk_scan_receipt().expect("a workspace walks");
        assert_ne!(scope_a, scope_b, "removing a declared root changed nothing");

        // Back to A. The same composition as before — and reaching it is its own transition.
        declare(both);
        let (scope_a_again, _) = graph.walk_scan_receipt().expect("a workspace walks");
        assert_eq!(scope_a_again, scope_a, "the same declaration is the same scope");

        // A module under the root B dropped is answered by B, and only while B is what a
        // publication actually served.
        let key = root
            .join("ext/b")
            .canonicalize()
            .unwrap_or_else(|_| root.join("ext/b"))
            .join("CommonModules/Тест/Ext/Module.bsl")
            .to_string_lossy()
            .into_owned();
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(1),
            false,
            None,
            super::super::debt::RecoveryPublicationProof {
                generation: 1,
                declared_unread: Some(vec![key.clone()]),
                ..Default::default()
            },
        );
        let enumerated: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let unread: Vec<String> = Vec::new();
        let under_a = graph.recovery_proof(
            2,
            Some(&unread),
            super::super::snapshot::RecoveryCoverage::Walked {
                scope: scope_a.clone(),
                enumerated: &enumerated,
                complete: false,
                straddled: false,
            },
        );
        assert!(
            under_a.out_of_scope_covered.is_empty(),
            "a scope that still declares the root answered it away: roots {scope_a:?}, key {key}",
        );
        let under_b = graph.recovery_proof(
            2,
            Some(&unread),
            super::super::snapshot::RecoveryCoverage::Walked {
                scope: scope_b,
                enumerated: &enumerated,
                complete: false,
                straddled: false,
            },
        );
        assert_eq!(
            under_b.out_of_scope_covered.len(),
            1,
            "the narrowed declaration still asks for a root it dropped",
        );
    }

    /// An invalid project restricts the walk; it does not retire the roots it can no longer
    /// see.
    ///
    /// The fallback is what the loader does when it cannot read the declaration. A root it
    /// never looked for is not a root that went away, and a module under one is still owed an
    /// observation — which is found again, without any event, once the declaration reads.
    #[test]
    fn invalid_project_fallback_is_not_root_retirement() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::test_support::write_extension_workspace(root, false);
        let graph = GraphState::for_workspace(root.to_path_buf());
        let (declared, _) = graph.walk_scan_receipt().expect("a workspace walks");
        assert!(declared.is_validated());

        // A configuration that will not parse.
        fs::write(root.join("bsl-analyzer.toml"), "[source\nroot = \"").unwrap();
        let (restricted, _) = graph.walk_scan_receipt().expect("even a broken project walks");
        assert!(
            !restricted.is_validated(),
            "a restricted fallback passed itself off as a declaration",
        );

        let key =
            root.join("ext/b/CommonModules/Тест/Ext/Module.bsl").to_string_lossy().into_owned();
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(1),
            false,
            None,
            super::super::debt::RecoveryPublicationProof {
                generation: 1,
                declared_unread: Some(vec![key.clone()]),
                ..Default::default()
            },
        );
        let enumerated: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let unread: Vec<String> = Vec::new();
        let under_fallback = graph.recovery_proof(
            2,
            Some(&unread),
            super::super::snapshot::RecoveryCoverage::Walked {
                scope: restricted,
                enumerated: &enumerated,
                complete: true,
                straddled: false,
            },
        );
        assert!(
            under_fallback.out_of_scope_covered.is_empty()
                && under_fallback.absent_covered.is_empty(),
            "a restricted fallback answered an obligation it never looked for",
        );
        assert!(
            lock_recover(&graph.debt)
                .outstanding_recovery()
                .keys
                .iter()
                .any(|(outstanding, _)| outstanding == &key),
            "the obligation was dropped while the project could not be read",
        );
    }

    /// What a real publication's critical section costs, sampled from INSIDE it.
    ///
    /// Where the answered obligations are freed is the property, and from outside the call
    /// the two placements are indistinguishable — the payload is dropped before
    /// `install_prepared_snapshot` returns either way. Sampled inside the section instead:
    /// with the payload carried out, the bytes of every retired key and of the consumed proof
    /// are still live while the gate, the lease and `inner` are held, and go afterwards.
    #[cfg(unix)]
    #[test]
    fn a_real_publication_frees_what_it_retired_after_its_locks() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // Enough unread addresses for the retirement to be worth measuring, all real.
        let modules = root.join("CommonModules");
        let mut hidden = Vec::new();
        for i in 0..64 {
            let module = modules.join(format!("Скрытый{i:03}")).join("Ext");
            fs::create_dir_all(&module).unwrap();
            let file = module.join("Module.bsl");
            fs::write(&file, "Функция Считать() Экспорт Возврат 1; КонецФункции").unwrap();
            hidden.push(file);
        }
        let borrowed: Vec<&Path> = hidden.iter().map(std::path::PathBuf::as_path).collect();
        let restore = RestoredModes::of(&borrowed);
        for file in &hidden {
            fs::set_permissions(file, fs::Permissions::from_mode(0o000)).unwrap();
        }
        if fs::read(&hidden[0]).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }

        /// What the publishing thread had allocated at each named point of its publication.
        type Marks = std::collections::BTreeMap<&'static str, crate::measured_alloc::Sample>;
        let marks: Arc<std::sync::Mutex<Marks>> = Arc::new(std::sync::Mutex::new(Marks::new()));
        let section_hook = {
            let marks = Arc::clone(&marks);
            Arc::new(move |point: &'static str| {
                lock_recover(&marks).entry(point).or_insert_with(crate::measured_alloc::sample);
            }) as Arc<dyn Fn(&'static str) + Send + Sync>
        };
        let window_hook = {
            let marks = Arc::clone(&marks);
            Arc::new(move || {
                lock_recover(&marks)
                    .entry("returned")
                    .or_insert_with(crate::measured_alloc::sample);
            }) as Arc<dyn Fn() + Send + Sync>
        };
        // Armed on the building thread, which is the one that publishes.
        let arm_hook = Arc::new(|_: &GraphState| crate::measured_alloc::arm()) as PostClaimHook;
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_install_section_hook(section_hook)
            .with_publish_window_hook(window_hook)
            .with_post_claim_hook(arm_hook);

        graph.ensure_loading();
        wait_ready(&graph);
        wait_until(&graph, "the initial publication to return", || {
            lock_recover(&marks).contains_key("returned")
        });
        // The initial publication establishes the unread obligations; only the next one can
        // retire them. Keep its samples out of the recovery measurement below.
        lock_recover(&marks).clear();
        let owed = lock_recover(&graph.debt).outstanding_recovery().keys;
        let outstanding = owed.len();
        assert_eq!(outstanding, hidden.len(), "every unreadable module is an obligation");
        // What the payload owns at the very least: one String per retired address, and the
        // copy of it the proof carries. Freeing that is the work the section used to do.
        let key_bytes: usize = owed.iter().map(|(key, _)| key.len()).sum();

        // They all heal, one real pass measures it, and the build that follows reads them.
        drop(restore);
        wait_until_driving(
            &graph,
            "the healed modules to be read",
            || {
                lock_recover(&graph.debt).probe_now(Instant::now());
                graph.probe_recovery();
            },
            || graph.snapshot().is_some_and(|snapshot| snapshot.unread_files() == 0),
        );
        wait_until(&graph, "the healing publication to return", || {
            lock_recover(&marks).contains_key("returned")
        });

        let marks = lock_recover(&marks).clone();
        let at = |point: &str| {
            marks.get(point).copied().unwrap_or_else(|| panic!("the publication passed {point}"))
        };
        // What the payload's own release frees. The two placements differ HERE and nowhere
        // else: dropped inside the section, there is nothing left for this line to free.
        let freed_by_the_drop = at("gate-released").live - at("returned").live;
        // Each retired address is owned twice at that moment — once by the key lifted out of
        // the map, once by the copy the proof carries — and both go with the payload.
        let owned = 2 * key_bytes as isize;
        assert!(
            freed_by_the_drop >= owned,
            "the answered obligations were freed inside the locks: releasing the payload \
             afterwards freed {freed_by_the_drop} bytes, and the {outstanding} retired \
             addresses own at least {owned} of them",
        );
        assert!(
            lock_recover(&graph.debt).outstanding_recovery().keys.is_empty(),
            "the build read every address, so nothing is outstanding",
        );
        // And the cost that is NOT the ledger's: the walk of the whole workspace, which every
        // build pays whatever recovery is outstanding. Measured here so the two are never
        // reported as one number.
        crate::measured_alloc::arm();
        let before_walk = crate::measured_alloc::sample();
        let walked = graph.walk_scan_receipt().expect("a workspace walks");
        let after_walk = crate::measured_alloc::sample();
        crate::measured_alloc::disarm();
        eprintln!(
            "publication section: {outstanding} obligations retired, {key_bytes} bytes of keys; \
             {} blocks allocated on the building thread; {freed_by_the_drop} bytes freed by \
             the payload's own release, after the gate was given up. \
             One whole-workspace walk beside it ({} roots, complete={}): {} blocks / peak {} \
             bytes — the scan's own cost, which no recovery bound changes",
            at("under-locks").blocks,
            walked.0.roots_len(),
            walked.1,
            after_walk.blocks - before_walk.blocks,
            after_walk.peak,
        );
        crate::measured_alloc::disarm();
    }

    /// What the ledger actually costs, measured rather than asserted away.
    ///
    /// Bounded by what is OUTSTANDING, not by how much has ever passed through: a churn of
    /// answered addresses leaves the map at its anchor, and every address of a workload that
    /// does not shrink is still there — which is the point, and what a cap used to hide.
    ///
    /// Measured, all of it: the opens are real `File::open` calls on real files, the walks are
    /// counted where the walk happens, and the allocations come from an allocator that counts
    /// this thread's. A number printed from the length of a list is not a measurement of
    /// anything.
    #[test]
    fn native_memory_and_probe_costs_are_proportional_to_the_outstanding_set() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf());
        // Real addresses, so the pass below really opens them. Half of them exist and half do
        // not: both answers come from the filesystem, and both cost an open.
        let modules = dir.path().join("CommonModules");
        fs::create_dir_all(&modules).unwrap();
        const CELLS: usize = 512;
        let required: Vec<String> = (0..CELLS)
            .map(|i| {
                let path = modules.join(format!("М{i:05}.bsl"));
                if i % 2 == 0 {
                    fs::write(&path, "Процедура П() КонецПроцедуры").unwrap();
                }
                path.to_string_lossy().into_owned()
            })
            .collect();
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(1),
            false,
            None,
            super::super::debt::RecoveryPublicationProof {
                generation: 1,
                declared_unread: Some(required.clone()),
                scan_complete: Some(false),
                scope: Some(super::super::debt::RecoveryScope::of(
                    std::slice::from_ref(&modules),
                    &[],
                    true,
                )),
                ..Default::default()
            },
        );

        let outstanding = lock_recover(&graph.debt).outstanding_recovery();
        assert_eq!(outstanding.keys.len(), required.len(), "every declared gap is remembered");
        let bytes: usize = outstanding.keys.iter().map(|(key, _)| key.len()).sum();

        // What one pass costs, on this thread, with the debt held for the reservation.
        crate::measured_alloc::arm();
        let reserved = crate::measured_alloc::sample();
        let plan = lock_recover(&graph.debt).reserve_probe().expect("the obligations stand");
        let planned = crate::measured_alloc::sample();
        assert_eq!(
            plan.open.len(),
            required.len(),
            "the walk observes every outstanding address, not the newest list",
        );

        // The pass itself: an actual open per address, and an actual walk for the scope.
        let walks_before = graph.scan_count();
        let started = Instant::now();
        let outcome = graph.recovery_probe(&plan);
        let looking = started.elapsed();
        let (opens, walked) = match outcome {
            super::super::snapshot::ProbeOutcome::Looked { levels, scope } => (levels, scope),
            super::super::snapshot::ProbeOutcome::CouldNotLook => {
                panic!("the probe could not look at a workspace it owns")
            }
        };
        let walks = graph.scan_count() - walks_before;
        let opened = |wanted: super::super::debt::Level| {
            opens
                .iter()
                .filter(|(capability, level)| {
                    matches!(capability, super::super::debt::Capability::Open(_))
                        && *level == wanted
                })
                .count()
        };
        let granted = opened(super::super::debt::Level::Granted);
        let absent = opened(super::super::debt::Level::Absent);
        assert_eq!(
            granted + absent,
            required.len(),
            "the pass did not actually open every address it was given",
        );
        assert_eq!(granted, CELLS / 2, "the files that exist opened");
        assert_eq!(absent, CELLS - CELLS / 2, "and the ones that do not are told apart");
        assert_eq!(walks, 1, "one walk obligation, one walk: {walks}");
        assert!(walked.is_some(), "the walk came back with the scope it covered");
        lock_recover(&graph.debt).release_probe(Instant::now(), plan.token, true);

        // And what the critical section itself does: the answered obligations leave the map
        // inside it, and everything they own is thrown away OUTSIDE it.
        let answered: Vec<(String, u64)> = lock_recover(&graph.debt)
            .outstanding_recovery()
            .keys
            .into_iter()
            .filter(|(key, _)| key != &required[0])
            .collect();
        crate::measured_alloc::reset_peak();
        let before_install = crate::measured_alloc::sample();
        let section = Instant::now();
        let retired = lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(2),
            true,
            Some(0),
            super::super::debt::RecoveryPublicationProof {
                generation: 2,
                captured_seq: u64::MAX,
                declared_unread: Some(vec![required[0].clone()]),
                read_covered: answered,
                ..Default::default()
            },
        );
        let held = section.elapsed();
        let inside = crate::measured_alloc::sample();
        drop(retired);
        let after_drop = crate::measured_alloc::sample();

        crate::measured_alloc::disarm();

        let after = lock_recover(&graph.debt).outstanding_recovery();
        assert_eq!(
            after.keys.len(),
            1,
            "the answered addresses are still remembered: {} cells",
            after.keys.len(),
        );
        assert!(
            after_drop.live < inside.live,
            "the answered obligations were freed inside the section, not after it: \
             {} live bytes inside, {} after the payload was dropped",
            inside.live,
            after_drop.live,
        );

        eprintln!(
            "recovery ledger: {cells} cells, {bytes} bytes of keys; \
             plan clone {plan_blocks} blocks / {plan_bytes} bytes; \
             pass {opens} opens + {walks} walk in {looking:?}; \
             ledger section {section_blocks} blocks / peak {section_peak} bytes in {held:?}; \
             retired payload freed outside it: {freed} bytes",
            cells = outstanding.keys.len(),
            plan_blocks = planned.blocks - reserved.blocks,
            plan_bytes = planned.live - reserved.live,
            opens = granted + absent,
            section_blocks = inside.blocks - before_install.blocks,
            section_peak = inside.peak - before_install.live,
            freed = inside.live - after_drop.live,
        );
    }

    /// The publish pass and an executor turn reaching the hook together: one offer, fired
    /// once, and no deadlock between them.
    ///
    /// The pair the executor-against-executor barrier does not cover. The pass holds the gate
    /// across its own hook; a turn arriving inside that window must find the offer owned and
    /// leave it, and the bits raised meanwhile must survive to be offered afterwards.
    #[test]
    fn an_executor_turn_during_a_publish_pass_offers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let fires = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let inside = Arc::new(AtomicBool::new(false));
        let turn_done = Arc::new(AtomicBool::new(false));
        let hook = {
            let (fires, armed, inside, turn_done) = (
                Arc::clone(&fires),
                Arc::clone(&armed),
                Arc::clone(&inside),
                Arc::clone(&turn_done),
            );
            Arc::new(move |_: GraphPublishSignal| {
                if !armed.load(Ordering::SeqCst) {
                    return GraphPublishOutcome::HANDLED;
                }
                fires.fetch_add(1, Ordering::SeqCst);
                if !inside.swap(true, Ordering::SeqCst) {
                    assert!(
                        crate::change_hub::test_support::eventually(
                            Duration::from_secs(30),
                            || turn_done.load(Ordering::SeqCst)
                        ),
                        "the executor turn never finished",
                    );
                }
                GraphPublishOutcome::HANDLED
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        // Ready is written by the install, and the load's own publish pass runs after it. Armed
        // before that pass is over, the hook would count the load's offer as this test's.
        super::super::test_support::wait_publish_pass_within(
            &graph,
            super::super::test_support::WAIT_CEILING,
            1,
        );
        armed.store(true, Ordering::SeqCst);

        let during_turn = Arc::new(AtomicUsize::new(usize::MAX));
        let turn = {
            let (graph, inside, turn_done, fires, during_turn) = (
                graph.clone(),
                Arc::clone(&inside),
                Arc::clone(&turn_done),
                Arc::clone(&fires),
                Arc::clone(&during_turn),
            );
            std::thread::spawn(move || {
                assert!(
                    crate::change_hub::test_support::eventually(Duration::from_secs(30), || {
                        inside.load(Ordering::SeqCst)
                    }),
                    "the publish pass never reached its hook",
                );
                // Raised while the pass owns the offer, and taken by nobody until it returns.
                graph.record_hook_debt(HookDebt { topology: true, roots: false, marks: false });
                let before = fires.load(Ordering::SeqCst);
                graph.drive();
                during_turn.store(fires.load(Ordering::SeqCst) - before, Ordering::SeqCst);
                turn_done.store(true, Ordering::SeqCst);
            })
        };

        graph.notify_published(false);
        turn.join().expect("the executor turn finished");

        assert_eq!(
            during_turn.load(Ordering::SeqCst),
            0,
            "an executor turn fired the hook while the publish pass owned the offer",
        );
        // And what it raised is not lost: the pass's own trailing turn offers it once the
        // gate is free again.
        assert_eq!(
            fires.load(Ordering::SeqCst),
            2,
            "the bits raised during the pass were never offered",
        );
        assert!(!graph.hook_debt().topology, "and the offer that followed was taken");
    }

    /// Only an actual open failure is a negative.
    ///
    /// A module whose bytes will not decode is unread for a reason that says nothing about
    /// whether it OPENS — and it opens, every time. Its first measurement is first knowledge
    /// and buys one build; every later pass over the same state buys nothing, however many
    /// unsound generations go by.
    #[test]
    fn only_an_actual_open_failure_is_a_negative() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // Valid UTF-16 to a text editor, invalid UTF-8 to a reader: the build cannot decode
        // it, the filesystem opens it without complaint.
        let module = root.join("CommonModules").join("Кривой").join("Ext");
        fs::create_dir_all(&module).unwrap();
        let path = module.join("Module.bsl");
        fs::write(&path, [0xff, 0xfe, 0x41, 0x00, 0x42, 0x00]).unwrap();

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let snapshot = graph.snapshot().expect("a ready graph publishes a snapshot");
        if snapshot.unread_files() == 0 {
            eprintln!("skipping: this build decoded the fixture after all");
            return;
        }
        drop(snapshot);

        // The probe opens it — and that is first knowledge, worth one build.
        let plan = lock_recover(&graph.debt).reserve_probe().expect("the obligation stands");
        let levels = match graph.recovery_probe(&plan) {
            super::super::snapshot::ProbeOutcome::Looked { levels, .. } => levels,
            super::super::snapshot::ProbeOutcome::CouldNotLook => {
                panic!("the probe could not look")
            }
        };
        assert!(
            levels.iter().any(|(_, level)| *level == super::super::debt::Level::Granted),
            "a module that opens was measured as though it could not: {levels:?}",
        );
        let first = lock_recover(&graph.debt).finish_probe(
            Instant::now(),
            super::super::debt::ProbeReceipt {
                token: plan.token,
                basis: plan.basis,
                levels: levels.clone(),
                scope: None,
            },
        );
        assert_eq!(
            first,
            super::super::debt::ProbeResult::NewEvidence,
            "first knowledge of a required capability is news",
        );

        // And never again for the same state, whatever the publication does.
        for _ in 0..3 {
            lock_recover(&graph.debt).probe_now(Instant::now());
            let plan = lock_recover(&graph.debt).reserve_probe().expect("the obligation stands");
            let again = lock_recover(&graph.debt).finish_probe(
                Instant::now(),
                super::super::debt::ProbeReceipt {
                    token: plan.token,
                    basis: plan.basis,
                    levels: levels.clone(),
                    scope: None,
                },
            );
            assert_eq!(
                again,
                super::super::debt::ProbeResult::NoNewEvidence,
                "the same open, measured again, was paid for again",
            );
        }
    }

    /// A subtree that stays unreadable costs one probe per interval and NO builds, and the
    /// interval doubles so the cost falls away. Without the healing condition this is the
    /// rebuild loop the probe would itself create.
    #[cfg(unix)]
    #[test]
    fn a_chronically_unreadable_subtree_is_probed_but_never_rebuilt() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hidden = root.join("CommonModules").join("Скрытый");
        fs::create_dir_all(hidden.join("Ext")).unwrap();
        fs::write(hidden.join("Ext").join("Module.bsl"), "Функция Ф() Экспорт КонецФункции")
            .unwrap();
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&hidden).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let generation =
            lock_recover(&graph.inner).published.as_ref().expect("the build published").generation;
        let first_interval = lock_recover(&graph.debt).probe_interval().expect("a probe is owed");

        for _ in 0..5 {
            lock_recover(&graph.debt).probe_now(Instant::now());
            graph.drive();
        }

        assert_eq!(
            lock_recover(&graph.inner).published.as_ref().unwrap().generation,
            generation,
            "a probe that healed nothing rebuilt the graph anyway",
        );
        let interval = lock_recover(&graph.debt).probe_interval().expect("the probe is still owed");
        assert!(
            interval > first_interval,
            "the probe did not back off: {first_interval:?} then {interval:?}",
        );
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A loader thread that never started is a build nobody will ever call back about. It is
    /// owed exactly like a build that started and failed, and the retry brings the graph up.
    #[test]
    fn a_loader_that_cannot_start_is_owed_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());

        graph.loader_cannot_spawn.store(true, Ordering::SeqCst);
        graph.ensure_loading();
        graph.loader_cannot_spawn.store(false, Ordering::SeqCst);

        assert!(matches!(graph.status(), GraphStatus::Failed(_)), "the spawn failure is a failure");
        assert!(graph.owes_failed(), "a build that never started is owed like one that failed");
        assert_eq!(
            graph.debt_standing(Instant::now()).failed,
            Some(crate::graph::debt::Ripeness::Now),
            "and its retry is owned: the first attempt is immediate, so the turn is the              executor's and no alarm is owed for it",
        );

        graph.drive();
        wait_ready(&graph);
        assert!(!graph.owes_failed(), "the retry that published left the debt behind");
    }

    /// A publication whose build could not read part of the workspace may not take marks
    /// either. It is unsound in the same way a `force_stale` one is — the rendering it would
    /// charge them against never saw the files they were placed for — and the next publication
    /// that reads everything is the one that owes that work.
    #[test]
    fn a_publication_that_could_not_read_everything_consumes_no_marks() {
        const BOUND: i64 = 41;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let hook = Arc::new(|_: GraphPublishSignal| GraphPublishOutcome::HANDLED)
            as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>;
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        {
            let mut inner = lock_recover(&graph.inner);
            inner.status = GraphStatus::Ready { files: 1 };
            inner.published = Some(Published {
                generation: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: None,
                observed_through: Some(9),
            });
        }
        // What the install records for a build that left files unread.
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(9),
            false,
            None,
            left_unread(9, &["/ws/Модуль.bsl"]),
        );

        graph.marks_placed(BOUND, 9);

        assert!(
            graph.marks_pending(),
            "marks were cleared against a graph that never read the files they name"
        );
        assert!(graph.owes_recovery(), "control: the publication is the unsound one");

        // And the publication that read everything takes them.
        let sound = read_everything(&graph, 9);
        lock_recover(&graph.debt).record_publication(Instant::now(), Some(9), false, None, sound);
        graph.marks_placed(BOUND, 9);
        assert!(!graph.marks_pending(), "a sound publication left the marks behind");
    }

    /// A publication that is knowingly stale — a cache served on purpose while the build that
    /// replaces it is already claimed — is not a publication only a probe can heal. Owing it a
    /// probe puts a walk of the whole tree on a schedule nothing can satisfy: the probe heals
    /// on a restored permission or a file that became readable, and a stale cache has neither
    /// to offer. What it does have is a fingerprint that differs from disk, which the ordinary
    /// comparison answers.
    #[test]
    fn a_knowingly_stale_publication_owes_no_probe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(!graph.owes_recovery(), "control: a clean build owes no probe");

        // What `try_publish_stale_and_catch_up` records: served, known to be behind, and
        // nothing unreadable about it.
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(0),
            false,
            None,
            super::super::debt::RecoveryPublicationProof::without_coverage(0),
        );
        assert!(!graph.owes_recovery());

        // And the unsound one still does: that is the case the probe exists for.
        lock_recover(&graph.debt).record_publication(
            Instant::now(),
            Some(0),
            false,
            None,
            left_unread(1, &["/ws/Модуль.bsl"]),
        );
        assert!(graph.owes_recovery(), "an unsound publication still owes its probe");
    }

    /// Nothing under the publication gate asks the database what it managed to read.
    ///
    /// The lenient reader answers "nothing is unread" for a database that will not answer at
    /// all, and that answer used to close the episode. What the install carries now is the
    /// strict proof prepared outside every lock — so the section itself must contain no read
    /// of its own, or the same shortcut returns by another name.
    #[test]
    fn the_publication_gate_reads_no_metadata_of_its_own() {
        let source = crate::inventory::production_source(include_str!("snapshot.rs"));
        let install = source
            .split_once("pub(super) fn install_prepared_snapshot")
            .expect("the install is where a publication lands")
            .1;
        let body = install.split_once("\n    /// ").expect("the install ends").0;
        assert!(
            !body.contains("unread_files()") && !body.contains("unread_paths"),
            "the publication gate reads unread metadata again instead of carrying the proof",
        );
        assert!(
            body.contains("recovery.straddled |= published.force_stale"),
            "the install no longer records that a straddled build cannot vouch for itself",
        );
    }

    /// A slot this generation claimed is never abandoned with a thread owed to it.
    ///
    /// The claim IS the admission point, so whatever happens after it, the single-flight slot
    /// has to end up either running or free. Left `Running` with no thread behind it, every
    /// later decision reads a build in flight and returns at once: no retry, no probe, no hook
    /// flush for the rest of the generation, and the graph reports itself catching up on a
    /// snapshot nothing will ever replace.
    #[test]
    fn a_claimed_slot_is_never_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let stop = crate::state::OwnerStop::default();
        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_lease(lease)
            .with_owner_stop(stop.clone());
        graph.ensure_loading();
        wait_ready(&graph);

        // The slot is taken while the graph is still running, and the stop lands between the
        // claim and the spawn — the window the claim's own tree walk makes wide.
        assert!(matches!(graph.try_claim_reload(true), ReloadClaim::Claimed));
        assert_eq!(
            lock_recover(&graph.inner).published.as_ref().map(|p| p.reload.clone()),
            Some(ReloadState::Running),
            "the claim did not take the slot, so this test proves nothing",
        );
        stop.stop();
        graph.spawn_reload();

        assert_eq!(
            lock_recover(&graph.inner).published.as_ref().map(|p| p.reload.clone()),
            Some(ReloadState::Idle),
            "a declined spawn left the single-flight slot claimed by nobody",
        );
        assert!(!graph.facts().in_flight, "and the graph still reads as building");
    }

    /// The fused cold build is an admission point like the other two.
    ///
    /// The boot reads the stop once, early, and then spends minutes opening the store and
    /// indexing before it asks for this claim. A stop landing in that window would otherwise
    /// admit a whole workspace build after the daemon had asked every owner to leave — and the
    /// lease is released only after that asking, so ownership still reads true.
    #[test]
    fn a_stopped_graph_admits_no_fused_cold_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let stop = crate::state::OwnerStop::default();
        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_lease(lease)
            .with_owner_stop(stop.clone());

        assert!(graph.try_begin_external_build(), "the fused claim is available before the stop");
        lock_recover(&graph.inner).status = GraphStatus::Idle;

        stop.stop();
        assert!(
            !graph.try_begin_external_build(),
            "the fused cold build was admitted after the daemon asked its owners to leave",
        );
        assert_eq!(graph.status(), GraphStatus::Idle, "and it must not have taken the slot");
    }

    /// The boot's own call may not restart a failed graph the schedule is holding off.
    ///
    /// `ensure_loading` is reached from three of the boot's failure paths. Restarting from each
    /// of them spends a retry budget outside the schedule that exists to bound it — including
    /// one already exhausted by an operation error, which is a workspace that cannot build
    /// being rebuilt from boot for ever.
    #[test]
    fn the_boots_own_call_respects_a_spent_retry_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());

        lock_recover(&graph.inner).status = GraphStatus::Failed("operation".to_owned());
        graph.record_failure(FailureKind::Operation);
        assert!(
            matches!(
                graph.debt_standing(Instant::now()).failed,
                Some(crate::graph::debt::Ripeness::Exhausted(_))
            ),
            "an operation error stops the retry budget",
        );

        graph.ensure_loading();
        assert!(
            matches!(graph.status(), GraphStatus::Failed(_)),
            "the boot restarted a build the schedule had stopped",
        );

        // And fresh work revives it, which is what the exhaustion names — proved end to end by
        // the build the revival lets run.
        graph.record_change(11);
        assert!(
            !matches!(graph.status(), GraphStatus::Failed(_)),
            "a delivered change is the work a spent budget waits for, and it did not revive it",
        );
    }

    /// The stop is INSIDE the decision, so a path that records a debt and drives from within
    /// itself starts nothing either.
    ///
    /// This is the shape the watcher's drain has: `record_change` and `record_forced` each call
    /// `drive`, so a stop checked around the drain is a stop half the window never sees.
    #[test]
    fn a_stopped_graph_starts_nothing_however_it_is_driven() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let stop = crate::state::OwnerStop::default();
        let graph = GraphState::for_workspace(root.to_path_buf()).with_owner_stop(stop.clone());

        stop.stop();

        // The recording path, which drives from inside itself.
        graph.record_change(7);
        assert_eq!(graph.status(), GraphStatus::Idle, "a recording path started a build");
        graph.record_forced(8);
        assert_eq!(graph.status(), GraphStatus::Idle, "a forced recording started a build");
        // And the request path, and the alarm.
        graph.ensure_first_build();
        assert_eq!(graph.status(), GraphStatus::Idle, "a request started a build");
        graph.drive();
        assert_eq!(graph.status(), GraphStatus::Idle, "the alarm started a build");
        assert_eq!(graph.wake_at(Instant::now()), None, "a leaving graph set an alarm");
        // Nothing was dropped: the debts are still there for whoever comes next.
        assert_eq!(graph.owes_change(), Some(7), "the stop dropped the delivered change");
        assert_eq!(graph.owes_forced(), Some(8), "the stop dropped the forced reload");
    }

    /// A stop that lands BETWEEN the decision and the claim is answered at the claim.
    ///
    /// That window is not narrow: the claim walks the tree for its fingerprint first, and on a
    /// large workspace the walk takes seconds. So the admission point is the claim itself —
    /// read under the very lock that grants the slot — and not the decision that led to it.
    #[test]
    fn a_stop_during_the_claims_own_walk_is_refused_at_the_claim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let stop = crate::state::OwnerStop::default();
        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_lease(lease)
            .with_owner_stop(stop.clone());
        graph.ensure_loading();
        wait_ready(&graph);

        // The decision was taken while the graph was running; the stop arrives after it and
        // before the claim, which is what the walk inside the claim makes possible.
        stop.stop();

        assert!(
            matches!(graph.try_claim_reload(true), ReloadClaim::Stopping),
            "the claim admitted a build after the daemon asked its owners to leave",
        );
        assert_eq!(
            lock_recover(&graph.inner).published.as_ref().map(|p| p.reload.clone()),
            Some(ReloadState::Idle),
            "and it must not have taken the slot on the way to refusing",
        );
    }

    /// The marks' budget pays for an attempt that goes and reads disk. A decision that ends
    /// without a claim — ownership not confirmed at that moment, or another drive already
    /// building — made no attempt, and charging it would spend the schedule that is the only
    /// thing left to answer the marks.
    #[test]
    fn a_decision_that_claims_no_build_spends_no_mark_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        let graph = GraphState::for_workspace(root.to_path_buf()).with_lease(lease.clone());
        graph.ensure_loading();
        wait_ready(&graph);

        // Marks owed and due, so the decision below is a forced build.
        let placed = Instant::now() - Duration::from_secs(60);
        lock_recover(&graph.debt).place_marks(placed, 7, 1);
        lock_recover(&graph.debt).settle_marks(placed + Duration::from_secs(30), false);
        assert!(graph.owes_marks(), "the marks are owed");
        let due_before = lock_recover(&graph.debt).marks_due(Instant::now());

        // Ownership stops being confirmable between the decision and the claim: the build is
        // held, and nothing reads disk.
        graph.claim_is_held.store(true, Ordering::SeqCst);
        graph.drive();

        assert!(graph.owes_marks(), "the held decision dropped the marks");
        assert_eq!(
            lock_recover(&graph.debt).marks_due(Instant::now()),
            due_before,
            "a decision that built nothing still charged the marks' budget"
        );
    }

    /// A hold says "decide again in a moment", and that moment passes. Nothing clears it —
    /// ownership is confirmed by the next decision, not announced by an event — so a hold
    /// left in the past would stay the earliest wake-up for ever: the watcher would return
    /// from every wait at once, re-read the lease file and drain the hub on every turn, and
    /// one unconfirmed check would cost a core for the life of the daemon.
    #[test]
    fn a_hold_that_has_passed_is_not_a_wake_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());

        let now = Instant::now();
        lock_recover(&graph.debt).hold(now);
        assert!(
            graph.wake_at(now).is_some_and(|due| due > now),
            "control: while the hold stands it IS the wake-up"
        );

        let after = now + Duration::from_secs(3600);
        assert!(
            graph.wake_at(after).is_none_or(|due| due > after),
            "the watcher is left with a wake-up in the past: that is a spin, not a schedule"
        );
    }

    /// The retry a failure owed can find nothing to build: the edit that provoked it was
    /// reverted, or the build that failed was owed to marks and the tree never moved. That
    /// comparison answers the debt. Left open behind an elapsed schedule it becomes a
    /// standing order — the decision picks it at every wake, walks the whole tree, publishes
    /// nothing, and the graph reads stale for ever while being perfectly current.
    #[test]
    fn a_failure_whose_retry_finds_nothing_to_build_is_answered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert!(!graph.owes_failed(), "control: a clean build owes nothing");

        // A reload failed transiently, and its retry is due now. Nothing on disk moved, so
        // the comparison the retry makes will answer "already published".
        // Long enough ago for the retry to be due, not so long that its window is spent —
        // an exhausted window is a different state, with an owner of its own.
        lock_recover(&graph.debt).record_failure(
            Instant::now() - Duration::from_secs(60),
            FailureKind::Transient,
            crate::graph::debt::Sponsors { primary: true, marks: false },
        );
        assert!(graph.owes_failed(), "the failure is owed");

        graph.drive();

        assert!(!graph.owes_failed(), "the comparison left the failure open");
        assert!(
            !lock_recover(&graph.debt).stale(),
            "a graph that matches disk still reads behind by its own account"
        );
        let now = Instant::now();
        assert!(
            graph.wake_at(now).is_none_or(|due| due > now),
            "the watcher is left with a wake-up in the past, which is a spin, not a schedule"
        );
    }

    /// A lease that cannot be confirmed right now is not a takeover: the marks stay, the
    /// decision is held, and it is taken again on the schedule. Dropping them here would lose
    /// a re-render nobody would ever ask for again.
    #[test]
    fn marks_survive_a_lease_that_cannot_be_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        let graph = GraphState::for_workspace(root.to_path_buf()).with_lease(lease.clone());

        let held = lease.hold_file_lock_for_test();
        std::fs::remove_file(crate::cache::WorkspaceCacheLayout::for_workspace(root).lease_path())
            .unwrap();
        graph.marks_placed(7, 3);
        drop(held);

        assert!(!lease.is_superseded(), "the fixture must not look like a takeover");
        assert!(graph.marks_pending(), "the marks were dropped while the lease was busy");
        assert!(graph.owes_marks() || graph.drift_pending(), "and nothing is owed to them");
    }

    /// A graph nobody watches cannot know it fell behind, so it never calls itself fresh: not
    /// before its watcher's first look, not after the watcher left. A quiet start on a healthy
    /// hub completes that first look with no event at all.
    #[test]
    fn freshness_rests_on_the_drift_watch() {
        use crate::tools::location::DriftWatch;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let unwatched = GraphState::for_workspace(root.to_path_buf());
        unwatched.ensure_loading();
        wait_ready(&unwatched);
        let snapshot = unwatched.snapshot().unwrap();
        assert_eq!(unwatched.drift_watch(), DriftWatch::Unobserved);
        assert!(unwatched.cached_freshness(&snapshot).stale, "an unwatched graph called fresh");

        let (graph, hub, stop) = super::super::test_support::watched_graph(root);
        graph.ensure_loading();
        wait_ready(&graph);
        wait_until(&graph, "the boot nudge to settle", || !graph.drift_pending());
        let snapshot = graph.snapshot().unwrap();
        let freshness = graph.cached_freshness(&snapshot);
        assert_eq!(freshness.drift_watch, DriftWatch::Watching);
        assert!(!freshness.stale, "a watched, current graph reported stale");
        assert_eq!(published_report(&graph).drift_watch, Some("watching"));

        graph.set_watch(super::super::watcher::WatchPhase::Starting, None);
        assert!(graph.cached_freshness(&snapshot).stale, "a graph still starting called fresh");
        stop.stop();
        hub.interrupt_waiters();
        wait_until(&graph, "the watcher to leave", || {
            graph.drift_watch() == DriftWatch::Unobserved
        });
        assert!(graph.cached_freshness(&snapshot).stale, "an abandoned graph called fresh");
    }

    /// Marks owed a build make the graph behind by construction, even with nothing else amiss.
    #[test]
    fn owed_marks_make_the_graph_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let (graph, hub, stop) = super::super::test_support::watched_graph(root);
        graph.ensure_loading();
        wait_ready(&graph);
        wait_until(&graph, "the boot nudge to settle", || !graph.drift_pending());
        let snapshot = graph.snapshot().unwrap();
        assert!(!graph.cached_freshness(&snapshot).stale);
        graph.marks_placed(1, u64::MAX);
        assert!(graph.cached_freshness(&snapshot).stale, "owed marks reported fresh");
        stop.stop();
        hub.interrupt_waiters();
    }

    /// A forced build owed to marks nobody consumes is paid for inside a retry budget, like
    /// every other retry owner's work: with a hook that keeps refusing, the forced rebuilds
    /// stop once the budget is spent, and only fresh marks start them again.
    #[test]
    fn owed_marks_stop_rebuilding_when_their_budget_is_spent() {
        let dir = tempfile::tempdir().unwrap();
        let (graph, _bounds) = recording_graph(dir.path(), |_| false);
        let graph = graph.with_lease(crate::workspace_lease::WorkspaceLease::claim(dir.path()));
        publish(&graph, Some(0), false, false);
        graph.marks_placed(5, 8);
        // Taken over: a forced reload is then counted without a build thread racing the test.
        let _newer = crate::workspace_lease::WorkspaceLease::claim(dir.path());

        let facts = Facts { ready: true, owns: true, ..Facts::default() };
        let start = Instant::now();
        let mut now = start;
        let mut rebuilds = 0;
        loop {
            let mut debt = lock_recover(&graph.debt);
            let Some(due) = debt.marks_due(now) else { break };
            now = now.max(due);
            if debt.decide(now, facts).start.is_some_and(|start| start.forced) {
                rebuilds += 1;
                debt.spend_mark_attempt(now);
            }
            assert!(now < start + Duration::from_secs(3 * 3600), "the owed build never stopped");
            now += Duration::from_secs(1);
        }
        assert!(rebuilds >= 1, "no forced build was ever paid for");
        assert!(graph.owes_marks(), "a spent budget dropped the debt");

        let mut debt = lock_recover(&graph.debt);
        debt.place_marks(now, 9, 12);
        assert!(debt.marks_due(now).is_some(), "fresh marks did not revive the obligation");
    }

    /// Fresh marks that revive a spent obligation wake the watcher, so the build they are
    /// owed is due within its grace, not after the watcher's current sleep.
    #[test]
    fn marks_that_revive_a_spent_obligation_wake_the_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let (graph, _bounds) = recording_graph(dir.path(), |_| false);
        publish(&graph, Some(0), false, false);
        graph.marks_placed(5, 8);
        lock_recover(&graph.debt).stop_marks_budget();
        assert!(
            lock_recover(&graph.debt).marks_due(Instant::now()).is_none(),
            "the budget is not spent",
        );
        let before = graph.alarms.load(Ordering::SeqCst);
        graph.marks_placed(9, 12);
        assert!(
            lock_recover(&graph.debt).marks_due(Instant::now()).is_some(),
            "fresh marks did not revive it"
        );
        assert!(graph.alarms.load(Ordering::SeqCst) > before, "the watcher sleeps on");
    }

    fn recording_graph(
        root: &Path,
        handled: impl Fn(usize) -> bool + Send + Sync + 'static,
    ) -> (GraphState, Arc<Mutex<Vec<i64>>>) {
        let bounds = Arc::new(Mutex::new(Vec::new()));
        let hook = {
            let bounds = Arc::clone(&bounds);
            Arc::new(move |signal: GraphPublishSignal| {
                let mut seen = lock_recover(&bounds);
                seen.push(signal.mark_bound);
                let handled = handled(seen.len());
                GraphPublishOutcome { topology_handled: handled, roots_handled: true }
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        (GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook), bounds)
    }

    fn publish(graph: &GraphState, observed_through: Option<u64>, stale: bool, force_stale: bool) {
        {
            let mut inner = lock_recover(&graph.inner);
            let generation = inner.published.as_ref().map_or(1, |p| p.generation + 1);
            inner.status = GraphStatus::Ready { files: 0 };
            inner.published = Some(Published {
                generation,
                fingerprint: crate::graph_db::GraphFp::default(),
                stale,
                reload: ReloadState::Idle,
                force_stale,
                search_roots: None,
                observed_through,
            });
        }
        graph.notify_published(false);
    }

    fn consumed(bounds: &Arc<Mutex<Vec<i64>>>, bound: i64) -> bool {
        lock_recover(bounds).contains(&bound)
    }

    /// The marks' fact reached the hub before the build scanned disk, and the marks
    /// themselves were placed while that build ran. Its publication observed the fact, so
    /// it consumes them — a bound taken from the mark counter at build start would have left
    /// them hanging, since they were stamped after it.
    #[test]
    fn marks_placed_during_a_build_are_consumed_by_its_publication() {
        let dir = tempfile::tempdir().unwrap();
        let (graph, bounds) = recording_graph(dir.path(), |_| true);
        lock_recover(&graph.inner).status = GraphStatus::Loading;

        graph.marks_placed(5, 8);
        assert!(lock_recover(&bounds).is_empty(), "nothing is published to consume against");
        assert!(!graph.owes_marks(), "a build in flight owes nothing yet");

        publish(&graph, Some(10), false, false);
        assert!(consumed(&bounds, 5));
        assert!(!graph.marks_pending());
        assert!(!graph.owes_marks());
    }

    /// The publication came first and had already observed the fact when the marks arrived:
    /// they are consumed against it at once, with no later build needed.
    #[test]
    fn marks_placed_after_an_observing_publication_are_consumed_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (graph, bounds) = recording_graph(dir.path(), |_| true);
        publish(&graph, Some(10), false, false);

        graph.marks_placed(5, 8);
        assert!(consumed(&bounds, 5));
        assert!(!graph.marks_pending());
        assert!(!graph.owes_marks());
    }

    /// A publication that did not observe the fact consumes nothing, and nothing else is on
    /// its way to: a build is owed. The next publication that observes the fact consumes the
    /// marks and discharges it.
    #[test]
    fn marks_no_publication_observed_are_owed_a_build() {
        let dir = tempfile::tempdir().unwrap();
        let (graph, bounds) = recording_graph(dir.path(), |_| true);
        publish(&graph, Some(3), false, false);

        graph.marks_placed(5, 8);
        assert!(!consumed(&bounds, 5), "consumed against a graph that never saw the fact");
        assert!(graph.marks_pending());
        assert!(graph.owes_marks(), "no build is owed to the marks");

        publish(&graph, Some(9), false, false);
        assert!(consumed(&bounds, 5));
        assert!(!graph.marks_pending());
        assert!(!graph.owes_marks(), "the obligation outlived the marks");
    }

    /// A publication whose build straddled a write, and the boot's stale cache, consume
    /// nothing, whatever they observed.
    #[test]
    fn straddled_and_stale_publications_consume_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (graph, bounds) = recording_graph(dir.path(), |_| true);
        publish(&graph, Some(10), false, true);
        graph.marks_placed(5, 8);
        assert!(!consumed(&bounds, 5), "consumed against a straddled build");

        publish(&graph, None, true, false);
        graph.consume_leftover_marks(4);
        assert!(!consumed(&bounds, 4) && !consumed(&bounds, 5), "consumed against a stale cache");
        assert!(graph.marks_pending());

        publish(&graph, Some(8), false, false);
        assert!(consumed(&bounds, 5), "the catch-up publication observed every placed fact");
        assert!(!graph.marks_pending());
    }

    /// Every order of a nudge (a build starts), two publications and the consumer's two mark
    /// placements (in the order the consumer places them): marks are offered only to a
    /// publication that observed their fact, and a mark is consumed by the end exactly when
    /// such a publication was current when it was placed or came after it.
    #[test]
    fn marks_are_consumed_by_an_observing_publication_in_every_order() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Step {
            Mark(i64, u64),
            Publish(u64),
            Nudge,
        }
        const MARKS: [(i64, u64); 2] = [(5, 4), (9, 12)];
        let steps = [
            Step::Mark(MARKS[0].0, MARKS[0].1),
            Step::Mark(MARKS[1].0, MARKS[1].1),
            Step::Publish(6),
            Step::Publish(14),
            Step::Nudge,
        ];
        fn orders(rest: Vec<Step>, prefix: Vec<Step>, out: &mut Vec<Vec<Step>>) {
            if rest.is_empty() {
                out.push(prefix);
                return;
            }
            for i in 0..rest.len() {
                let (mut rest, mut prefix) = (rest.clone(), prefix.clone());
                prefix.push(rest.remove(i));
                orders(rest, prefix, out);
            }
        }
        let mut all = Vec::new();
        orders(steps.to_vec(), Vec::new(), &mut all);
        let consumer_order = |order: &Vec<Step>| {
            let at = |mark: (i64, u64)| order.iter().position(|s| *s == Step::Mark(mark.0, mark.1));
            at(MARKS[0]) < at(MARKS[1])
        };
        let all: Vec<_> = all.into_iter().filter(consumer_order).collect();
        assert_eq!(all.len(), 60);

        for order in all {
            let dir = tempfile::tempdir().unwrap();
            let current = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let offers = Arc::new(Mutex::new(Vec::<(i64, u64)>::new()));
            let hook = {
                let (current, offers) = (Arc::clone(&current), Arc::clone(&offers));
                Arc::new(move |signal: GraphPublishSignal| {
                    lock_recover(&offers).push((signal.mark_bound, current.load(Ordering::SeqCst)));
                    GraphPublishOutcome { topology_handled: true, roots_handled: true }
                })
                    as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
            };
            let graph = GraphState::for_workspace(dir.path().to_path_buf()).with_publish_hook(hook);
            // What the ledger must do, step by step: `(consuming observation, consumed)`.
            let mut consuming: Option<u64> = None;
            let mut expected = [false; 2];
            let mut placed = [false; 2];
            for step in &order {
                match *step {
                    Step::Mark(mark, fact) => {
                        let i = MARKS.iter().position(|m| *m == (mark, fact)).unwrap();
                        placed[i] = true;
                        expected[i] |= consuming.is_some_and(|observed| observed >= fact);
                        graph.marks_placed(mark, fact);
                    }
                    Step::Publish(observed) => {
                        current.store(observed, Ordering::SeqCst);
                        consuming = Some(observed);
                        for (i, (_, fact)) in MARKS.iter().enumerate() {
                            expected[i] |= placed[i] && observed >= *fact;
                        }
                        publish(&graph, Some(observed), false, false);
                    }
                    Step::Nudge => {
                        consuming = None;
                        lock_recover(&graph.inner).status = GraphStatus::Loading;
                    }
                }
            }
            for &(bound, observed) in lock_recover(&offers).iter() {
                let allowed =
                    MARKS.iter().filter(|(_, fact)| *fact <= observed).map(|(m, _)| *m).max();
                assert!(
                    allowed.is_some_and(|allowed| bound <= allowed),
                    "{order:?}: marks up to {bound} offered to a publication through {observed}"
                );
            }
            let left = lock_recover(&graph.debt).placed_marks();
            for (i, mark) in MARKS.iter().enumerate() {
                assert_eq!(
                    !left.contains(mark),
                    expected[i],
                    "{order:?}: mark {mark:?}, left {left:?}"
                );
            }
        }
    }

    /// A hook that could not run the refresh keeps every mark it was offered; the next
    /// publication offers them again.
    #[test]
    fn a_refused_consume_keeps_the_marks_for_the_next_publication() {
        let dir = tempfile::tempdir().unwrap();
        let (graph, bounds) = recording_graph(dir.path(), |call| call > 2);
        publish(&graph, Some(10), false, false);
        graph.marks_placed(5, 8);
        assert!(consumed(&bounds, 5), "the marks were offered");
        assert!(graph.marks_pending(), "a refused consume dropped the marks");

        publish(&graph, Some(10), false, false);
        assert_eq!(lock_recover(&bounds).iter().filter(|bound| **bound == 5).count(), 2);
        assert!(!graph.marks_pending(), "the next publication did not consume them");
    }

    #[test]
    fn an_unchanged_disk_comparison_consumes_marks_without_republishing() {
        use super::super::test_support::{wait_publish_pass_within, WAIT_CEILING};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let hub = super::super::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let offered = Arc::new(Mutex::new(Vec::new()));
        let mark_attempts = Arc::new(AtomicUsize::new(0));
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_change_hub(hub.clone())
            .with_publish_hook({
                let offered = Arc::clone(&offered);
                let mark_attempts = Arc::clone(&mark_attempts);
                Arc::new(move |signal: GraphPublishSignal| {
                    lock_recover(&offered).push(signal.mark_bound);
                    let handled =
                        signal.mark_bound == 0 || mark_attempts.fetch_add(1, Ordering::SeqCst) > 0;
                    GraphPublishOutcome { topology_handled: handled, roots_handled: true }
                })
            });
        graph.set_watch(super::super::watcher::WatchPhase::Running, None);
        graph.ensure_loading();
        wait_ready(&graph);
        wait_publish_pass_within(&graph, WAIT_CEILING, 1);
        lock_recover(&offered).clear();

        let revision = graph.status_report().revision.expect("published graph revision");
        let full_builds = graph.full_builds_started.load(Ordering::SeqCst);
        let publication = graph.consuming_observation().expect("clean publication observation");
        let metadata_path = root.join("Configuration.xml");
        let same_bytes = fs::read(&metadata_path).unwrap();

        // Order one: the complete comparison proves the same graph covers this new fact,
        // before any context mark exists. That proof must survive for a later consumer.
        let before = hub.seq().max(publication);
        fs::write(&metadata_path, &same_bytes).unwrap();
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(5), || {
                hub.seq() > before
            }),
            "the same-byte XML write did not reach the workspace hub"
        );
        let first_fact = hub.seq();
        assert!(first_fact > publication, "the comparison must postdate the installed publication");
        graph.record_change_quietly(first_fact);
        graph.check_against_disk();

        // The first context refresh refuses. The persisted comparison still proves this
        // mark is covered, so retrying the hook must not schedule a graph build.
        graph.marks_placed(7, first_fact);
        assert!(graph.marks_pending(), "a refused hook must retain its mark");
        graph.flush_hook_obligations();
        assert!(!graph.marks_pending(), "the successful hook retry did not consume the mark");
        assert_eq!(mark_attempts.load(Ordering::SeqCst), 2, "one refusal and one retry ran");

        // Order two: a later fact is marked before its same-byte comparison. The prior
        // comparison frontier must not claim it; the new exact scan may then advance it.
        let before = hub.seq();
        fs::write(&metadata_path, &same_bytes).unwrap();
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(5), || {
                hub.seq() > before
            }),
            "the second same-byte XML write did not reach the workspace hub"
        );
        let second_fact = hub.seq();
        assert!(second_fact > first_fact, "the second mark needs a newer fact");
        graph.marks_placed(8, second_fact);
        assert!(graph.marks_pending(), "the prior comparison cannot cover a newer fact");
        graph.record_change_quietly(second_fact);
        graph.check_against_disk();

        let offered = lock_recover(&offered).clone();
        assert!(
            offered.contains(&7) && offered.contains(&8),
            "the comparisons did not offer both proven mark bounds"
        );
        assert!(!graph.marks_pending(), "the successful hook did not consume the mark");
        assert!(!graph.owes_marks(), "the comparison left a covered marks schedule armed");
        assert!(!graph.drift_pending(), "the comparison left a graph reload in flight");
        std::thread::sleep(super::super::debt::OWED_MARKS_GRACE + Duration::from_millis(100));
        graph.drive();
        assert!(!graph.owes_marks(), "the grace period re-armed a covered marks schedule");
        assert!(!graph.drift_pending(), "the grace period started a covered graph reload");
        assert_eq!(graph.status_report().revision, Some(revision));
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), full_builds);
        hub.shutdown();
    }

    #[test]
    fn deferred_context_mark_keeps_its_reported_fact() {
        use super::super::test_support::{wait_publish_pass_within, WAIT_CEILING};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let hub = super::super::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_change_hub(hub.clone())
            .with_publish_hook(Arc::new(|_| GraphPublishOutcome::HANDLED));
        graph.set_watch(super::super::watcher::WatchPhase::Running, None);
        graph.ensure_loading();
        wait_ready(&graph);
        wait_publish_pass_within(&graph, WAIT_CEILING, 1);

        let revision = graph.status_report().revision.expect("published graph revision");
        let full_builds = graph.full_builds_started.load(Ordering::SeqCst);
        let observed = graph.consuming_observation().expect("clean publication observation");
        let path = root.join("Configuration.xml");
        let same_bytes = fs::read(&path).unwrap();
        let before = hub.seq().max(observed);
        fs::write(&path, &same_bytes).unwrap();
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(5), || {
                hub.seq() > before
            }),
            "the first same-byte touch did not reach the hub"
        );
        let reported_fact = hub.seq();
        let owed = graph.owed_context_marks();
        owed.record(7);
        graph.marks_placed(7, reported_fact);
        graph.record_change_quietly(reported_fact);
        graph.check_against_disk();
        assert!(!graph.marks_pending(), "the no-op comparison did not consume the direct mark");

        // The provider's report remains deferred while another hub event arrives. Registration
        // must retain the fact at which it reported the mark, not rebind it to this later seq.
        fs::write(&path, &same_bytes).unwrap();
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(5), || {
                hub.seq() > reported_fact
            }),
            "the later same-byte touch did not advance the hub"
        );
        graph.register_owed_context_marks();
        assert!(
            !graph.marks_pending(),
            "deferred registration rebound the already-consumed high mark to a newer hub fact"
        );

        let newer_fact = hub.seq();
        owed.record(8);
        graph.register_owed_context_marks();
        assert!(graph.marks_pending(), "a new provider mark must keep its captured fact");
        assert!(
            lock_recover(&graph.debt).marks.placements() > 0,
            "the prior no-op comparison cannot claim a later provider mark"
        );

        fs::write(&path, &same_bytes).unwrap();
        assert!(
            crate::change_hub::test_support::eventually(Duration::from_secs(5), || {
                hub.seq() > newer_fact
            }),
            "the comparison touch for the newer provider mark did not reach the hub"
        );
        let comparison_fact = hub.seq();
        graph.record_change_quietly(comparison_fact);
        graph.check_against_disk();
        assert!(!graph.marks_pending(), "a complete later comparison should consume the mark");

        std::thread::sleep(super::super::debt::OWED_MARKS_GRACE + Duration::from_millis(100));
        graph.drive();
        assert_eq!(graph.status_report().revision, Some(revision));
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), full_builds);
        hub.shutdown();
    }

    #[test]
    fn refused_covered_mark_refresh_retries_without_a_graph_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook = {
            let attempts = Arc::clone(&attempts);
            Arc::new(move |signal: GraphPublishSignal| {
                let handled = if signal.mark_bound > 0 {
                    attempts.fetch_add(1, Ordering::SeqCst) > 1
                } else {
                    true
                };
                GraphPublishOutcome { topology_handled: handled, roots_handled: true }
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(dir.path().to_path_buf()).with_publish_hook(hook);
        publish(&graph, Some(10), false, false);
        let published_passes = graph.publish_passes.load(Ordering::SeqCst);
        graph.marks_placed(5, 8);
        assert!(graph.marks_pending(), "a refused hook left covered marks pending");

        graph.flush_hook_obligations();
        assert!(graph.marks_pending(), "two refusals must keep the covered mark pending");
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 0);

        graph.flush_hook_obligations();

        assert_eq!(attempts.load(Ordering::SeqCst), 3, "two refusals and one retry ran");
        assert!(!graph.marks_pending(), "the successful hook consumed its covered mark");
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 0);
        assert_eq!(graph.publish_passes.load(Ordering::SeqCst), published_passes);
    }

    #[test]
    fn a_later_covered_mark_success_clears_prior_hook_debt_without_an_extra_offer() {
        let dir = tempfile::tempdir().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let hook = {
            let attempts = Arc::clone(&attempts);
            Arc::new(move |signal: GraphPublishSignal| {
                let handled = if signal.mark_bound > 0 {
                    attempts.fetch_add(1, Ordering::SeqCst) > 0
                } else {
                    true
                };
                GraphPublishOutcome { topology_handled: handled, roots_handled: true }
            }) as Arc<dyn Fn(GraphPublishSignal) -> GraphPublishOutcome + Send + Sync>
        };
        let graph = GraphState::for_workspace(dir.path().to_path_buf()).with_publish_hook(hook);
        publish(&graph, Some(10), false, false);
        let publish_passes = graph.publish_passes.load(Ordering::SeqCst);

        graph.marks_placed(5, 8);
        assert!(graph.marks_pending(), "first hook refusal keeps the mark placed");
        assert!(graph.hook_debt().marks, "first hook refusal records mark refresh debt");
        assert!(!graph.owes_marks(), "fact 8 is already covered by generation 10");

        graph.marks_placed(6, 9);
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "the second placement retries the hook");
        assert!(!graph.marks_pending(), "the successful refresh consumed both covered placements");
        assert!(!graph.hook_debt().marks, "successful consume clears the previous hook debt");
        graph.flush_hook_obligations();
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "cleared hook debt offers no duplicate retry"
        );
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 0);
        assert_eq!(graph.publish_passes.load(Ordering::SeqCst), publish_passes);
    }

    #[test]
    fn a_covered_mark_retry_does_not_hide_a_new_unobserved_mark() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::for_workspace(dir.path().to_path_buf()).with_publish_hook(
            Arc::new(|signal: GraphPublishSignal| GraphPublishOutcome {
                topology_handled: signal.mark_bound == 0,
                roots_handled: true,
            }),
        );
        publish(&graph, Some(10), false, false);

        graph.marks_placed(5, 8);
        assert!(!lock_recover(&graph.debt).owes_marks(), "fact 8 is already covered by 10");

        graph.marks_placed(6, 11);
        assert!(graph.marks_pending(), "both refused covered work and fact 11 remain placed");
        assert!(
            lock_recover(&graph.debt).owes_marks(),
            "fact 11 is beyond the publication and still requires a graph build"
        );
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 0);

        let now = Instant::now() + super::super::debt::OWED_MARKS_GRACE + Duration::from_secs(1);
        let decision = lock_recover(&graph.debt)
            .decide(now, Facts { ready: true, owns: true, ..Facts::default() });
        assert!(decision.start.is_some_and(|start| start.forced));
    }

    /// A lazy first build that serves a stale cache hands its catch-up the admission it was
    /// granted — not a second one.
    ///
    /// The builder has already TAKEN its ticket out of the slot when it gets to the hand-over,
    /// so the slot is empty there. Treated as "nothing was admitted", the hand-over minted a
    /// ticket of its own: a new claim, the primary lane as its sponsor, and cutoffs read from
    /// the debts as they stand at the hand-over — including a demand delivered after the build
    /// was admitted, which that catch-up then answered for free.
    #[test]
    fn a_lazy_catch_up_carries_the_ticket_its_admission_paid_for() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let mut stale = super::super::scan::workspace_fingerprint(root);
        stale.files = stale.files.wrapping_add(1);
        super::super::test_support::seed_cache(root, stale);

        let seen: Arc<Mutex<Vec<Option<super::super::debt::BuildTicket>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_post_claim_hook({
            let seen = Arc::clone(&seen);
            Arc::new(move |graph: &GraphState| {
                let mut seen = lock_recover(&seen);
                seen.push(graph.claimed_ticket());
                if seen.len() == 1 {
                    // A demand delivered AFTER the first build was admitted.
                    lock_recover(&graph.debt).record_forced(Instant::now(), 42);
                }
            })
        });
        graph.ensure_loading();
        wait_until(&graph, "the catch-up to reach its own mandate", || {
            lock_recover(&seen).len() >= 2
        });

        let (admitted, handed) = {
            let seen = lock_recover(&seen);
            (
                seen[0].expect("the first build was admitted with a ticket"),
                seen[1].expect("the catch-up was handed a ticket"),
            )
        };
        assert!(
            !admitted.forced && admitted.kind == BuildKind::Initial,
            "the fixture needs an ordinary first build: a forced one never serves the stale cache",
        );
        assert_eq!(handed.kind, BuildKind::Reload, "the hand-over is a catch-up");
        assert_eq!(
            handed.claim, admitted.claim,
            "the catch-up was handed a second, free admission instead of the one that was paid for",
        );
        assert_eq!(
            handed.sponsors, admitted.sponsors,
            "the catch-up named sponsors that never paid"
        );
        assert_eq!(
            (handed.forced, handed.forced_through),
            (admitted.forced, admitted.forced_through),
            "the catch-up adopted a demand delivered after its admission",
        );
        assert_eq!(
            (handed.scan_cutoff, handed.recovery_cutoff),
            (admitted.scan_cutoff, admitted.recovery_cutoff),
            "the catch-up widened the cutoffs its admission fixed",
        );
        wait_until(&graph, "the catch-up to publish", || {
            lock_recover(&graph.inner).published.as_ref().is_some_and(|published| !published.stale)
        });
    }

    /// The build that hands a stale publication's catch-up over does not end the mandate its
    /// successor is carrying when it returns.
    ///
    /// The successor is spawned inside the hand-over and takes the ticket at once; the
    /// handing-over build returns afterwards and lets go of what IT carried. Letting go by
    /// clearing the slot outright cleared the successor's mandate instead: a build still under
    /// way that no longer reads as one, whose outcome would find nothing to report against.
    #[test]
    fn a_handover_does_not_end_the_mandate_its_successor_carries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        super::super::test_support::seed_cache(
            root,
            super::super::scan::workspace_fingerprint(root),
        );
        // A global-context change is deliberately outside the incremental projection. The
        // successor must therefore reach the full-build publication window this test parks.
        let descriptor = root.join("CommonModules/Сервер.xml");
        let xml = std::fs::read_to_string(&descriptor).unwrap();
        assert!(xml.contains("<Global>false</Global>"), "fixture starts non-global");
        super::super::test_support::write(
            root,
            "CommonModules/Сервер.xml",
            &xml.replace("<Global>false</Global>", "<Global>true</Global>"),
        );

        let successor_carried = Arc::new(AtomicBool::new(false));
        let handed_over = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_handover_hook({
                let successor_carried = Arc::clone(&successor_carried);
                Arc::new(move |graph: &GraphState| {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while Instant::now() < deadline {
                        let carrying = lock_recover(&graph.inner)
                            .building
                            .is_some_and(|ticket| ticket.kind == BuildKind::Reload);
                        if carrying {
                            successor_carried.store(true, Ordering::SeqCst);
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                })
            })
            .with_publish_window_hook({
                let (handed_over, parked, release) =
                    (Arc::clone(&handed_over), Arc::clone(&parked), Arc::clone(&release));
                Arc::new(move || {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !handed_over.load(Ordering::SeqCst) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    parked.store(true, Ordering::SeqCst);
                    while !release.load(Ordering::SeqCst) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                })
            });

        assert!(graph.try_begin_external_build(), "the admission this build runs on");
        graph.run_load(false);
        handed_over.store(true, Ordering::SeqCst);
        assert!(
            successor_carried.load(Ordering::SeqCst),
            "the fixture needs the successor carrying its mandate before the hand-over returned",
        );
        wait_until(&graph, "the successor to park while carrying its mandate", || {
            parked.load(Ordering::SeqCst)
        });
        let in_flight = graph.build_in_flight();
        release.store(true, Ordering::SeqCst);
        assert!(
            in_flight,
            "the build that handed the catch-up over ended the mandate its successor still carries",
        );
        wait_until(&graph, "the catch-up to publish", || {
            lock_recover(&graph.inner).published.as_ref().is_some_and(|published| !published.stale)
        });
    }

    /// A failed build's outcome answers for the ticket that build carried, not for a claim that
    /// landed after it.
    ///
    /// The lifecycle reads the build as over the moment its failure is written, and from there
    /// another claim is legal. Naming the sponsors from the slot later, the outcome found that
    /// claim first — and closed the lanes that paid for a build which had not even started,
    /// while the lane that paid for the failed one heard nothing.
    #[test]
    fn a_failed_builds_outcome_answers_for_its_own_ticket_not_the_claim_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let later: Arc<Mutex<Option<super::super::debt::BuildTicket>>> = Arc::new(Mutex::new(None));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_outcome_window_hook({
            let later = Arc::clone(&later);
            Arc::new(move |graph: &GraphState| {
                if lock_recover(&later).is_some() {
                    return;
                }
                lock_recover(&graph.debt).record_forced(Instant::now(), 42);
                if matches!(graph.try_claim_reload(true), ReloadClaim::Claimed) {
                    *lock_recover(&later) = graph.claimed_ticket();
                }
            })
        });
        graph.ensure_loading();
        wait_ready(&graph);

        {
            let mut debt = lock_recover(&graph.debt);
            debt.place_marks(Instant::now(), 11, 1);
            debt.settle_marks(Instant::now(), false);
        }
        assert!(
            matches!(graph.try_claim_reload(true), ReloadClaim::Claimed),
            "the marks buy the slot"
        );
        assert_eq!(
            graph.claimed_ticket().map(|ticket| ticket.sponsors),
            Some(super::super::debt::Sponsors { primary: false, marks: true }),
            "the fixture needs a build the marks alone paid for",
        );
        graph.refused_installs.store(1, Ordering::SeqCst);
        graph.spawn_reload();
        wait_until(&graph, "the refused build to report and let go of its mandate", || {
            lock_recover(&later).is_some() && lock_recover(&graph.inner).building.is_none()
        });

        let later =
            lock_recover(&later).expect("the fixture needs a claim in the outcome's window");
        assert!(
            later.sponsors.primary,
            "the fixture needs the later claim paid by a lane the failed build never used",
        );
        assert!(
            !lock_recover(&graph.debt).owes_failed(),
            "the failed build's outcome was reported against the claim that landed after it",
        );
    }

    /// A grant whose builder could not be spawned is given back with the failure.
    ///
    /// No thread will ever take that ticket. Left in the slot it reads as a build in flight,
    /// and the entry a request and the boot use returns on it for good — the retry the spawn
    /// failure scheduled is owed to a graph nobody will ever build again.
    #[test]
    fn a_refused_spawn_gives_its_claim_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());

        graph.loader_cannot_spawn.store(true, Ordering::SeqCst);
        graph.ensure_loading();
        assert!(matches!(graph.status(), GraphStatus::Failed(_)), "the spawn is refused");
        assert!(!graph.build_in_flight(), "a refused first-build spawn left its grant in the slot");
        graph.loader_cannot_spawn.store(false, Ordering::SeqCst);
        assert_eq!(
            graph.debt_standing(Instant::now()).failed,
            Some(crate::graph::debt::Ripeness::Now),
            "the fixture needs the spawn failure's retry due now",
        );
        graph.ensure_first_build();
        wait_until(&graph, "the retry a request asked for to start", || {
            !matches!(graph.status(), GraphStatus::Failed(_))
        });
        wait_ready(&graph);
    }

    /// [`a_refused_spawn_gives_its_claim_back`] for a reload.
    #[test]
    fn a_refused_reload_spawn_gives_its_claim_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        graph.loader_cannot_spawn.store(true, Ordering::SeqCst);
        lock_recover(&graph.debt).record_forced(Instant::now(), 7);
        assert!(
            matches!(graph.try_claim_reload(true), ReloadClaim::Claimed),
            "the slot is granted"
        );
        graph.spawn_reload();
        assert!(
            matches!(
                lock_recover(&graph.inner).published.as_ref().map(|p| p.reload.clone()),
                Some(ReloadState::Failed(_))
            ),
            "the reload spawn is refused",
        );
        assert!(!graph.build_in_flight(), "a refused reload spawn left its grant in the slot");
    }

    /// A cold graph whose daemon could not confirm it owns the workspace — a peer holds the
    /// lease lock, so the claim it made at startup never went through — takes no build for the
    /// asking. Ownership is not answered by the absence of a verdict: the build would scan and
    /// write a whole database that its own publication fence then throws away.
    fn unclaimed_lease_under_a_held_lock(
        cache: &crate::cache::WorkspaceCacheLayout,
    ) -> crate::workspace_lease::WorkspaceLease {
        let lease = crate::workspace_lease::WorkspaceLease::while_cache_lock_held(cache, || {
            crate::workspace_lease::WorkspaceLease::claim_cache(cache)
        });
        assert!(!cache.lease_path().exists(), "the fixture needs a startup claim that failed");
        lease
    }

    fn kick_asked_the_lease(lease: &crate::workspace_lease::WorkspaceLease) -> bool {
        lease.disk_check_threads().iter().any(|thread| thread == "bsl-graph-first-build")
    }

    #[test]
    fn a_first_build_is_not_admitted_while_ownership_is_unconfirmed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = unclaimed_lease_under_a_held_lock(&cache);
        let peer = lease.hold_file_lock_for_test();
        let admitted = Arc::new(AtomicUsize::new(0));
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone())
            .with_post_claim_hook({
                let admitted = Arc::clone(&admitted);
                Arc::new(move |_: &GraphState| {
                    admitted.fetch_add(1, Ordering::SeqCst);
                })
            });

        graph.ensure_first_build();
        // The kick has asked the lease, and has either admitted a build or put the ask back.
        wait_until(&graph, "the kick to decide under the held lock", || {
            kick_asked_the_lease(&lease)
                && (admitted.load(Ordering::SeqCst) > 0
                    || graph.first_build_asked.load(Ordering::SeqCst))
        });
        assert_eq!(
            admitted.load(Ordering::SeqCst),
            0,
            "a first build was admitted while the lease could not confirm ownership",
        );
        assert_eq!(graph.status(), GraphStatus::Idle, "and nothing was claimed for it");

        // The obstacle ends, and the ask that was already made is answered without another.
        drop(peer);
        wait_ready(&graph);
        assert_eq!(admitted.load(Ordering::SeqCst), 1, "exactly one build answered the ask");
        graph.stop.stop();
    }

    /// Countercontrol: a lease that owns the workspace answers the ask with exactly one build.
    #[test]
    fn a_first_build_under_an_owned_lease_is_admitted_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(cache.lease_path().exists(), "the fixture needs an owning lease");
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        graph.ensure_first_build();
        wait_ready(&graph);
        assert_eq!(graph.builders_started.load(Ordering::SeqCst), 1, "one build for one ask");
        lease.release();
    }

    /// Countercontrol: a lease another daemon has visibly taken over is terminal, and an ask
    /// builds nothing and waits for nothing.
    #[test]
    fn a_first_build_under_a_superseded_lease_is_not_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let newer = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(!lease.owns_caches_now(), "the fixture needs the takeover observed");
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        graph.ensure_first_build();
        wait_until(&graph, "the kick to finish", || {
            !graph.first_build_kick.load(Ordering::SeqCst)
                && !graph.first_build_asked.load(Ordering::SeqCst)
        });
        assert_eq!(graph.builders_started.load(Ordering::SeqCst), 0, "a superseded lease built");
        assert_eq!(graph.status(), GraphStatus::Idle);
        newer.release();
    }

    /// Countercontrol: a failed graph whose retry is due builds nothing while ownership cannot
    /// be confirmed either.
    #[test]
    fn a_due_retry_is_not_admitted_while_ownership_is_unconfirmed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = unclaimed_lease_under_a_held_lock(&cache);
        let peer = lease.hold_file_lock_for_test();
        let admitted = Arc::new(AtomicUsize::new(0));
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone())
            .with_post_claim_hook({
                let admitted = Arc::clone(&admitted);
                Arc::new(move |_: &GraphState| {
                    admitted.fetch_add(1, Ordering::SeqCst);
                })
            });
        lock_recover(&graph.debt).record_failure(
            Instant::now(),
            FailureKind::Transient,
            crate::graph::debt::Sponsors { primary: true, marks: false },
        );
        lock_recover(&graph.inner).status = GraphStatus::Failed("transient".to_owned());
        graph.ensure_first_build();
        wait_until(&graph, "the kick to decide under the held lock and finish", || {
            kick_asked_the_lease(&lease)
                && !graph.first_build_kick.load(Ordering::SeqCst)
                && !graph.first_build_asked.load(Ordering::SeqCst)
        });
        assert_eq!(
            admitted.load(Ordering::SeqCst),
            0,
            "a due retry was admitted while the lease could not confirm ownership",
        );
        drop(peer);
        graph.stop.stop();
    }

    /// An ask that lands while a watcherless kick is letting go is still carried.
    ///
    /// The kick takes the ask, decides, and only then releases the single-flight flag. A request
    /// arriving in between sees the flag taken and leaves its ask for that kick — which has
    /// already read the ask it came for and would not look again.
    #[test]
    fn an_ask_made_while_a_kick_lets_go_is_answered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let parked = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_kick_release_hook({
            let (parked, release) = (Arc::clone(&parked), Arc::clone(&release));
            Arc::new(move |_: &GraphState| {
                if parked.swap(true, Ordering::SeqCst) {
                    return;
                }
                let deadline = Instant::now() + Duration::from_secs(10);
                while !release.load(Ordering::SeqCst) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        });
        // A failed graph whose retry is held off: the first kick takes the ask and starts
        // nothing.
        let now = Instant::now();
        lock_recover(&graph.debt).fail_held_until(now, now + Duration::from_secs(3600));
        lock_recover(&graph.inner).status = GraphStatus::Failed("held".to_owned());
        graph.ensure_first_build();
        wait_until(&graph, "the first kick to park after taking the ask", || {
            parked.load(Ordering::SeqCst)
        });
        // The hold elapses, and a request asks again while that kick is letting go.
        lock_recover(&graph.debt).fail_held_until(now, now);
        assert_eq!(
            graph.debt_standing(Instant::now()).failed,
            Some(crate::graph::debt::Ripeness::Now),
            "the fixture needs the second ask to be for a retry due now",
        );
        graph.ensure_first_build();
        assert!(
            graph.first_build_kick.load(Ordering::SeqCst),
            "the fixture needs the second ask to find the kick still taken",
        );
        release.store(true, Ordering::SeqCst);
        wait_until_within(&graph, Duration::from_secs(10), "the ask to be answered", || {
            !matches!(graph.status(), GraphStatus::Failed(_))
        });
        wait_ready(&graph);
    }

    /// A warm cache published by a lazy first build gives back only a grant of its own.
    ///
    /// The builder took its ticket out of the slot, so the slot holds nothing of its admission
    /// once the cache is installed. What is there by then is somebody else's: the graph reads
    /// Ready, and another claim is legal. Clearing the slot outright deleted that claim.
    #[test]
    fn a_warm_cache_publication_leaves_a_claim_that_landed_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        super::super::test_support::seed_cache(
            root,
            super::super::scan::workspace_fingerprint(root),
        );
        let this: Arc<std::sync::OnceLock<GraphState>> = Arc::new(std::sync::OnceLock::new());
        let later: Arc<Mutex<Option<super::super::debt::BuildTicket>>> = Arc::new(Mutex::new(None));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_install_section_hook({
            let (this, later) = (Arc::clone(&this), Arc::clone(&later));
            Arc::new(move |section: &'static str| {
                if section != "gate-released" || lock_recover(&later).is_some() {
                    return;
                }
                let Some(graph) = this.get() else { return };
                lock_recover(&graph.debt).record_forced(Instant::now(), 42);
                if matches!(graph.try_claim_reload(true), ReloadClaim::Claimed) {
                    *lock_recover(&later) = graph.claimed_ticket();
                }
            })
        });
        assert!(this.set(graph.clone()).is_ok());

        graph.ensure_loading();
        wait_until(&graph, "the cache to publish and its builder to let go", || {
            lock_recover(&later).is_some() && lock_recover(&graph.inner).building.is_none()
        });
        let later = lock_recover(&later).expect("the fixture needs a claim after the install");
        assert!(
            lock_recover(&graph.inner).published.as_ref().is_some_and(|p| !p.stale),
            "the fixture needs the warm cache path",
        );
        assert_eq!(
            graph.claimed_ticket().map(|ticket| ticket.claim),
            Some(later.claim),
            "the warm cache path gave back a claim that was not its own",
        );
    }
}
