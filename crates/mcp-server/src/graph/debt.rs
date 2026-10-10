//! What the graph owes the disk, in one place.
//!
//! Every obligation the graph carries — a delivered change, a forced project reload, a build
//! that failed, a publication that could not vouch for itself, marks nobody has consumed, a
//! refresh the publish hook could not run — used to be its own flag with its own owner rule.
//! Each rule was reasonable on its own, and between them lived debts nothing owned: a
//! `force_stale` publication whose only claimant left with the request path, a loader thread
//! that failed to spawn, a retry nobody woke.
//!
//! So the obligations live here, in one state, and one function decides what the graph does
//! next. [`GraphDebt::decide`] is pure: it reads the debts and the graph's own lifecycle facts
//! and says what to start, what to check, what to probe and when to wake. It touches no disk,
//! takes no lock and spawns nothing — which is what lets every reachable order of events be
//! enumerated in a test instead of argued about.
//!
//! Execution is [`super::GraphState::drive`], and it is the only executor. Everything else
//! records: the watcher on its batch, the search consumer on its own, the build thread on its
//! outcome. Recording is cheap and cannot fail; deciding is one place to read.

use std::time::{Duration, Instant};

use crate::change_hub::LossHorizon;
use crate::state::retry_window::{RetryDecision, RetryOwner, RetryWindow};

/// How long a build owed to unconsumed marks waits before it runs, at the least. The consumer
/// places marks and then records a change itself; this is the room that has to start the build
/// that would consume them, before a forced one is paid for.
pub(super) const OWED_MARKS_GRACE: Duration = Duration::from_secs(2);

/// How long after a publication that cannot vouch for itself the first recovery probe runs.
/// One minute is the cadence the fingerprint map already re-anchors on, so a probe costs no
/// more than the walk a request used to pay for.
pub(super) const RECOVERY_PROBE: Duration = Duration::from_secs(60);

/// The longest a recovery probe waits when nothing has healed. A subtree that stays unreadable
/// costs one stat walk per this interval and nothing else.
pub(super) const RECOVERY_PROBE_CAP: Duration = Duration::from_secs(8 * 60);

/// How long a decision the lease could not confirm waits before it is taken again.
pub(super) const HELD_RETRY: Duration = Duration::from_secs(2);

/// The fact number a graph with no hub carries.
///
/// There is no fact stream, so there are no fact identities: every delivery reads as the same
/// number. A frontier over it would answer everything for ever after the first publication,
/// which is why the identity rules below step aside for it — a caller that nudges such a graph
/// is the only evidence there is, and it is telling the truth.
///
/// A TEST boundary, not a daemon state: every workspace graph a daemon builds carries a hub.
/// Stepping aside stops at credit — once an admission has spent this number there is no larger
/// one for a later nudge to be unused against, so no such nudge revives a spent budget, and a
/// test that needs a second attempt on a hubless graph has to drive it rather than nudge it.
pub(super) const NO_FACT_STREAM: u64 = u64::MAX;

/// How long a hook that declined waits before it is offered the same work again.
pub(super) const HOOK_REVISIT: Duration = Duration::from_secs(30);

/// Why a build failed, as far as the debt is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FailureKind {
    /// A fence or a lease refusal: retried inside the budget.
    Transient,
    /// The operation itself failed: the budget stops, and only fresh work reopens it.
    Operation,
    /// The thread never started. Nothing else will call back, so it is owed like any other
    /// failure — this is the debt that used to be lost entirely.
    Spawn,
    /// The workspace is gone for good. Nothing is owed any more.
    Terminal,
}

/// What a build is, for the decision that starts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BuildKind {
    /// The graph has nothing published (or its last build failed): a full load.
    Initial,
    /// The graph is published and must catch up.
    Reload,
}

/// A build the decision asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BuildStart {
    pub(super) kind: BuildKind,
    /// The debt branch that selected this attempt. Kept with the decision so
    /// observability reports the scheduler's choice without re-deriving it.
    pub(super) trigger: &'static str,
    /// Forced builds ignore an equal fingerprint and re-read the project: a config change or
    /// marks nobody consumed cannot be answered by a fingerprint comparison.
    pub(super) forced: bool,
}

/// What a build was admitted FOR, fixed at the claim and carried unchanged to its outcome.
///
/// The thing this type exists to make impossible: a builder reconstructing its own mandate
/// from whatever the debts happen to say once it is already running. It was admitted for the
/// facts on the table at the moment the slot was granted, and the proof it publishes covers
/// those and no others. Anything delivered afterwards is somebody else's mandate — it stays
/// pending, and it is what pays for the next admission.
///
/// `scan_cutoff` is read just BEFORE the slot is granted, never after. Early is safe in the
/// only direction that matters: a fact arriving in that sliver is simply not covered, and a
/// proof can never claim more than the build was admitted for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BuildTicket {
    /// Unique within this lease generation. Identifies the admission, not the published
    /// database generation — a failed build moves the latter not at all.
    pub(super) claim: u64,
    pub(super) kind: BuildKind,
    /// The mode, decided once. A demand arriving later cannot turn an ordinary catch-up into
    /// a forced reload, and a publication of an ordinary catch-up cannot discharge a forced
    /// demand that arrived after it was admitted.
    pub(super) forced: bool,
    /// The hub position this build's proof may cover.
    pub(super) scan_cutoff: u64,
    /// The forced fact this ticket answers, when it was admitted forced.
    pub(super) forced_through: Option<u64>,
    /// Every recovery origin issued when this build was admitted. Its publication answers
    /// those and no others: an improvement measured after this line keeps its own demand,
    /// whatever this build goes on to do.
    pub(super) recovery_cutoff: u64,
    /// Which lanes paid for this admission. An outcome closes THESE accounts and no others:
    /// a build sponsored only by marks that fails must not leave a primary retry budget
    /// behind it, and its Operation must close the account that actually paid.
    pub(super) sponsors: Sponsors,
}

/// The lanes that paid for one admission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Sponsors {
    pub(super) primary: bool,
    pub(super) marks: bool,
}

impl Sponsors {
    pub(super) fn any(self) -> bool {
        self.primary || self.marks
    }
}

/// The refreshes a publish hook reported it could not run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HookDebt {
    pub(crate) topology: bool,
    pub(crate) roots: bool,
    /// A bounded context refresh for covered marks was refused. Kept separate
    /// from topology so retrying marks never promotes a body edit to full refresh.
    pub(crate) marks: bool,
}

impl HookDebt {
    pub(crate) fn any(self) -> bool {
        self.topology || self.roots || self.marks
    }

    fn merge(&mut self, other: HookDebt) {
        self.topology |= other.topology;
        self.roots |= other.roots;
        self.marks |= other.marks;
    }
}

/// The graph's lifecycle as the decision sees it. Copied out before the debt lock is taken, so
/// deciding never reaches back into the graph.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Facts {
    pub(super) ready: bool,
    pub(super) failed: bool,
    /// Nothing published and nothing running: only the first build can start here.
    pub(super) idle: bool,
    /// A build or reload is running: its outcome decides what is still owed.
    pub(super) in_flight: bool,
    /// This daemon owns the workspace's caches right now.
    pub(super) owns: bool,
    /// The workspace is gone for good.
    pub(super) terminal: bool,
    /// The daemon has asked every background owner to leave.
    ///
    /// Part of the decision rather than a guard wrapped around its callers, because the paths
    /// that record a debt call the executor from inside themselves — the watcher's drain runs
    /// `record_change` and `record_forced`, and each of those drives. A check placed "around"
    /// the drain closes half the window by construction, which is what it did.
    pub(super) stopping: bool,
}

/// What the graph does next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Decision {
    /// Start a build.
    pub(super) start: Option<BuildStart>,
    /// Compare the published build against disk: a delivered change is owed an answer, and a
    /// fingerprint that still matches is the answer.
    pub(super) check: bool,
    /// Look at the paths a publication could not read.
    pub(super) probe: bool,
    /// Offer the publish hook what it could not take.
    pub(super) flush_hook: HookDebt,
    /// When the next decision is due, if it is due at a time rather than on an event.
    pub(super) wake_at: Option<Instant>,
}

impl Decision {
    /// Whether this decision asks for anything at all.
    #[cfg(test)]
    pub(super) fn does_work(&self) -> bool {
        self.start.is_some() || self.check || self.probe || self.flush_hook.any()
    }
}

/// Which debt a ripeness is talking about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DebtKind {
    Failed,
    Forced,
    Marks,
    Change,
    Recovery,
    Hook,
}

/// What would move a debt whose own budget is spent. Named rather than implied: an exhaustion
/// nobody can name is indistinguishable from a debt that was quietly dropped, and the model
/// proves the name by applying it and watching the debt come back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Revival {
    /// A delivered change or a forced reload — the fresh external work a spent budget waits for.
    FreshFact,
    /// Marks placed anew by a consumer.
    FreshMarks,
}

/// When one debt's next attempt may run — and, when it may not, what stands in the way.
///
/// The distinction this type exists for: an OPEN debt is not a RIPE one. Treating the two as
/// one is what started a build inside the grace its consumer had been given, and what left
/// marks queued behind a retry that had already spent its budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ripeness {
    /// The attempt may run at this moment: the executor's turn, not the alarm's.
    Now,
    /// It comes due then, and nothing else is needed to make it run.
    At(Instant),
    /// A standing watch on something no fact stream announces — a permission that may be
    /// restored, a subtree that may become readable again. No moment retires it; only a healing
    /// does. Its cadence backs off to a declared cap and stays there, so the cost is bounded.
    Watching(Instant),
    /// The budget is spent. No alarm will move it; only the named external work will.
    Exhausted(Revival),
    /// Another debt holds the single build slot. That debt's own ripeness IS this one's alarm,
    /// which is why a blocker may never itself be exhausted.
    Behind(DebtKind),
}

/// Every debt's ripeness at one moment: the whole of what the schedule says about itself. The
/// decision picks work from it, the alarm is its projection, and the model holds it to the
/// contract WITHOUT re-running the decision — which is what the ownership check used to do, and
/// why it proved nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Standing {
    pub(super) failed: Option<Ripeness>,
    pub(super) forced: Option<Ripeness>,
    pub(super) marks: Option<Ripeness>,
    pub(super) change: Option<Ripeness>,
    pub(super) recovery: Option<Ripeness>,
    pub(super) hook: Option<Ripeness>,
}

impl Standing {
    pub(super) fn each(&self) -> [(DebtKind, Option<Ripeness>); 6] {
        [
            (DebtKind::Failed, self.failed),
            (DebtKind::Forced, self.forced),
            (DebtKind::Marks, self.marks),
            (DebtKind::Change, self.change),
            (DebtKind::Recovery, self.recovery),
            (DebtKind::Hook, self.hook),
        ]
    }

    pub(super) fn of(&self, kind: DebtKind) -> Option<Ripeness> {
        match kind {
            DebtKind::Failed => self.failed,
            DebtKind::Forced => self.forced,
            DebtKind::Marks => self.marks,
            DebtKind::Change => self.change,
            DebtKind::Recovery => self.recovery,
            DebtKind::Hook => self.hook,
        }
    }
}

/// Whether a debt is somebody's to run: now, at a moment, or under a standing watch. An
/// exhausted debt is NOT live, and that single rule is what keeps another debt from queueing
/// behind it for ever.
pub(super) fn is_live(ripeness: Option<Ripeness>) -> bool {
    matches!(ripeness, Some(Ripeness::Now | Ripeness::At(_) | Ripeness::Watching(_)))
}

/// Whether the debt `kind` has an owner that will actually run, following the queue to its
/// head. A chain that ends on an exhaustion — or on nothing at all — has no owner.
///
/// Bounded by the number of debts, and a repeat is a cycle, which is the same answer.
fn owner_is_live(standing: &Standing, kind: DebtKind) -> bool {
    let mut at = kind;
    let mut seen = Vec::new();
    loop {
        if seen.contains(&at) {
            return false;
        }
        seen.push(at);
        match standing.of(at) {
            Some(Ripeness::Behind(next)) => at = next,
            other => return is_live(other),
        }
    }
}

/// Whether a debt's work is about to run THIS turn. What one debt may queue behind is almost
/// always this, not mere liveness: a debt whose moment is still ahead is not taking the slot,
/// and treating it as though it were is how a delivered change came to wait out a grace whose
/// whole purpose was to wait for a delivered change.
fn ripe_now(ripeness: Option<Ripeness>) -> bool {
    matches!(ripeness, Some(Ripeness::Now))
}

/// The moment a ripeness names, when it names one.
fn moment(ripeness: Option<Ripeness>) -> Option<Instant> {
    match ripeness {
        Some(Ripeness::At(at) | Ripeness::Watching(at)) => Some(at),
        _ => None,
    }
}

/// A retry schedule: when the next attempt may run, and the budget that bounds them all.
#[cfg_attr(test, derive(Clone))]
struct Schedule {
    next_allowed: Instant,
    attempts: u32,
    window: RetryWindow,
}

impl Schedule {
    fn new(now: Instant, first: Duration) -> Self {
        Self { next_allowed: now + first, attempts: 0, window: RetryWindow::new(RetryOwner::Graph) }
    }

    /// THE maturity question, and the only one. Whether the debt is open is the caller's
    /// business; this says when — and whether — its next attempt may run.
    ///
    /// A moment that has already passed is `Now`, never `At`: `At` is built HERE and nowhere
    /// else, behind `next_allowed > now`, so no debt can contribute a stale moment to the
    /// alarm. An alarm already past is a wait of zero length handed back on every turn.
    fn ripeness(&self, now: Instant, revival: Revival) -> Ripeness {
        if !self.window.is_open(now) {
            return Ripeness::Exhausted(revival);
        }
        if self.next_allowed <= now {
            Ripeness::Now
        } else {
            Ripeness::At(self.next_allowed)
        }
    }

    /// Whether an attempt may start now; if so, the next one is scheduled behind it. The
    /// delay is the caller's: a failure may retry at once and back off after, while a build
    /// owed to marks always leaves the consumer room to start one itself.
    fn attempt(&mut self, now: Instant, delay: Duration) -> bool {
        if self.next_allowed > now {
            return false;
        }
        match self.window.refused(now, delay) {
            RetryDecision::RetryAfter(delay) => {
                self.attempts = self.attempts.saturating_add(1);
                self.next_allowed = now + delay;
                true
            }
            RetryDecision::Stop(_) => false,
        }
    }

    /// The delay this schedule's next refusal earns.
    fn delay(&self) -> Duration {
        crate::state::overlay_retry::retry_delay(self.attempts)
    }

    /// Fresh external work is what a spent budget waits for. Says whether it revived one.
    fn revive(&mut self, now: Instant, first: Duration) -> bool {
        if !self.window.observe_external_work(now, true) {
            return false;
        }
        self.attempts = 0;
        self.next_allowed = now + first;
        true
    }

    fn operation_error(&mut self) {
        self.window.operation_error();
    }

    /// Whether this account may still buy an attempt at all — budget unspent and unexpired.
    /// Intrinsic: it does not ask who owns the slot.
    fn is_open(&self, now: Instant) -> bool {
        self.window.is_open(now)
    }
}

/// A build that failed and is owed again.
#[cfg_attr(test, derive(Clone))]
struct Failed {
    kind: FailureKind,
    schedule: Schedule,
}

/// The scope a walk covers: the declared scan roots, the subtrees excluded from them, and
/// whether the declaration was VALIDATED or the restricted fallback a broken project falls to.
///
/// Compared as a composition, never as a number. A fold is an index; two different root sets
/// that fold alike are still two scopes, and only an equality of the lists can say a root is
/// no longer required. A fallback declares nothing: it is what the loader does when it cannot
/// read the project, and a path it fails to mention is not a path that has gone away.
#[derive(Clone, Debug, Default)]
pub(super) struct RecoveryScope {
    roots: Vec<std::path::PathBuf>,
    excluded: Vec<std::path::PathBuf>,
    validated: bool,
    /// Whether every root and exclusion resolved to a real place on disk. A property of the
    /// ATTEMPT, not of the composition: the same declaration read while a parent directory
    /// refuses to be traversed is the same set of roots, so this is deliberately outside the
    /// identity below.
    resolved: bool,
}

/// Two descriptors are the same scope when they ask for the same thing. Whether the loader
/// managed to resolve them this time is a fact about the attempt — a root that could not be
/// canonicalised for a moment must not read as a different project.
impl PartialEq for RecoveryScope {
    fn eq(&self, other: &Self) -> bool {
        self.roots == other.roots
            && self.excluded == other.excluded
            && self.validated == other.validated
    }
}

impl Eq for RecoveryScope {}

impl RecoveryScope {
    /// The descriptor of one actually loaded project. `validated` says the declaration was
    /// read and understood — not the restricted fallback.
    pub(super) fn of(
        roots: &[std::path::PathBuf],
        excluded: &[std::path::PathBuf],
        validated: bool,
    ) -> Self {
        // Canonical spellings, because the addresses these roots are compared against are
        // canonical: the build enumerates canonically and hands `File::open` the very string
        // it recorded, so a declared root in another spelling of the same directory would
        // cover none of them. A root that cannot be canonicalised has gone — it keeps the
        // spelling it was declared with, and its disappearance is a change of composition like
        // any other.
        let mut resolved = true;
        let canonical = |paths: &[std::path::PathBuf], report: Option<&mut bool>| {
            let mut failed = false;
            let mut sorted: Vec<std::path::PathBuf> = paths
                .iter()
                .map(|path| {
                    path.canonicalize().unwrap_or_else(|_| {
                        failed = true;
                        path.clone()
                    })
                })
                .collect();
            sorted.sort();
            sorted.dedup();
            if let Some(report) = report {
                *report = !failed;
            }
            sorted
        };
        // Only the ROOTS decide whether this descriptor may speak for a removal. An exclusion
        // that does not resolve keeps its declared spelling and therefore excludes less than
        // it names — which leaves an obligation standing, the safe direction. A root in that
        // state covers NOTHING, which is the direction that quietly answers real work away.
        let roots = canonical(roots, Some(&mut resolved));
        let excluded = canonical(excluded, None);
        Self { roots, excluded, validated, resolved }
    }

    /// Whether the declaration behind this descriptor was read and understood — as opposed to
    /// the restricted fallback a broken project falls to.
    pub(super) fn is_validated(&self) -> bool {
        self.validated
    }

    /// How many roots this descriptor names, for a cost measurement that has to say what it
    /// walked.
    #[cfg(all(test, unix))]
    pub(super) fn roots_len(&self) -> usize {
        self.roots.len()
    }

    /// Whether this descriptor may say that an address is no longer required.
    ///
    /// Two things have to hold, and the second is the one a lexical fallback hides: the
    /// declaration was read and understood, AND every root in it resolved. A root that could
    /// not be canonicalised keeps the spelling it was declared with, and under that spelling
    /// the canonical addresses beneath it match nothing — so a directory that merely refused
    /// to be traversed would read as a directory that had gone away.
    pub(super) fn speaks_for_removal(&self) -> bool {
        self.validated && self.resolved
    }

    /// Whether this scope still asks for `key`: under one of its roots, and under none of its
    /// exclusions.
    pub(super) fn requires(&self, key: &str) -> bool {
        let path = std::path::Path::new(key);
        self.roots.iter().any(|root| path.starts_with(root))
            && !self.excluded.iter().any(|excluded| path.starts_with(excluded))
    }

    /// The composition alone: the same roots in another order are the same scope, and a change
    /// of exclusions is a change of what can be walked.
    fn same_composition(&self, other: &Self) -> bool {
        self.roots == other.roots && self.excluded == other.excluded
    }
}

/// What a probe measured about one capability.
///
/// Four, not three: a capability that is REQUIRED and never measured is Unknown, and Unknown is
/// not a negative. A build whose read failed to decode says nothing about whether the file
/// opens, and treating that silence as Denied is what a later "healing" would be paid for
/// twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    /// Required; nothing has been measured about it yet.
    Unknown,
    /// Measured negative: the walk came up short, or the open failed with an actual error.
    Denied,
    /// Measured positive: the walk completed, or the file opened.
    Granted,
    /// Measured absent: what could not be read is not there any more.
    Absent,
}

/// The capability one measurement is about — an identity, not a summary.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Capability {
    /// One path a publication could not read, by the very string the build hands `File::open`.
    Open(String),
    /// The walk of the required scope. Its identity is the scope descriptor the receipt
    /// carries beside it, never a name in here.
    ScanRoots,
}

/// What a measurement was taken against, compared WHOLE.
///
/// Per-key comparison was the hole: a key the memory has never seen has no basis of its own,
/// so a receipt from a publication that has since been replaced could introduce a brand new
/// capability — positive — and be paid for it. One token, one comparison, all names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RecoveryBasis {
    /// The unbroken chain these obligations belong to.
    episode: u64,
    /// The installed publication whose gaps they are.
    generation: u64,
    /// Bumped whenever what is required changes.
    revision: u64,
}

impl RecoveryBasis {
    /// The publication these obligations are the gaps of. What a walk must be looking at to
    /// be looking at them at all.
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }
}

/// One outstanding required capability.
#[cfg_attr(test, derive(Clone))]
struct Cell {
    level: Level,
    /// Which time round this address has been required. A path retired by proof and later
    /// genuinely unread again is a NEW obligation, not the old one resurrected.
    occurrence: u64,
    /// The origin of this cell's last measured improvement, kept until a claim captures it.
    /// Spending is global (`recovery_spent`), so capturing costs nothing per cell.
    pending: Option<u64>,
}

/// The one walk obligation an episode may carry. There is no map of scopes: what is owed is
/// the walk of the scope that stands, and what was measured last.
#[cfg_attr(test, derive(Clone))]
struct ScanCell {
    scope: RecoveryScope,
    level: Level,
    pending: Option<u64>,
}

/// What an installed publication proves about the obligations outstanding when it was
/// prepared.
///
/// Every field is something the publication ACTUALLY did. A shorter unread list is not a
/// proof: an enumeration that came up short answers nothing, and only the three positives
/// below may retire an obligation.
#[derive(Debug, Default)]
pub(super) struct RecoveryPublicationProof {
    /// The publication's own generation — the basis its gaps are measured against.
    pub(super) generation: u64,
    /// The required-set frontier this proof was prepared against: an improvement measured
    /// after it is not answered by it.
    pub(super) captured_seq: u64,
    /// The scope this publication speaks for, when the declaration was validated.
    pub(super) scope: Option<RecoveryScope>,
    /// The strict, generation-bound unread set of the installed artefact. `None` when this
    /// publication has no fresh authority over membership at all — a cache served as it
    /// stands, or metadata that would not read.
    pub(super) declared_unread: Option<Vec<String>>,
    /// Whether the walk behind it may speak for the whole tree. `None` when it did not walk.
    pub(super) scan_complete: Option<bool>,
    /// Whether the tree moved under this build. A complete enumeration of a world that has
    /// already changed is still a complete enumeration — of something else — so it is kept
    /// apart from the completeness above rather than folded into it.
    pub(super) straddled: bool,
    /// Outstanding keys this publication actually enumerated and read.
    pub(super) read_covered: Vec<(String, u64)>,
    /// Outstanding keys a complete, identity-exact walk of the requiring scope proves absent.
    pub(super) absent_covered: Vec<(String, u64)>,
    /// Outstanding keys a validated declaration no longer asks for.
    pub(super) out_of_scope_covered: Vec<(String, u64)>,
}

impl RecoveryPublicationProof {
    /// A publication that proves nothing about recovery and declares nothing either.
    ///
    /// Test-side only: every production publication goes through the producer, which pairs
    /// what the build actually did with what is outstanding — including a cache, which pairs
    /// it with nothing.
    #[cfg(test)]
    pub(super) fn without_coverage(generation: u64) -> Self {
        Self { generation, ..Self::default() }
    }
}

