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

/// The pause after the FIRST failed build before another attempt is allowed, doubled per
/// consecutive failure and capped by [`RETRY_CAP`]. A failure is retryable, but a tree
/// that cannot be read will not start reading by being asked again in a tight loop.
pub(super) const RETRY_BASE: Duration = Duration::from_secs(5);

/// The ceiling of the failed-build retry backoff: a build that keeps failing is retried
/// at most this often.
pub(super) const RETRY_CAP: Duration = Duration::from_secs(60);

/// How long a `Loading` build may run before the sweeper publishes it as `Failed`.
///
/// Without this a build that hangs pins every reader on `loading` for ever: the loader
/// thread's own completion is what would clear `loading_since`, and a stuck thread never
/// reaches it. The default is generous on purpose — a cold resident build over a large
/// configuration takes minutes, and a deadline that fires into a working build turns a
/// slow answer into a spurious failure. It is a backstop for a hung thread, not a budget.
pub(super) const BUILD_TIMEOUT: Duration = Duration::from_secs(600);

/// The floor of the retry base. Nothing may pace a retry faster than this, whatever the
/// environment says: a base of a second turns every transient read failure into a build
/// storm, and the deadline below would publish each attempt as `Failed` before it could
/// finish — a schedule that manufactures the very failures it is pacing.
pub(super) const MIN_RETRY_BASE: Duration = RETRY_BASE;

/// The floor of the build deadline: a deadline shorter than this fails working builds on a
/// large configuration, which is a spurious failure, not a backstop.
pub(super) const MIN_BUILD_TIMEOUT: Duration = Duration::from_secs(60);

/// The retry base this process runs with: `BSL_MCP_DIAG_RETRY_BASE_SECS`, floored at
/// [`MIN_RETRY_BASE`]. Unset and zero both mean the default, like `BSL_MCP_RECONCILE_SECS`.
fn clamp_retry_base(secs: Option<u64>) -> Duration {
    secs.filter(|secs| *secs > 0)
        .map_or(RETRY_BASE, |secs| Duration::from_secs(secs).max(MIN_RETRY_BASE))
}

/// The retry ceiling this process runs with: `BSL_MCP_DIAG_RETRY_CAP_SECS`, never below the
/// base it caps (a ceiling under the floor is not a ceiling — it is the floor wearing the
/// other name, and the caller that set it believes the schedule is longer than it is).
fn clamp_retry_cap(secs: Option<u64>, base: Duration) -> Duration {
    secs.filter(|secs| *secs > 0)
        .map_or(RETRY_CAP.max(base), |secs| Duration::from_secs(secs).max(base))
}

/// The build deadline this process runs with: `BSL_MCP_DIAG_BUILD_TIMEOUT_SECS`, floored at
/// [`MIN_BUILD_TIMEOUT`].
fn clamp_build_timeout(secs: Option<u64>) -> Duration {
    secs.filter(|secs| *secs > 0)
        .map_or(BUILD_TIMEOUT, |secs| Duration::from_secs(secs).max(MIN_BUILD_TIMEOUT))
}

fn env_secs(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok())
}

/// The pacing this process runs with, read once at construction: the failed-build retry
/// `(base, cap)` and the build deadline, each clamped to its floor. Overridable from the
/// environment because the schedule is a deployment property — a CI box can afford a faster
/// retry than a laptop — and clamped because a schedule below the floors is not a schedule,
/// it is a source of spurious failures.
fn pacing_from_env() -> Pacing {
    let base = clamp_retry_base(env_secs("BSL_MCP_DIAG_RETRY_BASE_SECS"));
    let cap = clamp_retry_cap(env_secs("BSL_MCP_DIAG_RETRY_CAP_SECS"), base);
    let build_timeout = clamp_build_timeout(env_secs("BSL_MCP_DIAG_BUILD_TIMEOUT_SECS"));
    Pacing { base, cap, build_timeout }
}

/// The three pacing values the resident lifecycle is run with. Held TOGETHER in the state the
/// mutex guards, never on the handle: a handle is cloned freely (every request takes one) and
/// a pacing that lived on the clone would be read by whichever clone happened to ask while the
/// others — the loader, the sweeper, the daemon's own handle — kept the old one.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pacing {
    /// Pause after the first failed build; doubles per consecutive failure (see [`retry_delay`]).
    pub(super) base: Duration,
    /// Ceiling of that backoff.
    pub(super) cap: Duration,
    /// How long a `Loading` build may run before the sweeper publishes it as `Failed`.
    pub(super) build_timeout: Duration,
}

/// The pause before the attempt that follows `attempts` consecutive failures: `base`
/// doubled per failure, capped. Pure, so the schedule is provable apart from the clock —
/// the same shape `state::overlay_retry::retry_delay` publishes for its own owner.
pub(super) fn retry_delay(base: Duration, cap: Duration, attempts: u32) -> Duration {
    if attempts == 0 {
        return Duration::ZERO;
    }
    // Doubled step by step and stopped at the cap, so any ceiling the environment sets is
    // reached rather than plateaued under: a fixed exponent clamp would stop short of a long
    // cap, and a shift by `attempts` overflows. The loop ends once the cap is reached, which a
    // saturating doubling does within a few dozen steps whatever `attempts` says.
    let mut delay = base;
    for _ in 1..attempts {
        if delay >= cap || delay.is_zero() {
            break;
        }
        delay = delay.saturating_mul(2);
    }
    delay.min(cap)
}

/// Milliseconds of `span`, saturated at `u64::MAX`: a duration that long is not a number any
/// caller acts on, and wrapping it into a small one would read as "no wait owed".
fn millis(span: Duration) -> u64 {
    u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
}

/// The pause a failed build still owes at `now`: `None` when the retry is due. `None` too
/// for a failure nobody stamped here (a state written from outside): waiting would be
/// waiting on a clock that was never set, so the attempt is allowed at once.
fn retry_wait(inner: &Inner, now: Instant) -> Option<Duration> {
    let failed_at = inner.failed_at?;
    let delay = retry_delay(inner.pacing.base, inner.pacing.cap, inner.failed_attempts);
    let Some(due) = failed_at.checked_add(delay) else {
        // A clock far enough out that the deadline cannot be represented: the pause is not
        // "already over", so the retry waits out the ceiling instead of firing at once.
        return Some(inner.pacing.cap);
    };
    (due > now).then(|| due - now)
}

/// Publish a build failure on `inner`: the reason, its clock and the attempt count behind
/// it. Shared by `run_load`'s error arm and the sweeper's build deadline, so a build that
/// failed and one that never finished leave the SAME state — a cause the caller can see
/// and a retry paced from the same schedule.
fn publish_build_failure(inner: &mut Inner, message: String) {
    inner.loading_since = None;
    inner.failed_attempts = inner.failed_attempts.saturating_add(1);
    inner.failed_at = Some(Instant::now());
    inner.failed_message = Some(message.clone());
    inner.status = DiagnosticsStatus::Failed(message);
}

