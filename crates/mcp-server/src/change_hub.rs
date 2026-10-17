//! Daemon-owned filesystem change hub.
//!
//! One [`notify`] watcher covers the workspace root recursively and folds raw
//! events into a bounded, typed accumulator keyed by canonical path. Consumers
//! (search today; diagnostics and graph later) each register a cursor and pull
//! the drift they have not yet seen, so a slow or restarting sink never
//! blast-radiuses the others.
//!
//! Reclamation is driven by the cursors: an entry is dropped once every live
//! cursor has advanced past it, so the capacity bounds only *undrained* in-flight
//! paths rather than growing for the daemon's whole lifetime.
//!
//! Two different things can force a reconcile, and they are kept apart because
//! they are owed by different people. If the event stream is lossy — the backend
//! dropped events before the hub saw them — nobody has the detail, so every live
//! cursor is told, exactly once, to reconcile with a full scan. If instead the cap
//! is reached, the accumulator is being held at whichever cursor stopped draining:
//! the detail is released by advancing THAT cursor, and only it is told. A
//! consumer keeping up therefore pays nothing for one that stopped — it keeps its
//! exact paths — and asks about health through its own cursor rather than through
//! a shared verdict somebody else's silence would spoil.

use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use project_model::{ExcludedPaths, PathScope, Spellings};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;

/// Default capacity, counted in *undrained* in-flight distinct paths. Beyond this
/// the hub drops detail and asks consumers to reconcile with a full scan — the
/// same cost they would pay on a cold start, so correctness is preserved.
const DEFAULT_CAPACITY: usize = 8192;

/// Bound on the notify-callback → hub-thread channel. `try_send` past this never
/// blocks the notify callback; the overflow is folded into the same rescan path
/// as an accumulator overflow, so a storm degrades gracefully instead of spiking
/// memory.
const CHANNEL_BOUND: usize = 65536;

/// Ceiling on a whole stop: enqueueing the message and waiting for the thread to act on
/// it share it, so no caller of [`HubThread::stop`] — including a `Drop` — can be held
/// longer than this whatever the thread is doing.
const STOP_BUDGET: Duration = Duration::from_secs(5);

/// How often a stop re-checks the two things it waits on (channel space, thread exit).
const STOP_POLL: Duration = Duration::from_millis(10);

/// The next topology epoch: a monotonic label for the age of one composition of scan roots.
///
/// Assigned where the composition is TAKEN — a [`crate::graph::input::ProjectSnapshot`] — and
/// carried with every declaration that speaks for it, so the hub can tell which of two
/// declarations describes the newer world when their arrival order is the reverse of their
/// age. The clock is the process's own: numbers are only ever compared within one hub's
/// lifetime (github#184).
pub(crate) fn next_topology_epoch() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// What is known to have happened to a path within a drain window. The kind is
/// re-derived from on-disk state at event time (stats are truth), so a
/// create-then-delete or delete-then-create burst settles on the final reality
/// rather than misclassifying on the first event seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    /// The path exists on disk; its content may have changed.
    MaybeChanged,
    /// A file path is gone; consumers should tombstone it.
    MaybeRemoved,
    /// An extension-less path under the root vanished — most likely a removed
    /// directory whose descendants must be expanded by the consumer.
    SubtreeRemoved,
}

/// Why the hub is asking consumers to reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DegradeReason {
    /// The watcher could not be created or could not watch the root (permanent).
    WatcherSetup,
    /// The watcher delivered a runtime error through its callback.
    RuntimeError,
    /// An event kind outside Create/Modify/Remove/Access arrived; rather than
    /// silently dropping it, the hub assumes it may have missed real drift.
    UnknownEvent,
    /// Extending the recursive watch to a newly-created subtree failed, so that
    /// subtree may be blind to further changes until a reconcile re-covers it.
    RewatchFailed,
    /// A consumer's periodic reconcile scan found drift the event stream never
    /// delivered — evidence the backend is lossy, so fall back to scanning.
    ReconcileMiss,
    /// The callback channel overflowed: the backend dropped events before anyone saw
    /// them, so every consumer alike lost detail and must reconcile.
    Overflow,
    /// THIS consumer fell so far behind that the detail it had not drained was released
    /// to keep the accumulator bounded. Nobody else lost anything — which is why it is a
    /// separate reason from [`DegradeReason::Overflow`], and why it is carried by the one
    /// cursor rather than by the shared reconcile window.
    CursorLagged,
    /// The watch was re-pointed: a new declared root set (an extension topology reload),
    /// or a stream restarted to reach a subtree an event revealed. Either way state a
    /// consumer derived beforehand predates the new coverage — and on a backend that
    /// rebuilds its stream from "now", the swap itself dropped whatever happened during
    /// it — so each must rescan once before trusting the stream again.
    Rearmed,
}

/// Run on the hub thread immediately before the watch is armed. `None` in production.
type BeforeArm = Arc<dyn Fn() + Send + Sync>;

/// The seams a test puts between the hub and its backend. `None` in production, where only
/// the backend decides and nothing needs the record.
///
/// Both ends of a watch in one object, because a test that sees only the arms cannot tell a
/// registration that was dropped from one that was never placed — and `notify` reports
/// neither.
struct WatchSeams {
    /// Consulted before every arm: a path it answers `true` for refuses to arm.
    refuses: Box<dyn Fn(&Path) -> bool + Send + Sync>,
    /// Told about every unwatch, and never consulted — a refusal makes a watch fail, and
    /// there is no such thing as a registration that refuses to go.
    disarmed: Box<dyn Fn(&Path) + Send + Sync>,
}

type WatchRefusal = Arc<WatchSeams>;

/// A seam that lets no watch arm: the hub's setup fails and it polls instead.
#[cfg(test)]
fn refuse_every_watch() -> WatchRefusal {
    Arc::new(WatchSeams { refuses: Box::new(|_| true), disarmed: Box::new(|_| {}) })
}

#[cfg(test)]
thread_local! {
    /// Set by a test that boots a whole backend and needs its hub to poll: the hub is
    /// created on the calling thread, deep inside the boot, where no parameter reaches.
    pub(crate) static POLL_INSTEAD_OF_WATCHING: std::cell::Cell<Option<PollConfig>> =
        const { std::cell::Cell::new(None) };
}

/// Holds a hub's thread short of arming until released.
#[cfg(test)]
pub(crate) struct HubHold {
    held: Mutex<bool>,
    released: Condvar,
}

#[cfg(test)]
impl HubHold {
    fn new() -> Self {
        Self { held: Mutex::new(true), released: Condvar::new() }
    }

    fn wait(&self) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        while *held {
            held = self.released.wait(held).unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub(crate) fn release(&self) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = false;
        self.released.notify_all();
    }
}

/// The watch the hub holds, and the only way this module places one.
///
/// A root that cannot be watched is the condition half of this file exists to survive — an
/// exhausted inotify limit, a denied permission, a path that is not a directory — and it
/// cannot be built from a test out of the file system alone. Permission bits are the
/// tempting way and the wrong one: they mean nothing to a process with CAP_DAC_READ_SEARCH,
/// so the same `chmod` that blinds a hub on a developer's machine leaves it fully sighted
/// in a root container, and every assertion about blindness there passes over a hub that
/// is not blind. So the refusal is DECLARED instead, and declared refusal and backend
/// refusal take the same branch at every call site because there is only one call site
/// each: [`Self::arm`].
struct Watch {
    backend: RecommendedWatcher,
    seams: Option<WatchRefusal>,
    /// The user's `[source].exclude`, shared with [`HubInner`] so a rescope reaches the
    /// backend door too.
    user_excluded: Arc<RwLock<ExcludedPaths>>,
    /// Directories armed non-recursively because an exclusion lies below them, each with
    /// the children armed in its place.
    narrowed: HashMap<PathBuf, Vec<PathBuf>>,
}

impl Watch {
    fn new(backend: RecommendedWatcher, seams: Option<WatchRefusal>, inner: &HubInner) -> Self {
        Self {
            backend,
            seams,
            user_excluded: Arc::clone(&inner.user_excluded),
            narrowed: HashMap::new(),
        }
    }

    /// Place a registration, never one that reaches into a user exclusion.
    ///
    /// Everything above decides coverage as if a recursive watch covered its whole tree;
    /// this is the one door to the backend, so the exclusions are honoured here: an
    /// excluded directory is not armed at all, and a recursive watch that would reach an
    /// exclusion — existing or yet to be created — is placed non-recursively instead,
    /// with each permitted child directory armed in turn. Only the branches leading to an
    /// exclusion are listed; the excluded directory itself never is.
    ///
    /// Every child armed in a narrowed registration passes the same refusal check and
    /// backend as the path itself, and a child that cannot be armed — or a directory
    /// that cannot be listed — fails the whole registration: a narrowed watch with a
    /// branch missing would claim coverage of a subtree nothing watches, so it is
    /// reported the way any failed arm is, and retried from there.
    fn arm(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        let excluded = self.user_excluded.read().unwrap_or_else(PoisonError::into_inner).clone();
        self.arm_within(path, mode, &excluded, &mut Vec::new())
    }

    /// `visited` holds the resolved directories a narrowed descent has entered, so a
    /// symlink leading back into one of them — a loop — is not descended again.
    fn arm_within(
        &mut self,
        path: &Path,
        mode: RecursiveMode,
        excluded: &ExcludedPaths,
        visited: &mut Vec<PathBuf>,
    ) -> notify::Result<()> {
        if self.seams.as_ref().is_some_and(|seams| (seams.refuses)(path)) {
            return Err(notify::Error::generic("the watch of this path is refused"));
        }
        if excluded.is_excluded_resolved(path) {
            return Ok(());
        }
        if mode == RecursiveMode::Recursive && excluded.has_exclusion_below_resolved(path) {
            let resolved = resolve_as_far_as_it_goes(path);
            if visited.contains(&resolved) {
                return Ok(());
            }
            visited.push(resolved);
            self.backend.watch(path, RecursiveMode::NonRecursive)?;
            let mut children = Vec::new();
            let mut failure = None;
            match std::fs::read_dir(path) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let child = entry.path();
                        if !std::fs::metadata(&child).is_ok_and(|meta| meta.is_dir())
                            || excluded.is_excluded_resolved(&child)
                        {
                            continue;
                        }
                        match self.arm_within(&child, RecursiveMode::Recursive, excluded, visited) {
                            Ok(()) => children.push(child),
                            Err(error) => {
                                tracing::warn!(dir = ?child, "workspace change hub could not watch a directory beside an exclusion: {error}");
                                failure.get_or_insert(error);
                            }
                        }
                    }
                }
                Err(error) => failure = Some(notify::Error::io(error).add_path(path.to_path_buf())),
            }
            if let Some(error) = failure {
                // All or nothing: the caller records no registration for a failed arm,
                // so nothing placed here may outlive it unnamed.
                let previous = self.narrowed.remove(path).unwrap_or_default();
                for child in children.into_iter().chain(previous) {
                    let _ = self.disarm(&child);
                }
                let _ = self.backend.unwatch(path);
                return Err(error);
            }
            if let Some(previous) = self.narrowed.insert(path.to_path_buf(), children.clone()) {
                for child in previous.into_iter().filter(|child| !children.contains(child)) {
                    let _ = self.disarm(&child);
                }
            }
            return Ok(());
        }
        if let Some(previous) = self.narrowed.remove(path) {
            for child in previous {
                let _ = self.disarm(&child);
            }
        }
        self.backend.watch(path, mode)
    }

    /// Whether `dir` lies directly in a directory armed non-recursively because of an
    /// exclusion below it — where no recursive watch covers a newcomer, whatever the
    /// records above say.
    ///
    /// Asked under the resolved spelling too, by the same rule the coverage check it
    /// corrects uses: a backend may report a newcomer under another spelling than the
    /// one its parent was armed with (a root declared through a link, `/tmp` reported
    /// as `/private/tmp`).
    fn is_in_narrowed(&self, dir: &Path) -> bool {
        let Some(parent) = dir.parent() else { return false };
        if self.narrowed.contains_key(parent) {
            return true;
        }
        if self.narrowed.is_empty() {
            return false;
        }
        let resolved = resolve_as_far_as_it_goes(parent);
        self.narrowed.keys().any(|armed| resolve_as_far_as_it_goes(armed) == resolved)
    }

    /// Drop a registration. Announced to the seam but never gated by it: a refusal makes a
    /// watch fail, and un-watching what was never armed is the backend's own no-op to
    /// report. A narrowed registration takes the children armed in its place with it.
    fn disarm(&mut self, path: &Path) -> notify::Result<()> {
        if let Some(seams) = self.seams.as_ref() {
            (seams.disarmed)(path);
        }
        if let Some(children) = self.narrowed.remove(path) {
            for child in children {
                let _ = self.disarm(&child);
            }
        }
        self.backend.unwatch(path)
    }
}

/// One end of a watch the hub asked for.
#[cfg(all(test, unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchCallKind {
    Arm,
    Disarm,
}

/// The paths a test has declared unwatchable, and the switch that clears them.
///
/// Compared by canonical path with a fallback to the raw one: the hub arms whichever
/// spelling it was declared with, and a refusal keyed by a different spelling of the same
/// directory would silently never fire — a seam that cannot refuse is worse than none,
/// because the tests built on it go green over the behaviour they meant to pin.
#[cfg(all(test, unix))]
#[derive(Default)]
pub(crate) struct RefusedWatches {
    paths: Mutex<Vec<PathBuf>>,
    /// Everything the hub told the watcher to do, in order — both ends of every watch.
    /// This is the only handle a test has on that, since `notify` reports no such thing,
    /// and the ORDER is part of it: a re-arm that unwatches after it has re-armed what it
    /// keeps strips on inotify exactly what it had just restored.
    asked: Mutex<Vec<(WatchCallKind, PathBuf)>>,
}

#[cfg(all(test, unix))]
impl RefusedWatches {
    /// Declared before the hub starts, for a root that must never arm in the first place.
    pub(crate) fn refusing(paths: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            paths: Mutex::new(paths.iter().map(|p| Self::key(p)).collect()),
            asked: Mutex::default(),
        })
    }

    pub(crate) fn none() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Refusing a path AFTER the hub is running is only observable where something arms a
    /// path it is already holding, and on FSEvents nothing does — the defensive pass is
    /// not run there (see [`a_kept_target_must_be_re_armed`]), and every test that flips a
    /// refusal mid-flight is gated off that platform for the same reason. The seam itself
    /// stays whole on every platform: half a seam is how a test ends up green over the
    /// behaviour it meant to pin.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn refuse(&self, path: &Path) {
        let key = Self::key(path);
        let mut paths = self.paths.lock().unwrap_or_else(PoisonError::into_inner);
        if !paths.contains(&key) {
            paths.push(key);
        }
    }

    pub(crate) fn allow(&self, path: &Path) {
        let key = Self::key(path);
        self.paths.lock().unwrap_or_else(PoisonError::into_inner).retain(|p| *p != key);
    }

    /// Record one end of a watch, and give back the key it was recorded under.
    fn note(&self, kind: WatchCallKind, path: &Path) -> PathBuf {
        let key = Self::key(path);
        self.asked.lock().unwrap_or_else(PoisonError::into_inner).push((kind, key.clone()));
        key
    }

    /// How many times the hub has asked to arm `path` since the last [`Self::forget_asks`].
    ///
    /// Two kinds of stand ask it. On FSEvents an arm COSTS something — it rebuilds the
    /// whole stream — so the count itself is the measurement, and those stands are gated to
    /// that platform. Everywhere else it is a barrier: "has the hub asked yet" is the only
    /// signal a test has that an event has been carried all the way to the watcher.
    pub(crate) fn arms_of(&self, path: &Path) -> usize {
        let key = Self::key(path);
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(kind, p)| *kind == WatchCallKind::Arm && *p == key)
            .count()
    }

    /// Draw a line under everything asked so far, so a stand can measure one step.
    pub(crate) fn forget_asks(&self) {
        self.asked.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// Everything the watcher was told, in the order it was told. Unlike the counters this
    /// is asked on every platform: the ordering it exposes is the one an inotify unwatch
    /// makes load-bearing.
    pub(crate) fn calls(&self) -> Vec<(WatchCallKind, PathBuf)> {
        self.asked.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Canonical where the path can be resolved, raw where it cannot — a refused root may
    /// be one nothing can describe, and dropping it from the set on that account would
    /// hand the test the arming it declared away.
    fn key(path: &Path) -> PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }

    /// The form the hub thread consults. Holds a clone, so a test can flip a refusal after
    /// the hub is running and the next arming pass sees it.
    fn as_refusal(self: &Arc<Self>) -> WatchRefusal {
        let refuses = Arc::clone(self);
        let disarmed = Arc::clone(self);
        Arc::new(WatchSeams {
            refuses: Box::new(move |path: &Path| {
                let key = refuses.note(WatchCallKind::Arm, path);
                refuses.paths.lock().unwrap_or_else(PoisonError::into_inner).contains(&key)
            }),
            disarmed: Box::new(move |path: &Path| {
                disarmed.note(WatchCallKind::Disarm, path);
            }),
        })
    }
}

/// The caller's end of a [`HubHold`]: releases the hold when it goes, so a hub still
/// parked when its handles are dropped goes on to arm and can then read the stop message.
///
/// The hold cannot do this itself. The parked thread's own closure owns a clone of it, so
/// a `Drop` on the shared hold would only run once that thread ended — which is precisely
/// what the hold is preventing. Separating the caller's end from the shared one is what
/// makes the release reachable at all.
#[cfg(test)]
pub(crate) struct HubHoldGuard(Arc<HubHold>);

#[cfg(test)]
impl HubHoldGuard {
    pub(crate) fn release(&self) {
        self.0.release();
    }

    /// A releaser another thread can own, for a test that releases on a schedule.
    pub(crate) fn shared(&self) -> Arc<HubHold> {
        Arc::clone(&self.0)
    }
}

#[cfg(test)]
impl Drop for HubHoldGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// What a consumer waiting for the watch has learnt when its wait returned.
///
/// Three answers, because two collapse the only distinction that matters to a caller
/// deciding whether to wait again: a hub that will never arm and a hub that has not armed
/// YET both read as "not armed", and a consumer that treats the second as the first gives
/// up on a workspace whose initial walk simply outlasted one slice of patience.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchReadiness {
    Armed,
    /// Permanent: the hub reported setup failure and will not arm.
    Failed,
    /// The wait expired with setup still in flight. Asking again is meaningful.
    NotYet,
}

/// Observable health of the hub. `WatcherSetup` is permanent; every other
/// degradation is transient and clears back to `Healthy` once all live cursors
/// have acknowledged the reconcile request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Health {
    Healthy,
    Degraded(DegradeReason),
}

impl Health {
    /// A stable label for status reporting.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::Degraded(DegradeReason::WatcherSetup) => "degraded:watcher-setup",
            Health::Degraded(DegradeReason::RuntimeError) => "degraded:runtime-error",
            Health::Degraded(DegradeReason::UnknownEvent) => "degraded:unknown-event",
            Health::Degraded(DegradeReason::RewatchFailed) => "degraded:rewatch-failed",
            Health::Degraded(DegradeReason::ReconcileMiss) => "degraded:reconcile-miss",
            Health::Degraded(DegradeReason::Overflow) => "degraded:overflow",
            Health::Degraded(DegradeReason::CursorLagged) => "degraded:cursor-lagged",
            Health::Degraded(DegradeReason::Rearmed) => "degraded:rearmed",
        }
    }
}

/// One accumulated change. Carries both the canonical key (matching the scan
/// universe used by drift detection) and the raw path as the watcher reported
/// it — consumers that strip a non-canonical root (search strips the configured
/// source root) need the raw spelling, or a symlinked root would fail to match.
///
/// `canonical` and `kind` are the drift-consumption contract: the shared
/// classifier re-stats `canonical` (stats are truth) and branches on `kind` for a
/// subtree removal. `raw` is the watcher spelling search strips its source root
/// against.
#[derive(Debug, Clone)]
pub(crate) struct ChangeEntry {
    pub(crate) canonical: PathBuf,
    pub(crate) raw: PathBuf,
    pub(crate) kind: ChangeKind,
    pub(crate) seq: u64,
}

/// A consumer's handle into the change stream. Opaque; the cursor's position and
/// pending-rescan flag live inside the hub, keyed by this id, so cursors are
/// independent and reclamation can track the slowest one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SinkCursor {
    id: u64,
}

/// The debt [`WorkspaceChangeHub::resubscribe`] carried from one cursor onto a fresh one, named
/// by the PAIR that must still hold for a later successful rebuild to settle it: the loss the
/// cursor was issued, and the POSITION the fresh cursor was put at when the carry happened.
///
/// The position is the half a loss alone cannot speak for. `make_room` advances a cursor's
/// position to cut it out of entries it never read, and it does so WITHOUT renaming a debt the
/// cursor already owes — one reconcile covers the cut too. A settle that answered to the loss
/// alone would then pay a rebuild whose baseline scan never contained the cut entries, and they
/// would vanish with no debt left on the cursor: any movement of the position while a debt is
/// live is the signature of a cut, and such a debt is not the settle's to close.
///
/// That refusal is conservative, and deliberately so: it also covers a cut that happened
/// BEFORE the baseline scan, whose facts that scan nevertheless read. Such a debt is simply
/// not settled here and is cleared by the next guard pass — the price being a possible extra
/// full scan on the reads until then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CarriedDebt {
    loss: u64,
    pos: u64,
}

/// A subscription whose release does not depend on remembering to release it.
///
/// A cursor is subscribed before the consumer that will read it exists, and the code
/// between the two has more ways out than one: an early return, a failed thread spawn
/// whose closure is simply dropped, a panic. Enumerating them is how one gets missed, and
/// a cursor nobody drains holds entries back for the life of the process. So the exits are
/// not enumerated: the lease releases on drop, and only handing the cursor to a consumer
/// that is actually running takes that duty away from it.
pub(crate) struct CursorLease {
    hub: WorkspaceChangeHub,
    cursor: Option<SinkCursor>,
}

impl CursorLease {
    pub(crate) fn new(hub: WorkspaceChangeHub) -> Self {
        let cursor = Some(hub.subscribe());
        Self { hub, cursor }
    }

    /// The cursor to hand to a consumer. `None` once the lease has been handed over.
    pub(crate) fn cursor(&self) -> Option<SinkCursor> {
        self.cursor
    }

    /// Give up the duty to release: the consumer is running and owns the cursor now.
    /// Called only AFTER the consumer exists — disarming on the attempt would put the
    /// leak back under the name of a fix.
    pub(crate) fn handed_over(&mut self) {
        self.cursor = None;
    }
}

impl Drop for CursorLease {
    fn drop(&mut self) {
        if let Some(cursor) = self.cursor.take() {
            self.hub.unsubscribe(cursor);
        }
    }
}

/// The result of draining a cursor: the entries newer than the cursor's last
/// position, the cursor to reuse, and whether this cursor must reconcile with a
/// full scan (delivered exactly once per overflow).
#[derive(Debug, Clone)]
pub(crate) struct DrainBatch {
    pub(crate) entries: Vec<ChangeEntry>,
    pub(crate) cursor: SinkCursor,
    pub(crate) rescan_required: bool,
    /// The identity of the loss this batch reports.
    ///
    /// A LOSS has no number of its own on the fact stream: a reconcile says the detail is
    /// gone, and a hub whose sequence has not moved can still have lost something new. So one
    /// is issued here — the SAME one to every cursor a shared window flagged, so two cursors
    /// that lost the same delivery report one event, and one of its own to a cursor cut out of
    /// entries nobody else lost. Re-materialising a batch does not move it.
    losses: u64,
    through_seq: u64,
    start_pos: u64,
    generation: u64,
}

impl DrainBatch {
    /// The number of the newest fact this batch covers: its latest entry, or — for a rescan,
    /// whose walk reads disk as it stands when the batch is taken — the hub's position then.
    /// The identity of the loss this batch reports, when it reports one.
    pub(crate) fn loss_token(&self) -> Option<u64> {
        self.rescan_required.then_some(self.losses)
    }

    pub(crate) fn fact_seq(&self) -> u64 {
        self.through_seq
    }
}

/// Per-cursor state held by the accumulator.
struct CursorState {
    /// The last sequence number this cursor has drained through.
    pos: u64,
    /// This cursor's own outstanding reconcile debt, WITH the reason it was incurred;
    /// delivered once on the next drain, then cleared. Carrying the reason here rather
    /// than only in the shared window is what lets a debt exist for one consumer alone —
    /// a debt nobody can name is a debt no health can report.
    pending: Option<DegradeReason>,
    /// The identity of the loss `pending` is about, issued when the debt was incurred. Held
    /// per cursor because that is where the two kinds differ: one shared window gives every
    /// cursor the same number, and a cursor cut out of its own entries gets one nobody else
    /// carries.
    loss: Option<u64>,
    /// The loss of the reconcile batch this cursor's consumer has taken and not acknowledged.
    ///
    /// A consumer records a loss between taking its batch and acknowledging it, and a newer
    /// window renames `loss` in that interval. Without this, a loss still in a consumer's hands
    /// would read as one no cursor can deliver any more.
    delivered: Option<u64>,
}

/// What the hub can still deliver of the losses it has issued.
///
/// A loss reaches a consumer only from a cursor that holds it — as the debt it owes, or as the
/// batch its consumer took and has not acknowledged — and a newcomer inherits only the window
/// that is open. So a loss issued by `issued` and absent from `live` can never reach a consumer
/// again. Which losses are live cannot be read off their numbers: a cursor still inside an old
/// window carries it past any number of newer losses issued to others.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LossHorizon {
    pub(crate) issued: u64,
    pub(crate) live: Vec<u64>,
}

impl LossHorizon {
    /// Whether `token` may still be delivered to a consumer.
    pub(crate) fn may_deliver(&self, token: u64) -> bool {
        token > self.issued || self.live.contains(&token)
    }
}

/// Bounded, seq-tagged accumulator. Entries coalesce by canonical path so the map
/// is bounded by the number of distinct *undrained* dirty paths, not the event
/// rate or the daemon's lifetime.
struct Accumulator {
    entries: HashMap<PathBuf, ChangeEntry>,
    cursors: HashMap<u64, CursorState>,
    next_cursor_id: u64,
    cap: usize,
    /// Next sequence number to assign. Monotonic across the hub's lifetime; a
    /// path's seq moves forward on every update so a lagging cursor still sees
    /// its latest state.
    next_seq: u64,
    /// Bumped when there is new work for sinks to drain (a recorded change, an
    /// entered-rescan transition, or setup completion), so sink threads sleep on
    /// the condvar and wake only when needed — never per dropped overflow event.
    generation: u64,
    /// Monotonic count of raw watcher events observed, for observability.
    events_seen: u64,
    /// The active reconcile reason, or `None` when healthy. Set when the hub
    /// enters a rescan; cleared once every live cursor has acknowledged.
    degrade_reason: Option<DegradeReason>,
    /// Set once if the watcher could not be set up. Permanent for the hub's life.
    setup_failed: bool,
    /// Every reconcile REQUEST, not every reconcile a consumer sees. `enter_rescan`
    /// collapses repeats inside an open window, and `drain` closes that window, so
    /// external state cannot tell one request per tick from one per target — while
    /// the cost is real: a consumer answers each with a full tree walk.
    rescans_requested: u64,
    /// Losses issued: reconcile windows raised, and cursors cut out of entries they had not
    /// drained. Distinct from `rescans_requested`, which counts what was ASKED for — a lag is
    /// nobody's request and still costs the cursor it hit a full reconcile.
    losses_issued: u64,
    /// The identity of the shared window that is open, as its cursors were handed it. A
    /// newcomer that inherits the window inherits THIS, never the counter: the counter also
    /// moves for losses of single cursors, and one loss under two names is two events to a
    /// consumer.
    window_loss: Option<u64>,
    /// The daemon is going away: every wait returns at once instead of sleeping out its
    /// timeout, so an owner parked here learns of the stop immediately.
    closing: bool,
}

impl Accumulator {
    fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            cursors: HashMap::new(),
            next_cursor_id: 1,
            cap,
            next_seq: 1,
            generation: 0,
            events_seen: 0,
            degrade_reason: None,
            setup_failed: false,
            rescans_requested: 0,
            losses_issued: 0,
            window_loss: None,
            closing: false,
        }
    }

    fn max_seq(&self) -> u64 {
        self.next_seq - 1
    }

    fn subscribe(&mut self, force: Option<DegradeReason>) -> u64 {
        let id = self.next_cursor_id;
        self.next_cursor_id += 1;
        // A cursor born during an active rescan window must still be told to
        // reconcile; one born while healthy starts clean. `force` carries the standing
        // reasons the window does not — a declared root nothing is watching outlives the
        // window that announced it.
        //
        // Only the SHARED window is inherited. A debt belonging to one lagging cursor is
        // not a loss of the stream: nothing was dropped before this cursor existed that
        // anyone else still holds, so charging it a full reconcile would be charging it
        // for somebody else's silence.
        //
        // And only while somebody is still INSIDE it. A window raised over an empty cursor
        // set belongs to nobody: there was no consumer to lose anything, and nobody who
        // could acknowledge it away, so handing it to whoever arrives next would charge a
        // full reconcile for a window that ended before they existed. The reason itself
        // stands — `health` reports the hub's condition to a status caller whether or not
        // anyone is there to be owed.
        let owed = self.cursors.values().any(|cursor| {
            matches!(&cursor.pending, Some(reason) if *reason != DegradeReason::CursorLagged)
        });
        let forced = force.is_some();
        let pending = force.or_else(|| owed.then(|| self.degrade_reason.clone()).flatten());
        // A debt inherited from the window that is still open is that window's loss, and a
        // debt forced on a newcomer is one of its own.
        let inherited = (owed && !forced).then_some(self.window_loss).flatten();
        let loss = pending.as_ref().map(|_| {
            inherited.unwrap_or_else(|| {
                self.losses_issued += 1;
                self.losses_issued
            })
        });
        self.cursors
            .insert(id, CursorState { pos: self.max_seq(), pending, loss, delivered: None });
        id
    }

    fn unsubscribe(&mut self, id: u64) {
        self.cursors.remove(&id);
        self.close_window_if_settled();
        self.reclaim();
    }

    /// What this cursor still owes, if anything, and the identity of that loss.
    fn debt_of(&self, id: u64) -> Option<(DegradeReason, Option<u64>)> {
        self.cursors
            .get(&id)
            .and_then(|cursor| cursor.pending.clone().map(|reason| (reason, cursor.loss)))
    }

    /// Put a debt carried over from another cursor on `id`, under the identity it was issued
    /// with: the loss did not happen again because the consumer changed cursors. A batch the
    /// consumer took from the old cursor is still in its hands, and goes along too.
    fn carry_debt(&mut self, id: u64, loss: Option<u64>, delivered: Option<u64>) {
        let Some(cursor) = self.cursors.get_mut(&id) else { return };
        if let Some(loss) = loss {
            cursor.loss = Some(loss);
        }
        cursor.delivered = delivered;
    }

    /// Settle the debt carried onto `id` by [`Self::carry_debt`], provided the cursor still
    /// holds EXACTLY that debt — the pair the carry captured. Says whether it did.
    ///
    /// A rebuild that took a fresh cursor and then succeeded has answered the reconcile the
    /// carried debt was for: its baseline scan read the tree. Settling it is what keeps the
    /// following reads on the event stream rather than paying a full walk, window after
    /// window, for a loss the build already covered.
    ///
    /// The pair is the whole guard, and each half refuses a different world:
    ///
    /// - a cursor that owes nothing or one the accumulator never issued holds no pair at all;
    /// - a cursor whose debt has since been REPLACED (a lag, a dropped event, blindness) no
    ///   longer carries the loss — the token names a window this call knows nothing about, and
    ///   settling on it would answer a reconcile nobody performed;
    /// - a cursor whose POSITION has moved is one `make_room` cut out of entries it never read,
    ///   without renaming the debt it already owed. The rebuild's baseline scan cannot contain
    ///   those entries, so this settle is not the one they were owed: it refuses and the debt
    ///   survives.
    ///
    /// Only the debt moves. `pos` stays where the re-subscription put it, so an entry that
    /// landed after the fresh cursor was taken is still delivered by the next drain, and
    /// `delivered` stays as the consumer's hand left it.
    fn settle_carried_debt(&mut self, id: u64, carried: &CarriedDebt) -> bool {
        let Some(cursor) = self.cursors.get_mut(&id) else { return false };
        if cursor.pending.is_none()
            || cursor.loss != Some(carried.loss)
            || cursor.pos != carried.pos
        {
            return false;
        }
        cursor.pending = None;
        cursor.loss = None;
        self.close_window_if_settled();
        true
    }

    fn loss_horizon(&self) -> LossHorizon {
        let mut live: Vec<u64> = self
            .cursors
            .values()
            .flat_map(|cursor| [cursor.loss, cursor.delivered])
            .flatten()
            .chain(self.window_loss)
            .collect();
        live.sort_unstable();
        live.dedup();
        LossHorizon { issued: self.losses_issued, live }
    }

    fn record(&mut self, canonical: PathBuf, raw: PathBuf, kind: ChangeKind) {
        // A brand-new key past the cap means more than `cap` distinct paths are waiting
        // undrained — but they are waiting for SOMEBODY, and the accumulator is held at
        // the slowest cursor. Making room is therefore that cursor's business, not
        // everyone's. Already-tracked keys just refresh below.
        if !self.entries.contains_key(&canonical) && self.entries.len() >= self.cap {
            self.make_room();
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.insert(canonical.clone(), ChangeEntry { canonical, raw, kind, seq });
        self.generation += 1;
    }

    /// Release undrained detail until the accumulator is back inside its cap, charging
    /// the release to the cursors that are actually holding it.
    ///
    /// The floor is the SLOWEST cursor, so the one furthest behind is advanced first and
    /// told it lost detail; everyone level with the stream keeps their exact paths and
    /// owes nothing. Only that one, not every cursor behind the head: with positions 0, 1
    /// and current, freeing what 0 pins is enough, and 1 lost nothing.
    ///
    /// Terminates by construction: each round advances one cursor that was strictly
    /// behind, and once every cursor sits at `max_seq` the reclaim below removes every
    /// entry there is.
    fn make_room(&mut self) {
        self.reclaim();
        while self.entries.len() >= self.cap {
            let max = self.max_seq();
            let Some(id) = self
                .cursors
                .iter()
                .filter(|(_, cursor)| cursor.pos < max)
                .min_by_key(|(_, cursor)| cursor.pos)
                .map(|(id, _)| *id)
            else {
                // Nobody is observing, or everybody is current: whatever is left is
                // nobody's to lose, and the cap has to hold regardless.
                self.entries.clear();
                return;
            };
            // A loss nobody else suffered, so it is issued its own identity — but only when
            // this cursor did not already owe one. A debt it has not yet paid covers this cut
            // too, and re-issuing would make the reconcile it is already about read as new.
            if let Some(cursor) = self.cursors.get_mut(&id) {
                cursor.pos = max;
                // `get_or_insert`, not an overwrite: an open window's reason is the more
                // informative of the two, and this cursor owes one reconcile either way.
                if cursor.pending.is_none() {
                    self.losses_issued += 1;
                    cursor.pending = Some(DegradeReason::CursorLagged);
                    cursor.loss = Some(self.losses_issued);
                }
                self.generation += 1;
            }
            self.reclaim();
        }
    }

    /// Enter a reconcile only if somebody is there to owe it to, deciding and acting under
    /// ONE hold of this lock. Says whether the window was opened.
    ///
    /// The two cannot be separate: the last cursor unsubscribing between them closes every
    /// window that existed and then a debt is written over an empty set — one nobody can
    /// acknowledge, which leaves the hub calling itself degraded and hands the next
    /// subscriber a reconcile for a window it was never inside.
    /// Hand ONE cursor a reconcile, without opening a shared window.
    ///
    /// For the cursor that arrived while blindness was being published: the two states live
    /// under two locks, in that order to keep either path from holding one while asking for
    /// the other, so a subscription can land in the gap between them. Nothing is owed to
    /// anybody else — the window, if there is one, has already flagged whoever was there.
    fn force_rescan(&mut self, id: u64, reason: DegradeReason) {
        if let Some(cursor) = self.cursors.get_mut(&id) {
            if cursor.pending.is_none() {
                // A loss of this cursor's own, so an identity of its own. Without one its batch
                // borrowed the last number issued — the one a consumer has usually just acted
                // on, which reads the new debt as a repeat.
                self.losses_issued += 1;
                cursor.pending = Some(reason);
                cursor.loss = Some(self.losses_issued);
                self.generation += 1;
            }
        }
    }

    fn enter_rescan_for_listeners(&mut self, reason: DegradeReason) -> bool {
        if self.cursors.is_empty() {
            // Nothing is opened and nothing is ERASED. That nobody is here to be owed a new
            // window says nothing about a reason recorded earlier, which `health` still
            // reports to a status caller; what closes such a reason is the obstacle ending,
            // and that is answered where the obstacle is read.
            return false;
        }
        self.enter_rescan(false, reason);
        true
    }

    /// Enter a reconcile window: optionally clear the (now-untrusted) entries, flag every
    /// live cursor to reconcile once, and record the reason.
    ///
    /// Idempotent in what it SAYS, not in what it owes: a repeated report of an open window
    /// re-logs nothing, so a storm does not fill the log — but every raise is a distinct
    /// loss, moves the generation, and cannot be acknowledged away by a batch taken before
    /// it.
    fn enter_rescan(&mut self, clear_entries: bool, reason: DegradeReason) {
        self.rescans_requested += 1;
        // ONE loss, however many cursors it reaches: a consumer that sees it through two
        // cursors is seeing one event, and the identity is what says so.
        self.losses_issued += 1;
        let issued = self.losses_issued;
        self.window_loss = Some(issued);
        let newly = self.degrade_reason.is_none();
        if clear_entries {
            self.entries.clear();
        }
        for cursor in self.cursors.values_mut() {
            // Overwritten, unlike a lag debt: this is the newest thing that went wrong,
            // and it is what a consumer asking why it must reconcile should be told.
            cursor.pending.replace(reason.clone());
            cursor.loss = Some(issued);
        }
        // Moved for every raise while anyone is listening, not only for the first. Two
        // windows in a row carry the same reason and would otherwise be one: a batch taken
        // against the first would still look current, so acknowledging it would clear a
        // debt the consumer's scan ended before — and a sink already waiting on the
        // generation would sleep out its whole timeout over a loss just handed to it.
        // Idempotence stays where it belongs, in the LOGGING below: a storm re-reports
        // nothing.
        let changed = newly || !self.cursors.is_empty();
        self.degrade_reason = Some(reason.clone());
        if changed {
            self.generation += 1;
        }
        if newly {
            tracing::warn!(
                ?reason,
                "workspace change hub entering reconcile; consumers will rescan"
            );
        }
    }

    fn materialize(&mut self, id: u64) -> DrainBatch {
        let max = self.max_seq();
        let pos = self.cursors.get(&id).map(|c| c.pos).unwrap_or(max);
        let mut entries: Vec<ChangeEntry> =
            self.entries.values().filter(|e| e.seq > pos).cloned().collect();
        entries.sort_by_key(|e| e.seq);
        let cursor = self.cursors.get(&id);
        let rescan_required = cursor.is_some_and(|cursor| cursor.pending.is_some());
        let batch = DrainBatch {
            entries,
            cursor: SinkCursor { id },
            rescan_required,
            losses: cursor.and_then(|cursor| cursor.loss).unwrap_or(self.losses_issued),
            through_seq: max,
            start_pos: pos,
            generation: self.generation,
        };
        if let Some(cursor) = self.cursors.get_mut(&id) {
            cursor.delivered = batch.loss_token();
        }
        batch
    }

    fn acknowledge(&mut self, batch: &DrainBatch) {
        let Some(cursor) = self.cursors.get_mut(&batch.cursor.id) else { return };
        // Whatever the acknowledgement settles, the consumer is done with this batch: it
        // recorded what the batch said before acknowledging it.
        if batch.loss_token().is_some() && cursor.delivered == batch.loss_token() {
            cursor.delivered = None;
        }
        if cursor.pos != batch.start_pos {
            return;
        }
        cursor.pos = batch.through_seq;
        if batch.rescan_required && self.generation == batch.generation {
            cursor.pending.take();
            cursor.loss.take();
        }
        self.close_window_if_settled();
        self.reclaim();
    }

    fn drain(&mut self, id: u64) -> DrainBatch {
        let batch = self.materialize(id);
        self.acknowledge(&batch);
        batch
    }

    /// Recover once no live cursor still owes THE WINDOW.
    ///
    /// A private lag debt does not count: it belongs to one cursor, and letting it hold the
    /// window open would put the shared verdict back under one consumer's silence — and
    /// charge every newcomer, which inherits the window, a reconcile it owes to nobody.
    ///
    /// A debt is settled two ways, and the second one is easy to miss: acknowledged by a
    /// drain, or carried off by a cursor that leaves. Leaving is the ordinary end for a
    /// consumer that never started, so a window closed only on drain would outlive everyone
    /// who was ever party to it and be inherited by whoever subscribes next.
    fn close_window_if_settled(&mut self) {
        let owes_window = |cursor: &CursorState| matches!(&cursor.pending, Some(reason) if *reason != DegradeReason::CursorLagged);
        if self.degrade_reason.is_some() && !self.cursors.values().any(owes_window) {
            self.degrade_reason = None;
            self.window_loss = None;
        }
    }

    /// Drop entries every live cursor has already advanced past. With no cursors
    /// nothing is being observed, so the map is emptied.
    fn reclaim(&mut self) {
        match self.cursors.values().map(|c| c.pos).min() {
            Some(min_pos) => self.entries.retain(|_, e| e.seq > min_pos),
            None => self.entries.clear(),
        }
    }

    fn health(&self) -> Health {
        if self.setup_failed {
            return Health::Degraded(DegradeReason::WatcherSetup);
        }
        match &self.degrade_reason {
            Some(reason) => Health::Degraded(reason.clone()),
            None => Health::Healthy,
        }
    }

    /// Health as it concerns ONE consumer: the hub's own condition plus that cursor's
    /// own outstanding debt — never somebody else's.
    ///
    /// Without a cursor the answer is the shared one: a consumer that has not subscribed
    /// has observed nothing, and nothing observed is no reason to trust the stream.
    fn health_for(&self, cursor: Option<u64>) -> Health {
        if self.setup_failed {
            return Health::Degraded(DegradeReason::WatcherSetup);
        }
        let Some(id) = cursor else {
            return self.health();
        };
        match self.cursors.get(&id).and_then(|cursor| cursor.pending.clone()) {
            Some(reason) => Health::Degraded(reason),
            None => Health::Healthy,
        }
    }
}

/// What the hub takes into work, derived from the watch targets themselves.
///
/// Two permissions, deliberately separate. A path under a SCAN ROOT may be
/// recorded, walked and re-watched. A project-config file directly in a config
/// directory may only be recorded — the name grants no right to walk, or a
/// directory that merely carries that name would be taken under recursive watch.
/// The permissions add up: in a flat project the workspace IS the scan root, so a
/// config-named directory there is walked on the ordinary rule.
#[derive(Debug, Default, Clone)]
struct Scope {
    /// The scan roots and the subtrees punched out of them — asked, never
    /// re-implemented: the walk decides the same pair of inputs with the same type,
    /// and a second answer here is how a file ends up walked but unwatched.
    ///
    /// Holes are the analyzer's own derived cache: by default it sits at
    /// `<workspace>/.build`, inside the recursive watch, so every index write the
    /// server performs comes back as an event about the workspace it was analyzing.
    /// Narrowing by roots alone cannot express that — the cache is not a smaller root,
    /// it is a hole inside one.
    paths: PathScope,
    /// Directories watched non-recursively for the project-config files sitting
    /// directly in them. Not roots: the hub's own concern, and the only question asked
    /// of them is whether one IS a given file's parent.
    config_dirs: Vec<Spellings>,
}

/// Watch targets whose relative paths have been resolved against the current
/// directory exactly ONCE, before anything is armed or compared.
///
/// A newtype rather than a convention, because forgetting the call is invisible:
/// the current directory would then be read twice from process-wide state — by the
/// scope and, later, by the backend inside `watch` — and a change in between would
/// leave the watcher armed on one tree while the scope describes another, with
/// every event from the armed tree filtered out in silence. Nothing observable
/// fails, so no test catches it; the type does. Handing the backend an
/// already-absolute path also removes that second read entirely: `watch_inner`
/// takes an absolute path as given.
/// In its own module so the fields are out of reach even here: a tuple constructor
/// visible to the rest of the file would let the resolution be skipped by writing
/// `ResolvedTargets(targets)`, which is precisely the mistake the type exists to
/// make impossible.
mod resolved {
    use std::path::Path;

    use super::WatchTarget;

    pub(super) struct ResolvedTargets {
        targets: Vec<WatchTarget>,
        complete: bool,
    }

    impl ResolvedTargets {
        /// Resolve against ONE snapshot of the current directory, taken by the
        /// caller for the whole set. Per-target reads would let a set spanning a
        /// scan root and a config directory land in two different workspaces.
        ///
        /// The join is exactly what a notify backend does to a relative target:
        /// prepend the current directory and leave the components alone.
        /// `std::path::absolute` is NOT a substitute — on Windows it goes through
        /// `GetFullPathNameW`, which resolves `..` away (`C:\foo\..\bar.rs` becomes
        /// `C:\foo\bar.rs`), while the backend keeps the component, so the watched
        /// path and the reported one would stop matching and the whole tree would go
        /// silent. On Unix it also drops `.` components, which the backend keeps.
        ///
        /// Without a snapshot (`cwd` is `None` — a deleted or unreadable current
        /// directory) a relative target is DROPPED rather than carried through
        /// relative: keeping it would hand the backend a path it resolves against
        /// its own later read of the same process-wide state, which is the very
        /// disagreement this type exists to prevent. The set then reports itself
        /// incomplete, and the caller degrades instead of claiming coverage.
        pub(super) fn resolve(targets: Vec<WatchTarget>, cwd: Option<&Path>) -> Self {
            let mut complete = true;
            let targets = targets
                .into_iter()
                .filter_map(|target| {
                    if target.path.is_absolute() {
                        return Some(target);
                    }
                    let placed = cwd.map(|cwd| cwd.join(&target.path));
                    // The join is checked, not trusted. On Windows a drive-relative
                    // target (`C:src`) carries a prefix without a root, and `join`
                    // REPLACES the base with it, so the result is still relative and
                    // the backend would resolve it against its own later read of the
                    // per-drive current directory — the very race being removed here.
                    match placed.filter(|path| path.is_absolute()) {
                        Some(path) => Some(WatchTarget { path, recursive: target.recursive }),
                        None => {
                            tracing::warn!(
                                root = ?target.path,
                                "workspace change hub cannot place a relative watch root"
                            );
                            complete = false;
                            None
                        }
                    }
                })
                .collect();
            Self { targets, complete }
        }

        pub(super) fn here(targets: Vec<WatchTarget>) -> Self {
            Self::resolve(targets, std::env::current_dir().ok().as_deref())
        }

        /// Whether every requested target survived resolution. `false` means the
        /// watch cannot cover what was asked for, whatever the backend then says.
        pub(super) fn is_complete(&self) -> bool {
            self.complete
        }

        pub(super) fn as_slice(&self) -> &[WatchTarget] {
            &self.targets
        }

        pub(super) fn into_inner(self) -> Vec<WatchTarget> {
            self.targets
        }
    }
}

use resolved::ResolvedTargets;

impl Scope {
    /// Recursive targets are the scan roots; a non-recursive target is there for
    /// the project-config files that live directly in it (see
    /// [`watch_targets_for`]). Built from the DESIRED targets, not the armed ones:
    /// a root that failed to arm is still part of the scope, and its events —
    /// arriving through a covering target — must not be dropped.
    #[cfg(test)]
    fn from_targets(targets: &ResolvedTargets, excluded: &[PathBuf]) -> Self {
        Self::from_targets_scoped(targets, excluded, &ExcludedPaths::default())
    }

    /// [`Self::from_targets`] with the user's `[source].exclude`, which wins over every
    /// root and is never carved back out.
    fn from_targets_scoped(
        targets: &ResolvedTargets,
        excluded: &[PathBuf],
        user_excluded: &ExcludedPaths,
    ) -> Self {
        let targets = targets.as_slice();
        let scan_roots: Vec<PathBuf> =
            targets.iter().filter(|t| t.recursive).map(|t| t.path.clone()).collect();
        Self {
            // Rebuilt here rather than carried over, because this runs on every re-arm
            // and the roots are what changed: a root declared under the cache after the
            // hub came up is carved back out of it, instead of being dropped in silence
            // for the rest of the process.
            paths: PathScope::with_exclusions(&scan_roots, excluded, user_excluded),
            config_dirs: targets
                .iter()
                .filter(|t| !t.recursive)
                .map(|t| Spellings::of(&t.path))
                .collect(),
        }
    }

    #[cfg(test)]
    fn from_targets_for_test(targets: &ResolvedTargets) -> Self {
        Self::from_targets(targets, &[])
    }

    /// Whether `path` lies in a subtree the hub does not speak for.
    fn is_excluded(&self, path: &Path) -> bool {
        self.paths.is_hole(path)
    }

    /// Whether a change to `path` may be walked and taken under recursive watch.
    fn may_walk(&self, path: &Path) -> bool {
        !self.is_excluded(path) && self.paths.covers(path)
    }

    /// Whether a change to `path` may be recorded for consumers.
    fn may_record(&self, path: &Path) -> bool {
        !self.is_excluded(path) && (self.may_walk(path) || self.is_project_config(path))
    }

    /// A project-config file sitting DIRECTLY in a config directory. Decided from
    /// the name and the parent alone, never from the disk: a deleted config shapes
    /// the topology just as much as an edited one, and a predicate gated on "the
    /// file exists" would drop the removal.
    fn is_project_config(&self, path: &Path) -> bool {
        let named_like_a_config = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(project_model::is_project_input_file_name);
        if !named_like_a_config {
            return false;
        }
        let Some(parent) = path.parent() else { return false };
        self.config_dirs.iter().any(|dir| dir.is(parent))
    }
}

struct HubInner {
    acc: Mutex<Accumulator>,
    /// Signalled when there is new work to drain, or setup has settled.
    wake: Condvar,
    /// How many times sinks were woken. A wake that carries no new work costs a
    /// consumer a full drain-and-apply pass, and the cost is invisible in the
    /// accumulator: a wake without a generation bump leaves no trace there. The
    /// counter is the only observable that separates "nothing was recorded" from
    /// "nothing was recorded and nobody was disturbed".
    notifications: AtomicU64,
    /// Subtrees the hub does not speak for, fixed when the hub is created.
    ///
    /// On the inner, not in the watch targets: `Scope` is rebuilt from the targets on
    /// every re-arm, and `ensure_roots` is called by consumers that know the scan
    /// roots but nothing about the cache layout (the graph builder, the diagnostics
    /// lifecycle). Carrying the exclusions in the target list would let any of them
    /// drop the exclusions by simply not knowing to pass them.
    excluded: Vec<PathBuf>,
    /// The user's `[source].exclude`. Unlike `excluded`, it follows the project: a
    /// consumer that re-declares the roots re-declares these with them
    /// ([`WorkspaceChangeHub::ensure_scope`]), and the backend door reads it on every arm.
    user_excluded: Arc<RwLock<ExcludedPaths>>,
    /// Events dropped because they landed in an excluded subtree. Diagnostic only:
    /// a workspace whose cache is being written constantly is otherwise
    /// indistinguishable from a quiet one.
    excluded_events: AtomicU64,
    /// Set once the recursive watch is armed; false until the hub thread finishes
    /// setup (or forever, if setup failed).
    watching: AtomicBool,
    /// Set by the notify callback when the bounded channel is full: events were
    /// dropped, so the hub thread must trigger a reconcile. Non-locking, so the
    /// callback never blocks.
    channel_overflow: AtomicBool,
    /// `(canonical path, recursive)` of the currently-armed watch targets,
    /// published by the hub thread at setup and after every re-arm, so a consumer
    /// can compare a project snapshot's targets against the live set without a
    /// control roundtrip.
    watched_roots: Mutex<Vec<(PathBuf, bool)>>,
    /// What the hub takes into work. Set before the thread starts (events may
    /// arrive before setup finishes) and re-derived on every re-arm.
    scope: Mutex<Scope>,
    /// How often the hub re-checks that its declared coverage is still live. A field,
    /// not a constant: a test that waited the production interval would be unusable,
    /// and a globally swappable constant would be shared state between parallel tests.
    tick_period: Duration,
    /// Coverage ticks that ran, and re-arms they caused. Both are needed: a tick that
    /// merely happens proves nothing, and a re-arm that never happens is exactly the
    /// failure this node exists to prevent.
    ticks: AtomicU64,
    rearms: AtomicU64,
    /// The fallback poll's cadence and read budget, and what it has done (see [`run_polling`]).
    poll: PollConfig,
    polling: AtomicBool,
    poll_state: Mutex<PollStatus>,
    /// The poll of declared roots that exist and are not watched, while the rest is.
    blind_poll: BlindPoll,
    /// The declaration the THREAD stands on, published by it after every declaration it
    /// applies. `ensure_roots` compares against this, not against the armed set: a target the
    /// backend could not take is a gap the hub repairs itself, and comparing coverage would
    /// make every rebuild re-declare, re-arm and hand every consumer a reconcile — one
    /// rebuild per reconcile, for ever. Compared by placed spelling AND mode: an alias swap
    /// keeps the canonical path and would otherwise pass unnoticed.
    declared_published: Mutex<Vec<WatchTarget>>,
    /// From when the hub owes an observation it has not made yet: the moment it entered the
    /// fallback poll, or the moment a declared root went blind. A poll that was promised and
    /// never happened is overdue exactly like one that stopped happening — without this, a
    /// poller whose thread never started reads as "fresh" for ever.
    poll_expected_since: Mutex<Option<Instant>>,
    /// Declared targets that exist and are not watched. DERIVED from the declaration and
    /// the armed set on every change to either, never accumulated: a target can leave the
    /// declaration without ever arming (a topology rebuild drops an extension, and a
    /// re-arm only ever arms what is now desired), and an accumulated set would keep the
    /// hub degraded for the life of the daemon over a root nobody declares any more.
    blind_targets: Mutex<Vec<PathBuf>>,
}

impl HubInner {
    /// Publish the armed targets' `(resolved-at-arm, recursive)` pairs for cheap
    /// comparisons by [`WorkspaceChangeHub::ensure_roots`]. Targets whose
    /// `watch()` failed are not included, so a retry re-arms them.
    ///
    /// DECLARED targets only. `ensure_roots` compares this list against the set it is
    /// about to declare, and a watch the declaration does not name — a door an event
    /// revealed — would read as a permanent difference: a re-arm, and the rescan it costs
    /// every consumer, on every call for as long as the door stands.
    fn publish_watched_roots(&self, armed: &[ArmedTarget]) {
        let pairs: Vec<(PathBuf, bool)> = armed
            .iter()
            .filter(|entry| entry.is_declared())
            .map(|entry| (entry.resolved().to_path_buf(), entry.target().recursive))
            .collect();
        *self.watched_roots.lock().unwrap_or_else(PoisonError::into_inner) = pairs;
    }

    /// The declaration the thread stands on right now.
    fn accepted_declaration(&self) -> Vec<WatchTarget> {
        self.declared_published.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Publish the declaration the thread has just applied. The ONLY writer is the thread:
    /// a declaration recorded before the thread accepted it would let the next identical
    /// request answer "already declared" over a set nothing stands on.
    fn accept_declaration(&self, declared: &[WatchTarget]) {
        *self.declared_published.lock().unwrap_or_else(PoisonError::into_inner) = declared.to_vec();
    }

    /// Record (or clear) the moment from which an observation is owed but not yet made.
    fn expect_poll_from(&self, at: Option<Instant>) {
        *self.poll_expected_since.lock().unwrap_or_else(PoisonError::into_inner) = at;
    }

    fn poll_expected_since(&self) -> Option<Instant> {
        *self.poll_expected_since.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_acc(&self) -> std::sync::MutexGuard<'_, Accumulator> {
        self.acc.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn scope(&self) -> Scope {
        self.scope.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Wake the sinks. The single door: a bare `wake.notify_all()` elsewhere would
    /// not be counted, and an uncounted wake is exactly the one that spins a sink.
    /// The only way a `Scope` is built after construction: it carries the hub's own
    /// exclusions, so a caller that re-arms with a new root set cannot drop them.
    fn scope_from(&self, targets: &ResolvedTargets) -> Scope {
        Scope::from_targets_scoped(targets, &self.excluded, &self.user_excluded())
    }

    fn user_excluded(&self) -> ExcludedPaths {
        self.user_excluded.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Note an event dropped for landing in an excluded subtree.
    fn note_excluded(&self, path: &Path) {
        let total = self.excluded_events.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::trace!(path = %path.display(), total, "change hub dropped an event inside an excluded root");
        if total.is_multiple_of(1024) {
            tracing::debug!(total, "change hub keeps dropping events inside an excluded root");
        }
    }

    fn notify(&self) {
        self.notifications.fetch_add(1, Ordering::Relaxed);
        self.wake.notify_all();
    }

    fn set_scope(&self, scope: Scope) {
        *self.scope.lock().unwrap_or_else(PoisonError::into_inner) = scope;
    }

    /// Fold one raw watcher result into the accumulator and return the directories
    /// that must be (re-)watched recursively. The caller (which owns the watcher)
    /// applies the re-watch off the notify callback thread, so `notify::watch` is
    /// never re-entered from inside a notify callback. The accumulator mutex is
    /// never held across filesystem I/O: every path is stat'd (and new subtrees
    /// walked) before the lock is taken.
    fn ingest_event(&self, res: Result<Event, notify::Error>) -> Vec<PathBuf> {
        let event = match res {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!("workspace watch event error: {error}");
                self.lock_acc().enter_rescan(false, DegradeReason::RuntimeError);
                self.notify();
                return Vec::new();
            }
        };

        {
            let mut acc = self.lock_acc();
            acc.events_seen += 1;
        }

        // The scope filter runs BEFORE the branch on event kind, and per PATH
        // rather than per event. Before, because an unknown kind degrades on the
        // spot, and one foreign file would otherwise drag every consumer into a
        // rescan. Per path, because `Modify(Name(Both))` carries the vanished and
        // the arrived path in one event, and a rename out of a scan root puts them
        // on opposite sides of the boundary. An event with NO paths is left alone:
        // an absent path is not evidence that the lost change was out of scope.
        //
        // A rescan notice is handled before any of that: it does not report a change
        // to the path it names, it reports that the stream lapsed and nothing
        // received so far can be trusted. Scope says nothing about what was lost, so
        // neither the filter nor the kind may swallow it — the flag is an attribute
        // in its own right, and nothing in notify's contract ties it to one kind.
        // Inotify raises it without a path; FSEvents attaches one, commonly the
        // workspace directory, which in a nested layout lies outside every scan root.
        let rescan_moved_generation = if event.need_rescan() {
            let mut acc = self.lock_acc();
            let before = acc.generation;
            acc.enter_rescan(false, DegradeReason::UnknownEvent);
            acc.generation != before
        } else {
            false
        };

        let scope = self.scope();
        for path in event.paths.iter().filter(|path| scope.is_excluded(path)) {
            self.note_excluded(path);
        }
        let paths: Vec<PathBuf> =
            event.paths.iter().filter(|path| scope.may_record(path)).cloned().collect();
        if !event.paths.is_empty() && paths.is_empty() {
            // Nothing was recorded, so there is nothing for a sink to drain. Waking
            // one anyway is not merely wasteful: a sink that writes into the watched
            // tree on every pass (the cache lease does) turns the wake into the next
            // event, and the two feed each other at syscall speed.
            //
            // "Nothing was recorded" is not the same as "nothing happened", though: a
            // rescan notice moves the generation before this filter runs, and the
            // notice is exactly the case where its path says nothing about what was
            // lost. Staying silent on a moved generation would leave every sink asleep
            // until its own timeout, blind to the whole window.
            if rescan_moved_generation {
                self.notify();
            }
            return Vec::new();
        }

        let mut rewatch: Vec<PathBuf> = Vec::new();
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                let mut records: Vec<(PathBuf, PathBuf, ChangeKind)> = Vec::new();
                for path in &paths {
                    // A directory that just appeared needs two things bare recursive
                    // watching does not give reliably on Linux: files written into
                    // it before the OS watch arms are lost, and a deep subtree
                    // created in one burst may never be watched. Walking the new
                    // subtree records whatever already exists (stats are truth),
                    // and re-arming a recursive watch covers everything created
                    // afterwards.
                    //
                    // Appearing is not only `Create`: a directory MOVED into the
                    // tree arrives as `Modify(Name(To))`, and the files that rode
                    // along with it fire no events of their own — their path
                    // changed, they did not. `Name` and not any `Modify`, though: a
                    // chmod on a large directory would walk it for nothing.
                    //
                    // Walking is the narrower permission: a config file is in scope
                    // by name, but a DIRECTORY carrying that name is still foreign,
                    // and walking it is the very cost this boundary exists to avoid.
                    let may_have_appeared = matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Modify(notify::event::ModifyKind::Name(_))
                    );
                    if may_have_appeared && scope.may_walk(path) {
                        if let Ok(meta) = std::fs::metadata(path) {
                            if meta.is_dir() {
                                rewatch.push(path.clone());
                                collect_subtree_within(
                                    path,
                                    &mut records,
                                    scope.paths.exclusions(),
                                );
                                continue;
                            }
                        }
                    }
                    if let Some((canonical, kind)) = classify_path(path) {
                        records.push((canonical, path.clone(), kind));
                    }
                }
                if !records.is_empty() {
                    let mut acc = self.lock_acc();
                    for (canonical, raw, kind) in records {
                        acc.record(canonical, raw, kind);
                    }
                }
            }
            // Reads/opens/closes are understood and irrelevant to drift; ignore
            // them without degrading. Anything else (`Any`/`Other`) is a kind we
            // do not model, so assume the stream may be incomplete and ask
            // consumers to reconcile — the scan then covers whatever was missed.
            EventKind::Access(_) => {}
            _ => self.lock_acc().enter_rescan(false, DegradeReason::UnknownEvent),
        }

        self.notify();
        rewatch
    }

    /// If the notify callback reported a dropped-event overflow, fold it into the
    /// reconcile path once.
    fn drain_channel_overflow(&self) {
        if self.channel_overflow.swap(false, Ordering::Relaxed) {
            self.lock_acc().enter_rescan(true, DegradeReason::Overflow);
            self.notify();
        }
    }

    /// A newly-created subtree could not be added to the recursive watch, so it
    /// may miss further changes. Ask consumers to reconcile (recoverable, like a
    /// runtime error) rather than silently going blind. Entries stay: what is
    /// already tracked is still valid; only the un-watched subtree is at risk.
    fn note_rewatch_failed(&self, dir: &Path, error: &notify::Error) {
        tracing::warn!(
            ?dir,
            "change hub could not extend watch to new subtree; drift there may be missed: {error}"
        );
        self.lock_acc().enter_rescan(false, DegradeReason::RewatchFailed);
        self.notify();
    }

    /// The watch was extended over a directory an event revealed, and the backend charged
    /// the stream that was already running for it: everything that happened anywhere in the
    /// watched tree between that stream's stop and the new one's start was dropped and will
    /// never be delivered.
    ///
    /// A successful arm, and still a loss — which is why no other reporter here covers it.
    /// The blind set answers the OPPOSITE case, a target that failed to arm, and it reports
    /// only the transition into blindness; a re-arm pays its own debt at the end of
    /// `apply_rearm`; and the arm itself, being a success, tells nobody anything.
    /// Owed to whoever was listening ACROSS the window: a cursor taken afterwards begins
    /// where the window ended, and a hub nobody has subscribed to has lost nothing for
    /// anyone. Asked and answered under one hold of the accumulator, so the last cursor
    /// cannot leave between the question and the debt.
    fn note_arming_window(&self, dir: &Path) {
        if !self.lock_acc().enter_rescan_for_listeners(DegradeReason::Rearmed) {
            return;
        }
        tracing::debug!(
            ?dir,
            "change hub restarted the watch stream to reach a new subtree; \
             consumers reconcile the window that cost"
        );
        self.notify();
    }

    /// A target that could not be placed leaves a subtree unwatched under a path
    /// nobody downstream can even name, unlike a root that merely failed to arm —
    /// there the path is known and a later re-arm retries it.
    ///
    /// What this buys is ONE forced reconcile, not standing ill health: `drain`
    /// clears the reason once every cursor has acknowledged that round, while the
    /// dropped target stays dropped. Standing ill health has exactly one carrier
    /// here, `setup_failed`, and it means the hub is unusable — spending it on a
    /// partial drop would cost every consumer the event stream it still has, to
    /// describe a subtree the periodic reconcile already covers. So: nothing
    /// derived before the drop is trusted, and coverage afterwards is the
    /// reconciler's, the same bargain an unarmable root gets.
    fn note_unplaced_targets(&self) {
        self.lock_acc().enter_rescan(false, DegradeReason::WatcherSetup);
        self.notify();
    }

    /// Is anything declared, present and unwatched right now?
    fn is_partially_blind(&self) -> bool {
        !self.blind_targets.lock().unwrap_or_else(PoisonError::into_inner).is_empty()
    }

    /// Blind, and the reconcile announcing it already issued. A newcomer is handed a reconcile
    /// of its own only then; before, the announcement that follows the first reading flags it.
    fn is_blind_and_announced(&self) -> bool {
        self.is_partially_blind() && !self.blind_poll.reconcile_pending.load(Ordering::SeqCst)
    }

    fn note_poll(&self, poller: &Poller) {
        let mut state = self.poll_state.lock().unwrap_or_else(PoisonError::into_inner);
        state.last = Some(Instant::now());
        state.polls += 1;
        state.bytes = poller.bytes();
    }

    fn mark_setup_failed(&self) {
        self.lock_acc().setup_failed = true;
        self.notify();
    }

    /// Hand the hub to its poll thread and announce the failure in ONE order: the poll flag
    /// first, the failure second. A waiter waking on `setup_failed` is being handed to the
    /// poll, and an instant where the failure is visible while the poll is not reads as "no
    /// watch and no poll" — which is the state a sink must never enable watcher mode over
    /// (github#186, github#187). The one failure that truly has no poll behind it — a thread
    /// the OS refused to start — keeps [`Self::mark_setup_failed`] alone.
    fn mark_polling_setup_failed(&self) {
        self.polling.store(true, Ordering::SeqCst);
        self.mark_setup_failed();
    }

    fn mark_watching(&self) {
        self.watching.store(true, Ordering::SeqCst);
        // Bump generation under the lock so `wait_until_watching` wakers re-check.
        self.lock_acc().generation += 1;
        self.notify();
    }
}

// Thread-local on purpose: tests run in parallel and a process-global counter
// would let one case observe another's walks.
#[cfg(test)]
thread_local! {
    static SUBTREE_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Walk a freshly-created directory and record every file already inside it, resolving
/// once per containing directory rather than once per file.
///
/// The key is the CONTAINING directory resolved, plus the file's own name — and a file
/// that is ITSELF a link is resolved in full, because only then does its directory's
/// prefix stop being the whole answer. That is `project_model::workspace_walk`'s rule, and
/// it has to be the same one: the scan keys its universe that way, and a key the scan
/// never produces reads to a consumer as drift the hub failed to deliver. Resolving only
/// the walked ROOT and joining the tail is not the same rule — a linked SUBdirectory
/// inside it leaves the tail unresolved — which is exactly the shape a door has.
///
/// The cache is keyed by the WALKED directory, not the resolved one, for the same reason
/// it is there: two links to one tree are two ways to reach the same files, and each file
/// keeps the spelling the walk actually used to get to it.
#[cfg(all(test, unix))]
fn collect_subtree(dir: &Path, records: &mut Vec<(PathBuf, PathBuf, ChangeKind)>) {
    collect_subtree_within(dir, records, &ExcludedPaths::default());
}

/// [`collect_subtree`] that never enters a directory the user excluded.
fn collect_subtree_within(
    dir: &Path,
    records: &mut Vec<(PathBuf, PathBuf, ChangeKind)>,
    user_excluded: &ExcludedPaths,
) {
    collect_subtree_noting(dir, records, None, user_excluded);
}

/// [`collect_subtree`], naming in `unreadable` every path the walk could not read for another
/// reason than absence.
fn collect_subtree_noting(
    dir: &Path,
    records: &mut Vec<(PathBuf, PathBuf, ChangeKind)>,
    mut unreadable: Option<&mut Vec<PathBuf>>,
    user_excluded: &ExcludedPaths,
) {
    #[cfg(test)]
    SUBTREE_WALKS.with(|walks| walks.set(walks.get() + 1));
    let mut resolved_dirs: HashMap<PathBuf, PathBuf> = HashMap::new();
    // The user's exclusions are pruned before the walk enters them — a directory arriving
    // already full of files must not be read inside an exclusion, under any spelling.
    let mut pruned = project_model::ResolvedDirs::default();
    let walk = WalkDir::new(dir).follow_links(true).into_iter().filter_entry(|entry| {
        !(entry.file_type().is_dir() || entry.depth() == 0 || entry.path_is_symlink())
            || !user_excluded.prunes_walked_dir(entry.path(), entry.path_is_symlink(), &mut pruned)
    });
    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let absent =
                    error.io_error().is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
                if let (Some(unreadable), Some(path), false) =
                    (unreadable.as_deref_mut(), error.path(), absent)
                {
                    unreadable.push(path.to_path_buf());
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let file = entry.path();
        let canonical = if entry.path_is_symlink() {
            let resolved = resolve_as_far_as_it_goes(file);
            // The role has to hold for BOTH spellings — the second half of the walk's rule,
            // and inseparable from the first. A link named `Alias.txt` onto a `Target.bsl`
            // resolves to a key the walk of the TARGET's root does list, but this walk was
            // never entitled to reach it, and handing it over would register a file from
            // outside the workspace as drift inside it.
            if project_model::file_role(&resolved) != project_model::file_role(file) {
                continue;
            }
            resolved
        } else {
            match (file.parent(), file.file_name()) {
                (Some(parent), Some(name)) => resolved_dirs
                    .entry(parent.to_path_buf())
                    .or_insert_with(|| resolve_as_far_as_it_goes(parent))
                    .join(name),
                _ => resolve_as_far_as_it_goes(file),
            }
        };
        records.push((canonical, file.to_path_buf(), ChangeKind::MaybeChanged));
    }
}

/// Re-derive what happened to `path` from its current on-disk state. Returns the
/// canonical key and the change kind, or `None` for events that carry no drift
/// (a bare directory whose children arrive as their own events, a transient stat
/// error that must not be mistaken for a removal).
fn classify_path(path: &Path) -> Option<(PathBuf, ChangeKind)> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => None,
        Ok(meta) if meta.is_file() => {
            let resolved = resolve_as_far_as_it_goes(path);
            // The role has to hold for BOTH spellings, exactly as the walk requires and as
            // the subtree walk here already does. A link named `Alias.txt` onto a
            // `Target.bsl` resolves to a key the scan of the TARGET's root does list, but
            // no walk of THIS root ever produces it — so handing it over would report a
            // file from outside the workspace as drift inside it, and the point stream and
            // the walk would describe two different file universes.
            if project_model::file_role(&resolved) != project_model::file_role(path) {
                return None;
            }
            Some((resolved, ChangeKind::MaybeChanged))
        }
        Ok(_) => None,
        // Only an actual absence is a removal. Any other stat error (permissions,
        // interruption, a momentary race) must not tombstone a live file.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // A path that is gone cannot canonicalize, so it is keyed by the same rule
            // every other placement here uses: the longest ancestor that still resolves,
            // plus the rest as spelled. This is what lets a create — keyed by the file's
            // own resolution — coalesce with its later removal under a symlinked root, and
            // it keeps answering when the PARENT went in the same burst, where a rule that
            // resolved only the parent would fall back to the raw spelling and leave the
            // hub naming a removal in a language the scan does not read.
            let canonical = resolve_as_far_as_it_goes(path);
            // A project input is known to be a file whatever its name looks like:
            // `.env` carries no extension, and the extension-less heuristic below
            // would read its removal as a vanished directory and force a full
            // rescan instead of a tombstone.
            let named_like_a_file = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(project_model::is_project_input_file_name);
            if path.extension().is_none() && !named_like_a_file {
                Some((canonical, ChangeKind::SubtreeRemoved))
            } else {
                Some((canonical, ChangeKind::MaybeRemoved))
            }
        }
        Err(_) => None,
    }
}

/// Messages the hub thread processes. Control travels the SAME channel as watcher
/// events, so a re-arm is ordered relative to the event stream and executed by the
/// one thread that owns the watcher — no cross-thread watcher mutation, no second
/// hub identity for the (many, clonable) handle holders to migrate to.
enum HubMsg {
    Event(Result<Event, notify::Error>),
    /// Declare the watch set (see [`WorkspaceChangeHub::rearm_with_epoch`]). `ack` fires once the
    /// declaration is applied; it carries whether EVERY desired target is actually armed
    /// (partial coverage must surface to the caller, not read as success). What the
    /// declaration costs — nothing, a record, or a re-arm and the reconcile that comes with
    /// it — is decided by the thread in [`apply_declaration`], never by the sender.
    Rearm {
        targets: Vec<WatchTarget>,
        /// The age of the composition this declaration speaks for ([`next_topology_epoch`]):
        /// an older declaration is ignored rather than allowed to roll the hub back onto the
        /// roots a newer build left behind (github#184).
        epoch: u64,
        ack: std::sync::mpsc::SyncSender<bool>,
    },
    /// Replace the user's `[source].exclude` and re-place every registration under it.
    /// `ack` carries whether every declared target is armed afterwards. `epoch` ages like
    /// [`HubMsg::Rearm`]'s: a moved exclusion set is a composition decision too, and an old
    /// one must not undo a newer one.
    Rescope {
        user_excluded: ExcludedPaths,
        epoch: u64,
        ack: std::sync::mpsc::SyncSender<bool>,
    },
    /// Run one coverage tick now. A test seam: production drives ticks by the
    /// deadline, and both paths call the same function so a broken periodic path
    /// cannot hide behind a working commanded one.
    #[cfg(test)]
    Tick,
    /// Exit the hub thread. Cursors keep draining the frozen stream. Sent by
    /// [`HubThread::stop`], which the last handle's `Drop` reaches like any other caller.
    Shutdown,
}

/// Daemon-owned hub over one recursive workspace watcher. Cheap to clone
/// (`Arc`-backed); every clone observes the same accumulator and health.
#[derive(Clone)]
pub(crate) struct WorkspaceChangeHub {
    inner: Arc<HubInner>,
    /// The hub thread: reached for control messages through [`Self::control`], and stopped
    /// when the last handle to it goes.
    thread: Arc<HubThread>,
}

/// The hub thread's lifetime, tied to the handles that can still reach it: the last
/// clone out stops it. Without that, every hub whose handles are dropped leaves a live
/// thread and its watcher behind for the life of the process — a per-hub inotify
/// instance against a per-uid quota that is measured in dozens.
struct HubThread {
    /// Producer side of the thread's channel, for control messages — the stop among them.
    /// The watcher callback holds its own clone for events.
    control: std::sync::mpsc::SyncSender<HubMsg>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The blind-root poller's stop, raised with the hub thread's (see [`BlindPoll`]).
    blind_stop: Arc<StopFlag>,
}

impl HubThread {
    /// Ask the thread to exit, then join it. Idempotent, and bounded END TO END —
    /// enqueue AND exit share one deadline.
    ///
    /// The exit is waited for rather than joined into: the stop message is read in the
    /// thread's message loop, and a thread held short of arming by a seam has not reached
    /// that loop, so it never reads the message at all. An unconditional `join` would then
    /// hold the dropping thread for ever, which in a test binary is not a failure but a
    /// hang — the one outcome a run cannot report. Past the deadline the thread is left
    /// detached instead: a leaked thread is visible in the warning, a wedged process is
    /// visible as nothing.
    fn stop(&self) {
        let deadline = Instant::now() + STOP_BUDGET;
        let mut msg = HubMsg::Shutdown;
        let sent = loop {
            match self.control.try_send(msg) {
                Ok(()) => break true,
                Err(std::sync::mpsc::TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        break false;
                    }
                    msg = back;
                    std::thread::sleep(STOP_POLL);
                }
                // Already gone: waiting below is safe and immediate.
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break true,
            }
        };
        if !sent {
            tracing::warn!("change hub shutdown could not be enqueued; leaving the thread running");
            return;
        }
        // Taken out either way: a thread this call does not manage to join is one nobody
        // else can join later, and leaving the handle behind would only invite a second
        // caller to wait out the same budget again.
        let Some(handle) = self.handle.lock().unwrap_or_else(PoisonError::into_inner).take() else {
            return;
        };
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!(
                    "workspace change hub thread did not exit within the stop budget; leaving it detached"
                );
                return;
            }
            std::thread::sleep(STOP_POLL);
        }
        let _ = handle.join();
    }
}

impl HubThread {
    /// Stop everything this hub runs: the blind poll and the thread itself.
    fn stop_all(&self) {
        self.blind_stop.raise();
        self.stop();
    }
}

impl Drop for HubThread {
    fn drop(&mut self) {
        self.stop_all();
    }
}

impl WorkspaceChangeHub {
    /// The control channel. One sender for the whole hub, owned by the thread's own record:
    /// a second copy on the handle would be a second thing to keep in step with it.
    fn control(&self) -> &std::sync::mpsc::SyncSender<HubMsg> {
        &self.thread.control
    }

    /// Spawn the hub over one or more roots (the drift-scan universe: the config source
    /// root plus each extension root). Returns immediately — each root is watched
    /// recursively on the hub thread (walking large trees must not block daemon startup).
    /// Nested roots are de-duplicated so a subtree is not double-watched. Use
    /// [`Self::wait_until_watching`] / [`Self::is_watching`] to observe setup completion;
    /// [`Self::health`] reports `Degraded(WatcherSetup)` if no root could be watched.
    /// Test constructor: every root recursive. Production spawns via
    /// [`Self::start_targets`] so the workspace root rides along non-recursively.
    #[cfg(test)]
    pub(crate) fn start(roots: Vec<PathBuf>) -> Self {
        Self::start_targets(roots.into_iter().map(WatchTarget::recursive).collect())
    }

    /// [`Self::start`] with explicit per-target modes (see [`watch_targets_for`]).
    #[cfg(test)]
    pub(crate) fn start_targets(targets: Vec<WatchTarget>) -> Self {
        Self::start_targets_excluding(targets, Vec::new())
    }

    /// [`Self::start_targets`] with subtrees the hub must not speak for.
    ///
    /// `excluded` is fixed here and nowhere else: see [`HubInner::excluded`] for why a
    /// re-arm must not be able to change it. Each path is taken in both the spelling
    /// given and its canonical form, because an event names whichever of the two the
    /// watch was armed with.
    #[cfg(test)]
    pub(crate) fn start_targets_excluding(
        targets: Vec<WatchTarget>,
        excluded: Vec<PathBuf>,
    ) -> Self {
        Self::start_targets_scoped(targets, excluded, ExcludedPaths::default())
    }

    /// [`Self::start_targets_excluding`] over a project with a `[source].exclude`: no
    /// registration reaches into those directories, and events from them are dropped.
    pub(crate) fn start_targets_scoped(
        targets: Vec<WatchTarget>,
        excluded: Vec<PathBuf>,
        user_excluded: ExcludedPaths,
    ) -> Self {
        #[cfg(test)]
        if let Some(poll) = POLL_INSTEAD_OF_WATCHING.with(std::cell::Cell::get) {
            return Self::start_seamed_scoped(
                targets,
                DEFAULT_CAPACITY,
                COVERAGE_TICK_PERIOD,
                false,
                None,
                Some(refuse_every_watch()),
                excluded,
                user_excluded,
                poll,
                BlindPollSeam::default(),
            );
        }
        Self::start_seamed_scoped(
            targets,
            DEFAULT_CAPACITY,
            COVERAGE_TICK_PERIOD,
            false,
            None,
            None,
            excluded,
            user_excluded,
            PollConfig::PRODUCTION,
            #[cfg(test)]
            BlindPollSeam::default(),
        )
    }

    /// A hub whose thread the operating system refused to start.
    #[cfg(test)]
    pub(crate) fn start_with_unstartable_thread(targets: Vec<WatchTarget>) -> Self {
        Self::start_seamed(
            targets,
            DEFAULT_CAPACITY,
            COVERAGE_TICK_PERIOD,
            true,
            None,
            None,
            Vec::new(),
            PollConfig::PRODUCTION,
            #[cfg(test)]
            BlindPollSeam::default(),
        )
    }

    /// A hub held just short of arming until the returned guard is released or dropped, so
    /// a consumer can be observed waiting on a hub that is alive and not yet ready. Once
    /// released it arms for real, which is what makes it a control and not a stub: the
    /// consumer's work after the wait has to actually happen.
    #[cfg(test)]
    pub(crate) fn start_targets_held(targets: Vec<WatchTarget>) -> (Self, HubHoldGuard) {
        let hold = Arc::new(HubHold::new());
        let gate = Arc::clone(&hold);
        let hub = Self::start_seamed(
            targets,
            DEFAULT_CAPACITY,
            COVERAGE_TICK_PERIOD,
            false,
            Some(Arc::new(move || gate.wait())),
            None,
            Vec::new(),
            PollConfig::PRODUCTION,
            #[cfg(test)]
            BlindPollSeam::default(),
        );
        (hub, HubHoldGuard(hold))
    }

    /// [`Self::start_targets`] with a tick interval a test can actually wait for.
    #[cfg(test)]
    pub(crate) fn start_targets_with_period(targets: Vec<WatchTarget>, period: Duration) -> Self {
        Self::start_with_capacity(targets, DEFAULT_CAPACITY, period)
    }

    /// A hub that refuses to arm the declared paths, for a test that needs a root nothing
    /// watches. The refusal is declared BEFORE the thread starts: an ordinary hub arms
    /// within milliseconds, so a refusal installed afterwards would race the arming it is
    /// meant to prevent.
    #[cfg(all(test, unix))]
    pub(crate) fn start_targets_refusing(
        targets: Vec<WatchTarget>,
        period: Duration,
        refusals: &Arc<RefusedWatches>,
    ) -> Self {
        Self::start_seamed(
            targets,
            DEFAULT_CAPACITY,
            period,
            false,
            None,
            Some(refusals.as_refusal()),
            Vec::new(),
            PollConfig::PRODUCTION,
            #[cfg(test)]
            BlindPollSeam::default(),
        )
    }

    /// [`Self::start_targets_refusing_polled`] whose blind poll cannot start its thread.
    #[cfg(all(test, unix))]
    pub(crate) fn start_targets_refusing_unpollable(
        targets: Vec<WatchTarget>,
        period: Duration,
        refusals: &Arc<RefusedWatches>,
        poll: PollConfig,
    ) -> Self {
        Self::start_seamed(
            targets,
            DEFAULT_CAPACITY,
            period,
            false,
            None,
            Some(refusals.as_refusal()),
            Vec::new(),
            poll,
            BlindPollSeam::refusing_to_start(),
        )
    }

    /// [`Self::start_targets_refusing_polled`] whose blind polls each wait for `gate`.
    #[cfg(all(test, unix))]
    pub(crate) fn start_targets_refusing_polled_gated(
        targets: Vec<WatchTarget>,
        period: Duration,
        refusals: &Arc<RefusedWatches>,
        poll: PollConfig,
        gate: Arc<PollGate>,
        announce: Option<Arc<AnnounceBarrier>>,
    ) -> Self {
        Self::start_seamed(
            targets,
            DEFAULT_CAPACITY,
            period,
            false,
            None,
            Some(refusals.as_refusal()),
            Vec::new(),
            poll,
            BlindPollSeam { cannot_start: false, gate: Some(gate), announce },
        )
    }

    /// [`Self::start_targets_refusing`] that polls its blind roots on `poll`'s schedule.
    #[cfg(all(test, unix))]
    pub(crate) fn start_targets_refusing_polled(
        targets: Vec<WatchTarget>,
        period: Duration,
        refusals: &Arc<RefusedWatches>,
        poll: PollConfig,
    ) -> Self {
        Self::start_seamed(
            targets,
            DEFAULT_CAPACITY,
            period,
            false,
            None,
            Some(refusals.as_refusal()),
            Vec::new(),
            poll,
            #[cfg(test)]
            BlindPollSeam::default(),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_capacity(
        targets: Vec<WatchTarget>,
        cap: usize,
        tick_period: Duration,
    ) -> Self {
        Self::start_seamed(
            targets,
            cap,
            tick_period,
            false,
            None,
            None,
            Vec::new(),
            PollConfig::PRODUCTION,
            #[cfg(test)]
            BlindPollSeam::default(),
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn start_seamed(
        targets: Vec<WatchTarget>,
        cap: usize,
        tick_period: Duration,
        refuse_spawn: bool,
        before_arm: Option<BeforeArm>,
        watch_refusal: Option<WatchRefusal>,
        excluded: Vec<PathBuf>,
        poll: PollConfig,
        #[cfg(test)] blind_poll_seam: BlindPollSeam,
    ) -> Self {
        Self::start_seamed_scoped(
            targets,
            cap,
            tick_period,
            refuse_spawn,
            before_arm,
            watch_refusal,
            excluded,
            ExcludedPaths::default(),
            poll,
            #[cfg(test)]
            blind_poll_seam,
        )
    }

    /// The hub with its startup seams exposed. Production passes `false`, two `None`s and
    /// [`PollConfig::PRODUCTION`].
    ///
    /// Each exists because the state it produces cannot be provoked on demand and is a
    /// state no other door leads to. `refuse_spawn`: an operating system refusing a
    /// thread — every other permanent failure is reported through a different path, so
    /// the wiring that turns THIS one into a report is otherwise unreachable.
    /// `before_arm`: a hub alive, not yet armed and not failed — what a huge initial walk
    /// looks like, and the only readiness answer that means "ask again later".
    /// `watch_refusal`: a root the watch will not take, which no file system produces for
    /// every uid alike (see [`Watch`]). `poll`: the fallback poll's clock, which a test runs
    /// by hand rather than waiting a production period out.
    // Each seam is a state no other door leads to, and a bag struct would carry exactly these
    // same arguments under one more name.
    #[allow(clippy::too_many_arguments)]
    fn start_seamed_scoped(
        targets: Vec<WatchTarget>,
        cap: usize,
        tick_period: Duration,
        refuse_spawn: bool,
        before_arm: Option<BeforeArm>,
        watch_refusal: Option<WatchRefusal>,
        excluded: Vec<PathBuf>,
        user_excluded: ExcludedPaths,
        poll: PollConfig,
        #[cfg(test)] blind_poll_seam: BlindPollSeam,
    ) -> Self {
        let placed = ResolvedTargets::here(targets.clone());

        let inner = Arc::new(HubInner {
            acc: Mutex::new(Accumulator::new(cap)),
            wake: Condvar::new(),
            notifications: AtomicU64::new(0),
            watching: AtomicBool::new(false),
            channel_overflow: AtomicBool::new(false),
            watched_roots: Mutex::new(Vec::new()),
            // A starting value only; the hub thread re-derives it right before
            // arming, so the relative spellings are resolved against the same
            // current directory the backend will use.
            scope: Mutex::new(Scope::from_targets_scoped(&placed, &excluded, &user_excluded)),
            excluded,
            user_excluded: Arc::new(RwLock::new(user_excluded)),
            excluded_events: AtomicU64::new(0),
            tick_period,
            ticks: AtomicU64::new(0),
            rearms: AtomicU64::new(0),
            poll,
            polling: AtomicBool::new(false),
            poll_state: Mutex::new(PollStatus::default()),
            poll_expected_since: Mutex::new(None),
            blind_poll: BlindPoll {
                #[cfg(test)]
                cannot_start: blind_poll_seam.cannot_start,
                #[cfg(test)]
                gate: blind_poll_seam.gate,
                #[cfg(test)]
                announce: blind_poll_seam.announce,
                ..BlindPoll::default()
            },
            // Placed, like every later declaration: the record is compared against
            // those, and a raw spelling would never equal its own placed form.
            declared_published: Mutex::new(placed.as_slice().to_vec()),
            blind_targets: Mutex::new(Vec::new()),
        });
        let (tx, rx) = std::sync::mpsc::sync_channel::<HubMsg>(CHANNEL_BOUND);

        let thread_inner = Arc::clone(&inner);
        let event_tx = tx.clone();
        let spawned = if refuse_spawn {
            Err(std::io::Error::other("hub thread spawn refused by test seam"))
        } else {
            std::thread::Builder::new().name("bsl-workspace-change-hub".to_owned()).spawn(
                move || {
                    run_hub_thread(thread_inner, targets, event_tx, rx, before_arm, watch_refusal)
                },
            )
        };
        // A hub whose thread never started arms nothing, ever. Dropping the error would
        // leave `watching` false and `setup_failed` unset — the one state that reads as
        // "still starting", so every consumer would wait out its whole readiness budget
        // and then take the slow path anyway, having learnt nothing.
        let thread = match spawned {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::error!("workspace change hub thread could not start: {error}");
                inner.mark_setup_failed();
                None
            }
        };

        let blind_stop = Arc::clone(&inner.blind_poll.stop);
        Self {
            inner,
            thread: Arc::new(HubThread { control: tx, handle: Mutex::new(thread), blind_stop }),
        }
    }

    /// Declare `targets` as of now — a test-side convenience. Production always states the
    /// age of its composition ([`Self::rearm_with_epoch`]), because a declaration with no
    /// age cannot be ordered against the one in force (github#184).
    #[cfg(test)]
    pub(crate) fn rearm(&self, targets: Vec<WatchTarget>, timeout: Duration) -> bool {
        self.rearm_with_epoch(targets, next_topology_epoch(), timeout)
    }

    /// Ask the hub thread to re-point the watch set at `targets`, blocking until it
    /// acknowledges or `timeout` elapses. The hub identity is stable across a
    /// re-arm: cursors, health and all clonable handles keep working — only the
    /// covered subtrees change. `timeout` bounds the WHOLE call: the enqueue onto
    /// a possibly-full channel and the wait for the acknowledgement share one
    /// deadline. Returns whether every desired target is actually armed; `false`
    /// for a timeout, a dead hub thread, or partial coverage (an unwatchable
    /// target) — the caller must not treat any of those as covered.
    ///
    /// `epoch` is the age of the composition that chose `targets` ([`next_topology_epoch`]).
    /// A caller whose snapshot predates a newer one must say so, or the thread cannot tell
    /// its declaration from a fresh one and a slow build finishing last would roll the hub
    /// back onto the roots the newer build left (github#184).
    pub(crate) fn rearm_with_epoch(
        &self,
        targets: Vec<WatchTarget>,
        epoch: u64,
        timeout: Duration,
    ) -> bool {
        self.handshake(|ack| HubMsg::Rearm { targets, epoch, ack }, timeout)
    }

    /// Send the message `build` makes around an acknowledgement channel and wait for the
    /// hub thread's answer; `timeout` bounds the enqueue and the wait together.
    fn handshake(
        &self,
        build: impl FnOnce(std::sync::mpsc::SyncSender<bool>) -> HubMsg,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        let mut msg = build(ack_tx);
        loop {
            match self.control().try_send(msg) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    msg = back;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        ack_rx.recv_timeout(remaining).unwrap_or(false)
    }

    /// Declare `targets` to the hub, and say whether the hub covers them.
    ///
    /// The comparison is against the declaration the thread STANDS on, never against the
    /// armed set. A target the backend could not take — a blind root, a directory that does
    /// not exist — is a gap the hub repairs on its own schedule; measuring it here would make
    /// every caller re-declare, every re-declaration re-arm, and every re-arm hand each
    /// consumer a reconcile. Each such reconcile makes the graph rebuild, and each rebuild
    /// calls this again: a loop with no external cause, which is why the repeat has to cost
    /// nothing at all rather than merely little.
    ///
    /// An unchanged declaration therefore moves nothing — no re-arm, no reconcile — and
    /// answers from what the hub already holds. It still reaches the hub with its AGE, best
    /// effort and without waiting ([`Self::remark`]): a composition that agrees with the stand
    /// keeps the hub's mark moving, which is what refuses a still older, DIFFERENT
    /// declaration arriving after it (github#184).
    ///
    /// The form without an age is a test-side convenience: production states the age of the
    /// composition it speaks for ([`Self::ensure_roots_with_epoch`]).
    #[cfg(test)]
    pub(crate) fn ensure_roots(&self, targets: &[WatchTarget]) -> bool {
        self.ensure_roots_with_epoch(targets, next_topology_epoch())
    }

    /// [`Self::ensure_roots`] under the epoch of the composition that produced `targets`:
    /// the sender states how old the world it speaks for is, and the thread refuses to
    /// apply a declaration older than the one it already stands on (github#184).
    pub(crate) fn ensure_roots_with_epoch(&self, targets: &[WatchTarget], epoch: u64) -> bool {
        // Resolved before comparing, exactly as the hub thread will: the declaration travels
        // and is remembered in PLACED spellings, and one relative spelling names two
        // different targets under two different current directories.
        let resolved = ResolvedTargets::here(targets.to_vec());
        let declaration = resolved.as_slice().to_vec();
        if same_declaration(&self.inner.accepted_declaration(), &declaration) {
            // Nothing is waited on — that is the point of the barrier — but the declaration
            // still travels with its AGE: the mark has to hear that a newer composition
            // agreed with the stand, or a still older, DIFFERENT declaration would apply
            // after it and take the hub off a world the newer one had confirmed
            // (github#184). Sent once, without waiting: the mark is best effort, and a full
            // channel under an event storm must not delay a caller that used to send nothing
            // at all.
            self.remark(targets.to_vec(), epoch);
            // The rest of the answer is about coverage, and coverage is not knowable while
            // the watch is still being armed: the accepted declaration is recorded before
            // the thread has armed anything, so a caller asking in that window would be
            // told "not covered" about a watch that arms a moment later. Waiting for the
            // hub to settle costs the arming time once, which is what the acknowledgement
            // of a re-arm used to cost anyway.
            let _ = self.watch_readiness_or(REARM_ACK_TIMEOUT, || false);
            return self.covers(&resolved);
        }
        tracing::info!(?targets, "workspace change hub declaring new scan roots");
        self.rearm_with_epoch(targets.to_vec(), epoch, REARM_ACK_TIMEOUT)
    }

    /// Send an unchanged declaration as what it is — a declaration, at its age — without
    /// waiting for an answer. "Unchanged" was read on the caller's thread, and the stand can
    /// move before the thread hears it; `apply_declaration` weighs it against what stands
    /// when it arrives, so an agreeing word costs nothing there and an overtaken one applies.
    fn remark(&self, targets: Vec<WatchTarget>, epoch: u64) {
        // Nobody waits on the answer: the receiver is dropped here, and the thread's
        // `try_send` of the acknowledgement fails quietly.
        let (ack, _) = std::sync::mpsc::sync_channel(1);
        let _ = self.control().try_send(HubMsg::Rearm { targets, epoch, ack });
    }

    /// [`Self::ensure_roots`] together with the user's `[source].exclude` the roots were
    /// resolved under. Unchanged exclusions cost what `ensure_roots` costs; changed ones
    /// re-place every registration, because the same roots no longer mean the same
    /// coverage, and owe consumers the reconcile any re-arm owes. `epoch` is the age of the
    /// composition both were taken from, and rides both declarations.
    pub(crate) fn ensure_scope(
        &self,
        targets: &[WatchTarget],
        user_excluded: &ExcludedPaths,
        epoch: u64,
    ) -> bool {
        // The roots are declared whatever the rescope answered: a hub that polls answers
        // "not covered" to everything, and the new roots must reach it all the same.
        let rescoped = self.inner.user_excluded() == *user_excluded || {
            tracing::info!("workspace change hub re-scoping to a new [source].exclude");
            self.rescope(user_excluded.clone(), epoch, REARM_ACK_TIMEOUT)
        };
        let covered = self.ensure_roots_with_epoch(targets, epoch);
        rescoped && covered
    }

    fn rescope(&self, user_excluded: ExcludedPaths, epoch: u64, timeout: Duration) -> bool {
        self.handshake(|ack| HubMsg::Rescope { user_excluded, epoch, ack }, timeout)
    }

    /// Whether every declared target is placed and armed right now. A verdict read off what
    /// the hub already holds — it starts nothing.
    fn covers(&self, resolved: &ResolvedTargets) -> bool {
        if !resolved.is_complete() {
            return false;
        }
        let mut desired: Vec<(PathBuf, bool)> = dedup_targets(resolved.as_slice().to_vec())
            .into_iter()
            .map(|(target, canonical)| (canonical, target.recursive))
            .collect();
        desired.sort();
        let mut current =
            self.inner.watched_roots.lock().unwrap_or_else(PoisonError::into_inner).clone();
        current.sort();
        current == desired
    }

    /// The targets this hub currently stands declared on — what it was ASKED to
    /// watch, not what the watcher managed to take. The distinction is the point:
    /// an unwatchable root leaves the declaration alone, so a reader of this set
    /// sees its caller's intent even on a machine whose inotify limit is spent.
    #[cfg(test)]
    pub(crate) fn declared_targets(&self) -> Vec<WatchTarget> {
        self.inner.declared_published.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Terminate the hub's threads and join what can be joined. Cursors keep draining
    /// whatever was accumulated; no further events arrive. Idempotent, and reached from two
    /// directions: explicitly by the daemon's shutdown, and from the last handle's [`Drop`].
    ///
    /// `closing` FIRST, and that is the point: an owner parked in `wait_for_change_or`
    /// returns on a new generation, on `closing`, or on its own predicate — never on a bare
    /// wake — so stopping the threads without it would leave that owner asleep for its whole
    /// timeout over a hub that has already gone. The blind poll is raised here too: its stop
    /// flag otherwise lives only in `Drop`, and a daemon that shuts down while a handle is
    /// still held would keep walking blind roots.
    pub(crate) fn shutdown(&self) {
        self.interrupt_waiters();
        self.thread.stop_all();
    }

    /// Register a cursor positioned at "everything up to now already seen": a fresh
    /// subscriber only receives changes that land after it subscribes (plus a
    /// pending reconcile flag if it subscribes during an open rescan window, or while
    /// a declared root is unwatched — the window closes as soon as the cursors that
    /// existed at the time acknowledge it, and the blindness it announced does not).
    /// Wake every waiter for good: [`Self::wait_for_change`] and [`Self::watch_readiness`]
    /// return at once from now on. Called by the daemon's shutdown so no owner sleeps out a
    /// hub wait after being asked to stop.
    pub(crate) fn interrupt_waiters(&self) {
        self.inner.lock_acc().closing = true;
        self.inner.notify();
    }

    /// The number of the latest fact the hub has taken in. Monotonic; a consumer reads it
    /// before it looks at disk to say which facts that look already covers.
    pub(crate) fn seq(&self) -> u64 {
        self.inner.lock_acc().max_seq()
    }

    /// Wake every waiter to re-check its own condition, without announcing new work: a
    /// waiter on the generation alone goes straight back to sleep.
    pub(crate) fn wake_waiters(&self) {
        let _acc = self.inner.lock_acc();
        self.inner.wake.notify_all();
    }

    /// The hub could not set up a watch and polls the targets instead.
    pub(crate) fn is_polling(&self) -> bool {
        self.inner.polling.load(Ordering::SeqCst)
    }

    /// Bounded telemetry sample: polling, cycle seconds, and overdue qualification.
    pub(crate) fn try_poll_status(&self) -> Option<(bool, Option<u64>, bool)> {
        let polling = self.is_polling() || !self.inner.blind_targets.try_lock().ok()?.is_empty();
        if !polling {
            return Some((false, None, false));
        }
        let state = *self.inner.poll_state.try_lock().ok()?;
        let expected = *self.inner.poll_expected_since.try_lock().ok()?;
        let passes = state.bytes.div_ceil(self.inner.poll.verify_bytes.max(1)).max(1);
        let ticks = passes.saturating_mul(2).saturating_add(3);
        let cycle = self.inner.poll.period.saturating_mul(u32::try_from(ticks).unwrap_or(u32::MAX));
        let owed_since = match (state.last, expected) {
            (Some(last), Some(expected)) => Some(last.max(expected)),
            (last, expected) => last.or(expected),
        };
        let overdue =
            owed_since.is_some_and(|since| since.elapsed() > self.poll_period().saturating_mul(2));
        Some((true, Some(cycle.as_secs()), overdue))
    }

    /// How old the fallback poll's last walk is, and how long an edit that kept its size and
    /// mtime can go unnoticed. `None` while nothing is polled.
    ///
    /// The bound is CONSERVATIVE, and one pass over every polled byte is not it. Verification
    /// reads a budget of bytes per tick and resumes a file where it stopped; a partial read
    /// keeps what it already has while the file's own stamp is unchanged, and the digest is
    /// compared only once the file has been read through. So an edit landing behind the offset
    /// a pass has already passed is not in that pass's bytes at all — it is seen by the NEXT
    /// full read, which is why the pass in flight, a whole pass after it, and the ticks that
    /// start and finish them are all inside the number a consumer is told.
    ///
    /// Two qualifications ride with it and cannot be dropped: the polled set has to stay
    /// finite and readable — a file that cannot be read is not verified at all — and the walk's
    /// own I/O is on top of this, since the number counts ticks rather than disk time.
    pub(crate) fn poll_report(&self) -> Option<(Option<Duration>, Duration)> {
        if !self.is_polling() && !self.is_partially_blind() {
            return None;
        }
        let state = *self.inner.poll_state.lock().unwrap_or_else(PoisonError::into_inner);
        let passes = state.bytes.div_ceil(self.inner.poll.verify_bytes.max(1)).max(1);
        let ticks = passes.saturating_mul(2).saturating_add(3);
        let cycle = self.inner.poll.period.saturating_mul(u32::try_from(ticks).unwrap_or(u32::MAX));
        Some((state.last.map(|last| last.elapsed()), cycle))
    }

    /// Some declared root exists and nothing watches it: it is found by polling.
    pub(crate) fn is_partially_blind(&self) -> bool {
        self.inner.is_partially_blind()
    }

    /// Whether the hub polls — all of the workspace, or its blind roots — and its last walk
    /// is older than twice the poll period: a poll that stopped happening vouches for nothing.
    pub(crate) fn poll_overdue(&self) -> bool {
        if !self.is_polling() && !self.is_partially_blind() {
            return false;
        }
        let last = self.inner.poll_state.lock().unwrap_or_else(PoisonError::into_inner).last;
        // A poll that was promised and never made is overdue exactly like one that stopped:
        // the thread that would have made it may have failed to start, and reading "never
        // walked" as "nothing to report" would vouch for a tree nobody has looked at.
        let owed_since = match (last, self.inner.poll_expected_since()) {
            (Some(last), Some(expected)) => Some(last.max(expected)),
            (last, expected) => last.or(expected),
        };
        owed_since.is_some_and(|since| since.elapsed() > self.poll_period().saturating_mul(2))
    }

    /// The configured poll period, for a reader deciding whether a poll is overdue.
    pub(crate) fn poll_period(&self) -> Duration {
        self.inner.poll.period
    }

    /// A hub that can watch nothing and polls on `poll`'s schedule, held short of setup
    /// until the guard releases it so a test can subscribe first.
    #[cfg(test)]
    pub(crate) fn start_polling(
        targets: Vec<WatchTarget>,
        poll: PollConfig,
    ) -> (Self, HubHoldGuard) {
        let hold = Arc::new(HubHold::new());
        let gate = Arc::clone(&hold);
        let hub = Self::start_seamed(
            targets,
            DEFAULT_CAPACITY,
            COVERAGE_TICK_PERIOD,
            false,
            Some(Arc::new(move || gate.wait())),
            Some(refuse_every_watch()),
            Vec::new(),
            poll,
            #[cfg(test)]
            BlindPollSeam::default(),
        );
        (hub, HubHoldGuard(hold))
    }

    /// Whether `cursor` has been told to reconcile, without acknowledging anything.
    #[cfg(test)]
    pub(crate) fn drain_peek(&self, cursor: SinkCursor) -> bool {
        self.materialize(cursor).rescan_required
    }

    /// Run one poll now and wait for it to finish.
    #[cfg(test)]
    pub(crate) fn poll_now(&self) -> bool {
        let polls = || self.inner.poll_state.lock().unwrap_or_else(PoisonError::into_inner).polls;
        let before = polls();
        if self.control().send(HubMsg::Tick).is_err() {
            return false;
        }
        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(5),
        );
        while patience.allows() {
            if polls() > before {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Wait until the reconcile announcing the current blindness has been issued — it follows
    /// the first reading of every blind file.
    #[cfg(all(test, unix))]
    pub(crate) fn wait_until_blindness_announced(&self) {
        assert!(
            test_support::eventually(|| {
                !self.inner.blind_poll.reconcile_pending.load(Ordering::SeqCst)
            }),
            "the reconcile announcing the blind root never came",
        );
    }

    /// Whether [`Self::interrupt_waiters`] has run.
    pub(crate) fn is_closing(&self) -> bool {
        self.inner.lock_acc().closing
    }

    pub(crate) fn subscribe(&self) -> SinkCursor {
        // Read before taking the accumulator, so no path holds one of the two locks
        // while asking for the other.
        let blind = self.inner.is_blind_and_announced().then_some(DegradeReason::RewatchFailed);
        let id = self.inner.lock_acc().subscribe(blind.clone());
        // And read AGAIN, because those two lines are not one moment. A blind set published
        // between them belongs to a hub thread that flagged every cursor it could see — and
        // this one was not there yet — so without the second read a consumer walks away
        // clean over a declared root nothing is watching. Published before the flag, so the
        // orders that matter are covered both ways: either the flag found this cursor, or
        // this finds the publication.
        if blind.is_none() && self.inner.is_blind_and_announced() {
            self.inner.lock_acc().force_rescan(id, DegradeReason::RewatchFailed);
        }
        SinkCursor { id }
    }

    /// Replace one consumer's cursor, carrying whatever it still owes onto the new one, and
    /// report the PAIR that names that debt when there was one.
    ///
    /// Re-subscribing is not leaving. A consumer does it to take a fresh baseline — the
    /// resident does exactly this at the start of a rebuild — and that rebuild can fail,
    /// leaving the old state still being served. Letting the debt die with the old cursor
    /// would be settling it against a baseline that was never taken: the events the window
    /// was opened for are gone, and the only record that they were missed would go with it.
    ///
    /// The token is what a caller that DID take the baseline hands to
    /// [`Self::settle_carried_debt`] once its build succeeds. It is captured under the SAME
    /// hold of the accumulator that performs the carry: the loss is the number `carry_debt`
    /// wrote onto the fresh cursor, and the position is the one the fresh subscription gave
    /// it. Read back after `drop(acc)` instead, the loss could be renamed by a window raised
    /// in the gap, and the token would name a window this rebuild never answered; the second
    /// blind re-arm below moves neither (it writes only when nothing is owed), so it does not
    /// disturb the captured pair.
    pub(crate) fn resubscribe(&self, cursor: SinkCursor) -> (SinkCursor, Option<CarriedDebt>) {
        // Read before taking the accumulator, so no path holds one of the two locks while
        // asking for the other.
        let blind = self.inner.is_blind_and_announced().then_some(DegradeReason::RewatchFailed);
        let mut acc = self.inner.lock_acc();
        let carried = acc.debt_of(cursor.id);
        let delivered = acc.cursors.get(&cursor.id).and_then(|cursor| cursor.delivered);
        acc.unsubscribe(cursor.id);
        let (reason, loss) = carried.map_or((None, None), |(reason, loss)| (Some(reason), loss));
        let id = acc.subscribe(reason.or(blind.clone()));
        acc.carry_debt(id, loss, delivered);
        // Captured HERE, under the same hold as the carry: `loss` is what `carry_debt` wrote
        // and `pos` is where the fresh subscription put the cursor. Taking either back after
        // `drop(acc)` would let a window raised in the gap rename the loss and turn the token
        // into one for a reconcile this rebuild never answered.
        // Infallible: `subscribe` above inserted this cursor under the same hold, so the
        // `None` arm of a silent fallback (a fabricated position) is unreachable — and were
        // it ever reached, a fabricated position would make the token read as settled against
        // a baseline that never covered it: a false SETTLE, the direction that loses events.
        let pos = acc.cursors.get(&id).map(|cursor| cursor.pos).expect("just subscribed");
        let token = loss.map(|loss| CarriedDebt { loss, pos });
        drop(acc);
        // The same second read as in [`Self::subscribe`], for the same gap between the two
        // locks. It moves neither half of the captured pair: a blind re-arm writes only onto a
        // cursor that owes nothing, and this one carries a debt.
        if blind.is_none() && self.inner.is_blind_and_announced() {
            self.inner.lock_acc().force_rescan(id, DegradeReason::RewatchFailed);
        }
        (SinkCursor { id }, token)
    }

    /// Settle a debt [`Self::resubscribe`] carried onto `cursor`, if that cursor still holds
    /// exactly that PAIR — the loss AND the position the carry captured. Says whether it was
    /// settled.
    ///
    /// For the caller whose re-subscription has since been justified — a rebuild that read the
    /// tree whole. The pair is the guard: a loss names the window, and the position says no cut
    /// has widened it since (any movement of the position while a debt is live is a `make_room`
    /// cut, and that debt is not this settle's to close); a window raised after the carry under
    /// a new loss is refused too. Conditional on purpose: see
    /// [`Accumulator::settle_carried_debt`].
    pub(crate) fn settle_carried_debt(&self, cursor: SinkCursor, carried: &CarriedDebt) -> bool {
        self.inner.lock_acc().settle_carried_debt(cursor.id, carried)
    }

    /// Drop a cursor and reclaim any entries it was the last to hold back. For a consumer
    /// that is gone; one that is coming back uses [`Self::resubscribe`].
    pub(crate) fn unsubscribe(&self, cursor: SinkCursor) {
        self.inner.lock_acc().unsubscribe(cursor.id);
    }

    /// Return the changes newer than `cursor`'s last position and advance it.
    /// Cursors are independent: draining one never affects another's view.
    pub(crate) fn drain(&self, cursor: SinkCursor) -> DrainBatch {
        self.inner.lock_acc().drain(cursor.id)
    }

    /// Materialize this cursor's next batch without advancing it. The caller acknowledges the
    /// exact checkpoint only after its fenced apply succeeds.
    pub(crate) fn materialize(&self, cursor: SinkCursor) -> DrainBatch {
        self.inner.lock_acc().materialize(cursor.id)
    }

    pub(crate) fn acknowledge(&self, batch: &DrainBatch) {
        self.inner.lock_acc().acknowledge(batch);
    }

    /// What this hub can still deliver of the losses it has issued.
    pub(crate) fn loss_horizon(&self) -> LossHorizon {
        self.inner.lock_acc().loss_horizon()
    }

    /// Reported health: the accumulator's transient reason, or — once that has been
    /// acknowledged away — the standing fact that something declared is unwatched.
    ///
    /// The transient reason wins while it lasts because it is the more urgent of the
    /// two: it names an unread window, whereas blindness names a subtree the consumer's
    /// own periodic scan already covers. Nothing here holds one lock while taking the
    /// other; the accumulator guard is released by the end of its own statement.
    pub(crate) fn health(&self) -> Health {
        let health = self.inner.lock_acc().health();
        match health {
            Health::Healthy if self.inner.is_partially_blind() => {
                Health::Degraded(DegradeReason::RewatchFailed)
            }
            health => health,
        }
    }

    /// Health as it concerns ONE consumer, for deciding between the event stream and a
    /// full scan.
    ///
    /// The hub's own condition is everyone's: a watch that never armed, or a declared
    /// root nothing is watching, means the stream is incomplete no matter who is asking.
    /// An outstanding reconcile debt is not — it belongs to the cursor that owes it, and
    /// answering it to everybody is how one consumer that stopped draining used to put
    /// every other consumer on the slow path for the life of the daemon.
    ///
    /// [`Self::health`] stays for reporting the hub itself.
    pub(crate) fn health_for(&self, cursor: Option<SinkCursor>) -> Health {
        let health = self.inner.lock_acc().health_for(cursor.map(|cursor| cursor.id));
        match health {
            Health::Healthy if self.inner.is_partially_blind() => {
                Health::Degraded(DegradeReason::RewatchFailed)
            }
            health => health,
        }
    }

    pub(crate) fn events_seen(&self) -> u64 {
        self.inner.lock_acc().events_seen
    }

    /// Ask every live cursor to reconcile: used by a consumer whose periodic scan
    /// found drift the event stream never delivered (a lossy backend). Recoverable
    /// like any other transient miss — health clears once all cursors acknowledge.
    /// Entries are kept; the caller applies the drift it already found.
    pub(crate) fn degrade_external(&self) {
        self.inner.lock_acc().enter_rescan(false, DegradeReason::ReconcileMiss);
        self.inner.notify();
    }

    /// Whether the watch is armed. False means setup is still in flight or failed.
    ///
    /// Point-in-time, for the sink's admission gate (`hub_is_feeding`) and for status
    /// reporting: a caller that has to WAIT for readiness uses [`Self::wait_until_watching`]
    /// instead.
    pub(crate) fn is_watching(&self) -> bool {
        self.inner.watching.load(Ordering::SeqCst)
    }

    /// Block until setup settles (watch armed or failed) or `timeout` elapses, and say
    /// WHICH of the three happened. Sinks call this instead of a bare `is_watching`
    /// check so they do not race the asynchronous setup.
    /// Kept for tests, which drive a hub with no daemon around it. Background code states
    /// its stop: the form without a predicate would let an owner sleep out a whole slice
    /// after it was told to leave.
    #[cfg(test)]
    pub(crate) fn watch_readiness(&self, timeout: Duration) -> WatchReadiness {
        self.watch_readiness_or(timeout, || false)
    }

    /// [`Self::watch_readiness`] that also gives up once `stopped` holds, re-checked on every
    /// wake. An owner whose daemon is leaving has no use for a watch that is still arming, and
    /// the answer to "will this watch arm for me" is then the same `Failed` a hub that cannot
    /// be set up gives. The hub's own `closing` stays the second barrier, not the only one:
    /// whichever of the two is raised first releases the wait.
    pub(crate) fn watch_readiness_or(
        &self,
        timeout: Duration,
        stopped: impl Fn() -> bool,
    ) -> WatchReadiness {
        let deadline = Instant::now() + timeout;
        let mut acc = self.inner.lock_acc();
        loop {
            if self.inner.watching.load(Ordering::SeqCst) {
                return WatchReadiness::Armed;
            }
            if acc.setup_failed || acc.closing || stopped() {
                return WatchReadiness::Failed;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Both re-read at the deadline: either can have settled while the last
                // wait was expiring, and reporting `NotYet` over a settled hub would
                // send the caller back to wait for something that already happened.
                return if self.inner.watching.load(Ordering::SeqCst) {
                    WatchReadiness::Armed
                } else if acc.setup_failed {
                    WatchReadiness::Failed
                } else {
                    WatchReadiness::NotYet
                };
            }
            let (guard, _) = self
                .inner
                .wake
                .wait_timeout(acc, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            acc = guard;
        }
    }

    /// Whether the watch armed. Kept alongside the three-state form because it is what every
    /// test asks: a test drives a hub that arms in milliseconds and has nothing to decide
    /// between "not yet" and "never". The wait is on a condition variable, so it wakes when the
    /// watch arms; the ceiling only bounds a watch that never does.
    #[cfg(test)]
    pub(crate) fn wait_until_watching(&self) -> bool {
        self.watch_readiness(test_support::STATE_CEILING) == WatchReadiness::Armed
    }

    /// The accumulator's generation right now.
    ///
    /// A sink that goes dormant reads this to say what it has already been told about: without
    /// it the generation it waits on is whatever it last happened to observe, and a batch that
    /// arrived before it went dormant would answer as work that arrived after.
    pub(crate) fn generation(&self) -> u64 {
        self.inner.lock_acc().generation
    }

    /// Block until the accumulator advances past `since` or `timeout` elapses,
    /// then return the current generation.
    ///
    /// Kept for tests only, for the same reason as [`Self::watch_readiness`]: a background
    /// sink states the stop it is to leave on, so the predicate-less form has no production
    /// caller left.
    #[cfg(test)]
    pub(crate) fn wait_for_change(&self, since: u64, timeout: Duration) -> u64 {
        self.wait_for_change_or(since, timeout, || false)
    }

    /// [`Self::wait_for_change`] that also returns once `woken` holds, re-checked on every
    /// [`Self::wake_waiters`]. For an owner whose next deadline can move while it sleeps —
    /// work owed by someone else's thread — and who cannot wait out a whole slice for it.
    pub(crate) fn wait_for_change_or(
        &self,
        since: u64,
        timeout: Duration,
        woken: impl Fn() -> bool,
    ) -> u64 {
        let deadline = Instant::now() + timeout;
        let mut acc = self.inner.lock_acc();
        loop {
            if acc.generation > since || acc.closing || woken() {
                return acc.generation;
            }
            // A condition variable may wake without a signal at all, and every
            // signal on this one is shared by every sink. Returning on the wake
            // itself would report "there is work" on a generation that never moved,
            // and the caller's answer to that is a full drain-and-apply pass.
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return acc.generation;
            };
            let (guard, _) = self
                .inner
                .wake
                .wait_timeout(acc, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            acc = guard;
        }
    }

    /// Wakes delivered to sinks so far. See [`HubInner::notifications`].
    #[cfg(test)]
    pub(crate) fn notifications(&self) -> u64 {
        self.inner.notifications.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn ingest_for_test(&self, res: Result<Event, notify::Error>) {
        // Re-watch requests are the caller's job; tests that need real subtree
        // watching drive the live watcher through `start` instead.
        let _rewatch = self.inner.ingest_event(res);
    }

    /// Report `path` as gone, exactly as the backend would, and classify it the way a real
    /// event is classified — the kind is re-derived from the disk, not asserted here.
    ///
    /// For a stand that needs ONE named path in the drain and is not itself about whether
    /// the platform reports it. A real removal is reported at the platform's discretion:
    /// under load FSEvents has been measured naming a directory's descendants while never
    /// naming the directory above them, so a stand that removes a tree and waits for the
    /// entry it wants ends up measuring which events the day's scheduling produced. What
    /// the backend does report is pinned where it belongs, by this module's own tests.
    #[cfg(test)]
    pub(crate) fn deliver_vanished_for_test(&self, path: &Path) {
        self.ingest_for_test(Ok(Event {
            kind: EventKind::Remove(notify::event::RemoveKind::Folder),
            paths: vec![path.to_path_buf()],
            attrs: Default::default(),
        }));
    }

    /// Report a backend error, exactly as the backend would: the stream lapsed, and every
    /// cursor then listening shares the one window that opens.
    #[cfg(test)]
    pub(crate) fn deliver_backend_error_for_test(&self) {
        self.ingest_for_test(Err(notify::Error::generic("the backend lost its stream")));
    }

    /// Number of registered cursors. Used by tests to wait deterministically for a
    /// sink to subscribe instead of sleeping a guessed interval.
    #[cfg(test)]
    pub(crate) fn active_cursor_count(&self) -> usize {
        self.inner.lock_acc().cursors.len()
    }

    /// Whether a blind-root poller thread exists right now.
    #[cfg(all(test, unix))]
    pub(crate) fn blind_poll_running(&self) -> bool {
        self.inner.blind_poll.running.load(Ordering::SeqCst)
    }

    /// How many fallback polls have landed, for a test that must see one stop happening.
    #[cfg(test)]
    pub(crate) fn poll_count(&self) -> u64 {
        self.inner.poll_state.lock().unwrap_or_else(PoisonError::into_inner).polls
    }

    #[cfg(test)]
    pub(crate) fn tick_count(&self) -> u64 {
        self.inner.ticks.load(Ordering::Relaxed)
    }

    /// Re-arms this hub decided on its own — by tick or by declaration, never by an
    /// explicit `rearm` from a caller.
    #[cfg(test)]
    pub(crate) fn self_rearm_count(&self) -> u64 {
        self.inner.rearms.load(Ordering::Relaxed)
    }

    /// Distinct paths held for cursors that have not drained them yet.
    #[cfg(test)]
    pub(crate) fn undrained_paths(&self) -> usize {
        self.inner.lock_acc().entries.len()
    }

    /// Reconcile REQUESTS, including those a consumer never distinguishes: `drain`
    /// closes the idempotence window, so repeats cost a full walk each.
    #[cfg(test)]
    pub(crate) fn rescan_request_count(&self) -> u64 {
        self.inner.lock_acc().rescans_requested
    }

    /// Run one coverage tick and wait for it to finish, without waiting out the
    /// period. Returns false if the hub thread is gone.
    #[cfg(test)]
    pub(crate) fn tick_now(&self) -> bool {
        let before = self.tick_count();
        if self.control().send(HubMsg::Tick).is_err() {
            return false;
        }
        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(5),
        );
        while patience.allows() {
            if self.tick_count() > before {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Drive the exact transition the re-watch error branch takes. A real
    /// `watcher.watch` failure cannot be injected without a mock backend, so tests
    /// exercise the state transition directly.
    #[cfg(test)]
    fn trigger_rewatch_failure_for_test(&self) {
        self.inner.note_rewatch_failed(Path::new("/unwatchable"), &notify::Error::generic("test"));
    }
}

/// One watch target: a directory watched recursively (a scan root) or
/// non-recursively (the workspace root, for the analyzer config files that live
/// directly in it). Watching the DIRECTORY — never the config files themselves —
/// is load-bearing twice over: an editor's atomic save replaces the file's inode
/// (killing a file watch while the canonical set looks unchanged), and a config
/// file that does not exist yet cannot be watched at all, so its creation would
/// go unseen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchTarget {
    pub(crate) path: PathBuf,
    pub(crate) recursive: bool,
}

impl WatchTarget {
    pub(crate) fn recursive(path: PathBuf) -> Self {
        Self { path, recursive: true }
    }

    fn mode(&self) -> RecursiveMode {
        if self.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        }
    }
}

/// Why a watch is in place, and therefore what may take it away.
///
/// The declared set is not everything the backend ends up holding. A directory an event
/// reveals can be a door out of every declared tree — a symlink, a mount point — and the
/// arm that follows it registers a subtree the declaration does not name. Recording both
/// kinds without telling them apart produces one of two opposite defects: a re-arm that
/// disarms what nothing will ever arm again, or coverage that outlives the topology which
/// justified it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArmOrigin {
    /// The declared set asks for this target. It is what a declaration is compared
    /// against, what is published as a watched root, and what a re-arm may disarm.
    Declared,
    /// An event revealed the directory while no armed recursive watch reached it. The
    /// declaration never names it, so a re-arm cannot arm it back and must not take it
    /// away with the declared targets; it lives exactly as long as the scope still walks
    /// the path it was armed on.
    Incidental,
}

/// One watch the backend is actually holding.
///
/// In its own module so the resolved spelling cannot be written from outside, for the same
/// reason [`ResolvedTargets`] hides its fields: three separate comparisons read that
/// spelling, the rule for it was once applied in two different ways, and the two agree on
/// every path whose leaf resolves — so a producer that reverts to the other rule breaks
/// nothing a test can see. The constructor is the guarantee; a convention is not.
mod arming {
    use std::path::{Path, PathBuf};

    use super::{resolve_as_far_as_it_goes, ArmOrigin, WatchTarget};

    fn birth_of(path: &Path) -> Option<std::time::SystemTime> {
        std::fs::metadata(path).ok().and_then(|meta| meta.created().ok())
    }

    #[derive(Debug, Clone)]
    pub(super) struct ArmedTarget {
        target: WatchTarget,
        resolved: PathBuf,
        /// When the thing at `resolved` came into being, as far as the file system will
        /// say. Best-effort by nature — a file system without a birth time reports none for
        /// every target, and there this simply never disagrees — which is the same bargain
        /// [`Fingerprint`] makes for a declared root.
        born: Option<std::time::SystemTime>,
        origin: ArmOrigin,
    }

    impl ArmedTarget {
        /// The ONE place an armed target's resolved spelling is decided, and it is decided
        /// by [`resolve_as_far_as_it_goes`]: a whole-path `canonicalize` with a fallback to
        /// the raw spelling answers differently for exactly the path whose leaf cannot be
        /// resolved, and under a symlinked ancestor the raw spelling and the resolved one
        /// are two different paths that no comparison will ever bring together.
        ///
        /// Read BEFORE the backend is asked to take the watch, never after. A link
        /// retargeted in that instant is a race either way — the backend does its own
        /// resolution inside `watch` — but the two readings fail differently. Recorded
        /// beforehand, the record disagrees with the snapshot taken later, the coverage
        /// check calls that movement, and the race costs one extra re-arm. Recorded
        /// afterwards, the record and the snapshot agree on the NEW target while the
        /// backend is still watching the old one, so nothing ever contradicts anything and
        /// the watch stays where nobody is looking. The snapshot is taken before arming for
        /// exactly this reason; this is the same trade on the same race.
        pub(super) fn arming(target: WatchTarget, origin: ArmOrigin) -> Self {
            let resolved = resolve_as_far_as_it_goes(&target.path);
            let born = birth_of(&resolved);
            Self { target, resolved, born, origin }
        }

        pub(super) fn target(&self) -> &WatchTarget {
            &self.target
        }

        pub(super) fn resolved(&self) -> &Path {
            &self.resolved
        }

        /// Whether the declaration owns this watch. The declared entries are the whole of
        /// what a declaration is compared against; the rest is coverage nobody asked for
        /// by name.
        pub(super) fn is_declared(&self) -> bool {
            matches!(self.origin, ArmOrigin::Declared)
        }

        /// The same watch, named by the spelling the declaration now uses.
        ///
        /// A re-arm keeps a target the backend is already holding, and the new declaration
        /// may name it by a DIFFERENT spelling of the same directory — two links to one
        /// tree, a link declared beside its target. The registration has not moved, so the
        /// resolution is carried as it stands and no rule is applied here; the spelling
        /// must follow the declaration, because that is the one the scope now accepts and
        /// the one the backend reports under once the defensive pass re-arms it. A set left
        /// on the dropped spelling describes a root the scope has stopped taking events
        /// for, and reads as different from the declaration for ever after — a full re-arm,
        /// and the reconcile it costs everyone, on every declaration of the same set.
        pub(super) fn under(&self, target: WatchTarget) -> Self {
            Self { target, resolved: self.resolved.clone(), born: self.born, origin: self.origin }
        }

        /// Whether two records name the SAME watch: the same mode, the same place, and the
        /// same object at that place.
        ///
        /// The object is the half a path comparison cannot see. A directory removed and
        /// recreated under one name resolves identically, so a re-arm matching on the path
        /// alone would carry the old record forward and leave the backend watching what is
        /// gone — the very case the periodic check has just detected, since the snapshot
        /// fingerprints birth time for exactly this reason.
        pub(super) fn names_the_same_watch_as(&self, other: &ArmedTarget) -> bool {
            self.target.recursive == other.target.recursive
                && self.resolved == other.resolved
                && self.born == other.born
        }

        /// Whether the spelling still leads where this record says the registration went.
        ///
        /// Asked to decide whether a registration must be RE-POINTED, never to claim one. A
        /// record that answers `true` is not evidence that the backend still delivers from
        /// there: nothing re-arms a door between events, and a stale record allowed to claim
        /// coverage would suppress exactly the arm that would have made it true again.
        ///
        /// And a `false` is not grounds to FORGET the record either. A door whose target has
        /// stepped aside for a moment — a rebuild renaming a directory and putting it back —
        /// answers `false` while the link itself never changed and will never fire another
        /// event, so the record is the only thing left that can re-point it.
        pub(super) fn still_leads_where_recorded(&self) -> bool {
            // The same PLACE and the same THING. A directory removed and recreated under
            // one name keeps the spelling while the registration stays on the object that
            // is gone, so comparing where the path leads would call a dead registration
            // live — and nothing else in the module would ever notice, because the link
            // itself never changed and fires no event.
            resolve_as_far_as_it_goes(&self.target.path) == self.resolved
                && birth_of(&self.resolved) == self.born
        }
    }
}

use arming::ArmedTarget;

/// The full watch-target set for a workspace: the drift-scan roots (recursive)
/// plus the workspace root itself, non-recursively, so config-file
/// creation/edit/atomic-replace is event-delivered even in a nested layout
/// where the workspace root is NOT a scan root. A non-recursive root already
/// covered by a recursive scan root is deduplicated at arm time.
pub(crate) fn watch_targets_for(workspace_root: &Path, scan_roots: &[PathBuf]) -> Vec<WatchTarget> {
    let mut targets: Vec<WatchTarget> =
        scan_roots.iter().cloned().map(WatchTarget::recursive).collect();
    targets.push(WatchTarget { path: workspace_root.to_path_buf(), recursive: false });
    targets
}

/// What one `stat` of a watch target says about it, at three levels rather than two.
///
/// `Absent` is a PROVEN absence and nothing weaker: the same allow-list the workspace
/// walker uses (`project-model/src/workspace_walk.rs`), because a denied or interrupted
/// call describes a path that is still there, and calling it gone would re-arm the whole
/// tree twice — once on the failure, once on the recovery.
///
/// `Unknown` is that weaker case. It equals itself, so a persistent failure never reads
/// as movement, and it carries no canonical path, so such a target stays out of the
/// cover until a `stat` succeeds.
///
/// `created` is best-effort by nature: a filesystem without a birth time (NFSv3, some
/// FUSE mounts) reports none for every target, and there a root deleted and recreated at
/// the same path fingerprints identically, so an isolated one — no watched ancestor to
/// report its re-creation — stays uncovered until something re-arms. Nothing cheaper
/// answers better: the inode is reused across an immediate re-create on ext4, so it would
/// read as unchanged too. What remains is the consumers' own reconcile, one round later.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fingerprint {
    Absent,
    Unknown,
    Present { canonical: PathBuf, created: Option<SystemTime> },
}

/// A link cycle belongs in `Absent` by the same argument as `NotADirectory`, but it
/// cannot be named: `ErrorKind::FilesystemLoop` is unstable on the toolchain this crate
/// builds with, and matching a raw errno would differ per platform. It therefore lands
/// in `Unknown`, which errs toward keeping the watch rather than dropping a live tree.
fn target_cannot_exist(kind: std::io::ErrorKind) -> bool {
    matches!(kind, std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
}

/// Read one target's fingerprint. `previous` is consulted only for the `Unknown` case:
/// a target already described keeps its description, so a transient failure costs
/// nothing, while a target seen for the first time records `Unknown` as itself.
fn fingerprint_of(path: &Path, previous: Option<&Fingerprint>) -> Fingerprint {
    match std::fs::metadata(path) {
        Ok(meta) => match path.canonicalize() {
            Ok(canonical) => Fingerprint::Present { canonical, created: meta.created().ok() },
            Err(error) if target_cannot_exist(error.kind()) => Fingerprint::Absent,
            Err(_) => previous.cloned().unwrap_or(Fingerprint::Unknown),
        },
        Err(error) if target_cannot_exist(error.kind()) => Fingerprint::Absent,
        Err(_) => previous.cloned().unwrap_or(Fingerprint::Unknown),
    }
}

/// Fingerprints of every DECLARED target, keyed by its declared spelling.
type Snapshot = HashMap<PathBuf, Fingerprint>;

/// Take a fresh snapshot, carrying each target's previous description into a failed
/// `stat` (see [`Fingerprint::Unknown`]). Targets that left the declared set are dropped;
/// new ones are described as they are now.
fn snapshot_of(declared: &[WatchTarget], previous: &Snapshot) -> Snapshot {
    declared
        .iter()
        .map(|target| {
            let fingerprint = fingerprint_of(&target.path, previous.get(&target.path));
            (target.path.clone(), fingerprint)
        })
        .collect()
}

/// The minimal cover derived FROM A SNAPSHOT — never by canonicalizing again.
///
/// [`dedup_targets`] re-reads the filesystem and collapses every error to the raw path,
/// so a denied `stat` on a symlink's parent would swap that target's canonical path for
/// its declared one and read as a composition change: a full re-arm every tick for as
/// long as the failure lasts. The snapshot already holds the canonical paths, and a
/// target without one (absent or unknown) is simply not part of what can be watched.
fn cover_from_snapshot(
    declared: &[WatchTarget],
    snapshot: &Snapshot,
) -> Vec<(WatchTarget, PathBuf)> {
    let mut pairs: Vec<(PathBuf, WatchTarget)> = declared
        .iter()
        .filter_map(|target| match snapshot.get(&target.path) {
            Some(Fingerprint::Present { canonical, .. }) => {
                Some((canonical.clone(), target.clone()))
            }
            _ => None,
        })
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.recursive.cmp(&a.1.recursive)));
    pairs.dedup_by(|a, b| a.0 == b.0);

    let mut kept: Vec<(WatchTarget, PathBuf)> = Vec::new();
    for (canonical, target) in pairs {
        if kept.iter().any(|(k, kc)| k.recursive && canonical.starts_with(kc)) {
            continue;
        }
        kept.push((target, canonical));
    }
    kept
}

/// Has the watched world moved since `previous` was taken?
///
/// Two things count, and nothing else. The cover's MEMBERSHIP — compared by declared
/// spelling too, because collapsing canonical duplicates keeps one target and the winner
/// may swap from one alias to another with every fingerprint equal, leaving the watch on
/// a spelling the scope no longer knows. And a fingerprint change on a target that is IN
/// the cover: a target absorbed by a recursive ancestor does not reach `apply_rearm` at
/// all, so paying a full walk for its re-creation would buy nothing — notify re-arms
/// subdirectories of a recursive watch by itself.
fn coverage_moved(declared: &[WatchTarget], previous: &Snapshot, current: &Snapshot) -> bool {
    let was = cover_from_snapshot(declared, previous);
    let now = cover_from_snapshot(declared, current);
    let key = |cover: &[(WatchTarget, PathBuf)]| -> Vec<(PathBuf, bool, PathBuf)> {
        let mut keys: Vec<(PathBuf, bool, PathBuf)> = cover
            .iter()
            .map(|(t, canonical)| (t.path.clone(), t.recursive, canonical.clone()))
            .collect();
        keys.sort();
        keys
    };
    if key(&was) != key(&now) {
        return true;
    }
    now.iter().any(|(target, _)| previous.get(&target.path) != current.get(&target.path))
}

/// Does the cover a declaration asks for differ from what the watcher actually holds?
///
/// Compared by DECLARED spelling: two aliases of one directory canonicalize the same, so
/// a cover that swapped one for the other is identical everywhere except here, while the
/// backend keeps reporting under the spelling it was armed with — the one the scope has
/// just stopped accepting. `armed` is the only record of that spelling.
///
/// Asked on a declaration and not on the tick: a declaration arrives once per rebuild,
/// whereas the tick runs on a period, and a target whose `watch` keeps failing would then
/// buy every consumer a full walk every period for as long as the obstacle lasts.
fn cover_differs_from_armed(cover: &[(WatchTarget, PathBuf)], armed: &[ArmedTarget]) -> bool {
    let mut wanted: Vec<(PathBuf, bool)> =
        cover.iter().map(|(t, _)| (t.path.clone(), t.recursive)).collect();
    // Declared entries alone: the question is whether the watch stands where the
    // declaration asks, and a watch no declaration names can never answer it either way.
    let mut held: Vec<(PathBuf, bool)> = armed
        .iter()
        .filter(|entry| entry.is_declared())
        .map(|entry| (entry.target().path.clone(), entry.target().recursive))
        .collect();
    wanted.sort();
    held.sort();
    wanted != held
}

/// The declared targets that exist and are not watched, with the canonical path the
/// cover ranks them by.
///
/// A target that is simply not there is NOT blind: nothing can watch what does not exist,
/// and its creation moves a fingerprint, which the periodic check already answers with a
/// full re-arm. Everything else declared and unwatched is, and it arrives two ways. Most
/// of it through the cover: the target the watcher refused — a permission on the target,
/// an exhausted inotify limit — which stats and canonicalizes perfectly while its subtree
/// goes unobserved. The rest never reaches the cover at all, because it cannot be
/// described: an unreadable PARENT denies every `stat` below it, and so does a symlink
/// cycle. Such a target is neither present nor absent, and reading blindness off the
/// cover alone would leave exactly it unreported, unwatched and never retried.
fn blind_targets(
    declared: &[WatchTarget],
    snapshot: &Snapshot,
    armed: &[ArmedTarget],
) -> Vec<(WatchTarget, PathBuf)> {
    // Matched against the DECLARED entries, as is the absorption question below: a record
    // of a door is a statement about the moment it was armed and nothing ever re-reads it,
    // so letting one answer here would call a target covered on the strength of a watch
    // that may since have stopped reaching it. This asks whether the target the
    // declaration named is itself held; the absorption question — is some other declared
    // recursive watch already reaching it — is asked below.
    let mut blind: Vec<(WatchTarget, PathBuf)> = cover_from_snapshot(declared, snapshot)
        .into_iter()
        .filter(|(target, canonical)| {
            !armed.iter().filter(|entry| entry.is_declared()).any(|entry| {
                entry.target().recursive == target.recursive
                    && entry.resolved() == canonical.as_path()
            })
        })
        .collect();
    for target in declared {
        if !matches!(snapshot.get(&target.path), Some(Fingerprint::Unknown)) {
            continue;
        }
        // Matched by DECLARED spelling, the only handle an undescribable target has: it
        // has no canonical path to rank by, which is why the cover cannot hold it. The
        // second test is the absorption the cover would have applied — an armed recursive
        // ancestor watches it already, and calling it blind would degrade the hub for ever
        // over a subtree that is in fact covered.
        let covered = armed.iter().filter(|entry| entry.is_declared()).any(|entry| {
            entry.target().recursive == target.recursive && entry.target().path == target.path
        }) || an_armed_recursive_target_covers(armed, &target.path);
        if !covered {
            blind.push((target.clone(), target.path.clone()));
        }
    }
    blind
}

/// Re-derive the blind set and report it to consumers ON THE TRANSITION into blindness.
///
/// Called wherever the declaration or the armed set can have moved, which is what makes
/// membership an intersection with the CURRENT declaration rather than a growing list.
///
/// Reporting only the transition is what makes retrying affordable: every reconcile
/// request costs each consumer a full tree walk, so a target re-tried each period would
/// buy a walk each period for as long as the obstacle lasts — the precise cost the retry
/// exists to avoid. What the repeats do not report, the standing ill health derived from
/// the set says instead, and unlike a reconcile window it is not cleared by `drain`.
fn refresh_blind_targets(
    inner: &Arc<HubInner>,
    declared: &[WatchTarget],
    snapshot: &Snapshot,
    armed: &[ArmedTarget],
) {
    let blind = blind_targets(declared, snapshot, armed);
    BlindPoll::retarget(inner, blind.iter().map(|(target, _)| target.clone()).collect());
    let (newly, cleared) = {
        let mut published = inner.blind_targets.lock().unwrap_or_else(PoisonError::into_inner);
        let newly = blind.iter().any(|(target, _)| !published.contains(&target.path));
        let cleared = !published.is_empty() && blind.is_empty();
        *published = blind.iter().map(|(target, _)| target.path.clone()).collect();
        (newly, cleared)
    };
    if newly {
        for (target, _) in &blind {
            tracing::warn!(root = ?target.path, "workspace change hub is not watching a declared root; changes under it are found by polling");
        }
        // Read first, announced after: the poll announces once every file of the newly blind
        // roots has been read once, so no consumer re-reads a root ahead of its baseline.
        if !BlindPoll::defer_reconcile(inner) {
            inner.lock_acc().enter_rescan(false, DegradeReason::RewatchFailed);
            inner.notify();
        }
    } else if cleared {
        // The TRANSITION out of blindness, not merely the absence of it: the obstacle this
        // pass announced has ended, and this is where that becomes knowable, since `drain`
        // closes a window from the consumer's side and a hub nobody has subscribed to has no
        // consumer's side. Read off the transition because `RewatchFailed` has a second
        // producer — a watch this module could not extend over a subtree an event revealed —
        // and a set that was never blind has ended no obstacle at all. A lost stream, an
        // overflow, a reconcile miss are other obstacles again, and a check that never read
        // them has no business answering for them.
        let mut acc = inner.lock_acc();
        if acc.degrade_reason == Some(DegradeReason::RewatchFailed) {
            acc.close_window_if_settled();
        }
    }
}

/// The path with every link resolved that CAN be resolved, keeping the rest as spelled.
///
/// A whole-path `canonicalize` answers all or nothing, and it answers nothing for
/// reasons that say nothing about where the path lies: the leaf does not exist yet (a
/// declared root created later, a config file about to be written), or a directory on
/// the way has been made unreadable. Every link ABOVE that point still resolves exactly
/// as it did, so falling straight back to the raw spelling throws away a resolution that
/// was available — and through a symlinked ancestor the raw spelling and the resolved
/// one are two different paths that no comparison will ever bring together.
///
/// Resolving the longest ancestor that still answers is also the stricter reading: a
/// missing leaf under a LINK inside a root lands where the link points, so a caller
/// asking "is this inside my root" gets the true answer instead of the one the spelling
/// suggests.
pub(crate) fn resolve_as_far_as_it_goes(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut current = path;
    while let (Some(parent), Some(name)) = (current.parent(), current.file_name()) {
        tail.push(name);
        if let Ok(mut resolved) = parent.canonicalize() {
            resolved.extend(tail.iter().rev());
            return resolved;
        }
        current = parent;
    }
    path.to_path_buf()
}

/// Reduce a set of watch targets to the minimal cover: drop any target nested under
/// a RECURSIVE target (a non-recursive ancestor covers only its direct children, so
/// it absorbs nothing), and collapse exact duplicates — a recursive duplicate wins
/// over a non-recursive one. Comparison is by the resolved path
/// ([`resolve_as_far_as_it_goes`], so a target that does not exist yet is still placed
/// against the tree it lies in); the RAW path is what gets watched, so event paths keep
/// the spelling consumers strip against (the search sink strips the non-canonical source
/// root). Returns each kept target with the resolved path used for the decision.
/// Whether two declarations name the same watch, whatever order they name it in.
///
/// A declaration is a SET: the graph and the diagnostics resident derive theirs from the same
/// project and can list the roots in different orders, and reading that as a different
/// declaration would cost a re-arm and a reconcile every time either of them rebuilt — for
/// ever, since neither order is the "right" one.
fn same_declaration(left: &[WatchTarget], right: &[WatchTarget]) -> bool {
    let key = |targets: &[WatchTarget]| {
        let mut keys: Vec<(PathBuf, bool)> =
            targets.iter().map(|t| (t.path.clone(), t.recursive)).collect();
        keys.sort();
        keys.dedup();
        keys
    };
    key(left) == key(right)
}

fn dedup_targets(targets: Vec<WatchTarget>) -> Vec<(WatchTarget, PathBuf)> {
    let mut pairs: Vec<(PathBuf, WatchTarget)> =
        targets.into_iter().map(|t| (resolve_as_far_as_it_goes(&t.path), t)).collect();
    // Parents sort before descendants; among equal canonicals the recursive one first.
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.recursive.cmp(&a.1.recursive)));
    pairs.dedup_by(|a, b| a.0 == b.0);

    let mut kept: Vec<(WatchTarget, PathBuf)> = Vec::new();
    for (canonical, target) in pairs {
        if kept.iter().any(|(k, kc)| k.recursive && canonical.starts_with(kc)) {
            continue;
        }
        kept.push((target, canonical));
    }
    kept
}

/// Record a watch the backend has just taken, replacing whatever stood under the same
/// declared spelling.
///
/// One path is one registration, so it has to be one record. A door re-armed after it was
/// retargeted — a link removed and recreated elsewhere, a mount point replaced — arrives
/// here a second time, and appending would keep the abandoned resolution beside the true
/// one for the life of the daemon.
///
/// Same ORIGIN only. A declared record and a door can name one spelling at once — a
/// declared root that is itself a link, re-pointed while an event describes it — and a
/// door must never take a declaration's place: the declared entries are the whole of what
/// a declaration is compared against, and a set that had quietly demoted one would report
/// an armed root as unwatched.
fn record_arm(armed: &mut Vec<ArmedTarget>, entry: ArmedTarget) {
    armed.retain(|standing| {
        standing.is_declared() != entry.is_declared()
            || standing.target().path != entry.target().path
    });
    armed.push(entry);
}

/// How long a re-arm caller waits for the hub thread's acknowledgement before
/// reporting failure. The thread only pumps events, so this is generous.
const REARM_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the hub re-checks that its declared coverage is still live.
///
/// A symlinked root retargeted in place emits no event at all, so nothing but this
/// interval bounds how long a daemon can watch a tree nobody declared any more. Thirty
/// seconds sits alongside the consumers' own reconcile cadence, and the check itself is
/// a handful of `stat` calls over a handful of targets.
const COVERAGE_TICK_PERIOD: Duration = Duration::from_secs(30);

/// How often the fallback poll walks the targets when no watch could be set up.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How many bytes one poll reads to verify contents a stat cannot vouch for.
const VERIFY_BYTES: u64 = 32 * 1024 * 1024;

/// The fallback poll's cadence and read budget.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PollConfig {
    pub(crate) period: Duration,
    pub(crate) verify_bytes: u64,
}

impl PollConfig {
    pub(crate) const PRODUCTION: Self = Self { period: POLL_INTERVAL, verify_bytes: VERIFY_BYTES };
}

/// What the fallback poll has done, for a status to report.
#[derive(Debug, Default, Clone, Copy)]
struct PollStatus {
    last: Option<Instant>,
    polls: u64,
    /// Bytes under the polled targets as of the last walk: with the read budget it bounds how
    /// long an edit that kept its size and mtime can go unnoticed.
    bytes: u64,
}

/// What the fallback poll knows about one file.
struct PolledFile {
    raw: PathBuf,
    stamp: (Option<SystemTime>, u64),
}

/// The fallback poll's own record of the tree: a `(mtime, len)` per file, and its OWN content
/// hash per file. The hashes answer "did this file change since the poll last read it", never
/// "does it match a baseline" — no store, no index mode, nothing a consumer owns takes part.
/// The most the blind poll's content check reads in one hold of the poller.
///
/// The budget it spends is `verify_bytes`; this is how much of it may be read before the lock
/// goes back, so a hub thread declaring a root blind waits for a slice rather than for a whole
/// file. Large enough that the per-slice overhead is noise next to the read itself.
const VERIFY_SLICE: u64 = 256 * 1024;

#[derive(Default)]
struct Poller {
    files: HashMap<PathBuf, PolledFile>,
    hashes: HashMap<PathBuf, blake3::Hash>,
    verify_next: usize,
    /// A file whose reading a tick's budget cut short, resumed where it stopped.
    partial: Option<PartialRead>,
    /// The same for the first reading of a root that has just turned blind. Kept apart, because
    /// that reading and the ordinary rotation take turns, and one slot would throw the other's
    /// progress away on every turn.
    baseline_partial: Option<PartialRead>,
    /// Whose turn the next content check is while a first reading is under way.
    baseline_turn: bool,
    /// The first picture is taken. A file first read after it may have changed since the
    /// reconcile that picture stood for, and no earlier hash would show it.
    pictured: bool,
    /// Except these: keys of a root that has just turned blind, whose first hash is the
    /// baseline and not news. That is sound only because the reconcile announcing the blindness
    /// is held back until every one of them has been read: a consumer's re-read then follows
    /// the baseline, and any edit after the baseline differs from it. Announced first, an edit
    /// landing between a consumer's re-read and the first hash became the baseline in silence.
    unpictured: std::collections::BTreeSet<PathBuf>,
    /// Bytes the content check read in the last poll.
    #[cfg(test)]
    last_read: u64,
}

/// How far the content check has read one file, and the hash so far.
struct PartialRead {
    key: PathBuf,
    stamp: (Option<SystemTime>, u64),
    offset: u64,
    hasher: blake3::Hasher,
}

/// What a walk saw, and where it could not look: a path it could not stat or read for
/// another reason than absence says nothing about the files below it.
#[derive(Default)]
struct Walked {
    files: HashMap<PathBuf, PolledFile>,
    unreadable: Vec<PathBuf>,
}

/// The content check reads in pieces of this size, so a tick holds no more than one in memory.
const VERIFY_CHUNK: usize = 64 * 1024;

impl Poller {
    /// Every file the scope lets the hub record, keyed as the event path keys it.
    fn walk(targets: &[WatchTarget], scope: &Scope) -> Walked {
        let mut found: Vec<(PathBuf, PathBuf, ChangeKind)> = Vec::new();
        let mut unreadable = Vec::new();
        for target in targets {
            if target.recursive {
                collect_subtree_noting(
                    &target.path,
                    &mut found,
                    Some(&mut unreadable),
                    scope.paths.exclusions(),
                );
            } else {
                match std::fs::read_dir(&target.path) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.is_file() {
                                found.push((
                                    resolve_as_far_as_it_goes(&path),
                                    path,
                                    ChangeKind::MaybeChanged,
                                ));
                            }
                        }
                    }
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        unreadable.push(target.path.clone());
                    }
                    Err(_) => {}
                }
            }
        }
        let mut files = HashMap::new();
        for (canonical, raw, _) in found.into_iter().filter(|(_, raw, _)| scope.may_record(raw)) {
            match std::fs::metadata(&raw) {
                Ok(meta) => {
                    let stamp = (meta.modified().ok(), meta.len());
                    files.insert(canonical, PolledFile { raw, stamp });
                }
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => unreadable.push(raw),
                Err(_) => {}
            }
        }
        Walked { files, unreadable }
    }

    /// One poll: what moved since the last one, by stat and then by content within `budget`
    /// bytes. `map_only` takes the first picture and reports nothing.
    fn poll(
        &mut self,
        targets: &[WatchTarget],
        scope: &Scope,
        budget: u64,
        map_only: bool,
    ) -> Vec<(PathBuf, PathBuf, ChangeKind)> {
        let now = Self::walk(targets, scope);
        self.take(now, budget, map_only)
    }

    /// [`Self::poll`] over a walk already taken.
    fn take(
        &mut self,
        now: Walked,
        budget: u64,
        map_only: bool,
    ) -> Vec<(PathBuf, PathBuf, ChangeKind)> {
        let Walked { files: mut now, unreadable } = now;
        // A file under a path the walk could not look into is not gone: it keeps its last
        // stamp until the walk can see it again.
        for (key, old) in &self.files {
            if !now.contains_key(key) && unreadable.iter().any(|path| old.raw.starts_with(path)) {
                now.insert(key.clone(), PolledFile { raw: old.raw.clone(), stamp: old.stamp });
            }
        }
        let mut records = Vec::new();
        if !map_only {
            for (key, file) in &now {
                if self.files.get(key).is_none_or(|old| old.stamp != file.stamp) {
                    records.push((key.clone(), file.raw.clone(), ChangeKind::MaybeChanged));
                }
            }
            for (key, old) in &self.files {
                if !now.contains_key(key) {
                    records.push((key.clone(), old.raw.clone(), ChangeKind::MaybeRemoved));
                }
            }
        }
        self.hashes.retain(|key, _| now.contains_key(key));
        self.unpictured.retain(|key| now.contains_key(key));
        self.files = now;
        self.verify(budget, &mut records);
        if map_only {
            self.pictured = true;
        }
        records
    }

    /// Read the next files in turn, no more than `budget` bytes in all and at least some of
    /// one file, and report those whose bytes changed since this poll last read them. A file
    /// larger than what is left of the budget is read across ticks. A file read for the first
    /// time after the picture is reported too — it may have changed unseen before that
    /// reading — and a change is reported once: the new hash replaces the old one.
    ///
    /// While a root that has just turned blind is being read for the first time, that reading
    /// and the ordinary rotation take turns: the first reading is what its reconcile waits for,
    /// and the rotation is what finds edits in every root already read — neither waits for the
    /// other to finish.
    fn verify(&mut self, budget: u64, records: &mut Vec<(PathBuf, PathBuf, ChangeKind)>) {
        let mut keys: Vec<PathBuf> = self.files.keys().cloned().collect();
        keys.sort();
        let (baseline, settled): (Vec<PathBuf>, Vec<PathBuf>) =
            keys.into_iter().partition(|key| self.unpictured.contains(key));
        let mut left = budget.max(1);
        let baseline_turn = if baseline.is_empty() {
            false
        } else if settled.is_empty() {
            true
        } else {
            self.baseline_turn = !self.baseline_turn;
            self.baseline_turn
        };
        if baseline_turn {
            for key in &baseline {
                if left == 0 || !self.read_one(key, true, &mut left, records) {
                    break;
                }
            }
        } else {
            let mut visited = 0;
            while visited < settled.len() && left > 0 {
                let index = self.verify_next % settled.len();
                if !self.read_one(&settled[index], false, &mut left, records) {
                    break;
                }
                self.verify_next = index + 1;
                visited += 1;
            }
        }
        #[cfg(test)]
        {
            self.last_read = budget.max(1) - left;
        }
    }

    /// Read `key` on from where its slot stopped. `false` when the budget ran out inside it.
    fn read_one(
        &mut self,
        key: &PathBuf,
        baseline: bool,
        left: &mut u64,
        records: &mut Vec<(PathBuf, PathBuf, ChangeKind)>,
    ) -> bool {
        let (raw, stamp) = {
            let file = &self.files[key];
            (file.raw.clone(), file.stamp)
        };
        let slot = if baseline { &mut self.baseline_partial } else { &mut self.partial };
        let mut reading = match slot.take() {
            Some(partial) if partial.key == *key && partial.stamp == stamp => partial,
            _ => PartialRead { key: key.clone(), stamp, offset: 0, hasher: blake3::Hasher::new() },
        };
        match Self::read_some(&raw, &mut reading, left) {
            // Unreadable now: nothing to compare, and the stat walk still sees it. Nor is there
            // a baseline to take, so the reading that does succeed later is news, not one.
            None => {
                self.unpictured.remove(key);
                true
            }
            Some(false) => {
                *(if baseline { &mut self.baseline_partial } else { &mut self.partial }) =
                    Some(reading);
                false
            }
            Some(true) => {
                let hash = reading.hasher.finalize();
                let already = records.iter().any(|(recorded, _, _)| recorded == key);
                let changed = match self.hashes.insert(key.clone(), hash) {
                    Some(previous) => previous != hash,
                    None => {
                        // Taken off the list whatever the picture says, so one silent
                        // reading is all a key ever gets.
                        let covered = self.unpictured.remove(key);
                        self.pictured && !covered
                    }
                };
                if changed && !already {
                    records.push((key.clone(), raw, ChangeKind::MaybeChanged));
                }
                true
            }
        }
    }

    /// Every file of the roots that turned blind has been read once, or could not be.
    fn baseline_complete(&self) -> bool {
        !self.files.keys().any(|key| self.unpictured.contains(key))
    }

    /// Read on from `reading.offset`, at most `left` bytes, into its hash. `Some(true)`: the
    /// file is read to its end; `Some(false)`: the budget ran out first; `None`: unreadable.
    fn read_some(path: &Path, reading: &mut PartialRead, left: &mut u64) -> Option<bool> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path).ok()?;
        file.seek(SeekFrom::Start(reading.offset)).ok()?;
        let mut buffer = vec![0u8; VERIFY_CHUNK];
        loop {
            if *left == 0 {
                return Some(reading.offset >= reading.stamp.1 && file.read(&mut [0u8]).ok()? == 0);
            }
            let want = buffer.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
            let read = file.read(&mut buffer[..want]).ok()?;
            if read == 0 {
                return Some(true);
            }
            reading.hasher.update(&buffer[..read]);
            reading.offset += read as u64;
            *left -= read as u64;
        }
    }

    fn bytes(&self) -> u64 {
        self.files.values().map(|file| file.stamp.1).sum()
    }
}

/// A raised-once stop with a wait on it, shared by a thread and whoever stops it.
#[derive(Default)]
struct StopFlag {
    raised: Mutex<bool>,
    wake: Condvar,
    /// Work is waiting: the current wait ends early, once, and the stop is not raised by it.
    poked: std::sync::atomic::AtomicBool,
}

impl StopFlag {
    fn raise(&self) {
        *self.raised.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.wake.notify_all();
    }

    /// End the current wait (or the next one) early, without stopping anything.
    fn poke(&self) {
        let _raised = self.raised.lock().unwrap_or_else(PoisonError::into_inner);
        self.poked.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    #[cfg(test)]
    fn is_raised(&self) -> bool {
        *self.raised.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait up to `timeout`, or until poked; says whether the stop was raised.
    fn wait(&self, timeout: Duration) -> bool {
        let raised = self.raised.lock().unwrap_or_else(PoisonError::into_inner);
        let raised = self
            .wake
            .wait_timeout_while(raised, timeout, |raised| {
                !*raised && !self.poked.load(Ordering::SeqCst)
            })
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        self.poked.store(false, Ordering::SeqCst);
        *raised
    }
}

/// The poll of blind roots: declared, present, and not watched while everything else is.
///
/// The hub thread keeps the target set current and takes each newly blind root's first
/// picture itself, BEFORE the reconcile that announces the blindness — so a consumer's
/// rescan reads everything the picture missed and every later change is a poll record. The
/// periodic polls run on a thread of their own, so an event stream still flowing for the
/// watched roots is never held up by a walk.
#[derive(Default)]
struct BlindPoll {
    state: Mutex<BlindPollState>,
    stop: Arc<StopFlag>,
    running: AtomicBool,
    /// Refuses to start the poll thread, so a test can see what a hub that promised an
    /// observation and never made one reports. Per hub, never global: these tests run beside
    /// others that need a working poll.
    #[cfg(test)]
    cannot_start: bool,
    /// `reconcile_owed`, readable without the poller's lock: a cursor subscribed while the
    /// first reading is under way is not handed a reconcile of its own ahead of the baseline —
    /// the announcement that follows the baseline flags it like everyone else.
    reconcile_pending: AtomicBool,
    /// Holds each poll of this hub's blind roots until a test lets it run.
    #[cfg(test)]
    gate: Option<Arc<PollGate>>,
    /// Parks the announcement of a blind root's reconcile where a test asks.
    #[cfg(test)]
    announce: Option<Arc<AnnounceBarrier>>,
}

/// The test seams of a hub's blind poll, fixed when the hub is built so the poll thread cannot
/// run before they are in place.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct BlindPollSeam {
    cannot_start: bool,
    gate: Option<Arc<PollGate>>,
    announce: Option<Arc<AnnounceBarrier>>,
}

#[cfg(test)]
impl BlindPollSeam {
    #[cfg(unix)]
    pub(crate) fn refusing_to_start() -> Self {
        Self { cannot_start: true, gate: None, announce: None }
    }
}

/// A barrier in front of every blind poll: a poll runs only on a permit, and each arrival is
/// counted, so a test can say "exactly this many polls have completed". Stop-aware, so a hub
/// shut down while a poll waits here is not held by it.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct PollGate {
    counts: Mutex<(usize, usize)>,
    moved: Condvar,
}

#[cfg(test)]
impl PollGate {
    fn arrive(&self, stop: &StopFlag) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        counts.1 += 1;
        self.moved.notify_all();
        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(20),
        );
        let mut wakes = 0;
        while counts.0 == 0 && !stop.is_raised() {
            let (next, go) = test_support::wait_step(
                &self.moved,
                counts,
                Duration::from_millis(20),
                &mut patience,
                &mut wakes,
            );
            counts = next;
            if !go {
                break;
            }
        }
        counts.0 = counts.0.saturating_sub(1);
    }

    /// Wait until the poll has reached the gate at least `arrivals` times.
    #[cfg(unix)]
    pub(crate) fn wait_arrivals(&self, arrivals: usize) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(20),
        );
        let mut wakes = 0;
        while counts.1 < arrivals {
            let (next, go) = test_support::wait_step(
                &self.moved,
                counts,
                Duration::from_millis(20),
                &mut patience,
                &mut wakes,
            );
            counts = next;
            if !go {
                break;
            }
        }
        assert!(counts.1 >= arrivals, "the blind poll never reached its gate {arrivals} times");
    }

    /// Let `polls` more polls run, without waiting for any of them.
    #[cfg(unix)]
    pub(crate) fn allow(&self, polls: usize) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        counts.0 += polls;
        self.moved.notify_all();
    }

    /// Let `polls` polls run, and return once each of them has come back to the gate.
    #[cfg(unix)]
    pub(crate) fn run_polls(&self, polls: usize) {
        let target = {
            let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
            counts.0 += polls;
            self.moved.notify_all();
            counts.1 + polls
        };
        self.wait_arrivals(target);
    }
}

/// Where the announcement of a blind root's reconcile can be parked.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnnouncePoint {
    /// The first moment another thread can act after the poll decided to announce.
    BeforeIssue,
    /// The first moment another thread can act after the reconcile was issued.
    AfterIssue,
}

/// A barrier inside the announcement: armed for one point, the poll parks there once until the
/// test releases it. Stop-aware and bounded, so a failing stand does not hold the hub.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct AnnounceBarrier {
    /// The armed point, whether the poll is parked, and whether it has been released.
    state: Mutex<(Option<AnnouncePoint>, bool, bool)>,
    moved: Condvar,
}

#[cfg(test)]
impl AnnounceBarrier {
    #[cfg(unix)]
    pub(crate) fn arm(&self, point: AnnouncePoint) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = (Some(point), false, false);
    }

    fn reach(&self, point: AnnouncePoint, stop: &StopFlag) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.0 != Some(point) {
            return;
        }
        state.0 = None;
        state.1 = true;
        self.moved.notify_all();
        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(20),
        );
        let mut wakes = 0;
        while !state.2 && !stop.is_raised() {
            let (next, go) = test_support::wait_step(
                &self.moved,
                state,
                Duration::from_millis(20),
                &mut patience,
                &mut wakes,
            );
            state = next;
            if !go {
                break;
            }
        }
        state.1 = false;
        self.moved.notify_all();
    }

    #[cfg(unix)]
    pub(crate) fn wait_parked(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(20),
        );
        let mut wakes = 0;
        while !state.1 {
            let (next, go) = test_support::wait_step(
                &self.moved,
                state,
                Duration::from_millis(20),
                &mut patience,
                &mut wakes,
            );
            state = next;
            if !go {
                break;
            }
        }
        assert!(state.1, "the announcement never reached its barrier");
    }

    #[cfg(unix)]
    pub(crate) fn release(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).2 = true;
        self.moved.notify_all();
    }
}

#[derive(Default)]
struct BlindPollState {
    targets: Vec<WatchTarget>,
    poller: Poller,
    /// Moved on every retarget, so a walk taken against an older set is not applied.
    epoch: u64,
    /// A root turned blind and its reconcile is not announced yet: the poll announces it once
    /// the first reading of every blind file is done.
    reconcile_owed: bool,
}

impl BlindPoll {
    /// Poll `targets` from now on. Roots joining the set are mapped here, on the calling
    /// (hub) thread; roots leaving it take their files with them.
    fn retarget(inner: &Arc<HubInner>, targets: Vec<WatchTarget>) {
        let scope = inner.scope();
        {
            let mut state = inner.blind_poll.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.targets == targets {
                return;
            }
            // Two walks, and both are needed: `joining` is the picture the reconcile stands
            // for — files under a root that has just become blind are recorded as they are
            // now, so they are not reported as changes — while `keep` says which of the files
            // already recorded survive the new set. Deriving the first from the second by
            // path containment would add a ninth place comparing paths (`one_path_rule`), and
            // deriving it by "not seen before" would silently absorb a file that appeared
            // under a SURVIVING root since the last walk, which is a change and must be
            // reported. Both walks are over the BLIND roots, never over the watched tree.
            let joining: Vec<WatchTarget> =
                targets.iter().filter(|target| !state.targets.contains(target)).cloned().collect();
            let fresh = Poller::walk(&joining, &scope);
            let keep = Poller::walk(&targets, &scope);
            state.poller.files.retain(|key, _| keep.files.contains_key(key));
            state.poller.hashes.retain(|key, _| keep.files.contains_key(key));
            state.poller.unpictured.retain(|key| keep.files.contains_key(key));
            // The joining files ARE the picture the reconcile announcing the blindness stands
            // for. Their content cannot be hashed here — this runs on the hub thread, and the
            // root may be any size — so the poll's first reading of each takes the baseline,
            // silently, and the reconcile is announced only once that reading is done.
            // Anything after that is news.
            state.poller.unpictured.extend(fresh.files.keys().cloned());
            state.poller.files.extend(fresh.files);
            state.poller.pictured = true;
            state.targets = targets;
            state.epoch += 1;
            if state.targets.is_empty() {
                // Nothing is blind any more: nothing is owed, and a stale expectation would
                // keep reporting an overdue poll over a set nobody polls.
                inner.expect_poll_from(None);
                // Except the reconcile a blindness that has now ended still owes: there is
                // nothing left to read first, so it is announced here rather than forgotten.
                let owed = state.reconcile_owed;
                if owed {
                    Self::issue_reconcile(inner, &mut state);
                }
                drop(state);
                if owed {
                    inner.notify();
                }
                return;
            }
            // Owed from now. If the poll thread below never starts, this is what keeps the
            // blindness visible instead of reading as a tree in good order.
            inner.expect_poll_from(Some(Instant::now()));
        }
        if !inner.blind_poll.running.swap(true, Ordering::SeqCst) {
            let weak = Arc::downgrade(inner);
            let stop = Arc::clone(&inner.blind_poll.stop);
            let period = inner.poll.period;
            #[cfg(test)]
            if inner.blind_poll.cannot_start {
                tracing::warn!("workspace change hub cannot poll its blind roots: refused by test");
                inner.blind_poll.running.store(false, Ordering::SeqCst);
                return;
            }
            let spawned = std::thread::Builder::new()
                .name("bsl-workspace-blind-poll".to_owned())
                .spawn(move || Self::run(weak, stop, period));
            if let Err(error) = spawned {
                tracing::warn!("workspace change hub cannot poll its blind roots: {error}");
                inner.blind_poll.running.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Owe the reconcile of a root that has just turned blind to the poll, which announces it
    /// once every blind file has been read once. `false` when there is no poll to do that — the
    /// thread could not start — and the caller announces at once.
    fn defer_reconcile(inner: &HubInner) -> bool {
        let mut state = inner.blind_poll.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.targets.is_empty() || !inner.blind_poll.running.load(Ordering::SeqCst) {
            return false;
        }
        state.reconcile_owed = true;
        inner.blind_poll.reconcile_pending.store(true, Ordering::SeqCst);
        drop(state);
        // The first reading starts now, not a whole idle period from now: what the reconcile
        // waits for is that reading, and each poll still reads no more than its budget.
        inner.blind_poll.stop.poke();
        true
    }

    /// Issue the reconcile the blind roots are owed, and publish that it is made — under
    /// `state`, the lock a retarget takes to add unread files and a newly blind root takes to
    /// owe another announcement, so neither can come between the decision and the reconcile.
    /// The accumulator is taken inside it, the one order these two locks are ever held in.
    ///
    /// The publication is made before the accumulator is released: a subscription lands either
    /// before the reconcile, which flags it, or after the publication, which it reads as made
    /// and is owed a reconcile of its own. Released in between, a newcomer with no open window
    /// to inherit would read the announcement as still to come and never be told.
    fn issue_reconcile(inner: &HubInner, state: &mut BlindPollState) {
        state.reconcile_owed = false;
        let mut acc = inner.lock_acc();
        acc.enter_rescan(false, DegradeReason::RewatchFailed);
        inner.blind_poll.reconcile_pending.store(false, Ordering::SeqCst);
        drop(acc);
    }

    fn run(inner: std::sync::Weak<HubInner>, stop: Arc<StopFlag>, period: Duration) {
        Self::poll_until_stopped(&inner, &stop, period);
        // Left for good: the claim that a poller exists goes with the poller.
        if let Some(inner) = inner.upgrade() {
            inner.blind_poll.running.store(false, Ordering::SeqCst);
        }
    }

    fn poll_until_stopped(
        inner: &std::sync::Weak<HubInner>,
        stop: &Arc<StopFlag>,
        period: Duration,
    ) {
        while !stop.wait(period) {
            let Some(inner) = inner.upgrade() else { return };
            let (targets, epoch) = {
                let state = inner.blind_poll.state.lock().unwrap_or_else(PoisonError::into_inner);
                (state.targets.clone(), state.epoch)
            };
            if targets.is_empty() {
                continue;
            }
            #[cfg(test)]
            if let Some(gate) = &inner.blind_poll.gate {
                gate.arrive(stop);
            }
            let now = Poller::walk(&targets, &inner.scope());
            // The content check reads whole files — up to `verify_bytes`, which is 32 MB in
            // production — and it needs the poller, which `retarget` needs too, on the hub
            // thread. Taken in one hold, that read is how long the hub waits to declare a root
            // blind. So the budget is spent a SLICE at a time and the lock is dropped between
            // slices: the poller already carries an interrupted read across calls, so this
            // changes what is held and for how long, not what is read.
            let mut records = {
                let mut state =
                    inner.blind_poll.state.lock().unwrap_or_else(PoisonError::into_inner);
                if state.epoch != epoch {
                    continue;
                }
                state.poller.take(now, VERIFY_SLICE.min(inner.poll.verify_bytes), false)
            };
            let mut spent = VERIFY_SLICE.min(inner.poll.verify_bytes);
            while spent < inner.poll.verify_bytes {
                let slice = VERIFY_SLICE.min(inner.poll.verify_bytes - spent);
                let mut state =
                    inner.blind_poll.state.lock().unwrap_or_else(PoisonError::into_inner);
                if state.epoch != epoch {
                    break;
                }
                state.poller.verify(slice, &mut records);
                drop(state);
                spent += slice;
            }
            if !records.is_empty() {
                let mut acc = inner.lock_acc();
                for (canonical, raw, kind) in records {
                    acc.record(canonical, raw, kind);
                }
                drop(acc);
                inner.notify();
            }
            let announced = {
                let mut state =
                    inner.blind_poll.state.lock().unwrap_or_else(PoisonError::into_inner);
                inner.note_poll(&state.poller);
                // Read under the lock a retarget takes to add its files: a root that joined
                // since this poll's walk is already among the files not yet read.
                let ready = state.reconcile_owed && state.poller.baseline_complete();
                if ready {
                    // Inside the hold, on both sides of the reconcile: what a stand can do
                    // from another thread here — subscribe, declare, drain — is exactly what
                    // the hold has to keep from landing in between.
                    #[cfg(test)]
                    if let Some(barrier) = &inner.blind_poll.announce {
                        barrier.reach(AnnouncePoint::BeforeIssue, stop);
                    }
                    Self::issue_reconcile(&inner, &mut state);
                    #[cfg(test)]
                    if let Some(barrier) = &inner.blind_poll.announce {
                        barrier.reach(AnnouncePoint::AfterIssue, stop);
                    }
                }
                ready
            };
            if announced {
                inner.notify();
            }
        }
    }
}

/// The hub without a watch: poll the declared targets on this thread until shutdown.
///
/// The first walk only takes the picture, and only THEN is every cursor told to reconcile
/// once: a consumer's rescan after that point reads everything the picture missed, and every
/// change after the picture is a poll record. The order the other way round would lose a
/// change landing between a consumer's rescan and the picture.
fn run_polling(
    inner: &Arc<HubInner>,
    mut declared: Vec<WatchTarget>,
    rx: &std::sync::mpsc::Receiver<HubMsg>,
) {
    // Raised by the fallback's callers too: `mark_polling_setup_failed` raises it BEFORE the
    // failure is announced, so a waiter that sees `Failed` never reads "no watch and no poll"
    // in the instant between the two. Idempotent, and kept here for any future direct caller.
    inner.polling.store(true, Ordering::SeqCst);
    inner.accept_declaration(&declared);
    // From here an observation is owed. Until the first walk lands, "never polled" must read
    // as overdue rather than as fresh.
    inner.expect_poll_from(Some(Instant::now()));
    let mut poller = Poller::default();
    let map = |poller: &mut Poller, declared: &[WatchTarget], reason: DegradeReason| {
        poller.poll(declared, &inner.scope(), inner.poll.verify_bytes, true);
        inner.note_poll(poller);
        inner.lock_acc().enter_rescan_for_listeners(reason);
        inner.notify();
    };
    tracing::warn!("workspace change hub has no watch; polling the workspace instead");
    map(&mut poller, &declared, DegradeReason::WatcherSetup);
    // The age of the stand this thread holds: the set it started on is the world as of
    // startup, so anything a message carries is newer than it.
    let mut applied_epoch: u64 = 0;
    let mut due = Instant::now() + inner.poll.period;
    loop {
        let now = Instant::now();
        if now >= due {
            poll_once(inner, &mut poller, &declared);
            due = Instant::now() + inner.poll.period;
            continue;
        }
        match rx.recv_timeout(due - now) {
            Ok(HubMsg::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return;
            }
            Ok(HubMsg::Rearm { targets, epoch, ack }) => {
                if epoch < applied_epoch {
                    tracing::info!(
                        epoch,
                        applied = applied_epoch,
                        "workspace change hub ignoring a declaration older than the applied one"
                    );
                    let _ = ack.try_send(false);
                    continue;
                }
                applied_epoch = epoch;
                // The same rule the watching thread applies: a declaration equal to the one
                // in force costs nothing. Re-taking the picture would throw away the map the
                // poll compares against and owe every consumer a reconcile for a set that
                // did not move — and that reconcile is what rebuilds the graph, which
                // declares again.
                let resolved = ResolvedTargets::here(targets);
                if !same_declaration(resolved.as_slice(), declared.as_slice()) {
                    declared = repoint_polling(inner, resolved);
                    poller = Poller::default();
                    map(&mut poller, &declared, DegradeReason::Rearmed);
                }
                // Never "covered": polling is what the hub does when it could not watch.
                let _ = ack.try_send(false);
            }
            Ok(HubMsg::Rescope { user_excluded, epoch, ack }) => {
                if epoch < applied_epoch {
                    tracing::info!(
                        epoch,
                        applied = applied_epoch,
                        "workspace change hub ignoring a rescope older than the applied one"
                    );
                    let _ = ack.try_send(false);
                    continue;
                }
                applied_epoch = epoch;
                *inner.user_excluded.write().unwrap_or_else(PoisonError::into_inner) =
                    user_excluded;
                declared = repoint_polling(inner, ResolvedTargets::here(declared));
                poller = Poller::default();
                map(&mut poller, &declared, DegradeReason::Rearmed);
                let _ = ack.try_send(false);
            }
            #[cfg(test)]
            Ok(HubMsg::Tick) => {
                poll_once(inner, &mut poller, &declared);
                due = Instant::now() + inner.poll.period;
            }
            Ok(HubMsg::Event(_)) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn repoint_polling(inner: &HubInner, resolved: ResolvedTargets) -> Vec<WatchTarget> {
    inner.set_scope(inner.scope_from(&resolved));
    let declared = resolved.into_inner();
    inner.accept_declaration(&declared);
    declared
}

fn poll_once(inner: &HubInner, poller: &mut Poller, declared: &[WatchTarget]) {
    let records = poller.poll(declared, &inner.scope(), inner.poll.verify_bytes, false);
    if !records.is_empty() {
        let mut acc = inner.lock_acc();
        for (canonical, raw, kind) in records {
            acc.record(canonical, raw, kind);
        }
        drop(acc);
        inner.notify();
    }
    inner.note_poll(poller);
}

/// Arm the watch over every target and pump events (and control messages) until
/// shutdown. Runs on its own thread so `start` returns without blocking on the
/// initial (potentially huge) directory walks.
fn run_hub_thread(
    inner: Arc<HubInner>,
    targets: Vec<WatchTarget>,
    event_tx: std::sync::mpsc::SyncSender<HubMsg>,
    rx: std::sync::mpsc::Receiver<HubMsg>,
    before_arm: Option<BeforeArm>,
    watch_refusal: Option<WatchRefusal>,
) {
    let callback_inner = Arc::clone(&inner);
    let watcher = RecommendedWatcher::new(
        move |res| {
            // Never block the notify thread: drop-and-flag on a full channel and
            // let the hub thread fold that into a reconcile.
            if event_tx.try_send(HubMsg::Event(res)).is_err() {
                callback_inner.channel_overflow.store(true, Ordering::Relaxed);
            }
        },
        NotifyConfig::default(),
    );

    let mut watcher = match watcher {
        Ok(backend) => Watch::new(backend, watch_refusal, &inner),
        Err(error) => {
            tracing::warn!("workspace change hub failed to create watcher: {error}");
            let targets = ResolvedTargets::here(targets);
            inner.set_scope(inner.scope_from(&targets));
            inner.mark_polling_setup_failed();
            run_polling(&inner, targets.into_inner(), &rx);
            return;
        }
    };

    // `armed` holds each successfully-watched target with its canonical path
    // CAPTURED AT WATCH TIME — later comparisons must never re-canonicalize a raw
    // spelling (a retargeted symlink would then claim coverage it lost). A target
    // whose `watch()` failed is deliberately NOT recorded, so a later re-arm onto
    // the same set retries it instead of assuming coverage.
    // Re-derive the scope here, on the thread that is about to arm the watch, from
    // targets whose relative spellings are already placed: the backend receives an
    // absolute path and never reads the process-wide current directory again, so
    // the scope cannot end up describing a different tree than the one armed.
    let mut armed: Vec<ArmedTarget> = Vec::new();
    let targets = ResolvedTargets::here(targets);
    if !targets.is_complete() {
        inner.note_unplaced_targets();
    }
    inner.set_scope(inner.scope_from(&targets));
    // The declared set, kept for the life of the thread: `dedup_targets` below drops
    // whatever a recursive ancestor absorbs or a canonical duplicate collapses, and
    // either can become a target in its own right when a link is retargeted. A set
    // rebuilt from the survivors could never bring those back.
    let mut declared = targets.as_slice().to_vec();
    // Taken BEFORE anything is armed. A snapshot taken afterwards would describe the
    // tree the watcher ended up on, so a retarget racing the arming pass would read as
    // agreement forever; taken before, the same race costs one extra re-arm.
    let mut snapshot = snapshot_of(&declared, &Snapshot::new());
    if let Some(before_arm) = before_arm {
        before_arm();
    }
    for (target, _) in dedup_targets(targets.into_inner()) {
        let candidate = ArmedTarget::arming(target, ArmOrigin::Declared);
        match watcher.arm(&candidate.target().path, candidate.target().mode()) {
            Ok(()) => {
                tracing::info!(root = ?candidate.target().path, recursive = candidate.target().recursive, "workspace change hub watching root");
                // No debt for the window between one arm and the next, though on FSEvents
                // that window is real: arming the second target restarts the stream the
                // first one started. Nothing was lost that anybody has to go looking for —
                // a consumer reaching the hub at startup has no baseline yet and builds one
                // by walking the tree, which covers this window and every earlier moment
                // besides. Owing a reconcile here would turn every boot with two declared
                // targets into a cold one, which is what
                // `a_second_boot_over_a_matching_cache_declares_every_source_root_to_the_hub`
                // measures and refuses.
                armed.push(candidate);
            }
            // A single unwatchable root (a missing extension dir, an inotify-limit) leaves
            // that subtree to the reconciler rather than failing the whole hub.
            Err(error) => {
                tracing::warn!(root = ?candidate.target().path, "workspace change hub failed to watch root: {error}")
            }
        }
    }
    if armed.is_empty() {
        drop(watcher);
        inner.mark_polling_setup_failed();
        run_polling(&inner, declared, &rx);
        return;
    }
    inner.publish_watched_roots(&armed);
    // The declaration this thread now stands on: a caller asking for the same set again is
    // answered without a message, however much of it the backend managed to take.
    inner.accept_declaration(&declared);
    // Before readiness is announced, so a consumer that waits for it and subscribes is
    // told to reconcile the window it was never watching over.
    refresh_blind_targets(&inner, &declared, &snapshot, &armed);
    inner.mark_watching();

    // The deadline is checked BEFORE reading a message, not derived from a receive
    // timeout: `recv_timeout` hands over whatever is already queued regardless of how
    // long the deadline has been past, so a storm of events would starve the tick
    // indefinitely — on exactly the tree where losing coverage costs the most.
    let mut due = Instant::now() + inner.tick_period;
    // The age of the stand this thread holds: the set it started on is the world as of
    // startup, so anything a message carries is newer than it.
    let mut applied_epoch: u64 = 0;
    loop {
        inner.drain_channel_overflow();
        let now = Instant::now();
        if now >= due {
            coverage_tick(&inner, &mut watcher, &mut armed, &declared, &mut snapshot);
            due = Instant::now() + inner.tick_period;
            continue;
        }
        let msg = match rx.recv_timeout(due - now) {
            Ok(msg) => msg,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        match msg {
            HubMsg::Event(res) => {
                for dir in inner.ingest_event(res) {
                    // Placed first, so the directory is resolved ONCE and by the one rule:
                    // three decisions turn on where it lies, and they must not answer
                    // differently.
                    let candidate =
                        ArmedTarget::arming(WatchTarget::recursive(dir), ArmOrigin::Incidental);
                    let dir = candidate.target().path.clone();

                    // A spelling the DECLARATION holds is the declaration's to re-point,
                    // and the periodic check does that whole: the old registration dropped
                    // before the new one is placed, the record corrected, the defensive
                    // pass, the debt. Doing half of it from here leaves a door's record
                    // beside a declared one that still names the tree it used to reach,
                    // after which the check reads the set as a retarget and pays for the
                    // swap a second time. What it costs to wait is bounded by
                    // `COVERAGE_TICK_PERIOD`, which is the bound this module already
                    // accepts for a root re-pointed in place.
                    if armed.iter().any(|held| held.is_declared() && held.target().path == dir) {
                        continue;
                    }

                    // A door whose spelling no longer leads where its record says names a
                    // registration nothing reaches through it any more, and this is the only
                    // moment anything can still name it: the kernel keys a watch by inode,
                    // so the same path over a new target takes a new descriptor while
                    // `notify` keys its own map by path and forgets the old one. Dropped
                    // BEFORE the question of whether a new watch is worth placing — whether
                    // the new target happens to be covered already says nothing about the
                    // old registration. The record goes with it: from here on it names
                    // nothing.
                    let replacing = armed.iter().any(|entry| {
                        !entry.is_declared()
                            && entry.target().path == dir
                            && !entry.still_leads_where_recorded()
                    });
                    if replacing {
                        if let Err(error) = watcher.disarm(&dir) {
                            tracing::debug!(root = ?dir, "workspace change hub unwatch of a door being replaced: {error}");
                        }
                        // The record is NOT dropped here. It is stale on purpose from this
                        // moment: it names where the door used to lead, which is what makes
                        // the periodic check see a difference and try again. Dropping it
                        // would leave a failed arm with nothing to retry from — no
                        // declaration names a door, and a link that merely stands there
                        // fires no event, so the watch would be lost for the life of the
                        // daemon. A successful arm replaces it a few lines below.
                        //
                        // Paid HERE, not after the arm. The registration is already gone —
                        // on FSEvents the unwatch rebuilt the stream from "now" and on
                        // inotify the descriptors went — while whether a new watch is worth
                        // placing is a later question that may end this round without ever
                        // reaching the arm.
                        inner.note_arming_window(&dir);
                    }

                    // A recursive record over a directory armed narrowly does not cover a
                    // newcomer placed right in it: the backend watch there is non-recursive.
                    let covered = an_armed_recursive_watch_reaches(&armed, candidate.resolved())
                        && !watcher.is_in_narrowed(&dir);
                    if !watch_is_additive_and_needed(covered) {
                        continue;
                    }
                    let recorded = an_arm_already_recorded_covers(&armed, &candidate);
                    match watcher.arm(&dir, RecursiveMode::Recursive) {
                        Ok(()) => {
                            // A replacement is a window on EVERY backend — the old
                            // registration is gone and the new one starts from now — so the
                            // debt does not wait on the platform question.
                            if arming_restarts_the_stream() {
                                inner.note_arming_window(&dir);
                            }
                            if !recorded {
                                record_arm(&mut armed, candidate);
                            }
                            if replacing {
                                restore_watches_beneath(&inner, &mut watcher, &mut armed, &dir);
                            }
                        }
                        Err(error) => {
                            // The door itself has nothing to take back: a replacement
                            // dropped both the old registration and its record above, and a
                            // first arm placed neither. What DOES have to be put back is
                            // everything the unwatch took from beneath it — the arm that
                            // would have re-walked that subtree is the one that just failed.
                            if replacing {
                                restore_watches_beneath(&inner, &mut watcher, &mut armed, &dir);
                            }
                            inner.note_rewatch_failed(&dir, &error);
                        }
                    }
                }
            }
            HubMsg::Rescope { user_excluded, epoch, ack } => {
                if epoch < applied_epoch {
                    tracing::info!(
                        epoch,
                        applied = applied_epoch,
                        "workspace change hub ignoring a rescope older than the applied one"
                    );
                    let _ = ack.try_send(false);
                    continue;
                }
                applied_epoch = epoch;
                let covered = apply_rescope(
                    &inner,
                    &mut watcher,
                    &mut armed,
                    &declared,
                    &snapshot,
                    user_excluded,
                );
                let _ = ack.try_send(covered);
            }
            HubMsg::Rearm { targets, epoch, ack } => {
                if epoch < applied_epoch {
                    // A slow build finishing after a newer one: its composition is older
                    // than the one in force, and applying it would roll the hub back onto
                    // the roots the newer build left behind (github#184). The caller-side
                    // freshness guard has already let it through — check-then-send is a
                    // race by construction — so the ordering has to be decided here, on
                    // the one thread that owns the watch set. "Not covered" is the honest
                    // answer: the declaration was not applied.
                    tracing::info!(
                        epoch,
                        applied = applied_epoch,
                        "workspace change hub ignoring a declaration older than the applied one"
                    );
                    let _ = ack.try_send(false);
                    continue;
                }
                applied_epoch = epoch;
                // One path for every declaration the thread receives. `apply_declaration`
                // decides what the declaration is worth: an unchanged one costs nothing, one
                // that moves no coverage is recorded without a re-arm, and only a real move
                // re-arms and owes the reconcile that comes with it.
                let covered = apply_declaration(
                    &inner,
                    &mut watcher,
                    &mut armed,
                    &mut declared,
                    &mut snapshot,
                    targets,
                );
                // `try_send`, not `send`: the requester may have timed out and
                // dropped its receiver; the hub thread must never block on it.
                let _ = ack.try_send(covered);
            }
            #[cfg(test)]
            HubMsg::Tick => {
                coverage_tick(&inner, &mut watcher, &mut armed, &declared, &mut snapshot);
                due = Instant::now() + inner.tick_period;
            }
            HubMsg::Shutdown => return,
        }
    }
}

/// Re-check that the declared coverage is still the coverage in force, and re-arm the
/// whole set when it is not.
///
/// The tick decides, `apply_rearm` acts. Splitting it the other way — unwatching and
/// watching target by target — was tried and abandoned: a recursive `unwatch` takes
/// descendant registrations with it, and a nested target's removal punches a hole in a
/// kept ancestor, so any per-target sequence has to rebuild the guarantees
/// `apply_rearm` already provides by arming additions before removals and defensively
/// re-watching everything it keeps.
fn coverage_tick(
    inner: &Arc<HubInner>,
    watcher: &mut Watch,
    armed: &mut Vec<ArmedTarget>,
    declared: &[WatchTarget],
    snapshot: &mut Snapshot,
) {
    // First, before a single arm in this tick: a watch dropped after one would take, on
    // inotify, the registrations that arm had just placed beneath it. Counted as movement,
    // so the re-arm below pays for the window the unwatch cost and puts back whatever it
    // stripped — the same bargain `apply_rearm` makes for every other unwatch it issues.
    let doors_gone = drop_doors_that_are_gone(watcher, armed);
    rearm_doors_that_moved(inner, watcher, armed);
    let current = snapshot_of(declared, snapshot);
    // A door that went costs a re-arm only where the unwatch cost the stream: there the
    // window has to be paid for and whatever the swap dropped put back. Where it does not,
    // the only registrations it took were reachable through the very path that is gone, so
    // nothing is owed and nothing needs restoring.
    let moved = coverage_moved(declared, snapshot, &current)
        || (doors_gone && unwatching_restarts_the_stream());
    // Stored on BOTH branches. "Coverage did not move" is a statement about the targets
    // IN the cover; the ones outside it can still have changed, and blindness is read off
    // this snapshot. A target first described as undescribable and later proven gone moves
    // no coverage either way, so keeping the old description would leave it blind for ever
    // over a path that no longer exists. Nothing else drifts: when the check says the
    // cover did not move, every fingerprint inside it is equal by definition.
    *snapshot = current;
    if moved {
        // Already absolute, so placing them again cannot move them; going through the
        // one constructor keeps that the only way a target reaches the watcher.
        apply_rearm(inner, watcher, armed, ResolvedTargets::here(declared.to_vec()));
        // Counted AFTER the watch is in place, for the same reason as the tick below:
        // a test that waits for this counter and then makes a one-shot change would
        // otherwise make it inside the window where the new target is not armed yet.
        inner.rearms.fetch_add(1, Ordering::Relaxed);
    } else {
        retry_blind_targets(inner, watcher, armed, declared, snapshot);
    }
    refresh_blind_targets(inner, declared, snapshot, armed);
    // Counted LAST, after every effect above is visible: a test that waits for this
    // counter is told the tick finished, not that it started.
    inner.ticks.fetch_add(1, Ordering::Relaxed);
}

/// Try again on the declared targets the watcher does not hold, without disturbing the
/// ones it does.
///
/// A separate path, not a re-arm: the obstacles that leave a target unwatched — a denied
/// permission, an exhausted inotify limit — clear without touching a single fingerprint,
/// so the coverage check sees nothing to react to and would never retry at all. Going
/// through `apply_rearm` instead would be worse than useless here: it unwatches, and a
/// recursive `unwatch` of an overlapping root strips descendant registrations, so a
/// periodic re-arm would keep paying that risk for a target that is merely missing.
/// This path only ever ADDS, so it takes nothing away from what is already covered.
///
/// Success is new coverage after a blind window — everything under the target changed
/// unobserved for as long as it stayed blind — and is worth exactly one reconcile for the
/// whole batch. Failure leaves nothing at all behind: no reconcile, no publication, no
/// repeat of a report consumers already have.
fn retry_blind_targets(
    inner: &HubInner,
    watcher: &mut Watch,
    armed: &mut Vec<ArmedTarget>,
    declared: &[WatchTarget],
    snapshot: &Snapshot,
) {
    let mut armed_any = false;
    for (target, _) in blind_targets(declared, snapshot, armed) {
        // Placed through the one constructor, like every other arming path: a target the
        // snapshot could not describe has no resolved path to carry, and one it could may
        // have moved since, so the truthful spelling is the one taken now — by the rule
        // the three comparisons that read it all expect.
        let candidate = ArmedTarget::arming(target, ArmOrigin::Declared);
        match watcher.arm(&candidate.target().path, candidate.target().mode()) {
            Ok(()) => {
                tracing::info!(root = ?candidate.target().path, recursive = candidate.target().recursive, "workspace change hub watching root (retry)");
                record_arm(armed, candidate);
                armed_any = true;
            }
            Err(error) => {
                tracing::debug!(root = ?candidate.target().path, "workspace change hub retry still cannot watch root: {error}")
            }
        }
    }
    if !armed_any {
        return;
    }
    inner.publish_watched_roots(armed);
    // Owed to whoever was listening across the blind window: a cursor taken after the retry
    // begins where that window ended.
    inner.lock_acc().enter_rescan_for_listeners(DegradeReason::Rearmed);
    inner.notify();
}

/// Apply a declaration and report whether the hub covers it.
///
/// Three outcomes, and which one a declaration gets is decided HERE rather than by its
/// sender. A declaration equal to the one in force is answered from what the hub already
/// holds: nothing is armed, nothing is re-pictured, nothing is owed. One that moves no
/// coverage is recorded without a re-arm. Only a real move re-arms, and only then does a
/// consumer owe a reconcile.
///
/// The sender's belief about coverage is not trusted: it compared on ITS thread, and a
/// target absorbed by an ancestor at that moment can be standing on its own by the time
/// this runs. The snapshot is therefore taken FIRST — before the cover is recomputed — so a
/// retarget inside that window reads as movement instead of being recorded as the
/// starting state, and the decision to arm uses the same rule the tick uses.
fn apply_declaration(
    inner: &Arc<HubInner>,
    watcher: &mut Watch,
    armed: &mut Vec<ArmedTarget>,
    declared: &mut Vec<WatchTarget>,
    snapshot: &mut Snapshot,
    targets: Vec<WatchTarget>,
) -> bool {
    let resolved = ResolvedTargets::here(targets);
    let resolved_complete = resolved.is_complete();
    let next = resolved.as_slice().to_vec();
    if same_declaration(&next, declared) {
        // The second barrier, and the one that holds even when a caller asks directly. The
        // first is `ensure_roots`, which does not send this message at all for a repeat.
        return declared_coverage(declared, resolved_complete, armed);
    }
    // Merged, not replaced: a drift that already happened to a surviving target must
    // survive this update, and a target seen for the first time gets described now.
    let current = snapshot_of(&next, snapshot);
    // Both movement checks read one declared set against two snapshots, so neither can see
    // the declaration itself hand the cover from one alias to another: that is what the
    // third asks, against the watch as it really stands.
    let moved = coverage_moved(declared, snapshot, &current)
        || coverage_moved(&next, snapshot, &current)
        || cover_differs_from_armed(&cover_from_snapshot(&next, &current), armed);
    *snapshot = current;
    *declared = next;
    inner.set_scope(inner.scope_from(&resolved));
    if moved {
        // Denies coverage on an unplaced target itself, so the branch below is the only
        // one left without that report.
        apply_rearm(inner, watcher, armed, resolved);
        inner.rearms.fetch_add(1, Ordering::Relaxed);
    } else {
        if !resolved.is_complete() || !declared_coverage(declared, true, armed) {
            // A declaration this pass did not arm in full — a target that could not be
            // placed, or one that names a directory which does not exist — silently narrows
            // what the hub can see, and the caller already counts the declaration as
            // delivered. So it is reported here, like at every other placement point, rather
            // than left to look like agreement.
            //
            // Reported on the CHANGE only: this branch is past the equality check above, so a
            // repeat of the same declaration says nothing a second time. That is the whole
            // difference between announcing a gap and re-announcing it on every rebuild.
            inner.note_unplaced_targets();
        }
        // Only on this branch, and only because nothing was armed on it: a scope can narrow
        // without the cover moving at all — a root carved back out of an exclusion is
        // absorbed by its recursive ancestor and never reaches the cover — and a door
        // inside what the scope has stopped walking is a registration nothing else will
        // ever drop. On the other branch `apply_rearm` has already done it, and doing it
        // again there would put an unwatch AFTER the defensive pass, which on inotify
        // strips exactly what that pass had just restored.
        let dropped = drop_doors_the_scope_stopped_reaching(watcher, armed, &inner.scope());
        // And its own defensive pass, because this branch has no other. A recursive unwatch
        // takes the registrations beneath it by SPELLING on inotify, and a declared root can
        // be spelled under a door while resolving somewhere else entirely — which is exactly
        // why it stands as a registration of its own instead of being absorbed. Its record
        // survives the strip, so nothing downstream would ever notice: the blind set reads
        // the set, not the backend.
        if !dropped.is_empty() && a_kept_target_must_be_re_armed() {
            let mut lost: Vec<PathBuf> = Vec::new();
            let mut restored: Vec<ArmedTarget> = Vec::with_capacity(armed.len());
            for entry in armed.drain(..) {
                if !entry.is_declared() {
                    restored.push(entry);
                    continue;
                }
                match watcher.arm(&entry.target().path, entry.target().mode()) {
                    Ok(()) => restored.push(entry),
                    // DROPPED on failure, exactly as the same pass drops one in
                    // `apply_rearm`: the set is read as "already covered", so a record left
                    // over a watch that may be gone keeps the blind set silent and the retry
                    // away for ever. Dropping it is what lets the refresh below report the
                    // root and the periodic check put it back.
                    Err(error) => {
                        tracing::warn!(root = ?entry.target().path, "workspace change hub lost a root while restoring it after dropping a watch above it: {error}");
                        // And the registration goes with the record (github#185), once the
                        // pass knows what it ends with — the twin of `apply_rearm`'s drop.
                        lost.push(entry.target().path.clone());
                    }
                }
            }
            unwatch_the_lost(watcher, &lost, &restored);
            *armed = restored;
            // Republished because the set changed here: `ensure_roots` compares the live
            // list against what it is about to declare, and a root left in it after its
            // record was dropped would answer the next identical declaration "already
            // covered" over a subtree nothing is watching.
            inner.publish_watched_roots(armed);
        }
        if !dropped.is_empty() {
            // The unwatch above took whatever lay beneath it and the pass put it back, and
            // between the two nothing was watching — the same window `apply_rearm` pays for
            // at its end, for the same reason, so it is owed here too.
            inner.lock_acc().enter_rescan_for_listeners(DegradeReason::Rearmed);
            inner.notify();
        }
    }
    // On BOTH branches. A declaration that re-arms nothing is exactly how a blind target
    // leaves the set: outside the cover, so dropping it moves no coverage at all, and a
    // set reconciled only inside `apply_rearm` would hold ill health over it forever.
    refresh_blind_targets(inner, declared, snapshot, armed);
    // Only now, and only by the thread: from here an identical declaration is answered
    // without a message. Recorded after the arming above, so a repeat can never be answered
    // "already declared" over a set this pass has not finished placing.
    inner.accept_declaration(declared);
    // Read off the declaration as it now stands: `apply_rearm` consumed the resolved set, and
    // re-resolving here would answer about a tree that may have moved since.
    declared_coverage(declared, resolved_complete, armed)
}

/// Take a new `[source].exclude` and re-place every registration under it.
///
/// The declared set does not move, so nothing in [`apply_declaration`] would re-arm it,
/// yet the coverage it stands for has: a recursive watch that was fine may now reach
/// an exclusion, and a narrowed one may no longer need to be. Every standing registration
/// is dropped and placed again through the backend door, which narrows by the new list;
/// a watch no declaration names is kept only while the new scope still walks it. The
/// window between the two is owed to the listeners like any re-arm.
fn apply_rescope(
    inner: &Arc<HubInner>,
    watcher: &mut Watch,
    armed: &mut Vec<ArmedTarget>,
    declared: &[WatchTarget],
    snapshot: &Snapshot,
    user_excluded: ExcludedPaths,
) -> bool {
    *inner.user_excluded.write().unwrap_or_else(PoisonError::into_inner) = user_excluded;
    let resolved = ResolvedTargets::here(declared.to_vec());
    let placed = resolved.is_complete();
    let scope = inner.scope_from(&resolved);
    inner.set_scope(scope.clone());
    for entry in armed.iter() {
        if let Err(error) = watcher.disarm(&entry.target().path) {
            tracing::debug!(root = ?entry.target().path, "workspace change hub unwatch on rescope: {error}");
        }
    }
    let mut kept: Vec<ArmedTarget> = Vec::with_capacity(armed.len());
    for entry in armed.drain(..) {
        if !entry.is_declared() && !the_scope_still_reaches(&scope, &entry) {
            continue;
        }
        match watcher.arm(&entry.target().path, entry.target().mode()) {
            Ok(()) => kept.push(entry),
            Err(error) => {
                tracing::warn!(root = ?entry.target().path, "workspace change hub lost a watch on rescope: {error}");
                if !entry.is_declared() {
                    inner.note_rewatch_failed(&entry.target().path, &error);
                }
            }
        }
    }
    *armed = kept;
    inner.publish_watched_roots(armed);
    refresh_blind_targets(inner, declared, snapshot, armed);
    inner.lock_acc().enter_rescan_for_listeners(DegradeReason::Rearmed);
    inner.notify();
    declared_coverage(declared, placed, armed)
}

/// Whether every declared target is placed and armed. The same question `ensure_roots` asks
/// of the published list, asked here against the live one.
fn declared_coverage(declared: &[WatchTarget], placed: bool, armed: &[ArmedTarget]) -> bool {
    if !placed {
        return false;
    }
    dedup_targets(declared.to_vec()).into_iter().all(|(target, canonical)| {
        armed.iter().any(|entry| {
            entry.is_declared()
                && entry.resolved() == canonical
                && entry.target().recursive == target.recursive
        })
    })
}

/// Whether arming `dir` is worth what the backend charges for it.
///
/// A question about ONE backend's mechanics, not about recursion in general — so the
/// answer is per platform, and the platform is what the two bodies below select on.
///
/// FSEvents: a single kernel stream watches whole subtrees, and `watch` does not extend
/// a running one. `fsevent::watch_inner` stops the stream, rebuilds it over the widened
/// path list and starts the new one from "now" (`kFSEventStreamEventIdSinceNow`), so
/// every change ANYWHERE in the tree during that swap is dropped and never reported
/// again. A directory that appears under an armed recursive root is already inside that
/// stream's subtree, so arming it registers nothing new and pays a blind window for it —
/// and one arm per created directory turns a checkout of a large configuration into
/// thousands of blind windows over the very tree the hub exists to follow.
///
/// inotify: NOT recursive. `RecursiveMode::Recursive` is emulated — the backend walks
/// the tree and registers each directory separately, and a directory created later is
/// covered only once the backend has learnt of it from an event and registered it in
/// turn. So the arm here is what creates the registration, nothing is torn down to do
/// it, and skipping it would take away coverage rather than a redundancy. That is the
/// whole reason this is gated instead of applied everywhere: extending the skip to
/// inotify would trade a wasted arm for silent blindness.
///
/// Takes the coverage answer rather than deriving it, because the caller needs the same
/// answer for a second decision — whether the registration this arm places is one the
/// declared set accounts for — and one path resolved twice is one that can be resolved two
/// ways.
#[cfg(target_os = "macos")]
fn watch_is_additive_and_needed(already_covered: bool) -> bool {
    !already_covered
}

#[cfg(not(target_os = "macos"))]
fn watch_is_additive_and_needed(_already_covered: bool) -> bool {
    true
}

/// Drop every watch no declaration names whose SPELLING has gone from disk, and say
/// whether any went.
///
/// A door is reached only through its own spelling: no declaration names it, and a re-arm
/// builds only from what is declared. Once the link itself is gone nothing will ever
/// deliver an event under it again, so the registration it left behind is one only this can
/// name — and a link put back in its place fires a create, which arms and records it
/// afresh.
///
/// ABSENCE is the test, deliberately, and not where the spelling leads. A door whose tree
/// merely moved out from under it is still a door: forgetting it there would leave nothing
/// able to name that registration when the tree came back, and no event would ever say so,
/// because the link itself never changed.
fn drop_doors_that_are_gone(watcher: &mut Watch, armed: &mut Vec<ArmedTarget>) -> bool {
    let mut gone: Vec<PathBuf> = Vec::new();
    armed.retain(|entry| {
        if entry.is_declared() || !the_spelling_is_gone(&entry.target().path) {
            return true;
        }
        gone.push(entry.target().path.clone());
        false
    });
    for path in &gone {
        if let Err(error) = watcher.disarm(path) {
            tracing::debug!(root = ?path, "workspace change hub unwatch of a watch whose path is gone: {error}");
        }
    }
    !gone.is_empty()
}

/// Re-point every watch no declaration names whose door no longer leads where its record
/// says.
///
/// The registration stands on what the door reached when it was armed. A link re-pointed
/// with nothing to say so, or a target removed and recreated under one name, leaves it there
/// while the door leads somewhere else — and nothing else would ever notice, because no
/// declaration names a door and a link that merely stands there fires no event. This is the
/// only pass that can put it right.
///
/// A door leading NOWHERE is left exactly as it stands: the arm would fail, and the record
/// is both the only handle to the registration and the only thing that will notice when the
/// tree comes back.
fn rearm_doors_that_moved(inner: &HubInner, watcher: &mut Watch, armed: &mut Vec<ArmedTarget>) {
    let moved: Vec<WatchTarget> = armed
        .iter()
        .filter(|entry| !entry.is_declared() && !entry.still_leads_where_recorded())
        .map(|entry| entry.target().clone())
        .collect();
    for target in &moved {
        if let Err(error) = watcher.disarm(&target.path) {
            tracing::debug!(root = ?target.path, "workspace change hub unwatch of a door being re-pointed: {error}");
        }
        // The record stays across the attempt, for the reason the event branch keeps it:
        // stale is what makes the next check try again, and a successful arm replaces it.
        let candidate = ArmedTarget::arming(target.clone(), ArmOrigin::Incidental);
        if !candidate.resolved().is_dir() {
            // The door leads nowhere a watch can be placed — a dangling link, or one now
            // pointing at a file. The registration is dropped all the same, because it goes
            // on delivering for a tree the door has stopped reaching and every one of those
            // events arrives spelled as though it were still inside the workspace. The
            // RECORD stays, now describing where the door leads (nowhere), so the next check
            // sees the difference the moment a directory appears there again — and so
            // something is still able to name the watch when it does.
            record_arm(armed, candidate);
            inner.note_arming_window(&target.path);
            continue;
        }
        match watcher.arm(&target.path, target.mode()) {
            Ok(()) => {
                // A replacement is a window on every backend: the old registration is gone
                // and the new one starts from now.
                inner.note_arming_window(&target.path);
                record_arm(armed, candidate);
                restore_watches_beneath(inner, watcher, armed, &target.path);
            }
            Err(error) => {
                // The arm that would have re-walked what lies under this spelling is the one
                // that failed, so nothing else will put back what the unwatch took from
                // beneath it.
                restore_watches_beneath(inner, watcher, armed, &target.path);
                // Logged, not reported. The transition into this state was reported once
                // already, where the door first failed to move; repeating it every period
                // would buy every consumer a full walk each period for as long as the
                // obstacle lasts — the precise cost `refresh_blind_targets` refuses for the
                // same reason. The record stays, so the next check tries again.
                tracing::debug!(root = ?target.path, "workspace change hub still cannot re-point a watch no declaration names: {error}");
            }
        }
    }
}

/// Place again every watch recorded UNDER a path that was just handed to `unwatch`.
///
/// A recursive unwatch takes the registrations whose spelling begins with the one given, and
/// the arm that follows it re-walks only what lies behind the path itself. Anything else
/// recorded beneath that spelling — a door revealed inside another door, a declared root
/// written under one — has to be placed again, or the set goes on naming registrations the
/// backend no longer holds. A watch that cannot be placed is DROPPED for the same reason: a
/// record is read as a registration that can still be dropped, and one over a watch that is
/// gone can never be made true.
///
/// Asked of the platform first, because where an unwatch takes only what it was given there
/// is nothing beneath it to restore.
fn restore_watches_beneath(
    inner: &HubInner,
    watcher: &mut Watch,
    armed: &mut Vec<ArmedTarget>,
    dropped: &Path,
) {
    if !an_unwatch_takes_what_lies_beneath_it() {
        return;
    }
    let mut lost: Vec<PathBuf> = Vec::new();
    for entry in armed.iter() {
        let path = &entry.target().path;
        if path == dropped || !path.starts_with(dropped) {
            continue;
        }
        if let Err(error) = watcher.arm(path, entry.target().mode()) {
            tracing::warn!(root = ?path, "workspace change hub lost a watch that lay under one it dropped: {error}");
            lost.push(path.clone());
        }
    }
    if lost.is_empty() {
        return;
    }
    armed.retain(|entry| !lost.contains(&entry.target().path));
    // Republished because the set changed: `ensure_roots` compares the live list against
    // what it is about to declare, and a root left in it after its record was dropped would
    // answer the next identical declaration "already covered" over a subtree nothing is
    // watching.
    inner.publish_watched_roots(armed);
}

/// Where a spelling itself lies, with the leaf left as written.
///
/// Not where it LEADS. A door is a link, and following it answers with the tree behind it;
/// what is asked here is whether the door itself is still inside a declared tree. A root
/// re-declared under an equivalent spelling — another link to the same directory — moves
/// that answer only when it is read physically, and a door judged on the dropped spelling
/// alone is dropped with it while the tree behind it stays perfectly reachable and
/// perfectly unwatched.
fn where_the_spelling_lies(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolve_as_far_as_it_goes(parent).join(name),
        _ => resolve_as_far_as_it_goes(path),
    }
}

/// Whether the scope still leads to a watch no declaration names, and the watch is still
/// nameable at all.
///
/// Two questions, and NOT a third. Where the spelling now leads is deliberately not asked:
/// a door whose target has stepped aside for a moment — a rebuild renaming a directory and
/// putting it back — would answer no, and dropping it there destroys the only record able to
/// re-point it, while the link itself never changed and will never fire another event. That
/// state is the periodic check's to mend, and it mends it by re-pointing rather than by
/// forgetting. What a re-arm does own is the spelling that is GONE: nothing will ever
/// deliver under it again, so the registration behind it is one only this can still name.
fn the_scope_still_reaches(scope: &Scope, entry: &ArmedTarget) -> bool {
    let path = &entry.target().path;
    (scope.may_walk(path) || scope.may_walk(&where_the_spelling_lies(path)))
        && !the_spelling_is_gone(path)
}

/// Whether a path is PROVABLY absent, as against merely impossible to look at.
///
/// A parent made unsearchable for a moment answers `PermissionDenied`, and taking that for a
/// removal would drop the only handle to a live registration over a link that never changed
/// — after which no event would ever say so. The distinction `fingerprint_of` draws, for the
/// same reason.
fn the_spelling_is_gone(path: &Path) -> bool {
    // And the absence has to be about THIS spelling. A `NotFound` on a nested path proves
    // only that something on the way is missing — an ancestor link whose target has stepped
    // aside answers exactly that — and forgetting the record there destroys the only thing
    // able to re-point it once the way back opens. So the parent must be reachable, THROUGH
    // its links, before the leaf's absence means anything.
    let reached = path.parent().is_some_and(|parent| parent.metadata().is_ok());
    reached && path.symlink_metadata().is_err_and(|error| target_cannot_exist(error.kind()))
}

/// Drop every watch no declaration names that the scope has stopped reaching, and hand
/// each one to `unwatch`. Gives back what it dropped.
///
/// A door lives on two conditions, and losing either ends it: the scope must still walk the
/// spelling — that is what a declaration reaching a door means — and the spelling must still
/// be there at all, because nothing will ever deliver under one that is gone. Where the
/// spelling now LEADS is deliberately not among them; [`the_scope_still_reaches`] says why,
/// and the periodic check is what mends that state, by re-pointing rather than forgetting.
fn drop_doors_the_scope_stopped_reaching(
    watcher: &mut Watch,
    armed: &mut Vec<ArmedTarget>,
    scope: &Scope,
) -> Vec<PathBuf> {
    let mut dropped: Vec<PathBuf> = Vec::new();
    armed.retain(|entry| {
        if entry.is_declared() || the_scope_still_reaches(scope, entry) {
            return true;
        }
        dropped.push(entry.target().path.clone());
        false
    });
    for path in &dropped {
        if let Err(error) = watcher.disarm(path) {
            tracing::debug!(root = ?path, "workspace change hub unwatch of a watch no declaration names: {error}");
        }
    }
    dropped
}

/// Whether a target the watcher IS holding recursively already reaches `path`.
///
/// One implementation, because two callers ask it and they must not answer differently:
/// the blind set decides whether a declared target it cannot describe is nonetheless
/// being watched, and the re-watch decides whether a directory that just appeared needs
/// arming at all. The same armed set, the same question — and a disagreement would mean
/// one of them reporting a subtree as unwatched while the other declines to watch it.
///
/// Asked of where the two paths LIE, not of how they are written. A recursive watch
/// covers a file-system subtree, and neither backend follows links out of it: a symlink
/// (or a mount point) created inside a watched root is a door into another tree, and
/// everything behind it is unwatched however plainly the spelling reads as "inside".
/// Measured, not assumed — a write in the target of such a link is delivered only once
/// the link itself has been armed. Resolving the candidate first is also what makes the
/// two spellings of the root a non-question: a path written through the root's declared
/// name resolves to the same place as one written through its resolved name.
///
/// The root's side is the resolution CAPTURED AT WATCH TIME and is never re-derived: a
/// root retargeted since then no longer matches, which is the conservative answer —
/// coverage it has actually lost is not claimed.
///
/// Read off the DECLARED watches only. A declared target's resolution is policed: the
/// periodic check re-fingerprints the declaration and re-arms the whole set the moment it
/// moves, so a record of one is a statement about now. Nothing polices a door — no
/// declaration names it, and a symlink that merely stands there fires no event — so a
/// record of one is a statement about the moment it was armed and may since have become
/// false. Letting that answer here would suppress exactly the arm that would have made it
/// true again: the cost of arming a tree twice is one stream swap, the cost of not arming
/// it is every change in it, for ever.
fn an_armed_recursive_target_covers(armed: &[ArmedTarget], path: &Path) -> bool {
    an_armed_recursive_watch_reaches(armed, &resolve_as_far_as_it_goes(path))
}

/// Whether a registration this arm is about to place is one an existing record already
/// answers for — the only ground on which recording it can be skipped.
///
/// About REGISTRATIONS, not about trees, and the difference is the whole point. A record
/// exists so that a re-arm can hand its path to `unwatch`; another record answers for this
/// one exactly when un-watching that one takes this one with it, which is a question about
/// the paths the backend was given. Two doors into ONE tree are two registrations and two
/// spellings: the second lies under neither the first nor any declared root, so nothing
/// would ever drop it, and its watch would outlive every topology that could justify it.
/// A directory revealed BEHIND a door does lie under the door's spelling, and that is what
/// keeps the set from taking a record for every directory a checkout creates.
fn an_arm_already_recorded_covers(armed: &[ArmedTarget], candidate: &ArmedTarget) -> bool {
    if !an_unwatch_takes_what_lies_beneath_it() {
        return false;
    }
    armed.iter().any(|entry| {
        // Inside the other registration BOTH ways: under its spelling, so the unwatch takes
        // it, and inside the tree it reaches, so the arm that follows puts it back. A
        // directory created behind a door — or inside a declared root — satisfies both, and
        // that is what keeps the set from taking a record for every directory a checkout
        // creates. A door revealed INSIDE another registration does not: it leads out of
        // that tree, so re-arming the outer one never places it again, and without a record
        // of its own nothing could ever name what it left behind when its own link is later
        // re-pointed.
        //
        // Declared and incidental alike, because the question is about what an unwatch
        // takes, and the backend does not care why a registration was placed.
        candidate.resolved().starts_with(entry.resolved())
            // The candidate's OWN spelling is excluded, and it is the reason this is a
            // function and not an inline `starts_with`: containment is reflexive, so a
            // record of this very door would otherwise answer "already covered" and send
            // the arm past the one place that refreshes it. The record would then keep the
            // resolution the door had BEFORE it was retargeted, and the re-arm — which
            // reads exactly that to decide what to drop — would unwatch a live door the
            // declaration still reaches, with nothing left able to arm it again.
            && entry.target().path != candidate.target().path
            && candidate.target().path.starts_with(&entry.target().path)
    })
}

/// The same question asked of a path already placed, for the caller that has the
/// resolution in hand and must not read it a second time.
fn an_armed_recursive_watch_reaches(armed: &[ArmedTarget], lies_at: &Path) -> bool {
    armed.iter().any(|entry| {
        entry.is_declared() && entry.target().recursive && lies_at.starts_with(entry.resolved())
    })
}

/// Whether handing a path to `unwatch` takes the registrations placed beneath it.
///
/// A fourth question about one backend's mechanics, and the one that decides when a record
/// may be left out: a registration inside another's reach goes when that one goes, and only
/// then does it need no name of its own.
///
/// inotify: yes. `remove_watch` walks its own map and drops every entry whose PATH begins
/// with the one given, so a directory armed behind a door goes with the door.
///
/// FSEvents: the question does not arise — nothing beneath an armed recursive watch is ever
/// armed separately there, and events behind a door arrive under the physical path, outside
/// every declared root, so no such directory is ever revealed to begin with. Answering `no`
/// costs nothing and keeps the claim to what can be shown.
///
/// Windows: no. Each `ReadDirectoryChangesW` registration stands on its own, so a child
/// armed behind a door survives the door's removal and would be left with no record able to
/// name it — a handle held for the life of the process.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn an_unwatch_takes_what_lies_beneath_it() -> bool {
    true
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn an_unwatch_takes_what_lies_beneath_it() -> bool {
    false
}

/// Whether arming a path costs the stream that was already running.
///
/// A third question about one backend's mechanics, per platform for the same reason as
/// [`watch_is_additive_and_needed`], and the reason a debt is owed on one platform and
/// would be ruinous on the other.
///
/// FSEvents: yes. `fsevent::watch_inner` stops the running stream, rebuilds it over the
/// widened path list and starts the new one from `kFSEventStreamEventIdSinceNow`, so every
/// change ANYWHERE in the watched tree between the stop and the start is dropped and never
/// reported again. Nobody can name what was lost — that is what makes it a window and not
/// an event — so the only honest answer is the one a reconcile gives.
///
/// inotify: no. A registration is added beside the ones already in place; nothing is torn
/// down and nothing is lost. Owing a reconcile here would cost every consumer a full tree
/// walk for each of the thousands of directories a checkout creates, to describe a window
/// that never existed.
#[cfg(target_os = "macos")]
fn arming_restarts_the_stream() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
fn arming_restarts_the_stream() -> bool {
    false
}

/// Whether handing a path to `unwatch` costs the stream that was already running.
///
/// The same mechanics as [`arming_restarts_the_stream`] read from the other end.
///
/// FSEvents: yes — `fsevent::unwatch_inner` stops the stream, rebuilds it over the narrowed
/// path list and starts the new one from "now", so removing one path costs everything that
/// happened anywhere in the tree during the swap.
///
/// inotify: no. `remove_watch` drops the descriptors whose stored path begins with the one
/// given and touches nothing else — and every one of those was reachable only through the
/// path just removed. Nothing that is still reachable was lost, so charging a reconcile
/// there would turn an ordinary symlink churn into a full workspace walk for every
/// consumer.
#[cfg(target_os = "macos")]
fn unwatching_restarts_the_stream() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
fn unwatching_restarts_the_stream() -> bool {
    false
}

/// Whether a target the watcher is ALREADY holding, and which stays in the set, has to
/// be armed a second time after obsolete targets were unwatched.
///
/// Another question about one backend's mechanics, per platform for the same reason as
/// [`watch_is_additive_and_needed`].
///
/// inotify: yes. Registrations are per directory, and a recursive `unwatch` of an
/// obsolete root that overlapped a kept one strips the kept target's descendants along
/// with its own. Arming it again re-registers exactly what was taken, and costs nothing
/// but the walk — no other registration is disturbed.
///
/// FSEvents: no, and it is not merely wasted. There is nothing to restore — an unwatch
/// removes one path from the stream's list and leaves the rest of the subtree covered —
/// while the arm itself rebuilds the whole stream from "now", so every change in the
/// tree during the swap is lost. Measured on a re-arm onto an UNCHANGED set, where this
/// pass is the only watcher call made at all: a directory removed alongside it went
/// undelivered in roughly one run in twelve, with nothing delivered in its place. A
/// re-arm is asked for after every rebuild, so this is a blind window on a schedule.
#[cfg(target_os = "macos")]
fn a_kept_target_must_be_re_armed() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
fn a_kept_target_must_be_re_armed() -> bool {
    true
}

/// Whether handing `path` to `unwatch` would take the registration of any of `records`
/// with it: one of them names a path strictly BENEATH `path`, and the backend strips by
/// spelling ([`an_unwatch_takes_what_lies_beneath_it`]).
///
/// Asked before an unwatch that goes with a dropped record: such an unwatch would take the
/// registrations of records that are staying, and their claims of coverage would then be
/// lies nothing downstream could notice. The leak left by NOT unwatching is the smaller
/// wrong: it belongs to a declared root that is reported blind and put back by the retry,
/// which is exactly what walks the two back into agreement (github#185). Where each
/// registration stands on its own, nothing beneath is at stake, and the unwatch always goes.
fn an_unwatch_would_strip_a_record<'a>(
    records: impl IntoIterator<Item = &'a Path>,
    path: &Path,
) -> bool {
    an_unwatch_takes_what_lies_beneath_it()
        && records.into_iter().any(|other| other != path && other.starts_with(path))
}

/// Unwatch every path in `lost` — records a pass dropped after a refused arm — unless that
/// would strip a registration one of `standing`, the records the pass ends with, still needs.
fn unwatch_the_lost(watcher: &mut Watch, lost: &[PathBuf], standing: &[ArmedTarget]) {
    for path in lost {
        if an_unwatch_would_strip_a_record(
            standing.iter().map(|entry| entry.target().path.as_path()),
            path,
        ) {
            continue;
        }
        if let Err(error) = watcher.disarm(path) {
            tracing::debug!(root = ?path, "workspace change hub unwatch of a lost root: {error}");
        }
    }
}

/// Re-point the watch set at `new_targets`, on the hub thread. Additions are armed
/// BEFORE obsolete targets are unwatched, so a subtree present in both sets has no
/// unwatched window; every surviving target is then defensively re-armed, because a
/// recursive `unwatch` of an overlapping old root can deregister a kept target's
/// descendants on inotify. Comparison uses each armed target's canonical path
/// CAPTURED AT WATCH TIME — a symlink retargeted since then must read as "not
/// covered" and be re-armed, not silently claimed. Every cursor is then flagged to
/// rescan once: anything a consumer derived under the old set predates the new
/// targets' coverage, and events inside a newly-added root from before its arm were
/// never delivered. A watch no declaration names — a door an event revealed — is neither
/// re-armed nor disarmed with the declared targets: it is kept while the new scope still
/// walks it and dropped when it does not. Returns whether EVERY desired target is armed
/// afterwards.
fn apply_rearm(
    inner: &HubInner,
    watcher: &mut Watch,
    armed: &mut Vec<ArmedTarget>,
    new_targets: ResolvedTargets,
) -> bool {
    // Scope follows the DESIRED set, before de-duplication: a target absorbed by a
    // recursive ancestor is still part of what the hub watches for.
    //
    // Taking targets already placed, rather than placing them here, is what keeps the
    // caller's snapshot and the armed watch describing one tree.
    // A target that could not be placed is absent from the desired set, so the
    // arming loop below has nothing to fail on: coverage has to be denied here or
    // the caller would read a silent drop as success.
    let mut full_coverage = new_targets.is_complete();
    if !full_coverage {
        inner.note_unplaced_targets();
    }
    // Taken once and used twice: the scope the events are filtered by, and the same
    // question asked of the watches no declaration names — a door is kept exactly while
    // the new declaration still leads to it.
    let scope = inner.scope_from(&new_targets);
    inner.set_scope(scope.clone());
    let desired = dedup_targets(new_targets.into_inner());
    // Spellings this pass has already handed to `unwatch`, before placing the new
    // registration under them. The obsolete loop below matches the same records — their
    // resolutions differ, which is what made them retargets — and unwatching a second time
    // would take away the registration this pass has just placed.
    let mut retargeted: Vec<PathBuf> = Vec::new();
    let held = |list: &[ArmedTarget], wanted: &ArmedTarget| {
        list.iter().any(|entry| entry.is_declared() && entry.names_the_same_watch_as(wanted))
    };

    // Spellings the defensive pass must place again — which is not quite "the spellings this
    // pass unwatched", and the difference is deliberate. The retarget and alias branches
    // unwatch one spelling meaning the watch to end up under ANOTHER, so it is the
    // destination that goes in here; the obsolete loop unwatches and means it, so what it
    // records is its own. What every entry has in common is the only thing the pass asks:
    // this spelling needs a watch and may not have one.
    let mut needs_placing: Vec<PathBuf> = Vec::new();
    let mut next_armed: Vec<ArmedTarget> = Vec::new();
    for (target, _) in &desired {
        let candidate = ArmedTarget::arming(target.clone(), ArmOrigin::Declared);
        if let Some(standing) = armed
            .iter()
            .find(|entry| entry.is_declared() && entry.names_the_same_watch_as(&candidate))
        {
            // The RESOLUTION is carried as it stands — it was captured when the backend
            // took the watch, and re-deriving it here would swap that fact for a fresh
            // reading of a tree that may have moved since. The SPELLING is the declaration's
            // (see [`ArmedTarget::under`]): the match was made on the resolution, so this is
            // the same watch named a second way, and the name has to be the one the scope
            // now accepts.
            if standing.target().path != target.path {
                // The record moves to the declaration's spelling while the registration it
                // names stands under the one it came from, so the new spelling is marked for
                // the defensive pass: the watch has to end up under the name the record now
                // claims, not under one nothing will ever look for again.
                needs_placing.push(target.path.clone());
                // And the registration under the OLD spelling has to go — the backend is
                // keyed by the path it was given, so left in place it would hold the dropped
                // alias for the life of the process, and a run of alias swaps would pile up
                // one per swap. Unless the declaration still NAMES that spelling: then
                // another target in this same pass owns it, and unwatching here would take
                // away exactly what that one placed.
                if !desired.iter().any(|(other, _)| other.path == standing.target().path) {
                    if let Err(error) = watcher.disarm(&standing.target().path) {
                        tracing::debug!(root = ?standing.target().path, "workspace change hub unwatch of an alias the declaration dropped: {error}");
                    }
                }
            }
            next_armed.push(standing.under(target.clone()));
            continue;
        }
        // The spelling is held, but not by the same watch: a RETARGET, not an addition, and
        // this is the last moment anything can name what it used to hold — the same path
        // over a new object takes a new inotify descriptor while `notify` keys its own map
        // by path and forgets the old one. Arming additions before removals exists to spare
        // a subtree that is in BOTH sets an unwatched window, and a retarget is in neither:
        // the tree it used to reach has left the set.
        if armed.iter().any(|entry| entry.is_declared() && entry.target().path == target.path) {
            if let Err(error) = watcher.disarm(&target.path) {
                tracing::debug!(root = ?target.path, "workspace change hub unwatch of a root being retargeted: {error}");
            }
            retargeted.push(target.path.clone());
        }
        match watcher.arm(&candidate.target().path, candidate.target().mode()) {
            Ok(()) => {
                tracing::info!(root = ?target.path, recursive = target.recursive, "workspace change hub watching root (re-arm)");
                next_armed.push(candidate);
            }
            Err(error) => {
                tracing::warn!(root = ?target.path, "workspace change hub failed to watch new root: {error}");
                full_coverage = false;
            }
        }
    }
    // Which DECLARED spellings were handed to `unwatch`, not merely which targets left.
    // The backend is keyed by the path it was given, and one path can be both obsolete and
    // kept at once: a symlinked root retargeted in place keeps its spelling while its
    // resolution moves, so it enters the loop above as a new target (a different canonical)
    // and this one as an obsolete entry (the old canonical) — and the unwatch here takes
    // away the watch that was just armed. Whatever the re-arm policy below, such a target
    // has to be armed again.
    for entry in armed.iter().filter(|entry| entry.is_declared()) {
        if retargeted.contains(&entry.target().path) {
            continue;
        }
        if !held(&next_armed, entry) {
            if let Err(error) = watcher.disarm(&entry.target().path) {
                tracing::debug!(root = ?entry.target().path, "workspace change hub unwatch on re-arm: {error}");
            }
            needs_placing.push(entry.target().path.clone());
        }
    }
    // A watch no declaration names cannot be re-armed from a declared set, and nothing
    // will name it again until an event happens to reveal the same door a second time —
    // which a door that merely stands there never does. So taking it away alongside the
    // obsolete targets would blind the subtree behind it for as long as the daemon runs.
    // It is kept exactly while the NEW scope still walks the path it was armed on, the
    // condition that put it there, and dropped the moment that stops holding, so the
    // registration cannot outlive the topology that justified it either.
    //
    // Decided HERE, among the other unwatches and before the defensive pass below, not
    // after it: a recursive unwatch strips descendant registrations on inotify, so a door
    // dropped after the re-arm would take a kept target that lies behind it with it and
    // leave `armed` claiming a root nothing watches.
    let mut doors: Vec<ArmedTarget> = Vec::new();
    for entry in armed.iter().filter(|entry| !entry.is_declared()) {
        // A spelling the declaration now HOLDS is the declaration's: keeping the door
        // beside it would leave two records over one registration. Read off `next_armed`
        // and not off the desired set — a declared arm that failed placed nothing, and
        // dropping the door on the strength of an intention would leave its registration
        // standing with no record left to name it by.
        if next_armed.iter().any(|held| held.target().path == entry.target().path) {
            continue;
        }
        // Two questions, and the lexical one alone is not enough — see
        // [`the_scope_still_reaches`], which also names the third question this deliberately
        // does not ask and what asking it would cost.
        if the_scope_still_reaches(&scope, entry) {
            doors.push(entry.clone());
            continue;
        }
        if let Err(error) = watcher.disarm(&entry.target().path) {
            tracing::debug!(root = ?entry.target().path, "workspace change hub unwatch of a watch no declaration names: {error}");
        }
        needs_placing.push(entry.target().path.clone());
    }
    // Defensive re-arm of every kept target: on inotify a recursive unwatch of an
    // overlapping obsolete root strips descendant registrations, including a kept
    // target's. Re-watching an already-watched path is idempotent there, and it restores
    // exactly what the unwatch above may have taken away.
    //
    // A target that fails here is DROPPED, exactly as one that fails the first pass:
    // `armed` is what every later decision reads as "already covered", so leaving it
    // there would make the next request for the same set find coverage equal and answer
    // yes over a subtree nothing is watching. Dropping a target whose watch may in fact
    // still stand costs one retry; keeping one that does not costs the events.
    let mut lost: Vec<PathBuf> = Vec::new();
    let mut kept: Vec<ArmedTarget> = Vec::with_capacity(next_armed.len() + doors.len());
    for entry in next_armed {
        if !a_kept_target_must_be_re_armed() && !needs_placing.contains(&entry.target().path) {
            kept.push(entry);
            continue;
        }
        match watcher.arm(&entry.target().path, entry.target().mode()) {
            Ok(()) => kept.push(entry),
            Err(error) => {
                tracing::warn!(root = ?entry.target().path, "workspace change hub lost a kept root on re-arm: {error}");
                // The record is being dropped, so the registration goes with it once the
                // pass knows what it ends with ([`unwatch_the_lost`], below).
                lost.push(entry.target().path.clone());
                full_coverage = false;
            }
        }
    }
    // The surviving doors take the same defensive pass, because the unwatches above can
    // have stripped them for the same reason they strip a kept root. A door that fails is
    // DROPPED rather than kept on trust — `armed` is read as "already covered", and a
    // record over a registration that is gone is how a whole linked subtree goes silent —
    // but it does not deny COVERAGE: the declaration never asked for it, so its loss is
    // not an answer to what the declaration asked.
    for entry in doors {
        if !a_kept_target_must_be_re_armed() && !needs_placing.contains(&entry.target().path) {
            kept.push(entry);
            continue;
        }
        // Was the registration this record names PROVABLY gone before the pass ran? Only
        // where an unwatch takes what lies beneath it, and only when one of the paths handed
        // to `unwatch` above is an ancestor of this one. Then a failed arm is not an open
        // question: the record would name nothing, and no later pass would ever notice,
        // because the door still leads where it did and the object behind it has not moved.
        let stripped = an_unwatch_takes_what_lies_beneath_it()
            && needs_placing.iter().any(|gone| entry.target().path.starts_with(gone));
        if let Err(error) = watcher.arm(&entry.target().path, entry.target().mode()) {
            if stripped {
                tracing::warn!(root = ?entry.target().path, "workspace change hub lost a watch no declaration names and could not place it again: {error}");
                continue;
            }
            // KEPT, unlike a declared target that fails the same pass. There the record is a
            // claim of coverage, so holding one over a watch that may be gone would answer
            // the next declaration yes over an unwatched subtree — and the blind set reports
            // the loss and the retry puts it back. A door's record claims nothing and is
            // reached by neither: it is the only handle anything has on the registration,
            // and this pass is defensive, so the watch it names may well still stand.
            // Dropping it would leave that registration with nothing able to name it again.
            //
            // What the record cannot do is say whether the subtree is still being watched,
            // so that is reported instead — the same answer this module has always given
            // for a watch it could not extend, and the only honest one when nothing can
            // tell which way the arm failed.
            inner.note_rewatch_failed(&entry.target().path, &error);
        }
        kept.push(entry);
    }
    // A refused arm of an ALREADY watched path does not take the old registration away, and
    // left behind a dropped record it would outlive every name that could ever unwatch it,
    // feeding events from a root the declaration no longer holds (github#185). Decided only
    // now, against the records the pass ends with: a record beneath a lost root spares its
    // unwatch only while it stands, and one this pass lost later, or unwatched as obsolete
    // earlier, holds nothing to spare. A declared root spelled under the lost one while
    // resolving elsewhere is among them, which is why the spellings are what is compared.
    unwatch_the_lost(watcher, &lost, &kept);
    *armed = kept;
    inner.publish_watched_roots(armed);

    // Owed to whoever was listening across the swap, and under one hold of the lock, like
    // every other Rearmed debt: a set re-pointed before any consumer exists has taken
    // nothing from anyone, and a debt over an empty cursor set is one nobody can
    // acknowledge — it would leave the hub calling itself degraded and hand the first
    // subscriber a reconcile for a window it was never inside.
    inner.lock_acc().enter_rescan_for_listeners(DegradeReason::Rearmed);
    inner.notify();
    full_coverage
}

/// Stands other modules' tests need, not just this one's. A consumer proving it no
/// longer pays for somebody else's silence has to be able to build a hub that is
/// standing-degraded, and that stand can only be built here, next to what it exercises.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{ChangeKind, WorkspaceChangeHub};
    #[cfg(unix)]
    use super::{RefusedWatches, WatchTarget};
    use std::time::Duration;
    #[cfg(unix)]
    use std::{path::PathBuf, sync::Arc};

    /// Attempts a state wait makes under `timing_contract` before it gives up: a count of polls,
    /// not a span of time, so a loaded machine that stretches every poll still fits a merely late
    /// state inside it. An ordinary run makes no such count — see [`test_utils::wait`].
    const STATE_ATTEMPTS: u32 = 6_000;

    /// The ceiling every hand-written state wait is counted against: [`STATE_ATTEMPTS`] polls
    /// at the ordinary ten-millisecond step under `timing_contract`, and none otherwise.
    pub(crate) const STATE_CEILING: Duration = test_utils::wait::ceiling(Duration::from_secs(60));

    /// Whether the hub's thread has left: nobody holds the other end of its control channel any
    /// more. Probed with a tick, which a thread still there would answer with a poll — so a test
    /// that reads polls after a stop sees a thread that outlived it either way.
    pub(crate) fn thread_has_left(hub: &WorkspaceChangeHub) -> bool {
        matches!(
            hub.control().try_send(super::HubMsg::Tick),
            Err(std::sync::mpsc::TrySendError::Disconnected(_))
        )
    }

    /// Polls `f` until it holds: a wait for a state the test goes on to assert, bounded by
    /// [`STATE_CEILING`] only under `timing_contract`.
    pub(crate) fn eventually(f: impl FnMut() -> bool) -> bool {
        if cfg!(timing_contract) {
            return eventually_within(STATE_ATTEMPTS, f);
        }
        test_utils::wait::poll_until(STATE_CEILING, Duration::from_millis(10), f)
    }

    /// The polls a hand-written wait may spend: a ceiling read as "this many polls at one per
    /// `step`", counted rather than timed. Each [`Patience::allows`] spends one poll, so a wait
    /// that loops on it gives up after the same number of polls whatever the machine's speed.
    pub(crate) struct Patience {
        left: u64,
    }

    /// Wakes a condition-variable wait may take before it gives up. A cap against a notify storm
    /// that would keep the wait alive forever; ordinary runs take a handful.
    const WAKE_CAP: u32 = 100_000;

    /// One step of a bounded condition-variable wait. Waits up to `step` for a wake and spends a
    /// poll only when the step runs out unwoken, so the budget counts time spent waiting rather
    /// than wakeups. Returns the guard and whether the wait may go on.
    pub(crate) fn wait_step<'a, T>(
        moved: &std::sync::Condvar,
        guard: std::sync::MutexGuard<'a, T>,
        step: Duration,
        patience: &mut Patience,
        wakes: &mut u32,
    ) -> (std::sync::MutexGuard<'a, T>, bool) {
        let (guard, outcome) =
            moved.wait_timeout(guard, step).unwrap_or_else(std::sync::PoisonError::into_inner);
        if outcome.timed_out() {
            let go = patience.allows();
            return (guard, go);
        }
        *wakes += 1;
        (guard, *wakes <= WAKE_CAP)
    }

    impl Patience {
        pub(crate) fn new(ceiling: Duration, step: Duration) -> Self {
            let polls = ceiling.as_nanos() / step.as_nanos().max(1);
            Self { left: u64::try_from(polls).unwrap_or(u64::MAX) }
        }

        /// Spends one poll; `false` once the allowance is gone.
        pub(crate) fn allows(&mut self) -> bool {
            if self.left == 0 {
                return false;
            }
            self.left -= 1;
            true
        }
    }

    /// [`eventually`] with a bound of its own, in every run: `attempts` polls. For a window a
    /// test reads either way — a statement given the time to become false before it is
    /// believed — and for a wait whose event is scheduled by a production retry rather than by
    /// the work the test drives.
    pub(crate) fn eventually_within(attempts: u32, mut f: impl FnMut() -> bool) -> bool {
        let started = std::time::Instant::now();
        for _ in 0..attempts {
            if f() {
                return true;
            }
            if started.elapsed() > HANG_GUARD {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        f()
    }

    /// Wall-clock backstop for the poll budgets. A poll that is itself slow would stretch the
    /// budget far past its nominal length; this bounds the failing path to minutes, and no wait
    /// that succeeds comes near it.
    const HANG_GUARD: Duration = Duration::from_secs(20 * 60);

    /// One live root and one the watch refuses to take, so the hub is standing degraded for
    /// a reason no drain clears. The period is an hour, so every tick is one the test asked
    /// for and nothing happens between two assertions on its own.
    ///
    /// `b` stays an ordinary readable directory: it is present, it stats, it canonicalizes,
    /// and the only thing wrong with it is that nothing watches it — which is the branch
    /// these tests are about. The root that cannot even be described is a different branch
    /// with a test of its own.
    #[cfg(unix)]
    pub(crate) fn partly_blind_hub(
    ) -> (tempfile::TempDir, PathBuf, PathBuf, WorkspaceChangeHub, Arc<RefusedWatches>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let refusals = RefusedWatches::refusing(vec![b.clone()]);
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching(), "the live root still arms");
        hub.wait_until_blindness_announced();
        (dir, a, b, hub, refusals)
    }

    /// Push the accumulator past its cap, so the hub `make_room`s its slowest cursor out of
    /// entries it never read. Opaque to a caller in another module: the cut can only be
    /// provoked next to the cap, and the cap is private here. The paths are synthetic — no
    /// event has to reach the watcher for the accumulator to hold them undrained.
    pub(crate) fn overflow_the_cap(hub: &WorkspaceChangeHub) {
        let mut acc = hub.inner.lock_acc();
        let count = acc.cap + 1;
        for i in 0..count {
            let path = std::path::PathBuf::from(format!("overflow/{i}.bsl"));
            acc.record(path.clone(), path, ChangeKind::MaybeChanged);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn poll_telemetry_is_bounded_and_matches_the_owner_sample() {
        let hub = WorkspaceChangeHub::start(vec![]);
        hub.shutdown();
        hub.inner.polling.store(false, Ordering::SeqCst);
        assert_eq!(hub.try_poll_status(), Some((false, None, false)));
        {
            let _held = hub.inner.blind_targets.lock().unwrap();
            assert!(hub.try_poll_status().is_none());
        }
        hub.inner.polling.store(true, Ordering::SeqCst);
        assert_eq!(
            hub.try_poll_status(),
            Some((true, hub.poll_report().map(|(_, cycle)| cycle.as_secs()), hub.poll_overdue()))
        );
        {
            let _held = hub.inner.poll_state.lock().unwrap();
            assert!(hub.try_poll_status().is_none());
        }
        {
            let _held = hub.inner.poll_expected_since.lock().unwrap();
            assert!(hub.try_poll_status().is_none());
        }
    }

    use super::test_support::*;
    use super::*;
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};
    use tempfile::tempdir;

    fn change_event(kind: EventKind, path: PathBuf) -> Result<Event, notify::Error> {
        Ok(Event { kind, paths: vec![path], attrs: Default::default() })
    }

    fn event_with_paths(kind: EventKind, paths: Vec<PathBuf>) -> Result<Event, notify::Error> {
        Ok(Event { kind, paths, attrs: Default::default() })
    }

    /// A nested project: the workspace holds the config files, the scan root sits
    /// one level down. This is the layout the scope boundary exists for — a flat
    /// project, where the workspace IS the scan root, cannot show the difference.
    struct NestedProject {
        _dir: tempfile::TempDir,
        workspace: PathBuf,
        scan_root: PathBuf,
    }

    fn nested_project() -> NestedProject {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let scan_root = workspace.join("src");
        std::fs::create_dir_all(&scan_root).unwrap();
        NestedProject { _dir: dir, workspace, scan_root }
    }

    impl NestedProject {
        fn hub(&self) -> WorkspaceChangeHub {
            let hub = WorkspaceChangeHub::start_targets(watch_targets_for(
                &self.workspace,
                std::slice::from_ref(&self.scan_root),
            ));
            assert!(hub.wait_until_watching());
            hub
        }

        /// A ready-made directory with one `.bsl` inside, built OUTSIDE the watched
        /// tree so a later rename into it carries content that never fired an event.
        fn staged_dir(&self, name: &str) -> PathBuf {
            let staged = self.workspace.join(format!(".staging-{name}"));
            std::fs::create_dir_all(&staged).unwrap();
            std::fs::write(staged.join("Module.bsl"), "Процедура П() КонецПроцедуры").unwrap();
            staged
        }
    }

    fn subtree_walks() -> usize {
        SUBTREE_WALKS.with(|walks| walks.get())
    }

    fn entry_names(batch: &DrainBatch) -> Vec<String> {
        batch.entries.iter().map(|e| e.raw.to_string_lossy().into_owned()).collect()
    }

    /// A temporary directory named the way the file system resolves it.
    ///
    /// `tempfile` can hand back a path carrying an unresolved link component (macOS puts
    /// temporaries under `/var`, a link to `/private/var`), and a backend that resolves
    /// its watch path before arming then reports every event under a spelling the test
    /// never wrote down. A test about DELIVERY has no business being sensitive to that;
    /// the tests that ARE about spelling name both spellings themselves.
    fn resolved_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap_or_else(|_| dir.path().to_path_buf());
        (dir, path)
    }

    /// A workspace whose scan root is reached through a symlink, so the root can be
    /// retargeted in place — the move that produces no filesystem event at all.
    #[cfg(unix)]
    struct LinkedRoot {
        _dir: tempfile::TempDir,
        workspace: PathBuf,
        link: PathBuf,
        first: PathBuf,
        second: PathBuf,
    }

    #[cfg(unix)]
    fn linked_root() -> LinkedRoot {
        let dir = tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let first = workspace.join("first");
        let second = workspace.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let link = workspace.join("root");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        LinkedRoot { _dir: dir, workspace, link, first, second }
    }

    #[cfg(unix)]
    impl LinkedRoot {
        fn hub(&self, period: Duration) -> WorkspaceChangeHub {
            let hub = WorkspaceChangeHub::start_targets_with_period(
                watch_targets_for(&self.workspace, std::slice::from_ref(&self.link)),
                period,
            );
            assert!(hub.wait_until_watching(), "the hub must arm");
            hub
        }

        fn retarget(&self, to: &Path) {
            std::fs::remove_file(&self.link).unwrap();
            std::os::unix::fs::symlink(to, &self.link).unwrap();
        }
    }

    /// Wait until `f` holds, polling; returns whether it ever did.
    /// The move this whole node exists for, and the one nothing else can catch: a
    /// symlinked scan root retargeted in place emits NO event, so an idle hub has
    /// nothing to react to. Only the periodic check notices, and a hub that ran its
    /// detector solely on the arrival of some other message would stay pointed at a
    /// tree nobody declared any more — for as long as the daemon lives.
    #[cfg(unix)]
    #[test]
    fn an_idle_hub_notices_a_root_retargeted_in_place() {
        let project = linked_root();
        let hub = project.hub(Duration::from_millis(50));
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);

        project.retarget(&project.second);
        assert!(
            eventually(|| hub.self_rearm_count() > 0),
            "an idle hub must notice a retarget nothing reports"
        );

        std::fs::write(project.second.join("Module.bsl"), "x").unwrap();
        assert!(
            eventually(|| { !entry_names(&hub.drain(cursor)).is_empty() }),
            "the new target must be delivered once coverage follows it"
        );
    }

    /// A busy hub is the one that can least afford blind coverage, and it is exactly
    /// where a deadline read off a receive timeout never fires: `recv_timeout` returns
    /// whatever is already queued, however long the deadline has been past.
    #[cfg(unix)]
    #[test]
    fn a_tick_still_runs_while_the_queue_never_empties() {
        let project = linked_root();
        let hub = project.hub(Duration::from_millis(50));
        let stop = Arc::new(AtomicBool::new(false));
        let noise = {
            let stop = Arc::clone(&stop);
            let dir = project.first.clone();
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::fs::write(dir.join(format!("Noise{n}.bsl")), "x");
                    n += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };

        project.retarget(&project.second);
        let noticed = eventually(|| hub.self_rearm_count() > 0);
        stop.store(true, Ordering::Relaxed);
        noise.join().unwrap();
        assert!(noticed, "a busy queue must not starve the coverage tick");
    }

    /// One tick is not a tick: a deadline armed once would pass every check that moves
    /// the target before the first firing, and leave the hub blind from then on.
    #[cfg(unix)]
    #[test]
    fn the_tick_keeps_firing_after_the_first_one() {
        let project = linked_root();
        let hub = project.hub(Duration::from_millis(50));
        assert!(eventually(|| hub.tick_count() >= 1), "the first tick must happen");
        let after_first = hub.tick_count();

        project.retarget(&project.second);
        assert!(
            eventually(|| { hub.self_rearm_count() > 0 && hub.tick_count() > after_first }),
            "a retarget after the first tick must still be noticed by a completed tick"
        );
    }

    /// A target that is simply not there costs nothing at all. It stays in the declared
    /// set and out of the cover, tick after tick, so neither a full re-arm nor a
    /// reconcile — which every consumer answers with a complete tree walk — may be
    /// spent on it. The live neighbour is the positive control: without it the thread
    /// would give up before the loop and both counters would read zero for the wrong
    /// reason.
    #[test]
    fn a_target_that_stays_missing_costs_no_rearm_and_no_reconcile() {
        let dir = tempdir().unwrap();
        let live = dir.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        let hub = WorkspaceChangeHub::start_targets_with_period(
            vec![
                WatchTarget::recursive(live.clone()),
                WatchTarget::recursive(dir.path().join("absent")),
            ],
            Duration::from_millis(20),
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);
        let rescans = hub.rescan_request_count();

        assert!(eventually(|| hub.tick_count() >= 3), "ticks must run");
        assert_eq!(hub.self_rearm_count(), 0, "a stable absence is not movement");
        assert_eq!(hub.rescan_request_count(), rescans, "and it must not cost a reconcile");
    }

    /// A target that disappears has moved once, not once per tick. The first tick
    /// after the removal is entitled to a re-arm; every later one sees the same
    /// absence and must stay silent, or a deleted extension root would put every
    /// consumer through a full walk every period for the life of the daemon.
    #[test]
    fn a_removed_target_is_noticed_once_and_not_again() {
        let dir = tempdir().unwrap();
        let live = dir.path().join("live");
        let doomed = dir.path().join("doomed");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&doomed).unwrap();
        let hub = WorkspaceChangeHub::start_targets_with_period(
            vec![WatchTarget::recursive(live), WatchTarget::recursive(doomed.clone())],
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching());

        std::fs::remove_dir_all(&doomed).unwrap();
        assert!(hub.tick_now());
        assert_eq!(hub.self_rearm_count(), 1, "the removal itself is movement");

        let after = hub.self_rearm_count();
        let rescans = hub.rescan_request_count();
        assert!(hub.tick_now());
        assert!(hub.tick_now());
        assert_eq!(hub.self_rearm_count(), after, "the same absence is not movement again");
        assert_eq!(hub.rescan_request_count(), rescans);
    }

    /// The flat layout is the one that breaks a naive detector: `watch_targets_for`
    /// declares the workspace both recursively and non-recursively, so a rule that
    /// compared the declared set against what is armed would find a target missing
    /// forever and re-walk the whole tree every period.
    #[test]
    fn a_flat_workspace_that_does_not_move_costs_nothing() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let hub = WorkspaceChangeHub::start_targets_with_period(
            watch_targets_for(&root, std::slice::from_ref(&root)),
            Duration::from_millis(20),
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);
        let rescans = hub.rescan_request_count();

        assert!(eventually(|| hub.tick_count() >= 5), "ticks must run");
        assert_eq!(hub.self_rearm_count(), 0, "a still tree is not movement");
        assert_eq!(hub.rescan_request_count(), rescans);
    }

    /// The production interval is part of the contract, not an implementation detail:
    /// every other test either shortens it or drives the tick by hand, so a hub built
    /// the ordinary way could carry an interval of a day and still pass them all.
    #[test]
    fn the_production_hub_ticks_every_thirty_seconds() {
        assert_eq!(COVERAGE_TICK_PERIOD, Duration::from_secs(30));
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        assert_eq!(hub.inner.tick_period, Duration::from_secs(30));
    }

    /// `armed` is what every later decision reads as "already covered", so a target whose
    /// re-watch failed must not sit in it: the failed call is reported once, and then the
    /// next request for the same set finds coverage equal and answers yes over a subtree
    /// nothing is watching.
    /// Gated off FSEvents, where the input cannot exist: there is no defensive pass to
    /// lose a root in (see [`a_kept_target_must_be_re_armed`]), so a target that is
    /// already armed and stays in the set is never asked to arm a second time and has no
    /// second chance to fail. The first pass's own failure is covered wherever this runs.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_target_whose_defensive_watch_failed_is_not_claimed_as_covered() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        for path in [&a, &b, &c] {
            std::fs::create_dir(path).unwrap();
        }
        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            COVERAGE_TICK_PERIOD,
            &refusals,
        );
        assert!(hub.wait_until_watching());

        // Refusing `b` moves no fingerprint, so the re-arm has to come from elsewhere:
        // adding `c` is the ordinary reason a rebuild re-arms, and it carries `b` into the
        // defensive pass, where the watch now fails.
        refusals.refuse(&b);
        let targets = vec![
            WatchTarget::recursive(a.clone()),
            WatchTarget::recursive(b.clone()),
            WatchTarget::recursive(c.clone()),
        ];
        assert!(
            !hub.ensure_roots(&targets),
            "the defensive watch of `b` fails, so coverage is denied"
        );
        assert!(
            !hub.ensure_roots(&targets),
            "the same set must still be denied: `b` is not watched, however long it stays in the live set"
        );

        hub.shutdown();
    }

    /// A root the watch could not take at startup must be retried, and the retry has to
    /// come from the hub itself: nothing outside re-declares a topology that did not
    /// change, and the obstacle — a permission, an inotify limit — clears without
    /// touching a single fingerprint, so the coverage check sees no movement to react to.
    #[cfg(unix)]
    #[test]
    fn a_root_the_watch_could_not_take_at_start_is_armed_by_a_later_tick() {
        let (_dir, _a, b, hub, refusals) = partly_blind_hub();
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);

        refusals.allow(&b);
        assert!(hub.tick_now());

        std::fs::write(b.join("Module.bsl"), "x").unwrap();
        assert!(
            eventually(|| {
                entry_names(&hub.drain(cursor)).iter().any(|n| n.ends_with("Module.bsl"))
            }),
            "the retry must arm the root, and changes under it must then arrive"
        );
    }

    /// The first failure has to reach consumers, and health alone does not prove it did:
    /// standing ill health is derived from the unwatched target, so an implementation
    /// that only computed it — never asking anyone to reconcile — would satisfy a health
    /// assertion while the window between startup and this subscription stayed unread.
    #[cfg(unix)]
    #[test]
    fn the_first_root_the_watch_could_not_take_is_reported_at_once() {
        let (_dir, _a, _b, hub, _refusals) = partly_blind_hub();
        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::RewatchFailed),
            "a root nothing watches is not health"
        );
        let cursor = hub.subscribe();
        assert!(
            hub.drain(cursor).rescan_required,
            "the blind window between startup and this subscription is nobody else's to read"
        );
    }

    /// Ill health lasts as long as the obstacle, not as long as the reconcile window.
    /// `drain` clears the reason the moment every cursor has acknowledged it, so an
    /// obstacle that does not block WRITES — an exhausted inotify limit on a readable,
    /// writable directory — would leave the hub calling itself healthy over a subtree it
    /// cannot see. A consumer that subscribes after that window has to learn it too.
    #[cfg(unix)]
    #[test]
    fn a_root_that_stays_unwatchable_keeps_the_hub_unhealthy() {
        let (_dir, _a, _b, hub, _refusals) = partly_blind_hub();
        let cursor = hub.subscribe();
        assert!(hub.drain(cursor).rescan_required);
        assert!(!hub.drain(cursor).rescan_required, "the window is closed");

        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::RewatchFailed),
            "the root is still unwatched, whatever the cursors have acknowledged"
        );
        let late = hub.subscribe();
        assert!(
            hub.drain(late).rescan_required,
            "a consumer arriving after the window still has the blind subtree to reconcile"
        );
    }

    /// Arming a root that was blind is new coverage, and everything under it changed
    /// unobserved for as long as it stayed blind — so it is worth exactly one reconcile.
    /// The drain-clean before the measurement is what makes this able to fail: without
    /// it the flag from the FIRST failure, inherited by a subscription inside the open
    /// window, would pass for the one the retry is supposed to raise.
    #[cfg(unix)]
    #[test]
    fn arming_a_root_after_its_blind_window_asks_for_one_reconcile() {
        let (_dir, _a, b, hub, refusals) = partly_blind_hub();
        let cursor = hub.subscribe();
        assert!(hub.drain(cursor).rescan_required);
        let batch = hub.drain(cursor);
        assert!(
            !batch.rescan_required && batch.entries.is_empty(),
            "drained clean, so what follows is the retry's doing and nothing else"
        );

        refusals.allow(&b);
        std::fs::write(b.join("Module.bsl"), "x").unwrap();
        assert!(hub.tick_now());
        assert!(
            hub.drain(cursor).rescan_required,
            "a change made while the root was blind is only found by a reconcile"
        );
    }

    /// The cost is paid while the obstacle lasts and stops with it. Mutation: keep a
    /// target in the unwatched set after it arms — the hub then stays degraded forever
    /// and every consumer keeps its slow path for the life of the daemon.
    #[cfg(unix)]
    #[test]
    fn the_hub_is_healthy_again_once_the_root_arms() {
        let (_dir, _a, b, hub, refusals) = partly_blind_hub();
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);

        refusals.allow(&b);
        assert!(hub.tick_now());
        let _ = hub.drain(cursor);
        assert_eq!(hub.health(), Health::Healthy, "everything declared is watched again");
    }

    /// A blind target can leave the declared set without ever arming: a topology rebuild
    /// drops the extension, and the re-arm only ever arms what is now desired. Membership
    /// of the unwatched set is therefore derived from the declaration, not accumulated —
    /// mutation: accumulate, and ill health outlives both the obstacle and the target.
    #[cfg(unix)]
    #[test]
    fn a_blind_root_dropped_from_the_declaration_stops_costing_health() {
        let (_dir, a, _b, hub, _refusals) = partly_blind_hub();
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);

        assert!(hub.rearm(vec![WatchTarget::recursive(a.clone())], Duration::from_secs(10)));
        let _ = hub.drain(cursor);
        assert_eq!(hub.health(), Health::Healthy, "everything still declared is watched");
    }

    /// The same exit, on the branch that never re-arms: a declaration whose cover equals
    /// the one in force is applied without `apply_rearm` at all. Reaching it takes a root
    /// that is blind AND outside the cover, so that dropping it moves no coverage — which
    /// is exactly the undescribable root, here a symlink pointing at itself. A missing
    /// root would not do: it is not blind at all, so the test would pass over any
    /// implementation whatever.
    #[cfg(unix)]
    #[test]
    fn a_declaration_that_re_arms_nothing_still_clears_a_dropped_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let a = root.join("a");
        std::fs::create_dir(&a).unwrap();
        let loopy = root.join("loopy");
        std::os::unix::fs::symlink(&loopy, &loopy).unwrap();

        let hub = WorkspaceChangeHub::start_targets_with_period(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(loopy)],
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);
        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::RewatchFailed),
            "the loop is declared and unwatched, so there is ill health to clear"
        );

        assert!(hub.ensure_roots(&[WatchTarget::recursive(a.clone())]));
        let _ = hub.drain(cursor);
        assert!(eventually(|| hub.health() == Health::Healthy), "nothing declared is unwatched");
    }

    /// A root that could not be described and then turned out to be gone is not blind:
    /// nothing can watch what does not exist. Both fingerprints sit outside the cover, so
    /// the coverage check sees no movement — and a tick that reads blindness off the
    /// snapshot it declined to update would hold ill health over a target that no longer
    /// exists, for the life of the daemon, while retrying a `watch` on nothing every
    /// period.
    #[cfg(unix)]
    #[test]
    fn a_blind_root_that_turns_out_to_be_gone_stops_costing_health() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let a = root.join("a");
        std::fs::create_dir(&a).unwrap();
        let loopy = root.join("loopy");
        std::os::unix::fs::symlink(&loopy, &loopy).unwrap();

        let hub = WorkspaceChangeHub::start_targets_with_period(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(loopy.clone())],
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);
        assert_eq!(hub.health(), Health::Degraded(DegradeReason::RewatchFailed));

        std::fs::remove_file(&loopy).unwrap();
        assert!(hub.tick_now());
        let _ = hub.drain(cursor);
        assert_eq!(
            hub.health(),
            Health::Healthy,
            "the obstacle and the target are both gone, so the cost must be gone with them"
        );
    }

    /// A watch that failed in the DEFENSIVE pass is as blind as one that failed the first
    /// pass, and it is the pass no arming loop reports: an implementation registering only
    /// the first would call the hub healthy the moment the re-arm's own reconcile window
    /// closed, over a root nothing watches.
    /// Gated off FSEvents, where the input cannot exist: there is no defensive pass to
    /// lose a root in (see [`a_kept_target_must_be_re_armed`]), so a target that is
    /// already armed and stays in the set is never asked to arm a second time and has no
    /// second chance to fail. The first pass's own failure is covered wherever this runs.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_root_lost_in_the_defensive_pass_keeps_the_hub_unhealthy() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        for path in [&a, &b, &c] {
            std::fs::create_dir(path).unwrap();
        }
        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            COVERAGE_TICK_PERIOD,
            &refusals,
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);

        refusals.refuse(&b);
        assert!(!hub.ensure_roots(&[
            WatchTarget::recursive(a.clone()),
            WatchTarget::recursive(b.clone()),
            WatchTarget::recursive(c.clone()),
        ]));
        let _ = hub.drain(cursor);
        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::RewatchFailed),
            "the root the defensive pass lost is unwatched like any other"
        );
        hub.shutdown();
    }

    /// A root lost in the defensive pass recovers the same way any other does — by the
    /// hub's own retry. Nothing re-declares the topology afterwards: it never changed.
    ///
    /// Health, not delivery, is what this can assert. A defensive re-watch that fails
    /// never unregistered the watch the first pass had already placed, so events under
    /// the root may well keep arriving — the hub's own record is what went wrong, and a
    /// retry restricted to roots that failed the FIRST pass would leave it wrong forever.
    /// Gated off FSEvents, where the input cannot exist: there is no defensive pass to
    /// lose a root in (see [`a_kept_target_must_be_re_armed`]), so a target that is
    /// already armed and stays in the set is never asked to arm a second time and has no
    /// second chance to fail. The first pass's own failure is covered wherever this runs.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_root_lost_in_the_defensive_pass_is_armed_by_a_later_tick() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        for path in [&a, &b, &c] {
            std::fs::create_dir(path).unwrap();
        }
        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);

        refusals.refuse(&b);
        assert!(!hub.ensure_roots(&[
            WatchTarget::recursive(a.clone()),
            WatchTarget::recursive(b.clone()),
            WatchTarget::recursive(c.clone()),
        ]));

        refusals.allow(&b);
        assert!(hub.tick_now());
        let _ = hub.drain(cursor);
        assert_eq!(
            hub.health(),
            Health::Healthy,
            "the retry must cover the root the defensive pass dropped"
        );
    }

    /// A retry that fails is not a decision to stop retrying. The obstacle that clears
    /// between two periods — an inotify limit freed by another process — is the ordinary
    /// case, and a single silent attempt would leave the root blind for the daemon's life.
    #[cfg(unix)]
    #[test]
    fn a_retry_that_failed_does_not_stop_the_next_one() {
        let (_dir, _a, b, hub, refusals) = partly_blind_hub();
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);

        assert!(hub.tick_now(), "this retry must fail");
        refusals.allow(&b);
        assert!(hub.tick_now(), "and this one must be attempted at all");

        std::fs::write(b.join("Module.bsl"), "x").unwrap();
        assert!(
            eventually(|| {
                entry_names(&hub.drain(cursor)).iter().any(|n| n.ends_with("Module.bsl"))
            }),
            "the second retry must arm the root"
        );
    }

    /// Retrying is only affordable because it is silent. Every reconcile request costs
    /// each consumer a full tree walk, so once the failure is reported the repeats add
    /// exactly none — not "at most one", which would let precisely that unnecessary walk
    /// through while still looking like a bound.
    #[cfg(unix)]
    #[test]
    fn a_retry_that_keeps_failing_asks_for_no_reconcile() {
        let (_dir, _a, _b, hub, _refusals) = partly_blind_hub();
        let cursor = hub.subscribe();
        assert!(hub.drain(cursor).rescan_required, "the first failure is reported");
        let rescans = hub.rescan_request_count();

        for _ in 0..3 {
            assert!(hub.tick_now());
        }
        assert_eq!(
            hub.rescan_request_count(),
            rescans,
            "a repeat of a failure already reported buys the consumers nothing"
        );
    }

    /// A declared root can be unstattable rather than merely unwatched: a symlink cycle
    /// answers every `stat` with a loop, so the fingerprint is neither present nor absent,
    /// and a blindness derived from the cover alone would never see such a root — no
    /// report, no standing ill health, and no retry, which is the whole of what this node
    /// is for.
    ///
    /// The obstacle is the file system's own and needs no seam: `ELOOP` is answered to
    /// every caller, root included, which is exactly what a permission cannot claim.
    #[cfg(unix)]
    #[test]
    fn a_root_that_cannot_even_be_stat_ed_is_blind_like_any_other() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let a = root.join("a");
        let loopy = root.join("loopy");
        std::fs::create_dir(&a).unwrap();
        std::os::unix::fs::symlink(&loopy, &loopy).unwrap();

        let hub = WorkspaceChangeHub::start_targets_with_period(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(loopy.clone())],
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching(), "the live root still arms");
        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::RewatchFailed),
            "a declared root nothing watches is not health, however it came to be unwatchable"
        );

        // The cycle is replaced by an ordinary directory at the same path: the obstacle
        // clears without the declaration moving, which is the only way the retry — and not
        // a re-arm — can be what covers it.
        std::fs::remove_file(&loopy).unwrap();
        std::fs::create_dir(&loopy).unwrap();
        assert!(hub.tick_now());
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);
        assert_eq!(hub.health(), Health::Healthy, "and the retry must reach it too");
    }

    /// A hub whose thread never started arms nothing and never will, and it is the one
    /// permanent failure that reaches no other reporting path: dropping the spawn error
    /// leaves the hub looking like one that is merely still starting, so every consumer
    /// waits out its whole readiness budget before falling back to the slow path it was
    /// entitled to immediately.
    #[test]
    fn a_hub_whose_thread_never_started_reports_failure_at_once() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start_with_unstartable_thread(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        // The answer is the failure itself: a wait that had run out its thirty seconds would say
        // NotYet, so no clock is read to tell the two apart.
        let budget = Duration::from_secs(30);
        let asked = Instant::now();
        assert_eq!(hub.watch_readiness(budget), WatchReadiness::Failed);
        if cfg!(timing_contract) {
            assert!(asked.elapsed() < budget / 2, "the failure is answered at once");
            assert!(asked.elapsed() < Duration::from_secs(5), "the failure is answered at once");
        }
        assert_eq!(hub.health(), Health::Degraded(DegradeReason::WatcherSetup));
    }

    /// "Not armed" is two states, and only one of them is worth waiting on. A hub still
    /// walking a large tree must be distinguishable from one that failed, or a consumer
    /// has to choose between abandoning the first and hanging on the second.
    #[test]
    fn a_hub_still_starting_says_not_yet_and_arms_once_released() {
        let dir = tempdir().unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        assert_eq!(hub.watch_readiness(Duration::ZERO), WatchReadiness::NotYet);
        assert_eq!(hub.health(), Health::Healthy, "nothing has gone wrong yet");

        hold.release();
        assert_eq!(hub.watch_readiness(STATE_CEILING), WatchReadiness::Armed);
    }

    /// Two aliases of one directory canonicalize the same, so the declaration dropping the
    /// one the watch actually stands on moves no canonical path and no fingerprint. The
    /// backend keeps reporting paths under the dropped spelling, which the narrowed scope
    /// no longer accepts — the root goes silent while everything about it looks agreed.
    #[cfg(unix)]
    #[test]
    fn dropping_the_alias_the_watch_stands_on_re_arms() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        let first = root.join("first");
        let second = root.join("second");
        std::os::unix::fs::symlink(&real, &first).unwrap();
        std::os::unix::fs::symlink(&real, &second).unwrap();

        let hub = WorkspaceChangeHub::start_targets_with_period(
            vec![WatchTarget::recursive(first.clone()), WatchTarget::recursive(second.clone())],
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching());

        // The winner of the collapse moves to `second` inside the declaration, then
        // `first` leaves it. Both sets cover the same canonical directory recursively.
        assert!(hub.ensure_roots(&[
            WatchTarget::recursive(second.clone()),
            WatchTarget::recursive(first.clone()),
        ]));
        assert!(hub.ensure_roots(&[WatchTarget::recursive(second.clone())]));
        // A declaration is delivered asynchronously; the tick shares the channel, so its
        // acknowledgement proves both were applied.
        assert!(hub.tick_now());

        let cursor = hub.subscribe();
        std::fs::write(real.join("Module.bsl"), "x").unwrap();
        assert!(
            eventually(|| !entry_names(&hub.drain(cursor)).is_empty()),
            "a change under the only declared spelling must still be delivered"
        );

        // And the set has SETTLED on the spelling the declaration uses. A watch left named
        // by the alias that was dropped reads as different from every later declaration of
        // the same tree, so each one re-arms and charges every consumer a reconcile —
        // coverage that looks agreed while nothing ever agrees. The declaration below adds
        // back a target its recursive twin absorbs, so the cover does not move and nothing
        // is owed unless the armed set is still named by the alias.
        let settled = hub.drain(hub.subscribe()).cursor;
        assert!(hub.ensure_roots(&[WatchTarget::recursive(second), WatchTarget::recursive(first),]));
        assert!(hub.tick_now());
        assert!(
            !hub.drain(settled).rescan_required,
            "a declaration whose cover is already in force must cost nothing",
        );
    }

    /// One relative spelling names two different targets under two different current
    /// directories, so a record kept in raw spellings would suppress a real change of
    /// declaration as a repeat, and the periodic check would keep policing the target the
    /// hub was started from. The two-directory run itself is not reproduced here — the
    /// current directory is process-wide, and moving it would decide the outcome of every
    /// other test in this binary — so what is pinned is the property that forbids it.
    #[test]
    fn a_declaration_is_recorded_in_placed_spellings() {
        let hub = WorkspaceChangeHub::start_targets(vec![WatchTarget::recursive(PathBuf::from(
            "a-root-that-is-not-placed",
        ))]);
        let published =
            hub.inner.declared_published.lock().unwrap_or_else(PoisonError::into_inner).clone();
        hub.shutdown();

        assert_eq!(published.len(), 1, "the target is placeable, so it is kept");
        assert!(
            published[0].path.is_absolute(),
            "a record in raw spellings cannot be compared across current directories: {published:?}"
        );
    }

    /// The record of published declarations exists to skip REPEATS, so recording one the
    /// thread never received would silence every later attempt to send it — and the
    /// periodic check would keep policing a topology nobody declared any more. Delivery
    /// is failed here by ending the thread; a control channel filled by an event storm
    /// takes the same branch, and is what makes this reachable in production.
    #[cfg(unix)]
    #[test]
    fn a_declaration_the_thread_never_received_is_not_recorded() {
        let layout = linked_root();
        let started = vec![WatchTarget::recursive(layout.link.clone())];
        let hub = WorkspaceChangeHub::start_targets(started.clone());
        assert!(hub.wait_until_watching());
        hub.shutdown();

        // Same coverage under a different spelling: the input that publishes a
        // declaration instead of asking for a re-arm.
        hub.ensure_roots(&[WatchTarget::recursive(layout.first.clone())]);

        let published =
            hub.inner.declared_published.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(published, started, "an undelivered declaration must not count as sent");
    }

    /// The hub takes a path into work only when it belongs to the observed scope.
    /// A directory OUTSIDE every scan root — the build output or a vendored clone
    /// that lands next to the sources — must not be walked: `collect_subtree`
    /// records one entry per file it finds, and a foreign tree larger than the
    /// accumulator's capacity would push every consumer into a full rescan.
    #[test]
    fn a_foreign_directory_is_not_walked() {
        let project = nested_project();
        let hub = project.hub();
        let cursor = hub.subscribe();

        let foreign = project.workspace.join("node_modules");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("index.js"), "x").unwrap();
        std::fs::write(foreign.join("Module.bsl"), "Процедура П() КонецПроцедуры").unwrap();

        let walks_before = subtree_walks();
        let rewatch =
            hub.inner.ingest_event(change_event(EventKind::Create(CreateKind::Folder), foreign));

        assert_eq!(subtree_walks(), walks_before, "a foreign directory is not walked");
        assert!(rewatch.is_empty(), "and is not handed back for a recursive re-watch");
        assert!(hub.drain(cursor).entries.is_empty(), "so none of its files reach the accumulator");
    }

    /// The positive control for the case above: the very same shape INSIDE a scan
    /// root is walked, re-watched and recorded. Without it, a predicate that
    /// filtered everything would pass the negative test.
    #[test]
    fn a_directory_inside_a_scan_root_is_walked() {
        let project = nested_project();
        let hub = project.hub();
        let cursor = hub.subscribe();

        let owned = project.scan_root.join("CommonModules");
        std::fs::create_dir_all(&owned).unwrap();
        std::fs::write(owned.join("Module.bsl"), "Процедура П() КонецПроцедуры").unwrap();

        let walks_before = subtree_walks();
        let rewatch = hub
            .inner
            .ingest_event(change_event(EventKind::Create(CreateKind::Folder), owned.clone()));

        assert_eq!(subtree_walks(), walks_before + 1, "a directory in scope is walked");
        assert_eq!(rewatch, vec![owned], "and is handed back for a recursive re-watch");
        assert!(
            entry_names(&hub.drain(cursor)).iter().any(|p| p.ends_with("Module.bsl")),
            "its files reach the accumulator"
        );
    }

    /// A vanished path outside the scope must not reach the accumulator either.
    /// `classify_path` returns `None` only for a directory that still EXISTS; a
    /// gone extension-less path becomes `SubtreeRemoved`, which every consumer
    /// reads as "reconsider the whole tree".
    #[test]
    fn a_foreign_directory_removal_asks_for_no_rescan() {
        let project = nested_project();
        let hub = project.hub();
        let cursor = hub.subscribe();

        let foreign = project.workspace.join("vendor");
        hub.inner.ingest_event(change_event(EventKind::Remove(RemoveKind::Folder), foreign));

        let batch = hub.drain(cursor);
        assert!(batch.entries.is_empty(), "a removal outside the scope is not recorded");
        assert_eq!(hub.health(), Health::Healthy, "and does not degrade the hub");
    }

    /// Positive control: the same removal INSIDE a scan root still asks consumers
    /// to reconsider the subtree.
    #[test]
    fn a_scan_root_directory_removal_is_recorded() {
        let project = nested_project();
        let hub = project.hub();
        let cursor = hub.subscribe();

        let gone = project.scan_root.join("Catalogs");
        hub.inner.ingest_event(change_event(EventKind::Remove(RemoveKind::Folder), gone));

        let batch = hub.drain(cursor);
        assert_eq!(batch.entries.len(), 1, "a removal in scope is recorded");
        assert_eq!(batch.entries[0].kind, ChangeKind::SubtreeRemoved);
    }

    /// An unknown event kind is a "we may have missed something" signal, so it
    /// degrades. But a path outside the scope carries nothing we were watching for,
    /// and degrading on it lets one foreign file drag every consumer into a scan.
    #[test]
    fn an_unknown_event_outside_the_scope_does_not_degrade() {
        let project = nested_project();
        let hub = project.hub();

        hub.ingest_for_test(change_event(EventKind::Other, project.workspace.join("stray.tmp")));

        assert_eq!(hub.health(), Health::Healthy);
    }

    /// Positive control: an unknown event about a path we ARE watching still
    /// degrades, and so does one that carries no path at all — an absent path is
    /// not evidence that the lost change was out of scope.
    #[test]
    fn an_unknown_event_in_scope_or_without_paths_still_degrades() {
        let project = nested_project();
        let hub = project.hub();
        hub.ingest_for_test(change_event(EventKind::Other, project.scan_root.join("x.bsl")));
        assert_eq!(hub.health(), Health::Degraded(DegradeReason::UnknownEvent));

        let project = nested_project();
        let hub = project.hub();
        hub.ingest_for_test(event_with_paths(EventKind::Other, Vec::new()));
        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::UnknownEvent),
            "an event with no path proves nothing about scope"
        );
    }

    /// Every notify backend makes a relative watch target absolute before arming
    /// it, so events come back spelled from the current directory. Keeping only the
    /// declared (relative) and canonical spellings loses the whole tree whenever
    /// those two differ from the reported one — a `..` component, or a symlink on
    /// the way. A plain relative root hides this: its canonical spelling already
    /// matches what the watcher reports.
    ///
    /// This case holds the behaviour but cannot, on Unix, distinguish the two ways
    /// of building that third spelling: `std::path::absolute` differs from the
    /// backend's plain join only in dropping `.` (which component comparison
    /// ignores anyway) and in resolving `..` — and the latter it does on Windows
    /// alone. So the mutation that swaps one for the other is only observable on a
    /// Windows host; here the reasoning rests on the std docs and the backend
    /// source, not on a red test.
    #[test]
    fn a_relative_target_stays_in_scope_when_events_arrive_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        std::fs::create_dir(dir.path().join("prefix")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();

        let base = dir.path().strip_prefix(&cwd).unwrap();
        // `.` alongside `..`: the backend keeps both, `std::path::absolute` drops
        // the first everywhere and resolves the second on Windows. Without them the
        // test cannot tell the two ways of building the spelling apart.
        let declared = base.join("prefix").join("..").join(".").join("src");
        let scope =
            Scope::from_targets_for_test(&ResolvedTargets::here(vec![WatchTarget::recursive(
                declared.clone(),
            )]));

        // The reference is computed the way the backend computes it — joining to the
        // current directory, components untouched.
        let reported = cwd.join(&declared);
        assert!(
            scope.may_record(&reported.join("Module.bsl")),
            "the spelling the watcher actually reports is in scope"
        );
        assert!(scope.may_walk(&reported.join("CommonModules")));
    }

    /// Reading the current directory twice — once for the scope, once inside the
    /// backend's `watch` — is a race on process-wide state: a change in between
    /// arms the watcher on one tree while the scope describes another, and every
    /// event from the armed tree is then filtered out in silence. Resolving the
    /// targets once, before anything is armed or compared, removes the second read:
    /// the backend takes an absolute path as given.
    #[test]
    fn targets_are_resolved_once_so_the_watcher_and_the_scope_cannot_disagree() {
        let resolved = ResolvedTargets::here(vec![
            WatchTarget::recursive(PathBuf::from("src")),
            WatchTarget { path: PathBuf::from("."), recursive: false },
        ]);
        assert!(resolved.is_complete());
        let resolved = resolved.as_slice();

        assert!(
            resolved.iter().all(|t| t.path.is_absolute()),
            "nothing relative reaches the watcher, so it never re-reads the directory"
        );
        // Modes must survive the rewrite: the non-recursive one is what carries the
        // project-config files.
        assert_eq!(resolved.iter().filter(|t| t.recursive).count(), 1);
        assert_eq!(resolved.iter().filter(|t| !t.recursive).count(), 1);

        let absolute = std::env::current_dir().unwrap().join("src");
        assert!(
            ResolvedTargets::here(vec![WatchTarget::recursive(absolute.clone())]).as_slice()[0]
                .path
                == absolute,
            "an already-absolute target is left exactly as it was"
        );
    }

    /// One directory snapshot for the whole set, not one per target. A set spanning
    /// a scan root and the workspace config directory that were placed against
    /// different directories would watch the sources of one project and the
    /// configuration of another, and nothing downstream could tell.
    ///
    /// The single read is a property of the signature — `resolve` cannot consult
    /// process state at all — so this asserts the consequence: every relative target
    /// lands under the one directory handed in, and absolute ones are left alone.
    #[test]
    fn every_relative_target_is_placed_against_the_one_directory_given() {
        // Not a literal: `/base` carries no drive prefix, so Windows reads it as
        // RELATIVE and the resolver would rightly drop it.
        let base = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // Canonicalized, not merely temporary: the system temp directory is
        // whatever `TMPDIR` says, and a test that assumes it absolute would be
        // testing the environment instead of the resolver.
        let base = base.path().canonicalize().unwrap();
        let elsewhere = elsewhere.path().canonicalize().unwrap().join("extension");
        let resolved = ResolvedTargets::resolve(
            vec![
                WatchTarget::recursive(PathBuf::from("src")),
                WatchTarget { path: PathBuf::from("."), recursive: false },
                WatchTarget::recursive(elsewhere.clone()),
            ],
            Some(&base),
        );

        assert!(resolved.is_complete());
        let placed: Vec<&PathBuf> = resolved.as_slice().iter().map(|t| &t.path).collect();
        assert_eq!(placed, vec![&base.join("src"), &base.join("."), &elsewhere]);
    }

    /// Placing a target is a claim that the backend will not resolve the path
    /// again, and only an absolute path makes that claim true. A base that is
    /// itself relative cannot produce one — nor can a Windows drive-relative
    /// target, whose prefix REPLACES the base on join and leaves the result
    /// relative to the per-drive current directory. The join is therefore checked
    /// rather than assumed.
    #[test]
    fn a_target_that_stays_relative_after_the_join_is_dropped() {
        let resolved = ResolvedTargets::resolve(
            vec![WatchTarget::recursive(PathBuf::from("src"))],
            Some(Path::new("relative/base")),
        );

        assert!(!resolved.is_complete());
        assert!(resolved.as_slice().is_empty());
    }

    /// Without a readable current directory a relative target cannot be placed at
    /// all. Carrying it through relative would hand the backend a path it resolves
    /// against its own later read of the same process-wide state — the disagreement
    /// between the armed tree and the described one that this whole boundary exists
    /// to prevent. Dropping it is only safe if the set says so: a caller that read
    /// the drop as success would report full coverage over a subtree nobody watches.
    #[test]
    fn a_relative_target_without_a_directory_is_dropped_and_the_set_says_so() {
        // A real temporary directory, not a `/base/src` literal: on Windows a path
        // without a drive prefix is relative, and the assertion would invert.
        let dir = tempfile::tempdir().unwrap();
        let absolute = dir.path().canonicalize().unwrap().join("src");
        let resolved = ResolvedTargets::resolve(
            vec![
                WatchTarget::recursive(PathBuf::from("src")),
                WatchTarget::recursive(absolute.clone()),
            ],
            None,
        );

        assert!(!resolved.is_complete(), "coverage cannot be claimed over a dropped target");
        let placed: Vec<&PathBuf> = resolved.as_slice().iter().map(|t| &t.path).collect();
        assert_eq!(placed, vec![&absolute], "what could be placed is still watched");
    }

    /// A rescan notice is not a change to the path it names — it says the event
    /// stream itself lapsed, so nothing received so far can be trusted. Scope tells
    /// nothing about what was lost, and dropping the notice would leave consumers
    /// serving stale results with the hub reporting good health. Inotify raises it
    /// without a path, FSEvents attaches one (the workspace directory, quite
    /// possibly outside every scan root) — which is exactly where a scope filter
    /// would swallow it.
    #[test]
    fn a_rescan_notice_outside_the_scope_still_degrades() {
        // The flag is an attribute in its own right: nothing in the contract ties it
        // to one kind, so every kind a backend may pair it with must degrade. Each
        // kind gets its own hub — health does not reset between notices.
        for kind in [
            EventKind::Other,
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Remove(RemoveKind::File),
            EventKind::Access(notify::event::AccessKind::Close(notify::event::AccessMode::Write)),
        ] {
            let project = nested_project();
            let hub = project.hub();
            let notice = Event::new(kind)
                .add_path(project.workspace.join("vendor"))
                .set_flag(notify::event::Flag::Rescan);
            hub.ingest_for_test(Ok(notice));

            assert_eq!(
                hub.health(),
                Health::Degraded(DegradeReason::UnknownEvent),
                "a rescan notice carrying {kind:?} must still degrade"
            );
        }
    }

    /// An ordinary file outside every scan root is not a config file and not a
    /// directory — the plainest way to be out of scope, and the one a
    /// directory-only filter would miss.
    #[test]
    fn an_ordinary_file_outside_the_scope_is_not_recorded() {
        let project = nested_project();
        let hub = project.hub();
        let cursor = hub.subscribe();

        let stray = project.workspace.join("notes.tmp");
        std::fs::write(&stray, "x").unwrap();
        hub.ingest_for_test(change_event(EventKind::Modify(ModifyKind::Any), stray));

        assert!(hub.drain(cursor).entries.is_empty());
    }

    /// Every project-config name — not just the TOML one — reaches consumers from
    /// the workspace directory, even though that directory is not a scan root.
    /// This is what the non-recursive workspace target exists for.
    #[test]
    fn every_config_file_name_in_the_workspace_is_recorded() {
        for name in project_model::PROJECT_INPUT_FILE_NAMES {
            let project = nested_project();
            let hub = project.hub();
            let cursor = hub.subscribe();

            let config = project.workspace.join(name);
            std::fs::write(&config, "{}").unwrap();
            hub.ingest_for_test(change_event(EventKind::Modify(ModifyKind::Any), config));

            assert!(
                !hub.drain(cursor).entries.is_empty(),
                "a change to {name} must reach consumers"
            );
        }
    }

    /// The config predicate reads the NAME and the PARENT, never the disk: a
    /// deleted config is exactly as topology-shaping as an edited one, and a
    /// predicate gated on "the file exists" would drop it.
    #[test]
    fn every_config_file_removal_in_the_workspace_is_recorded() {
        for name in project_model::PROJECT_INPUT_FILE_NAMES {
            let project = nested_project();
            let hub = project.hub();
            let cursor = hub.subscribe();

            let config = project.workspace.join(name);
            hub.ingest_for_test(change_event(EventKind::Remove(RemoveKind::File), config));

            let batch = hub.drain(cursor);
            assert_eq!(batch.entries.len(), 1, "the removal of {name} must reach consumers");
            assert_eq!(batch.entries[0].kind, ChangeKind::MaybeRemoved);
        }
    }

    /// The config exception is anchored to the workspace directory. The same name
    /// deeper down, outside every scan root, is somebody else's file.
    #[test]
    fn a_config_name_outside_the_workspace_directory_is_not_recorded() {
        let project = nested_project();
        let hub = project.hub();
        let cursor = hub.subscribe();

        let nested = project.workspace.join("vendor").join(project_model::CONFIG_FILE_NAMES[0]);
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "{}").unwrap();
        hub.ingest_for_test(change_event(EventKind::Modify(ModifyKind::Any), nested));

        assert!(hub.drain(cursor).entries.is_empty());
    }

    /// The two permissions add up rather than cancel: a config NAME grants the
    /// right to record, never the right to walk. A directory carrying that name is
    /// still a foreign directory.
    #[test]
    fn a_directory_named_like_a_config_is_not_walked() {
        let project = nested_project();
        let hub = project.hub();

        let trap = project.workspace.join(project_model::CONFIG_FILE_NAMES[0]);
        std::fs::create_dir_all(&trap).unwrap();
        std::fs::write(trap.join("payload.tmp"), "x").unwrap();

        let walks_before = subtree_walks();
        let rewatch =
            hub.inner.ingest_event(change_event(EventKind::Create(CreateKind::Folder), trap));

        assert_eq!(subtree_walks(), walks_before, "a config-named directory is not walked");
        assert!(rewatch.is_empty(), "and is not taken under recursive watch");
    }

    /// Positive control for the case above, and the reason it must be worded
    /// carefully: in a FLAT project the workspace IS the scan root, so a
    /// config-named directory sits under a scan root and is walked on the ordinary
    /// rule. Forbidding it outright would silently drop the contents of a moved
    /// directory.
    #[test]
    fn a_config_named_directory_under_a_scan_root_is_walked() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let hub = WorkspaceChangeHub::start_targets(watch_targets_for(
            &root,
            std::slice::from_ref(&root),
        ));
        assert!(hub.wait_until_watching());

        let trap = root.join(project_model::CONFIG_FILE_NAMES[0]);
        std::fs::create_dir_all(&trap).unwrap();
        std::fs::write(trap.join("Module.bsl"), "Процедура П() КонецПроцедуры").unwrap();

        let walks_before = subtree_walks();
        let rewatch = hub
            .inner
            .ingest_event(change_event(EventKind::Create(CreateKind::Folder), trap.clone()));

        assert_eq!(subtree_walks(), walks_before + 1, "under a scan root it is walked");
        assert_eq!(rewatch, vec![trap]);
    }

    /// A scan root declared through a symlink sends events spelled the DECLARED
    /// way: `dedup_targets` decides by canonical path but hands `watcher.watch`
    /// the raw spelling. A predicate comparing against one spelling alone would
    /// throw the whole tree away.
    #[cfg(unix)]
    #[test]
    fn both_spellings_of_a_scan_root_are_in_scope() {
        // Resolved, so that the spelling this test calls canonical really is one: under an
        // unresolved link component `real` is a THIRD spelling, neither declared nor
        // canonical, and the case the test means to cover would never be reached.
        let (_dir, workspace) = resolved_tempdir();
        let real = workspace.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = workspace.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let hub = WorkspaceChangeHub::start_targets(watch_targets_for(
            &workspace,
            std::slice::from_ref(&link),
        ));
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        // Two DIFFERENT files, one fed by each spelling: feeding one file twice
        // would prove nothing, since both spellings coalesce onto a single key and
        // dropping either would leave the count unchanged.
        std::fs::write(real.join("Declared.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        std::fs::write(real.join("Canonical.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        hub.ingest_for_test(change_event(
            EventKind::Modify(ModifyKind::Any),
            link.join("Declared.bsl"),
        ));
        hub.ingest_for_test(change_event(
            EventKind::Modify(ModifyKind::Any),
            real.join("Canonical.bsl"),
        ));

        let names = entry_names(&hub.drain(cursor));
        assert!(
            names.iter().any(|p| p.ends_with("Declared.bsl")),
            "the declared spelling — what the watcher actually reports — is in scope"
        );
        assert!(names.iter().any(|p| p.ends_with("Canonical.bsl")), "and so is the canonical one");
    }

    /// A directory MOVED into the tree arrives as `Modify(Name(To))`, never as a
    /// `Create`. Its files did not change — their path did — so they fire no events
    /// of their own: unless the arrival itself is walked, nothing in it is ever
    /// indexed until a full reconcile.
    #[test]
    fn a_directory_moved_into_a_scan_root_is_walked() {
        let project = nested_project();
        let hub = project.hub();
        let cursor = hub.subscribe();

        let staged = project.staged_dir("moved");
        let landed = project.scan_root.join("CommonModules");
        std::fs::rename(&staged, &landed).unwrap();

        let walks_before = subtree_walks();
        let rewatch = hub.inner.ingest_event(change_event(
            EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::To)),
            landed.clone(),
        ));

        assert_eq!(subtree_walks(), walks_before + 1, "the arrived directory is walked");
        assert_eq!(rewatch, vec![landed], "and taken under recursive watch");
        assert!(
            entry_names(&hub.drain(cursor)).iter().any(|p| p.ends_with("Module.bsl")),
            "the content that rode along with it reaches the accumulator"
        );
    }

    /// Only `Name` widens the branch. A `chmod` on a directory is a `Modify` too,
    /// and walking a large tree for it would be pure waste — the plainest wrong
    /// widening (to any `Modify`) is exactly what this guards against.
    #[test]
    fn a_non_rename_modify_of_a_directory_is_not_walked() {
        let project = nested_project();
        let hub = project.hub();

        let dir = project.scan_root.join("CommonModules");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Module.bsl"), "Процедура П() КонецПроцедуры").unwrap();

        for kind in [
            ModifyKind::Metadata(notify::event::MetadataKind::Permissions),
            ModifyKind::Data(notify::event::DataChange::Any),
            ModifyKind::Any,
            ModifyKind::Other,
        ] {
            let walks_before = subtree_walks();
            let rewatch =
                hub.inner.ingest_event(change_event(EventKind::Modify(kind), dir.clone()));
            assert_eq!(subtree_walks(), walks_before, "a {kind:?} on a directory is not walked");
            assert!(rewatch.is_empty(), "and does not take it under recursive watch");
        }
    }

    /// A mixed `Name(Both)` carries the vanished path and the arrived one in a
    /// single event, and a rename out of a scan root puts them on opposite sides
    /// of the boundary. Filtering per EVENT would let the foreign directory
    /// through; the filter is per PATH.
    #[test]
    fn a_rename_across_the_boundary_filters_each_path() {
        let project = nested_project();
        let hub = project.hub();
        let cursor = hub.subscribe();

        let gone = project.scan_root.join("Catalogs");
        let landed = project.workspace.join("vendor");
        std::fs::create_dir_all(&landed).unwrap();
        std::fs::write(landed.join("Module.bsl"), "Процедура П() КонецПроцедуры").unwrap();

        let walks_before = subtree_walks();
        let rewatch = hub.inner.ingest_event(event_with_paths(
            EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Both)),
            vec![gone.clone(), landed],
        ));

        assert_eq!(subtree_walks(), walks_before, "the arrived foreign directory is not walked");
        assert!(rewatch.is_empty());
        let batch = hub.drain(cursor);
        assert_eq!(batch.entries.len(), 1, "only the in-scope path is recorded");
        assert_eq!(batch.entries[0].raw, gone);
    }

    #[test]
    fn create_then_delete_in_one_window_settles_on_removal() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("Module.bsl");
        std::fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut acc = Accumulator::new(64);
        let cursor = acc.subscribe(None);
        // First event fires while the file exists.
        let (canonical, kind) = classify_path(&file).unwrap();
        assert_eq!(kind, ChangeKind::MaybeChanged);
        acc.record(canonical, file.clone(), kind);

        // The file is deleted before the next event is classified: on-disk truth
        // now says removed, and that is what the coalesced entry must reflect. The
        // create and remove must land on the SAME canonical key so they coalesce.
        std::fs::remove_file(&file).unwrap();
        let (canonical, kind) = classify_path(&file).unwrap();
        assert_eq!(kind, ChangeKind::MaybeRemoved);
        acc.record(canonical, file.clone(), kind);

        let batch = acc.drain(cursor);
        assert_eq!(batch.entries.len(), 1, "the path coalesced to a single entry");
        assert_eq!(batch.entries[0].kind, ChangeKind::MaybeRemoved);
    }

    #[cfg(unix)]
    #[test]
    fn create_then_remove_coalesce_under_symlinked_root() {
        // A symlinked directory component makes the raw and canonical spellings
        // differ. The removal key must still match the create key (via the parent's
        // canonicalization) so the two coalesce instead of leaving a ghost entry.
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        return;

        #[cfg(unix)]
        {
            let via_link = link.join("Module.bsl");
            std::fs::write(&via_link, "x").unwrap();

            let mut acc = Accumulator::new(64);
            let cursor = acc.subscribe(None);
            let (create_key, _) = classify_path(&via_link).unwrap();
            acc.record(create_key, via_link.clone(), ChangeKind::MaybeChanged);

            std::fs::remove_file(&via_link).unwrap();
            let (remove_key, kind) = classify_path(&via_link).unwrap();
            assert_eq!(kind, ChangeKind::MaybeRemoved);
            acc.record(remove_key, via_link.clone(), kind);

            let batch = acc.drain(cursor);
            assert_eq!(batch.entries.len(), 1, "create+remove coalesced under the symlink");
            assert_eq!(batch.entries[0].kind, ChangeKind::MaybeRemoved);
        }
    }

    #[test]
    fn removed_extensionless_path_is_a_subtree_removal() {
        let dir = tempdir().unwrap();
        let gone = dir.path().join("Catalogs");
        let (_canonical, kind) = classify_path(&gone).unwrap();
        assert_eq!(kind, ChangeKind::SubtreeRemoved);
    }

    #[test]
    fn reclamation_releases_entries_once_all_cursors_advance() {
        let mut acc = Accumulator::new(64);
        let a = acc.subscribe(None);
        let b = acc.subscribe(None);

        for i in 0..10 {
            let p = PathBuf::from(format!("/f{i}.bsl"));
            acc.record(p.clone(), p, ChangeKind::MaybeChanged);
        }
        assert_eq!(acc.entries.len(), 10);

        // A drains; B has not, so nothing is reclaimed yet (B still needs them).
        let _ = acc.drain(a);
        assert_eq!(acc.entries.len(), 10, "the slower cursor still holds the entries");

        // B drains; now the slowest cursor has advanced and the map empties.
        let _ = acc.drain(b);
        assert_eq!(acc.entries.len(), 0, "entries release once every cursor passed them");

        // Cap counts undrained in-flight paths, so post-drain recording keeps going
        // without ever tripping overflow.
        for i in 0..200 {
            let p = PathBuf::from(format!("/g{i}.bsl"));
            acc.record(p.clone(), p, ChangeKind::MaybeChanged);
            let _ = acc.drain(a);
            let _ = acc.drain(b);
        }
        assert_eq!(acc.health(), Health::Healthy, "steady drain never overflows");
        assert!(acc.entries.is_empty());
    }

    /// One consumer stopping must cost the others nothing. A cursor that stops draining
    /// pins the reclaim floor for everyone, so the cap is reached over and over, and the
    /// price of the cap — a full reconcile — used to be charged to every cursor alive.
    ///
    /// The one that keeps up is subscribed FIRST on purpose: with the other order an
    /// implementation sacrificing the OLDEST cursor rather than the furthest behind
    /// passes this gate while still punishing exactly the wrong one.
    #[test]
    fn a_cursor_that_keeps_up_pays_nothing_for_one_that_stopped() {
        let mut acc = Accumulator::new(2);
        let fast = acc.subscribe(None);
        let _stalled = acc.subscribe(None);

        for n in 0..5 {
            let p = PathBuf::from(format!("/p{n}.bsl"));
            acc.record(p.clone(), p, ChangeKind::MaybeChanged);
            let batch = acc.drain(fast);
            assert!(!batch.rescan_required, "step {n}: keeping up must cost no reconcile");
            assert_eq!(batch.entries.len(), 1, "step {n}: and must deliver the path itself");
        }
        assert!(acc.entries.len() <= acc.cap, "the cap still bounds the accumulator");
    }

    #[test]
    fn materialized_batch_advances_only_after_ack() {
        let mut acc = Accumulator::new(8);
        let cursor = acc.subscribe(None);
        let first = PathBuf::from("/first.bsl");
        acc.record(first.clone(), first, ChangeKind::MaybeChanged);
        acc.enter_rescan(false, DegradeReason::Overflow);

        let batch = acc.materialize(cursor);
        assert_eq!(batch.entries.len(), 1);
        assert!(batch.rescan_required);
        assert_eq!(acc.materialize(cursor).entries.len(), 1, "refusal keeps the same batch");

        let second = PathBuf::from("/second.bsl");
        acc.record(second.clone(), second, ChangeKind::MaybeChanged);
        acc.acknowledge(&batch);
        let next = acc.materialize(cursor);
        assert_eq!(next.entries.len(), 1, "drift newer than the checkpoint remains pending");
        assert!(next.rescan_required, "a changed generation keeps the rescan obligation");

        acc.acknowledge(&next);
        let empty = acc.materialize(cursor);
        assert!(empty.entries.is_empty());
        assert!(!empty.rescan_required);
    }

    /// The one who fell behind is told, and told once: it lost detail, and silence would
    /// leave it serving state that predates changes nobody will ever replay for it.
    #[test]
    fn the_cursor_that_fell_behind_is_told_to_reconcile() {
        let mut acc = Accumulator::new(2);
        let fast = acc.subscribe(None);
        let stalled = acc.subscribe(None);

        for n in 0..5 {
            let p = PathBuf::from(format!("/p{n}.bsl"));
            acc.record(p.clone(), p, ChangeKind::MaybeChanged);
            let _ = acc.drain(fast);
        }
        assert!(acc.drain(stalled).rescan_required, "the cursor that lost detail must know");
        assert!(!acc.drain(stalled).rescan_required, "and be told exactly once");
    }

    /// Only the FURTHEST behind pays. With three cursors at three different positions,
    /// advancing the last one is enough to free room, and the middle one lost nothing —
    /// an implementation flagging everyone who is behind would punish it too, and the
    /// two-cursor stand cannot see that: there the second cursor is always level.
    #[test]
    fn only_the_furthest_behind_pays_for_the_overflow() {
        let mut acc = Accumulator::new(3);
        let stalled = acc.subscribe(None);
        let middle = acc.subscribe(None);
        let level = acc.subscribe(None);

        let record = |acc: &mut Accumulator, name: &str| {
            let p = PathBuf::from(name);
            acc.record(p.clone(), p, ChangeKind::MaybeChanged);
        };
        record(&mut acc, "/p1.bsl");
        let _ = acc.drain(middle);
        let _ = acc.drain(level);
        record(&mut acc, "/p2.bsl");
        let _ = acc.drain(level);
        record(&mut acc, "/p3.bsl");
        let _ = acc.drain(level);
        // `stalled` at the very beginning, `middle` one path in, `level` current.
        record(&mut acc, "/p4.bsl");

        assert!(acc.drain(stalled).rescan_required, "the furthest behind pays");
        let batch = acc.drain(middle);
        assert!(!batch.rescan_required, "the one in the middle lost nothing");
        let mut names: Vec<String> =
            batch.entries.iter().map(|e| e.raw.to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["/p2.bsl", "/p3.bsl", "/p4.bsl"], "and keeps its exact paths");
    }

    /// Nobody is observing yet, so nobody is owed anything. Dropping the detail keeps the
    /// memory bound before the first subscriber; opening a reconcile window would hand a
    /// debt to a consumer that had not even arrived when the paths went by.
    #[test]
    fn an_overflow_before_the_first_subscriber_owes_nobody() {
        let mut acc = Accumulator::new(1);
        acc.record(PathBuf::from("/a.bsl"), PathBuf::from("/a.bsl"), ChangeKind::MaybeChanged);
        acc.record(PathBuf::from("/b.bsl"), PathBuf::from("/b.bsl"), ChangeKind::MaybeChanged);

        assert!(acc.entries.len() <= acc.cap, "the cap holds before anyone subscribes");
        assert_eq!(acc.health(), Health::Healthy, "an unobserved drop is nobody's debt");
        let first = acc.subscribe(None);
        assert!(!acc.drain(first).rescan_required, "and the first arrival inherits nothing");
    }

    /// A cursor falling behind is not a loss of the stream. Nothing was dropped for anyone
    /// else, so a consumer arriving afterwards owes no reconcile — and the debt of the one
    /// that did fall behind has to be visible on ITS health, which needs a reason of its
    /// own: reusing `Overflow` would put the two events back under one name.
    #[test]
    fn a_lagging_cursor_is_its_own_debt_and_not_the_streams() {
        let mut acc = Accumulator::new(2);
        let stalled = acc.subscribe(None);
        for n in 0..4 {
            let p = PathBuf::from(format!("/p{n}.bsl"));
            acc.record(p.clone(), p, ChangeKind::MaybeChanged);
        }

        assert_eq!(acc.health(), Health::Healthy, "the stream lost nothing");
        let late = acc.subscribe(None);
        assert!(!acc.drain(late).rescan_required, "so a newcomer owes nothing");

        let Health::Degraded(reason) = acc.health_for(Some(stalled)) else {
            panic!("the cursor that lost detail is not healthy");
        };
        assert_ne!(reason, DegradeReason::Overflow, "its own lag is not a loss of the stream");
    }

    /// A lag is a loss with an identity of its own.
    ///
    /// The token is what a consumer tells one loss from another by — a reconcile says the
    /// detail is gone, and the fact number cannot say it, so the same number twice IS the same
    /// event and is deduplicated. A cursor cut out of entries it had not drained lost something
    /// nobody else lost; handing it the last SHARED loss number made its reconcile read as a
    /// repeat of an event it had already acted on, and the debt it should have opened was
    /// dropped on the way in.
    #[test]
    fn a_cursor_cut_out_of_its_entries_reports_a_loss_of_its_own() {
        let mut acc = Accumulator::new(2);
        let lagging = acc.subscribe(None);
        let other = acc.subscribe(None);
        let record = |acc: &mut Accumulator, name: &str| {
            let p = PathBuf::from(name);
            acc.record(p.clone(), p, ChangeKind::MaybeChanged);
        };

        // One loss of the stream, seen by both: the same event, and both must say so.
        acc.enter_rescan(false, DegradeReason::RuntimeError);
        let shared_lagging = acc.drain(lagging).loss_token().expect("the window is a loss");
        let shared_other = acc.drain(other).loss_token().expect("the window is a loss");
        assert_eq!(
            shared_lagging, shared_other,
            "one loss reaching two cursors is one event and must carry one identity",
        );

        // And now a loss of one cursor's own: `other` keeps draining, `lagging` does not, and
        // the cap cuts it out of what it never read.
        for name in ["/p1.bsl", "/p2.bsl", "/p3.bsl", "/p4.bsl"] {
            record(&mut acc, name);
            acc.drain(other);
        }
        let batch = acc.drain(lagging);
        assert!(batch.rescan_required, "the stand needs the lagging cursor to have been cut");
        assert_ne!(
            batch.loss_token().expect("a cut is a loss"),
            shared_lagging,
            "a lag was handed the identity of the shared loss it had already acted on",
        );
    }

    /// A newcomer inheriting an open window is handed THAT window's identity.
    ///
    /// Cutting a cursor that already owed the window issues nothing — its debt covers the cut —
    /// but it still moved the counter, and a newcomer inheriting the window took its number
    /// from the counter. One loss then reached a consumer under two names, and the second
    /// name revived its budgets a second time.
    #[test]
    fn a_newcomer_inherits_the_identity_of_the_window_it_joins() {
        let mut acc = Accumulator::new(2);
        let behind = acc.subscribe(None);
        let current = acc.subscribe(None);
        acc.enter_rescan(false, DegradeReason::RuntimeError);
        let window = acc.materialize(behind).loss_token().expect("the window is a loss");

        // `current` reconciles and keeps up; `behind` does not, and the cap cuts it while it
        // still owes the window.
        acc.drain(current);
        for name in ["/p1.bsl", "/p2.bsl", "/p3.bsl", "/p4.bsl"] {
            let path = PathBuf::from(name);
            acc.record(path.clone(), path, ChangeKind::MaybeChanged);
            acc.drain(current);
        }
        assert_eq!(
            acc.materialize(behind).loss_token(),
            Some(window),
            "the stand needs the cut cursor still owing the window, under its identity",
        );
        // And a loss of another cursor's own after the window: the counter moves past it.
        acc.force_rescan(current, DegradeReason::RewatchFailed);

        let newcomer = acc.subscribe(None);
        assert_eq!(
            acc.materialize(newcomer).loss_token(),
            Some(window),
            "a newcomer inheriting the open window was given another identity for the same loss",
        );
    }

    /// A reconcile forced on one cursor is a loss of its own, with an identity of its own.
    ///
    /// Left without one, its batch borrowed the number of the last loss issued — which is the
    /// number a consumer has usually just acted on, so the new debt read as a repeat and was
    /// dropped on the way in.
    #[test]
    fn a_forced_reconcile_is_not_named_after_a_loss_already_acted_on() {
        let mut acc = Accumulator::new(4);
        let early = acc.subscribe(None);
        acc.enter_rescan(false, DegradeReason::RuntimeError);
        let acted_on = acc.drain(early).loss_token().expect("the window is a loss");

        let late = acc.subscribe(None);
        acc.force_rescan(late, DegradeReason::RewatchFailed);
        let batch = acc.materialize(late);
        assert!(batch.rescan_required, "the forced reconcile is owed");
        assert_ne!(
            batch.loss_token(),
            Some(acted_on),
            "a reconcile forced on a newcomer was named after a loss the consumer already acted on",
        );
        assert_eq!(
            acc.materialize(late).loss_token(),
            batch.loss_token(),
            "re-materialising the same debt moved its identity",
        );
    }

    /// Re-subscribing carries a debt across with the identity it was issued under.
    ///
    /// A consumer re-subscribes to take a fresh baseline, and the debt goes with it — but under
    /// a new number, so the consumer that had already seen the old one read the same loss as a
    /// new event and paid for it again.
    #[test]
    fn a_resubscribed_debt_keeps_its_identity() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        hub.inner.lock_acc().enter_rescan(false, DegradeReason::RuntimeError);
        let before = hub.materialize(cursor);
        assert!(before.rescan_required, "the stand needs a debt to carry");

        let (replaced, _) = hub.resubscribe(cursor);
        let after = hub.materialize(replaced);
        hub.shutdown();
        assert!(after.rescan_required, "the debt did not survive re-subscribing");
        assert_eq!(
            after.loss_token(),
            before.loss_token(),
            "re-subscribing re-issued the same loss under a new identity",
        );
    }

    /// A debt of one cursor's own is not the shared window. Left conflated, a private lag
    /// keeps the window open after every cursor that owed the SHARED reconcile has paid,
    /// so `health()` stays degraded and every newcomer inherits a full reconcile it owes
    /// to nobody — the same "one silent consumer taxes the rest" this node removes,
    /// re-entering by the back door.
    #[test]
    fn a_private_lag_does_not_hold_the_shared_window_open() {
        let mut acc = Accumulator::new(2);
        let lagging = acc.subscribe(None);
        let other = acc.subscribe(None);
        let record = |acc: &mut Accumulator, name: &str| {
            let p = PathBuf::from(name);
            acc.record(p.clone(), p, ChangeKind::MaybeChanged);
        };

        acc.enter_rescan(false, DegradeReason::RuntimeError);
        record(&mut acc, "/p1.bsl");
        assert!(acc.drain(lagging).rescan_required, "it owed the shared window too");

        // Now `lagging` falls behind on its own while `other` still owes the window.
        for name in ["/p2.bsl", "/p3.bsl", "/p4.bsl", "/p5.bsl"] {
            record(&mut acc, name);
        }
        assert!(acc.drain(other).rescan_required, "the last debtor of the window pays");

        assert_eq!(acc.health(), Health::Healthy, "the shared window is closed");
        let late = acc.subscribe(None);
        assert!(
            !acc.drain(late).rescan_required,
            "and a newcomer owes nothing for somebody else's private lag"
        );
    }

    /// `health_for` has to carry BOTH standing conditions of the hub, and one stand cannot
    /// show that: with a single test the carrier that is missing is exactly the one the
    /// stand does not raise. This is the thread that never started.
    #[test]
    fn a_hub_that_never_started_is_unhealthy_for_every_cursor() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start_with_unstartable_thread(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);
        let cursor = hub.subscribe();
        let _ = hub.drain(cursor);

        assert_eq!(
            hub.health_for(Some(cursor)),
            Health::Degraded(DegradeReason::WatcherSetup),
            "a hub that will never watch is nobody's fast path"
        );
    }

    /// The other carrier: a declared root nothing watches. The cursor is drained clean, so
    /// its own debt cannot be what answers here — and without this the blind branch of
    /// `health_for` is held by no test at all, while consumers keep trusting a stream that
    /// does not cover the root.
    #[cfg(unix)]
    #[test]
    fn a_blind_hub_is_unhealthy_for_a_cursor_that_owes_nothing() {
        let (_dir, _a, _b, hub, _refusals) = partly_blind_hub();
        let cursor = hub.subscribe();
        assert!(hub.drain(cursor).rescan_required, "the blindness was announced");
        assert!(!hub.drain(cursor).rescan_required, "and acknowledged");

        assert_eq!(
            hub.health_for(Some(cursor)),
            Health::Degraded(DegradeReason::RewatchFailed),
            "a root nothing watches is the hub's condition, not this cursor's debt"
        );
    }

    /// A genuine loss of the stream is everyone's, and is told to each cursor exactly
    /// once. Driven through `enter_rescan` directly, because the accumulator's own cap no
    /// longer produces this: filling the cap is one consumer falling behind, which is a
    /// different event under a different name.
    #[test]
    fn a_lost_stream_is_told_to_each_cursor_once_then_recovers() {
        let mut acc = Accumulator::new(2);
        let a = acc.subscribe(None);
        let b = acc.subscribe(None);

        acc.enter_rescan(true, DegradeReason::Overflow);
        assert_eq!(acc.health(), Health::Degraded(DegradeReason::Overflow));
        assert!(acc.entries.is_empty(), "a lost stream drops the untrusted detail");

        acc.record(PathBuf::from("/d.bsl"), PathBuf::from("/d.bsl"), ChangeKind::MaybeChanged);
        assert_eq!(acc.entries.len(), 1, "new changes after the loss are captured");

        let batch_a = acc.drain(a);
        assert!(batch_a.rescan_required);
        assert!(!acc.drain(a).rescan_required, "the flag is delivered only once");
        // A alone acknowledging is not enough; B still owes a reconcile.
        assert_eq!(acc.health(), Health::Degraded(DegradeReason::Overflow));

        assert!(acc.drain(b).rescan_required);
        assert_eq!(acc.health(), Health::Healthy, "recovers once all cursors confirm");
    }

    /// A debt can be settled by leaving as well as by acknowledging. A consumer that never
    /// started releases its cursor and takes its share of the window with it; if only a
    /// drain could close the window, one that nobody is left to acknowledge would outlive
    /// every party to it and be inherited by whoever subscribes next — who would pay for it
    /// with a full reconcile of a loss that happened before it existed.
    #[test]
    fn a_window_nobody_is_left_to_acknowledge_does_not_outlive_them() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());

        let lease = CursorLease::new(hub.clone());
        hub.degrade_external();
        assert!(matches!(hub.health(), Health::Degraded(_)), "the window is open");

        drop(lease);

        assert_eq!(hub.health(), Health::Healthy, "the last party to the window took it away");
        let newcomer = hub.subscribe();
        assert!(
            !hub.drain(newcomer).rescan_required,
            "and a newcomer inherits nothing it could not have observed",
        );
    }

    /// Taking a fresh cursor is not the same as going away, and only the second settles a
    /// debt. A consumer re-subscribes to take a new baseline, and the build that follows can
    /// fail — leaving the old state served by a cursor that now owes nothing, with the
    /// events it was owed for long gone. The debt belongs to the consumer, not to the id.
    #[test]
    fn a_consumer_taking_a_fresh_cursor_carries_its_debt_with_it() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());

        let sink = hub.subscribe();
        let diagnostics = hub.subscribe();
        hub.degrade_external();
        assert!(hub.drain(sink).rescan_required, "the sink acknowledges the window");

        let (diagnostics, _) = hub.resubscribe(diagnostics);

        assert!(
            hub.drain(diagnostics).rescan_required,
            "a rebuild that has not happened yet cannot have settled the debt",
        );
    }

    /// Settling pays the carried debt — and nothing else. The cursor's POSITION is where the
    /// re-subscription put it, so an entry that landed afterwards is still delivered by the
    /// next drain. Settling by draining would have consumed the position with the debt and
    /// dropped that entry in the rebuild's own window.
    #[test]
    fn settling_a_carried_debt_closes_it_without_swallowing_later_entries() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());

        let cursor = hub.subscribe();
        hub.degrade_external();
        let (replaced, carried) = hub.resubscribe(cursor);
        let carried = carried.expect("the window's debt was carried onto the fresh cursor");

        // An entry that lands after the re-subscription: it belongs to the window the
        // rebuild's own baseline scan cannot contain.
        let after = dir.path().join("After.bsl");
        hub.inner.lock_acc().record(after.clone(), after.clone(), ChangeKind::MaybeChanged);

        assert!(hub.settle_carried_debt(replaced, &carried), "the carried debt was not settled");
        assert_eq!(hub.health(), Health::Healthy, "and the window it belonged to is closed");

        let batch = hub.drain(replaced);
        assert!(
            batch.entries.iter().any(|entry| entry.raw == after),
            "settling the debt swallowed an entry that landed after the re-subscription",
        );
        assert!(!batch.rescan_required, "the settled debt is not owed again");
    }

    /// A settlement names ONE loss. A window raised after the re-subscription is a different
    /// loss, and the old token must not pay it: the rebuild that earned the settle happened
    /// before that loss existed.
    #[test]
    fn a_debt_replaced_after_resubscribing_is_not_settled_by_the_old_token() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());

        let cursor = hub.subscribe();
        hub.degrade_external();
        let (replaced, first) = hub.resubscribe(cursor);
        let first = first.expect("the first window's debt was carried onto the fresh cursor");

        // A second loss lands between the re-subscription and the rebuild's success.
        hub.degrade_external();

        assert!(
            !hub.settle_carried_debt(replaced, &first),
            "a superseded identity settled the window that replaced it",
        );
        assert!(
            matches!(hub.health(), Health::Degraded(_)),
            "the newer window is still owed and must still read as degraded",
        );
        let batch = hub.drain(replaced);
        assert!(batch.rescan_required, "and it is still demanded of the consumer");
        assert_ne!(batch.loss_token(), Some(first.loss), "under an identity of its own");
    }

    /// The token a re-subscription hands back is captured UNDER the accumulator hold that does
    /// the carry, never read back from the fresh cursor afterwards. A read-back token is taken
    /// after `drop(acc)`, so a window raised in between renames the loss it names — and the
    /// rebuild that later settles that token answers a reconcile it never performed.
    ///
    /// The interleaving is provoked through a lock the gap itself already takes, not through a
    /// production hook: this thread holds the accumulator and spawns the worker, which blocks
    /// on it; while still holding it, this thread takes `blind_targets` — the lock the SECOND
    /// blindness read takes, which the worker (stuck on the accumulator) can not reach yet.
    /// On release the worker carries the debt, drops `acc` and stops on that second read,
    /// holding nothing; this thread waits for the old cursor to leave the map and raises a
    /// fresh window right there, with the worker still in the gap.
    ///
    /// The worker is placed past its first read by a pause: nothing it does between that read
    /// and the accumulator shows outside the hub. A worker the pause has not moved that far
    /// blocks on the lock this thread then takes, and the two wait on each other, so the stand
    /// belongs to the timing profile.
    #[test]
    #[cfg_attr(
        not(timing_contract),
        ignore = "необязательный тест: a pause places the worker past its first read; запуск: CARGO_TARGET_DIR=target-timing RUSTFLAGS='--cfg timing_contract' cargo test --workspace"
    )]
    fn resubscribing_captures_the_debt_it_carried_before_a_window_can_replace_it() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());

        let cursor = hub.subscribe();
        hub.degrade_external();

        let guard = hub.inner.lock_acc();
        let carried_loss = guard
            .cursors
            .get(&cursor.id)
            .and_then(|cursor| cursor.loss)
            .expect("the stand needs an owed window to carry");
        let old_id = cursor.id;
        let hub2 = hub.clone();
        let worker = std::thread::spawn(move || hub2.resubscribe(cursor));
        // The worker passes the first blindness read and stops on the accumulator this thread
        // holds. It cannot reach the second one yet, so taking that lock here is safe.
        std::thread::sleep(Duration::from_millis(50));
        let blind = hub.inner.blind_targets.lock().unwrap_or_else(|e| e.into_inner());
        drop(guard);
        // The carry is done the moment the old cursor leaves the map; raise the window then,
        // while the worker waits on `blind` just past `drop(acc)`.
        let mut patience =
            test_support::Patience::new(test_support::STATE_CEILING, Duration::from_micros(100));
        loop {
            let mut acc = hub.inner.lock_acc();
            if !acc.cursors.contains_key(&old_id) {
                acc.enter_rescan(false, DegradeReason::RuntimeError);
                break;
            }
            drop(acc);
            assert!(patience.allows(), "the carry never happened");
            std::thread::sleep(Duration::from_micros(100));
        }
        drop(blind);
        let (replaced, token) = worker.join().unwrap();
        let token = token.expect("the carry reported a token");
        assert_eq!(token.loss, carried_loss, "the token named a loss the carry did not write");
        hub.unsubscribe(replaced);
        hub.shutdown();
    }

    /// The token a re-subscription hands back names the debt the OLD cursor carried, captured
    /// from the carry itself — not a number read back from the fresh cursor afterwards. A
    /// cursor already owing a window keeps that loss through the re-subscription, and the
    /// token IS that loss; the pair then settles exactly the carried window.
    #[test]
    fn resubscribing_captures_the_debt_it_carried() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());

        let cursor = hub.subscribe();
        hub.degrade_external();
        // The loss the old cursor owes, read from the accumulator before the re-subscription.
        let before = hub.inner.lock_acc().cursors.get(&cursor.id).and_then(|c| c.loss);
        assert!(before.is_some(), "the stand needs an owed window");

        let (replaced, carried) = hub.resubscribe(cursor);
        let carried = carried.expect("the window's debt was carried onto the fresh cursor");
        assert_eq!(Some(carried.loss), before, "the token renames the debt that was carried");
        assert!(
            hub.settle_carried_debt(replaced, &carried),
            "the captured pair settles the window it was carried for",
        );
        hub.shutdown();
    }

    /// Settlement is by the PAIR, so a debt is paid only by the cursor that still holds both
    /// halves. A cursor carrying a debt of its own, one the accumulator never issued, one that
    /// owes nothing at all, and — the half a loss cannot speak for — one whose position has
    /// moved since the carry are all refusals; the alternative being a rebuild that answers a
    /// reconcile it never performed.
    #[test]
    fn settling_a_debt_is_refused_for_a_cursor_that_does_not_hold_it() {
        let mut acc = Accumulator::new(4);
        let debtor = acc.subscribe(None);
        acc.enter_rescan(false, DegradeReason::RuntimeError);
        let (shared, debtor_pos) = {
            let cursor = acc.cursors.get(&debtor).expect("the debtor");
            (cursor.loss.expect("the window was issued an identity"), cursor.pos)
        };
        let held = CarriedDebt { loss: shared, pos: debtor_pos };

        // A cursor carrying a debt of its OWN: same accumulator, a different loss.
        let forced = acc.subscribe(Some(DegradeReason::WatcherSetup));
        let (own, forced_pos) = {
            let cursor = acc.cursors.get(&forced).expect("the forced cursor");
            (cursor.loss.expect("the forced cursor was issued one of its own"), cursor.pos)
        };
        assert_ne!(own, shared, "the stand needs two distinct debts");
        assert!(
            !acc.settle_carried_debt(forced, &held),
            "another cursor's loss was settled against this one",
        );

        assert!(
            !acc.settle_carried_debt(u64::MAX, &held),
            "a cursor the accumulator never issued owes nothing",
        );

        // The other half of the pair: the loss still names the carried window, but the
        // position moved — the signature of a `make_room` cut this settle may not pay.
        let moved = CarriedDebt { loss: shared, pos: debtor_pos + 1 };
        assert!(
            !acc.settle_carried_debt(debtor, &moved),
            "a cursor whose position moved was settled as if nothing had been cut",
        );

        assert!(acc.settle_carried_debt(debtor, &held), "the debtor pays its own pair");
        let forced_held = CarriedDebt { loss: own, pos: forced_pos };
        assert!(acc.settle_carried_debt(forced, &forced_held), "and the forced cursor its own");
        let healthy = acc.subscribe(None);
        assert!(
            !acc.settle_carried_debt(healthy, &held),
            "a cursor that owes nothing is not a debtor",
        );
    }

    /// A cut does not RENAME a debt, it moves the position out from under it. `make_room`
    /// advances a cursor already owing a reconcile to `max`, and leaves the loss as it was —
    /// one reconcile covers the cut too. So the old token still carries the right loss while
    /// the position has moved past entries the rebuild's baseline scan never read: a settle
    /// that answered to the loss alone would pay that reconcile and the cut entries would
    /// vanish with no debt left on the cursor.
    #[test]
    fn a_cut_during_a_carried_debt_is_not_settled_by_the_old_pair() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start_with_capacity(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            4,
            COVERAGE_TICK_PERIOD,
        );
        assert!(hub.wait_until_watching());

        let cursor = hub.subscribe();
        hub.degrade_external();
        let (replaced, carried) = hub.resubscribe(cursor);
        let carried = carried.expect("the window's debt was carried onto the fresh cursor");

        // The rebuild is running and the accumulator overflows: `make_room` cuts the fresh
        // cursor forward. It already owes a reconcile, so the cut does not give it a new loss.
        {
            let mut acc = hub.inner.lock_acc();
            for i in 0..6 {
                let path = dir.path().join(format!("Cut{i}.bsl"));
                acc.record(path.clone(), path, ChangeKind::MaybeChanged);
            }
        }

        assert!(
            !hub.settle_carried_debt(replaced, &carried),
            "a cut was settled against a baseline scan that never read the cut entries",
        );
        assert!(
            matches!(hub.health_for(Some(replaced)), Health::Degraded(_)),
            "the cut still owes a reconcile",
        );
        let batch = hub.drain(replaced);
        assert!(
            batch.rescan_required,
            "the consumer is told to reconcile over the cut, not left believing the stream is whole",
        );
        hub.shutdown();
    }

    /// The backend dropping events is the loss that belongs to everyone: the paths were
    /// gone before the hub ever saw them, so no cursor can be spared. BOTH discriminate:
    /// an implementation opening the window only for future subscribers satisfies the
    /// late-arrival half while the cursor that was already there loses the change for good.
    #[test]
    fn a_dropped_event_is_owed_by_cursors_present_and_future() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        let present = hub.subscribe();
        let _ = hub.drain(present);

        hub.inner.channel_overflow.store(true, Ordering::Relaxed);
        hub.inner.drain_channel_overflow();
        assert_eq!(hub.health(), Health::Degraded(DegradeReason::Overflow));

        // Subscribed while the window is still open — draining `present` first would
        // close it, and then a clean answer for `late` would be right rather than wrong.
        let late = hub.subscribe();
        assert!(
            hub.drain(present).rescan_required,
            "a cursor alive when the backend dropped events lost them too"
        );
        assert!(
            hub.drain(late).rescan_required,
            "and a cursor born inside the window cannot know what it missed either"
        );
    }

    #[test]
    fn independent_cursors_drain_their_own_deltas() {
        let mut acc = Accumulator::new(64);
        let x = acc.subscribe(None);
        acc.record(PathBuf::from("/a.bsl"), PathBuf::from("/a.bsl"), ChangeKind::MaybeChanged);
        acc.record(PathBuf::from("/b.bsl"), PathBuf::from("/b.bsl"), ChangeKind::MaybeChanged);

        let batch_x = acc.drain(x);
        assert_eq!(batch_x.entries.len(), 2);

        // Y subscribes now — it must not replay the earlier changes.
        let y = acc.subscribe(None);
        acc.record(PathBuf::from("/c.bsl"), PathBuf::from("/c.bsl"), ChangeKind::MaybeChanged);

        let batch_y = acc.drain(y);
        assert_eq!(batch_y.entries.len(), 1, "Y sees only the change after it subscribed");
        assert_eq!(batch_y.entries[0].raw, Path::new("/c.bsl"));

        let batch_x = acc.drain(x);
        assert_eq!(batch_x.entries.len(), 1, "X sees only its own delta");
        assert_eq!(batch_x.entries[0].raw, Path::new("/c.bsl"));
    }

    #[test]
    fn health_flips_on_setup_error_for_invalid_root() {
        let hub =
            WorkspaceChangeHub::start(vec![PathBuf::from("/definitely/not/a/real/path/xyzzy")]);
        assert!(!hub.wait_until_watching());
        assert_eq!(hub.health(), Health::Degraded(DegradeReason::WatcherSetup));
        assert!(!hub.is_watching());
    }

    #[test]
    fn start_arms_the_watch_asynchronously() {
        let dir = tempdir().unwrap();
        // `start` returns immediately; the watch arms on the hub thread. Poll for it
        // rather than asserting synchronously.
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching(), "setup settles to watching");
        assert!(hub.is_watching());
        assert_eq!(hub.health(), Health::Healthy);
    }

    #[test]
    fn health_flips_on_unknown_event_kind() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        assert_eq!(hub.health(), Health::Healthy);
        hub.ingest_for_test(change_event(EventKind::Other, dir.path().join("x")));
        assert_eq!(hub.health(), Health::Degraded(DegradeReason::UnknownEvent));
    }

    #[test]
    fn runtime_error_flips_health() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        hub.ingest_for_test(Err(notify::Error::generic("boom")));
        assert_eq!(hub.health(), Health::Degraded(DegradeReason::RuntimeError));
    }

    #[test]
    fn rewatch_failure_degrades_and_recovers_through_rescan() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        // A failure to extend the watch to a new subtree must not stay silent: it
        // degrades health and asks the consumer to reconcile, recoverable like any
        // other transient miss once the cursor acknowledges.
        hub.trigger_rewatch_failure_for_test();
        assert_eq!(hub.health(), Health::Degraded(DegradeReason::RewatchFailed));

        let batch = hub.drain(cursor);
        assert!(batch.rescan_required, "the sink is told to reconcile the possibly-blind subtree");
        assert_eq!(hub.health(), Health::Healthy, "recovers once the cursor acknowledges");
    }

    #[test]
    fn access_events_are_ignored_without_degrading() {
        use notify::event::AccessKind;
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        hub.ingest_for_test(change_event(
            EventKind::Access(AccessKind::Read),
            dir.path().join("x.bsl"),
        ));
        assert_eq!(hub.health(), Health::Healthy, "a read is not drift");
    }

    #[test]
    fn config_file_paths_are_accumulated_not_filtered() {
        let dir = tempdir().unwrap();
        let toml = dir.path().join("bsl-analyzer.toml");
        std::fs::write(&toml, "[source]\nroot = \".\"\n").unwrap();

        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        hub.ingest_for_test(change_event(EventKind::Modify(ModifyKind::Any), toml.clone()));

        let batch = hub.drain(cursor);
        assert!(
            batch.entries.iter().any(|e| e.raw == toml),
            "the hub accepts any path; kind-filtering is the consumer's job",
        );
    }

    fn polling_hub(root: &Path, verify_bytes: u64) -> (WorkspaceChangeHub, SinkCursor) {
        let (hub, hold) = WorkspaceChangeHub::start_polling(
            vec![WatchTarget::recursive(root.to_path_buf())],
            PollConfig { period: Duration::from_secs(3600), verify_bytes },
        );
        let cursor = hub.subscribe();
        hold.release();
        assert_eq!(hub.watch_readiness(STATE_CEILING), WatchReadiness::Failed);
        assert!(eventually(|| hub.is_polling() && hub.drain_peek(cursor)));
        (hub, cursor)
    }

    /// The bound a status answer publishes covers the pass in flight AND the one after it.
    ///
    /// Verification reads a budget per tick and resumes a file where it stopped, keeping what
    /// it already read while the file's stamp is unchanged and comparing the digest only once
    /// the file has been read through. An edit landing behind the offset the current pass has
    /// already passed is therefore not in that pass at all: it is found by the next full read.
    /// One pass over every polled byte is the arithmetic of the work, not a bound on when the
    /// edit is seen, and it was published as the second.
    #[test]
    fn the_published_poll_bound_covers_the_pass_in_flight_and_the_one_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("A.bsl"), vec![b'a'; 4096]).unwrap();

        for (budget, passes) in [(4096u64, 1u64), (2048, 2), (1024, 4)] {
            let (hub, _cursor) = polling_hub(root, budget);
            let (_, cycle) = hub.poll_report().expect("a polling hub reports its bound");
            assert_eq!(
                cycle,
                Duration::from_secs(3600) * u32::try_from(2 * passes + 3).unwrap(),
                "the bound for a {budget}-byte budget is not the conservative one",
            );
            hub.shutdown();
        }
    }

    /// A hub whose watch never came up tells every cursor to reconcile exactly once — the
    /// facts before its first picture are lost — and from then on delivers changes as poll
    /// records, with no further reconcile however many polls run.
    #[test]
    fn a_hub_without_a_watch_polls_after_one_reconcile() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Before.bsl"), "Процедура П() КонецПроцедуры\n").unwrap();
        let (hub, cursor) = polling_hub(root, VERIFY_BYTES);

        let first = hub.drain(cursor);
        assert!(first.rescan_required, "the facts before the picture are lost: one reconcile");
        for _ in 0..3 {
            assert!(hub.poll_now());
            let batch = hub.drain(first.cursor);
            assert!(!batch.rescan_required, "a poll cost another reconcile");
            assert!(batch.entries.is_empty(), "a quiet poll reported {:?}", batch.entries);
        }
        assert_eq!(hub.rescan_request_count(), 1);

        std::fs::write(root.join("After.bsl"), "Процедура Н() КонецПроцедуры\n").unwrap();
        std::fs::remove_file(root.join("Before.bsl")).unwrap();
        assert!(hub.poll_now());
        let batch = hub.drain(first.cursor);
        assert!(!batch.rescan_required);
        let kinds: Vec<(String, ChangeKind)> = batch
            .entries
            .iter()
            .map(|e| (e.raw.file_name().unwrap().to_string_lossy().into_owned(), e.kind))
            .collect();
        assert!(kinds.contains(&("After.bsl".to_owned(), ChangeKind::MaybeChanged)), "{kinds:?}");
        assert!(kinds.contains(&("Before.bsl".to_owned(), ChangeKind::MaybeRemoved)), "{kinds:?}");
        assert_eq!(
            hub.health_for(Some(first.cursor)),
            Health::Degraded(DegradeReason::WatcherSetup)
        );
        hub.shutdown();
    }

    /// Re-declaring the set the hub already stands on costs nothing at all: no message, no
    /// re-mapping, no reconcile. The loop this prevents is the graph's: a reconcile makes it
    /// rebuild, and every rebuild declares its scan roots again.
    #[test]
    fn a_repeated_declaration_costs_the_fallback_poll_nothing() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Модуль.bsl"), "Процедура П() КонецПроцедуры\n").unwrap();
        let (hub, cursor) = polling_hub(root, VERIFY_BYTES);
        let first = hub.drain(cursor);
        assert!(first.rescan_required, "entering the poll is owed exactly one reconcile");
        let targets = vec![WatchTarget::recursive(root.to_path_buf())];
        let polls = hub.poll_count();

        for _ in 0..3 {
            assert!(!hub.ensure_roots(&targets), "a polled hub covers nothing by watching");
            assert!(hub.poll_now());
            let batch = hub.drain(first.cursor);
            assert!(!batch.rescan_required, "a repeated declaration cost a reconcile");
            assert!(batch.entries.is_empty(), "a quiet poll reported {:?}", batch.entries);
        }
        assert_eq!(hub.rescan_request_count(), 1, "the declaration was answered by re-mapping");
        assert_eq!(hub.poll_count(), polls + 3, "each tick is one poll, and no re-map besides");
        hub.shutdown();
    }

    /// The same rule with a root the backend refuses: the gap is the hub's to repair, so
    /// re-declaring it neither re-arms nor reconciles. Without this a workspace with one
    /// unwatchable root rebuilds its graph for ever.
    #[cfg(unix)]
    #[test]
    fn a_repeated_declaration_costs_a_blind_root_nothing() {
        let dir = tempdir().unwrap();
        let watched = dir.path().join("наблюдаемый");
        let blind = dir.path().join("слепой");
        std::fs::create_dir_all(&watched).unwrap();
        std::fs::create_dir_all(&blind).unwrap();
        let refusals = RefusedWatches::refusing(vec![blind.clone()]);
        let targets =
            vec![WatchTarget::recursive(watched.clone()), WatchTarget::recursive(blind.clone())];
        let hub = WorkspaceChangeHub::start_targets_refusing(
            targets.clone(),
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());
        assert!(eventually(|| hub.is_partially_blind()));
        hub.wait_until_blindness_announced();
        let cursor = hub.subscribe();
        // A cursor taken while a root is blind is handed that blindness at once; the question
        // here is what the REPEATS cost, so it is cleared first.
        let announced = hub.drain(cursor);
        hub.acknowledge(&announced);
        let rescans = hub.rescan_request_count();
        let rearms = hub.self_rearm_count();

        for _ in 0..3 {
            assert!(!hub.ensure_roots(&targets), "a blind root is not covered");
        }

        assert_eq!(hub.rescan_request_count(), rescans, "a repeat owed a consumer a reconcile");
        assert_eq!(hub.self_rearm_count(), rearms, "a repeat re-armed the watch");
        assert!(!hub.drain(announced.cursor).rescan_required);
        hub.shutdown();
    }

    /// A declared root that does not exist is reported ONCE, when the declaration changes —
    /// and never again while it stands. Reporting it per declaration is what used to turn
    /// every rebuild into a reconcile.
    #[test]
    fn a_declaration_naming_a_missing_root_is_reported_once() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let elsewhere = tempdir().unwrap();
        let missing = elsewhere.path().join("нет-такого-каталога");
        let hub = WorkspaceChangeHub::start(vec![root.clone()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        let targets =
            vec![WatchTarget::recursive(root.clone()), WatchTarget::recursive(missing.clone())];

        assert!(!hub.ensure_roots(&targets), "a root that does not exist is not covered");
        let announced = hub.drain(cursor);
        assert!(announced.rescan_required, "the narrowed coverage has to be announced once");
        let rescans = hub.rescan_request_count();

        for _ in 0..3 {
            assert!(!hub.ensure_roots(&targets));
        }
        assert_eq!(hub.rescan_request_count(), rescans, "the repeat was announced again");
        assert!(!hub.drain(announced.cursor).rescan_required);
        hub.shutdown();
    }

    /// Two owners — the graph and the diagnostics resident — declare the same scan roots
    /// after their own rebuilds. The second declaration is a repeat like any other.
    #[test]
    fn two_owners_declaring_one_set_declare_it_once() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let hub = WorkspaceChangeHub::start(vec![root.clone()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        // Outside the first root on purpose: a directory nested under a recursive watch is
        // already covered, and declaring it moves nothing.
        let extension = tempdir().unwrap();
        let extension = extension.path().to_path_buf();
        let targets =
            vec![WatchTarget::recursive(root.clone()), WatchTarget::recursive(extension.clone())];

        assert!(hub.ensure_roots(&targets), "the first declaration arms the new root");
        let first = hub.drain(cursor);
        assert!(first.rescan_required, "a widened watch owes one reconcile");
        let rescans = hub.rescan_request_count();
        let rearms = hub.self_rearm_count();

        assert!(hub.ensure_roots(&targets), "the second owner declares the same set");
        assert_eq!(hub.rescan_request_count(), rescans, "the second owner cost a reconcile");
        assert_eq!(hub.self_rearm_count(), rearms, "the second owner cost a re-arm");
        assert!(!hub.drain(first.cursor).rescan_required);
        hub.shutdown();
    }

    /// A blind root whose poll thread never started is not "fresh": the hub promised an
    /// observation it has not made, and after two periods that promise reads as overdue.
    #[cfg(unix)]
    #[test]
    fn a_blind_root_whose_poll_cannot_start_reads_overdue() {
        let dir = tempdir().unwrap();
        let watched = dir.path().join("наблюдаемый");
        let blind = dir.path().join("слепой");
        std::fs::create_dir_all(&watched).unwrap();
        std::fs::create_dir_all(&blind).unwrap();
        let refusals = RefusedWatches::refusing(vec![blind.clone()]);
        let hub = WorkspaceChangeHub::start_targets_refusing_unpollable(
            vec![WatchTarget::recursive(watched), WatchTarget::recursive(blind)],
            Duration::from_secs(3600),
            &refusals,
            PollConfig { period: Duration::from_millis(20), verify_bytes: VERIFY_BYTES },
        );
        assert!(hub.wait_until_watching());
        assert!(eventually(|| hub.is_partially_blind()));
        assert!(
            eventually(|| hub.poll_overdue()),
            "a poll that never ran must not read as an up-to-date observation"
        );
        hub.shutdown();
    }

    /// The barrier answers about coverage, and during the hub's own arming there is nothing
    /// to answer with yet: the accepted declaration is recorded before the thread arms a
    /// single root. A caller asking then — the first rebuild after the boot, every time —
    /// would be told its roots are not covered while they are being covered, and log an
    /// alarm about a watch that works.
    #[test]
    fn the_first_declaration_waits_for_the_watch_it_asks_about() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);

        // Asked immediately, without waiting for the watch: the answer must still be about
        // what the hub ends up holding.
        assert!(
            hub.ensure_roots(&[WatchTarget::recursive(dir.path().to_path_buf())]),
            "the hub answered 'not covered' about roots it was arming"
        );
        assert!(hub.wait_until_watching(), "control: it did arm them");
        hub.shutdown();
    }

    /// A declaration is a SET. Two owners derive theirs from the same project and may list
    /// the roots in different orders, and an order-sensitive comparison reads that as a new
    /// declaration: a message and an acknowledgement every time either of them rebuilds. The
    /// coverage checks behind it do hold — nothing is re-armed and nobody is asked to
    /// reconcile, which is what this measures — so the cost is the round trip, paid for ever.
    #[test]
    fn a_declaration_in_another_order_is_the_same_declaration() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("первый");
        let second = dir.path().join("второй");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let hub = WorkspaceChangeHub::start(vec![first.clone(), second.clone()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        let reconciles_before = hub.rescan_request_count();

        assert!(hub.ensure_roots(&[
            WatchTarget::recursive(second.clone()),
            WatchTarget::recursive(first.clone()),
        ]));

        assert_eq!(
            hub.rescan_request_count(),
            reconciles_before,
            "the same set in another order asked every consumer to reconcile"
        );
        assert!(!hub.drain(cursor).rescan_required, "and told this consumer to rescan");
        hub.shutdown();
    }

    /// The daemon's shutdown stops the fallback poll, not just the waiting on it: a hub that
    /// kept walking the workspace after its daemon stopped would read every file in it on a
    /// schedule nobody owns any more.
    #[test]
    fn shutdown_stops_the_fallback_poll() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Модуль.bsl"), "Процедура П() КонецПроцедуры\n").unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_polling(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            PollConfig { period: Duration::from_millis(20), verify_bytes: VERIFY_BYTES },
        );
        hold.release();
        assert_eq!(hub.watch_readiness(STATE_CEILING), WatchReadiness::Failed);
        assert!(eventually(|| hub.poll_count() >= 2));
        assert!(!thread_has_left(&hub), "control: a running hub's thread reads as still there");

        hub.shutdown();
        // The poll runs on the hub's thread: once that thread has left, nothing is left to poll.
        assert!(eventually(|| thread_has_left(&hub)), "the poll outlived the daemon that owned it");
        let stopped = hub.poll_count();
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(120));
        }
        assert_eq!(hub.poll_count(), stopped, "the poll outlived the daemon that owned it");
    }

    /// An edit that keeps a file's size and mtime is invisible to a stat. The poll's own
    /// content check finds it within one pass over the budget, and reports it once.
    #[test]
    fn a_same_stat_edit_is_found_by_the_content_check_once() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let file = root.join("Same.bsl");
        std::fs::write(&file, "Процедура А() КонецПроцедуры\n").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        let mtime = meta.modified().unwrap();
        // A budget of exactly the file: one poll reads it whole.
        let (hub, cursor) = polling_hub(root, meta.len());
        let cursor = hub.drain(cursor).cursor;

        std::fs::write(&file, "Процедура Б() КонецПроцедуры\n").unwrap();
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        assert!(hub.poll_now());
        let batch = hub.drain(cursor);
        assert_eq!(batch.entries.len(), 1, "the content change went unnoticed");
        assert_eq!(batch.entries[0].kind, ChangeKind::MaybeChanged);

        assert!(hub.poll_now());
        assert!(hub.drain(batch.cursor).entries.is_empty(), "the same change was reported again");
        hub.shutdown();
    }

    /// The content check reads no more than its budget in a tick, a file larger than the
    /// budget included: such a file is read across several ticks, and its change is still found.
    #[test]
    fn the_content_check_keeps_its_budget_for_a_large_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let big = root.join("Big.bsl");
        std::fs::write(&big, vec![b'a'; 10 * 1024]).unwrap();
        let targets = [WatchTarget::recursive(root.to_path_buf())];
        let scope = Scope::from_targets(&ResolvedTargets::here(targets.to_vec()), &[]);
        let budget = 1024;
        let mut poller = Poller::default();
        poller.poll(&targets, &scope, budget, true);
        let mut reads = vec![poller.last_read];
        for _ in 0..12 {
            poller.poll(&targets, &scope, budget, false);
            reads.push(poller.last_read);
        }
        assert!(reads.iter().all(|read| *read <= budget), "a tick read past its budget: {reads:?}");
        let mtime = std::fs::metadata(&big).unwrap().modified().unwrap();
        std::fs::write(&big, vec![b'b'; 10 * 1024]).unwrap();
        std::fs::File::options().write(true).open(&big).unwrap().set_modified(mtime).unwrap();
        let found = (0..24).any(|_| !poller.poll(&targets, &scope, budget, false).is_empty());
        assert!(found, "a same-stat change of a file larger than the budget went unnoticed");
    }

    /// The same budget contract with a SECOND file beside the large one — the case a single
    /// file cannot show. A partial read is kept across ticks only if the next tick comes back
    /// to the same key, so this asserts the cursor does not move past the file it suspended.
    #[test]
    fn a_large_file_resumes_across_ticks_with_other_files_beside_it() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("A.bsl"), "Процедура А() КонецПроцедуры\n").unwrap();
        let big = root.join("Big.bsl");
        std::fs::write(&big, vec![b'a'; 10 * 1024]).unwrap();
        let targets = [WatchTarget::recursive(root.to_path_buf())];
        let scope = Scope::from_targets(&ResolvedTargets::here(targets.to_vec()), &[]);
        let budget = 1024;
        let mut poller = Poller::default();
        poller.poll(&targets, &scope, budget, true);
        for _ in 0..24 {
            poller.poll(&targets, &scope, budget, false);
        }
        let mtime = std::fs::metadata(&big).unwrap().modified().unwrap();
        std::fs::write(&big, vec![b'b'; 10 * 1024]).unwrap();
        std::fs::File::options().write(true).open(&big).unwrap().set_modified(mtime).unwrap();
        let found = (0..48).any(|_| {
            poller
                .poll(&targets, &scope, budget, false)
                .iter()
                .any(|(key, _, _)| key.file_name().unwrap() == "Big.bsl")
        });
        assert!(
            found,
            "a same-stat edit in the tail of a file larger than the budget was never seen, so \
             the partial read never survived a tick",
        );
    }

    /// A same-stat edit made before the content check first read the file is still found: a
    /// file first read after the picture may have changed since the reconcile that picture
    /// stood for, so its first reading is reported — once.
    #[test]
    fn a_same_stat_edit_before_the_first_read_is_found() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("A.bsl"), "Процедура А() КонецПроцедуры\n").unwrap();
        let b = root.join("B.bsl");
        std::fs::write(&b, "Процедура Б() КонецПроцедуры\n").unwrap();
        let meta = std::fs::metadata(&b).unwrap();
        let mtime = meta.modified().unwrap();
        // A budget of one file (A and B are the same length): the picture reads A only.
        let (hub, cursor) = polling_hub(root, meta.len());
        let mut cursor = hub.drain(cursor).cursor;
        std::fs::write(&b, "Процедура В() КонецПроцедуры\n").unwrap();
        std::fs::File::options().write(true).open(&b).unwrap().set_modified(mtime).unwrap();
        let mut reported = 0;
        for _ in 0..4 {
            assert!(hub.poll_now());
            let batch = hub.drain(cursor);
            reported += batch.entries.iter().filter(|e| e.raw.file_name() == b.file_name()).count();
            cursor = batch.cursor;
        }
        assert_eq!(reported, 1, "the edit before B's first reading was reported {reported} times");
        hub.shutdown();
    }

    /// A walk that cannot read a directory for the moment does not report the files it did
    /// not see as removed: a live file tombstoned by a failed stat costs every consumer the
    /// index state it had for it.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_not_a_removal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let root = dir.path();
        let sub = root.join("Модуль");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("Module.bsl"), "Процедура П() КонецПроцедуры\n").unwrap();
        let (hub, cursor) = polling_hub(root, VERIFY_BYTES);
        let cursor = hub.drain(cursor).cursor;
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        let polled = hub.poll_now();
        let batch = hub.drain(cursor);
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(polled);
        let removed: Vec<_> =
            batch.entries.iter().filter(|e| e.kind == ChangeKind::MaybeRemoved).collect();
        assert!(removed.is_empty(), "an unreadable directory tombstoned {removed:?}");
        hub.shutdown();
    }

    /// A root nobody watches while the rest is watched is polled: its changes reach every
    /// cursor as records, after the one reconcile its blindness cost, and no poll costs
    /// another.
    #[cfg(unix)]
    #[test]
    fn a_blind_root_is_polled_while_the_rest_is_watched() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(b.join("Old.bsl"), "Процедура С() КонецПроцедуры\n").unwrap();
        let refusals = RefusedWatches::refusing(vec![b.clone()]);
        let hub = WorkspaceChangeHub::start_targets_refusing_polled(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            Duration::from_secs(3600),
            &refusals,
            PollConfig { period: Duration::from_millis(50), verify_bytes: VERIFY_BYTES },
        );
        assert!(hub.wait_until_watching());
        hub.wait_until_blindness_announced();
        let cursor = hub.subscribe();
        let first = hub.drain(cursor);
        assert!(first.rescan_required, "a blind root is announced by one reconcile");
        let rescans = hub.rescan_request_count();

        std::fs::write(b.join("New.bsl"), "Процедура Н() КонецПроцедуры\n").unwrap();
        let mut cursor = first.cursor;
        let delivered = eventually(|| {
            let batch = hub.drain(cursor);
            cursor = batch.cursor;
            assert!(!batch.rescan_required, "a poll cost another reconcile");
            batch.entries.iter().any(|entry| entry.raw.ends_with("New.bsl"))
        });
        assert!(delivered, "a change under the blind root never reached the cursor");
        // A reconcile no poll should raise is an absence. Only the timing profile waits a
        // window out for it; the reading itself holds at any moment.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(200));
        }
        assert_eq!(hub.rescan_request_count(), rescans, "polls raised reconciles of their own");
        hub.shutdown();
    }

    /// The guard on a dropped record's unwatch spares a staying record only where the backend
    /// would actually take it along: on a backend whose registrations stand alone, sparing it
    /// would leave the dropped registration with no name able to remove it (github#185).
    #[test]
    fn an_unwatch_spares_a_record_beneath_only_where_it_would_strip_it() {
        let root = Path::new("/w/a");
        let beneath = [Path::new("/w/a/b")];
        assert_eq!(
            an_unwatch_would_strip_a_record(beneath, root),
            an_unwatch_takes_what_lies_beneath_it(),
        );
        assert!(
            !an_unwatch_would_strip_a_record([Path::new("/w/a")], root),
            "the record's own spelling is not one beneath it",
        );
        assert!(
            !an_unwatch_would_strip_a_record([Path::new("/w/ab"), Path::new("/w")], root),
            "a sibling sharing a prefix of the spelling, or a parent, is not beneath it",
        );
    }

    /// The guard on a lost record's unwatch spares only records that are STAYING. A record
    /// beneath it that this very pass unwatched as obsolete has no registration left to
    /// protect, and sparing the unwatch for its sake would leave the lost root's
    /// registration with no record able to name it (github#185).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_record_unwatched_by_the_same_pass_does_not_spare_a_lost_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (kept, obsolete) = (root.join("a"), root.join("a").join("b"));
        std::fs::create_dir_all(&obsolete).unwrap();
        let refusals = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let disarms: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let (refuse, unwatches, key) = (Arc::clone(&refusals), Arc::clone(&disarms), kept.clone());
        let seams = Arc::new(WatchSeams {
            refuses: Box::new(move |path: &Path| {
                refuse.load(std::sync::atomic::Ordering::SeqCst) && path == key.as_path()
            }),
            disarmed: Box::new(move |path: &Path| {
                unwatches.lock().unwrap_or_else(PoisonError::into_inner).push(path.to_path_buf());
            }),
        });
        // Flat, so the root beneath it stands as a record of its own instead of being absorbed.
        let flat = WatchTarget { path: kept.clone(), recursive: false };
        let hub = WorkspaceChangeHub::start_seamed(
            vec![flat.clone(), WatchTarget::recursive(obsolete.clone())],
            DEFAULT_CAPACITY,
            Duration::from_secs(3600),
            false,
            None,
            Some(seams),
            Vec::new(),
            PollConfig::PRODUCTION,
            BlindPollSeam::default(),
        );
        assert!(hub.wait_until_watching());

        refusals.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!hub.rearm(vec![flat], Duration::from_secs(10)), "a lost root must deny coverage");

        let asked = disarms.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(asked.contains(&obsolete), "the obsolete root is unwatched first: {asked:?}");
        assert!(
            asked.contains(&kept),
            "the lost root's registration was spared for a record already gone: {asked:?}",
        );
        hub.shutdown();
    }

    /// A record beneath a lost root spares its unwatch only if it is still standing when the
    /// pass ENDS: one the same pass loses a moment later held nothing to spare, and the root's
    /// registration would be left with no record able to name it, through every later
    /// declaration (github#185).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_record_lost_later_in_the_same_pass_does_not_spare_a_lost_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (parent, child, other) = (root.join("a"), root.join("a").join("b"), root.join("c"));
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let refusals = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let disarms: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let (refuse, unwatches, keys) =
            (Arc::clone(&refusals), Arc::clone(&disarms), [parent.clone(), child.clone()]);
        let seams = Arc::new(WatchSeams {
            refuses: Box::new(move |path: &Path| {
                refuse.load(std::sync::atomic::Ordering::SeqCst) && keys.iter().any(|k| k == path)
            }),
            disarmed: Box::new(move |path: &Path| {
                unwatches.lock().unwrap_or_else(PoisonError::into_inner).push(path.to_path_buf());
            }),
        });
        // Flat, so the root beneath it stands as a record of its own instead of being absorbed.
        let flat = WatchTarget { path: parent.clone(), recursive: false };
        let hub = WorkspaceChangeHub::start_seamed(
            vec![flat.clone(), WatchTarget::recursive(child.clone())],
            DEFAULT_CAPACITY,
            Duration::from_secs(3600),
            false,
            None,
            Some(seams),
            Vec::new(),
            PollConfig::PRODUCTION,
            BlindPollSeam::default(),
        );
        assert!(hub.wait_until_watching());

        refusals.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!hub.rearm(
            vec![flat, WatchTarget::recursive(child.clone()), WatchTarget::recursive(other)],
            Duration::from_secs(10),
        ));

        let asked = disarms.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            asked.contains(&parent),
            "the lost root was spared for a record the same pass lost: {asked:?}",
        );
        hub.shutdown();
    }

    /// A kept target whose defensive re-arm fails is dropped from the records — and the
    /// registration has to go with the record: a refused arm is not an unwatched one, and a
    /// registration left behind a dropped record outlives every name that could ever take
    /// it away, feeding events from a root the declaration no longer holds (github#185).
    /// The seam is told about every end of a watch, so the unwatch itself is the gate.
    ///
    /// Not on macOS: there the defensive pass skips a kept target
    /// ([`a_kept_target_must_be_re_armed`] is false), so the drop this test provokes cannot
    /// happen and the stand would stay covered. The scenario is real only where the
    /// backend wants the kept target placed again.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_kept_target_lost_on_re_arm_is_also_unwatched() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::create_dir(&c).unwrap();
        // Both ends of every watch in one seam, because a test that sees only the arms
        // cannot tell a registration that was dropped from one that was never placed. Built
        // by hand rather than through `RefusedWatches`, whose bookkeeping is unix-only —
        // the invariant this pins is the hub's own, and it holds wherever a watch can be
        // refused.
        let refusals = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let disarms: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let (refuse_b, unwatches, key) = (Arc::clone(&refusals), Arc::clone(&disarms), b.clone());
        let seams = Arc::new(WatchSeams {
            refuses: Box::new(move |path: &Path| {
                refuse_b.load(std::sync::atomic::Ordering::SeqCst) && path == key.as_path()
            }),
            disarmed: Box::new(move |path: &Path| {
                unwatches.lock().unwrap_or_else(PoisonError::into_inner).push(path.to_path_buf());
            }),
        });
        let hub = WorkspaceChangeHub::start_seamed(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            DEFAULT_CAPACITY,
            Duration::from_secs(3600),
            false,
            None,
            Some(seams),
            Vec::new(),
            PollConfig::PRODUCTION,
            BlindPollSeam::default(),
        );
        assert!(hub.wait_until_watching());
        assert!(
            !disarms.lock().unwrap_or_else(PoisonError::into_inner).contains(&b),
            "the stand starts with `b` watched, not unwatched",
        );

        // `b` becomes unwatchable while it is still declared: the defensive pass of the next
        // re-arm fails on it and drops its record.
        refusals.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            !hub.rearm(
                vec![
                    WatchTarget::recursive(a.clone()),
                    WatchTarget::recursive(b.clone()),
                    WatchTarget::recursive(c.clone()),
                ],
                Duration::from_secs(10),
            ),
            "a dropped target must deny coverage",
        );

        let asked = disarms.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            asked.contains(&b),
            "the registration outlived the record that named it: {asked:?}",
        );
        hub.shutdown();
    }

    /// The sender's "unchanged" is read on ITS thread, and the stand can move before the
    /// thread hears it: a declaration of an older composition can apply between the read and
    /// the send. The newer composition's word then has to be weighed against what stands
    /// when it ARRIVES — read as a bare age, it would raise the mark over a stand it never
    /// agreed with, keep the hub on the older roots, and refuse the very roots it had
    /// confirmed when a build in between declares them again (github#184).
    #[test]
    fn an_unchanged_declaration_overtaken_before_it_arrives_still_applies() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let a = first.path().canonicalize().unwrap();
        let c = second.path().canonicalize().unwrap();
        let hub = WorkspaceChangeHub::start_targets(vec![WatchTarget::recursive(a.clone())]);
        assert!(hub.wait_until_watching());
        let only = |path: &PathBuf| vec![WatchTarget::recursive(path.clone())];
        assert!(hub.rearm_with_epoch(only(&a), 5, Duration::from_secs(10)));

        // Composition 9 read `a` as standing; composition 7 moves the hub to `c` before 9's
        // word is sent.
        assert!(hub.rearm_with_epoch(only(&c), 7, Duration::from_secs(10)));
        hub.remark(only(&a), 9);
        // Acknowledged in order behind the remark, so the thread has heard it by now.
        let _ = hub.rearm_with_epoch(only(&a), 8, Duration::from_secs(10));

        assert!(
            same_declaration(
                &hub.inner.accepted_declaration(),
                ResolvedTargets::here(only(&a)).as_slice(),
            ),
            "the newest composition's roots must stand, not the older one's: {:?}",
            hub.inner.accepted_declaration(),
        );
        hub.shutdown();
    }

    /// A declaration that speaks for an older composition must not roll the hub back onto
    /// the roots a newer one already applied: a slow build passes its caller-side freshness
    /// check while its topology is still live, is overtaken between that check and the
    /// declaration, and arrives after the newer one. The epoch is the age of the composition,
    /// so the thread can order the two however they arrive (github#184).
    #[test]
    fn a_declaration_older_than_the_applied_one_is_ignored() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let a = first.path().canonicalize().unwrap();
        let b = second.path().canonicalize().unwrap();
        let hub = WorkspaceChangeHub::start_targets(vec![WatchTarget::recursive(a.clone())]);
        assert!(hub.wait_until_watching());

        // The newer composition declares both roots, at its own age.
        let both = vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())];
        assert!(hub.rearm_with_epoch(both.clone(), 10, Duration::from_secs(10)));
        let standing = |targets: &[WatchTarget]| {
            same_declaration(
                &hub.inner.accepted_declaration(),
                ResolvedTargets::here(targets.to_vec()).as_slice(),
            )
        };
        assert!(standing(&both), "the newer declaration is the one in force");

        // An older composition declares only `a`. Applied, it would drop `b`'s watch — the
        // hub has to refuse it on its own, because the caller-side check cannot cover the
        // window: it read the live topology before the newer build moved it.
        assert!(
            !hub.rearm_with_epoch(
                vec![WatchTarget::recursive(a.clone())],
                9,
                Duration::from_secs(10),
            ),
            "a superseded declaration is not applied, and not answered as covered",
        );
        assert!(
            standing(&both),
            "the older declaration rolled the hub back: {:?}",
            hub.inner.accepted_declaration(),
        );

        // The ordering is not a freeze: the same age still applies, so a rescope and its
        // re-arm, which travel under one epoch, both take.
        assert!(hub.rearm_with_epoch(
            vec![WatchTarget::recursive(a.clone())],
            10,
            Duration::from_secs(10),
        ));
        assert!(standing(&[WatchTarget::recursive(a.clone())]), "an equal age is not superseded",);
        hub.shutdown();
    }

    /// An unchanged declaration moves no coverage, but its age still has to reach the hub: a
    /// newer composition that AGREES with what the hub holds must raise the mark, or a still
    /// older, DIFFERENT declaration would apply after it — the flip-flop the epoch exists to
    /// refuse, in the one case where nothing but the age travels (github#184).
    #[test]
    fn an_unchanged_declaration_still_advances_the_mark() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let a = first.path().canonicalize().unwrap();
        let b = second.path().canonicalize().unwrap();
        let hub = WorkspaceChangeHub::start_targets(vec![WatchTarget::recursive(a.clone())]);
        assert!(hub.wait_until_watching());

        // A declaration at age 5, then a silent repeat of the same set at age 9.
        assert!(hub.rearm_with_epoch(
            vec![WatchTarget::recursive(a.clone())],
            5,
            Duration::from_secs(10),
        ));
        assert!(hub.ensure_roots_with_epoch(&[WatchTarget::recursive(a.clone())], 9));

        // A different composition, older than the silent repeat, must not apply…
        assert!(
            !hub.rearm_with_epoch(
                vec![WatchTarget::recursive(b.clone())],
                7,
                Duration::from_secs(10),
            ),
            "an age under the silent repeat's is superseded by it",
        );
        assert!(same_declaration(
            &hub.inner.accepted_declaration(),
            ResolvedTargets::here(vec![WatchTarget::recursive(a.clone())]).as_slice(),
        ));

        // …while one above it still does.
        assert!(hub.rearm_with_epoch(
            vec![WatchTarget::recursive(b.clone())],
            11,
            Duration::from_secs(10),
        ));
        assert!(same_declaration(
            &hub.inner.accepted_declaration(),
            ResolvedTargets::here(vec![WatchTarget::recursive(b.clone())]).as_slice(),
        ));
        hub.shutdown();
    }

    /// The content check reads whole files, and the poller it reads them into is what the hub
    /// thread needs to declare a root blind. Enforced structurally because the failure is a
    /// wait, not a wrong answer: one hold of the poller may cover a slice of the budget, never
    /// the budget itself.
    #[test]
    fn the_blind_poll_never_reads_a_whole_budget_under_the_poller_lock() {
        fn whole_budget_line(source: &str) -> Option<String> {
            let production = crate::inventory::production_source(source);
            let at = production.find("fn poll_until_stopped(").expect("the blind poll loop");
            let body = &production[at..];
            let end = body.find("\n    }\n").expect("the blind poll loop ends");
            let body = &body[..end];
            assert!(
                body.contains("VERIFY_SLICE"),
                "the blind poll spends its budget in slices, and this one does not",
            );
            body.lines()
                .map(str::trim)
                .find(|line| {
                    !line.starts_with("//")
                        && line.contains("poll.verify_bytes")
                        && !line.contains("VERIFY_SLICE")
                        && !line.contains("spent <")
                        && !line.contains("- spent")
                })
                .map(str::to_owned)
        }
        let source = include_str!("change_hub.rs").replace("\r\n", "\n");
        let mutant = source.replacen(
            "state.poller.take(now, VERIFY_SLICE.min(inner.poll.verify_bytes), false)",
            "state.poller.take(now, inner.poll.verify_bytes, false)",
            1,
        );
        assert_ne!(source, mutant, "the injected whole-budget call must replace a real call");
        for newline in ["\n", "\r\n"] {
            assert_eq!(
                whole_budget_line(&source.replace('\n', newline)),
                None,
                "a whole verify budget is handed to one hold of the poller",
            );
            assert!(
                whole_budget_line(&mutant.replace('\n', newline)).is_some(),
                "the gate must reject an injected whole-budget call",
            );
        }
    }

    /// The owner accepted a bounded, ONE-TIME cost at the start of a fallback poll: a file
    /// whose tail the first picture had no budget to hash may be reported once when it is
    /// finally read. What was not accepted is repetition — an untouched file reported again
    /// and again would be an endless reindex — and what must not be weakened to buy that
    /// bound is detection: a same-stat edit before the first hash is still found.
    #[test]
    fn an_untouched_file_is_reported_at_most_once_and_a_same_stat_edit_is_still_found() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("A.bsl"), "Процедура А() КонецПроцедуры\n").unwrap();
        let quiet = root.join("Quiet.bsl");
        std::fs::write(&quiet, vec![b'q'; 4 * 1024]).unwrap();
        let targets = [WatchTarget::recursive(root.to_path_buf())];
        let scope = Scope::from_targets(&ResolvedTargets::here(targets.to_vec()), &[]);
        let budget = 512;
        let mut poller = Poller::default();
        poller.poll(&targets, &scope, budget, true);

        // Nothing is touched. The tail may cost ONE report as it is first read; after that the
        // file is known, and a quiet file must go quiet.
        let mut reports = 0;
        for _ in 0..40 {
            reports += poller
                .poll(&targets, &scope, budget, false)
                .iter()
                .filter(|(key, _, _)| key.file_name().unwrap() == "Quiet.bsl")
                .count();
        }
        assert!(
            reports <= 1,
            "an untouched file was reported {reports} times: the accepted cost is one-time, \
             not a loop",
        );

        // And detection is not what paid for that bound: a same-stat edit is still found.
        let mtime = std::fs::metadata(&quiet).unwrap().modified().unwrap();
        std::fs::write(&quiet, vec![b'z'; 4 * 1024]).unwrap();
        std::fs::File::options().write(true).open(&quiet).unwrap().set_modified(mtime).unwrap();
        let found = (0..40).any(|_| {
            poller
                .poll(&targets, &scope, budget, false)
                .iter()
                .any(|(key, _, _)| key.file_name().unwrap() == "Quiet.bsl")
        });
        assert!(found, "a same-stat edit went unnoticed — detection must not pay for the bound");
    }

    /// A hub whose root `b` (and any other refused root) is blind, and whose blind polls each
    /// wait for a permit. One file of `verify_bytes` is read per poll.
    #[cfg(unix)]
    fn gated_blind_hub(
        declared: Vec<WatchTarget>,
        refused: Vec<PathBuf>,
        verify_bytes: u64,
    ) -> (WorkspaceChangeHub, Arc<PollGate>, Arc<RefusedWatches>) {
        barred_blind_hub(declared, refused, verify_bytes, None)
    }

    /// [`gated_blind_hub`] whose announcement also waits at `announce`, when given.
    #[cfg(unix)]
    fn barred_blind_hub(
        declared: Vec<WatchTarget>,
        refused: Vec<PathBuf>,
        verify_bytes: u64,
        announce: Option<Arc<AnnounceBarrier>>,
    ) -> (WorkspaceChangeHub, Arc<PollGate>, Arc<RefusedWatches>) {
        let gate = Arc::new(PollGate::default());
        let refusals = RefusedWatches::refusing(refused);
        let hub = WorkspaceChangeHub::start_targets_refusing_polled_gated(
            declared,
            Duration::from_secs(3600),
            &refusals,
            PollConfig { period: Duration::from_millis(5), verify_bytes },
            Arc::clone(&gate),
            announce,
        );
        assert!(hub.wait_until_watching());
        gate.wait_arrivals(1);
        (hub, gate, refusals)
    }

    /// Files of the blind roots the poll has not read once yet.
    #[cfg(unix)]
    fn unread_blind_files(hub: &WorkspaceChangeHub) -> usize {
        let state = hub.inner.blind_poll.state.lock().unwrap();
        state.poller.files.keys().filter(|key| state.poller.unpictured.contains(*key)).count()
    }

    /// Let polls run one at a time until `cursor` sees its reconcile, and say how many ran and
    /// how many blind files were still unread at that moment — read while the poll is parked.
    #[cfg(unix)]
    fn polls_until_reconcile(
        hub: &WorkspaceChangeHub,
        gate: &PollGate,
        cursor: SinkCursor,
        bound: usize,
    ) -> (usize, DrainBatch, usize) {
        for polls in 0..=bound {
            let unread = unread_blind_files(hub);
            let batch = hub.materialize(cursor);
            if batch.rescan_required {
                return (polls, batch, unread);
            }
            gate.run_polls(1);
        }
        panic!("the reconcile announcing the blind root never came within {bound} polls");
    }

    #[cfg(unix)]
    fn same_stat_edit(path: &Path, byte: u8) {
        let before = std::fs::metadata(path).unwrap();
        std::fs::write(path, vec![byte; before.len() as usize]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(before.modified().unwrap())
            .unwrap();
    }

    #[cfg(unix)]
    fn reported_names(batch: &DrainBatch) -> Vec<String> {
        batch
            .entries
            .iter()
            .map(|entry| entry.canonical.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(unix)]
    fn blind_fixture(files: &[(&str, usize)]) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        for (name, len) in files {
            std::fs::write(b.join(name), vec![b'o'; *len]).unwrap();
        }
        (dir, a, b)
    }

    /// The reconcile that announces a blind root is issued only once every file of that root
    /// has been read once: those readings are the baseline its later readings compare with,
    /// and a consumer re-reading the root before the baseline exists could re-read contents the
    /// baseline then silently absorbs.
    #[cfg(unix)]
    #[test]
    fn a_blind_root_is_read_once_before_its_reconcile_is_announced() {
        let (_dir, a, b) = blind_fixture(&[("One.bsl", 64), ("Two.bsl", 64), ("Three.bsl", 64)]);
        let (hub, gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(b.clone())],
            vec![b],
            64,
        );
        let cursor = hub.subscribe();
        let (polls, _batch, unread) = polls_until_reconcile(&hub, &gate, cursor, 30);
        hub.shutdown();
        assert_eq!(
            unread, 0,
            "the blind reconcile was visible after {polls} polls with {unread} files never read",
        );
    }

    /// A same-stat edit made AFTER a consumer has finished the blind reconcile is reported, and
    /// a file nobody touched is not.
    #[cfg(unix)]
    #[test]
    fn a_same_stat_edit_after_the_blind_reconcile_is_reported_and_untouched_files_are_not() {
        let (_dir, a, b) = blind_fixture(&[("Edited.bsl", 64), ("Untouched.bsl", 64)]);
        let (hub, gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(b.clone())],
            vec![b.clone()],
            64,
        );
        let cursor = hub.subscribe();
        let (_, batch, _) = polls_until_reconcile(&hub, &gate, cursor, 30);
        // The consumer's reconcile, completed: a full re-read and the acknowledgement.
        for name in ["Edited.bsl", "Untouched.bsl"] {
            assert_eq!(std::fs::read(b.join(name)).unwrap().len(), 64);
        }
        hub.acknowledge(&batch);

        same_stat_edit(&b.join("Edited.bsl"), b'e');
        gate.run_polls(4);
        let reported = reported_names(&hub.drain(batch.cursor));
        hub.shutdown();
        assert!(
            !reported.iter().any(|name| name == "Untouched.bsl"),
            "an untouched file was reported after the blind reconcile: {reported:?}",
        );
        assert!(
            reported.iter().any(|name| name == "Edited.bsl"),
            "a same-stat edit made after the blind reconcile was never reported: {reported:?}",
        );
    }

    /// The window between the baseline being ready and the consumer taking the reconcile: an
    /// edit there is reported by the poll's own comparison, whenever the consumer reads.
    #[cfg(unix)]
    #[test]
    fn a_same_stat_edit_before_the_blind_reconcile_is_taken_is_reported() {
        let (_dir, a, b) = blind_fixture(&[("Edited.bsl", 64), ("Untouched.bsl", 64)]);
        let (hub, gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(b.clone())],
            vec![b.clone()],
            64,
        );
        let cursor = hub.subscribe();
        let (_, _visible, _) = polls_until_reconcile(&hub, &gate, cursor, 30);
        same_stat_edit(&b.join("Edited.bsl"), b'e');
        let batch = hub.materialize(cursor);
        hub.acknowledge(&batch);
        gate.run_polls(4);
        let reported = reported_names(&hub.drain(batch.cursor));
        hub.shutdown();
        assert!(
            reported.iter().any(|name| name == "Edited.bsl"),
            "an edit between the baseline and the consumer's reconcile was never reported: {reported:?}",
        );
    }

    /// A root that turns blind while another is still being read once: the reconcile waits for
    /// both, and nothing announces the first one's readiness over the second one's files.
    #[cfg(unix)]
    #[test]
    fn a_root_turning_blind_during_the_first_reading_waits_for_it_too() {
        let (_dir, a, b) = blind_fixture(&[("B1.bsl", 64), ("B2.bsl", 64), ("B3.bsl", 64)]);
        let c = a.parent().unwrap().join("c");
        std::fs::create_dir(&c).unwrap();
        for name in ["C1.bsl", "C2.bsl"] {
            std::fs::write(c.join(name), vec![b'c'; 64]).unwrap();
        }
        let (hub, gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            vec![b.clone(), c.clone()],
            64,
        );
        let cursor = hub.subscribe();
        if !hub.materialize(cursor).rescan_required {
            gate.run_polls(1);
        }
        // Acknowledged as not wholly armed — `c` is refused on purpose — so the answer is not
        // the premise; the premise is `c` in the blind poll's set.
        let _ = hub.rearm(
            vec![
                WatchTarget::recursive(a),
                WatchTarget::recursive(b),
                WatchTarget::recursive(c.clone()),
            ],
            Duration::from_secs(10),
        );
        let joined = || {
            let state = hub.inner.blind_poll.state.lock().unwrap();
            state.poller.files.keys().filter(|key| key.starts_with(&c)).count()
        };
        assert!(eventually(|| joined() == 2), "the stand needs the second root in the blind set");
        // The declaration move has a reconcile of its own, and a consumer may take it at once.
        // What must follow the first reading of every blind file is a reconcile issued after it.
        let mut cursor = cursor;
        let mut polls = 0;
        while unread_blind_files(&hub) > 0 && polls < 40 {
            let batch = hub.materialize(cursor);
            hub.acknowledge(&batch);
            cursor = batch.cursor;
            gate.run_polls(1);
            polls += 1;
        }
        assert_eq!(
            unread_blind_files(&hub),
            0,
            "the first reading did not finish within {polls} polls"
        );
        let after = hub.materialize(cursor);
        hub.shutdown();
        assert!(
            after.rescan_required,
            "no reconcile was issued after the last blind file was read once ({polls} polls)",
        );
    }

    /// A root larger than one poll's budget does not hold every other blind root's content
    /// check until it has been read through: an edit in a root read long ago is found while
    /// the large one is still being read once, and the large one's reconcile still comes.
    #[cfg(unix)]
    #[test]
    fn a_large_joining_root_neither_starves_the_others_nor_stalls() {
        let (_dir, a, b) = blind_fixture(&[("Settled.bsl", 64)]);
        let c = a.parent().unwrap().join("c");
        std::fs::create_dir(&c).unwrap();
        std::fs::write(c.join("Large.bsl"), vec![b'l'; 64 * 12]).unwrap();
        let (hub, gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            vec![b.clone(), c.clone()],
            64,
        );
        let cursor = hub.subscribe();
        let (_, batch, _) = polls_until_reconcile(&hub, &gate, cursor, 30);
        hub.acknowledge(&batch);
        let mut cursor = batch.cursor;
        while unread_blind_files(&hub) > 0 {
            gate.run_polls(1);
        }

        let _ = hub.rearm(
            vec![
                WatchTarget::recursive(a),
                WatchTarget::recursive(b.clone()),
                WatchTarget::recursive(c.clone()),
            ],
            Duration::from_secs(10),
        );
        assert!(
            eventually(|| unread_blind_files(&hub) > 0),
            "the stand needs the large root in the blind set",
        );
        same_stat_edit(&b.join("Settled.bsl"), b's');
        let mut found_while_large_unread = false;
        let mut large_announced = false;
        for _ in 0..80 {
            gate.run_polls(1);
            let batch = hub.materialize(cursor);
            let large_unread = unread_blind_files(&hub) > 0;
            if reported_names(&batch).iter().any(|name| name == "Settled.bsl") && large_unread {
                found_while_large_unread = true;
            }
            hub.acknowledge(&batch);
            cursor = batch.cursor;
            if batch.rescan_required && !large_unread {
                large_announced = true;
                break;
            }
        }
        hub.shutdown();
        assert!(
            found_while_large_unread,
            "an edit in a settled blind root waited for a joining root to be read through",
        );
        assert!(large_announced, "the large root's reconcile never came within the bound");
    }

    /// A file that cannot be read during the first reading does not hold the reconcile for
    /// ever, and has no baseline: once it can be read, it is reported.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_neither_holds_the_blind_reconcile_nor_gets_a_silent_baseline() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, a, b) = blind_fixture(&[("Readable.bsl", 64), ("Closed.bsl", 64)]);
        let closed = b.join("Closed.bsl");
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&closed).is_ok() {
            eprintln!("skipping: mode 0o000 is not an obstacle for this user");
            return;
        }
        let (hub, gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(b.clone())],
            vec![b.clone()],
            64,
        );
        let cursor = hub.subscribe();
        let (_, batch, unread) = polls_until_reconcile(&hub, &gate, cursor, 30);
        hub.acknowledge(&batch);
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o644)).unwrap();
        gate.run_polls(4);
        let reported = reported_names(&hub.drain(batch.cursor));
        hub.shutdown();
        assert_eq!(unread, 0, "the reconcile came with readable files never read");
        assert!(
            reported.iter().any(|name| name == "Closed.bsl"),
            "a file first read after the reconcile got a silent baseline: {reported:?}",
        );
    }

    /// A blind root that vanishes before its first reading is done does not take the reconcile
    /// it owed with it: its files were under a subtree nobody watched, the poll forgets them
    /// without a record, and a consumer still holding them learns of it only by reconciling.
    #[cfg(unix)]
    #[test]
    fn a_blind_root_gone_before_its_first_reading_still_gets_its_reconcile() {
        let (_dir, a, b) = blind_fixture(&[("One.bsl", 64), ("Two.bsl", 64), ("Three.bsl", 64)]);
        let (hub, gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(b.clone())],
            vec![b.clone()],
            64,
        );
        let cursor = hub.subscribe();
        gate.run_polls(1);
        let before = hub.materialize(cursor);
        assert!(
            !before.rescan_required,
            "the stand needs the first reading still under way when the root goes",
        );
        std::fs::remove_dir_all(&b).unwrap();
        assert!(hub.tick_now());
        assert!(
            eventually(|| !hub.is_partially_blind()),
            "the stand needs the gone root out of the blind set",
        );
        let owed = hub.materialize(cursor).rescan_required;
        // The hub's own record of an announcement still to come is settled too: left standing,
        // it would hold back the reconcile a newcomer is handed the next time a root goes blind.
        let settled = crate::change_hub::test_support::eventually_within(200, || {
            !hub.inner.blind_poll.reconcile_pending.load(Ordering::SeqCst)
        });
        hub.shutdown();
        assert!(owed, "the reconcile a blind root owed was forgotten when the root went away");
        assert!(settled, "an announcement nobody will make was left pending");
    }

    /// A hub shut down while its blind poll waits returns promptly and leaves no poll behind.
    #[cfg(unix)]
    #[test]
    fn a_blind_poll_parked_before_its_first_reading_stops_with_the_hub() {
        let (_dir, a, b) = blind_fixture(&[("One.bsl", 64)]);
        let (hub, _gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(b.clone())],
            vec![b],
            64,
        );
        let started = Instant::now();
        hub.shutdown();
        assert!(eventually(|| !hub.blind_poll_running()), "the blind poll outlived the hub",);
        if cfg!(timing_contract) {
            assert!(started.elapsed() < Duration::from_secs(5));
        }
    }

    /// A hub whose blind root `b` holds two files, with `subscribers` cursors taken before its
    /// first reading, and the announcement that follows that reading parked at `point`.
    #[cfg(unix)]
    #[allow(clippy::type_complexity)] // rationale: a stand's parts, destructured at every call.
    fn parked_announcement(
        point: AnnouncePoint,
        subscribers: usize,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        WorkspaceChangeHub,
        Arc<PollGate>,
        Arc<AnnounceBarrier>,
        Arc<RefusedWatches>,
        Vec<SinkCursor>,
    ) {
        let (dir, a, b) = blind_fixture(&[("Edited.bsl", 64), ("Untouched.bsl", 64)]);
        let barrier = Arc::new(AnnounceBarrier::default());
        barrier.arm(point);
        let (hub, gate, refusals) = barred_blind_hub(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(b.clone())],
            vec![b.clone()],
            64,
            Some(Arc::clone(&barrier)),
        );
        let cursors = (0..subscribers).map(|_| hub.subscribe()).collect();
        gate.allow(8);
        barrier.wait_parked();
        (dir, b, hub, gate, barrier, refusals, cursors)
    }

    #[cfg(unix)]
    fn finish_announcement(hub: &WorkspaceChangeHub, barrier: &AnnounceBarrier) {
        barrier.release();
        hub.wait_until_blindness_announced();
    }

    /// A consumer that subscribes while the blind reconcile is being issued, with nobody else
    /// listening, is owed that reconcile: it arrived after the reconcile flagged every cursor
    /// there was, and it has not been told the root is blind any other way. Its reconcile done,
    /// a same-stat edit is reported and an untouched file is not.
    #[cfg(unix)]
    #[test]
    fn a_newcomer_arriving_while_the_blind_reconcile_is_issued_is_owed_it() {
        let (_dir, b, hub, gate, barrier, _refusals, _) =
            parked_announcement(AnnouncePoint::AfterIssue, 0);
        let newcomer = hub.subscribe();
        finish_announcement(&hub, &barrier);
        let owed = hub.materialize(newcomer);
        let mut reported = Vec::new();
        if owed.rescan_required {
            hub.acknowledge(&owed);
            same_stat_edit(&b.join("Edited.bsl"), b'e');
            gate.run_polls(4);
            reported = reported_names(&hub.drain(owed.cursor));
        }
        hub.shutdown();
        assert!(
            owed.rescan_required,
            "a consumer subscribed during the announcement was never told"
        );
        assert!(
            reported.iter().any(|name| name == "Edited.bsl"),
            "a same-stat edit after the newcomer's reconcile was not reported: {reported:?}",
        );
        assert!(
            !reported.iter().any(|name| name == "Untouched.bsl"),
            "an untouched file was reported: {reported:?}",
        );
    }

    /// The same newcomer when the only other consumer has already settled its reconcile: there
    /// is no open window left to inherit, so the announcement itself has to reach it.
    #[cfg(unix)]
    #[test]
    fn a_newcomer_arriving_after_everyone_settled_the_issued_blind_reconcile_is_owed_it() {
        let (_dir, _b, hub, _gate, barrier, _refusals, cursors) =
            parked_announcement(AnnouncePoint::AfterIssue, 1);
        let taken = hub.materialize(cursors[0]);
        hub.acknowledge(&taken);
        let settled = !hub.materialize(taken.cursor).rescan_required;
        let newcomer = hub.subscribe();
        finish_announcement(&hub, &barrier);
        let owed = hub.materialize(newcomer).rescan_required;
        hub.shutdown();
        assert!(
            taken.rescan_required && settled,
            "the stand needs the reconcile issued and settled"
        );
        assert!(owed, "a consumer subscribed after the others settled was never told");
    }

    /// Let the gated poll run until the blind reconcile is announced.
    #[cfg(unix)]
    fn polls_until_announced(hub: &WorkspaceChangeHub, gate: &PollGate) {
        for _ in 0..30 {
            if !hub.inner.blind_poll.reconcile_pending.load(Ordering::SeqCst) {
                return;
            }
            gate.run_polls(1);
        }
        panic!("the reconcile announcing the blind root never came within 30 polls");
    }

    /// Countercontrol: while the blind reconcile is still to come, a newcomer inherits a window
    /// another consumer still owes, under that window's identity — and the announcement then
    /// reaches both as one loss.
    #[cfg(unix)]
    #[test]
    fn a_newcomer_before_the_blind_reconcile_inherits_a_window_still_owed() {
        let (_dir, a, b) = blind_fixture(&[("Edited.bsl", 64), ("Untouched.bsl", 64)]);
        let (hub, gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(b.clone())],
            vec![b],
            64,
        );
        let owing = hub.subscribe();
        hub.deliver_backend_error_for_test();
        let newcomer = hub.subscribe();
        let (window, inherited) = (hub.materialize(owing), hub.materialize(newcomer));
        polls_until_announced(&hub, &gate);
        let (owing, newcomer) = (hub.materialize(owing), hub.materialize(newcomer));
        hub.shutdown();
        assert!(window.rescan_required && inherited.rescan_required);
        assert_eq!(inherited.loss_token(), window.loss_token(), "one window is one loss");
        assert!(owing.rescan_required && newcomer.rescan_required);
        assert_eq!(newcomer.loss_token(), owing.loss_token(), "one announcement is one loss");
        assert_ne!(
            owing.loss_token(),
            window.loss_token(),
            "the announcement is a loss of its own"
        );
    }

    /// What the accumulator says about the blind reconcile right now: the window's reason, and
    /// the announcement the hub still owes.
    #[cfg(unix)]
    fn announcement_state(hub: &WorkspaceChangeHub) -> (Option<DegradeReason>, bool) {
        let reason = hub.inner.lock_acc().degrade_reason.clone();
        (reason, hub.inner.blind_poll.reconcile_pending.load(Ordering::SeqCst))
    }

    /// A consumer that subscribes BEFORE the reconcile is issued is flagged by the reconcile
    /// itself, under the window's identity — the same loss the consumer that was there all
    /// along is told about.
    ///
    /// The boundary is asserted, not assumed: at the barrier no window has been entered and the
    /// announcement is still owed, so a newcomer here cannot be answered by the published-
    /// blindness path in `subscribe` and its debt can only come from the reconcile.
    #[cfg(unix)]
    #[test]
    fn a_newcomer_before_the_blind_reconcile_is_issued_is_flagged_by_it() {
        let (_dir, _b, hub, _gate, barrier, _refusals, cursors) =
            parked_announcement(AnnouncePoint::BeforeIssue, 1);
        let (reason, pending) = announcement_state(&hub);
        let newcomer = hub.subscribe();
        let at_barrier = hub.materialize(newcomer);
        finish_announcement(&hub, &barrier);
        let (resident, flagged) = (hub.materialize(cursors[0]), hub.materialize(newcomer));
        hub.shutdown();
        assert_eq!(reason, None, "the stand needs a barrier before the reconcile is issued");
        assert!(pending, "the stand needs the announcement still owed at the barrier");
        assert!(!at_barrier.rescan_required, "a newcomer was answered before the reconcile");
        assert!(flagged.rescan_required, "the reconcile did not flag a consumer that was there");
        assert!(resident.rescan_required);
        assert_eq!(
            flagged.loss_token(),
            resident.loss_token(),
            "one reconcile reaching two cursors is one loss",
        );
    }

    /// A consumer that subscribes AFTER the reconcile is issued cannot be flagged by it, and is
    /// owed one of its own — a different loss, because it is a different event for this cursor.
    #[cfg(unix)]
    #[test]
    fn a_newcomer_after_the_blind_reconcile_is_issued_is_owed_one_of_its_own() {
        let (_dir, _b, hub, _gate, barrier, _refusals, cursors) =
            parked_announcement(AnnouncePoint::AfterIssue, 1);
        let (reason, pending) = announcement_state(&hub);
        let newcomer = hub.subscribe();
        let at_barrier = hub.materialize(newcomer);
        finish_announcement(&hub, &barrier);
        let (resident, owed) = (hub.materialize(cursors[0]), hub.materialize(newcomer));
        hub.shutdown();
        assert_eq!(
            reason,
            Some(DegradeReason::RewatchFailed),
            "the stand needs a barrier after the reconcile is issued",
        );
        assert!(!pending, "the announcement is published with the reconcile, under one hold");
        assert!(
            at_barrier.rescan_required,
            "a newcomer after the reconcile was left owing nothing"
        );
        assert!(owed.rescan_required && resident.rescan_required);
        assert_ne!(
            owed.loss_token(),
            resident.loss_token(),
            "a reconcile the newcomer was never inside was given its identity",
        );
    }

    /// Countercontrol: a consumer re-subscribing while the reconcile is being issued carries the
    /// debt it holds, under its identity.
    #[cfg(unix)]
    #[test]
    fn a_resubscription_while_the_blind_reconcile_is_issued_carries_its_debt() {
        let (_dir, _b, hub, _gate, barrier, _refusals, cursors) =
            parked_announcement(AnnouncePoint::AfterIssue, 1);
        let held = hub.materialize(cursors[0]);
        let (replaced, _) = hub.resubscribe(cursors[0]);
        finish_announcement(&hub, &barrier);
        let carried = hub.materialize(replaced);
        hub.shutdown();
        assert!(held.rescan_required, "the stand needs a debt to carry");
        assert!(carried.rescan_required, "the debt did not survive re-subscribing");
        assert_eq!(carried.loss_token(), held.loss_token());
    }

    /// Countercontrol: a batch taken before the blind reconcile was issued, acknowledged after,
    /// does not settle it.
    #[cfg(unix)]
    #[test]
    fn a_batch_taken_before_the_blind_reconcile_does_not_settle_it() {
        let (_dir, a, b) = blind_fixture(&[("Edited.bsl", 64), ("Untouched.bsl", 64)]);
        let (hub, gate, _refusals) = gated_blind_hub(
            vec![WatchTarget::recursive(a), WatchTarget::recursive(b.clone())],
            vec![b],
            64,
        );
        let cursor = hub.subscribe();
        let old = hub.materialize(cursor);
        polls_until_announced(&hub, &gate);
        hub.acknowledge(&old);
        let owed = hub.materialize(old.cursor).rescan_required;
        hub.shutdown();
        assert!(!old.rescan_required, "the stand needs a batch taken before the announcement");
        assert!(owed, "an acknowledgement of an older batch settled the blind reconcile");
    }

    /// The blind roots the hub has published, as the subscription path reads them.
    #[cfg(unix)]
    fn published_blind(hub: &WorkspaceChangeHub) -> Vec<PathBuf> {
        hub.inner.blind_targets.lock().unwrap().clone()
    }

    /// A root that joins the blind set while the poll is issuing the reconcile of the others:
    /// the declaration waits, so no reconcile is issued over files nobody has read — and the
    /// joining root's own reconcile follows its first reading.
    ///
    /// The declaration goes through the real path (`rearm`), on its own thread, because the
    /// hub thread is what the reconcile's hold of the poll state blocks. Its own reconcile —
    /// the one a declaration move owes — is counted before the barrier is released, so the
    /// blind announcement is never confused with it.
    #[cfg(unix)]
    #[test]
    fn a_root_joining_while_an_announcement_is_issued_waits_for_it() {
        let (_dir, a, b) = blind_fixture(&[("B1.bsl", 64)]);
        let c = a.parent().unwrap().join("c");
        std::fs::create_dir(&c).unwrap();
        for name in ["C1.bsl", "C2.bsl"] {
            std::fs::write(c.join(name), vec![b'c'; 64]).unwrap();
        }
        let barrier = Arc::new(AnnounceBarrier::default());
        barrier.arm(AnnouncePoint::BeforeIssue);
        let (hub, gate, _refusals) = barred_blind_hub(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            vec![b.clone(), c.clone()],
            64,
            Some(Arc::clone(&barrier)),
        );
        let cursor = hub.subscribe();
        gate.allow(1);
        barrier.wait_parked();
        let issued = || hub.inner.lock_acc().losses_issued;
        let at_park = issued();

        let (asking, asked) = std::sync::mpsc::channel();
        let declaring = {
            let (hub, a, b, c) = (hub.clone(), a.clone(), b.clone(), c.clone());
            std::thread::spawn(move || {
                asking.send(()).unwrap();
                hub.rearm(
                    vec![
                        WatchTarget::recursive(a),
                        WatchTarget::recursive(b),
                        WatchTarget::recursive(c),
                    ],
                    Duration::from_secs(10),
                )
            })
        };
        asked
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the declaration was never asked for");
        // The reconcile is held at the barrier, so the join it would announce cannot come yet. A
        // window can only hide such a join from this check, never invent one, so it is a window,
        // and only the timing profile waits it out.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_secs(2));
        }
        let joined_at_the_barrier = published_blind(&hub).contains(&c);

        barrier.release();
        // The declaration is acknowledged as NOT wholly armed: `c` is refused on purpose, which
        // is what puts it in the blind set at all.
        let wholly_armed = declaring.join().expect("the declaring thread");
        let joined = eventually(|| unread_blind_files(&hub) == 2);
        let after_join = issued();

        let mut cursor = cursor;
        let mut announced_while_unread = Vec::new();
        let mut polls = 0;
        while unread_blind_files(&hub) > 0 && polls < 20 {
            let batch = hub.materialize(cursor);
            hub.acknowledge(&batch);
            cursor = batch.cursor;
            let before = issued();
            gate.run_polls(1);
            polls += 1;
            if issued() > before && unread_blind_files(&hub) > 0 {
                announced_while_unread.push(polls);
            }
        }
        let owed_after_reading = issued() > after_join && hub.materialize(cursor).rescan_required;
        hub.shutdown();
        assert!(
            !joined_at_the_barrier,
            "a root joined the blind set while the reconcile of the others was being issued",
        );
        assert!(!wholly_armed, "the stand needs the joining root refused, not watched");
        assert!(joined, "the stand needs the joining root in the blind set");
        assert!(after_join > at_park, "the stand needs the reconcile and the declaration's own");
        assert!(
            announced_while_unread.is_empty(),
            "announced over unread files at polls {announced_while_unread:?}",
        );
        assert!(owed_after_reading, "the joining root's reconcile never followed its reading");
    }

    /// The reconcile announcing a blind root already tells every consumer to re-read that root
    /// whole. Reporting each of its files as changed on top of that is a second full re-index
    /// of work just done — and it is every file, not a file that changed.
    #[cfg(unix)]
    #[test]
    fn a_root_turning_blind_does_not_report_its_untouched_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(b.join("Old.bsl"), "Процедура С() КонецПроцедуры\n").unwrap();
        std::fs::write(b.join("Older.bsl"), "Процедура Д() КонецПроцедуры\n").unwrap();
        let refusals = RefusedWatches::refusing(vec![b.clone()]);
        let hub = WorkspaceChangeHub::start_targets_refusing_polled(
            vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
            Duration::from_secs(3600),
            &refusals,
            PollConfig { period: Duration::from_millis(50), verify_bytes: VERIFY_BYTES },
        );
        assert!(hub.wait_until_watching());
        hub.wait_until_blindness_announced();
        let cursor = hub.subscribe();
        let first = hub.drain(cursor);
        assert!(first.rescan_required, "the stand needs the reconcile that announces blindness");

        // Nothing is touched from here on. Several polls, so the first reading of every file
        // has certainly happened.
        let mut cursor = first.cursor;
        let mut reported: Vec<String> = Vec::new();
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(60));
            let batch = hub.drain(cursor);
            cursor = batch.cursor;
            reported.extend(batch.entries.iter().map(|entry| entry.raw.display().to_string()));
        }
        hub.shutdown();
        assert!(
            reported.is_empty(),
            "the reconcile already covered these files and they never changed: {reported:#?}",
        );
    }

    /// An interrupted hub answers every waiter at once, however long it asked to wait, and
    /// keeps answering: a stop that woke one wait and let the next one sleep would park the
    /// owner right back.
    #[test]
    fn interrupted_waiters_return_at_once_and_stay_released() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        // Each wait below would sleep its whole budget. An ordinary run gives it one no run
        // reaches, so a wait that comes back at all was released; under `timing_contract` the
        // budget is a minute, and half of that tells a released wait from one that slept on.
        const WAIT: Duration = STATE_CEILING;
        let since = hub.generation();
        let waiter = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                hub.wait_for_change(since, WAIT);
                started.elapsed()
            })
        };
        // Nothing tells when the waiter has parked, and an interruption that lands before it
        // does must release it just the same; the timing profile aims at the parked wait.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(100));
        }
        hub.interrupt_waiters();
        let parked = waiter.join().unwrap();

        let started = Instant::now();
        hub.wait_for_change(hub.generation(), WAIT);
        let later = started.elapsed();
        let started = Instant::now();
        hub.watch_readiness(WAIT);
        let readiness = started.elapsed();
        if cfg!(timing_contract) {
            assert!(parked < WAIT / 2, "the parked wait slept on");
            assert!(later < WAIT / 2, "a later wait slept on");
            assert!(readiness < WAIT / 2, "a readiness wait slept on");
            assert!(parked < Duration::from_secs(5), "the parked wait slept on");
            assert!(later < Duration::from_secs(1), "a later wait slept on");
            assert!(readiness < Duration::from_secs(1), "a readiness wait slept on");
        }
        hub.shutdown();
    }

    #[test]
    fn events_seen_counts_every_raw_event() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        assert_eq!(hub.events_seen(), 0);
        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            dir.path().join("a.bsl"),
        ));
        hub.ingest_for_test(change_event(
            EventKind::Remove(RemoveKind::Any),
            dir.path().join("a.bsl"),
        ));
        assert_eq!(hub.events_seen(), 2);
    }

    /// The default cache sits inside the recursive watch, so every index write the
    /// server performs would otherwise be an event about the workspace it analyzed.
    #[test]
    fn writes_inside_the_excluded_cache_are_not_workspace_changes() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join(".build");
        std::fs::create_dir_all(&cache).unwrap();
        let hub = WorkspaceChangeHub::start_targets_excluding(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            vec![cache.clone()],
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        for name in ["writer.lease", "writer.tmp.4242", "writer.lease.lock"] {
            hub.ingest_for_test(change_event(EventKind::Create(CreateKind::Any), cache.join(name)));
        }
        assert!(hub.materialize(cursor).entries.is_empty(), "a cache write was recorded");

        // Positive control: a source file in the same root must still be recorded, or
        // the assertion above would hold on a hub that records nothing at all.
        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            dir.path().join("M.bsl"),
        ));
        assert_eq!(hub.materialize(cursor).entries.len(), 1, "a source edit was dropped");
    }

    /// An event names the root by whichever spelling the watch was armed with, and that
    /// is the pre-canonical one. A filter built on the canonical spelling alone matches
    /// nothing on Windows (`\\?\C:\...` against `C:\...`) while staying green anywhere
    /// the two happen to coincide; a symlinked root is the same defect, reproducible here.
    #[cfg(unix)]
    #[test]
    fn the_excluded_root_is_recognised_under_either_spelling() {
        let real = tempdir().unwrap();
        let links = tempdir().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        std::fs::create_dir_all(real.path().join(".build")).unwrap();

        let layout = crate::cache::WorkspaceCacheLayout::for_workspace(&link);
        let hub = WorkspaceChangeHub::start_targets_excluding(
            vec![WatchTarget::recursive(link.clone())],
            layout.spellings().iter().map(|p| p.to_path_buf()).collect(),
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        for root in [link.join(".build"), real.path().join(".build")] {
            hub.ingest_for_test(change_event(
                EventKind::Create(CreateKind::Any),
                root.join("writer.lease"),
            ));
        }
        assert!(
            hub.materialize(cursor).entries.is_empty(),
            "the cache was recognised under only one of its two spellings"
        );
    }

    /// The exclusion is a path, not a name: `starts_with` on a `Path` compares whole
    /// components, and a filter that compared strings would swallow a sibling directory
    /// whose name merely begins the same way.
    #[test]
    fn a_sibling_sharing_the_cache_name_prefix_is_not_excluded() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start_targets_excluding(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            vec![dir.path().join(".build")],
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        std::fs::create_dir_all(dir.path().join(".buildfoo")).unwrap();
        let sibling = dir.path().join(".buildfoo").join("M.bsl");
        std::fs::write(&sibling, "").unwrap();
        hub.ingest_for_test(change_event(EventKind::Create(CreateKind::Any), sibling));
        assert_eq!(hub.materialize(cursor).entries.len(), 1, "a sibling directory was excluded");
    }

    #[cfg(unix)]
    #[test]
    fn a_runtime_subtree_is_pruned_before_entering_its_excluded_child() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("runtime");
        let blocked = root.join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(root.join("Allowed.bsl"), "").unwrap();
        std::fs::write(blocked.join("Hidden.bsl"), "").unwrap();
        std::os::unix::fs::symlink(&root, blocked.join("cycle")).unwrap();

        let mut records = Vec::new();
        let mut unreadable = Vec::new();
        collect_subtree_noting(
            &root,
            &mut records,
            Some(&mut unreadable),
            &ExcludedPaths::new([blocked.clone()]),
        );

        assert!(unreadable.is_empty(), "the runtime walk entered the excluded cycle");
        assert!(records.iter().any(|(_, path, _)| path == &root.join("Allowed.bsl")));
        assert!(
            records.iter().all(|(_, path, _)| !path.starts_with(&blocked)),
            "an excluded runtime descendant was collected: {records:?}"
        );
    }

    /// The default cache is lazy: it does not exist when the hub starts. The exclusion
    /// still has to hold once the first index write creates it.
    #[test]
    fn a_cache_created_after_the_hub_started_is_still_excluded() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join(".build");
        let layout = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        assert!(!cache.exists(), "the fixture must start without the cache");
        let hub = WorkspaceChangeHub::start_targets_excluding(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            layout.spellings().iter().map(|p| p.to_path_buf()).collect(),
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        std::fs::create_dir_all(&cache).unwrap();
        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            cache.join("writer.lease"),
        ));
        assert!(hub.materialize(cursor).entries.is_empty(), "a lazily-created cache was watched");
    }

    /// The service directories of the workspace root are stated as exclusions, and the hub
    /// drops their events the same way it drops the cache's: `git fetch`, a build or a
    /// package-manager run can produce thousands of events in one burst, and every event
    /// that reaches the accumulator is a step toward the overflow that forces a full
    /// rescan — while the files it names were never workspace inputs. Stated as PATHS, not
    /// by name: a `target` deeper in the tree is nobody's to exclude, and stays in the
    /// universe.
    #[test]
    fn writes_inside_service_directories_are_not_workspace_changes() {
        let dir = tempdir().unwrap();
        for service in [".git", "target", "node_modules"] {
            std::fs::create_dir_all(dir.path().join(service)).unwrap();
        }
        std::fs::create_dir_all(dir.path().join("sub").join("target")).unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        let hub = WorkspaceChangeHub::start_targets_excluding(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            cache.exclusions(dir.path()),
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        for path in [
            dir.path().join(".git").join("objects").join("ab").join("cdef"),
            dir.path().join("target").join("debug").join("build").join("x.out"),
            dir.path().join("node_modules").join("pkg").join("index.js"),
        ] {
            hub.ingest_for_test(change_event(EventKind::Create(CreateKind::Any), path));
        }
        assert!(
            hub.materialize(cursor).entries.is_empty(),
            "a service-directory write was recorded",
        );

        // Positive controls: the exclusion is a stated path, not a name — a nested
        // `target` is still a workspace directory — and a source file must survive.
        for path in [
            dir.path().join("sub").join("target").join("deep").join("file.o"),
            dir.path().join("M.bsl"),
        ] {
            hub.ingest_for_test(change_event(EventKind::Create(CreateKind::Any), path));
        }
        assert_eq!(
            hub.materialize(cursor).entries.len(),
            2,
            "a stated-path exclusion swallowed a nested directory by name",
        );
    }

    /// The exclusion must survive a real re-arm. `Scope` is rebuilt from the targets
    /// every time the watch is re-pointed, and `ensure_roots` is called by consumers
    /// that know the scan roots but nothing about the cache — so the gate has to force
    /// the path that rebuilds the scope, not the early return that skips it.
    #[test]
    fn a_rearm_onto_new_roots_keeps_the_exclusion() {
        let dir = tempdir().unwrap();
        let extension = tempdir().unwrap();
        let cache = dir.path().join(".build");
        std::fs::create_dir_all(&cache).unwrap();
        let hub = WorkspaceChangeHub::start_targets_excluding(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            vec![cache.clone()],
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        // Before: the extension root is outside the scope, so its events are dropped.
        // This is what makes the re-arm below observable rather than assumed.
        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            extension.path().join("M.bsl"),
        ));
        assert!(hub.materialize(cursor).entries.is_empty());

        assert!(hub.rearm(
            vec![
                WatchTarget::recursive(dir.path().to_path_buf()),
                WatchTarget::recursive(extension.path().to_path_buf()),
            ],
            Duration::from_secs(5),
        ));
        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            cache.join("writer.lease"),
        ));
        assert!(hub.materialize(cursor).entries.is_empty(), "the re-arm dropped the exclusion");

        // Positive control: the added root is now live, which is the proof the re-arm
        // actually rebuilt the scope instead of returning early as a no-op.
        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            extension.path().join("M.bsl"),
        ));
        assert_eq!(hub.materialize(cursor).entries.len(), 1, "the re-arm was a no-op");
    }

    /// A cache outside the workspace changes nothing: the tree is watched as before.
    #[test]
    fn a_cache_outside_the_workspace_leaves_the_watch_untouched() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start_targets_excluding(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            vec![outside.path().to_path_buf()],
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            dir.path().join("M.bsl"),
        ));
        assert_eq!(
            hub.materialize(cursor).entries.len(),
            1,
            "an external cache narrowed the watch"
        );
    }

    /// A wake costs every sink a full drain-and-apply pass, and a sink that writes
    /// into the watched tree turns that pass into the next event. The observable is
    /// the wake counter, not "did `wait_for_change` return": once the wait rechecks
    /// its predicate it swallows a spurious wake, so a gate phrased over the wait
    /// stays green on a hub that still disturbs everyone on every foreign event.
    #[test]
    fn an_event_filtered_to_nothing_wakes_nobody() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());

        let before = hub.notifications();
        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            outside.path().join("a.bsl"),
        ));
        assert_eq!(hub.notifications(), before, "an out-of-scope path woke the sinks");

        // Positive control: an in-scope path must still wake them, or the assert
        // above would hold on a hub that never wakes anyone at all.
        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            dir.path().join("a.bsl"),
        ));
        assert!(hub.notifications() > before, "an in-scope path failed to wake the sinks");
    }

    /// The exclusion is fixed when the hub is created; scan roots are declared again on
    /// every re-arm. A topology reload can therefore name a root under the cache long
    /// after the boot-time refusal has had its say, and the hub must not answer that by
    /// going quietly blind to the root it was just told to follow.
    #[test]
    fn a_scan_root_declared_under_the_excluded_cache_wins_over_it() {
        let ws = tempdir().unwrap();
        let cache = ws.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let hub = WorkspaceChangeHub::start_targets_excluding(
            vec![WatchTarget::recursive(ws.path().to_path_buf())],
            vec![cache.clone()],
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        assert!(hub.rearm(
            vec![
                WatchTarget::recursive(ws.path().to_path_buf()),
                WatchTarget::recursive(cache.join("newext")),
            ],
            Duration::from_secs(5),
        ));

        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            cache.join("newext").join("M.bsl"),
        ));
        assert_eq!(
            hub.materialize(cursor).entries.len(),
            1,
            "a root declared under the cache was silently dropped"
        );

        // Positive control: the rest of the cache stays excluded, so the carve-out is a
        // hole in the hole and not a way of switching the exclusion off.
        hub.ingest_for_test(change_event(
            EventKind::Create(CreateKind::Any),
            cache.join("writer.lease"),
        ));
        assert_eq!(
            hub.materialize(cursor).entries.len(),
            1,
            "the carve-out disabled the exclusion instead of narrowing it"
        );
    }

    /// A rescan notice says the stream lapsed, not that its path changed — so the scope
    /// filter must not swallow the wake it owes. FSEvents attaches a path to the notice
    /// (commonly the workspace directory, outside every scan root in a nested layout),
    /// and an excluded cache root reaches the same branch. Skipping the wake there costs
    /// the sink its whole timeout with every change in that window unseen.
    #[test]
    fn a_rescan_notice_whose_path_is_filtered_still_wakes_the_sink() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join(".build");
        std::fs::create_dir_all(&cache).unwrap();
        let hub = WorkspaceChangeHub::start_targets_excluding(
            vec![WatchTarget::recursive(dir.path().to_path_buf())],
            vec![cache.clone()],
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();
        let generation = hub.wait_for_change(0, Duration::from_millis(1));
        let before = hub.notifications();

        hub.ingest_for_test(Ok(Event::new(EventKind::Other)
            .add_path(cache.join("writer.lease"))
            .set_flag(notify::event::Flag::Rescan)));

        assert!(hub.notifications() > before, "a rescan notice woke nobody");
        assert!(
            hub.wait_for_change(generation, Duration::from_millis(50)) > generation,
            "a rescan notice left the sink waiting for its own timeout"
        );
        assert!(hub.materialize(cursor).rescan_required, "the notice did not require a rescan");
    }

    /// A condition variable may wake without a signal, and every signal here is
    /// shared by every sink. Returning on the wake instead of on the predicate
    /// reports work that does not exist.
    #[test]
    fn wait_for_change_holds_until_the_generation_moves() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        let generation = hub.wait_for_change(0, Duration::from_millis(1));

        let waker = hub.clone();
        std::thread::spawn(move || {
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(5));
                waker.inner.notify();
            }
        });

        let started = Instant::now();
        let observed = hub.wait_for_change(generation, Duration::from_millis(250));
        assert_eq!(observed, generation, "a bare wake reported work that was not there");
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "the wait returned on a wake instead of on the deadline: {:?}",
            started.elapsed()
        );
    }

    /// Empirical check that a file created under a directory that did not exist
    /// when the watcher started is still observed. Bare `RecursiveMode::Recursive`
    /// races the OS watch arming; the hub closes that race by walking a
    /// freshly-created subtree on its create event.
    #[test]
    fn nested_directory_creation_is_observed() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        let nested = dir.path().join("deep").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("Module.bsl");
        std::fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let canonical = file.canonicalize().unwrap_or_else(|_| file.clone());
        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(50),
        );
        let mut seen = false;
        while patience.allows() {
            let batch = hub.drain(cursor);
            if batch.entries.iter().any(|e| e.canonical == canonical || e.raw == file) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(seen, "a file under a freshly-created subdirectory must be captured");
    }

    /// The hub watches EVERY root it is given (the config source root plus each extension
    /// root), so drift in a disjoint extension tree is event-delivered, not left to a scan.
    #[test]
    fn watches_all_roots() {
        let (_a, a) = resolved_tempdir();
        let (_b, b) = resolved_tempdir();
        let hub = WorkspaceChangeHub::start(vec![a.clone(), b.clone()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        std::thread::sleep(Duration::from_millis(100));
        // A change in the SECOND root must be observed.
        let file = b.join("Ext.bsl");
        std::fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(50),
        );
        let mut seen = false;
        while patience.allows() {
            let batch = hub.drain(cursor);
            if batch.entries.iter().any(|e| e.raw == file) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(seen, "a change in a secondary watch root must be captured");
    }

    /// In a nested layout the analyzer config sits ABOVE every scan root; the
    /// watch-target set must cover it as an individual file target, or a
    /// `dependsOn` edit would never be event-delivered to any consumer.
    #[test]
    fn watch_targets_cover_config_files_above_the_scan_roots() {
        let (_dir, root) = resolved_tempdir();
        let root = root.as_path();
        let source = root.join("src/cf");
        std::fs::create_dir_all(&source).unwrap();
        let toml = root.join("bsl-analyzer.toml");
        std::fs::write(&toml, "[source]\nroot = \"src/cf\"\n").unwrap();

        let hub = WorkspaceChangeHub::start_targets(watch_targets_for(root, &[source]));
        assert!(hub.wait_until_watching());
        let mut cursor = hub.subscribe();

        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(&toml, "[source]\nroot = \"src/cf\"\nextensions = []\n").unwrap();

        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(50),
        );
        let mut seen = false;
        while patience.allows() {
            let batch = hub.drain(cursor);
            cursor = batch.cursor;
            if batch.entries.iter().any(|e| e.raw == toml || e.canonical == toml) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(seen, "an edit to the config file above the scan roots must be delivered");
    }

    /// The workspace-root dir watch must deliver a config file that did NOT exist
    /// at arm time (absent -> create) and keep delivering across editor-style
    /// atomic saves (write temp + rename over), which replace the inode and would
    /// permanently kill a watch on the file itself.
    #[test]
    fn config_creation_and_atomic_replace_are_delivered_via_the_root_watch() {
        let (_dir, root) = resolved_tempdir();
        let root = root.as_path();
        let source = root.join("src/cf");
        std::fs::create_dir_all(&source).unwrap();
        // NO config file exists yet.
        let hub = WorkspaceChangeHub::start_targets(watch_targets_for(root, &[source]));
        assert!(hub.wait_until_watching());
        let mut cursor = hub.subscribe();
        std::thread::sleep(Duration::from_millis(100));

        let toml = root.join("bsl-analyzer.toml");
        let expect_delivery = |cursor: &mut SinkCursor, what: &str| {
            let mut patience = test_support::Patience::new(
                test_support::STATE_CEILING,
                std::time::Duration::from_millis(50),
            );
            loop {
                let batch = hub.drain(*cursor);
                *cursor = batch.cursor;
                if batch.entries.iter().any(|e| e.raw == toml || e.canonical == toml) {
                    break;
                }
                assert!(patience.allows(), "config {what} must be delivered");
                std::thread::sleep(Duration::from_millis(50));
            }
        };

        std::fs::write(&toml, "[source]\nroot = \"src/cf\"\n").unwrap();
        expect_delivery(&mut cursor, "creation");

        for round in 0..2 {
            let tmp = root.join(format!(".bsl-analyzer.toml.tmp{round}"));
            std::fs::write(&tmp, format!("[source]\nroot = \"src/cf\"\n# v{round}\n")).unwrap();
            std::fs::rename(&tmp, &toml).unwrap();
            expect_delivery(&mut cursor, "atomic replace");
        }
    }

    /// A re-arm extends coverage to the new root without a hub restart: the cursor
    /// survives (same id, one rescan flag), and a change in the NEWLY-added root is
    /// event-delivered afterwards.
    #[test]
    fn rearm_extends_coverage_and_flags_cursors_to_rescan() {
        let (_a, a) = resolved_tempdir();
        let (_b, b) = resolved_tempdir();
        let hub = WorkspaceChangeHub::start(vec![a.clone()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        assert!(
            hub.rearm(
                vec![WatchTarget::recursive(a.clone()), WatchTarget::recursive(b.clone())],
                Duration::from_secs(10)
            ),
            "the hub thread acknowledges the re-arm with full coverage"
        );
        let batch = hub.drain(cursor);
        assert!(batch.rescan_required, "a re-arm owes every cursor exactly one rescan");
        let cursor = batch.cursor;
        assert_eq!(hub.health(), Health::Healthy, "health recovers once cursors acknowledge");

        std::thread::sleep(Duration::from_millis(100));
        let file = b.join("Новый.bsl");
        std::fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut patience = test_support::Patience::new(
            test_support::STATE_CEILING,
            std::time::Duration::from_millis(50),
        );
        let mut seen = false;
        while patience.allows() {
            let batch = hub.drain(cursor);
            if batch.entries.iter().any(|e| e.raw == file) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(seen, "a change in the newly-armed root must be captured after the re-arm");
    }

    /// `ensure_roots` with the live set is free: no rescan round, no health blip —
    /// so calling it after EVERY rebuild is safe.
    #[test]
    fn ensure_roots_is_a_no_op_for_the_same_set() {
        let a = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![a.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(a.path().to_path_buf())]));
        let batch = hub.drain(cursor);
        assert!(!batch.rescan_required, "an unchanged root set must not force a rescan");
        assert_eq!(hub.health(), Health::Healthy);
    }

    /// A re-arm onto a set the watcher is ALREADY holding must not touch the watcher at
    /// all on FSEvents, where an arm is a whole-stream swap: the stream is stopped,
    /// rebuilt and restarted from "now", so everything that happens in the tree while
    /// that is in flight is lost. A re-arm is asked for after every rebuild, which would
    /// make that a scheduled blind window over an unchanged watch set.
    ///
    /// Counted through the refusal seam, which the hub consults on every arm and on
    /// nothing else — `notify` reports no registration count of its own. The seam refuses
    /// nothing here, so the count is the arms that were actually attempted.
    ///
    /// The control is the second half: a root that is NOT yet armed must still be armed by
    /// the same re-arm, or the rule would read as "re-arms do nothing".
    #[cfg(target_os = "macos")]
    #[test]
    fn a_re_arm_does_not_touch_a_watch_the_backend_already_holds() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Disjoint, not nested: a root under a recursive one is absorbed by the minimal
        // cover and never reaches the watcher, so it could not be the control.
        let elsewhere = tempdir().unwrap();
        let second = elsewhere.path().canonicalize().unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(root.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());
        assert_eq!(refusals.arms_of(&root), 1, "the initial arm is the one being counted");

        refusals.forget_asks();
        assert!(hub.rearm(vec![WatchTarget::recursive(root.clone())], Duration::from_secs(10)));
        assert_eq!(
            refusals.arms_of(&root),
            0,
            "a re-arm onto the live set rebuilt the stream over a watch already held",
        );

        refusals.forget_asks();
        assert!(hub.rearm(
            vec![WatchTarget::recursive(root.clone()), WatchTarget::recursive(second.clone())],
            Duration::from_secs(10),
        ));
        assert_eq!(refusals.arms_of(&second), 1, "a root not yet armed must still be armed");
        assert_eq!(refusals.arms_of(&root), 0, "and the one already held still must not be");
    }

    /// A re-arm that cannot watch one of the new roots reports partial coverage (the
    /// armable subset is covered) and degrades health so consumers scan, instead of
    /// silently pretending the missing subtree is watched.
    #[test]
    fn rearm_onto_a_missing_root_degrades_but_still_acks() {
        let a = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![a.path().to_path_buf()]);
        assert!(hub.wait_until_watching());
        let cursor = hub.subscribe();

        // Disjoint from `a`: a missing path nested under a watched recursive root
        // is legitimately covered (the recursive watch sees it once created).
        let elsewhere = tempdir().unwrap();
        let missing = elsewhere.path().join("нет-такого-каталога");
        assert!(
            !hub.rearm(
                vec![
                    WatchTarget::recursive(a.path().to_path_buf()),
                    WatchTarget::recursive(missing),
                ],
                Duration::from_secs(10)
            ),
            "a re-arm that leaves a root unarmed must NOT read as covered"
        );
        assert!(matches!(hub.health(), Health::Degraded(_)), "an unwatchable root degrades health");
        let batch = hub.drain(cursor);
        assert!(batch.rescan_required, "the armable subset still owes a rescan");
    }

    /// `shutdown` terminates and joins the hub thread; later control requests fail
    /// fast and cursors keep draining the frozen stream.
    #[test]
    fn shutdown_joins_the_thread_and_freezes_the_stream() {
        let a = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![a.path().to_path_buf()]);
        assert!(hub.wait_until_watching());

        hub.shutdown();
        assert!(
            !hub.rearm(
                vec![WatchTarget::recursive(a.path().to_path_buf())],
                Duration::from_millis(100)
            ),
            "a re-arm after shutdown must report failure, not hang"
        );
        let cursor = hub.subscribe();
        let batch = hub.drain(cursor);
        assert!(batch.entries.is_empty(), "the frozen stream still drains cleanly");
        hub.shutdown();
    }

    /// The thread belongs to the handles collectively: it stops when the LAST one goes, not
    /// the first. A sink is handed a clone and the starter's handle is dropped — the stream
    /// has to survive that, or a daemon that hands its hub to a sink and keeps no copy of
    /// its own would silently lose every event.
    #[test]
    fn a_hub_lives_while_any_handle_still_holds_it() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let hub = WorkspaceChangeHub::start(vec![root.clone()]);
        assert!(hub.wait_until_watching());
        let survivor = hub.clone();
        let cursor = survivor.subscribe();
        drop(hub);

        std::fs::write(root.join("Module.bsl"), "x").unwrap();
        assert!(
            eventually(|| {
                entry_names(&survivor.drain(cursor)).iter().any(|n| n.ends_with("Module.bsl"))
            }),
            "the surviving clone still holds the hub, so its stream is still live"
        );
    }

    /// Dropping a hub whose thread will never read the stop message has to RETURN. The
    /// message is read in the message loop, and a thread parked short of arming has not
    /// reached it, so a stop that waited for the thread unconditionally would wedge whoever
    /// dropped the hub — in a test binary not a failure but a hang, the one outcome no run
    /// can report. The hold is deliberately kept for the whole drop, so nothing releases
    /// the thread and only the stop's own bound can end the wait; and the drop happens on
    /// another thread so that a regression here is reported instead of hanging the binary.
    #[test]
    fn dropping_a_hub_whose_thread_cannot_answer_returns() {
        let dir = tempdir().unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);
        assert_eq!(
            hub.watch_readiness(Duration::from_millis(50)),
            WatchReadiness::NotYet,
            "the hub is alive and short of arming, which is the state this is about"
        );

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(hub);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(STOP_BUDGET * 3).is_ok(),
            "the drop of a hub nobody can join must end on its own budget"
        );
        // Only now: the thread is free to run out, and the temporary directory to go.
        drop(hold);
    }

    /// The guard on the hold is what keeps the case above from costing the whole budget:
    /// released as it goes, the parked thread arms, reaches its loop and reads the stop,
    /// so the drop joins a finished thread instead of waiting one out. Mutation: take the
    /// release out of the guard's `Drop` and this spends the budget it is asserting against.
    #[test]
    fn a_released_hold_lets_a_parked_hub_stop_at_once() {
        let dir = tempdir().unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);
        assert_eq!(hub.watch_readiness(Duration::ZERO), WatchReadiness::NotYet);

        let started = Instant::now();
        drop(hold);
        drop(hub);
        let waited = started.elapsed();
        if cfg!(timing_contract) {
            assert!(waited < STOP_BUDGET, "a released hub stops without its budget: {waited:?}");
        }
    }

    /// An explicit `shutdown` and the last handle's `Drop` reach the same stop, so the two
    /// in sequence must be as harmless as either alone. What this pins is that the second
    /// stop finds the thread already taken and treats that as nothing left to do — an
    /// implementation that assumed a handle would still be there would panic here.
    #[test]
    fn an_explicit_shutdown_before_the_last_drop_is_harmless() {
        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start(vec![dir.path().to_path_buf()]);
        assert!(hub.wait_until_watching());

        hub.shutdown();
        hub.shutdown();
        let started = Instant::now();
        drop(hub);
        let waited = started.elapsed();
        if cfg!(timing_contract) {
            assert!(
                waited < STOP_BUDGET,
                "a stop with nothing left to stop is immediate: {waited:?}"
            );
        }
    }

    /// On FSEvents an arm is a whole-stream swap: the running stream is stopped and a
    /// new one started from "now", and every change anywhere in the tree during the swap
    /// is dropped for good. A directory that appeared under an armed recursive root is
    /// already covered there, so arming it costs a blind window and buys nothing.
    ///
    /// The negative controls are the point: a directory OUTSIDE every armed root, and one
    /// under a NON-recursive target (which covers only its direct children), still have to
    /// be armed — a rule that refused those would go blind to whole subtrees instead.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_directory_already_inside_a_recursive_watch_is_not_re_armed() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let recursive = root.join("scan");
        let config_dir = root.join("conf");
        let outside = root.join("elsewhere");
        let armed = vec![
            ArmedTarget::arming(WatchTarget::recursive(recursive.clone()), ArmOrigin::Declared),
            ArmedTarget::arming(
                WatchTarget { path: config_dir.clone(), recursive: false },
                ArmOrigin::Declared,
            ),
        ];

        let needs_arming = |dir: &Path| {
            watch_is_additive_and_needed(an_armed_recursive_target_covers(&armed, dir))
        };

        assert!(
            !needs_arming(&recursive.join("CommonModules")),
            "a directory the recursive root already covers must not restart the stream",
        );
        assert!(
            needs_arming(&config_dir.join("sub")),
            "a non-recursive target covers only its direct children, so this one needs arming",
        );
        assert!(
            needs_arming(&outside.join("sub")),
            "a directory no armed root covers needs arming",
        );
    }

    /// A root declared through a symlink is armed under the DECLARED spelling while its
    /// resolved one is what the watch captured, and the two callers hold different ones:
    /// the blind set has only the declared spelling, an event arrives under whatever the
    /// backend reports. A path written either way lies in the same place, so coverage has
    /// to answer the same for both — reading one spelling alone would report a watched
    /// subtree as blind on one caller and restart the stream for nothing on the other.
    #[cfg(unix)]
    #[test]
    fn coverage_of_an_armed_root_is_read_in_both_spellings() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let real = root.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let armed =
            vec![ArmedTarget::arming(WatchTarget::recursive(link.clone()), ArmOrigin::Declared)];

        assert!(
            an_armed_recursive_target_covers(&armed, &link.join("sub")),
            "the declared spelling missed",
        );
        assert!(
            an_armed_recursive_target_covers(&armed, &real.join("sub")),
            "the resolved spelling missed",
        );
        // A control, so the two above cannot be held by a predicate that covers everything.
        assert!(
            !an_armed_recursive_target_covers(&armed, &root.join("elsewhere").join("sub")),
            "a directory outside the root was called covered",
        );
    }

    /// A recursive watch covers a file-system SUBTREE, and neither backend follows a link
    /// out of it. So a symlink created inside a watched root — a vendored dependency, a
    /// shared common-module tree, a mount point — is a door into another tree, and nothing
    /// behind it is watched however plainly the spelling reads as "inside". It needs a
    /// watch of its own, and a coverage test decided lexically would suppress exactly that
    /// one and leave the linked tree silent.
    ///
    /// Measured before it was written: with the root armed and the link not, a write in
    /// the link's target is delivered only after the link itself is armed.
    #[cfg(unix)]
    #[test]
    fn a_linked_subtree_is_not_covered_by_the_root_it_hangs_in() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempdir().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        let link = root.join("linked");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        std::fs::create_dir_all(root.join("plain")).unwrap();
        let armed =
            vec![ArmedTarget::arming(WatchTarget::recursive(root.clone()), ArmOrigin::Declared)];

        assert!(
            !an_armed_recursive_target_covers(&armed, &link),
            "a door into another tree was called covered, so its watch is never armed",
        );
        // The control that keeps the rule from reading as "nothing under a root is
        // covered": an ordinary directory created there is covered, which is the whole
        // point of not re-arming on FSEvents.
        assert!(
            an_armed_recursive_target_covers(&armed, &root.join("plain")),
            "an ordinary directory inside the root must stay covered",
        );
    }

    /// A hub over `first`, with a door out of the watched tree already armed, and a second
    /// declarable root beside it. The tree behind the door is what `arms_of` and the
    /// unwatch log both key on, so a stand can watch the door's whole life through it.
    #[cfg(unix)]
    fn hub_with_a_door() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        PathBuf,
        WorkspaceChangeHub,
        Arc<RefusedWatches>,
    ) {
        let (shared_dir, shared) = resolved_tempdir();
        std::fs::write(shared.join("Shared.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let (dir, root) = resolved_tempdir();
        let (first, second) = (root.join("first"), root.join("second"));
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(first.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());

        std::os::unix::fs::symlink(&shared, first.join("door")).unwrap();
        assert!(
            eventually(|| refusals.arms_of(&shared) >= 1),
            "the door out of the watched tree is armed",
        );
        assert!(hub.tick_now(), "the arm is in place");
        refusals.forget_asks();
        (shared_dir, dir, first, second, shared, hub, refusals)
    }

    #[cfg(unix)]
    fn unwatched(refusals: &RefusedWatches, path: &Path) -> bool {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        refusals
            .calls()
            .iter()
            .any(|(kind, called)| *kind == WatchCallKind::Disarm && *called == key)
    }

    /// One spelling keeps one record — but a declaration is never taken over by a door.
    /// A declared root that is itself a link can be described by an event while its
    /// recorded resolution is already stale, and the declared entries are the whole of what
    /// a declaration is compared against: a set that had quietly demoted one would report
    /// an armed root as unwatched, and publish it away from the roots it names.
    #[test]
    fn a_record_replaces_only_its_own_kind() {
        let (_dir, root) = resolved_tempdir();
        let door = root.join("door");
        let mut armed =
            vec![ArmedTarget::arming(WatchTarget::recursive(door.clone()), ArmOrigin::Declared)];

        record_arm(
            &mut armed,
            ArmedTarget::arming(WatchTarget::recursive(door.clone()), ArmOrigin::Incidental),
        );
        assert!(
            armed.iter().any(|entry| entry.is_declared()),
            "a door took the place of the declaration that named the same path",
        );

        record_arm(
            &mut armed,
            ArmedTarget::arming(WatchTarget::recursive(door), ArmOrigin::Incidental),
        );
        assert_eq!(armed.len(), 2, "one spelling kept more than one record of a kind");
    }

    /// A watch armed because an event revealed a door out of the watched tree is a
    /// registration the declaration does not name, and the set holds it for one reason: so
    /// that a declaration which stops reaching it can take it away. Nothing else can — a
    /// re-arm builds only from what is declared — so a registration left out of the set
    /// outlives every topology that could justify it, for the life of the daemon.
    #[cfg(unix)]
    #[test]
    fn a_watch_armed_by_an_event_goes_when_the_declaration_stops_reaching_it() {
        let (_shared_dir, _dir, _first, second, shared, hub, refusals) = hub_with_a_door();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(second)]));

        assert!(
            unwatched(&refusals, &shared),
            "the declaration stopped leading to the door and nothing dropped its watch: {:?}",
            refusals.calls(),
        );
    }

    /// A scope can narrow without the watch cover moving at all: a root declared inside an
    /// excluded subtree is absorbed by its recursive ancestor and never reaches the cover,
    /// while it does carve a hole back out of the exclusion. Dropping such a root leaves the
    /// cover identical and the scope smaller — and a door inside what the scope has stopped
    /// walking is a registration nothing else will ever hand to `unwatch`.
    #[cfg(unix)]
    #[test]
    fn a_door_the_scope_stopped_walking_goes_even_when_the_cover_did_not_move() {
        let (_shared_dir, shared) = resolved_tempdir();
        std::fs::write(shared.join("Shared.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let (_dir, workspace) = resolved_tempdir();
        let cache = workspace.join("cache");
        let extension = cache.join("ext");
        std::fs::create_dir_all(&extension).unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_seamed(
            vec![
                WatchTarget::recursive(workspace.clone()),
                WatchTarget::recursive(extension.clone()),
            ],
            DEFAULT_CAPACITY,
            Duration::from_secs(3600),
            false,
            None,
            Some(refusals.as_refusal()),
            vec![cache.clone()],
            PollConfig::PRODUCTION,
            BlindPollSeam::default(),
        );
        assert!(hub.wait_until_watching());

        std::os::unix::fs::symlink(&shared, extension.join("door")).unwrap();
        assert!(
            eventually(|| refusals.arms_of(&shared) >= 1),
            "a door inside the carved-out root is armed",
        );
        assert!(hub.tick_now(), "the arm is in place");
        refusals.forget_asks();

        let cursor = hub.drain(hub.subscribe()).cursor;
        assert!(hub.ensure_roots(&[WatchTarget::recursive(workspace)]));
        assert!(hub.tick_now(), "the declaration is applied");

        assert!(
            unwatched(&refusals, &shared),
            "the scope stopped walking the door and its watch was left standing: {:?}",
            refusals.calls(),
        );
        assert!(
            hub.materialize(cursor).rescan_required,
            "a registration was taken away, and on a backend that strips descendants with \
             it whatever lay beneath was taken and put back — a window nobody was told of",
        );
    }

    /// The point stream and the walk have to describe ONE file universe. A link named one
    /// thing onto a file named another resolves to a key the scan of the TARGET's root does
    /// list, and no walk of this root ever produces it — so delivering it reports a file
    /// from outside the workspace as drift inside it.
    #[cfg(unix)]
    #[test]
    fn a_point_event_on_a_role_mismatched_link_carries_nothing() {
        let (_dir, base) = resolved_tempdir();
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("Target.bsl");
        std::fs::write(&target, "Процедура П() КонецПроцедуры").unwrap();
        std::os::unix::fs::symlink(&target, base.join("Alias.txt")).unwrap();
        std::os::unix::fs::symlink(&target, base.join("Agreed.bsl")).unwrap();

        assert!(
            classify_path(&base.join("Alias.txt")).is_none(),
            "a link whose name and target disagree on role was taken into drift",
        );
        // The control, so this cannot read as "links are never taken".
        assert_eq!(
            classify_path(&base.join("Agreed.bsl")),
            Some((target, ChangeKind::MaybeChanged)),
            "a link both spellings agree about must be delivered, resolved",
        );
    }

    /// Every raise is its own window. Two in a row carry the same reason, so a shared
    /// counter left still would let a batch taken against the first still look current —
    /// and a sink already waiting on it would sleep out its whole timeout over a loss it
    /// has just been handed.
    #[test]
    fn a_second_window_moves_the_generation_a_sink_waits_on() {
        let mut acc = Accumulator::new(8);
        let id = acc.subscribe(None);

        acc.enter_rescan(false, DegradeReason::Rearmed);
        let taken = acc.materialize(id);
        let before = acc.generation;

        assert!(acc.enter_rescan_for_listeners(DegradeReason::Rearmed), "a second window opens");
        assert!(acc.generation > before, "a sink waiting on the generation was not woken");

        acc.acknowledge(&taken);
        assert!(
            acc.materialize(id).rescan_required,
            "the second window was cleared by an acknowledgement taken against the first",
        );
    }

    /// A declared root re-named to an equivalent spelling — another link to the same
    /// directory — leaves every door under it physically where it was. Judged on the
    /// dropped spelling alone a door goes with the alias, and nothing will name it again: a
    /// re-arm builds only from what is declared, and a link that merely stands there fires
    /// no event.
    ///
    /// Asked of the predicate rather than through a hub, because the spelling a door is
    /// recorded under is the backend's: FSEvents reports physical paths, so a door there is
    /// already recorded as `real/door` and the question never arises; inotify reports under
    /// the path it was given, which is where it does.
    #[cfg(unix)]
    #[test]
    fn a_door_is_in_scope_by_where_it_lies_not_only_by_how_it_is_spelled() {
        let (_shared_dir, shared) = resolved_tempdir();
        let (_dir, base) = resolved_tempdir();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let (first, second) = (base.join("first"), base.join("second"));
        std::os::unix::fs::symlink(&real, &first).unwrap();
        std::os::unix::fs::symlink(&real, &second).unwrap();
        std::os::unix::fs::symlink(&shared, real.join("door")).unwrap();

        let door =
            ArmedTarget::arming(WatchTarget::recursive(first.join("door")), ArmOrigin::Incidental);
        let scope =
            Scope::from_targets_for_test(&ResolvedTargets::here(vec![WatchTarget::recursive(
                second.clone(),
            )]));

        assert!(
            the_scope_still_reaches(&scope, &door),
            "the root was re-declared by an equivalent spelling and the door under it stops \
             counting as reached, though it stands exactly where it did",
        );
        // The control: a door under a root the declaration really dropped is not reached.
        let elsewhere =
            Scope::from_targets_for_test(&ResolvedTargets::here(vec![WatchTarget::recursive(
                base.join("nowhere"),
            )]));
        assert!(
            !the_scope_still_reaches(&elsewhere, &door),
            "a door no declared root leads to must not count as reached",
        );
    }

    /// A record carried onto the declaration's spelling still names a registration standing
    /// under the one it came from, and anything in the same pass may unwatch that one. On a
    /// backend without an unconditional defensive pass, only being marked as unwatched gets
    /// it armed under the name it now claims — otherwise the set names a tree nothing
    /// watches, and says so to every later declaration.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_record_carried_to_another_spelling_is_armed_under_the_one_it_now_names() {
        let (_dir, base) = resolved_tempdir();
        let (real_one, real_two) = (base.join("real1"), base.join("real2"));
        std::fs::create_dir(&real_one).unwrap();
        std::fs::create_dir(&real_two).unwrap();
        let root = base.join("root");
        std::os::unix::fs::symlink(&real_one, &root).unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(root.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());

        // The root moves to another tree while a second spelling takes over the first.
        std::fs::remove_file(&root).unwrap();
        std::os::unix::fs::symlink(&real_two, &root).unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&real_one, &alias).unwrap();
        refusals.forget_asks();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(root), WatchTarget::recursive(alias),]));
        assert!(
            refusals.arms_of(&real_one) >= 1,
            "the tree the surviving record names was left to nobody: {:?}",
            refusals.calls(),
        );
    }

    /// A record carried onto another spelling of the same directory leaves a registration
    /// standing under the one it came from. The backend is keyed by the path it was given,
    /// so that one has to go: left in place it holds the dropped alias for the life of the
    /// process, and a run of alias swaps piles up one registration per swap — and after the
    /// record has moved, nothing names the old one any more.
    #[cfg(unix)]
    #[test]
    fn an_alias_the_declaration_dropped_does_not_keep_its_registration() {
        let (_dir, base) = resolved_tempdir();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let (first, second) = (base.join("first"), base.join("second"));
        std::os::unix::fs::symlink(&real, &first).unwrap();
        std::os::unix::fs::symlink(&real, &second).unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(first)],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());
        refusals.forget_asks();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(second)]));
        assert!(hub.tick_now(), "the declaration is applied");

        let calls = refusals.calls();
        assert!(
            calls.iter().any(|(kind, _)| *kind == WatchCallKind::Disarm),
            "the alias the declaration dropped kept its registration: {calls:?}",
        );
    }

    /// A door whose target steps aside for a moment — a rebuild renaming a directory and
    /// putting it back — is not a door that has gone. Dropping its record there destroys the
    /// only thing able to re-point it: the link itself never changed, so no event will ever
    /// describe it again, and no declaration names it.
    #[cfg(unix)]
    #[test]
    fn a_door_whose_target_stepped_aside_survives_a_re_arm() {
        let (_shared_dir, shared) = resolved_tempdir();
        let slot = shared.join("current");
        std::fs::create_dir(&slot).unwrap();
        let (_dir, root) = resolved_tempdir();
        let (first, second) = (root.join("first"), root.join("second"));
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(first.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());

        std::os::unix::fs::symlink(&slot, first.join("door")).unwrap();
        assert!(eventually(|| refusals.arms_of(&slot) >= 1), "the door is armed",);
        assert!(hub.tick_now(), "the arm is in place");

        // A declaration arrives while the target is away, and the target comes back.
        std::fs::rename(&slot, shared.join("previous")).unwrap();
        assert!(hub.ensure_roots(&[
            WatchTarget::recursive(first),
            WatchTarget::recursive(second.clone()),
        ]));
        std::fs::rename(shared.join("previous"), &slot).unwrap();
        refusals.forget_asks();

        // The record has to have survived, and the one observable of that is the only thing
        // a record is for: a declaration that stops reaching the door can still drop it.
        assert!(hub.ensure_roots(&[WatchTarget::recursive(second)]));
        assert!(
            unwatched(&refusals, &slot),
            "the record was thrown away while the target was away, so nothing was left able \
             to name the registration afterwards: {:?}",
            refusals.calls(),
        );
    }

    /// A cursor that arrives while blindness is being published is not a clean one. The
    /// standing blind set and the cursors live under two locks, taken in that order so
    /// neither path holds one while asking for the other, and a subscription can land in the
    /// gap — after the hub thread flagged everybody it could see, before this cursor
    /// existed. It is flagged after the fact, and alone: whoever was there already has the
    /// window.
    #[test]
    fn a_cursor_that_arrived_in_the_gap_is_flagged_alone() {
        let mut acc = Accumulator::new(8);
        let early = acc.subscribe(None);
        let late = acc.subscribe(None);

        acc.force_rescan(late, DegradeReason::RewatchFailed);

        assert!(acc.materialize(late).rescan_required, "the cursor that arrived in the gap");
        assert!(
            !acc.materialize(early).rescan_required,
            "nobody else is owed anything: the window, if there was one, found them",
        );
    }

    /// That nobody is here to be owed a new window says nothing about a reason recorded
    /// earlier. `health` reports the hub's condition to a status caller, and an unrelated
    /// operation that owes nothing must not answer for it.
    #[test]
    fn a_window_nobody_can_be_owed_does_not_erase_the_reason_before_it() {
        let mut acc = Accumulator::new(8);
        acc.enter_rescan(false, DegradeReason::RuntimeError);

        assert!(
            !acc.enter_rescan_for_listeners(DegradeReason::Rearmed),
            "with no cursor there is nobody to owe a window to",
        );
        assert_eq!(
            acc.health(),
            Health::Degraded(DegradeReason::RuntimeError),
            "the reason recorded before it was erased by an operation that owed nothing",
        );
    }

    /// A DECLARED target names an object too. A directory removed and recreated under one
    /// absolute path resolves identically, so a re-arm matching on the path alone carries
    /// the old record forward and leaves the backend watching what is gone — on the very
    /// pass the periodic check reached because it had already seen the object change.
    #[cfg(unix)]
    #[test]
    fn a_declared_directory_recreated_under_one_name_is_re_armed() {
        let (_dir, base) = resolved_tempdir();
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(root.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());
        refusals.forget_asks();

        std::fs::rename(&root, base.join("previous")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(hub.tick_now(), "the periodic check reads the root");

        let calls = refusals.calls();
        assert!(
            calls.iter().any(|(kind, _)| *kind == WatchCallKind::Disarm)
                && calls.iter().any(|(kind, _)| *kind == WatchCallKind::Arm),
            "the watch stayed on the directory that is gone: {calls:?}",
        );
    }

    /// A check that reads one obstacle must not answer for another. The declared set being
    /// fully watched says nothing about a stream that was lost — and with no consumer there
    /// to acknowledge anything, the two would otherwise be closed together, erasing a loss
    /// nobody has read from the report that is the only place it appears.
    #[test]
    fn an_empty_blind_set_does_not_close_a_reason_it_never_read() {
        let (_dir, root) = resolved_tempdir();
        let hub = WorkspaceChangeHub::start_targets_with_period(
            vec![WatchTarget::recursive(root)],
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching());

        hub.ingest_for_test(Err(notify::Error::generic("the stream is gone")));
        assert_eq!(hub.health(), Health::Degraded(DegradeReason::RuntimeError));

        assert!(hub.tick_now(), "the periodic check runs");
        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::RuntimeError),
            "a check that never read the lost stream closed the report of it",
        );
    }

    /// A `NotFound` on a nested spelling proves only that something on the way is missing.
    /// An ancestor whose target steps aside for a moment answers exactly that, and forgetting
    /// the record there destroys the only thing able to re-point the door once the way back
    /// opens — nothing else names it, and the link itself never changed.
    #[cfg(unix)]
    #[test]
    fn a_door_is_not_forgotten_because_the_way_to_it_was_briefly_shut() {
        let (_outer_dir, outer_target) = resolved_tempdir();
        let (_dir, root) = resolved_tempdir();
        let slot = root.join("slot");
        std::fs::rename(&outer_target, &slot).unwrap();
        let (_inner_dir, inner_target) = resolved_tempdir();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(root.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());

        std::os::unix::fs::symlink(&slot, root.join("outer")).unwrap();
        std::os::unix::fs::symlink(&inner_target, slot.join("inner")).unwrap();
        assert!(
            eventually(|| refusals.arms_of(&inner_target) >= 1),
            "the door behind the outer link is armed",
        );
        assert!(hub.tick_now(), "the arm is in place");

        // The way to it is shut for a moment and opened again. Nothing about the door itself
        // changed, so nothing will ever describe it again.
        let aside = root.join("aside");
        std::fs::rename(&slot, &aside).unwrap();
        assert!(hub.tick_now(), "a check lands while the way is shut");
        std::fs::rename(&aside, &slot).unwrap();
        refusals.forget_asks();

        // Only a record can notice this: the door is re-pointed with no event to say so.
        let (_moved_dir, moved_to) = resolved_tempdir();
        std::fs::remove_file(slot.join("inner")).unwrap();
        std::os::unix::fs::symlink(&moved_to, slot.join("inner")).unwrap();
        assert!(hub.tick_now(), "the check reads the doors again");
        assert!(
            refusals.arms_of(&moved_to) >= 1,
            "the record was thrown away while the way to it was shut: {:?}",
            refusals.calls(),
        );
    }

    /// `RewatchFailed` has two producers, and the blind set is only one of them. A watch this
    /// module could not extend over a subtree an event revealed is a different obstacle, and
    /// a check that never read it must not close the only report of it.
    #[cfg(unix)]
    #[test]
    fn a_failed_arm_over_a_revealed_subtree_is_not_closed_by_the_blind_check() {
        let (_shared_dir, shared) = resolved_tempdir();
        let (_dir, root) = resolved_tempdir();

        let refusals = RefusedWatches::refusing(vec![shared.clone()]);
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(root.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());

        std::os::unix::fs::symlink(&shared, root.join("door")).unwrap();
        assert!(
            eventually(|| { hub.health() == Health::Degraded(DegradeReason::RewatchFailed) }),
            "the arm over the revealed subtree failed and was reported",
        );

        assert!(hub.tick_now(), "the periodic check runs");
        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::RewatchFailed),
            "a check that read only the declared set closed the report of a subtree it \
             never looked at",
        );
    }

    /// A door that leads nowhere a watch can be placed — a dangling link, or one now
    /// pointing at a file — still holds the registration it was armed with, and that
    /// registration goes on delivering for a tree the door has stopped reaching. Every one
    /// of those events arrives spelled as though it were still inside the workspace.
    #[cfg(unix)]
    #[test]
    fn a_door_that_leads_nowhere_loses_the_registration_it_held() {
        let (_shared_dir, _dir, first, _second, shared, hub, refusals) = hub_with_a_door();
        let door = first.join("door");

        std::fs::remove_file(&door).unwrap();
        std::os::unix::fs::symlink(shared.join("never-was"), &door).unwrap();
        assert!(hub.tick_now(), "the periodic check reads the doors");

        // Named by the DOOR: the link no longer resolves, so that is the only key the
        // watcher was given and the only one it is logged under.
        assert!(
            unwatched(&refusals, &door),
            "the door leads nowhere and the watch it held on the old tree was left \
             standing: {:?}",
            refusals.calls(),
        );
    }

    /// A record names an OBJECT, not only a spelling. A directory removed and recreated
    /// under one name leaves the registration on the one that is gone while the door still
    /// reads as leading exactly where it did — and nothing else in the module would ever
    /// notice, because the link itself never changed and fires no event.
    #[cfg(unix)]
    #[test]
    fn a_door_whose_target_was_recreated_under_one_name_is_re_pointed() {
        let (_shared_dir, shared) = resolved_tempdir();
        let slot = shared.join("current");
        std::fs::create_dir(&slot).unwrap();
        let (_dir, root) = resolved_tempdir();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(root.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());

        std::os::unix::fs::symlink(&slot, root.join("door")).unwrap();
        assert!(
            eventually(|| refusals.arms_of(&slot) >= 1),
            "the door is armed on the directory the slot holds now",
        );
        assert!(hub.tick_now(), "the arm is in place");

        // Same name, different directory — and the link is untouched, so nothing says so.
        std::fs::rename(&slot, shared.join("previous")).unwrap();
        std::fs::create_dir(&slot).unwrap();
        refusals.forget_asks();

        assert!(hub.tick_now(), "the periodic check reads the door");
        assert!(
            refusals.arms_of(&slot) >= 1,
            "the registration stayed on the directory that is gone: {:?}",
            refusals.calls(),
        );
    }

    /// A window raised before anyone subscribed is not INHERITED by whoever comes next: a
    /// cursor takes its debt from the blind set alone, never from the transient reason. The
    /// reason itself stands, because `health` reports the hub's condition to a status
    /// caller whether or not a consumer exists to be owed anything.
    #[test]
    fn a_window_raised_before_the_first_cursor_is_not_handed_to_it() {
        let mut acc = Accumulator::new(8);
        acc.enter_rescan(false, DegradeReason::RuntimeError);
        let id = acc.subscribe(None);
        assert!(
            !acc.materialize(id).rescan_required,
            "a consumer that arrived after the window was handed the window",
        );
    }

    /// A window nobody can be owed is a window that is over. With no cursor to acknowledge
    /// it, a reconcile raised before a full recovery would outlive that recovery and be
    /// handed to the first consumer to subscribe — for a window that ended before it
    /// arrived.
    #[cfg(unix)]
    #[test]
    fn a_recovery_with_nobody_listening_leaves_no_window_behind() {
        let (_dir, _a, b, hub, refusals) = partly_blind_hub();
        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::RewatchFailed),
            "the refused root is the obstacle the stand starts from",
        );

        refusals.allow(&b);
        assert!(hub.tick_now(), "the retry arms it");

        assert_eq!(hub.health(), Health::Healthy, "the obstacle is gone and so is the window");
        let newcomer = hub.subscribe();
        assert!(
            !hub.materialize(newcomer).rescan_required,
            "a consumer that arrived after the recovery inherited the window it closed",
        );
    }

    /// A door is reached only through its own spelling, so once the link is gone nothing
    /// will ever deliver an event under it again — and the registration it left behind is
    /// one only the periodic check can still name. Waiting for a declaration to arrive
    /// leaves it standing indefinitely, and every door created and removed since adds
    /// another.
    #[cfg(unix)]
    #[test]
    fn a_door_removed_from_disk_is_dropped_by_the_periodic_check() {
        let (_shared_dir, _dir, first, _second, _shared, hub, refusals) = hub_with_a_door();
        let door = first.join("door");

        std::fs::remove_file(&door).unwrap();
        assert!(hub.tick_now(), "the periodic check runs");

        // Named by the DOOR, not by the tree it led to: the spelling no longer resolves, so
        // that is the only key the watcher was given and the only one it is logged under.
        assert!(
            unwatched(&refusals, &door),
            "the door is gone from disk and its watch was left standing, with nothing able \
             to name it again: {:?}",
            refusals.calls(),
        );
    }

    /// A declared root that is itself a link is re-pointed the same way a door is, and an
    /// event describes it long before the periodic check re-reads its fingerprint. The
    /// event branch leaves it alone all the same: re-pointing a declared target is the
    /// check's work and it does the whole of it — the old registration dropped before the
    /// new one is placed, the record corrected, the defensive pass, the debt — where doing
    /// half of it from an event leaves a door's record beside a declared one that still
    /// names the tree it used to reach, and the check then pays for the swap a second time.
    /// What waiting costs is bounded by the period, which is the bound this module already
    /// accepts for a root re-pointed in place.
    #[cfg(unix)]
    #[test]
    fn a_declared_link_re_pointed_at_runtime_is_left_to_the_periodic_check() {
        let (_old_dir, old) = resolved_tempdir();
        let (_new_dir, new) = resolved_tempdir();
        std::fs::write(new.join("Moved.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let (_dir, workspace) = resolved_tempdir();
        let extension = workspace.join("ext");
        std::os::unix::fs::symlink(&old, &extension).unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![
                WatchTarget::recursive(workspace.clone()),
                WatchTarget::recursive(extension.clone()),
            ],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());
        let mut cursor = hub.subscribe();
        refusals.forget_asks();

        std::fs::remove_file(&extension).unwrap();
        std::os::unix::fs::symlink(&new, &extension).unwrap();
        assert!(
            eventually(|| {
                let batch = hub.drain(cursor);
                cursor = batch.cursor;
                batch.entries.iter().any(|entry| entry.raw.to_string_lossy().contains("Moved"))
            }),
            "the hub saw the re-pointed root",
        );
        assert!(hub.tick_now(), "and handled the message carrying it");

        // ONE unwatch over the whole window: the event left the spelling alone and the
        // check re-pointed it once. Two is the event branch having done half the work and
        // the check having paid for the swap all over again.
        //
        // Counted on the unwatches and not on the arms, because how many arms a re-point
        // costs is platform-dependent by design here: off macOS the defensive pass arms
        // every kept target again after the unwatches, so a two-target declaration is three
        // arms for one re-point. The unwatch count is one on every backend.
        let calls = refusals.calls();
        assert_eq!(
            calls.iter().filter(|(kind, _)| *kind == WatchCallKind::Disarm).count(),
            1,
            "one re-point is one unwatch: {calls:?}",
        );
        let first_arm = calls.iter().position(|(kind, _)| *kind == WatchCallKind::Arm);
        let first_drop = calls.iter().position(|(kind, _)| *kind == WatchCallKind::Disarm);
        assert!(
            matches!((first_drop, first_arm), (Some(drop), Some(arm)) if drop < arm),
            "the old registration was left standing under the same spelling: {calls:?}",
        );
    }

    /// Whether the tree a door now leads to happens to be watched already says nothing about
    /// the registration the door used to hold. Deciding the arm first and the replacement
    /// second lets a door re-pointed INTO a covered tree keep its old registration on the
    /// outside one, which no event and no pass would ever name again.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_door_re_pointed_into_a_covered_tree_still_drops_what_it_held() {
        let (_shared_dir, _dir, first, _second, _shared, hub, refusals) = hub_with_a_door();
        let inside = first.join("inside");
        std::fs::create_dir(&inside).unwrap();
        let door = first.join("door");
        let cursor = hub.drain(hub.subscribe()).cursor;
        assert!(!hub.materialize(cursor).rescan_required, "the stand starts level");

        std::fs::remove_file(&door).unwrap();
        std::os::unix::fs::symlink(&inside, &door).unwrap();
        // No tick as a barrier: the periodic check re-points a moved door itself, and its
        // own debt would stand in for the one this stand is about.
        assert!(
            eventually(|| unwatched(&refusals, &inside)),
            "the door now leads inside the watched tree, and the watch it held on the tree \
             outside was left standing: {:?}",
            refusals.calls(),
        );
        // And the window that unwatch cost is owed. Whether a NEW watch is worth placing is
        // a later question — here the answer is no, because the tree is already covered —
        // and the debt must not ride on it.
        assert!(
            eventually(|| hub.materialize(cursor).rescan_required),
            "the unwatch restarted the stream and nobody was told",
        );
    }

    /// A replacement whose new arm fails has already dropped the old registration, and the
    /// record is what is left to try again from. Nothing else can: no declaration names a
    /// door, and a link that merely stands there fires no event — so a record thrown away on
    /// a transient refusal costs the subtree behind the door for the life of the daemon.
    /// Kept, it is stale by design, and staleness is exactly what the periodic check reads
    /// to know there is something to re-point.
    #[cfg(unix)]
    #[test]
    fn a_door_whose_replacement_failed_is_tried_again() {
        let (_shared_dir, _dir, first, _second, _shared, hub, refusals) = hub_with_a_door();
        let (_new_dir, moved_to) = resolved_tempdir();
        std::fs::write(moved_to.join("Moved.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let door = first.join("door");

        refusals.refuse(&moved_to);
        std::fs::remove_file(&door).unwrap();
        std::os::unix::fs::symlink(&moved_to, &door).unwrap();
        assert!(
            eventually(|| refusals.arms_of(&moved_to) >= 1),
            "the hub tried to arm the door where it leads now, and was refused",
        );
        assert!(hub.tick_now(), "the attempt is over");

        refusals.allow(&moved_to);
        refusals.forget_asks();
        assert!(hub.tick_now(), "the periodic check tries again");

        assert!(
            refusals.arms_of(&moved_to) >= 1,
            "the obstacle cleared and nothing tried the door again: {:?}",
            refusals.calls(),
        );
    }

    /// A DECLARED spelling re-pointed at another directory is a retarget, not an addition,
    /// and the old registration has to go before the new one is taken. The kernel keys a
    /// watch by inode, so the same path over a new target takes a new descriptor while
    /// `notify` keys its map by path and forgets the old one — after which nothing can name
    /// it. Arming additions before removals is for a subtree that is in BOTH sets; the tree
    /// a retarget left is in neither.
    #[cfg(unix)]
    #[test]
    fn a_declared_retarget_drops_the_registration_it_replaces() {
        let (_dir, base) = resolved_tempdir();
        let (old, new) = (base.join("old"), base.join("new"));
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&new).unwrap();
        let root = base.join("root");
        std::os::unix::fs::symlink(&old, &root).unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_targets_refusing(
            vec![WatchTarget::recursive(root.clone())],
            Duration::from_secs(3600),
            &refusals,
        );
        assert!(hub.wait_until_watching());
        refusals.forget_asks();

        std::fs::remove_file(&root).unwrap();
        std::os::unix::fs::symlink(&new, &root).unwrap();
        assert!(hub.tick_now(), "the periodic check sees the retarget");

        let calls = refusals.calls();
        let first_arm = calls.iter().position(|(kind, _)| *kind == WatchCallKind::Arm);
        let first_drop = calls.iter().position(|(kind, _)| *kind == WatchCallKind::Disarm);
        assert!(
            matches!((first_drop, first_arm), (Some(drop), Some(arm)) if drop < arm),
            "the root was armed on its new target before the watch on the old one was \
             dropped, so nothing can name that one again: {calls:?}",
        );
        assert_eq!(
            calls.iter().filter(|(kind, _)| *kind == WatchCallKind::Disarm).count(),
            1,
            "one retarget is one unwatch: a second, after the new registration is in place, \
             takes away exactly what the first made room for: {calls:?}",
        );
    }

    /// The restore that follows a dropped door answers a failure the way the same pass does
    /// in a re-arm: by DROPPING the record. A record is read as "already covered", so one
    /// left over a watch that may be gone keeps the blind set silent and the retry away for
    /// ever — the hub would call itself healthy over a declared root nothing watches.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_root_lost_while_restoring_it_is_reported_and_retried() {
        let (_shared_dir, shared) = resolved_tempdir();
        std::fs::write(shared.join("Shared.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let (_dir, workspace) = resolved_tempdir();
        let cache = workspace.join("cache");
        let extension = cache.join("ext");
        std::fs::create_dir_all(&extension).unwrap();

        let refusals = RefusedWatches::none();
        let hub = WorkspaceChangeHub::start_seamed(
            vec![
                WatchTarget::recursive(workspace.clone()),
                WatchTarget::recursive(extension.clone()),
            ],
            DEFAULT_CAPACITY,
            Duration::from_secs(3600),
            false,
            None,
            Some(refusals.as_refusal()),
            vec![cache.clone()],
            PollConfig::PRODUCTION,
            BlindPollSeam::default(),
        );
        assert!(hub.wait_until_watching());

        std::os::unix::fs::symlink(&shared, extension.join("door")).unwrap();
        assert!(
            eventually(|| refusals.arms_of(&shared) >= 1),
            "a door inside the carved-out root is armed",
        );
        assert!(hub.tick_now(), "the arm is in place");

        // The restore of the workspace root fails, so its record must not survive — and the
        // declaration that caused it is answered "not covered", not acknowledged.
        refusals.refuse(&workspace);
        assert!(
            !hub.ensure_roots(&[WatchTarget::recursive(workspace.clone())]),
            "a declaration whose root was lost while restoring it is not covered",
        );
        assert!(hub.tick_now(), "the declaration is applied");
        assert_eq!(
            hub.health(),
            Health::Degraded(DegradeReason::RewatchFailed),
            "a root nothing watches is not health",
        );

        refusals.allow(&workspace);
        refusals.forget_asks();
        assert!(hub.tick_now(), "the periodic check retries it");
        assert!(
            refusals.arms_of(&workspace) >= 1,
            "the root was never tried again: {:?}",
            refusals.calls(),
        );
    }

    /// A door whose link is re-pointed is a registration on a tree its spelling no longer
    /// reaches, and arming again does not replace it: inotify keys a watch by inode, so the
    /// same path over a new target takes a new descriptor while `notify` keys its own map
    /// by path and forgets the old one — unremovable by anything afterwards.
    #[cfg(unix)]
    #[test]
    fn a_retargeted_door_drops_the_registration_it_replaces() {
        let (_shared_dir, _dir, first, _second, _old, hub, refusals) = hub_with_a_door();
        let (_new_dir, moved_to) = resolved_tempdir();
        std::fs::write(moved_to.join("Moved.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let door = first.join("door");

        std::fs::remove_file(&door).unwrap();
        std::os::unix::fs::symlink(&moved_to, &door).unwrap();
        assert!(
            eventually(|| refusals.arms_of(&moved_to) >= 1),
            "the retargeted door is armed where it leads now",
        );
        assert!(hub.tick_now(), "the arm is in place");

        // Asserted on the ORDER, because the seam keys a call by where the path resolved
        // when it was made: by the time the old registration is dropped the link already
        // points elsewhere, so both ends of the replacement are logged under the new tree.
        // What distinguishes a replacement from a second registration is that the drop
        // comes first.
        let calls = refusals.calls();
        let first_arm = calls.iter().position(|(kind, _)| *kind == WatchCallKind::Arm);
        let first_drop = calls.iter().position(|(kind, _)| *kind == WatchCallKind::Disarm);
        assert!(
            matches!((first_drop, first_arm), (Some(drop), Some(arm)) if drop < arm),
            "the registration on the tree the door used to reach was left standing, and \
             nothing can name it again: {calls:?}",
        );
    }

    /// A door re-armed after it was retargeted has to leave a record of where it leads NOW.
    /// The only reader of that half is the re-arm's own decision about what to drop, so a
    /// record left on the old resolution inverts exactly the decision it exists for: the
    /// watch the declaration still reaches is dropped, and nothing can arm it again.
    #[cfg(unix)]
    #[test]
    fn a_door_re_armed_after_a_retarget_records_where_it_leads_now() {
        let (_shared_dir, _dir, first, second, _old, hub, refusals) = hub_with_a_door();
        let (_new_dir, moved_to) = resolved_tempdir();
        std::fs::write(moved_to.join("Moved.bsl"), "Процедура П() КонецПроцедуры").unwrap();
        let door = first.join("door");

        std::fs::remove_file(&door).unwrap();
        std::os::unix::fs::symlink(&moved_to, &door).unwrap();
        assert!(
            eventually(|| refusals.arms_of(&moved_to) >= 1),
            "the retargeted door is armed where it leads now",
        );
        assert!(hub.tick_now(), "the arm is in place");
        refusals.forget_asks();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(first), WatchTarget::recursive(second),]));
        assert!(
            !unwatched(&refusals, &moved_to),
            "the declaration still leads to the door and the door still leads to the tree, \
             yet the watch was dropped: {:?}",
            refusals.calls(),
        );
    }

    /// A watch re-pointed before any consumer exists has taken nothing from anyone. A debt
    /// raised over an empty cursor set is one nobody can acknowledge: it leaves the hub
    /// calling itself degraded for the rest of its life, and hands the first subscriber a
    /// full reconcile for a window it was never inside.
    #[test]
    fn a_re_arm_nobody_was_listening_to_owes_nothing() {
        let (_dir, root) = resolved_tempdir();
        let (_other, second) = resolved_tempdir();
        let hub = WorkspaceChangeHub::start_targets_with_period(
            vec![WatchTarget::recursive(root.clone())],
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching());

        assert!(hub.ensure_roots(&[WatchTarget::recursive(root), WatchTarget::recursive(second),]));

        assert_eq!(hub.health(), Health::Healthy, "a debt nobody can acknowledge is not health");
        let newcomer = hub.subscribe();
        assert!(
            !hub.materialize(newcomer).rescan_required,
            "a consumer that arrived after the swap inherited a reconcile for it",
        );
    }

    /// The role has to hold for BOTH spellings. A link named one thing onto a file named
    /// another resolves to a key the walk of the TARGET's root does list — and this walk was
    /// never entitled to reach it, so handing it over would register a file from outside
    /// the workspace as drift inside it.
    #[cfg(unix)]
    #[test]
    fn a_link_whose_two_spellings_disagree_on_role_is_not_handed_over() {
        let (_dir, base) = resolved_tempdir();
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("Target.bsl");
        std::fs::write(&target, "Процедура П() КонецПроцедуры").unwrap();
        let incoming = base.join("incoming");
        std::fs::create_dir_all(&incoming).unwrap();
        std::os::unix::fs::symlink(&target, incoming.join("Alias.txt")).unwrap();
        std::os::unix::fs::symlink(&target, incoming.join("Agreed.bsl")).unwrap();

        let mut records = Vec::new();
        collect_subtree(&incoming, &mut records);
        let keys: Vec<&PathBuf> = records.iter().map(|(canonical, _, _)| canonical).collect();

        assert_eq!(
            keys.len(),
            1,
            "a link whose name and target disagree on role was handed over: {keys:?}",
        );
        // The control: a link the walk WOULD take is still taken, resolved.
        assert_eq!(keys[0], &target, "a link both spellings agree about must be delivered");
    }

    /// Two doors into ONE tree are two registrations, and the set has to name both. The
    /// second lies under neither the first's spelling nor any declared root, so nothing
    /// else would ever hand it to `unwatch` — and a record kept only for the first would
    /// leave its twin watching a tree no topology declares, for the life of the daemon.
    #[cfg(unix)]
    #[test]
    fn every_watch_armed_by_an_event_is_named_by_the_set() {
        let (_shared_dir, _dir, first, second, shared, hub, refusals) = hub_with_a_door();

        std::os::unix::fs::symlink(&shared, first.join("other-door")).unwrap();
        assert!(
            eventually(|| refusals.arms_of(&shared) >= 1),
            "the second door into the same tree is armed too",
        );
        assert!(hub.tick_now(), "the arm is in place");
        refusals.forget_asks();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(second)]));

        let key = shared.canonicalize().unwrap();
        let dropped = refusals
            .calls()
            .iter()
            .filter(|(kind, path)| *kind == WatchCallKind::Disarm && *path == key)
            .count();
        assert_eq!(
            dropped,
            2,
            "both registrations into the tree had to be dropped: {:?}",
            refusals.calls(),
        );
    }

    /// A door whose spelling the declaration takes over is the declaration's — but only
    /// once the declaration actually HOLDS it. An arm that failed placed nothing, and a
    /// record dropped on the strength of the intention leaves the old registration standing
    /// with nothing able to name it again.
    #[cfg(unix)]
    #[test]
    fn a_door_whose_promotion_failed_keeps_the_record_that_can_drop_it() {
        let (_shared_dir, _dir, first, second, shared, hub, refusals) = hub_with_a_door();
        let door = first.join("door");

        refusals.refuse(&door);
        assert!(
            !hub.ensure_roots(&[
                WatchTarget::recursive(first),
                WatchTarget::recursive(door.clone()),
            ]),
            "a declared root that will not arm is not full coverage",
        );
        refusals.allow(&door);
        refusals.forget_asks();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(second)]));
        assert!(
            unwatched(&refusals, &shared),
            "the declaration never took the door's watch, and the record that could drop \
             it was thrown away anyway: {:?}",
            refusals.calls(),
        );
    }

    /// A door removed from disk keeps a spelling that still reads as inside the scope while
    /// the registration it placed hangs over a tree nothing will walk again. Deciding on
    /// the spelling alone is how such a watch survives every re-arm.
    #[cfg(unix)]
    #[test]
    fn a_door_that_is_gone_is_dropped_even_though_its_spelling_is_in_scope() {
        let (_shared_dir, _dir, first, second, _shared, hub, refusals) = hub_with_a_door();
        let door = first.join("door");

        std::fs::remove_file(&door).unwrap();
        refusals.forget_asks();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(first), WatchTarget::recursive(second),]));
        assert!(
            unwatched(&refusals, &door),
            "the door is gone and its watch was kept because the spelling still reads as \
             inside the scope: {:?}",
            refusals.calls(),
        );
    }

    /// The defensive pass exists on backends where a recursive unwatch strips descendants,
    /// and a door that fails it is KEPT. Unlike a declared target — which the blind set
    /// reports and the retry puts back — a door is reached by neither, so its record is the
    /// only handle anything has on the registration; the pass is defensive, so the watch it
    /// names may well still stand, and dropping the record would leave it unnameable.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_door_that_fails_the_defensive_pass_keeps_the_record_that_can_drop_it() {
        let (_shared_dir, _dir, first, second, shared, hub, refusals) = hub_with_a_door();

        refusals.refuse(&shared);
        assert!(hub.ensure_roots(&[
            WatchTarget::recursive(first),
            WatchTarget::recursive(second.clone()),
        ]));
        refusals.allow(&shared);
        refusals.forget_asks();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(second)]));
        assert!(
            unwatched(&refusals, &shared),
            "a defensive arm that failed threw away the only record that could drop the \
             watch: {:?}",
            refusals.calls(),
        );
    }

    /// And it must NOT go while the declaration still reaches it. No declaration names such
    /// a watch, so a re-arm cannot put it back, and nothing will reveal the door a second
    /// time — a symlink that merely stands there fires no event. Taking it away with the
    /// declared targets is the trade the obvious fix for the leak makes: coverage lost for
    /// coverage leaked, which is the worse half.
    #[cfg(unix)]
    #[test]
    fn a_watch_armed_by_an_event_survives_a_re_arm_that_still_reaches_it() {
        let (_shared_dir, _dir, first, second, shared, hub, refusals) = hub_with_a_door();

        assert!(hub.ensure_roots(&[WatchTarget::recursive(first), WatchTarget::recursive(second),]));

        assert!(
            !unwatched(&refusals, &shared),
            "a declaration that still leads to the door took its watch away, and nothing \
             will ever place it again: {:?}",
            refusals.calls(),
        );
    }

    /// On FSEvents an arm is a whole-stream swap: the running stream is stopped and a new
    /// one started from "now", so every change anywhere in the watched tree during the swap
    /// is dropped and never reported again. The arm SUCCEEDS, which is why nothing else
    /// here says a word about it — the blind set reports the opposite case, and a consumer
    /// that is never told cannot know to go looking.
    #[cfg(target_os = "macos")]
    #[test]
    fn arming_a_watch_over_a_revealed_subtree_owes_the_window_it_cost() {
        let (_shared_dir, shared) = resolved_tempdir();
        let (_dir, root) = resolved_tempdir();
        let hub = WorkspaceChangeHub::start_targets_with_period(
            vec![WatchTarget::recursive(root.clone())],
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching());
        let cursor = hub.drain(hub.subscribe()).cursor;
        assert!(!hub.materialize(cursor).rescan_required, "the stand starts level");

        std::os::unix::fs::symlink(&shared, root.join("door")).unwrap();

        assert!(
            eventually(|| hub.materialize(cursor).rescan_required),
            "the stream was restarted to reach the linked subtree, and the window that \
             cost is owed to every consumer",
        );
    }

    /// A target that does not exist YET — a declared root created later — cannot be
    /// canonicalised, and the raw spelling left over is not the spelling its recursive
    /// ancestor is remembered by once a link sits above them both. Compared that way the
    /// ancestor does not contain it, so it is armed on its own, fails on a path that is
    /// not there, and the whole set reports itself uncovered — over a subtree the
    /// ancestor's recursive watch is in fact already following.
    ///
    /// The second half is the control that keeps this from reading as "anything missing
    /// is absorbed": the resolution is real, so a missing path behind a link that leaves
    /// the root lands outside it and stays a target of its own.
    #[cfg(unix)]
    #[test]
    fn a_target_that_does_not_exist_yet_is_still_placed_against_its_ancestor() {
        let dir = tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let real = base.join("real");
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        std::os::unix::fs::symlink(&elsewhere, real.join("out")).unwrap();

        let inside = link.join("later").join("ext");
        let kept = dedup_targets(vec![
            WatchTarget::recursive(link.clone()),
            WatchTarget::recursive(inside),
        ]);
        assert_eq!(
            kept.iter().map(|(t, _)| t.path.clone()).collect::<Vec<_>>(),
            vec![link.clone()],
            "a root the ancestor's recursive watch covers was armed separately",
        );

        let out_of_the_root = link.join("out").join("ext");
        let kept = dedup_targets(vec![
            WatchTarget::recursive(link.clone()),
            WatchTarget::recursive(out_of_the_root.clone()),
        ]);
        let mut placed: Vec<PathBuf> = kept.iter().map(|(t, _)| t.path.clone()).collect();
        placed.sort();
        let mut expected = vec![link, out_of_the_root];
        expected.sort();
        assert_eq!(
            placed, expected,
            "a missing path that resolves OUT of the root is not covered by it",
        );
    }

    /// Nested targets collapse under a recursive ancestor so a subtree is never
    /// double-watched (which some backends would report as duplicate events),
    /// regardless of input order — while a NON-recursive ancestor absorbs nothing
    /// (it covers only direct children), and a recursive duplicate of the same
    /// path wins over a non-recursive one.
    #[test]
    fn dedup_targets_drops_subtrees_of_recursive_ancestors_only() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("parent");
        let child = parent.join("sub");
        let sibling = dir.path().join("sibling");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let r = |p: &PathBuf| WatchTarget::recursive(p.clone());
        let nr = |p: &PathBuf| WatchTarget { path: p.clone(), recursive: false };
        let kept_paths = |targets: Vec<WatchTarget>| -> Vec<(PathBuf, bool)> {
            dedup_targets(targets).into_iter().map(|(t, _)| (t.path, t.recursive)).collect()
        };

        let kept = kept_paths(vec![r(&parent), r(&child), r(&sibling)]);
        assert!(kept.contains(&(parent.clone(), true)), "the ancestor is kept");
        assert!(kept.contains(&(sibling.clone(), true)), "a disjoint root is kept");
        assert!(!kept.iter().any(|(p, _)| p == &child), "a nested root is dropped");

        // Order-independent: the child listed first is still dropped.
        assert_eq!(kept_paths(vec![r(&child), r(&parent)]), vec![(parent.clone(), true)]);

        // A non-recursive ancestor does not absorb a recursive descendant.
        let kept = kept_paths(vec![nr(&parent), r(&child)]);
        assert!(kept.contains(&(parent.clone(), false)));
        assert!(kept.contains(&(child.clone(), true)), "non-recursive parent covers no subtree");

        // Same path, both modes: the recursive registration wins.
        assert_eq!(kept_paths(vec![nr(&parent), r(&parent)]), vec![(parent.clone(), true)]);
    }
}