/// What an installed publication leaves for the caller to throw away.
///
/// The keys of the obligations it answered and the proof it consumed — every string of both.
/// Freed where it is dropped, so it is handed OUT of the critical section rather than
/// released inside it: the publication gate, the lease and `inner` are all held there, and
/// returning several thousand allocations to the allocator under them is work every reader of
/// the graph waits through for nothing.
#[derive(Default)]
pub(super) struct RetiredPayload {
    keys: Vec<String>,
    proof: Option<RecoveryPublicationProof>,
}

/// What the producer of a proof needs to know: which obligations stand, and where the origin
/// frontier is.
pub(super) struct OutstandingRecovery {
    pub(super) keys: Vec<(String, u64)>,
    pub(super) captured_seq: u64,
}

/// The reservation one probe holds: what to observe, and what it is measuring against.
pub(super) struct ProbePlan {
    pub(super) token: u64,
    pub(super) basis: RecoveryBasis,
    /// EVERY outstanding obligation, not merely the last publication's unread set: a retained
    /// obligation outside the newest enumeration would otherwise have memory and no observer.
    pub(super) open: Vec<String>,
    /// The scope whose walk is still owed, if one is.
    pub(super) scan: Option<RecoveryScope>,
}

/// What a probe brings back.
pub(super) struct ProbeReceipt {
    pub(super) token: u64,
    pub(super) basis: RecoveryBasis,
    pub(super) levels: Vec<(Capability, Level)>,
    /// The scope the walk actually covered, from the same walk that returned its verdict.
    pub(super) scope: Option<RecoveryScope>,
}

/// Whether measuring `now` where `previous` stood is a transition worth a build.
///
/// A negative buys nothing: it is where an obligation starts, and measuring it again says the
/// world has not moved. Everything else that CHANGES is news — first knowledge of a required
/// capability, a measured improvement, and a measured change of composition either way, since
/// a file that went away and one that came back are both facts about the tree the graph is
/// meant to describe.
fn improves(previous: Level, now: Level) -> bool {
    match now {
        Level::Denied | Level::Unknown => false,
        _ => previous != now,
    }
}

/// What the schedule made of a receipt. A semantic answer, because the executor cannot read
/// one off `Option<forced_fact>`: several healings can share one observation, and the number
/// does not move when new authority arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProbeResult {
    /// Something was measured that had not been measured before; work is owed.
    NewEvidence,
    /// The probe looked and found nothing new.
    NoNewEvidence,
    /// The receipt was measured against a basis that no longer stands; nothing was applied.
    Obsolete,
    /// The probe could not look at all.
    CouldNotLook,
}

/// A publication that cannot vouch for itself without an external event: it straddled a write,
/// its scan was short, or it could not read some modules. Nothing on the fact stream will
/// announce the healing — a restored permission is not a file change — so the only owner it
/// can have is a probe.
#[cfg_attr(test, derive(Clone))]
struct Recovery {
    next_probe: Instant,
    interval: Duration,
    /// A probe that found a healing and whose build came back just as unsound: the pacing
    /// stops following requests, so a file flapping between readable and unreadable cannot
    /// rebuild the graph once per request.
    flapping: bool,
    /// The unbroken chain. It survives failures, unsound publications and new database
    /// generations; only an installed coherent proof or a terminal lease ends it.
    episode: u64,
    /// The installed publication these obligations are measured against, and the revision of
    /// what is required. Together with the episode they are the basis a receipt must match.
    generation: u64,
    revision: u64,
    /// P: one cell per distinct outstanding required address. Not a cache and not a journal —
    /// every entry is an obligation nothing has answered yet, so nothing may be dropped from
    /// it to save room. What bounds it is the workload, and what shrinks it is proof.
    required: std::collections::BTreeMap<String, Cell>,
    /// The one walk obligation, when the publication that stands could not vouch for its own
    /// coverage.
    scan: Option<ScanCell>,
    /// The scope the installed publication speaks for, and the last scope a walk measured.
    /// Two descriptors, not a set of every scope ever seen.
    installed_scope: Option<RecoveryScope>,
    observed_scope: Option<RecoveryScope>,
    /// The origin of the last measured change of composition, kept until a publication has
    /// actually served the declaration that changed. It belongs to no single address, which
    /// is why it cannot live in a cell.
    composition_pending: Option<u64>,
}

impl Recovery {
    fn opened(now: Instant, episode: u64, generation: u64) -> Self {
        Self {
            next_probe: now + RECOVERY_PROBE,
            interval: RECOVERY_PROBE,
            flapping: false,
            episode,
            generation,
            revision: 0,
            required: std::collections::BTreeMap::new(),
            scan: None,
            installed_scope: None,
            observed_scope: None,
            composition_pending: None,
        }
    }

    fn basis(&self) -> RecoveryBasis {
        RecoveryBasis {
            episode: self.episode,
            generation: self.generation,
            revision: self.revision,
        }
    }

    /// Whether anything is still owed an observation. An episode whose obligations have all
    /// been answered still exists until a coherent proof closes it, but it asks for nothing.
    fn outstanding(&self) -> bool {
        !self.required.is_empty() || self.scan.is_some()
    }

    /// The highest origin still attached to something nothing has answered.
    ///
    /// Derived, never banked: an origin lives on the obligation it was measured for, so a
    /// proof that retires the obligation answers the origin with it. A scalar kept beside the
    /// cells said otherwise — it outlived the very address it was issued for and went on
    /// financing builds for work already delivered.
    fn owed_origin(&self) -> Option<u64> {
        self.required
            .values()
            .filter_map(|cell| cell.pending)
            .chain(self.scan.as_ref().and_then(|cell| cell.pending))
            .chain(self.composition_pending)
            .max()
    }
}

/// Context-dirty marks waiting to be consumed, and the build owed to anything no publication
/// has observed. The rule itself is unchanged: a mark is consumed by the first publication
/// that observed the fact it was placed for.
#[derive(Default)]
#[cfg_attr(test, derive(Clone))]
pub(super) struct MarkLedger {
    /// `(mark_high, fact)`: marks up to `mark_high` were placed for hub facts up to `fact`.
    placed: Vec<(i64, u64)>,
    owed: Option<Schedule>,
    /// The highest demand ever placed, kept past consumption. A consumer that places the same
    /// `(mark_high, fact)` again is repeating itself, not asking for anything new, and a
    /// repeat must not buy an admission — that is how a quiet consumer's one placement came
    /// to revive a spent budget on every later call.
    demanded: Option<(i64, u64)>,
}

/// Placed marks outlive a hook that could not consume them; past this many entries the two
/// oldest are merged. Merging is conservative: the merged entry waits for the later fact, so
/// a mark is consumed late at worst and never against a graph that did not observe it.
pub(super) const MARK_LEDGER_CAP: usize = 1024;

impl MarkLedger {
    /// Records the placement and says whether it demanded anything NEW.
    fn place(&mut self, mark_high: i64, fact: u64) -> bool {
        let fresh = self.demanded.is_none_or(|(mark, at)| mark_high > mark || fact > at);
        if fresh {
            self.demanded = Some(
                self.demanded
                    .map_or((mark_high, fact), |(mark, at)| (mark.max(mark_high), at.max(fact))),
            );
        }
        self.placed.push((mark_high, fact));
        if self.placed.len() > MARK_LEDGER_CAP {
            self.placed.sort_by_key(|&(mark, fact)| (fact, mark));
            let (first_mark, first_fact) = self.placed.remove(0);
            let merged = &mut self.placed[0];
            *merged = (merged.0.max(first_mark), merged.1.max(first_fact));
        }
        fresh
    }

    /// The highest mark a publication that observed facts up to `observed` may consume, or
    /// `None` when it observed none of the placed ones.
    pub(super) fn bound(&self, observed: u64) -> Option<i64> {
        self.placed.iter().filter(|&&(_, fact)| fact <= observed).map(|&(mark, _)| mark).max()
    }

    pub(super) fn consumed(&mut self, observed: u64, bound: i64) {
        self.placed.retain(|&(mark, fact)| !(fact <= observed && mark <= bound));
    }

    pub(super) fn has_placed(&self) -> bool {
        !self.placed.is_empty()
    }

    fn has_uncovered(&self, observed: Option<u64>) -> bool {
        self.placed.iter().any(|&(_, fact)| observed.is_none_or(|observed| fact > observed))
    }

    /// How many placements the ledger is holding, for a test that has to see the cap hold.
    #[cfg(test)]
    pub(super) fn placements(&self) -> usize {
        self.placed.len()
    }

    /// The highest mark and fact ever demanded, kept past consumption.
    #[cfg(test)]
    pub(super) fn demanded(&self) -> Option<(i64, u64)> {
        self.demanded
    }
}

/// Everything the graph owes, and the only thing that decides what it does about it.
#[derive(Default)]
#[cfg_attr(test, derive(Clone))]
pub(super) struct GraphDebt {
    /// A delivered change of the scan universe, by the highest fact it was delivered under. It
    /// is answered by a comparison, not by a build: an equal fingerprint IS the answer.
    change: Option<u64>,
    /// A change that no fingerprint comparison can answer — a config edit, a reconcile, marks
    /// nobody consumed — by the highest fact that asked for it.
    forced: Option<u64>,
    failed: Option<Failed>,
    recovery: Option<Recovery>,
    hook: HookDebt,
    /// Which offer of the hook debt is current. A flush claims the revision it was given and
    /// may clear ONLY that one: an offer that came back while a newer publication was writing
    /// its own unhandled bits would otherwise report those as taken too.
    hook_revision: u64,
    /// The earliest the hook may be offered again after it declined.
    ///
    /// A refusal used to leave the debt plainly ripe, so the runner offered it again, and
    /// again, inside the same turn — eight times, then latched a continuation, and the owner
    /// came straight back with a zero wait. A hook that keeps refusing is a real state, not a
    /// transient: a search engine that never came up refuses for as long as the daemon lives.
    hook_revisit: Option<Instant>,
    /// A decision the lease could not confirm. Nothing is dropped for it: the debts stay and
    /// the decision is taken again.
    held_until: Option<Instant>,
    /// The highest fact a PROOF has answered.
    ///
    /// Spending and answering are different events and need different lines. A consumer
    /// repeating a delivery the graph has already answered is not the world changing twice:
    /// two cursors carry the same hub fact, and the second arrival found the demand gone and
    /// recorded it again as though it were news.
    answered_fact: u64,
    /// Where the fact stream stood when the watcher looked.
    ///
    /// Kept APART from a delivered change, and that separation is the whole point: a level
    /// owes a comparison — a build that scanned earlier has not seen this far — and it is not
    /// authority. Sharing the field with deliveries let a synthetic level finance a retry
    /// epoch for a failure that came after it, which is the same forbidden source as before,
    /// only in the other order.
    observed_level: Option<u64>,
    /// The declared losses this graph has acted on that the hub can still deliver. A reconcile
    /// is authority of its own — the detail is gone, and a hub whose sequence has not moved can
    /// still have lost something new — but the SAME loss reaching this graph twice, through two
    /// cursors, is one event and buys one admission, however many other losses arrive between
    /// the two deliveries.
    ///
    /// Bounded by `loss_horizon`: a loss the hub can no longer deliver cannot arrive again, so
    /// it is forgotten, and what stays is at most what the hub still holds plus the loss just
    /// recorded.
    acted_losses: Vec<u64>,
    /// The horizon read last by the hub's issue count. Each horizon stays true once read — a
    /// loss outside it never comes back — so the latest one says everything the older ones did.
    loss_horizon: Option<LossHorizon>,
    /// Healings this graph has measured. Each is its own authority; the count is what keeps a
    /// repeat of the same level from being mistaken for another one.
    healings: u64,
    /// Origins issued to measured recovery improvements. Its own sequence, because a healing
    /// writes nothing a hub could number and the graph must still tell one from the next.
    recovery_seq: u64,
    /// The highest recovery origin a build claim has captured. One number for the whole
    /// ledger: an improvement above it has not been paid for, whatever it is about.
    recovery_spent: u64,
    /// The highest recovery origin an installed publication has accounted for. Monotone, and
    /// apart from the spent line: spending happens at the admission, answering at the install.
    recovery_answered: u64,
    /// The probe that holds the walk, if one is out, with the basis it is measuring against.
    ///
    /// Held by the LEDGER rather than by the episode: a consumer doing I/O outlives the
    /// publication that sent it, and an owner forgotten when an episode closed let a second
    /// walker open the same tree beside the first.
    probe_owner: Option<(u64, RecoveryBasis)>,
    /// Episodes opened, and probe reservations issued. Monotone names, never a count of what
    /// is remembered.
    episodes: u64,
    probes: u64,
    /// Obligation occurrences issued. A path retired by proof and later genuinely unread again
    /// gets a NEW occurrence, so a proof prepared about the old one cannot retire the new —
    /// and no record of retired names has to be kept to know it.
    occurrences: u64,
    /// The marks demand that paid for the admission now in flight, when marks paid for it.
    ///
    /// The marks account is ONE schedule, so a placement arriving while that build runs joins
    /// it. Without this, an outcome closing "the marks account" closed a demand the build was
    /// never admitted for and nobody has served — the same thing the primary lane refuses
    /// when it looks for a credit nobody spent.
    marks_paid: Option<(i64, u64)>,
    /// The same line for forced demands, kept apart: an ordinary publication answers a change
    /// without re-reading the project, so it may not retire a forced demand.
    answered_forced: u64,
    /// The highest fact whose admission credit this graph has already SPENT.
    ///
    /// The rule it enforces, and the one a global boolean could not: one external fact buys
    /// one first attempt. A fact above this line is an unused credit and may open a new epoch;
    /// a fact at or below it has already bought one, and no later failure gets to spend it
    /// again. The line moves where the credit is actually used — at the admission — not when
    /// the fact arrives and not when something succeeds.
    primary_spent: u64,
    pub(super) marks: MarkLedger,
}

impl GraphDebt {
    /// A change of the scan universe was delivered under `fact`.
    pub(super) fn record_change(&mut self, now: Instant, fact: u64) {
        if fact != NO_FACT_STREAM && fact <= self.answered_fact {
            // Already answered by a proof. The delivery is a repeat — a second cursor carrying
            // the same hub fact — and repeating it records no debt and buys no admission.
            return;
        }
        self.change = Some(self.change.map_or(fact, |current| current.max(fact)));
        self.revive_on_fresh_work(now, fact);
    }

    /// Where the fact stream stands, observed rather than delivered.
    ///
    /// It records the same debt a delivery would — a build that scanned earlier has not seen
    /// this far, so a comparison is owed — and it issues NO credit: nothing external happened,
    /// and a budget that a mere observation could reopen is not a budget.
    pub(super) fn observe_current_level(&mut self, level: u64) {
        self.observed_level = Some(self.observed_level.map_or(level, |current| current.max(level)));
    }

    /// The comparison this graph owes: a delivery, or merely a level somebody observed.
    fn comparison_owed(&self) -> Option<u64> {
        match (self.change, self.observed_level) {
            (Some(fact), Some(level)) => Some(fact.max(level)),
            (fact, level) => fact.or(level),
        }
    }

    /// A change no fingerprint comparison can answer was delivered under `fact`.
    pub(super) fn record_forced(&mut self, now: Instant, fact: u64) {
        if fact != NO_FACT_STREAM && fact <= self.answered_forced {
            // Answered by a build that actually ran forced. A repeat of it is the consumer
            // saying the same thing twice, not the project changing twice.
            return;
        }
        self.forced = Some(self.forced.map_or(fact, |current| current.max(fact)));
        self.revive_on_fresh_work(now, fact);
    }

    /// Whether the MARKS account is itself entitled to work right now — its own window open,
    /// its own deadline unexpired — asked without reference to who happens to hold the slot.
    ///
    /// `Behind(Failed)` says only that a live retry owns the slot. It says nothing about
    /// whether the marks may still buy work of their own, and reading it as though it did let
    /// a marks demand whose budget had run out go on turning every retry into a forced project
    /// reload for as long as the retry lane stayed alive.
    #[cfg(test)]
    pub(super) fn marks_are_eligible(&self, now: Instant) -> bool {
        self.marks_eligible(now)
    }

    /// How many acted-on losses the ledger remembers.
    #[cfg(test)]
    pub(super) fn losses_remembered(&self) -> usize {
        self.acted_losses.len()
    }

    /// Which losses those are, by identity — what a stand asserting "this loss, once" needs and
    /// a count cannot say.
    #[cfg(test)]
    pub(super) fn acted_losses(&self) -> Vec<u64> {
        self.acted_losses.clone()
    }

    /// The highest fact a forced build has answered.
    #[cfg(test)]
    pub(super) fn answered_forced(&self) -> u64 {
        self.answered_forced
    }

    fn marks_eligible(&self, now: Instant) -> bool {
        self.marks.owed.as_ref().is_some_and(|owed| owed.is_open(now))
    }

    /// The hub declared a loss of detail: anything may have changed, including the project.
    ///
    /// Authority of its own, and it has to be: the fact number stands still while the detail
    /// goes, so addressing this by the sequence would make a real new loss indistinguishable
    /// from a repeat of an old one — and, after an answer, silently unable to buy the reload
    /// it exists to ask for. `token` is the loss's identity, so the same loss arriving through
    /// a second cursor is one event.
    pub(super) fn record_loss(
        &mut self,
        now: Instant,
        token: Option<u64>,
        observation: u64,
        horizon: Option<LossHorizon>,
    ) {
        if let Some(token) = token {
            match horizon {
                Some(horizon) => {
                    if self.loss_horizon.as_ref().is_none_or(|kept| horizon.issued >= kept.issued) {
                        self.loss_horizon = Some(horizon);
                    }
                }
                // No hub to ask what it can still deliver: nothing proves an older loss is out
                // of reach, and nothing may be kept for ever, so only the latest is remembered.
                None => self.acted_losses.retain(|acted| *acted == token),
            }
            if let Some(kept) = &self.loss_horizon {
                self.acted_losses.retain(|acted| kept.may_deliver(*acted));
            }
            if self.acted_losses.contains(&token) {
                return;
            }
            self.acted_losses.push(token);
        }
        self.forced = Some(self.forced.map_or(observation, |current| current.max(observation)));
        if let Some(failed) = self.failed.as_mut() {
            failed.schedule.revive(now, Duration::ZERO);
        }
        if let Some(owed) = self.marks.owed.as_mut() {
            owed.revive(now, OWED_MARKS_GRACE);
        }
        if let Some(recovery) = self.recovery.as_mut() {
            recovery.interval = RECOVERY_PROBE;
            recovery.flapping = false;
            recovery.next_probe = recovery.next_probe.min(now + RECOVERY_PROBE);
        }
    }

    /// A capability the probe MEASURED coming back. Authority of its own, and the contract
    /// rests on it being exactly that.
    ///
    /// It cannot be addressed by a hub number. Nothing was written — a restored permission and
    /// a subtree that became readable move no sequence at all — so a number taken from the
    /// fact stream is the one the last admission already spent, and spending it again means
    /// the one event that could revive the budget arrives already spent. `origin` is this
    /// improvement's OWN identity, issued by the recovery ledger and belonging to no other
    /// lane. It is also not a delivery: the probe's earned pace and its flap latch belong to
    /// the probe's schedule, and a healing is news about the disk, not a reason to start
    /// paying for probes at the floor.
    fn credit_recovery(&mut self, now: Instant, origin: u64) {
        self.healings = self.healings.saturating_add(1);
        let _ = origin;
        if let Some(failed) = self.failed.as_mut() {
            failed.schedule.revive(now, Duration::ZERO);
        }
        if let Some(owed) = self.marks.owed.as_mut() {
            owed.revive(now, OWED_MARKS_GRACE);
        }
    }

    /// Whether `fact` is authority nobody has used and no proof has discharged.
    fn unused_credit(&self, fact: u64) -> bool {
        fact > self.primary_spent && fact > self.answered_fact
    }

    /// An admission is taken: the credit that justified it is spent here, once, and cannot
    /// justify a second first attempt. Called with the ticket the claim fixed, so what is
    /// charged is what the admission was actually granted for.
    pub(super) fn charge_admission(
        &mut self,
        sponsor_fact: u64,
        now: Instant,
        forced: bool,
    ) -> Sponsors {
        // A standing demand, a retry whose budget is still open, or marks entitled to work.
        // Asked HERE, at the line, and not only where the decision was taken: the walk before
        // this point can run for seconds, and a deadline that passes inside it takes the only
        // sponsor with it. Equality is exhausted, so a claim at the deadline is not a last
        // free attempt.
        let primary = match self.failed.as_ref() {
            // A retry lane exists, so it is what pays for this attempt — and a lane whose
            // budget has run out pays nothing. The drift the caller just measured is the very
            // thing that lane was already about; it is not a second, fresh justification.
            Some(failed) => failed.schedule.is_open(now),
            // No retry lane. The marks are a lane of their own, so a claim only THEY asked
            // for is charged to them and names nobody else: naming the primary lane beside
            // them handed the outcome to an account that financed nothing — a marks-only
            // failure minted a fresh primary retry budget, and its Operation closed the wrong
            // book. Anything else that reaches the grant IS the primary lane asking: a
            // delivered change, a forced demand, an unpaid recovery improvement, a first
            // build, or drift the caller measured against the publication itself.
            None => {
                let marks_alone = self.marks_eligible(now)
                    && self.change.is_none()
                    && self.forced.is_none()
                    && !self.unused_recovery();
                !marks_alone
            }
        };
        let sponsors = Sponsors { primary, marks: self.marks_eligible(now) };
        if !sponsors.any() {
            return sponsors;
        }
        self.primary_spent = self.primary_spent.max(sponsor_fact);
        self.marks_paid = sponsors.marks.then_some(self.marks.demanded).flatten();
        if sponsors.marks && forced {
            // An attachment pays on the claim, in grace or not: joining a build is what the
            // marks' budget is FOR, and skipping the charge while their first pause had not
            // elapsed left an account whose deadline was never opened at all.
            self.open_mark_attempt(now);
        }
        sponsors
    }

    /// A delivered change is the fresh external work every spent budget waits for. A failure
    /// is not, and neither is a wake: that separation is what keeps one drift from rebuilding
    /// a failing graph for ever.
    fn revive_on_fresh_work(&mut self, now: Instant, fact: u64) {
        // A spent budget waits for work nobody has paid with yet. The fact that STARTS a build
        // pays for that build's admission and for nothing after it — which is why the frontier
        // moves at the claim, and why a failure, a success or a repeat delivery cannot move it
        // back.
        if !self.unused_credit(fact) {
            // Spent on an admission, or answered by a proof. Either way it is not authority,
            // and it may not revive a budget, reset a probe's pace or re-open a marks window.
            return;
        }
        if let Some(failed) = self.failed.as_mut() {
            failed.schedule.revive(now, Duration::ZERO);
        }
        if let Some(recovery) = self.recovery.as_mut() {
            recovery.interval = RECOVERY_PROBE;
            recovery.flapping = false;
            recovery.next_probe = recovery.next_probe.min(now + RECOVERY_PROBE);
        }
        // A delivered fact means a build is owed anyway, so the marks it would consume get a
        // fresh attempt window too. Without this their exhaustion would name work that, for a
        // consumer placing no further marks, never arrives.
        if let Some(owed) = self.marks.owed.as_mut() {
            owed.revive(now, OWED_MARKS_GRACE);
        }
    }