/// What a caller still needs after [`publish_build_success`] moved the built resident into
/// the state: the scan roots the hub is re-armed on, and the two identity values that re-arm
/// is guarded by.
struct PublishTail {
    files: usize,
    scan_roots: Vec<PathBuf>,
    physical_topology: u64,
    declaration_epoch: u64,
}

/// Publish a built resident on `inner`: the swap-in EVERY successful (re)build performs, in
/// one place so a first build and a reload cannot publish different states. A reload that
/// forgot to clear the failure bookkeeping, say, would leave a `Ready` resident still
/// reporting the cause it answered.
fn publish_build_success(inner: &mut Inner, built: ResidentBuild) -> PublishTail {
    let files = built.resident.file_count();
    inner.resident = Some(built.resident);
    inner.stats = built.stats;
    inner.baseline_epoch += 1;
    inner.config_fp = built.config_fp;
    inner.topology = built.topology;
    inner.generation += 1;
    inner.reload = ReloadState::Idle;
    inner.loading_since = None;
    // A build that succeeded answers both the failure it retried and the schedule that
    // failure was being retried on: the next failure starts again from the base pause.
    inner.failed_message = None;
    inner.failed_at = None;
    inner.failed_attempts = 0;
    inner.status = DiagnosticsStatus::Ready { files };
    PublishTail {
        files,
        scan_roots: built.scan_roots,
        physical_topology: built.physical_topology,
        declaration_epoch: built.declaration_epoch,
    }
}

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
    /// `elapsed_ms`. Set on `Idle → Loading` (and on `Failed → Loading`), cleared when the
    /// build publishes an outcome.
    pub(super) loading_since: Option<Instant>,
    /// The message of the last published build failure. Kept while a retry of that failure
    /// runs — a `Loading` that erased the cause would leave a caller unable to tell a retry
    /// from a first attempt — and cleared when a build finally succeeds.
    pub(super) failed_message: Option<String>,
    /// When that failure was published: the clock the retry backoff is measured from (the
    /// observable form of it is `failed_age_ms`). Cleared on success.
    pub(super) failed_at: Option<Instant>,
    /// Consecutive failed build attempts since the last successful one. Indexes the retry
    /// schedule: the attempt after the n-th failure waits `retry_delay(n)`.
    pub(super) failed_attempts: u32,
    /// The retry pacing and the build deadline (see [`Pacing`]). Bound to the STATE, not to a
    /// handle: clones are handed to every request, the loader and the sweeper, and pacing read
    /// off a clone would be that clone's pacing alone.
    pub(super) pacing: Pacing,
    /// The build attempt in flight, and the token that tells a live one from a retired one.
    ///
    /// Bumped when an attempt is STARTED (`ensure_loading`) and when a live one is RETIRED
    /// (the deadline giving up on it). `run_load` captures its token and publishes its outcome
    /// only while the state still holds it: a build the deadline already gave up on may finish
    /// minutes later, and publishing it unconditionally is how a stale success replaces a
    /// newer resident — or a stale failure takes one down, refusing every reader, stopping
    /// eviction and pinning the hub cursor — under a state that is already serving.
    pub(super) build_attempt: u64,
    /// One-shot test gate every loader waits in before publishing, so a test can HOLD one
    /// attempt while another runs and observe which of the two the state accepts. Unarmed in
    /// production, where it is not read at all.
    #[cfg(test)]
    pub(super) build_gate: Option<Arc<BuildGate>>,
    /// How many loaders found their token retired and published nothing. The only way a test
    /// can see a REFUSAL: the state a refused attempt leaves behind is exactly the state it
    /// found, which is indistinguishable from an attempt that never ran.
    #[cfg(test)]
    pub(super) stale_build_publications: Arc<AtomicUsize>,
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
    /// One-shot test seam fired INSIDE the failure snapshot, between the read that came back
    /// failed and the lifecycle sample that words it. That window is the only one in which the
    /// state can move on under an answer already decided to be a failure, and the seam is what
    /// makes the race a schedule instead of a hope.
    #[cfg(test)]
    pub(super) failure_probe: ReconcileProbe,
    /// One-shot test seam fired at the very start of a loader, before it reads or touches
    /// anything — the only moment at which an attempt can be retired before it takes a cursor,
    /// and therefore the only place the "retired attempts touch nothing" rule can be observed.
    #[cfg(test)]
    pub(super) load_start_probe: ReconcileProbe,
}

/// A one-shot callback the reconciler fires between its first drain and its scan.
#[cfg(test)]
pub(super) type ReconcileProbe = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// The gate a test holds a loader in before it publishes.
///
/// The subject of the build-attempt token is a race between two attempts, and a race a test
/// cannot schedule is one it cannot observe: without a hold, the first attempt publishes
/// before the second is even started, and the test proves nothing but that builds finish.
/// So the loader's publish step is gated — the test arms the gate, waits until the attempts
/// it wants are inside it, and releases them together, which is the one arrangement in which
/// "both are live and only one may publish" is a fact rather than a hope.
#[cfg(test)]
pub(crate) struct BuildGate {
    shared: Arc<(Mutex<GateState>, std::sync::Condvar)>,
}

#[cfg(test)]
#[derive(Default)]
struct GateState {
    held: usize,
    released: bool,
}

#[cfg(test)]
impl BuildGate {
    /// The loader's side: raise the count, then wait for the release. Bounded twice over, so a
    /// test that panics while holding the gate — or simply forgets it — cannot leave a build
    /// thread parked for the life of the process: each wait times out, and the whole hold ends
    /// after [`BuildGate::HOLD_BUDGET`], after which the loader publishes as if unarmed.
    fn hold(&self) {
        let started = Instant::now();
        let (lock, wake) = &*self.shared;
        let mut state = lock_recover(lock);
        state.held += 1;
        wake.notify_all();
        while !state.released && started.elapsed() < Self::HOLD_BUDGET {
            let (next, _) = wake
                .wait_timeout(state, Duration::from_secs(1))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
        }
    }

    /// How long a loader may be held before it gives up on the gate. Far longer than any gate
    /// in this file holds one (they hold for milliseconds), and far shorter than the harness
    /// itself.
    const HOLD_BUDGET: Duration = Duration::from_secs(20);

    /// How many loaders are parked in the gate right now.
    fn held(&self) -> usize {
        lock_recover(&self.shared.0).held
    }

    /// The test's side: what the subject is, so it is a gate and not a sleep.
    #[cfg(test)]
    pub(crate) fn wait_until_held(&self, count: usize, what: &str) {
        let started = Instant::now();
        while self.held() < count {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "{what}: only {} of {count} loaders reached the gate",
                self.held(),
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Let every parked loader publish. One-way: a gate is used once.
    pub(crate) fn release(&self) {
        let (lock, wake) = &*self.shared;
        lock_recover(lock).released = true;
        wake.notify_all();
    }
}

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
        publish_build_failure(&mut inner, message.to_owned());
    }

    /// State the retry schedule this handle runs with, for a test whose subject is the
    /// pacing rather than the number it starts from. The pacing lives in the STATE, so this
    /// reaches the loader, the sweeper and every clone — a handle-level setting would be
    /// invisible to all three, and the gate built on it would be measuring the defaults.
    #[cfg(test)]
    pub(crate) fn with_retry_pacing_for_test(&self, base: Duration, cap: Duration) {
        let mut inner = lock_recover(&self.inner);
        inner.pacing.base = base;
        inner.pacing.cap = cap;
    }

    /// State the build deadline this handle runs with, for a test whose subject is the
    /// sweeper's deadline (see [`Self::expire_stuck_build`]).
    #[cfg(test)]
    pub(crate) fn with_build_timeout_for_test(&self, timeout: Duration) {
        lock_recover(&self.inner).pacing.build_timeout = timeout;
    }

    /// Arm the loader gate every build waits in before publishing, and hand back the handle a
    /// test releases it with. Disarmed by never arming it, which is what production does.
    #[cfg(test)]
    pub(crate) fn arm_build_gate_for_test(&self) -> Arc<BuildGate> {
        let gate = Arc::new(BuildGate {
            shared: Arc::new((Mutex::new(GateState::default()), std::sync::Condvar::new())),
        });
        lock_recover(&self.inner).build_gate = Some(Arc::clone(&gate));
        gate
    }