    /// Says whether a standing change opened a new epoch for the failed build: the caller
    /// starts it at once then, because nothing else will deliver that change again.
    pub(super) fn record_failure(
        &mut self,
        now: Instant,
        kind: FailureKind,
        sponsors: Sponsors,
    ) -> bool {
        if kind == FailureKind::Terminal {
            self.failed = None;
            return false;
        }
        if sponsors.marks {
            // The marks financed this claim, alone or beside the primary lane, so the outcome
            // spends THEIR account too: an Operation closes it, and a transient one is paced
            // by it. A joint claim that skipped this left the account that had already bought
            // this build open to buy it again at once.
            //
            // Their account, but not necessarily only their demand: a placement that arrived
            // while the build ran joined the same schedule, and this build was not admitted
            // for it. A failure buys nothing — so what the outcome spends is the demand that
            // paid, and anything asked for after it is still owed.
            let later_demand = self.marks.demanded != self.marks_paid;
            if let Some(owed) = self.marks.owed.as_mut() {
                if kind == FailureKind::Operation {
                    owed.operation_error();
                } else {
                    let delay = owed.delay().max(OWED_MARKS_GRACE);
                    owed.attempt(now, delay);
                }
                if later_demand {
                    owed.revive(now, OWED_MARKS_GRACE);
                }
            }
            if !sponsors.primary {
                // Sponsored by marks alone, so the outcome belongs to their account and to no
                // other. Minting a primary retry budget here would give a lane that never paid
                // a fresh 600 seconds of its own.
                return false;
            }
        }
        let failed = self
            .failed
            .get_or_insert_with(|| Failed { kind, schedule: Schedule::new(now, Duration::ZERO) });
        failed.kind = kind;
        let operation = kind == FailureKind::Operation;
        if operation {
            failed.schedule.operation_error();
        } else {
            // Counted as one refusal, and paced by the schedule every other retry owner
            // shares: the first retry is immediate, and each further failure earns the next
            // step of the backoff.
            let delay = failed.schedule.delay();
            failed.schedule.attempt(now, delay);
        }
        // A failure buys nothing; it ends the epoch that was running. What may open the NEXT
        // one is a credit nobody has spent — a fact delivered after this build was admitted,
        // which this build therefore cannot have answered. The fact that paid for this very
        // admission is at or below the frontier and has nothing left to give, which is what
        // keeps a workspace that cannot build from rebuilding for ever.
        let pending = self.change.into_iter().chain(self.forced).max();
        let unspent =
            pending.is_some_and(|fact| self.unused_credit(fact)) || self.unused_recovery();
        if !unspent {
            return false;
        }
        self.failed.as_mut().is_some_and(|failed| failed.schedule.revive(now, Duration::ZERO))
    }

    /// A publication landed. `observed_through` is the hub position its scan is good for and
    /// `forced` says it ran as a forced project reload.
    ///
    /// Whether it can vouch for itself is read off the PROOF, not off a boolean beside it: the
    /// caller's convenient answer came from the lenient unread reader, where a database that
    /// would not open reads as a database with nothing left to read.
    pub(super) fn record_publication(
        &mut self,
        now: Instant,
        observed_through: Option<u64>,
        forced: bool,
        recovery_through: Option<u64>,
        proof: RecoveryPublicationProof,
    ) -> RetiredPayload {
        if let Some(observed) = observed_through {
            // The proof's own line. Demands at or below it are answered — and stay answered,
            // so a later repeat of the same delivery cannot record them again.
            //
            // Not drawn over the no-hub sentinel: there every delivery reads as the same
            // number, so a line there would answer everything that ever arrives. Such a graph
            // is bounded by the spent frontier alone, which the admission still moves.
            if observed != NO_FACT_STREAM {
                self.answered_fact = self.answered_fact.max(observed);
            }
            if self.change.is_some_and(|fact| fact <= observed) {
                self.change = None;
            }
            if self.observed_level.is_some_and(|level| level <= observed) {
                self.observed_level = None;
            }
            if forced {
                if observed != NO_FACT_STREAM {
                    self.answered_forced = self.answered_forced.max(observed);
                }
                if self.forced.is_some_and(|fact| fact <= observed) {
                    self.forced = None;
                }
            }
        }
        if let Some(through) = recovery_through {
            // The build this publication came from was admitted to answer every recovery
            // origin up to its cutoff. An improvement measured after that claim is not
            // answered by it and keeps its own demand.
            self.recovery_answered = self.recovery_answered.max(through);
        }
        self.failed = None;
        self.apply_recovery_publication(now, proof)
    }

    /// What an installed publication does to the recovery ledger.
    ///
    /// Only an installed, non-stale coherent proof ends an episode. A failure does not, an
    /// unsound publication does not, and neither does a new database generation: none of them
    /// heals a disk, and forgetting what was measured is what turned one healing into a fresh
    /// credit on every pass.
    fn apply_recovery_publication(
        &mut self,
        now: Instant,
        proof: RecoveryPublicationProof,
    ) -> RetiredPayload {
        // What this artefact can speak for at all. A build whose tree moved under it, and one
        // whose unread metadata would not read, have no authority over what is outstanding —
        // the strict reader says so by returning nothing at all, where the lenient one
        // returned an empty list and answered everything.
        let vouched = !proof.straddled && proof.declared_unread.is_some();
        let declares_gaps = proof.declared_unread.as_ref().is_some_and(|unread| !unread.is_empty());
        let owes_validation = match proof.scan_complete {
            Some(complete) => !complete || proof.straddled,
            None => false,
        };
        if self.recovery.is_none() && !declares_gaps && !owes_validation {
            // Nothing to own. An episode is opened by an actual gap, not by every publication
            // that cannot call itself perfect.
            return RetiredPayload { keys: Vec::new(), proof: Some(proof) };
        }
        let opening = self.recovery.is_none();
        if opening {
            self.episodes = self.episodes.saturating_add(1);
            let episode = self.episodes;
            self.recovery = Some(Recovery::opened(now, episode, proof.generation));
        }
        let mut occurrences = self.occurrences;
        let mut retired = RetiredPayload::default();
        let Some(recovery) = self.recovery.as_mut() else {
            retired.proof = Some(proof);
            return retired;
        };
        if !opening {
            // The same publication is unsound again: the probe that started this build does
            // not get to start another on the same schedule, and a request may not pull it
            // forward either.
            recovery.flapping = true;
            recovery.interval = (recovery.interval * 2).min(RECOVERY_PROBE_CAP);
            recovery.next_probe = now + recovery.interval;
        }

        // What this publication itself could not read. An address on that list is required BY
        // this very artefact, so no proof travelling beside it may retire it: removing the
        // cell and letting the list below put it back would make an unchanged file first
        // knowledge again, and buy a build for it on every pass.
        let still_unread: std::collections::BTreeSet<&str> = proof
            .declared_unread
            .iter()
            .flat_map(|declared| declared.iter().map(String::as_str))
            .collect();

        // Retirement first, and only by proof about the occurrence that stands. A key whose
        // improvement was measured AFTER this proof was prepared is not answered by it.
        //
        // And only from an artefact that could say what it read at all: without the strict
        // list there is no authority over membership, so whatever else this publication
        // carries retires nothing. The producer refuses to build such vectors; the ledger
        // refuses to apply them, because the authority is the same fact in both places.
        for (key, occurrence) in proof.declared_unread.iter().flat_map(|_| {
            proof
                .read_covered
                .iter()
                .chain(proof.absent_covered.iter())
                .chain(proof.out_of_scope_covered.iter())
        }) {
            if still_unread.contains(key.as_str()) {
                continue;
            }
            let answered = recovery.required.get(key).is_some_and(|cell| {
                cell.occurrence == *occurrence
                    && cell.pending.is_none_or(|origin| origin <= proof.captured_seq)
            });
            if answered {
                // Unhooked from the map here, thrown away by the caller: the nodes and the
                // strings of an answered obligation are not the publication gate's work.
                if let Some((key, _)) = recovery.required.remove_entry(key.as_str()) {
                    retired.keys.push(key);
                }
            }
        }

        // Membership: an authoritative unread set adds what it names. It never shrinks P by
        // omission — an enumeration that came up short has answered nothing, and a path it
        // failed to mention is still owed an observation.
        if let Some(declared) = &proof.declared_unread {
            for key in declared {
                if !recovery.required.contains_key(key) {
                    occurrences = occurrences.saturating_add(1);
                    recovery.required.insert(
                        key.clone(),
                        Cell { level: Level::Unknown, occurrence: occurrences, pending: None },
                    );
                }
            }
        }

        let mut proof = proof;
        if let Some(scope) = proof.scope.take() {
            // The walk obligation belongs to the scope that stands. A different composition is
            // a different capability, so its verdict starts unmeasured rather than inheriting
            // the last one.
            match (proof.scan_complete, recovery.scan.as_mut()) {
                // Complete AND vouched for. A build whose tree moved under it enumerated a
                // world that has already been replaced: however whole that walk was, it is
                // not a validation of the one that stands now.
                (Some(true), _) if !proof.straddled => recovery.scan = None,
                (Some(_), Some(cell)) if cell.scope.same_composition(&scope) => {}
                (Some(_), _) => {
                    recovery.scan = Some(ScanCell {
                        scope: scope.clone(),
                        level: Level::Unknown,
                        pending: None,
                    })
                }
                (None, _) => {}
            }
            // A publication that served the composition a walk measured answers the origin
            // that measured it. Its own generation answers nothing: what closes the demand is
            // having built the declaration, not having been published after it.
            let served = recovery
                .observed_scope
                .as_ref()
                .is_some_and(|observed| observed.same_composition(&scope));
            if served
                && recovery.composition_pending.is_some_and(|origin| origin <= proof.captured_seq)
            {
                recovery.composition_pending = None;
            }
            recovery.installed_scope = Some(scope);
        }
        recovery.generation = proof.generation;
        recovery.revision = recovery.revision.saturating_add(1);
        self.occurrences = occurrences;
        if vouched && !recovery.outstanding() {
            // Every remaining obligation is answered by a publication that can vouch for
            // itself: the chain ends here. Not because a count came back zero — because
            // nothing is left that a walk could still be owed.
            self.recovery = None;
        }
        retired.proof = Some(proof);
        retired
    }

    /// Marks up to `mark_high` were placed for facts up to `fact`.
    pub(super) fn place_marks(&mut self, now: Instant, mark_high: i64, fact: u64) {
        let fresh = self.marks.place(mark_high, fact);
        if let Some(owed) = self.marks.owed.as_mut() {
            // Marks actually placed ANEW are the external work a spent obligation waits for.
            // A repeat of a demand already on the books is not: it names no work that was not
            // already named, and letting it revive turned one quiet consumer into an endless
            // supply of admissions.
            if fresh {
                owed.revive(now, OWED_MARKS_GRACE);
            }
        }
    }

    /// Decide what the ledger is owed now that a publication (or a placement) has settled:
    /// marks whose fact no publication has observed need a build, and marks nobody is waiting
    /// on need nothing. Says whether a new obligation was armed.
    #[cfg(test)]
    pub(super) fn settle_marks(&mut self, now: Instant, in_flight: bool) -> bool {
        self.settle_marks_through(now, in_flight, None)
    }

    pub(super) fn settle_marks_through(
        &mut self,
        now: Instant,
        in_flight: bool,
        observed: Option<u64>,
    ) -> bool {
        if !self.marks.has_placed() {
            self.marks.owed = None;
            return false;
        }
        if !self.marks.has_uncovered(observed) {
            self.marks.owed = None;
            return false;
        }
        if in_flight || self.marks.owed.is_some() {
            return false;
        }
        self.marks.owed = Some(Schedule::new(now, OWED_MARKS_GRACE));
        true
    }

    /// The hook could not run what a publication asked of it.
    pub(super) fn record_hook(&mut self, unhandled: HookDebt) {
        if !unhandled.any() {
            return;
        }
        self.hook.merge(unhandled);
        // New bits are a new offer: a flush already in flight was given the old one and may
        // not report these as taken. The revisit pause belongs to the refusal that earned it,
        // not to the work that arrived after it.
        self.hook_revision = self.hook_revision.saturating_add(1);
        self.hook_revisit = None;
    }

    /// The offer a flush is entitled to answer for.
    pub(super) fn claim_hook(&self) -> (u64, HookDebt) {
        (self.hook_revision, self.hook)
    }

    /// The hook took what it was offered — of the revision it was offered.
    pub(super) fn hook_handled(&mut self, revision: u64, handled: HookDebt) {
        if revision != self.hook_revision {
            return;
        }
        self.hook.topology &= !handled.topology;
        self.hook.roots &= !handled.roots;
        self.hook.marks &= !handled.marks;
        if !self.hook.any() {
            self.hook_revisit = None;
        }
    }

    /// The hook was offered and took nothing. Paced, so a consumer that refuses for as long as
    /// it lives cannot be asked again on every turn of the executor.
    pub(super) fn hook_refused(&mut self, now: Instant, revision: u64) {
        if revision == self.hook_revision {
            self.hook_revisit = Some(now + HOOK_REVISIT);
        }
    }

    /// The offer a publication made was refused whole. Paces what that refusal left on the
    /// books — whatever revision it now stands at, because the publication's own record is
    /// what raised it.
    pub(super) fn pace_hook_refusal(&mut self, now: Instant) {
        if self.hook.any() {
            let delay = if self.hook.marks { OWED_MARKS_GRACE } else { HOOK_REVISIT };
            self.hook_revisit = Some(now + delay);
        }
    }

    #[cfg(test)]
    pub(super) fn hook_debt(&self) -> HookDebt {
        self.hook
    }

    /// The lease could not be confirmed: nothing is dropped, the decision waits.
    pub(super) fn hold(&mut self, now: Instant) {
        self.held_until = Some(now + HELD_RETRY);
    }

    /// A request asked about the graph. The only thing it may do is pull a recovery probe
    /// forward — it reads no disk itself — and only while the probe is not pacing a flap.
    pub(super) fn note_request(&mut self, now: Instant) -> bool {
        let Some(recovery) = self.recovery.as_mut() else { return false };
        if recovery.flapping || recovery.interval <= RECOVERY_PROBE {
            return false;
        }
        let pulled = now + RECOVERY_PROBE;
        if pulled >= recovery.next_probe {
            // The probe is already due sooner than a request could ask for. Nothing is pulled,
            // so nothing is spent — and the BACKOFF is not reset either. Resetting it here made
            // an agent polling the graph hold the interval at its floor for ever: each request
            // put it back to a minute, and the failing probe that followed doubled from there
            // instead of from where the backoff had climbed to. A probe that heals nothing is
            // supposed to get cheaper, and a client asking about the graph is not news about
            // the disk.
            return false;
        }
        // Only the MOMENT moves. The interval is left exactly where the probing earned it: it
        // is the one thing here that must not depend on how often anybody asks.
        recovery.next_probe = pulled;
        true
    }

    /// A probe could not look at all — the lease was held, the database would not open. No
    /// observation was made, so nothing is recorded about the workspace; but the attempt must
    /// still be paced, or the watcher asks again on every turn of its slice for as long as the
    /// obstacle lasts. Paced WITHOUT doubling: the interval describes how often a healing is
    /// worth looking for, and nothing was learned about that.
    pub(super) fn probe_could_not_look(&mut self, now: Instant) {
        if let Some(recovery) = self.recovery.as_mut() {
            recovery.next_probe = now + recovery.interval;
        }
    }

    /// Take the episode's one probe reservation, with the basis and the whole outstanding set
    /// to observe.
    ///
    /// One walker: a second caller reaching here while a walk is out finds the work owned and
    /// leaves it alone, instead of opening the same tree beside it. The plan carries P, not
    /// the newest unread list — an obligation the latest enumeration did not mention still has
    /// to be looked at, or it would keep its memory and lose its observer.
    pub(super) fn reserve_probe(&mut self) -> Option<ProbePlan> {
        if self.probe_owner.is_some() {
            // A walk is out. Not "a walk of this episode": the consumer holding it is still
            // opening files, and an episode that closed underneath it does not end its I/O.
            return None;
        }
        let token = self.probes.saturating_add(1);
        let recovery = self.recovery.as_mut()?;
        if !recovery.outstanding() {
            return None;
        }
        let basis = recovery.basis();
        let plan = ProbePlan {
            token,
            basis,
            open: recovery.required.keys().cloned().collect(),
            scan: recovery.scan.as_ref().map(|cell| cell.scope.clone()),
        };
        self.probe_owner = Some((token, basis));
        self.probes = token;
        Some(plan)
    }

    /// Give a reservation back without a measurement — the walk could not start, or it could
    /// not look. Only the holder's own token is released: a walker that has already been
    /// superseded must not clear the reservation of the one that replaced it.
    pub(super) fn release_probe(&mut self, now: Instant, token: u64, looked: bool) {
        if self.probe_owner.map(|(owner, _)| owner) != Some(token) {
            return;
        }
        self.probe_owner = None;
        if !looked {
            self.probe_could_not_look(now);
        }
    }

    /// A probe came back. The whole receipt is checked against the basis it was measured
    /// against before a single name of it is believed.
    ///
    /// Rejected WHOLE, and that is the point: a receipt from a replaced publication describes
    /// gaps that publication had. Its negatives must not overwrite a level measured since, and
    /// its names — including ones this memory has never seen — must not enter the required set
    /// at all. Only an authoritative publication says what is required.
    pub(super) fn finish_probe(&mut self, now: Instant, receipt: ProbeReceipt) -> ProbeResult {
        let seq_before = self.recovery_seq;
        if self.probe_owner.map(|(owner, _)| owner) != Some(receipt.token) {
            // Somebody else's walk, or one this owner already finished. It holds nothing to
            // give back and it has nothing to say.
            return ProbeResult::Obsolete;
        }
        // Given back here whatever the receipt turns out to be worth: a walk that measured a
        // world since replaced still stops being the walk that is out.
        self.probe_owner = None;
        let Some(recovery) = self.recovery.as_mut() else { return ProbeResult::Obsolete };
        if recovery.basis() != receipt.basis {
            // The whole batch, every name in it, and no change to the pace beyond a finite
            // revisit: this walk measured a world that has been replaced.
            recovery.next_probe = now + recovery.interval;
            return ProbeResult::Obsolete;
        }
        let mut seq = seq_before;
        let mut improved = false;
        for (capability, level) in receipt.levels {
            match capability {
                Capability::Open(key) => {
                    let Some(cell) = recovery.required.get_mut(&key) else {
                        // Outside P. A receipt cannot enlarge what is required: membership
                        // comes from an authoritative build gap, never from a walk.
                        continue;
                    };
                    if improves(cell.level, level) {
                        seq += 1;
                        cell.pending = Some(seq);
                        improved = true;
                    }
                    cell.level = level;
                }
                Capability::ScanRoots => {
                    let Some(cell) = recovery.scan.as_mut() else { continue };
                    // The scope is part of the measurement, not a separate lookup: a verdict
                    // read from one place and roots from another can pair a composition with
                    // a walk that never covered it.
                    let Some(walked) = receipt.scope.as_ref() else { continue };
                    if walked.same_composition(&cell.scope) {
                        if improves(cell.level, level) {
                            seq += 1;
                            cell.pending = Some(seq);
                            improved = true;
                        }
                        cell.level = level;
                    } else if walked.is_validated() {
                        // The declaration that stands is not the one this cell was measuring.
                        // There is still exactly ONE walk obligation — what it is a walk OF
                        // has changed — so the cell measures the composition that stands now,
                        // starting from what this very receipt measured of it.
                        //
                        // The old verdict is DROPPED rather than carried across: it was about
                        // other roots. And no credit is issued here, because the change of
                        // composition is itself the news and is credited once below; what
                        // this leaves behind is a level to improve on, which is how the same
                        // subtree becoming readable later is a healing of its own instead of
                        // a verdict nobody would look at again.
                        cell.scope = walked.clone();
                        cell.level = level;
                        cell.pending = None;
                    }
                }
            }
        }
        if let Some(walked) = receipt.scope {
            // A genuinely different composition is external news of its own — including a
            // return to a scope seen before, which is a new transition and not a resurrected
            // witness.
            // Against the last walk, or — for the FIRST walk of an episode — against the
            // declaration the publication was built from. Compared with nothing, a project
            // whose roots had been replaced measured a brand new tree and reported that
            // nothing had happened.
            let changed = recovery
                .observed_scope
                .as_ref()
                .or(recovery.installed_scope.as_ref())
                .is_some_and(|last| !last.same_composition(&walked));
            recovery.observed_scope = Some(walked);
            if changed {
                seq += 1;
                recovery.composition_pending = Some(seq);
                improved = true;
            }
        }
        self.recovery_seq = seq;
        let Some(recovery) = self.recovery.as_mut() else { return ProbeResult::NoNewEvidence };
        if improved {
            // A measured transition, and the only thing that buys a build here. The pace it
            // earned is kept: a healing is news, not a reason to start paying for probes at
            // the floor again.
            recovery.next_probe = now + recovery.interval;
            self.credit_recovery(now, seq);
            return ProbeResult::NewEvidence;
        }
        recovery.interval = (recovery.interval * 2).min(RECOVERY_PROBE_CAP);
        recovery.next_probe = now + recovery.interval;
        ProbeResult::NoNewEvidence
    }

    /// The obligations a publication being prepared may speak for, and the origin frontier it
    /// is being prepared against.
    pub(super) fn outstanding_recovery(&self) -> OutstandingRecovery {
        OutstandingRecovery {
            keys: self
                .recovery
                .as_ref()
                .map(|recovery| {
                    recovery
                        .required
                        .iter()
                        .map(|(key, cell)| (key.clone(), cell.occurrence))
                        .collect()
                })
                .unwrap_or_default(),
            captured_seq: self.recovery_seq,
        }
    }

    /// A build claim captures every recovery origin issued so far. Spending is one number, so
    /// this costs nothing per obligation — and an improvement measured AFTER it keeps an
    /// origin above the line, which is what makes post-claim work survive this ticket's
    /// outcome.
    pub(super) fn capture_recovery(&mut self) -> u64 {
        self.recovery_spent = self.recovery_spent.max(self.recovery_seq);
        self.recovery_spent
    }

    /// Whether a forced project reload is owed at all: by a hub fact, or by a measured
    /// recovery improvement that has no hub number of its own.
    fn forced_demanded(&self) -> bool {
        self.forced.is_some() || self.recovery_owed().is_some()
    }

    /// The highest measured improvement owed a forced build: an origin still attached to an
    /// outstanding obligation that no installed proof has accounted for.
    fn recovery_owed(&self) -> Option<u64> {
        self.recovery
            .as_ref()
            .and_then(Recovery::owed_origin)
            .filter(|origin| *origin > self.recovery_answered)
    }

    /// Whether a measured recovery improvement is owed a build at all. It has no hub fact
    /// behind it — nothing was written — so the mode cannot be read off one.
    pub(super) fn owes_recovery_build(&self) -> bool {
        self.recovery_owed().is_some()
    }

    /// Whether a measured recovery improvement is owed a build that no claim has paid for.
    fn unused_recovery(&self) -> bool {
        self.recovery_owed().is_some_and(|origin| origin > self.recovery_spent)
    }

    /// The fact a forced build is owed to, if one is owed. A build captures it before it
    /// reads disk and carries it to its publication, which is what discharges the debt.
    #[cfg(test)]
    pub(super) fn owes_forced_fact(&self) -> Option<u64> {
        self.forced
    }

    pub(super) fn forced_fact(&self) -> Option<u64> {
        self.forced
    }

    /// A forced build owed to marks is starting: its own schedule moves on, so a build that
    /// keeps failing to consume them backs off instead of running back to back.
    /// Charge the marks for an attempt they are joining, whether or not their own pause has
    /// elapsed: the attachment is the work their budget buys.
    pub(super) fn open_mark_attempt(&mut self, now: Instant) {
        if let Some(owed) = self.marks.owed.as_mut() {
            let delay = crate::state::overlay_retry::retry_delay(owed.attempts.saturating_add(1))
                .max(OWED_MARKS_GRACE);
            owed.next_allowed = owed.next_allowed.min(now);
            owed.attempt(now, delay);
        }
    }

    pub(super) fn spend_mark_attempt(&mut self, now: Instant) {
        if let Some(owed) = self.marks.owed.as_mut() {
            if owed.next_allowed <= now {
                let delay =
                    crate::state::overlay_retry::retry_delay(owed.attempts.saturating_add(1))
                        .max(OWED_MARKS_GRACE);
                owed.attempt(now, delay);
            }
        }
    }