    /// How many loaders found their token retired and published nothing.
    #[cfg(test)]
    pub(crate) fn stale_build_publications(&self) -> usize {
        lock_recover(&self.inner).stale_build_publications.load(Ordering::SeqCst)
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
                failed_message: None,
                failed_at: None,
                failed_attempts: 0,
                pacing: pacing_from_env(),
                build_attempt: 0,
                #[cfg(test)]
                build_gate: None,
                #[cfg(test)]
                stale_build_publications: Arc::new(AtomicUsize::new(0)),
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
            #[cfg(test)]
            failure_probe: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            load_start_probe: Arc::new(Mutex::new(None)),
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
            // A retry keeps the cause of the failure it answers: without it the caller sees
            // a `loading` it cannot tell from a first attempt, and the reason it is being
            // made at all would be known only to the log.
            DiagnosticsStatus::Loading => inner.failed_message.clone(),
            _ => None,
        };
        let now = Instant::now();
        // Owed only while FAILED: a build in flight has already been started, so there is
        // nothing to wait for and `elapsed_ms` speaks for it instead. `Some(0)` means the
        // next attempt is due now — a different fact from `None` ("no wait is owed because
        // nothing is being retried").
        let retry_after_ms = match &inner.status {
            DiagnosticsStatus::Failed(_) => {
                inner.failed_at.map(|_| retry_wait(&inner, now).map_or(0, millis))
            }
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
            elapsed_ms: inner.loading_since.map(|t| millis(t.elapsed())),
            failed_age_ms: inner.failed_at.map(|at| millis(now.saturating_duration_since(at))),
            attempts: inner.failed_attempts,
            retry_after_ms,
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

    /// Trigger the background build if this is the first call (or the db was evicted, or a
    /// failed build's backoff has elapsed). Transitions `Idle → Loading` and spawns exactly
    /// one loader thread; later calls return immediately. No-op for disabled / loading /
    /// ready, and for a failure that is not yet due for another attempt.
    ///
    /// A FAILED build is retryable, on a backoff (see [`RETRY_BASE`]/[`RETRY_CAP`]): the
    /// attempt that follows the n-th failure waits `retry_delay(n)` from the moment that
    /// failure was published. A caller arriving inside the pause is refused here and reads
    /// the remaining wait off the status — asking again never spends an attempt, so a poller
    /// cannot turn its own polling into a build storm. The retry runs the same path a first
    /// load does, so a resident left behind by the failed attempt (a hub cursor, say) is
    /// re-derived from scratch rather than half-reused.
    ///
    /// EXACTLY ONE attempt holds the right to PUBLISH an outcome, and that right is the
    /// [`Inner::build_attempt`] token handed to the loader here: the transition to `Loading`
    /// happens under the lock and stamps the attempt, so an attempt the deadline later retires
    /// can neither take a cursor nor publish its ending. The retired attempt's THREAD may well
    /// live on (a build that is merely slow finishes its walk) — what it loses is the right to
    /// say anything: its outcome is discarded, and the state keeps what the attempt that
    /// replaced it published.
    pub(crate) fn ensure_loading(&self) {
        if self.workspace_root.is_none() {
            return;
        }
        let attempt = {
            let mut inner = lock_recover(&self.inner);
            match inner.status {
                DiagnosticsStatus::Idle => {}
                DiagnosticsStatus::Failed(_) => {
                    let now = Instant::now();
                    if retry_wait(&inner, now).is_some() {
                        return;
                    }
                }
                _ => return,
            }
            inner.build_attempt = inner.build_attempt.wrapping_add(1);
            inner.status = DiagnosticsStatus::Loading;
            inner.loading_since = Some(Instant::now());
            inner.build_attempt
        };
        // Spawn the single idle sweeper on first use (it outlives evict→rebuild cycles).
        if !self.sweeper_started.swap(true, Ordering::SeqCst) {
            self.spawn_sweeper();
        }
        let state = self.clone();
        let spawned = std::thread::Builder::new()
            .name("bsl-diag-init".to_owned())
            .spawn(move || state.run_load(attempt));
        if let Err(e) = spawned {
            let mut inner = lock_recover(&self.inner);
            // The loader never started, so this attempt is over: its failure is published by
            // the caller that started it, under the token it was given.
            if inner.build_attempt == attempt {
                publish_build_failure(&mut inner, format!("could not spawn loader: {e}"));
            }
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
    /// publish `Ready`.
    ///
    /// `attempt` is the token `ensure_loading` handed out for this loader. The outcome — a
    /// resident or a failure — is published ONLY while the state still holds that token: a
    /// build the sweeper's deadline has given up on keeps running in its thread, and both of
    /// its endings are wrong by the time it reaches them. Its success would swap in a
    /// resident built from a baseline older than the one the replacing attempt is publishing;
    /// its failure would take a `Ready` resident down — every reading tool refusing over a
    /// database that is right there, eviction unable to drop it and the hub cursor never
    /// released. A retired attempt publishes NOTHING and says so in the log.
    fn run_load(&self, attempt: u64) {
        let Some(root) = self.workspace_root.clone() else {
            return;
        };
        #[cfg(test)]
        self.fire_load_start_probe();
        // The attempt can be retired before it has done anything at all: the deadline gives up
        // on a build still in its startup, and the retry that replaces it takes a cursor of its
        // own. The cursor slot belongs to the STATE, not to this thread — re-subscribing here
        // would drop the subscription the LIVE attempt is building under, leaving the resident
        // that arrives to be served under a window nobody opened for it. So a retired attempt
        // touches nothing, cursor included.
        //
        // The check and the re-subscription happen under ONE hold of `inner`: retiring an
        // attempt takes the same lock, so a deadline that fires between a passed check and the
        // cursor swap cannot leave a retired attempt replacing the live one's subscription.
        // Holding `inner` while taking the cursor is the order eviction already uses — no path
        // holds the cursor and then asks for `inner`.
        //
        // Snapshot the hub cursor at build start: the build's baseline scan captures the
        // disk as of now, so only events landing after this point need replaying onto the
        // published resident.
        //
        // `run_load` spawns from `Idle` (cursor slot empty: the slot is dropped on eviction
        // and never filled before a first build) and from a failed attempt whose backoff has
        // elapsed. The failed attempt's own `resubscribe_cursor` left its subscription in the
        // slot, so a retry takes the `hub.resubscribe` branch and may carry a debt forward —
        // the same story `run_reload` tells, and the reason the carry is settled below rather
        // than dropped. Whoever holds the slot, the subscription is replaced here so the
        // retry's baseline scan covers exactly the window its own events land after.
        let carried = {
            let inner = lock_recover(&self.inner);
            if inner.build_attempt != attempt {
                drop(inner);
                self.discard_stale_attempt(attempt, "start");
                return;
            }
            let carried = self.resubscribe_cursor();
            drop(inner);
            carried
        };
        tracing::info!(?root, attempt, "diagnostics resident db build started");
        match Self::catch_build(|| Self::build_resident(&root, &self.excluded)) {
            Ok(built) => {
                self.hold_at_build_gate();
                let mut inner = lock_recover(&self.inner);
                if inner.build_attempt != attempt {
                    drop(inner);
                    self.discard_stale_attempt(attempt, "success");
                    return;
                }
                let tail = publish_build_success(&mut inner, built);
                drop(inner);
                *lock_recover(&self.scan) = None;
                // The build read the tree whole, so the reconcile its fresh cursor carried
                // is answered. Settled BEFORE the hub re-arm below: that re-arm can raise a
                // window of its own, and that one is nobody's to pay here.
                self.settle_rebuild_debt(carried);
                tracing::info!(files = tail.files, "diagnostics resident db ready");
                self.ensure_hub_roots(
                    &tail.scan_roots,
                    tail.physical_topology,
                    tail.declaration_epoch,
                );
                self.recheck_config_identity_after_publish();
            }
            Err(msg) => {
                self.hold_at_build_gate();
                let mut inner = lock_recover(&self.inner);
                if inner.build_attempt != attempt {
                    drop(inner);
                    self.discard_stale_attempt(attempt, "failure");
                    return;
                }
                tracing::warn!("diagnostics resident db build failed: {msg}");
                publish_build_failure(&mut inner, msg);
            }
        }
    }

    /// Drop the outcome of an attempt whose token has been retired, saying so in the log: an
    /// attempt that publishes nothing is otherwise invisible — its effect on the state is
    /// exactly nothing, which is indistinguishable from never having run.
    fn discard_stale_attempt(&self, attempt: u64, outcome: &str) {
        let current = lock_recover(&self.inner).build_attempt;
        #[cfg(test)]
        lock_recover(&self.inner).stale_build_publications.fetch_add(1, Ordering::SeqCst);
        tracing::warn!(attempt, current, outcome, "discarding a superseded resident build attempt",);
    }

    /// Park at the armed test gate, if any. Production never arms one: the whole function is
    /// `#[cfg(test)]`, so a release build has no gate at all rather than a disabled one.
    #[cfg(test)]
    fn hold_at_build_gate(&self) {
        let gate = lock_recover(&self.inner).build_gate.clone();
        if let Some(gate) = gate {
            gate.hold();
        }
    }

    #[cfg(not(test))]
    fn hold_at_build_gate(&self) {}

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
        let carried = self.resubscribe_cursor();
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
                let mut inner = lock_recover(&self.inner);
                let tail = publish_build_success(&mut inner, built);
                drop(inner);
                *lock_recover(&self.scan) = None;
                // The swap states this build read the tree whole, so the reconcile its
                // fresh cursor carried is answered here — before the hub re-arm below,
                // whose window would be a new debt and nobody's to settle.
                self.settle_rebuild_debt(carried);
                tracing::info!(files = tail.files, "diagnostics resident db reloaded");
                self.ensure_hub_roots(
                    &tail.scan_roots,
                    tail.physical_topology,
                    tail.declaration_epoch,
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
    /// is visible AND retryable: a published failure is retried on the backoff [`RETRY_BASE`]
    /// schedules (see [`DiagnosticsState::ensure_loading`]), and a build that hangs rather
    /// than failing is published as `Failed` by the sweeper's deadline. Generic over the
    /// closure so the fold itself is testable.
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
        let mut config = ide::DiagnosticsConfig::from_project_file(
            &diagnostics,
            project.config.output.resolve_locale().unwrap_or_default(),
            project.config.config_file_path(),
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
        super::resident::apply_project_compatibility_mode(&mut db, project.compatibility_mode());
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
    /// for `eviction_after`, publishes an overrun `Loading` build as `Failed` (see
    /// [`Self::expire_stuck_build`]), and otherwise sleeps on. It does NOT exit after
    /// evicting, so a later rebuild (via `ensure_loading`/`run_reload`) is monitored again
    /// without needing a replacement sweeper.
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

                // A build that overran its deadline is a failure like any other — and the
                // only thing that can end it: the loader thread's own completion is what
                // would clear `loading_since`, and a stuck thread never reaches it.
                state.expire_stuck_build();

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

    /// Publish a `Loading` build that overran its deadline as `Failed`, so a build that
    /// never finishes takes the same route as one that failed outright: the reason is
    /// visible, and the retry schedule (`pacing`) applies to it. Returns whether it fired.
    ///
    /// The LIVE attempt is retired here as well (`build_attempt` is bumped), because the
    /// thread it names is still running: publishing `Failed` without retiring it would leave
    /// two attempts entitled to publish, and the one that is merely slow could finish minutes
    /// later and overwrite the retry's outcome — or, failing, take down the resident the retry
    /// published.
    ///
    /// Only `Loading` is watched. `Ready` serves a resident and is nobody's to fail here,
    /// and an in-flight RELOAD keeps serving the resident it has — its failure has its own
    /// channel (`reload`) and its own repair, and turning it into a build failure would
    /// retire the answer the reload was protecting.
    ///
    /// The sweeper is the clock above this step and the decision and the act live here, on
    /// the same split as [`Self::evict_if_idle`]: a test whose subject is the deadline calls
    /// this directly and does not depend on when the clock gets round to it.
    pub(super) fn expire_stuck_build(&self) -> bool {
        let mut inner = lock_recover(&self.inner);
        if !matches!(inner.status, DiagnosticsStatus::Loading) {
            return false;
        }
        let Some(since) = inner.loading_since else {
            return false;
        };
        if since.elapsed() < inner.pacing.build_timeout {
            return false;
        }
        let message = format!(
            "diagnostics resident build did not finish within {:?}",
            inner.pacing.build_timeout
        );
        inner.build_attempt = inner.build_attempt.wrapping_add(1);
        tracing::warn!("{message}");
        publish_build_failure(&mut inner, message);
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
    use crate::diagnostics_state::test_support::{
        sample_workspace, state_with_hub, wait_ready, wait_until,
    };

    /// The sweeper parks between ticks, and the shutdown wakes it out of that park rather
    /// than leaving it to time out. The tick is half a minute: slept out, a daemon told to
    /// stop keeps a thread walking its workspace for the rest of it.
    ///
    /// Only the clock tells a woken sweeper from one that slept its tick out, so an ordinary run
    /// checks that the shutdown ends it, and the timing profile that it ends it at once.
    #[test]
    fn a_shutdown_wakes_the_idle_sweeper_out_of_its_tick() {
        use crate::change_hub::test_support::eventually;
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let state = DiagnosticsState::for_workspace(dir.path().to_path_buf());
        state.spawn_sweeper();
        assert!(
            eventually(|| state.sweeper_running()),
            "the sweeper never started, so its exit below would prove nothing"
        );

        // Parked, in the timing profile: its next tick is SWEEP_INTERVAL away and nothing else
        // wakes it. A shutdown that lands before it parks must end it just the same.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(state.sweeper_running());
        assert!(
            SWEEP_INTERVAL >= Duration::from_secs(5),
            "the tick is short enough that timing out would pass for being woken"
        );

        let asked = Instant::now();
        state.shutdown();

        assert!(
            crate::change_hub::test_support::eventually(|| !state.sweeper_running()),
            "the sweeper slept through the shutdown"
        );
        // A sweeper that slept its tick out costs a whole interval; half of one is beyond any
        // slow machine.
        let stopped = asked.elapsed();
        if cfg!(timing_contract) {
            assert!(
                stopped < SWEEP_INTERVAL / 2,
                "the sweeper slept out its interval: {stopped:?}"
            );
            assert!(
                stopped < Duration::from_secs(1),
                "the sweeper slept out its interval: {stopped:?}"
            );
        }
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

    /// `references`, `outline` and `symbol_info` address a root by the id a search hit carried, and
    /// search keeps that id when the link declaring the root is retargeted. A resident rebuilt after
    /// the move must therefore register the root under the id search serves — resolved to the NEW
    /// target — rather than the id a fresh build would mint from the new target.
    #[cfg(unix)]
    #[test]
    fn a_resident_rebuilt_after_a_retarget_answers_to_the_id_search_serves() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let outside = tempfile::tempdir().unwrap();
        for root in [workspace.join("cf"), outside.path().join("one"), outside.path().join("two")] {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        }
        let link = workspace.join("ext");
        std::os::unix::fs::symlink(outside.path().join("one"), &link).unwrap();
        std::fs::write(
            workspace.join("bsl-analyzer.toml"),
            "[source]\nroot = \"cf\"\nextensions = [{ name = \"live\", path = \"ext\" }]\n",
        )
        .unwrap();
        let extension_id = |roots: &bsl_search::WorkspaceRoots| {
            roots.ids().find(|id| !id.is_empty()).expect("the extension is registered").to_owned()
        };

        let project = crate::project::at(&workspace).unwrap();
        let (served, _) = crate::project::workspace_roots(&project, &[]);
        crate::project::record_served_roots(&served);
        let kept = extension_id(&served);

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(outside.path().join("two"), &link).unwrap();
        let (fresh, _) = bsl_search::WorkspaceRoots::build(
            &workspace,
            &workspace.join("cf"),
            std::slice::from_ref(&link),
        );
        assert_ne!(extension_id(&fresh), kept, "control: a fresh build mints the new target's id");

        let built = DiagnosticsState::build_resident(&workspace, &[]).expect("builds");
        let roots = built.resident.workspace_roots();
        assert_eq!(
            roots.ids().collect::<Vec<_>>(),
            served.ids().collect::<Vec<_>>(),
            "the resident registers the ids search serves",
        );
        assert!(roots.registration_is_current(&kept), "and they are current, not stale");
        assert_eq!(
            roots
                .entries_with_canonical()
                .find(|(id, ..)| *id == kept)
                .map(|(_, _, canonical)| canonical.to_path_buf()),
            Some(std::fs::canonicalize(outside.path().join("two")).unwrap()),
            "resolved to the target the link leads to now",
        );
    }

    /// The schedule itself: no pause for a first attempt, the base for the retry after the
    /// first failure, doubling per failure behind it, and a hard cap — apart from the clock.
    #[test]
    fn the_retry_delay_grows_exponentially_to_a_cap() {
        let base = Duration::from_secs(5);
        let cap = Duration::from_secs(60);

        assert_eq!(retry_delay(base, cap, 0), Duration::ZERO, "a first attempt waits for nothing");
        assert_eq!(retry_delay(base, cap, 1), base);
        assert_eq!(retry_delay(base, cap, 2), Duration::from_secs(10));
        assert_eq!(retry_delay(base, cap, 3), Duration::from_secs(20));
        assert_eq!(retry_delay(base, cap, 4), Duration::from_secs(40));
        assert_eq!(retry_delay(base, cap, 5), cap, "the cap bites one step later");
        assert_eq!(retry_delay(base, cap, u32::MAX), cap, "and stays at the cap");

        // A ceiling far above the default is still reached by doubling, not stopped short of.
        let year = Duration::from_secs(365 * 24 * 60 * 60);
        assert_eq!(
            retry_delay(base, year, 22),
            base * (1 << 21),
            "doubling continues under a long cap"
        );
        assert_eq!(retry_delay(base, year, 30), year, "and ends at the cap the caller set");
        assert_eq!(retry_delay(base, year, u32::MAX), year);
    }

    /// A failure is not terminal: once its backoff has elapsed, the next call starts another
    /// attempt exactly as a first one would, the build walks the tree, and a success clears
    /// everything the failure left behind.
    #[test]
    fn a_failed_build_is_retried_after_its_backoff_and_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.with_retry_pacing_for_test(Duration::from_millis(60), Duration::from_secs(1));
        state.fail_for_test("synthetic first failure");

        let failed = state.status_report();
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.error.as_deref(), Some("synthetic first failure"));
        assert_eq!(failed.attempts, 1, "one failure is behind this state");
        assert!(
            failed.retry_after_ms.is_some(),
            "a failure is retryable, and the caller is told when: {:?}",
            failed.retry_after_ms,
        );
        if cfg!(timing_contract) {
            // Read within the 60 ms base, so a pause is still owed.
            assert!(
                failed.retry_after_ms.is_some_and(|owed| owed > 0),
                "a failure is retryable, and the caller is told when: {:?}",
                failed.retry_after_ms,
            );
        }
        assert!(failed.failed_age_ms.is_some(), "the cause has an age: {:?}", failed.failed_age_ms);
        assert!(failed.elapsed_ms.is_none(), "nothing is building");

        // The backoff runs out by the failure's own clock, moved past it rather than slept out.
        shift_failure_back(&state, Duration::from_millis(80));
        state.ensure_loading();
        wait_ready(&state);

        let ready = state.status_report();
        assert_eq!(ready.state, "ready");
        assert!(ready.generation >= 1, "the retry actually built something");
        assert!(ready.error.is_none(), "a success clears the cause it answered");
        assert_eq!(
            ready.attempts, 0,
            "and the schedule with it: retry_after_ms is {:?}",
            ready.retry_after_ms,
        );
        assert!(ready.failed_age_ms.is_none());
        assert!(ready.retry_after_ms.is_none());
        state.shutdown();
    }

    /// Inside the pause, asking again is refused: no build is started, no attempt is spent,
    /// and the refusal names the wait that is still owed — a poller cannot turn its own
    /// polling into a build storm, and it does not have to guess the schedule.
    #[test]
    fn a_retry_before_its_backoff_starts_no_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.with_retry_pacing_for_test(Duration::from_secs(3600), Duration::from_secs(3600));
        state.fail_for_test("synthetic failure");

        for _ in 0..5 {
            state.ensure_loading();
        }

        let report = state.status_report();
        assert_eq!(report.state, "failed", "the pause has not elapsed");
        assert_eq!(report.attempts, 1, "asking again is not an attempt");
        assert_eq!(report.generation, 0, "nothing was built");
        assert!(report.elapsed_ms.is_none(), "and nothing is building");
        assert!(
            report.retry_after_ms.is_some_and(|owed| owed > 0),
            "the refusal names the remaining wait rather than leaving the caller to guess: {:?}",
            report.retry_after_ms,
        );
        assert!(!state.sweeper_running(), "a refused call starts no sweeper clock either");
    }

    /// What the REPORT says while a retry runs: the cause it answers is still there, the
    /// failures behind it are still counted, and no wait is owed — a build is in flight.
    #[test]
    fn a_running_retry_still_names_the_cause_it_answers() {
        let dir = tempfile::tempdir().unwrap();
        let state = DiagnosticsState::for_workspace(dir.path().to_path_buf());
        state.fail_for_test("synthetic first failure");

        // The retry's window, stated directly: a loader thread would leave it on its own
        // schedule, and the property under test is what the report says WHILE one runs, not
        // how long a build takes.
        {
            let mut inner = lock_recover(&state.inner);
            inner.status = DiagnosticsStatus::Loading;
            inner.loading_since = Some(Instant::now());
        }

        let report = state.status_report();
        assert_eq!(report.state, "loading");
        assert_eq!(
            report.error.as_deref(),
            Some("synthetic first failure"),
            "a retry that erased its cause would be a first attempt to the caller",
        );
        assert_eq!(report.attempts, 1);
        assert!(report.elapsed_ms.is_some(), "the new attempt is what the age measures");
        assert!(report.retry_after_ms.is_none(), "a build in flight owes no wait");
    }

    /// A build that overran its deadline is published as a failure — visibly, with its own
    /// reason and the same retry schedule — instead of pinning every reader on `loading` for
    /// ever. A ready resident and an in-flight reload are not the deadline's business.
    #[test]
    fn a_loading_build_that_overruns_its_deadline_is_published_failed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.with_build_timeout_for_test(Duration::from_millis(1));
        // A pause no run of this test can outlast, so the owed wait below is never zero.
        state.with_retry_pacing_for_test(Duration::from_secs(3600), Duration::from_secs(3600));
        {
            // Started further back than the deadline, by the build's own clock.
            let mut inner = lock_recover(&state.inner);
            inner.status = DiagnosticsStatus::Loading;
            inner.loading_since = Instant::now().checked_sub(Duration::from_millis(8));
            assert!(inner.loading_since.is_some(), "the monotonic clock is older than 8 ms");
        }

        assert!(state.expire_stuck_build(), "an overrun build is published as failed");
        let report = state.status_report();
        assert_eq!(report.state, "failed");
        assert!(
            report.error.as_deref().is_some_and(|msg| msg.contains("did not finish within")),
            "the deadline names itself as the cause: {:?}",
            report.error,
        );
        assert!(report.elapsed_ms.is_none(), "loading_since is cleared, as on any failure");
        assert_eq!(report.attempts, 1);
        assert!(
            report.retry_after_ms.is_some_and(|owed| owed > 0),
            "and the retry schedule applies to it: {:?}",
            report.retry_after_ms,
        );
        assert!(!state.expire_stuck_build(), "the deadline fires once: the state is not loading");

        // Control: a resident that IS ready is never an overrun build, whatever the deadline.
        let ready = DiagnosticsState::for_workspace(root.to_path_buf());
        ready.ensure_loading();
        wait_ready(&ready);
        ready.with_build_timeout_for_test(Duration::from_secs(0));
        lock_recover(&ready.inner).reload = ReloadState::Running;
        assert!(!ready.expire_stuck_build(), "a serving resident is not a build that hung");
        let report = ready.status_report();
        assert_eq!(report.state, "ready", "and the deadline left it serving: {:?}", report.reload);
        assert_eq!(report.reload, "running", "with its reload still in flight");
        state.shutdown();
        ready.shutdown();
    }

    /// The same deadline, driven by the clock it actually runs on: the sweeper's tick is the
    /// tightest of its cadences, so a shortened one makes the wait observable instead of
    /// half a minute long.
    #[test]
    fn the_idle_sweeper_publishes_a_build_that_overran_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.with_build_timeout_for_test(Duration::from_millis(50));
        state.eviction_after = Duration::from_millis(50);
        state.reconcile_interval = Duration::from_millis(50);
        {
            let mut inner = lock_recover(&state.inner);
            inner.status = DiagnosticsStatus::Loading;
            inner.loading_since = Some(Instant::now());
        }

        state.spawn_sweeper_for_test();
        wait_until("the sweeper publishes the overrun build as failed", || match state.status() {
            DiagnosticsStatus::Failed(msg) if msg.contains("did not finish within") => Ok(()),
            other => Err(other),
        });
        state.shutdown();
    }

    /// A retry re-subscribes the hub cursor its failed attempt left behind: the new
    /// subscription is the one the arriving resident will be served under, and the watch is
    /// armed for it — the same ownership a reload re-establishes, on the first-build path.
    ///
    /// What this test states is exactly that and no more: the cursor WAS re-subscribed (a new
    /// id where the failed attempt's had been). Whether the window the old cursor owed was
    /// CARRIED onto it — the half that distinguishes re-subscribing from dropping and taking a
    /// fresh subscription — is pinned by
    /// [`a_retry_carries_the_hubs_window_across_a_failed_attempt_and_settles_it`], which drives
    /// a real debt through a failed retry.
    #[test]
    fn a_retry_after_a_failure_resubscribes_the_hub_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.with_retry_pacing_for_test(Duration::ZERO, Duration::ZERO);
        state.ensure_loading();
        wait_ready(&state);
        let first = lock_recover(&state.hub_cursor).expect("the first build subscribed a cursor");
        assert!(
            hub.is_watching(),
            "the stand's watch must be armed for this test to mean anything"
        );
        assert!(state.hub_armed.load(Ordering::SeqCst), "a build under an armed watch is armed");

        state.fail_for_test("synthetic failure");
        state.ensure_loading();
        wait_ready(&state);

        let second = lock_recover(&state.hub_cursor).expect("the retry holds a cursor too");
        assert_ne!(
            second, first,
            "the retry re-subscribed: keeping the failed attempt's cursor would leave the \
             resident served under a subscription nothing advanced",
        );
        assert!(state.hub_armed.load(Ordering::SeqCst), "and the retry re-armed the watch");
        state.shutdown();
    }

    /// The pacing floors: no environment value may shorten the retry pause below its base or
    /// the build deadline below a minute, and the ceiling never falls under the floor it caps.
    /// The failures these prevent are not hypothetical — a one-second deadline publishes
    /// working builds as failed, and then retries them on a schedule that guarantees the same.
    #[test]
    fn the_pacing_env_is_clamped_to_its_floors() {
        assert_eq!(clamp_retry_base(None), RETRY_BASE, "unset is the default");
        assert_eq!(clamp_retry_base(Some(0)), RETRY_BASE, "zero means unset");
        assert_eq!(clamp_retry_base(Some(1)), MIN_RETRY_BASE, "one second is a build storm");
        assert_eq!(clamp_retry_base(Some(30)), Duration::from_secs(30));

        assert_eq!(clamp_build_timeout(None), BUILD_TIMEOUT);
        assert_eq!(clamp_build_timeout(Some(0)), BUILD_TIMEOUT);
        assert_eq!(clamp_build_timeout(Some(1)), MIN_BUILD_TIMEOUT, "a minute is the floor");
        assert_eq!(clamp_build_timeout(Some(3600)), Duration::from_secs(3600));

        let base = clamp_retry_base(Some(600));
        assert_eq!(base, Duration::from_secs(600));
        assert_eq!(
            clamp_retry_cap(Some(10), base),
            base,
            "a ceiling under the floor is not a ceiling: it must rise to the base it caps",
        );
        assert_eq!(clamp_retry_cap(Some(1200), base), Duration::from_secs(1200));
        assert_eq!(clamp_retry_cap(None, base), RETRY_CAP.max(base));
    }