    /// A comparison answered the delivered change: disk still matches the published build, and
    /// the comparison covered every fact up to `checked_through`.
    pub(super) fn change_answered(&mut self, checked_through: u64) {
        if checked_through != NO_FACT_STREAM {
            self.answered_fact = self.answered_fact.max(checked_through);
        }
        if self.change.is_some_and(|fact| fact <= checked_through) {
            self.change = None;
        }
        if self.observed_level.is_some_and(|level| level <= checked_through) {
            self.observed_level = None;
        }
    }

    /// The same comparison answers a failed build: the publication on record describes the
    /// disk, so the build the failure owed has nothing left to do.
    ///
    /// Kept apart from [`Self::record_publication`], which discharges a failure by BUILDING.
    /// Here the retry ran and found nothing to build, and a debt left open behind an elapsed
    /// schedule is a standing order: the decision picks it again at once, every wake, for ever
    /// — a walk of the whole tree on every pass and a graph that reads stale while it is
    /// perfectly current.
    pub(super) fn failure_answered(&mut self) {
        self.failed = None;
    }

    /// The workspace is gone: nothing is owed, and nothing ever will be again.
    pub(super) fn abandon(&mut self) {
        self.change = None;
        self.forced = None;
        self.failed = None;
        self.recovery = None;
        self.probe_owner = None;
        self.hook = HookDebt::default();
        self.hook_revisit = None;
        self.held_until = None;
        self.marks.owed = None;
    }

    /// Whether the graph is behind by its own account: some debt is open, or a build that will
    /// answer one is running. The publication's own flags are added by the caller, which is
    /// where they live.
    pub(super) fn stale(&self) -> bool {
        self.comparison_owed().is_some()
            || self.forced_demanded()
            || self.failed.is_some()
            || self.recovery.is_some()
            || self.marks.owed.is_some()
    }

    /// What every debt says about itself at `now`. Pure, allocation-free, and the single
    /// account of the schedule: the decision picks work from it and the alarm projects it.
    pub(super) fn standing(&self, now: Instant, facts: Facts) -> Standing {
        let mut standing = Standing::default();
        // Nothing is anybody's to run in these: the workspace is gone, the daemon is leaving, a
        // build already holds the slot, or ownership could not be confirmed.
        if facts.terminal || facts.stopping || facts.in_flight || !facts.owns {
            return standing;
        }
        // Where the graph stands decides what build it could take at all.
        let startable = facts.ready || facts.failed || facts.idle;

        standing.failed = self.failed.as_ref().map(|failed| {
            if startable {
                failed.schedule.ripeness(now, Revival::FreshFact)
            } else {
                Ripeness::Behind(DebtKind::Failed)
            }
        });
        // A LIVE retry holds the single build slot, and the work it runs carries whatever the
        // debts beside it demand. A retry whose budget is spent holds NOTHING: queueing behind
        // it leaves a debt with no executor and no alarm, which is starvation with a name.
        let retry_owns = is_live(standing.failed);
        let queued = || (!startable || retry_owns).then_some(Ripeness::Behind(DebtKind::Failed));
        // A debt with no schedule of its own is bounded by the schedule of the failure that
        // holds the slot: once THAT budget is spent, the fact standing behind it has been
        // tried as often as the graph is allowed to try it, and only a NEWER fact justifies
        // another attempt. Without this the exhaustion bounded nothing — a forced reload is
        // discharged by a publication and by nothing else, so a workspace that could not build
        // rebuilt for ever. The marks are deliberately not treated this way: they carry their
        // own budget, so passing a spent retry costs them an attempt and comes to rest on its
        // own.
        let spent = || {
            matches!(standing.failed, Some(Ripeness::Exhausted(_)))
                .then_some(Ripeness::Exhausted(Revival::FreshFact))
        };
        let unowned = || queued().or_else(spent);

        // A hub fact, or a measured recovery improvement. The second has no number on the
        // fact stream — nothing was written — so a demand read off `forced` alone would lose
        // every healing the probe measures.
        standing.forced = self.forced_demanded().then(|| unowned().unwrap_or(Ripeness::Now));

        standing.marks = self.marks.owed.as_ref().map(|owed| {
            if let Some(behind) = queued() {
                behind
            } else if ripe_now(standing.forced) {
                // The forced build about to run is forced for the marks too.
                Ripeness::Behind(DebtKind::Forced)
            } else {
                // Deliberately not conditioned on `ready`: on an idle or failed graph the first
                // build is what answers the marks, and demanding `ready` here would trade one
                // starvation for another.
                owed.ripeness(now, Revival::FreshMarks)
            }
        });

        standing.change = self.comparison_owed().map(|_| {
            if let Some(behind) = unowned() {
                behind
            } else if ripe_now(standing.forced) {
                Ripeness::Behind(DebtKind::Forced)
            } else if ripe_now(standing.marks) {
                // Only when the marks' own build is starting NOW does it answer the change too.
                // While the marks are merely inside their grace the change goes first — that
                // grace exists to leave room for exactly this build, and making the change wait
                // for it would turn the room into a delay.
                Ripeness::Behind(DebtKind::Marks)
            } else {
                Ripeness::Now
            }
        });

        // An episode with nothing outstanding asks for nothing. It is still an episode — only
        // an installed coherent proof closes one — but every obligation it carried has been
        // answered, and there is nothing left for a walk to look at.
        standing.recovery =
            self.recovery.as_ref().filter(|recovery| recovery.outstanding()).map(|recovery| {
                // A probe's ONLY output is a forced build: it is what proves a healing. So while
                // one is already owed — whatever is holding it up — the probe has nothing to add,
                // and running it anyway walks the whole tree every interval to record a debt that
                // is already recorded. That is what kept a knowingly-stale publication over a
                // readable tree walking once a minute for as long as its builds kept failing.
                //
                // Only behind a forced reload somebody will actually run, though — and "somebody"
                // is resolved through the queue, not read off this one link: a forced demand that
                // is itself `Behind(Failed)` has an owner as surely as one that is due, because
                // the retry holding the slot will run it. What must NOT suppress the probe is a
                // demand whose chain ends on an exhaustion: the probe is the one owner that MAKES
                // the fresh fact a spent budget waits for, and queueing it there trades the hot
                // loop for a wait nothing can end.
                if owner_is_live(&standing, DebtKind::Forced) {
                    return Ripeness::Behind(DebtKind::Forced);
                }
                // Otherwise only behind what is starting this turn: the probe costs one stat walk,
                // takes no build slot, and a retry sleeping out its backoff is no reason to leave
                // an unreadable subtree unexamined.
                for (kind, ripeness) in [
                    (DebtKind::Failed, standing.failed),
                    (DebtKind::Marks, standing.marks),
                    (DebtKind::Change, standing.change),
                ] {
                    if ripe_now(ripeness) {
                        return Ripeness::Behind(kind);
                    }
                }
                if !facts.ready {
                    // Nothing published to probe and no live build owed: only fresh work can move
                    // it, and saying so plainly is the honest answer.
                    return Ripeness::Exhausted(Revival::FreshFact);
                }
                if recovery.next_probe <= now {
                    Ripeness::Now
                } else {
                    // Never `At`: no moment retires this debt, only a healing does.
                    Ripeness::Watching(recovery.next_probe)
                }
            });

        standing.hook = self.hook.any().then(|| {
            if facts.ready {
                // A refusal earns a finite pause, and the pause is a moment of its own: this
                // is the one debt whose owner can decline indefinitely, and offering it again
                // at once is how a turn came to spend its whole quantum on work that could
                // not move.
                return match self.hook_revisit {
                    Some(at) if at > now => Ripeness::At(at),
                    _ => Ripeness::Now,
                };
            }
            // The hook runs against a PUBLISHED graph, so on one that is not ready it waits
            // for whatever will publish next — never for a moment of its own. Naming a moment
            // here re-armed a fresh one every turn: the debt never came due, never exhausted,
            // and woke the watcher every thirty seconds, for ever, to do nothing.
            for (kind, ripeness) in [
                (DebtKind::Failed, standing.failed),
                (DebtKind::Forced, standing.forced),
                (DebtKind::Marks, standing.marks),
                (DebtKind::Change, standing.change),
            ] {
                if is_live(ripeness) {
                    return Ripeness::Behind(kind);
                }
            }
            Ripeness::Exhausted(Revival::FreshFact)
        });

        standing
    }

    /// The single decision. Pure: no disk, no locks, no spawning. It picks the RIPEST work,
    /// never the merely open debt, and the forced flag belongs to the work it picked.
    pub(super) fn decide(&self, now: Instant, facts: Facts) -> Decision {
        let mut decision = Decision::default();
        if facts.terminal {
            return decision;
        }
        // The stop is read here, inside the decision, for the reason spelled out on the field.
        if facts.stopping {
            return decision;
        }
        // A build in flight will publish or fail, and either outcome decides again. Nothing
        // starts beside it — that is what makes the slot single-flight.
        if facts.in_flight {
            return decision;
        }
        if !facts.owns {
            // Not a takeover, just an answer the lease could not give right now.
            decision.wake_at = self.unowned_wake(now);
            return decision;
        }
        let standing = self.standing(now, facts);
        let ripe = |ripeness: Option<Ripeness>| matches!(ripeness, Some(Ripeness::Now));
        // An idle graph has nothing published, so the only build it can take is the first one.
        let kind = if facts.ready { BuildKind::Reload } else { BuildKind::Initial };

        if ripe(standing.failed) {
            // The ONE place the demand is inherited: this build goes to read disk whatever
            // happens, so it reads the way the debts beside it need it read. Inherited by the
            // branch below instead, it fired before the marks' grace had run and went on firing
            // after their budget was spent.
            decision.start = Some(BuildStart {
                kind,
                trigger: "failed_retry",
                // Attached sponsors, not bystanders: a marks demand joins this retry's mode
                // only while its OWN budget is open, because joining costs it an attempt.
                forced: self.forced_demanded() || self.marks_eligible(now),
            });
        } else if ripe(standing.forced) {
            decision.start =
                Some(BuildStart { kind, trigger: "forced_project_reload", forced: true });
        } else if ripe(standing.marks) {
            // Marks nobody consumed: forced, because the fingerprint may be equal — a same-stat
            // edit, or a change the published build straddled — and an ordinary comparison
            // would then answer "nothing to do" for ever.
            decision.start = Some(BuildStart { kind, trigger: "owed_context_marks", forced: true });
        } else if ripe(standing.change) {
            if facts.ready {
                // A delivered change is answered by a comparison, not by a build: an equal
                // fingerprint IS the answer.
                decision.check = true;
            } else {
                decision.start = Some(BuildStart { kind, trigger: "disk_event", forced: false });
            }
        }
        if decision.start.is_none() && !decision.check && ripe(standing.recovery) {
            decision.probe = true;
        }
        if ripe(standing.hook) {
            decision.flush_hook = self.hook;
        }
        decision.wake_at = self.wake_at(now, facts, standing);
        decision
    }

    /// The alarm is a PROJECTION of the standing, not a second opinion about it: the earliest
    /// moment any debt named for itself, and the hold if one still stands.
    ///
    /// The rule this replaces decided separately which moments to mute, and muting them wrongly
    /// gave, by turns, an alarm already past (a wait of zero length, every turn) and no alarm at
    /// all (marks nothing would ever run). Here a debt queued behind another contributes no
    /// moment because `Behind` carries none, and a moment that has passed is `Now`, not `At`.
    fn wake_at(&self, now: Instant, facts: Facts, standing: Standing) -> Option<Instant> {
        if facts.terminal || facts.stopping {
            return None;
        }
        if !facts.owns {
            return self.unowned_wake(now);
        }
        standing
            .each()
            .into_iter()
            .filter_map(|(_, ripeness)| moment(ripeness))
            .chain(self.hold_due(now))
            .min()
    }

    /// When the held decision is to be taken again, while that moment is still ahead.
    ///
    /// A hold that has come and gone is not a wake-up: nothing clears it — ownership is
    /// confirmed by the next decision, not by an event — so a moment left in the past would
    /// be the earliest wake for ever, and the watcher would spin with a zero timeout,
    /// re-reading the lease file and draining the hub on every turn.
    fn hold_due(&self, now: Instant) -> Option<Instant> {
        self.held_until.filter(|at| *at > now)
    }

    /// Until when a hold still stands, for an owner that must not ask again before then.
    pub(super) fn held_until(&self, now: Instant) -> Option<Instant> {
        self.hold_due(now)
    }

    /// When to look again while ownership cannot be confirmed. Nothing can be decided without
    /// it, so the only question is when to ask again — the hold if one still stands, else one
    /// hold's time while anything is owed, and nothing at all when nothing is.
    fn unowned_wake(&self, now: Instant) -> Option<Instant> {
        self.hold_due(now).or_else(|| self.stale().then(|| now + HELD_RETRY))
    }

    /// The moment the marks' own schedule stands at, while its budget lasts — so a test can
    /// watch that schedule move. Deliberately the schedule's moment and not its ripeness: a
    /// moment already passed still reads as itself here, where `Ripeness` would read `Now`.
    #[cfg(test)]
    pub(super) fn marks_due(&self, now: Instant) -> Option<Instant> {
        let owed = self.marks.owed.as_ref()?;
        owed.window.is_open(now).then_some(owed.next_allowed)
    }

    /// Which debts are open, read from the debts THEMSELVES rather than from the schedule's
    /// account of them. The model needs a second source, or its check only ever agrees with the
    /// code that produced the answer it is checking.
    #[cfg(test)]
    pub(super) fn open(&self) -> [(DebtKind, bool); 6] {
        [
            (DebtKind::Failed, self.failed.is_some()),
            (DebtKind::Forced, self.forced_demanded()),
            (DebtKind::Marks, self.marks.owed.is_some()),
            (DebtKind::Change, self.comparison_owed().is_some()),
            (DebtKind::Recovery, self.recovery.is_some()),
            (DebtKind::Hook, self.hook.any()),
        ]
    }

    /// Apply the external work a spent budget named, so the model can prove the name is good.
    #[cfg(test)]
    pub(super) fn apply_revival(&mut self, now: Instant, revival: Revival, fact: u64, mark: i64) {
        match revival {
            Revival::FreshFact => self.record_change(now, fact),
            Revival::FreshMarks => self.place_marks(now, mark, fact),
        }
    }

    /// The hold, for the model: it is part of the alarm, so it must be part of what the model
    /// expects, or the check would forbid a moment the contract allows.
    #[cfg(test)]
    pub(super) fn held_for(&self, now: Instant) -> Option<Instant> {
        self.hold_due(now)
    }

    /// Stop the marks budget the way an operation error would.
    #[cfg(test)]
    pub(super) fn stop_marks_budget(&mut self) {
        if let Some(owed) = self.marks.owed.as_mut() {
            owed.operation_error();
        }
    }

    #[cfg(test)]
    pub(super) fn owes_marks(&self) -> bool {
        self.marks.owed.is_some()
    }

    /// The marks still placed, for a test that asserts exactly which ones a publication took.
    #[cfg(test)]
    pub(super) fn placed_marks(&self) -> Vec<(i64, u64)> {
        self.marks.placed.clone()
    }

    #[cfg(test)]
    pub(super) fn owes_change(&self) -> Option<u64> {
        self.comparison_owed()
    }

    #[cfg(test)]
    pub(super) fn owes_forced(&self) -> Option<u64> {
        self.forced
    }

    /// Bring the recovery probe forward to now, so a test need not wait out its interval.
    #[cfg(test)]
    pub(super) fn probe_now(&mut self, now: Instant) {
        if let Some(recovery) = self.recovery.as_mut() {
            recovery.next_probe = now;
        }
    }

    /// How long the probe waits at the moment, so a test can see the backoff move.
    #[cfg(test)]
    pub(super) fn probe_interval(&self) -> Option<Duration> {
        self.recovery.as_ref().map(|recovery| recovery.interval)
    }

    /// Whether the publication on record is one only a probe can heal: it could not vouch
    /// for its own scan, or it could not read part of the workspace. Read by the mark
    /// consumption, which may not charge marks against such a graph.
    pub(super) fn owes_recovery(&self) -> bool {
        self.recovery.as_ref().is_some_and(Recovery::outstanding)
    }

    #[cfg(test)]
    pub(super) fn owes_failed(&self) -> bool {
        self.failed.is_some()
    }