    /// A clock that cannot hold the deadline is not a reason to retry NOW: the wait saturates
    /// at the ceiling, and the millisecond fields saturate rather than wrapping to something
    /// small enough to read as "no wait owed".
    #[test]
    fn a_deadline_the_clock_cannot_hold_saturates_instead_of_retrying_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let state = DiagnosticsState::for_workspace(dir.path().to_path_buf());
        state.fail_for_test("synthetic failure");
        {
            let mut inner = lock_recover(&state.inner);
            inner.pacing.base = Duration::MAX;
            inner.pacing.cap = Duration::MAX;
        }

        let report = state.status_report();
        assert_eq!(report.state, "failed");
        assert_eq!(
            report.retry_after_ms,
            Some(u64::MAX),
            "the wait must saturate at the ceiling, not read as due",
        );
        state.ensure_loading();
        assert_eq!(
            state.status_report().state,
            "failed",
            "a saturation is still a wait: nothing may be retried before it elapses",
        );
        assert!(!state.sweeper_running(), "and no loader was started");
    }

    /// Move the recorded failure `by` into the past: the time a retry pause measures has passed,
    /// with no wait for it.
    fn shift_failure_back(state: &DiagnosticsState, by: Duration) {
        let mut inner = lock_recover(&state.inner);
        let at = inner.failed_at.expect("a failure is on record");
        inner.failed_at = Some(at.checked_sub(by).expect("the failure is younger than the clock"));
    }

    /// The wait counts DOWN: a poller reading `retry_after_ms` twice sees the remaining pause
    /// shrink, which is the difference between a schedule and a number that never moves.
    #[test]
    fn the_retry_wait_counts_down() {
        let dir = tempfile::tempdir().unwrap();
        let state = DiagnosticsState::for_workspace(dir.path().to_path_buf());
        // An hour-long pause: no run of this test can outlast it, so the wait is still owed
        // at the second reading however slowly the machine gets there.
        state.with_retry_pacing_for_test(Duration::from_secs(3600), Duration::from_secs(3600));
        state.fail_for_test("synthetic failure");

        let first = state.status_report().retry_after_ms.expect("a failure owes a wait");
        shift_failure_back(&state, Duration::from_millis(80));
        let second = state.status_report().retry_after_ms.expect("still owed");
        assert!(second > 0, "the pause is not over within an hour-long base");
        assert!(second < first, "the remaining wait must shrink: {first} ms then {second} ms",);
        assert!(
            first - second >= 40,
            "and shrink by the time that passed, not by a rounding: {first} → {second}",
        );
    }

    /// The build-attempt token, positive half: an attempt the deadline gave up on publishes
    /// NOTHING. Its success would otherwise replace the resident the retry published with one
    /// built from a baseline the retry's own build already superseded.
    #[test]
    fn a_success_from_a_retired_attempt_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.with_retry_pacing_for_test(Duration::ZERO, Duration::ZERO);
        state.with_build_timeout_for_test(Duration::from_millis(1));
        let gate = state.arm_build_gate_for_test();

        state.ensure_loading();
        gate.wait_until_held(1, "the first build reaches the gate");
        std::thread::sleep(Duration::from_millis(10));
        assert!(state.expire_stuck_build(), "the deadline gives up on the held build");

        state.ensure_loading();
        gate.wait_until_held(2, "the retry reaches the gate");
        gate.release();
        wait_until("the retired attempt is discarded", || match state.stale_build_publications() {
            discarded if discarded >= 1 => Ok(()),
            discarded => Err(format!("{discarded} attempts discarded so far")),
        });
        wait_ready(&state);

        assert_eq!(state.stale_build_publications(), 1, "the retired attempt must publish nothing");
        let report = state.status_report();
        assert_eq!(report.state, "ready");
        assert_eq!(report.generation, 1, "only the retry published: the resident is its own");
        assert!(report.error.is_none(), "and the failure that retired the first attempt is gone");
        assert!(report.retry_after_ms.is_none());
        state.shutdown();
    }

    /// The same token, the direction that costs a live resident: a retired attempt's FAILURE
    /// must not be published over the resident its replacement already built — every reader
    /// would refuse, eviction would stop and the hub cursor would never be released.
    #[test]
    fn a_failure_from_a_retired_attempt_cannot_take_the_resident_down() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // The first attempt's build fails on disk; the disk is repaired before the retry.
        std::fs::write(root.join("bsl-analyzer.toml"), "not = = toml").unwrap();

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.with_retry_pacing_for_test(Duration::ZERO, Duration::ZERO);
        state.with_build_timeout_for_test(Duration::from_millis(1));
        let gate = state.arm_build_gate_for_test();

        state.ensure_loading();
        gate.wait_until_held(1, "the failing build reaches the gate");
        std::thread::sleep(Duration::from_millis(10));
        assert!(state.expire_stuck_build(), "the deadline gives up on the held build");

        std::fs::remove_file(root.join("bsl-analyzer.toml")).unwrap();
        state.ensure_loading();
        gate.wait_until_held(2, "the retry reaches the gate");
        gate.release();
        wait_until("the retired attempt is discarded", || match state.stale_build_publications() {
            discarded if discarded >= 1 => Ok(()),
            discarded => Err(format!("{discarded} attempts discarded so far")),
        });
        wait_ready(&state);

        assert_eq!(state.stale_build_publications(), 1, "the retired failure must be discarded");
        let report = state.status_report();
        assert_eq!(report.state, "ready");
        assert!(report.error.is_none(), "a stale failure left its reason behind: {report:?}");
        assert_eq!(report.attempts, 0);
        assert!(
            matches!(state.read(|_, _| ()), ResidentOutcome::Ready(..)),
            "the resident the retry published must still answer",
        );
        state.shutdown();
    }

    /// The window a degraded hub opened is a debt SOMEONE owes, and a retry has to pick it up    /// rather than leave it unowned: dropping the cursor before the retry closes the window
    /// silently (nobody owes it any more) and the reconcile is never paid. This test states
    /// both halves — the debt survives a retry that FAILS, and is settled by one that succeeds.
    #[test]
    fn a_retry_carries_the_hubs_window_across_a_failed_attempt_and_settles_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.with_retry_pacing_for_test(Duration::ZERO, Duration::ZERO);
        state.ensure_loading();
        wait_ready(&state);

        let cursor_of = || lock_recover(&state.hub_cursor).expect("the state holds a hub cursor");
        hub.degrade_external();
        let first = cursor_of();
        assert!(
            matches!(hub.health_for(Some(first)), Health::Degraded(_)),
            "the stand needs a reconcile debt on the state's own cursor",
        );

        // A retry whose build fails: the rebuild did NOT read the tree, so the debt stands.
        std::fs::write(root.join("bsl-analyzer.toml"), "not = = toml").unwrap();
        state.fail_for_test("synthetic failure");
        state.ensure_loading();
        wait_until("the failed retry settles back into Failed", || match state.status() {
            DiagnosticsStatus::Failed(_) => Ok(()),
            other => Err(other),
        });
        let second = cursor_of();
        assert_ne!(second, first, "the retry re-subscribed the cursor");
        assert!(
            matches!(hub.health_for(Some(second)), Health::Degraded(_)),
            "a retry that FAILED must not settle the window: the debt is carried onto its own \
             cursor, and dropping that cursor instead would close the window unowned",
        );

        // And the retry that succeeds answers it.
        std::fs::remove_file(root.join("bsl-analyzer.toml")).unwrap();
        state.fail_for_test("synthetic failure again");
        state.ensure_loading();
        wait_ready(&state);
        let third = cursor_of();
        assert!(
            matches!(hub.health_for(Some(third)), Health::Healthy),
            "the successful rebuild's baseline scan settled the window it carried",
        );
        state.shutdown();
    }

    /// An attempt retired BEFORE it starts touches nothing — the cursor slot included. This is
    /// the case the token exists for on the way IN: the deadline gives up on a build still in
    /// its startup, and the retry that replaces it takes a cursor of its own; a re-subscribe
    /// from the retired thread would drop the subscription the live attempt is building under,
    /// leaving the resident it publishes to be served under a window nobody opened for it.
    ///
    /// The retirement happens from the loader's own start seam, which is the one moment where
    /// "retired before it did anything" is a fact rather than a race.
    #[test]
    fn an_attempt_retired_before_it_starts_does_not_touch_the_hub_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.with_retry_pacing_for_test(Duration::from_secs(3600), Duration::from_secs(3600));
        // The deadline is already over when the loader starts, so the seam below is a real
        // deadline verdict on a build that never got going.
        state.with_build_timeout_for_test(Duration::ZERO);
        assert!(lock_recover(&state.hub_cursor).is_none(), "the stand starts with no cursor");

        let retire = state.clone();
        state.set_load_start_probe(move || {
            assert!(retire.expire_stuck_build(), "the deadline must retire the starting attempt");
        });

        state.ensure_loading();
        wait_until("the retired attempt gives up", || match state.stale_build_publications() {
            discarded if discarded >= 1 => Ok(()),
            discarded => Err(format!("{discarded} attempts discarded so far")),
        });

        assert!(
            lock_recover(&state.hub_cursor).is_none(),
            "the retired attempt subscribed a cursor: nobody will ever drain it, eviction only \
             runs from Ready, and the live attempt's subscription is gone",
        );
        assert_eq!(
            state.status_report().state,
            "failed",
            "the deadline's verdict is what stands: the retired attempt published nothing",
        );
        assert!(hub.is_watching(), "the stand's watch was armed all along");
        state.shutdown();
    }
}