    /// Put the debt where a throttled failure leaves it: a build owed, held off until `at`.
    #[cfg(test)]
    pub(super) fn fail_held_until(&mut self, now: Instant, at: Instant) {
        self.record_failure(now, FailureKind::Transient, Sponsors { primary: true, marks: false });
        if let Some(failed) = self.failed.as_mut() {
            failed.schedule.next_allowed = at;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path the publication could not read.
    fn an_unread(path: &str) -> Capability {
        Capability::Open(path.to_owned())
    }

    /// One capability, measured once — the shape a probe reports.
    fn measured(capability: Capability, level: Level) -> Vec<(Capability, Level)> {
        vec![(capability, level)]
    }

    /// A scope descriptor, as a loaded project would produce one.
    fn a_scope(roots: &[&str], validated: bool) -> RecoveryScope {
        let roots: Vec<std::path::PathBuf> = roots.iter().map(std::path::PathBuf::from).collect();
        RecoveryScope::of(&roots, &[], validated)
    }

    /// A publication that says nothing about recovery coverage: it retires nothing, and what
    /// it declares unread is what it declares.
    fn publish(
        debt: &mut GraphDebt,
        now: Instant,
        observed: Option<u64>,
        forced: bool,
        coherent: bool,
    ) {
        let generation = observed.unwrap_or(0);
        // The shapes the two actually have. A coherent publication read every module and
        // walked the whole of its scope; an unsound one is here because its walk came up
        // short, which is the obligation a probe exists to answer.
        let proof = RecoveryPublicationProof {
            generation,
            captured_seq: debt.outstanding_recovery().captured_seq,
            declared_unread: Some(Vec::new()),
            scan_complete: Some(coherent),
            scope: (!coherent).then(|| a_scope(&["/ws"], true)),
            ..Default::default()
        };
        debt.record_publication(now, observed, forced, None, proof);
    }

    /// A publication that declares exactly these unread addresses, and — when it walked — the
    /// scope it covered and whether that walk could speak for the whole of it.
    fn publish_declaring(
        debt: &mut GraphDebt,
        now: Instant,
        observed: Option<u64>,
        generation: u64,
        unread: &[&str],
        walk: Option<(RecoveryScope, bool)>,
    ) {
        let captured_seq = debt.outstanding_recovery().captured_seq;
        let (scope, scan_complete) = match walk {
            Some((scope, complete)) => (Some(scope), Some(complete)),
            None => (None, None),
        };
        debt.record_publication(
            now,
            observed,
            false,
            None,
            RecoveryPublicationProof {
                generation,
                captured_seq,
                declared_unread: Some(unread.iter().map(|path| (*path).to_owned()).collect()),
                scan_complete,
                scope,
                ..Default::default()
            },
        );
    }

    /// What is outstanding, by key, in order.
    fn outstanding_keys(debt: &GraphDebt) -> Vec<String> {
        debt.outstanding_recovery().keys.into_iter().map(|(key, _)| key).collect()
    }

    /// A publication that proves it READ these addresses and re-declares those still unread.
    fn covering(
        debt: &GraphDebt,
        generation: u64,
        read: &[&str],
        still_unread: &[&str],
    ) -> RecoveryPublicationProof {
        let outstanding = debt.outstanding_recovery();
        let occurrence = |key: &str| {
            outstanding
                .keys
                .iter()
                .find(|(outstanding, _)| outstanding == key)
                .map(|(_, occurrence)| *occurrence)
                .unwrap_or_default()
        };
        RecoveryPublicationProof {
            generation,
            captured_seq: outstanding.captured_seq,
            declared_unread: Some(still_unread.iter().map(|key| (*key).to_owned()).collect()),
            read_covered: read.iter().map(|key| ((*key).to_owned(), occurrence(key))).collect(),
            ..Default::default()
        }
    }

    /// Reserve, measure, finish — the cycle one probe goes through, against the basis its own
    /// reservation handed out.
    fn probe(debt: &mut GraphDebt, now: Instant, levels: Vec<(Capability, Level)>) -> ProbeResult {
        probe_walking(debt, now, levels, None)
    }

    /// The same, for a probe that also walked a scope.
    fn probe_walking(
        debt: &mut GraphDebt,
        now: Instant,
        levels: Vec<(Capability, Level)>,
        scope: Option<RecoveryScope>,
    ) -> ProbeResult {
        let Some(plan) = debt.reserve_probe() else { return ProbeResult::Obsolete };
        debt.finish_probe(now, ProbeReceipt { token: plan.token, basis: plan.basis, levels, scope })
    }
    use std::collections::BTreeSet;

    /// A violation, named by the check that found it. Checks REPORT rather than assert, so one
    /// trace's first failure cannot hide which other checks a defect also trips — that is what
    /// tells the mutation controls apart.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Violation {
        check: &'static str,
        what: String,
    }

    fn v(check: &'static str, what: String) -> Violation {
        Violation { check, what }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        Change,
        Forced,
        Marks,
        PublishClean,
        PublishUnsound,
        FailTransient,
        FailOperation,
        FailSpawn,
        Heal,
        Hook,
        Unconfirmed,
        Tick,
        LongTick,
        Stop,
    }

    const ALPHABET: [Event; 14] = [
        Event::Change,
        Event::Forced,
        Event::Marks,
        Event::PublishClean,
        Event::PublishUnsound,
        Event::FailTransient,
        Event::FailOperation,
        Event::FailSpawn,
        Event::Heal,
        Event::Hook,
        Event::Unconfirmed,
        Event::Tick,
        Event::LongTick,
        Event::Stop,
    ];

    /// The graph as the CHECK keeps it: the debts under test plus a ledger of its own, kept by
    /// the events rather than by the code under test. Every invariant is read against this.
    #[derive(Clone)]
    struct Model {
        debt: GraphDebt,
        now: Instant,
        /// How many of the publication's unread paths have opened so far, so the next healing
        /// names a path that has not healed yet.
        healed: usize,
        /// Publications installed, so each declaration carries its own generation.
        generations: u64,
        ready: bool,
        failed: bool,
        idle: bool,
        in_flight: bool,
        forced_build: bool,
        owns: bool,
        stopping: bool,
        fact: u64,
        mark: i64,
        /// A delivered change no publication has observed.
        change_waiting: bool,
        /// A forced reload no FORCED publication has answered. Kept apart from the plain
        /// change: an ordinary publication observes the fact and still re-reads nothing, so
        /// one flag for both would call the graph caught up while a config re-read is owed.
        forced_waiting: bool,
        failure_open: bool,
        marks_waiting: Option<u64>,
        unsound: bool,
        external: u32,
        external_at_build: Option<u32>,
        builds: u32,
        unjustified: u32,
        work_after_stop: u32,
        /// A hook obligation the executor has not taken back.
        hook_waiting: bool,
        /// The lanes that paid for the admission now in flight.
        sponsors: Sponsors,
        /// Violations seen at the moment the EXECUTOR consulted the decision. Checked there and
        /// not only after: a decision that starts work it should not have leaves the graph
        /// in flight, and an `in_flight` graph is exempt from the ripeness checks — so a defect
        /// that starts a build hides behind the very state it created.
        pending: Vec<Violation>,
    }

    impl Model {
        fn new() -> Self {
            Self {
                debt: GraphDebt::default(),
                now: Instant::now(),
                healed: 0,
                generations: 0,
                ready: false,
                failed: false,
                idle: true,
                in_flight: false,
                forced_build: false,
                owns: true,
                stopping: false,
                fact: 0,
                mark: 0,
                change_waiting: false,
                forced_waiting: false,
                failure_open: false,
                marks_waiting: None,
                unsound: false,
                external: 0,
                external_at_build: None,
                builds: 0,
                unjustified: 0,
                work_after_stop: 0,
                hook_waiting: false,
                sponsors: Sponsors::default(),
                pending: Vec::new(),
            }
        }

        fn facts(&self) -> Facts {
            Facts {
                ready: self.ready,
                failed: self.failed,
                idle: self.idle,
                in_flight: self.in_flight,
                owns: self.owns,
                terminal: false,
                stopping: self.stopping,
            }
        }

        fn step(&mut self, event: Event) {
            if event != Event::Unconfirmed {
                self.owns = true;
            }
            match event {
                Event::Change => {
                    self.fact += 1;
                    self.debt.record_change(self.now, self.fact);
                    self.change_waiting = true;
                    self.external += 1;
                }
                Event::Forced => {
                    self.fact += 1;
                    self.debt.record_forced(self.now, self.fact);
                    self.forced_waiting = true;
                    self.external += 1;
                }
                Event::Marks => {
                    self.fact += 1;
                    self.mark += 1;
                    self.debt.place_marks(self.now, self.mark, self.fact);
                    self.debt.settle_marks(self.now, self.in_flight);
                    self.marks_waiting = Some(self.fact);
                    self.external += 1;
                }
                Event::PublishClean | Event::PublishUnsound => {
                    if !self.in_flight {
                        return;
                    }
                    let coherent = event == Event::PublishClean;
                    let generation = self.generations + 1;
                    self.generations = generation;
                    let cutoff = self.debt.capture_recovery();
                    // An unsound publication declares the gap that makes it unsound: what a
                    // probe may look at comes from an authoritative build, never from the
                    // probe. A coherent one PROVES it read what was outstanding, which is what
                    // answers those obligations and closes the episode.
                    let outstanding = self.debt.outstanding_recovery();
                    self.debt.record_publication(
                        self.now,
                        Some(self.fact),
                        self.forced_build,
                        Some(cutoff),
                        RecoveryPublicationProof {
                            generation,
                            captured_seq: cutoff,
                            declared_unread: Some(if coherent {
                                Vec::new()
                            } else {
                                vec!["unread-anchor".to_owned()]
                            }),
                            scan_complete: Some(coherent),
                            read_covered: if coherent { outstanding.keys } else { Vec::new() },
                            ..Default::default()
                        },
                    );
                    if coherent {
                        if let Some(bound) = self.debt.marks.bound(self.fact) {
                            self.debt.marks.consumed(self.fact, bound);
                        }
                        if self.marks_waiting.is_some_and(|fact| fact <= self.fact) {
                            self.marks_waiting = None;
                        }
                    }
                    self.debt.settle_marks(self.now, false);
                    // A publication observes every fact up to its own; only a FORCED one
                    // discharges what no comparison can answer.
                    self.change_waiting = false;
                    if self.forced_build {
                        self.forced_waiting = false;
                    }
                    self.in_flight = false;
                    self.forced_build = false;
                    self.ready = true;
                    self.failed = false;
                    self.idle = false;
                    self.failure_open = false;
                    self.unsound = !coherent;
                }
                Event::FailTransient | Event::FailOperation | Event::FailSpawn => {
                    if !self.in_flight {
                        return;
                    }
                    let kind = match event {
                        Event::FailTransient => FailureKind::Transient,
                        Event::FailOperation => FailureKind::Operation,
                        _ => FailureKind::Spawn,
                    };
                    self.debt.record_failure(self.now, kind, self.sponsors);
                    self.in_flight = false;
                    self.forced_build = false;
                    if !self.ready {
                        self.failed = true;
                        self.idle = false;
                    }
                    self.failure_open = true;
                }
                Event::Heal => {
                    if self.debt.owes_recovery() {
                        // One more of the publication's unread paths opens. A NAMED one: the
                        // same path opening again is a repeat, and the model would then be
                        // asserting about work no measurement asked for. The address is
                        // required because a publication declared it, never because a probe
                        // mentioned it.
                        self.healed += 1;
                        let key = format!("unread-{}", self.healed);
                        let generation = self.generations + 1;
                        self.generations = generation;
                        publish_declaring(
                            &mut self.debt,
                            self.now,
                            None,
                            generation,
                            &[key.as_str()],
                            None,
                        );
                        probe(
                            &mut self.debt,
                            self.now,
                            vec![(Capability::Open(key), Level::Granted)],
                        );
                        // The healing is proved only by the forced build it asks for.
                        self.forced_waiting = true;
                        self.external += 1;
                    }
                }
                Event::Hook => {
                    // A publication whose hook could not run what it asked for.
                    self.debt.record_hook(a_hook());
                    self.hook_waiting = true;
                }
                Event::Unconfirmed => self.owns = false,
                Event::Tick => self.now += Duration::from_secs(60),
                Event::LongTick => {
                    self.now +=
                        crate::state::retry_window::DEFAULT_RETRY_BUDGET + Duration::from_secs(60)
                }
                Event::Stop => self.stopping = true,
            }
            self.drive();
        }

        /// The executor, modelled: one decision, one action, nothing else.
        fn drive(&mut self) {
            let seen = self.check_all();
            self.pending.extend(seen);
            let decision = self.debt.decide(self.now, self.facts());
            if self.stopping && decision.does_work() {
                self.work_after_stop += 1;
            }
            if let Some(start) = decision.start {
                if self.external_at_build == Some(self.external)
                    && !self.failure_open
                    && !self.unsound
                {
                    self.unjustified += 1;
                }
                self.external_at_build = Some(self.external);
                self.builds += 1;
                self.in_flight = true;
                self.forced_build = start.forced;
                self.idle = false;
                // The executor's half of the admission: taking the slot spends what paid for
                // it. Modelled here because that is where production charges it — at the
                // claim — and a model that takes work without paying proves nothing about a
                // budget.
                self.sponsors = self.debt.charge_admission(self.fact, self.now, start.forced);
            } else if decision.check {
                self.debt.change_answered(self.fact);
                self.change_waiting = false;
            } else if decision.probe {
                probe(&mut self.debt, self.now, Vec::new());
            }
            if decision.flush_hook.any() {
                let (revision, _) = self.debt.claim_hook();
                self.debt.hook_handled(revision, decision.flush_hook);
                self.hook_waiting = false;
            }
        }

        fn quiet(&self) -> bool {
            self.stopping || self.in_flight || !self.owns
        }

        // ------------------------------------------------------------------- the invariants

        /// INV-OWN — every open debt has an owner the SCHEDULE names: work now, a finite alarm,
        /// a standing watch at a capped cadence, a declared exhaustion with the external work
        /// that revives it, or a place behind a blocker that is ITSELF owned. A debt the
        /// schedule says nothing about is progress lost, and this is the check that sees it.
        fn inv_own(&self, out: &mut Vec<Violation>) {
            if self.quiet() {
                return;
            }
            let standing = self.debt.standing(self.now, self.facts());
            for (kind, open) in self.debt.open() {
                if !open {
                    continue;
                }
                let Some(ripe) = standing.of(kind) else {
                    out.push(v(
                        "INV-OWN",
                        format!("{kind:?} is owed and the schedule names no owner"),
                    ));
                    continue;
                };
                match ripe {
                    Ripeness::Now | Ripeness::Exhausted(_) => {}
                    Ripeness::At(at) | Ripeness::Watching(at) => {
                        if at <= self.now {
                            out.push(v(
                                "INV-FORWARD",
                                format!("{kind:?} names an alarm already past — a zero-length wait every turn"),
                            ));
                        }
                    }
                    Ripeness::Behind(blocker) => {
                        // Follow the queue to its head: a debt may wait behind one that is
                        // itself waiting, and what the contract forbids is a queue that ends
                        // anywhere but on a debt somebody will run. Bounded by the number of
                        // debts, and a repeat is a cycle — which is the same fault.
                        let mut at = blocker;
                        let mut seen = vec![kind];
                        loop {
                            if seen.contains(&at) {
                                out.push(v(
                                    "INV-OWN",
                                    format!("{kind:?} waits behind itself through {seen:?}"),
                                ));
                                break;
                            }
                            seen.push(at);
                            match standing.of(at) {
                                Some(Ripeness::Behind(next)) => at = next,
                                other if is_live(other) => break,
                                other => {
                                    out.push(v(
                                        "INV-OWN",
                                        format!(
                                            "{kind:?} waits behind {at:?}, which is {other:?} — a queue that ends on something nobody will run",
                                        ),
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        /// INV-WORK / INV-QUIET — the decision acts exactly when some debt is RIPE, never
        /// merely when one is open. That conflation is the defect this contract replaces.
        fn inv_work(&self, out: &mut Vec<Violation>) {
            if self.quiet() {
                return;
            }
            let standing = self.debt.standing(self.now, self.facts());
            let any_ripe = standing.each().iter().any(|(_, r)| matches!(r, Some(Ripeness::Now)));
            let decision = self.debt.decide(self.now, self.facts());
            if any_ripe && !decision.does_work() {
                out.push(v("INV-WORK", format!("a debt is ripe and nothing runs: {standing:?}")));
            }
            if !any_ripe && decision.does_work() {
                out.push(v(
                    "INV-QUIET",
                    format!("nothing is ripe and the decision works anyway: {decision:?} over {standing:?}"),
                ));
            }
        }

        /// INV-ALARM — the alarm is the earliest moment the standing names, and nothing else.
        fn inv_alarm(&self, out: &mut Vec<Violation>) {
            if self.quiet() {
                return;
            }
            let standing = self.debt.standing(self.now, self.facts());
            let named = standing.each().into_iter().filter_map(|(_, r)| moment(r)).min();
            let expected = match (named, self.debt.held_for(self.now)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let decision = self.debt.decide(self.now, self.facts());
            if decision.wake_at != expected {
                out.push(v(
                    "INV-ALARM",
                    format!(
                        "the alarm {:?} disagrees with the standing {standing:?}",
                        decision.wake_at
                    ),
                ));
            }
            if !decision.does_work() {
                if let Some(at) = decision.wake_at {
                    if at <= self.now {
                        out.push(v(
                            "INV-FORWARD",
                            "an alarm already past, with no work to do".to_owned(),
                        ));
                    }
                }
            }
        }

        /// INV-REVIVE — an exhaustion is a state only if the work it NAMES actually revives it.
        /// Checked on a copy, so the trace under test is not perturbed. Without this an
        /// implementation could call any dropped debt "exhausted" and satisfy INV-OWN by word.
        fn inv_revive(&self, out: &mut Vec<Violation>) {
            if self.quiet() {
                return;
            }
            let standing = self.debt.standing(self.now, self.facts());
            for (kind, ripe) in standing.each() {
                let Some(Ripeness::Exhausted(revival)) = ripe else { continue };
                let mut probe = self.clone();
                probe.debt.apply_revival(probe.now, revival, probe.fact + 1, probe.mark + 1);
                let after = probe.debt.standing(probe.now, probe.facts()).of(kind);
                if after.is_none() || matches!(after, Some(Ripeness::Exhausted(_))) {
                    out.push(v(
                        "INV-REVIVE",
                        format!(
                            "{kind:?} claims {revival:?} revives it, and it does not: {after:?}"
                        ),
                    ));
                }
            }
        }

        /// INV-STOP — once the daemon has asked every owner to leave, the decision starts
        /// nothing, checks nothing, probes nothing and sets no alarm, however it is reached.
        fn inv_stop(&self, out: &mut Vec<Violation>) {
            if !self.stopping {
                return;
            }
            let decision = self.debt.decide(self.now, self.facts());
            if decision.does_work() {
                out.push(v("INV-STOP", format!("the decision works after the stop: {decision:?}")));
            }
            if decision.wake_at.is_some() {
                out.push(v("INV-STOP", "an alarm is set after the stop".to_owned()));
            }
            if self.work_after_stop > 0 {
                out.push(v(
                    "INV-STOP",
                    format!("{} actions ran after the stop", self.work_after_stop),
                ));
            }
        }

        /// INV-BUDGET — nothing starts a build once the budget that bounds builds is spent.
        ///
        /// Read from the STANDING, not from the decision, and stated over the whole slot rather
        /// than over one debt: the build slot has one budget, and a debt with no schedule of its
        /// own cannot outlive it. The check that used to stand here exempted every build taken
        /// while a failure was open, which is precisely the window an exhausted budget lives in
        /// — so a graph rebuilding for ever over a workspace that could not build tripped
        /// nothing at all.
        fn inv_budget(&self, out: &mut Vec<Violation>) {
            if self.quiet() {
                return;
            }
            let standing = self.debt.standing(self.now, self.facts());
            if !matches!(standing.failed, Some(Ripeness::Exhausted(_))) {
                return;
            }
            // The marks are the one debt allowed past a spent retry, and only because they pay
            // for the attempt out of a budget of their own.
            let decision = self.debt.decide(self.now, self.facts());
            if decision.start.is_some() && !matches!(standing.marks, Some(Ripeness::Now)) {
                out.push(v(
                    "INV-BUDGET",
                    format!("a build starts on a spent budget: {standing:?}"),
                ));
            }
        }

        /// INV-STALE — the graph never reads fresh while it is behind, nor behind for nothing.
        fn inv_stale(&self, out: &mut Vec<Violation>) {
            let behind = self.change_waiting
                || self.forced_waiting
                || self.failure_open
                || self.unsound
                || self.marks_waiting.is_some();
            if behind && !self.debt.stale() {
                out.push(v("INV-STALE", "the graph is behind and reads fresh".to_owned()));
            }
            if self.debt.stale() && !behind && !self.in_flight {
                out.push(v("INV-STALE", "the graph reads behind with nothing owed".to_owned()));
            }
        }

        fn check_all(&self) -> Vec<Violation> {
            let mut out = Vec::new();
            self.inv_own(&mut out);
            self.inv_work(&mut out);
            self.inv_alarm(&mut out);
            self.inv_revive(&mut out);
            self.inv_stop(&mut out);
            self.inv_stale(&mut out);
            self.inv_budget(&mut out);
            out
        }

        /// INV-LIVENESS, bounded — from here, with NO further external work, the graph settles
        /// within `budget` alarm-driven turns: it owes nothing, or every debt it still owes is
        /// exhausted or a standing watch. A schedule that keeps finding work for ever without
        /// an external cause trips INV-LOOP; one that leaves an alarm in the past trips
        /// INV-FORWARD; one that drops a debt trips INV-OWN.
        ///
        /// BOUND, stated plainly: this proves settling for the states reachable in the
        /// enumerated prefixes, under the events the model has, within `budget` turns. It is
        /// not a proof of general liveness for every infinite schedule.
        /// A bounded, alarm-driven walk from here with NO further external work, settling every
        /// build it starts with `outcome`.
        ///
        /// The outcome is a parameter because assuming the happy one is a blind spot, not a
        /// simplification: a walk that always publishes cleanly never visits the states a
        /// workspace that cannot build lives in, which is exactly where an unbounded rebuild
        /// hides. A graph must come to rest on EITHER outcome — by publishing, or by naming
        /// what would revive it.
        fn settles(&self, budget: u32, outcome: Event, out: &mut Vec<Violation>) {
            if self.quiet() {
                return;
            }
            let mut probe = self.clone();
            let before = probe.unjustified;
            for _ in 0..budget {
                // Every OPEN debt, not just the ones `stale` counts. `stale` answers "is the
                // graph behind", and the hook debt is deliberately not part of that question —
                // so a graph whose only debt was the hook looked settled the moment it was
                // reached, and the walk never saw the alarm it was re-arming for ever.
                let anything_open = probe.debt.open().into_iter().any(|(_, open)| open);
                if !anything_open && !probe.in_flight {
                    return;
                }
                if probe.in_flight {
                    // A build in flight is answered by its own outcome. Settling it the way the
                    // executor would invents no external work.
                    probe.step(outcome);
                    out.append(&mut probe.pending);
                    out.extend(probe.check_all());
                    continue;
                }
                let standing = probe.debt.standing(probe.now, probe.facts());
                // A settled graph may carry exhaustions and standing watches for ever: the one
                // names work that will never come without an external cause, the other a
                // bounded cadence on something no fact stream announces.
                let resting =
                    probe.debt.open().into_iter().filter(|&(_, open)| open).all(|(kind, _)| {
                        matches!(
                            standing.of(kind),
                            Some(Ripeness::Exhausted(_)) | Some(Ripeness::Watching(_))
                        )
                    });
                if resting {
                    return;
                }
                let decision = probe.debt.decide(probe.now, probe.facts());
                if decision.does_work() {
                    // Ripe work runs before the clock moves: a decision with something to do
                    // and no alarm is not a dead end, it is the executor's turn.
                    probe.drive();
                    out.append(&mut probe.pending);
                    out.extend(probe.check_all());
                } else if let Some(at) = decision.wake_at {
                    probe.now = at.max(probe.now + Duration::from_millis(1));
                    probe.drive();
                    out.append(&mut probe.pending);
                    out.extend(probe.check_all());
                } else {
                    for (kind, open) in probe.debt.open() {
                        if open
                            && !matches!(
                                standing.of(kind),
                                Some(Ripeness::Exhausted(_)) | Some(Ripeness::Watching(_))
                            )
                        {
                            out.push(v(
                                "INV-LIVENESS",
                                format!("{kind:?} is owed, nothing runs, no alarm is set and it is not exhausted"),
                            ));
                        }
                    }
                    return;
                }
                if probe.unjustified != before {
                    out.push(v(
                        "INV-LOOP",
                        "the graph kept building with no external cause".to_owned(),
                    ));
                    return;
                }
            }
            out.push(v(
                "INV-LIVENESS",
                format!(
                    "never settled in {budget} alarm-driven turns, settling builds with {outcome:?}"
                ),
            ));
        }
    }

    /// Every order of `depth` events, each state against the whole contract. Returns every
    /// distinct violation found, so one defect cannot hide another.
    fn enumerate(depth: usize, settle: bool) -> BTreeSet<(&'static str, String)> {
        let mut found: BTreeSet<(&'static str, String)> = BTreeSet::new();
        let mut index = vec![0usize; depth];
        loop {
            let trace: Vec<Event> = index.iter().map(|&i| ALPHABET[i]).collect();
            let mut model = Model::new();
            let mut violations = Vec::new();
            for &event in &trace {
                model.step(event);
                violations.append(&mut model.pending);
                violations.extend(model.check_all());
            }
            if model.unjustified > 0 {
                violations.push(v("INV-LOOP", "a build started with no external cause".to_owned()));
            }
            if settle {
                for outcome in [Event::PublishClean, Event::FailOperation, Event::FailTransient] {
                    model.settles(64, outcome, &mut violations);
                }
            }
            for violation in violations {
                found.insert((violation.check, format!("{trace:?}: {}", violation.what)));
            }
            // odometer
            let mut at = depth;
            loop {
                if at == 0 {
                    return found;
                }
                at -= 1;
                index[at] += 1;
                if index[at] < ALPHABET.len() {
                    break;
                }
                index[at] = 0;
            }
        }
    }

    fn report(found: &BTreeSet<(&'static str, String)>) -> String {
        let mut checks: BTreeSet<&'static str> = BTreeSet::new();
        for (check, _) in found {
            checks.insert(check);
        }
        let sample: Vec<String> =
            found.iter().take(6).map(|(c, w)| format!("  [{c}] {w}")).collect();
        format!("{} violations across {checks:?}\n{}", found.len(), sample.join("\n"))
    }

    /// Every order of three events the graph can see, against the whole contract, and then a
    /// bounded alarm-driven settling from each of those states with no further external work.
    #[test]
    fn every_short_order_keeps_the_contract_and_settles() {
        let found = enumerate(3, true);
        assert!(found.is_empty(), "{}", report(&found));
    }

    /// The four-deep enumeration, without the settling walk over it.
    #[test]
    fn every_four_deep_order_keeps_the_contract() {
        let found = enumerate(4, true);
        assert!(found.is_empty(), "{}", report(&found));
    }

    // ------------------------------------------------------------------ the named reproducers

    fn ready() -> Facts {
        Facts { ready: true, owns: true, ..Facts::default() }
    }

    /// One hook obligation, for the event that arms it.
    fn a_hook() -> HookDebt {
        HookDebt { topology: true, roots: false, marks: false }
    }

    /// A reconcile reaches the watcher and the search consumer alike, and both record it. One
    /// forced build answers both: the debt is the fact, not a request counter, so the second
    /// recorder adds nothing to pay for.
    #[test]
    fn two_owners_recording_one_reconcile_owe_one_build() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();

        debt.record_forced(now, 5);
        debt.record_forced(now, 5);
        let start = debt.decide(now, ready()).start.expect("the reconcile owes a forced build");
        assert!(start.forced, "a reconcile cannot be answered by a fingerprint comparison");

        // The build ran forced and its scan covered the fact both owners recorded.
        publish(&mut debt, now, Some(5), true, true);

        assert!(
            debt.decide(now, ready()).start.is_none(),
            "the second owner's request paid for a second rebuild of what was just published",
        );
    }

    /// The loop the hub barrier guards against, asked of the debts alone: with no fact, no
    /// failure and no healing, nothing starts a build however long the graph is driven.
    #[test]
    fn a_quiet_graph_starts_no_build_of_its_own() {
        let mut model = Model::new();
        model.step(Event::Change);
        model.step(Event::PublishClean);
        let after_publication = model.builds;

        for _ in 0..50 {
            model.step(Event::Tick);
        }

        assert_eq!(model.builds, after_publication, "a quiet graph rebuilt itself");
        assert!(!model.debt.stale(), "and it has nothing left to owe");
    }

    /// A publication that cannot vouch for itself is owed a probe, the probe backs off while
    /// nothing heals, and the healing — not the probe — is what pays for the next build.
    #[test]
    fn an_unsound_publication_is_probed_and_only_a_healing_rebuilds() {
        let mut model = Model::new();
        model.step(Event::Change);
        model.step(Event::PublishUnsound);
        assert!(model.debt.owes_recovery(), "an unsound publication owes a probe");
        let after_publication = model.builds;

        for _ in 0..20 {
            model.step(Event::Tick);
        }
        assert_eq!(model.builds, after_publication, "a probe that healed nothing rebuilt anyway");
        assert!(model.debt.stale(), "and the graph still reads behind");

        model.step(Event::Heal);
        assert_eq!(model.builds, after_publication + 1, "a healing did not pay for a rebuild");
    }

    /// R3 #1 `2e597721` — marks in their grace are an OPEN debt, not a ripe one, and the
    /// decision leaves the consumer the room the grace exists for.
    #[test]
    fn marks_in_their_grace_start_no_build() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 1, 10);
        assert!(debt.settle_marks(now, false));

        assert_eq!(debt.standing(now, ready()).marks, Some(Ripeness::At(now + OWED_MARKS_GRACE)));
        let decision = debt.decide(now, ready());
        assert!(decision.start.is_none(), "the grace did not hold the build off");
        assert_eq!(decision.wake_at, Some(now + OWED_MARKS_GRACE));

        let later = now + OWED_MARKS_GRACE;
        assert_eq!(debt.standing(later, ready()).marks, Some(Ripeness::Now));
        assert!(debt.decide(later, ready()).start.is_some_and(|s| s.forced));
    }

    /// R3 #1, the other half — once the marks' own budget is spent, the decision stops starting
    /// builds for them instead of running one on every turn.
    #[test]
    fn marks_whose_budget_is_spent_start_no_more_builds() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 1, 10);
        debt.settle_marks(now, false);
        if let Some(owed) = debt.marks.owed.as_mut() {
            owed.operation_error();
        }
        let later = now + Duration::from_secs(3600);
        assert_eq!(
            debt.standing(later, ready()).marks,
            Some(Ripeness::Exhausted(Revival::FreshMarks))
        );
        assert!(debt.decide(later, ready()).start.is_none(), "a spent budget kept starting builds");
        assert_eq!(debt.decide(later, ready()).wake_at, None, "and it kept an alarm it cannot use");
    }

    /// R3 #3 `7d301c1c` — a failure whose budget is spent owns nothing, so the marks behind it
    /// come forward instead of starving.
    #[test]
    fn marks_behind_a_spent_failure_are_not_starved() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 8);
        debt.settle_marks(now, false);
        debt.record_failure(now, FailureKind::Operation, Sponsors { primary: true, marks: false });

        let later = now + Duration::from_secs(3600);
        let standing = debt.standing(later, ready());
        assert_eq!(standing.failed, Some(Ripeness::Exhausted(Revival::FreshFact)));
        assert_eq!(standing.marks, Some(Ripeness::Now), "the marks stayed behind a dead blocker");
        assert!(
            debt.decide(later, ready()).start.is_some_and(|s| s.forced),
            "marks owed, failure spent, and nothing runs",
        );
    }

    /// R3 #2 `66acfbd8` — the stop is inside the decision, so a recording path that drives from
    /// within itself starts nothing either.
    #[test]
    fn a_stopped_graph_decides_no_work_however_it_is_driven() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_forced(now, 3);
        assert!(debt.decide(now, ready()).start.is_some());

        let decision = debt.decide(now, Facts { stopping: true, ..ready() });
        assert!(!decision.does_work(), "the decision worked after the stop: {decision:?}");
        assert_eq!(decision.wake_at, None, "a stopped graph set an alarm");
    }

    /// The check is not a second copy of the decision, and this is what that buys.
    ///
    /// The state below is one the decision is perfectly self-consistent about: a retry whose
    /// budget is spent, marks still owed, nothing ripe, nothing done, no alarm. A check that
    /// re-ran the decision to ask "is this debt owned" would see that agreement and pass — which
    /// is exactly what the old ownership check did. Reading the STANDING as a contract is what
    /// names the fault, and here it names the opposite of a fault: the marks are ripe, so the
    /// decision must act.
    #[test]
    fn a_spent_retry_holds_nothing_and_the_standing_says_so() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 8);
        debt.settle_marks(now, false);
        debt.record_failure(now, FailureKind::Operation, Sponsors { primary: true, marks: false });
        let later = now + Duration::from_secs(3600);

        let standing = debt.standing(later, ready());
        assert!(
            matches!(standing.failed, Some(Ripeness::Exhausted(_))),
            "an operation error stops the retry budget: {:?}",
            standing.failed,
        );
        assert_eq!(
            standing.marks,
            Some(Ripeness::Now),
            "and a stopped retry holds nothing, so the marks come forward",
        );
        assert!(
            debt.decide(later, ready()).start.is_some_and(|start| start.forced),
            "a debt that is ripe must be acted on, not left behind a blocker that will never run",
        );
    }

    /// A request may move the probe's next moment forward. It may not move its COST back: the
    /// backoff describes how often looking for a healing is worth the walk, and a client asking
    /// about the graph is not news about the disk.
    ///
    /// Two reachable sequences, and the contract has to survive both: a client polling faster
    /// than the current interval, and a subtree that stays unreadable for ever.
    #[test]
    fn a_request_pulls_the_probe_forward_but_never_resets_its_backoff() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Модуль.bsl"], None);
        probe(&mut debt, now, measured(an_unread("/ws/Модуль.bsl"), Level::Denied));
        assert_eq!(
            debt.probe_interval(),
            Some(RECOVERY_PROBE * 2),
            "one fruitless probe earns one step of the backoff",
        );

        assert!(debt.note_request(now + Duration::from_secs(1)), "the probe is pulled forward");
        assert_eq!(
            debt.probe_interval(),
            Some(RECOVERY_PROBE * 2),
            "a request pulled the probe forward and cost it its backoff as well",
        );

        // The whole sequence: an agent polling every 30s over a subtree that never heals must
        // still reach the declared cap, not oscillate at the floor for ever.
        let mut probing = GraphDebt::default();
        let mut at = Instant::now();
        probing.record_change(at, 1);
        publish_declaring(&mut probing, at, Some(1), 1, &["/ws/Модуль.bsl"], None);
        for _ in 0..600 {
            at += Duration::from_secs(30);
            probing.note_request(at);
            if matches!(probing.standing(at, ready()).recovery, Some(Ripeness::Now)) {
                probe(&mut probing, at, measured(an_unread("/ws/Модуль.bsl"), Level::Denied));
            }
        }
        assert_eq!(
            probing.probe_interval(),
            Some(RECOVERY_PROBE_CAP),
            "a polling client held the probe off its cap",
        );
    }

    /// A probe's only product is a forced build. While one is already owed AND somebody will
    /// actually run it, probing again walks the whole tree to record a debt already recorded —
    /// even when that forced demand is itself queued behind a retry in its backoff.
    ///
    /// The exception is what keeps this from becoming a wait nothing ends: once the chain ends
    /// on an exhaustion, nobody will run it, and the probe is the one owner that can measure
    /// the change which revives the budget.
    #[test]
    fn a_live_forced_owner_suppresses_redundant_probes() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Модуль.bsl"], None);
        // A forced demand owed to the probe's own earlier finding.
        debt.record_forced(now, 2);
        // A retry in its backoff owns the slot.
        debt.record_failure(now, FailureKind::Transient, Sponsors { primary: true, marks: false });
        debt.record_failure(now, FailureKind::Transient, Sponsors { primary: true, marks: false });

        let standing = debt.standing(now, ready());
        assert!(
            matches!(standing.failed, Some(Ripeness::At(_))),
            "the stand needs a retry inside its backoff: {:?}",
            standing.failed,
        );
        assert_eq!(
            standing.forced,
            Some(Ripeness::Behind(DebtKind::Failed)),
            "the stand needs a forced demand queued behind that retry",
        );
        assert_eq!(
            standing.recovery,
            Some(Ripeness::Behind(DebtKind::Forced)),
            "a forced build with a live owner already carries what the probe would record",
        );

        // Now spend the retry budget AND the credit the forced demand carries, the way an
        // admission does: with an unused credit still standing the lane would legitimately
        // open another epoch, and the question here is what happens when nothing is left.
        debt.charge_admission(2, now, true);
        debt.record_failure(now, FailureKind::Operation, Sponsors { primary: true, marks: false });
        let spent = debt.standing(now, ready());
        assert!(
            matches!(spent.failed, Some(Ripeness::Exhausted(_))),
            "the stand needs a spent retry: {:?}",
            spent.failed,
        );
        assert!(
            is_live(spent.recovery),
            "a forced demand nobody will run must not hold the probe off: {:?}",
            spent.recovery,
        );
    }

    /// The same measured level, reported again, is not news. Both of the probe's capabilities
    /// are sticky — a module that became readable stays readable, a scan that completed keeps
    /// completing — so a schedule that reads "healed" as a boolean re-arms a forced rebuild on
    /// every pass for a healing it already answered.
    #[test]
    fn repeated_recovery_level_never_reopens_an_epoch() {
        let scope = a_scope(&["/ws"], true);
        let cases: Vec<(&str, Vec<(Capability, Level)>)> = vec![
            ("the walk", vec![(Capability::ScanRoots, Level::Granted)]),
            ("one path", measured(an_unread("/ws/Модуль.bsl"), Level::Granted)),
            ("a removal", measured(an_unread("/ws/Модуль.bsl"), Level::Absent)),
            (
                "both",
                vec![
                    (Capability::ScanRoots, Level::Granted),
                    (an_unread("/ws/Модуль.bsl"), Level::Granted),
                ],
            ),
        ];
        for (what, levels) in cases {
            let mut debt = GraphDebt::default();
            let mut now = Instant::now();
            debt.record_change(now, 1);
            debt.charge_admission(1, now, false);
            publish_declaring(
                &mut debt,
                now,
                Some(1),
                1,
                &["/ws/Модуль.bsl"],
                Some((scope.clone(), false)),
            );

            // The transition: the first time this level is seen it IS news.
            assert_eq!(
                probe_walking(&mut debt, now, levels.clone(), Some(scope.clone())),
                ProbeResult::NewEvidence,
                "{what}: the first measure is news",
            );
            assert!(debt.owes_recovery_build(), "{what}: and a build is owed for it");
            // Answer it the way a build would, so the next probe is not merely queued behind
            // a forced demand nobody ran.
            let cutoff = debt.capture_recovery();
            debt.record_publication(
                now,
                Some(2),
                true,
                Some(cutoff),
                RecoveryPublicationProof {
                    generation: 2,
                    captured_seq: cutoff,
                    declared_unread: Some(vec!["/ws/Модуль.bsl".to_owned()]),
                    scan_complete: Some(false),
                    scope: Some(scope.clone()),
                    ..Default::default()
                },
            );
            assert!(!debt.owes_recovery_build(), "{what}: the build that ran answered it");

            // The same level again, and again: nothing new was measured.
            for _ in 0..4 {
                now += RECOVERY_PROBE_CAP;
                assert_eq!(
                    probe_walking(&mut debt, now, levels.clone(), Some(scope.clone())),
                    ProbeResult::NoNewEvidence,
                    "{what}: repeating a level already measured armed another rebuild",
                );
                assert!(!debt.owes_recovery_build(), "{what}: and owes no build for it");
            }
        }
    }

    /// A finite required set of any size is tracked — 1023, 1024, 1025 and larger.
    ///
    /// The cap dropped every capability past the first 1024 and never took it back, so the
    /// 1025th unreadable path could heal and buy nothing: no rebuild is armed, and the module
    /// stays missing for the life of the daemon. Nothing on the fact stream announces a
    /// restored permission, which is asserted here by holding the observation still.
    #[test]
    fn recovery_tracks_cap_minus_one_cap_and_cap_plus_one() {
        for required in [1023usize, 1024, 1025, 4096] {
            for order in ["forward", "reverse"] {
                let mut keys: Vec<String> = (0..required)
                    .map(|i| format!("/ws/CommonModules/М{i:05}/Ext/Module.bsl"))
                    .collect();
                if order == "reverse" {
                    keys.reverse();
                }
                let declared: Vec<&str> = keys.iter().map(String::as_str).collect();
                let mut debt = GraphDebt::default();
                let mut now = Instant::now();
                debt.record_change(now, 1);
                debt.charge_admission(1, now, false);
                publish_declaring(&mut debt, now, Some(1), 1, &declared, None);

                // Twenty-one full passes over the whole required set, nothing healed.
                for pass in 0..21 {
                    now += RECOVERY_PROBE_CAP;
                    let levels = keys
                        .iter()
                        .map(|key| (Capability::Open(key.clone()), Level::Denied))
                        .collect();
                    assert_eq!(
                        probe(&mut debt, now, levels),
                        ProbeResult::NoNewEvidence,
                        "{required}/{order}: pass {pass} bought a build for nothing",
                    );
                }

                // The LAST required path opens. One credit, whatever the size of the set.
                now += RECOVERY_PROBE_CAP;
                let mut levels: Vec<(Capability, Level)> =
                    keys.iter().map(|key| (Capability::Open(key.clone()), Level::Denied)).collect();
                levels.last_mut().expect("a non-empty required set").1 = Level::Granted;
                assert_eq!(
                    probe(&mut debt, now, levels),
                    ProbeResult::NewEvidence,
                    "{required}/{order}: the last required path healed and bought nothing",
                );
                assert!(
                    debt.owes_recovery_build(),
                    "{required}/{order}: and no build is owed for it",
                );
            }
        }
    }

    /// A saturated unread set does not swallow the scan capability.
    ///
    /// The walk's verdict was appended AFTER every unread path, so on a workspace with enough
    /// unreadable modules the one capability that retires a straddled publication was the
    /// first thing the cap threw away.
    #[test]
    fn saturated_unread_does_not_hide_scan_recovery() {
        let scope = a_scope(&["/ws"], true);
        let keys: Vec<String> =
            (0..1024).map(|i| format!("/ws/CommonModules/М{i:05}/Ext/Module.bsl")).collect();
        let declared: Vec<&str> = keys.iter().map(String::as_str).collect();
        let mut debt = GraphDebt::default();
        let mut now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &declared, Some((scope.clone(), false)));

        let mut levels: Vec<(Capability, Level)> =
            keys.iter().map(|key| (Capability::Open(key.clone()), Level::Denied)).collect();
        levels.push((Capability::ScanRoots, Level::Granted));
        assert_eq!(
            probe_walking(&mut debt, now, levels.clone(), Some(scope.clone())),
            ProbeResult::NewEvidence,
            "a complete walk behind a saturated unread set confirmed nothing",
        );

        // And only once: the same clean walk is not a second validation.
        let cutoff = debt.capture_recovery();
        debt.record_publication(
            now,
            Some(2),
            true,
            Some(cutoff),
            RecoveryPublicationProof {
                generation: 2,
                captured_seq: cutoff,
                declared_unread: Some(keys.clone()),
                scan_complete: Some(false),
                scope: Some(scope.clone()),
                ..Default::default()
            },
        );
        now += RECOVERY_PROBE_CAP;
        assert_eq!(
            probe_walking(&mut debt, now, levels, Some(scope)),
            ProbeResult::NoNewEvidence,
            "the same complete walk was validated twice",
        );
    }

    /// A receipt measured against an obsolete basis is rejected WHOLE — including names the
    /// memory has never seen.
    ///
    /// The basis was compared per key, and a key nobody remembers has no basis to compare
    /// against: an old probe could therefore introduce a brand new capability, positive, and
    /// be paid for it long after the publication whose gaps it was measuring was replaced.
    /// Every form of straggler is rejected here, not only the one that names something known.
    #[test]
    fn every_obsolete_basis_rejects_all_names() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(
            &mut debt,
            now,
            Some(1),
            1,
            &["/ws/Знакомый.bsl", "/ws/Ушедший.bsl"],
            None,
        );

        // Measured once, so there is a level for the straggler to overwrite.
        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/Знакомый.bsl"), Level::Granted)),
            ProbeResult::NewEvidence,
        );
        // And one address answered and gone, so a straggler can try to bring it back.
        let retired = covering(&debt, 2, &["/ws/Ушедший.bsl"], &["/ws/Знакомый.bsl"]);
        debt.record_publication(now, Some(1), false, None, retired);
        assert_eq!(outstanding_keys(&debt), vec!["/ws/Знакомый.bsl".to_owned()]);

        let mut generation = 2;
        for (what, levels) in [
            ("a known key", measured(an_unread("/ws/Знакомый.bsl"), Level::Denied)),
            ("a never-seen key", measured(an_unread("/ws/Никогда.bsl"), Level::Granted)),
            ("an answered key", measured(an_unread("/ws/Ушедший.bsl"), Level::Granted)),
            (
                "a mixed batch",
                vec![
                    (an_unread("/ws/Знакомый.bsl"), Level::Denied),
                    (an_unread("/ws/Никогда.bsl"), Level::Granted),
                ],
            ),
        ] {
            // Its OWN reservation, taken while the basis below still stands. Reusing one
            // token made every case after the first a test of the token guard: the whole
            // point here is the BASIS, which has to be what refuses a batch whose names the
            // memory would otherwise accept.
            let plan = debt.reserve_probe().expect("the obligation still stands");
            generation += 1;
            publish_declaring(
                &mut debt,
                now,
                Some(generation),
                generation,
                &["/ws/Знакомый.bsl"],
                None,
            );
            let owed_before = debt.owes_recovery_build();
            let keys_before = outstanding_keys(&debt);
            assert_eq!(
                debt.finish_probe(
                    now,
                    ProbeReceipt { token: plan.token, basis: plan.basis, levels, scope: None },
                ),
                ProbeResult::Obsolete,
                "{what}: a receipt from a replaced publication was believed",
            );
            assert_eq!(
                debt.owes_recovery_build(),
                owed_before,
                "{what}: and it changed what is owed",
            );
            assert_eq!(
                outstanding_keys(&debt),
                keys_before,
                "{what}: and it changed what is required",
            );
        }

        // A token nobody handed out, on a basis that does stand: it is not this walk, so it
        // says nothing AND it does not give the real walker's reservation away.
        let plan = debt.reserve_probe().expect("the obligation still stands");
        assert_eq!(
            debt.finish_probe(
                now,
                ProbeReceipt {
                    token: plan.token.saturating_add(7),
                    basis: plan.basis,
                    levels: measured(an_unread("/ws/Знакомый.bsl"), Level::Denied),
                    scope: None,
                },
            ),
            ProbeResult::Obsolete,
            "a receipt from a walk nobody reserved was believed",
        );
        assert!(
            debt.reserve_probe().is_none(),
            "a stranger's receipt released the walk the real owner is still on",
        );
        assert_eq!(
            debt.finish_probe(
                now,
                ProbeReceipt {
                    token: plan.token,
                    basis: plan.basis,
                    levels: measured(an_unread("/ws/Знакомый.bsl"), Level::Granted),
                    scope: None,
                },
            ),
            ProbeResult::NoNewEvidence,
            "the rejected receipts left their levels behind after all",
        );

        // Nothing any of them said was recorded, and the answered address stayed answered.
        assert_eq!(outstanding_keys(&debt), vec!["/ws/Знакомый.bsl".to_owned()]);
        assert!(
            debt.standing(now + RECOVERY_PROBE_CAP, ready()).recovery.is_some(),
            "the repeated rejections left the obligation with no owner at all",
        );
    }

    /// A publication that proves it read a path retires THAT path, while another stays unread.
    ///
    /// Waiting for a globally coherent episode to release anything is what made the memory
    /// grow without bound: a repaired module is answered by the build that read it, whatever
    /// the rest of the tree is still missing.
    #[test]
    fn repaired_paths_retire_while_another_path_remains_unread() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Свой.bsl", "/ws/Чужой.bsl"], None);
        assert_eq!(outstanding_keys(&debt), vec!["/ws/Свой.bsl", "/ws/Чужой.bsl"]);

        // The build read one of them and still could not read the other.
        let covered = covering(&debt, 2, &["/ws/Свой.bsl"], &["/ws/Чужой.bsl"]);
        debt.record_publication(now, Some(2), true, Some(0), covered);
        assert_eq!(
            outstanding_keys(&debt),
            vec!["/ws/Чужой.bsl"],
            "the path the build proved it read is still outstanding",
        );

        // A positive probe on its own retires nothing: only an installed result can.
        probe(&mut debt, now, measured(an_unread("/ws/Чужой.bsl"), Level::Granted));
        assert_eq!(
            outstanding_keys(&debt),
            vec!["/ws/Чужой.bsl"],
            "a probe that opened a file retired the obligation by itself",
        );
    }

    /// Long churn with a proof for every path that goes: the memory is bounded by what is
    /// OUTSTANDING, not by how much has ever passed through it.
    #[test]
    fn retired_paths_leave_no_trace_behind_them() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Якорь.bsl"], None);

        let mut peak = 0;
        for round in 0..10_000u64 {
            let repaired = format!("/ws/Сменный{round}.bsl");
            publish_declaring(
                &mut debt,
                now,
                Some(1),
                2 + round * 2,
                &["/ws/Якорь.bsl", repaired.as_str()],
                None,
            );
            peak = peak.max(debt.outstanding_recovery().keys.len());
            let covered = covering(&debt, 3 + round * 2, &[repaired.as_str()], &["/ws/Якорь.bsl"]);
            debt.record_publication(now, Some(1), true, Some(0), covered);
        }
        assert_eq!(
            outstanding_keys(&debt),
            vec!["/ws/Якорь.bsl"],
            "ten thousand answered paths left something behind",
        );
        assert_eq!(peak, 2, "the memory grew past what was outstanding at once: {peak}");
    }

    /// A path that simply stopped being mentioned is NOT answered.
    ///
    /// An enumeration that came up short proves nothing about what it failed to list, and
    /// retiring on the difference between two unread sets is how a real obligation disappears
    /// silently. It stays required, it stays observed, and its next positive is still news.
    #[test]
    fn omission_from_incomplete_unread_is_not_retirement() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Тихий.bsl", "/ws/Громкий.bsl"], None);
        probe(&mut debt, now, measured(an_unread("/ws/Тихий.bsl"), Level::Granted));
        let spent = debt.capture_recovery();
        assert!(spent > 0, "the healing was measured");

        // A shorter, unsound enumeration that does not mention it, and proves nothing.
        publish_declaring(&mut debt, now, Some(2), 2, &["/ws/Громкий.bsl"], None);
        assert!(
            outstanding_keys(&debt).contains(&"/ws/Тихий.bsl".to_owned()),
            "an omission from a short enumeration answered an obligation",
        );

        // It is still observed — and the level it already had is still remembered, so the same
        // positive is not news twice.
        let plan = debt.reserve_probe().expect("the obligation still stands");
        assert!(
            plan.open.contains(&"/ws/Тихий.bsl".to_owned()),
            "the retained obligation lost its observer: {:?}",
            plan.open,
        );
        assert_eq!(
            debt.finish_probe(
                now,
                ProbeReceipt {
                    token: plan.token,
                    basis: plan.basis,
                    levels: measured(an_unread("/ws/Тихий.bsl"), Level::Granted),
                    scope: None,
                },
            ),
            ProbeResult::NoNewEvidence,
            "the same positive was paid for twice",
        );

        // A path hidden from the newest enumeration can still heal, and that IS news.
        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/Тихий.bsl"), Level::Denied)),
            ProbeResult::NoNewEvidence,
            "a measured negative bought a build",
        );
        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/Тихий.bsl"), Level::Granted)),
            ProbeResult::NewEvidence,
            "a genuine negative-to-positive outside the newest enumeration went unnoticed",
        );
    }

    /// Recovery credits are spent by the claim that captured them, and only by it.
    ///
    /// A decision is not a claim, a held lease is not a claim, and an improvement measured
    /// after the slot was granted belongs to the NEXT one — whatever this build goes on to do.
    #[test]
    fn recovery_credits_are_claim_bound_not_bankable() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Первый.bsl", "/ws/Второй.bsl"], None);

        // Two paths heal in one pass: one build answers both.
        assert_eq!(
            probe(
                &mut debt,
                now,
                vec![
                    (an_unread("/ws/Первый.bsl"), Level::Granted),
                    (an_unread("/ws/Второй.bsl"), Level::Granted),
                ],
            ),
            ProbeResult::NewEvidence,
        );
        assert!(debt.owes_recovery_build());

        // Deciding is not claiming: nothing is spent until a slot is granted.
        debt.decide(now, ready());
        assert!(debt.owes_recovery_build(), "a decision spent the credit");

        let cutoff = debt.capture_recovery();
        // An improvement measured AFTER the claim keeps its own demand.
        publish_declaring(&mut debt, now, Some(1), 2, &["/ws/Третий.bsl"], None);
        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/Третий.bsl"), Level::Granted)),
            ProbeResult::NewEvidence,
        );

        // The ticket's publication answers what it captured, and no more.
        debt.record_publication(
            now,
            Some(2),
            true,
            Some(cutoff),
            RecoveryPublicationProof {
                generation: 3,
                captured_seq: cutoff,
                declared_unread: Some(vec!["/ws/Третий.bsl".to_owned()]),
                ..Default::default()
            },
        );
        assert!(
            debt.owes_recovery_build(),
            "the build answered an improvement measured after it was admitted",
        );
    }

    /// A removal is a measured change of composition, and it is news exactly once.
    ///
    /// Treating a path that is GONE as one more failed open leaves the publication owed a
    /// module nothing can ever supply: the rebuild that would drop it from the unread set is
    /// never armed, and the probe goes on opening a name that no longer exists.
    #[test]
    fn a_removed_unread_path_is_news_once() {
        let mut debt = GraphDebt::default();
        let mut now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Удалённый.bsl"], None);

        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/Удалённый.bsl"), Level::Absent)),
            ProbeResult::NewEvidence,
            "a path that is gone was not measured at all",
        );

        // Answered by the rebuild, and then measured again: absence does not become news a
        // second time. The path is still REQUIRED — only a proof of coverage retires it — so
        // the probe goes on measuring it and goes on finding nothing new.
        let cutoff = debt.capture_recovery();
        debt.record_publication(
            now,
            Some(2),
            true,
            Some(cutoff),
            RecoveryPublicationProof {
                generation: 2,
                captured_seq: cutoff,
                declared_unread: Some(vec!["/ws/Удалённый.bsl".to_owned()]),
                ..Default::default()
            },
        );
        for _ in 0..3 {
            now += RECOVERY_PROBE_CAP;
            assert_eq!(
                probe(&mut debt, now, measured(an_unread("/ws/Удалённый.bsl"), Level::Absent)),
                ProbeResult::NoNewEvidence,
                "a path still gone armed another rebuild",
            );
        }
    }

    /// A publication that straddled a write measured NOTHING about the walk, so the first
    /// complete walk over it is a real first confirmation — and only the first, however many
    /// unsound publications follow.
    ///
    /// The two halves are one rule about seeds. An unread path starts negative because the
    /// build actually tried to open it; the walk does not, because "the build was unsound"
    /// says nothing about whether the roots were walked end to end. Seeding the walk negative
    /// too would hand out that validation credit again on every unsound publication.
    #[test]
    fn a_straddled_publication_validates_its_scan_once_per_episode() {
        let scope = a_scope(&["/ws"], true);
        let mut debt = GraphDebt::default();
        let mut now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &[], Some((scope.clone(), false)));

        assert_eq!(
            probe_walking(
                &mut debt,
                now,
                vec![(Capability::ScanRoots, Level::Granted)],
                Some(scope.clone()),
            ),
            ProbeResult::NewEvidence,
            "the first complete walk confirms nothing",
        );

        // The build it armed comes back unsound, twice, and a failure lands in between. The
        // episode keeps what it measured through all of it.
        now += Duration::from_secs(1);
        let cutoff = debt.capture_recovery();
        for generation in [2, 3] {
            debt.record_publication(
                now,
                Some(2),
                true,
                Some(cutoff),
                RecoveryPublicationProof {
                    generation,
                    captured_seq: cutoff,
                    declared_unread: Some(Vec::new()),
                    scan_complete: Some(false),
                    scope: Some(scope.clone()),
                    ..Default::default()
                },
            );
            debt.record_failure(
                now,
                FailureKind::Operation,
                Sponsors { primary: true, marks: false },
            );
        }
        for _ in 0..3 {
            now += RECOVERY_PROBE_CAP;
            assert_eq!(
                probe_walking(
                    &mut debt,
                    now,
                    vec![(Capability::ScanRoots, Level::Granted)],
                    Some(scope.clone()),
                ),
                ProbeResult::NoNewEvidence,
                "the same complete walk was validated again on a later unsound publication",
            );
        }

        // A different scope IS a different capability: the declared roots changed, and what
        // was walked is not what was walked before.
        let wider = a_scope(&["/ws", "/ws-ext"], true);
        publish_declaring(&mut debt, now, Some(4), 4, &[], Some((wider.clone(), false)));
        assert_eq!(
            probe_walking(
                &mut debt,
                now,
                vec![(Capability::ScanRoots, Level::Granted)],
                Some(wider),
            ),
            ProbeResult::NewEvidence,
            "a walk over changed roots measured nothing",
        );
    }

    /// What a probe measured survives a failure and a new unsound publication — and the pace
    /// the probing earned survives with it.
    #[test]
    fn a_healing_already_measured_survives_a_failure_and_a_new_publication() {
        let mut debt = GraphDebt::default();
        let mut now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Модуль.bsl"], None);

        let healed = measured(an_unread("/ws/Модуль.bsl"), Level::Granted);
        assert_eq!(
            probe(&mut debt, now, healed.clone()),
            ProbeResult::NewEvidence,
            "the measured transition must arm a build",
        );

        now += Duration::from_secs(1);
        let cutoff = debt.capture_recovery();
        debt.record_failure(now, FailureKind::Operation, Sponsors { primary: true, marks: false });
        debt.record_publication(
            now,
            Some(2),
            true,
            Some(cutoff),
            RecoveryPublicationProof {
                generation: 2,
                captured_seq: cutoff,
                declared_unread: Some(vec!["/ws/Модуль.bsl".to_owned()]),
                ..Default::default()
            },
        );
        assert_eq!(
            debt.probe_interval(),
            Some(RECOVERY_PROBE * 2),
            "the unsound publication handed the pause back to the floor",
        );

        assert_eq!(
            probe(&mut debt, now, healed),
            ProbeResult::NoNewEvidence,
            "the same healing, re-measured after a failure and a publication, bought a rebuild",
        );
        assert_eq!(
            debt.probe_interval(),
            Some(RECOVERY_PROBE * 4),
            "the probe that measured nothing new did not back off",
        );
    }

    /// A look taken against an older publication than what is already remembered describes
    /// gaps that publication had. It is not news about the one standing now, and it does not
    /// overwrite what a later look measured.
    #[test]
    fn a_look_older_than_what_is_remembered_measures_nothing() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Модуль.bsl"], None);

        // A probe reserves against the publication that stands.
        let plan = debt.reserve_probe().expect("an outstanding obligation owes a probe");

        // A NEWER publication lands while it is out; the basis it holds is gone.
        publish_declaring(&mut debt, now, Some(2), 2, &["/ws/Модуль.bsl"], None);

        // Its receipt is rejected whole — the name it knows, and a name this memory has never
        // seen.
        let stale = ProbeReceipt {
            token: plan.token,
            basis: plan.basis,
            levels: vec![
                (an_unread("/ws/Модуль.bsl"), Level::Granted),
                (an_unread("/ws/Никогда.bsl"), Level::Granted),
            ],
            scope: None,
        };
        assert_eq!(
            debt.finish_probe(now, stale),
            ProbeResult::Obsolete,
            "a receipt from a replaced publication was believed",
        );
        assert!(!debt.owes_recovery_build(), "and it was paid for");

        // And it left no mark: the level that stands is still unmeasured, so the next probe
        // measuring the healing is news.
        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/Модуль.bsl"), Level::Granted)),
            ProbeResult::NewEvidence,
            "the rejected receipt recorded its levels after all",
        );
    }

    /// A publication that comes back unsound does not erase what the probe already measured,
    /// and it does not hand the pause back to the floor. The episode keeps its memory and its
    /// earned backoff: 60 → 120 → 240 → 480, capped.
    #[test]
    fn unsound_recovery_keeps_its_witness_and_backoff() {
        let mut debt = GraphDebt::default();
        let mut now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Модуль.bsl"], None);
        assert_eq!(debt.probe_interval(), Some(RECOVERY_PROBE));

        // A real healing, then a build that comes back unsound all the same.
        let healed = measured(an_unread("/ws/Модуль.bsl"), Level::Granted);
        assert_eq!(
            probe(&mut debt, now, healed.clone()),
            ProbeResult::NewEvidence,
            "the measured transition must arm a build",
        );
        now += Duration::from_secs(1);
        let cutoff = debt.capture_recovery();
        debt.record_publication(
            now,
            Some(2),
            true,
            Some(cutoff),
            RecoveryPublicationProof {
                generation: 2,
                captured_seq: cutoff,
                declared_unread: Some(vec!["/ws/Модуль.bsl".to_owned()]),
                ..Default::default()
            },
        );
        assert_eq!(
            debt.probe_interval(),
            Some(RECOVERY_PROBE * 2),
            "a publication that came back unsound pays one step of the backoff itself",
        );

        // The SAME level after the unsound publication is not a second healing.
        for expected in [RECOVERY_PROBE * 4, RECOVERY_PROBE * 8, RECOVERY_PROBE * 8] {
            now += RECOVERY_PROBE_CAP;
            assert_eq!(
                probe(&mut debt, now, healed.clone()),
                ProbeResult::NoNewEvidence,
                "the same level armed another rebuild",
            );
            assert_eq!(
                debt.probe_interval(),
                Some(expected.min(RECOVERY_PROBE_CAP)),
                "the backoff was handed back to the floor by an unsound publication",
            );
        }
        assert_eq!(debt.probe_interval(), Some(RECOVERY_PROBE_CAP), "the cap was never reached");
    }

    /// A marks demand joins a live retry's mode only while its OWN budget is open. Asking
    /// `Behind(Failed)` instead answers a different question — who owns the slot — and a
    /// demand whose deadline has passed went on forcing a full project reload out of every
    /// retry the failed lane still had.
    ///
    /// The expiry here is a real one: a long admitted build outlasts the marks' own window.
    #[test]
    fn expired_marks_do_not_force_a_live_retry() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 1);
        debt.settle_marks(now, false);
        // The marks' own 600s runs from their first sponsored admission.
        debt.spend_mark_attempt(now + OWED_MARKS_GRACE);

        // The retry lane opens LATER, so it is still alive when the marks lane has run out.
        // That gap is the whole point: the question is what a live retry inherits, and a test
        // where both lanes expire together cannot tell presence from eligibility.
        let failure = now + Duration::from_secs(300);
        debt.record_failure(
            failure,
            FailureKind::Transient,
            Sponsors { primary: true, marks: false },
        );

        // Both open: the retry carries the marks' mode. The positive control.
        assert!(
            debt.decide(failure, ready()).start.is_some_and(|start| start.forced),
            "an open marks account must be able to attach to the live retry",
        );

        // Past the marks' deadline, inside the retry's.
        let after = now
            + OWED_MARKS_GRACE
            + crate::state::retry_window::DEFAULT_RETRY_BUDGET
            + Duration::from_secs(1);
        let standing = debt.standing(after, ready());
        assert!(
            matches!(standing.failed, Some(Ripeness::Now)),
            "the stand needs a retry that is still live and due: {:?}",
            standing.failed,
        );
        assert!(
            !debt.marks_eligible(after),
            "the stand needs a marks account whose own budget has run out",
        );
        let start = debt.decide(after, ready()).start;
        assert!(start.is_some(), "the live retry must still run: {start:?}");
        assert!(
            !start.is_some_and(|start| start.forced),
            "an expired marks demand still forced a project reload out of the live retry",
        );
    }

    /// Fresh marks revive a spent marks account; the SAME marks do not. The credit is the new
    /// demand, and a repeat of one already answered is not a new demand.
    #[test]
    fn a_new_fact_revives_spent_marks_without_reusing_old_credit() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 1);
        debt.settle_marks(now, false);
        // Spend the marks budget the way a run of forced builds does.
        debt.stop_marks_budget();
        let spent = debt.standing(now, ready()).marks;
        assert!(
            matches!(spent, Some(Ripeness::Exhausted(Revival::FreshMarks))),
            "the stand needs a spent marks account: {spent:?}",
        );

        // The same placement again: nothing new is demanded.
        debt.place_marks(now, 5, 1);
        assert!(
            matches!(
                debt.standing(now, ready()).marks,
                Some(Ripeness::Exhausted(Revival::FreshMarks))
            ),
            "a repeat of the same marks bought another admission",
        );

        // Marks actually placed anew are the external work it was waiting for.
        debt.place_marks(now, 9, 4);
        assert!(
            matches!(debt.standing(now, ready()).marks, Some(Ripeness::Now | Ripeness::At(_))),
            "new marks did not revive the spent account",
        );
    }

    /// BUD-02. An outcome closes the accounts of ITS admission. A build sponsored by marks
    /// alone that hits a non-retryable error closes the marks account — the one that paid —
    /// and does not leave a primary retry budget behind it that nothing ever bought.
    #[test]
    fn marks_operation_closes_its_sponsor() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 1);
        debt.settle_marks(now, false);
        let at = now + OWED_MARKS_GRACE;
        let sponsors = debt.charge_admission(1, at, true);
        assert_eq!(
            sponsors,
            Sponsors { primary: false, marks: true },
            "the stand needs the marks, and only the marks, to pay",
        );

        assert!(!debt.record_failure(at, FailureKind::Operation, sponsors));

        assert!(
            !debt.marks_eligible(at),
            "the account that paid must be closed by its own Operation",
        );
        assert!(
            debt.decide(at, ready()).start.is_none(),
            "a closed marks account kept building: {:?}",
            debt.standing(at, ready()),
        );
    }

    /// BUD-02. A claim nothing but the marks asked for names the marks and nobody else.
    ///
    /// The primary lane is not a default. Naming it on a claim it never asked for hands the
    /// outcome to an account that paid nothing — which is how a marks-only failure came to
    /// mint a primary retry budget, and how an Operation came to close the wrong book.
    #[test]
    fn marks_only_claim_names_only_its_real_sponsor() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 1);
        assert!(debt.settle_marks(now, false));
        let at = now + OWED_MARKS_GRACE;

        let chosen = debt.decide(at, ready()).start.expect("marks are due");
        assert!(chosen.forced, "marks buy a forced build");
        assert_eq!(chosen.kind, BuildKind::Reload);
        // Nothing else is owed: no delivered change, no forced demand, no failure.
        assert!(debt.owes_change().is_none());
        assert!(debt.owes_forced().is_none());
        assert!(!debt.owes_failed());

        assert_eq!(
            debt.charge_admission(1, at, chosen.forced),
            Sponsors { primary: false, marks: true },
            "the claim named a primary sponsor that never asked for it",
        );
    }

    /// BUD-02. And because it named them, a transient failure of that build is paced by the
    /// marks' own account instead of opening a primary retry budget of its own.
    #[test]
    fn marks_only_transient_cannot_mint_primary_failure_account() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 1);
        assert!(debt.settle_marks(now, false));
        let at = now + OWED_MARKS_GRACE;
        let chosen = debt.decide(at, ready()).start.expect("marks are due");
        let sponsors = debt.charge_admission(1, at, chosen.forced);

        debt.record_failure(at, FailureKind::Transient, sponsors);

        assert!(!debt.owes_failed(), "a marks-origin failure minted a primary retry budget");
    }

    /// BUD-02. A claim both lanes paid for spends BOTH: the marks account that financed it is
    /// closed by its own Operation, not left open for the same build to be asked for again.
    #[test]
    fn joined_operation_closes_marks_that_paid_this_claim() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 1);
        assert!(debt.settle_marks(now, false));
        let at = now + OWED_MARKS_GRACE;
        debt.record_forced(at, 1);

        let sponsors = debt.charge_admission(1, at, true);
        assert_eq!(
            sponsors,
            Sponsors { primary: true, marks: true },
            "the stand needs both lanes to pay",
        );

        debt.record_failure(at, FailureKind::Operation, sponsors);

        assert!(
            !debt.marks_are_eligible(at),
            "the marks account that paid for this claim was left open by its own Operation",
        );
    }

    /// A marks demand placed AFTER the admission is not closed by that admission's outcome.
    ///
    /// The sponsor an outcome closes has to be the demand that PAID for it. The marks account
    /// is one schedule, so a placement arriving while the build runs joins it — and an
    /// Operation that then closes "the marks account" spends a demand nobody has served, for
    /// a build admitted before it existed. That is the same defect the primary lane names
    /// explicitly: a failure buys nothing, and what opens the next epoch is a credit nobody
    /// spent.
    #[test]
    fn a_marks_demand_placed_after_the_claim_survives_its_outcome() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 1);
        debt.settle_marks(now, false);
        let at = now + OWED_MARKS_GRACE;
        let sponsors = debt.charge_admission(1, at, true);
        assert!(sponsors.marks, "the stand needs the marks to pay");

        // A context the consumer marked while the build was running: work this build was never
        // admitted for and cannot have answered.
        debt.place_marks(at, 9, 2);
        debt.record_failure(at, FailureKind::Operation, sponsors);

        assert!(
            debt.marks_are_eligible(at),
            "the outcome closed a demand placed after the admission it belongs to",
        );
        let standing = debt.standing(at, ready());
        assert!(is_live(standing.marks), "and nothing will ever build for it: {standing:?}",);

        // The control: with NO later demand, the same Operation closes the account that paid.
        let mut spent = GraphDebt::default();
        spent.place_marks(now, 5, 1);
        spent.settle_marks(now, false);
        let paid = spent.charge_admission(1, at, true);
        spent.record_failure(at, FailureKind::Operation, paid);
        assert!(
            !spent.marks_are_eligible(at),
            "the account that paid must still be closed by its own Operation",
        );
        assert!(
            !is_live(spent.standing(at, ready()).marks),
            "and nothing may start for it: {:?}",
            spent.standing(at, ready()),
        );
    }

    /// BUD-02. A marks-only build whose long run outlasts the marks' own budget must not mint
    /// a primary retry account on the way out: that lane never paid for anything.
    #[test]
    fn expired_marks_outcome_does_not_mint_primary() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.place_marks(now, 5, 1);
        debt.settle_marks(now, false);
        let at = now + OWED_MARKS_GRACE;
        let sponsors = debt.charge_admission(1, at, true);

        // The build runs past the marks' own deadline and then fails transiently.
        let after = at + crate::state::retry_window::DEFAULT_RETRY_BUDGET + Duration::from_secs(1);
        debt.record_failure(after, FailureKind::Transient, sponsors);

        assert!(
            !debt.owes_failed(),
            "a marks-only outcome minted a primary retry budget with 600 fresh seconds",
        );
        assert!(
            debt.decide(after, ready()).start.is_none(),
            "and it started another build on it: {:?}",
            debt.standing(after, ready()),
        );
    }

    /// BUD-02. Marks that JOIN a live retry pay on the claim, inside their grace or not. The
    /// attachment is the work their budget buys, and skipping the charge while their first
    /// pause had not elapsed left an account whose deadline was never opened at all.
    #[test]
    fn marks_attached_in_grace_pay_on_claim() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        debt.record_failure(now, FailureKind::Transient, Sponsors { primary: true, marks: false });
        // Placed a moment ago: still inside the 2s grace.
        debt.place_marks(now, 5, 2);
        debt.settle_marks(now, false);

        let inside_grace = now + Duration::from_millis(500);
        let sponsors = debt.charge_admission(2, inside_grace, true);
        assert!(sponsors.marks, "the marks joined this retry");

        // Their own budget is now running: past it, they are exhausted rather than for ever
        // open.
        let after = inside_grace
            + crate::state::retry_window::DEFAULT_RETRY_BUDGET
            + Duration::from_secs(1);
        assert!(
            !debt.marks_eligible(after),
            "the attachment never opened the marks' deadline, so it never runs out",
        );
    }

    /// BUD-04. The walk before a claim takes seconds, and a deadline can pass inside it. The
    /// admission is the line, so eligibility is asked THERE — not only where the decision was
    /// taken, which was before the walk.
    #[test]
    fn a_claim_after_a_long_walk_revalidates_its_sponsor() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        debt.record_failure(now, FailureKind::Transient, Sponsors { primary: true, marks: false });
        debt.change_answered(1);
        assert!(
            matches!(debt.standing(now, ready()).failed, Some(Ripeness::Now)),
            "the stand needs a retry that is due when the decision is taken",
        );

        // The walk runs past the retry's own deadline.
        let after = now + crate::state::retry_window::DEFAULT_RETRY_BUDGET + Duration::from_secs(1);
        let sponsors = debt.charge_admission(1, after, false);
        assert!(
            !sponsors.any(),
            "the claim was granted on a sponsor whose budget had already run out",
        );
    }

    /// R1. A healing the probe MEASURED is external evidence in its own right. Addressing it
    /// with the hub's number spends it against a frontier that number already passed: the
    /// claim that ran before the failure moved the frontier to the same place, so the one
    /// thing that could revive the spent budget arrives already spent.
    ///
    /// No file event is involved, and none is required: a permission restored or a subtree
    /// that became readable moves no hub sequence at all.
    #[test]
    fn a_measured_healing_revives_a_spent_budget_without_a_new_fact() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        // An unsound publication under the fact the admission was charged for.
        debt.record_change(now, 7);
        debt.charge_admission(7, now, false);
        publish_declaring(&mut debt, now, Some(7), 7, &["/ws/Модуль.bsl"], None);
        // That build's retry spends the budget for good.
        debt.record_failure(now, FailureKind::Operation, Sponsors { primary: true, marks: false });
        assert!(
            matches!(debt.standing(now, ready()).failed, Some(Ripeness::Exhausted(_))),
            "the stand needs a spent budget",
        );

        // The hub has not moved: nothing was written. The probe measures a capability that has
        // genuinely come back.
        let healed = measured(an_unread("/ws/Модуль.bsl"), Level::Granted);
        assert_eq!(probe(&mut debt, now, healed), ProbeResult::NewEvidence);

        assert!(
            debt.decide(now, ready()).start.is_some_and(|start| start.forced),
            "a measured healing bought no build: {:?}",
            debt.standing(now, ready()),
        );
    }

    /// R3. A healing is news, not a reason to start paying for probes at the floor again. The
    /// pace the probing earned — and the flap latch it earned with it — belongs to the probe's
    /// own schedule, and a delivery is the only thing entitled to reset it.
    #[test]
    fn a_measured_healing_keeps_the_pace_the_probing_earned() {
        let mut debt = GraphDebt::default();
        let mut now = Instant::now();
        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Модуль.bsl"], None);

        // Earn the backoff: three fruitless probes at the same level.
        for _ in 0..3 {
            now += RECOVERY_PROBE_CAP;
            probe(&mut debt, now, measured(an_unread("/ws/Модуль.bsl"), Level::Denied));
        }
        assert_eq!(debt.probe_interval(), Some(RECOVERY_PROBE_CAP), "the stand needs a cap");

        // A genuine measured improvement.
        now += RECOVERY_PROBE_CAP;
        probe(&mut debt, now, measured(an_unread("/ws/Модуль.bsl"), Level::Granted));

        assert_eq!(
            debt.probe_interval(),
            Some(RECOVERY_PROBE_CAP),
            "the measured healing handed the probe's earned pace back to the floor",
        );
    }

    /// BUD-01. A consumer repeating a delivery the graph has already ANSWERED is not the world
    /// changing twice. A duplicate below the answered line buys no build and revives nothing.
    #[test]
    fn a_delivery_already_answered_buys_nothing_when_it_repeats() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();

        // Forced work, admitted, and answered by a forced publication.
        debt.record_forced(now, 4);
        debt.charge_admission(4, now, true);
        publish(&mut debt, now, Some(4), true, true);
        assert_eq!(debt.owes_forced(), None, "the proof answered it");

        // The second cursor delivers the SAME fact.
        debt.record_forced(now, 4);
        assert!(
            debt.decide(now, ready()).start.is_none(),
            "a repeat of answered work started a build: {:?}",
            debt.standing(now, ready()),
        );

        // And the same repeat cannot revive a marks account its own expiry closed.
        let mut marks = GraphDebt::default();
        marks.place_marks(now, 5, 2);
        marks.settle_marks(now, false);
        marks.charge_admission(2, now, true);
        publish(&mut marks, now, Some(2), true, true);
        marks.stop_marks_budget();
        marks.record_change(now, 2);
        assert!(
            matches!(marks.standing(now, ready()).marks, None | Some(Ripeness::Exhausted(_))),
            "a repeat of answered work revived a spent marks account: {:?}",
            marks.standing(now, ready()).marks,
        );
    }

    /// The first H1 alias: a credit an answer RETIRED cannot finance a failure that comes
    /// after it. One external fact buys one first attempt; a publication proving that fact
    /// answered spends nothing new, and the failure of a LATER build has to name its own
    /// unused credit or stop.
    #[test]
    fn answered_credit_cannot_finance_a_later_failure() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();

        debt.record_change(now, 1);
        debt.charge_admission(1, now, false);
        publish(&mut debt, now, Some(1), false, true);
        assert_eq!(debt.owes_change(), None, "the proof covered fact 1");

        // A build that answers nothing new and fails. Nothing above the frontier stands, so
        // there is no credit left to open another epoch.
        assert!(
            !debt.record_failure(
                now,
                FailureKind::Operation,
                Sponsors { primary: true, marks: false }
            ),
            "a retired credit financed a later failure",
        );
        let standing = debt.standing(now, ready());
        assert!(
            matches!(standing.failed, Some(Ripeness::Exhausted(_))),
            "the budget must be spent: {:?}",
            standing.failed,
        );
        assert!(debt.decide(now, ready()).start.is_none(), "a spent budget started a build");
    }

    /// The second H1 alias: a comparison answers exactly what its cutoff covers, and nothing
    /// past it. A fact delivered ABOVE the cutoff is not answered by that comparison, stays
    /// owed, and is the credit the next admission spends.
    #[test]
    fn comparison_answer_retires_only_covered_credits() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();

        debt.record_change(now, 3);
        debt.charge_admission(3, now, false);
        // Delivered while the comparison was walking: above its cutoff.
        debt.record_change(now, 7);

        debt.change_answered(3);
        assert_eq!(
            debt.owes_change(),
            Some(7),
            "the comparison retired a fact its cutoff never covered",
        );
        // And that surviving fact is a credit: it opens the next epoch, exactly once.
        assert!(
            debt.record_failure(
                now,
                FailureKind::Operation,
                Sponsors { primary: true, marks: false }
            ),
            "the uncovered fact did not pay for the next attempt",
        );
        debt.charge_admission(7, now, false);
        assert!(
            !debt.record_failure(
                now,
                FailureKind::Operation,
                Sponsors { primary: true, marks: false }
            ),
            "the same fact paid twice",
        );
    }

    /// The third H1 alias: work retained through a build pays for the NEXT claim, and that is
    /// the whole of what it buys. Its own failure is not a second epoch.
    #[test]
    fn pending_work_pays_for_its_next_claim_only_once() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();

        debt.record_forced(now, 2);
        debt.charge_admission(2, now, true);
        // Delivered after that admission: this build cannot have answered it.
        debt.record_forced(now, 5);
        // The build fails in a way that closes its epoch; the retained fact 5 is unused and
        // opens one new one. (A transient failure leaves the epoch live, so the credit stays
        // pending and the schedule — not the credit — decides the next attempt.)
        assert!(
            debt.record_failure(
                now,
                FailureKind::Operation,
                Sponsors { primary: true, marks: false }
            ),
            "the retained fact paid nothing"
        );

        // It is spent on the admission it opened.
        debt.charge_admission(5, now, true);
        assert!(
            !debt.record_failure(
                now,
                FailureKind::Operation,
                Sponsors { primary: true, marks: false }
            ),
            "the retained fact financed its own failure as well as its own claim",
        );
        assert!(
            matches!(debt.standing(now, ready()).forced, Some(Ripeness::Exhausted(_))),
            "the forced demand must carry the exhaustion, not come back as Now",
        );
    }

    /// A build slot with no budget of its own is bounded by the budget of the failure that
    /// holds it. A forced reload and a delivered change carry no schedule — the marks do, and
    /// that is why they may come past a spent retry while these two may not.
    ///
    /// Two separate rules meet here, and the trace needs both: the fact that STARTS a build is
    /// not fresh work the failure it caused has yet to answer, and a budget that failure spent
    /// is not re-opened by the standing fact that spent it.
    #[test]
    fn a_spent_budget_starts_no_more_builds() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        debt.record_forced(now, 1);
        // Admission: the fact that starts this build pays for it, here, once.
        debt.charge_admission(1, now, true);
        assert!(
            !debt.record_failure(
                now,
                FailureKind::Operation,
                Sponsors { primary: true, marks: false }
            ),
            "the fact that started the build is not a new epoch for the failure it caused",
        );

        let standing = debt.standing(now, ready());
        assert!(
            matches!(standing.failed, Some(Ripeness::Exhausted(_))),
            "an operation error stops the retry budget: {:?}",
            standing.failed,
        );
        assert_eq!(
            standing.forced,
            Some(Ripeness::Exhausted(Revival::FreshFact)),
            "a forced reload has no budget of its own, so it carries the failure's exhaustion \
             and names the external work that revives it",
        );
        assert!(
            debt.decide(now, ready()).start.is_none(),
            "a spent budget starts nothing: {:?}",
            debt.decide(now, ready()),
        );
    }

    /// The whole class, walked rather than asserted at one point: a workspace that cannot build
    /// must stop trying. Nothing external arrives, so every start after the budget is spent is
    /// a rebuild with no cause — the hot loop this schedule exists to make impossible.
    #[test]
    fn a_workspace_that_cannot_build_stops_trying() {
        for kind in [FailureKind::Operation, FailureKind::Transient] {
            let mut debt = GraphDebt::default();
            let mut now = Instant::now();
            debt.record_forced(now, 1);
            let mut starts = 0;
            let mut turns = 0;
            loop {
                turns += 1;
                assert!(turns <= 512, "{kind:?}: the walk itself never came to rest");
                let decision = debt.decide(now, ready());
                if let Some(start) = decision.start {
                    starts += 1;
                    assert!(
                        starts <= 8,
                        "{kind:?}: {starts} builds started and no external work ever arrived",
                    );
                    debt.charge_admission(1, now, start.forced);
                    debt.record_failure(now, kind, Sponsors { primary: true, marks: false });
                } else if let Some(at) = decision.wake_at {
                    now = at.max(now + Duration::from_millis(1));
                } else {
                    break;
                }
            }
            let standing = debt.standing(now, ready());
            assert!(
                matches!(standing.forced, Some(Ripeness::Exhausted(Revival::FreshFact))),
                "{kind:?}: the graph came to rest without naming what would revive it: {:?}",
                standing.forced,
            );
            // Rest is not silence: the named external work brings the debt back — and it is
            // work nobody has spent: fact 2 is above the frontier fact 1 left.
            debt.record_change(now, 2);
            assert!(
                debt.decide(now, ready()).start.is_some(),
                "{kind:?}: a fresh fact did not revive the spent budget",
            );
        }
    }

    /// A publication that cannot vouch for itself is watched, not retried into the ground: the
    /// probe backs off to a declared cap and the graph counts as settled while it stands.
    #[test]
    fn an_unsound_publication_settles_into_a_watch() {
        let mut debt = GraphDebt::default();
        let mut now = Instant::now();
        debt.record_change(now, 1);
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/Модуль.bsl"], None);
        for _ in 0..12 {
            let standing = debt.standing(now, ready());
            match standing.recovery {
                Some(Ripeness::Now) => {
                    probe(&mut debt, now, measured(an_unread("/ws/Модуль.bsl"), Level::Denied));
                }
                Some(Ripeness::Watching(at)) => now = at,
                other => panic!("a recovery debt that is neither ripe nor watched: {other:?}"),
            }
        }
        assert_eq!(
            debt.probe_interval(),
            Some(RECOVERY_PROBE_CAP),
            "the probe never reached its cap"
        );
    }

    /// A retry in its backoff, over marks whose own moment has already passed.
    ///
    /// The state a plan reviewer warned this redesign could spin in, once the old rule that
    /// muted the marks' alarm was gone. It does not spin, and two independent things stop it:
    /// a debt queued behind a live retry carries no moment at all, and a moment that has passed
    /// is `Now`, never `At` — so nothing stale can reach the alarm from either direction.
    #[test]
    fn marks_left_behind_a_live_retry_name_no_stale_alarm() {
        let mut debt = GraphDebt::default();
        let t0 = Instant::now();
        debt.place_marks(t0, 1, 1);
        debt.settle_marks(t0, false);
        // The marks come due and their build is charged for an attempt, which moves their next
        // moment a backoff ahead; then that moment passes.
        let t1 = t0 + Duration::from_secs(60);
        assert_eq!(debt.standing(t1, ready()).marks, Some(Ripeness::Now));
        debt.spend_mark_attempt(t1);
        // A build fails, retries at once, and fails again a minute later: the second refusal
        // earns a backoff reaching past the marks' moment.
        debt.record_failure(t1, FailureKind::Transient, Sponsors { primary: true, marks: false });
        let now = t1 + Duration::from_secs(60);
        debt.record_failure(now, FailureKind::Transient, Sponsors { primary: true, marks: false });

        let standing = debt.standing(now, ready());
        let Some(Ripeness::At(retry_at)) = standing.failed else {
            panic!("the retry should be in its backoff: {:?}", standing.failed)
        };
        assert!(retry_at > now, "a retry in its backoff names a moment ahead");
        assert_eq!(
            standing.marks,
            Some(Ripeness::Behind(DebtKind::Failed)),
            "the marks queue behind the live retry instead of naming a moment of their own",
        );
        let decision = debt.decide(now, ready());
        assert!(!decision.does_work(), "the slot is the retry's until its moment comes");
        assert_eq!(decision.wake_at, Some(retry_at), "so the alarm is the retry's, once");
        for (kind, ripeness) in standing.each() {
            if let Some(Ripeness::At(at) | Ripeness::Watching(at)) = ripeness {
                assert!(at > now, "{kind:?} named a moment that has already passed");
            }
        }
    }

    /// An answer the build actually delivered closes the credit that paid for looking.
    ///
    /// The credit and the obligation are two halves of one fact: a path measured positive owes
    /// a build, and the build that reads it has answered it. Keeping the credit after the
    /// answer is money for work already done — it forces one more reload nobody asked for, and
    /// it re-arms the account when that reload ends.
    #[test]
    fn a_covered_answer_closes_the_credit_that_paid_for_it() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/p.bsl", "/ws/q.bsl"], None);

        // The claim happens FIRST: this ticket captured nothing, so the healing below is
        // post-claim work by the ledger's own line.
        let cutoff = debt.capture_recovery();
        assert_eq!(cutoff, 0);
        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/p.bsl"), Level::Granted)),
            ProbeResult::NewEvidence,
        );
        assert!(debt.owes_recovery_build(), "a measured healing owes its build");

        // ...and the build that was already running read p after all. Its proof was prepared
        // above the healing's origin, so it speaks for it.
        let proof = covering(&debt, 2, &["/ws/p.bsl"], &["/ws/q.bsl"]);
        debt.record_publication(now, Some(1), true, Some(cutoff), proof);

        assert_eq!(outstanding_keys(&debt), vec!["/ws/q.bsl".to_owned()], "p was read");
        assert!(!debt.owes_recovery_build(), "the answered healing still owed a build of its own",);
        assert!(
            !debt.unused_recovery(),
            "the answered healing was still unspent authority for a later failure",
        );
        assert!(
            debt.decide(now, ready()).start.is_none(),
            "an answered healing financed another build",
        );
    }

    /// The same rule with the whole episode: a proof that closes it answers what it covered.
    #[test]
    fn a_coherent_answer_closes_the_credits_it_covered() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/p.bsl"], None);
        let cutoff = debt.capture_recovery();
        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/p.bsl"), Level::Granted)),
            ProbeResult::NewEvidence,
        );

        // What a coherent build actually installs: it read the address, it has nothing left
        // unread, and its walk covered the whole of the scope.
        let mut proof = covering(&debt, 2, &["/ws/p.bsl"], &[]);
        proof.scan_complete = Some(true);
        debt.record_publication(now, Some(1), true, Some(cutoff), proof);

        assert!(!debt.owes_recovery(), "the episode outlived the proof that answered it");
        assert!(!debt.owes_recovery_build(), "a closed episode left a credit behind it");
        assert!(debt.decide(now, ready()).start.is_none(), "and that credit bought a build");
    }

    /// A healing the build did NOT cover keeps its own demand — the control that says the rule
    /// above answers coverage, not the clock.
    #[test]
    fn an_uncovered_healing_outlives_the_publication_that_missed_it() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/p.bsl", "/ws/q.bsl"], None);
        let cutoff = debt.capture_recovery();
        assert_eq!(
            probe(
                &mut debt,
                now,
                vec![
                    (an_unread("/ws/p.bsl"), Level::Granted),
                    (an_unread("/ws/q.bsl"), Level::Granted),
                ],
            ),
            ProbeResult::NewEvidence,
        );

        // Only p was read. q healed just as much, and nothing has answered it.
        let proof = covering(&debt, 2, &["/ws/p.bsl"], &["/ws/q.bsl"]);
        debt.record_publication(now, Some(1), true, Some(cutoff), proof);
        assert!(debt.owes_recovery_build(), "the healing nobody covered lost its build");
        assert!(debt.unused_recovery(), "and lost its authority to revive a failure");
    }

    /// A publication that says "I could not read this" may not also say "this is answered".
    ///
    /// Both halves come from the same installed artefact, and the unread list is the one with
    /// authority: an address it names is required by definition. Retiring it anyway removes
    /// the cell and lets the very same list put it back as a NEW obligation — whose first
    /// measurement is first knowledge all over again, so an unchanged file buys a build on
    /// every pass.
    #[test]
    fn an_address_the_publication_declares_unread_is_never_retired() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/alias.bsl"], None);
        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/alias.bsl"), Level::Granted)),
            ProbeResult::NewEvidence,
        );
        let occurrence_before = debt.outstanding_recovery().keys;

        // A proof that claims the address is out of scope while declaring it unread.
        let captured_seq = debt.outstanding_recovery().captured_seq;
        debt.record_publication(
            now,
            Some(1),
            false,
            None,
            RecoveryPublicationProof {
                generation: 2,
                captured_seq,
                declared_unread: Some(vec!["/ws/alias.bsl".to_owned()]),
                out_of_scope_covered: vec![("/ws/alias.bsl".to_owned(), occurrence_before[0].1)],
                ..Default::default()
            },
        );

        assert_eq!(
            debt.outstanding_recovery().keys,
            occurrence_before,
            "the address the publication could not read was retired and re-registered",
        );
        // The measured positive survived with the occurrence, so looking again is not news.
        assert_eq!(
            probe(&mut debt, now, measured(an_unread("/ws/alias.bsl"), Level::Granted)),
            ProbeResult::NoNewEvidence,
            "an unchanged address bought a second build",
        );
    }

    /// A publication that could not read its own metadata answers nothing at all.
    ///
    /// The lenient reader turned a database that would not open into "nothing left unread",
    /// and that answer used to close the whole episode. The strict one says so by returning
    /// nothing — and a publication that knows nothing about what it read may not retire an
    /// obligation, end the chain, or be believed about either.
    #[test]
    fn a_publication_that_cannot_read_its_own_metadata_answers_nothing() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/p.bsl"], None);
        let occurrence = debt.outstanding_recovery().keys[0].1;

        debt.record_publication(
            now,
            Some(1),
            false,
            None,
            RecoveryPublicationProof {
                generation: 2,
                captured_seq: u64::MAX,
                // What the strict reader gives back when the metadata will not read.
                declared_unread: None,
                scan_complete: Some(true),
                read_covered: vec![("/ws/p.bsl".to_owned(), occurrence)],
                ..Default::default()
            },
        );

        assert_eq!(
            outstanding_keys(&debt),
            vec!["/ws/p.bsl".to_owned()],
            "an artefact that could not say what it read retired an obligation",
        );
        assert!(debt.owes_recovery(), "and it ended the chain that obligation belongs to");
    }

    /// A walk that straddled a write cannot validate the scan, however complete it was.
    ///
    /// Completeness is about the walk; vouching is about the publication. A build whose tree
    /// moved under it has a whole enumeration of a world that no longer stands, and letting it
    /// discharge the validation obligation leaves the episode with nothing outstanding — no
    /// probe, no wake, and a graph that waits for an event nobody will send.
    #[test]
    fn a_straddled_walk_does_not_validate_the_scan() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        let scope = a_scope(&["/ws"], true);

        // An unsound publication with an empty unread set and a complete pre-scan — the shape
        // a straddled build actually produces.
        debt.record_publication(
            now,
            Some(1),
            false,
            None,
            RecoveryPublicationProof {
                generation: 1,
                captured_seq: 0,
                declared_unread: Some(Vec::new()),
                scan_complete: Some(true),
                straddled: true,
                scope: Some(scope.clone()),
                ..Default::default()
            },
        );

        let standing = debt.standing(now + Duration::from_secs(1000), ready());
        assert!(
            standing.recovery.is_some(),
            "the straddled publication left no validation obligation at all",
        );
        let decision = debt.decide(now + Duration::from_secs(1000), ready());
        assert!(decision.probe, "nothing was owed a walk, so nothing would ever look again");

        // And one clean walk of the same scope answers it once.
        assert_eq!(
            probe_walking(
                &mut debt,
                now + Duration::from_secs(1000),
                vec![(Capability::ScanRoots, Level::Granted)],
                Some(scope),
            ),
            ProbeResult::NewEvidence,
        );
    }

    /// A publication that cannot vouch for itself does not end the chain, even when it has
    /// answered the last thing on it.
    ///
    /// Ending the chain is a statement about the whole tree — that there is nothing left a
    /// walk could still be owed. A build whose tree moved under it cannot make that statement
    /// about anything, however much it managed to read.
    #[test]
    fn a_straddled_publication_cannot_end_the_chain() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/p.bsl"], None);
        let occurrence = debt.outstanding_recovery().keys[0].1;

        // A point patch that re-projected the last unread module — on a publication that
        // straddled a write and is therefore served stale.
        debt.record_publication(
            now,
            Some(1),
            false,
            None,
            RecoveryPublicationProof {
                generation: 2,
                captured_seq: u64::MAX,
                declared_unread: Some(Vec::new()),
                straddled: true,
                read_covered: vec![("/ws/p.bsl".to_owned(), occurrence)],
                ..Default::default()
            },
        );

        assert!(outstanding_keys(&debt).is_empty(), "the address it re-projected is answered");
        assert!(
            debt.probe_interval().is_some(),
            "a publication that cannot vouch for itself ended the chain anyway",
        );
    }

    /// The first walk under a replaced declaration is news: the composition changed.
    ///
    /// Compared against the last walk alone, a first measurement has nothing to differ from,
    /// so a project whose roots were replaced measured a brand new tree and reported that
    /// nothing had happened — the one transition that must buy a build.
    #[test]
    fn the_first_walk_of_a_replaced_declaration_is_news() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        let a = a_scope(&["/ws/a"], true);
        let b = a_scope(&["/ws/b"], true);
        publish_declaring(&mut debt, now, Some(1), 1, &[], Some((a, false)));

        assert_eq!(
            probe_walking(
                &mut debt,
                now,
                vec![(Capability::ScanRoots, Level::Denied)],
                Some(b.clone()),
            ),
            ProbeResult::NewEvidence,
            "the walk of a declaration nobody has served yet was not news",
        );
        assert!(debt.owes_recovery_build(), "and it owed no build");

        // The same composition again is the same news, not another credit.
        let cutoff = debt.capture_recovery();
        assert_eq!(
            probe_walking(&mut debt, now, vec![(Capability::ScanRoots, Level::Denied)], Some(b)),
            ProbeResult::NoNewEvidence,
        );
        assert_eq!(debt.capture_recovery(), cutoff, "the same composition was paid for twice");
    }

    /// A declaration nobody has built yet is still measured, and its healing is news.
    ///
    /// The walk obligation belongs to the scope that STANDS; what a walk measured belongs to
    /// the composition it actually covered. Kept as one thing, a probe that found the roots
    /// replaced carried a verdict about a declaration nobody asks for any more, and every
    /// later look at the real one was thrown away for not matching it: the composition credit
    /// bought one build, that build failed without publishing, and the subtree becoming
    /// readable afterwards was measured as nothing at all.
    #[test]
    fn a_declaration_nobody_has_installed_yet_still_heals_on_its_own_account() {
        for same in [false, true] {
            let mut debt = GraphDebt::default();
            let now = Instant::now();
            let a = a_scope(&["/ws/a"], true);
            let b = a_scope(&["/ws/b"], true);
            let walked = if same { a.clone() } else { b };
            debt.record_change(now, 1);
            debt.charge_admission(1, now, false);
            publish_declaring(&mut debt, now, Some(1), 1, &[], Some((a, false)));

            // The first look: on a replaced declaration this is the change of composition,
            // and on the installed one it is a negative, which buys nothing.
            let first = probe_walking(
                &mut debt,
                now + Duration::from_secs(60),
                vec![(Capability::ScanRoots, Level::Denied)],
                Some(walked.clone()),
            );
            assert_eq!(
                first,
                if same { ProbeResult::NoNewEvidence } else { ProbeResult::NewEvidence },
                "same_scope={same}: the first look",
            );

            // Whatever that bought is spent, and the build it paid for ends without
            // publishing anything.
            let sponsors = debt.charge_admission(1, now + Duration::from_secs(60), true);
            debt.capture_recovery();
            debt.record_failure(now + Duration::from_secs(60), FailureKind::Operation, sponsors);

            // And then the tree becomes walkable.
            let healed = probe_walking(
                &mut debt,
                now + Duration::from_secs(1200),
                vec![(Capability::ScanRoots, Level::Granted)],
                Some(walked),
            );
            assert_eq!(
                healed,
                ProbeResult::NewEvidence,
                "same_scope={same}: a measured Denied -> Granted is a healing of its own",
            );
            assert!(
                debt.decide(now + Duration::from_secs(1200), ready()).start.is_some(),
                "same_scope={same}: and it owes the build that proves it",
            );
        }
    }

    /// The verdict about one set of roots is not re-attached to another.
    ///
    /// Re-basing the walk obligation onto the declaration that stands must start from what
    /// THIS walk measured of it. Carried across, the old roots' negative would make the first
    /// look at the new ones an "improvement" and buy a second build for one change — the
    /// composition credit, and then a healing that never happened.
    #[test]
    fn a_verdict_about_other_roots_is_not_carried_into_the_new_ones() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        let a = a_scope(&["/ws/a"], true);
        let b = a_scope(&["/ws/b"], true);
        publish_declaring(&mut debt, now, Some(1), 1, &[], Some((a.clone(), false)));

        // A measured negative about the declaration that was installed.
        assert_eq!(
            probe_walking(&mut debt, now, vec![(Capability::ScanRoots, Level::Denied)], Some(a)),
            ProbeResult::NoNewEvidence,
            "a negative buys nothing",
        );
        assert_eq!(debt.capture_recovery(), 0, "and issues no origin");

        // The declaration is replaced, and the first walk of the new roots succeeds.
        assert_eq!(
            probe_walking(
                &mut debt,
                now,
                vec![(Capability::ScanRoots, Level::Granted)],
                Some(b.clone()),
            ),
            ProbeResult::NewEvidence,
            "the composition changed, and that is news",
        );
        assert_eq!(
            debt.capture_recovery(),
            1,
            "one change of composition, one origin: the old roots' negative was carried across \
             and paid for as a healing of the new ones",
        );

        // And the new composition, unchanged, is not news again.
        assert_eq!(
            probe_walking(&mut debt, now, vec![(Capability::ScanRoots, Level::Granted)], Some(b)),
            ProbeResult::NoNewEvidence,
            "an unchanged declaration was paid for twice",
        );
    }

    /// A probe that outlived its episode still owns the walk until it comes back.
    ///
    /// The reservation belongs to the consumer doing the I/O, not to the episode that issued
    /// it. Tied to the episode, a coherent publication dropped the owner while its walk was
    /// still out, and the next unsound publication handed a second walker the same tree.
    #[test]
    fn a_probe_that_outlived_its_episode_still_owns_the_walk() {
        let mut debt = GraphDebt::default();
        let now = Instant::now();
        publish_declaring(&mut debt, now, Some(1), 1, &["/ws/p.bsl"], None);
        let plan = debt.reserve_probe().expect("the outstanding address is owed a walk");

        // The episode closes — proving it read what was outstanding — and a new one opens,
        // all while that walk is still out.
        let mut answered = covering(&debt, 2, &["/ws/p.bsl"], &[]);
        answered.scan_complete = Some(true);
        debt.record_publication(now, Some(1), false, None, answered);
        assert!(!debt.owes_recovery(), "the fixture needs the chain to end here");
        publish_declaring(&mut debt, now, Some(2), 3, &["/ws/q.bsl"], None);
        assert!(
            debt.reserve_probe().is_none(),
            "a second walker opened the tree beside the one still working",
        );

        // The old walk comes back: its basis is gone, so it says nothing — and gives the
        // reservation up.
        assert_eq!(
            debt.finish_probe(
                now,
                ProbeReceipt {
                    token: plan.token,
                    basis: plan.basis,
                    levels: measured(an_unread("/ws/q.bsl"), Level::Granted),
                    scope: None,
                },
            ),
            ProbeResult::Obsolete,
        );
        assert!(!debt.owes_recovery_build(), "the obsolete batch handed out a credit");
        assert!(debt.reserve_probe().is_some(), "the walk stayed owned by a consumer that left");
    }
}
