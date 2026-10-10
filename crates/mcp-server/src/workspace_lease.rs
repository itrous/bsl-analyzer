//! Which daemon owns a workspace's derived caches.
//!
//! One workspace-scoped cache leaf holds caches derived from the same sources — the call
//! graph and the code-search index — but the daemon that maintains them is not unique.
//! [`BackendKey`](crate::broker::BackendKey) forks a fresh backend on a binary upgrade, an
//! embedding-config change, or an extension-topology edit, and the superseded daemon lives on
//! until its idle TTL (indefinitely, while a client stays connected). Both processes then
//! rebuild and atomically rename the same graph database, and both re-render the same search
//! contexts: convergent, but wasteful, and each publish flickers the generation the other's
//! clients see.
//!
//! The lease makes that ownership explicit and single. A daemon claims it at startup under a
//! file lock when the directory is free, its owner stopped reporting, or its owner is an older
//! program: a newer version takes the workspace over, and the one it superseded stops writing
//! derived caches. A live owner of the same version, or of a version this program cannot
//! compare, keeps the workspace; the second daemon does not open the graph and says which
//! process holds the directory, until that owner stops reporting.
//!
//! A superseded daemon finishes the graph reads in flight, lets the graph file go, frees its
//! endpoint and exits, whether or not clients are still connected: they get `owner_changed`
//! and open a new session.
//!
//! What the lease deliberately does NOT gate is the search index's lexical side: chunks and
//! FTS text. Both generations derive those from the same files, SQLite serializes the writes
//! under WAL, and mark stamps come from the database itself (`bsl_search::Store`), so
//! duplicating them costs work rather than correctness — the one field whose meaning depends on
//! the graph, a chunk's rendered context, is covered by the topology check every graph reader
//! applies before adopting a file it did not write. Embeddings ARE gated: a vector is stored as
//! a bare blob against a chunk id, with no record of the model behind it, and the embedding
//! configuration is one of the axes that forks a generation in the first place.

use std::fs::{File, OpenOptions};
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[cfg(test)]
thread_local! {
    static CHECKPOINT_UNLOCK_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

/// Lease record file name, next to the caches it governs.
#[cfg(test)]
const LEASE_FILE: &str = "writer.lease";
/// The file locked for the read-modify-write of a claim. Separate from the record so the
/// record itself is only ever replaced by an atomic rename and readers need no lock.
use crate::cache::LEASE_LOCK_FILE;

/// How often the owner restamps its record so the others can tell it is still alive.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// A record older than this is treated as abandoned, and a daemon that has not observed a live
/// foreign owner may take the workspace. Comfortably above [`HEARTBEAT_INTERVAL`] so a loaded
/// machine cannot make a live owner look dead.
const STALE_AFTER: Duration = Duration::from_secs(60);
/// How often a held fence restamps the record through `checkpoint`.
///
/// The checkpoint exists so a LONG transaction does not let the record go stale; a
/// consumer that opens a fence per pass would otherwise restamp at the rate of its
/// own loop, and with the cache inside the watched tree each restamp is an event
/// that starts the next pass.
///
/// One second, not [`HEARTBEAT_INTERVAL`]: throttling raises the record's worst-case
/// age from "the gap between checkpoints" to "that gap plus the window", so the
/// window is what a gap that is safe today may grow by. [`is_stale`] compares
/// strictly against [`STALE_AFTER`], so at one second every gap up to 59 s stays
/// exactly as safe as it is now, while the restamp rate drops by three orders of
/// magnitude.
const CHECKPOINT_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// How long a cached ownership verdict is reused before the record is read again. Every gated
/// write path consults the lease, so this keeps the check off the syscall path without letting
/// a demotion go unnoticed for long.
pub(crate) const VERDICT_TTL: Duration = Duration::from_secs(2);
/// How long a claim waits for the lock file. The critical section is a read and one small
/// write, so anything beyond this means a peer wedged holding the lock — give up on this
/// attempt (the caller retries at its next check) rather than block a daemon's startup on it.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Generation of a lease that has not (yet) written a record: it owns nothing, and every
/// ownership check retries the claim.
const UNCLAIMED: u64 = 0;

/// The version a record names its owner by. Two live processes of one version do not take a
/// workspace from each other; a newer version takes it from an older one.
const PROGRAM_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
pub(crate) enum LeaseOperationError<E> {
    Lease(io::Error),
    Operation(E),
}

#[derive(Debug)]
pub(crate) enum LeaseOperationOutcome<T, E> {
    Applied(T),
    OperationError(LeaseOperationError<E>),
    TransientRefusal,
    Superseded,
    Released,
}

/// The on-disk record: who owns the workspace's derived caches, and since when.
#[derive(Serialize, Deserialize)]
struct LeaseRecord {
    /// Which claim this is. Ordering only — it decides who outbids whom, never who a record
    /// belongs to: generations restart from 1 whenever the record is deleted, so the same
    /// number can name two different daemons.
    generation: u64,
    /// WHO holds the workspace. Unique per claim (see [`new_token`]), which is what makes the
    /// identity check sound where the generation is not: after a `.build` wipe a daemon
    /// reclaiming as generation 1 must not be mistaken for the live generation-1 daemon it
    /// superseded, or both would own the workspace for good.
    token: u64,
    /// The owner's process id. Diagnostics only — liveness is decided by the heartbeat, which
    /// needs no cross-platform process introspection and is immune to pid reuse.
    pid: u32,
    /// Unix seconds of the owner's last heartbeat.
    heartbeat_secs: u64,
    /// The owner's program version. Absent in a record of a program that did not write it,
    /// which proves nothing about who is newer.
    #[serde(default)]
    version: Option<String>,
    /// The workspace this cache serves, canonicalized by the daemon that claimed it.
    ///
    /// A claim of a cache whose record names a DIFFERENT workspace is refused, stale or not
    /// (github#272): one directory must not serve two configurations. Absent in a record of
    /// an older program — such a record proves nothing about its owner and is adopted, and
    /// the next write names this daemon's workspace.
    #[serde(default)]
    workspace: Option<String>,
}

/// What an ownership check of this lease found, as the graph needs to tell it apart: a lost
/// workspace is left for good, while a check that could not answer only holds new work back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnershipCheck {
    Owned,
    /// The check could not answer: the record would not read or the claim could not be made.
    Unknown,
    /// Superseded by another owner, or released by this process. Never followed by `Owned`.
    Lost,
}

type OwnershipObserver = Arc<dyn Fn(OwnershipCheck) + Send + Sync>;

/// The live owner a claim found and would not take the workspace from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BusyOwner {
    pub(crate) pid: u32,
    pub(crate) version: Option<String>,
    /// The workspace the record names, when it names one: a refusal for a foreign workspace
    /// says so instead of posing as a live owner of the same one.
    pub(crate) workspace: Option<String>,
}

/// Whether a claim may take the workspace from the record found under the lock: a free
/// workspace (no record, or an owner that stopped reporting), a successor within this same
/// process, or an owner of an older program version. A live owner of the same version — or of
/// a version this program cannot compare — keeps the workspace.
fn claimable(found: Option<&LeaseRecord>) -> bool {
    let Some(record) = found else { return true };
    if is_stale(record) || record.pid == std::process::id() {
        return true;
    }
    match (record.version.as_deref().and_then(parse_version), parse_version(PROGRAM_VERSION)) {
        (Some(theirs), Some(ours)) => theirs < ours,
        _ => false,
    }
}

fn parse_version(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|part| part.parse().ok()).collect()
}

/// An identity for one claim: a 64-bit digest of this process's id, the instant of the claim,
/// and a per-process counter — so two claims differ even within one daemon in one nanosecond.
/// Distinctness is probabilistic in the digest, which at a handful of claims per workspace is a
/// collision chance no one will meet; what it must not be is DERIVABLE, as the generation is,
/// since that is what let two daemons read one record as both of theirs.
pub(crate) fn new_token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&NEXT.fetch_add(1, Ordering::SeqCst).to_le_bytes());
    u64::from_le_bytes(hasher.finalize().as_bytes()[..8].try_into().expect("blake3 yields 32"))
}

/// The durable identity of the workspace a cache serves: the canonical spelling of its root.
///
/// Canonicalization folds `..`, a trailing `.`, symlinks and the Windows `\\?\` prefix. It
/// does NOT fold bind-mounts: one tree reached through two mount points stays two identities,
/// and the claim check errs by refusing a cache it cannot prove is ours.
///
/// A root that is not valid UTF-8 is spelled by its escaped debug form rather than lossily: a
/// lossy spelling would fold two distinct roots into one identity. The escaped form opens with a
/// quote, which no absolute path does, so it never meets a plain spelling either.
fn workspace_identity(workspace: &Path) -> String {
    let canonical = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    match canonical.to_str() {
        Some(spelling) => spelling.to_owned(),
        None => format!("{canonical:?}"),
    }
}

/// A daemon's claim on one workspace's derived caches. Cheap to clone (every holder shares one
/// verdict cache), and safe to consult from any thread.
#[derive(Clone)]
pub(crate) struct WorkspaceLease {
    inner: Arc<Inner>,
}

struct Inner {
    /// `None` for a lease that governs nothing (no workspace, or the claim could not be
    /// written). Such a lease always reports ownership: coordination is best-effort, and
    /// failing closed would silently stop a lone daemon from maintaining its own caches.
    path: Option<PathBuf>,
    generation: AtomicU64,
    /// The token this daemon last wrote into the record; `0` while unclaimed.
    token: AtomicU64,
    /// This daemon's workspace identity, canonicalized once at claim time. `None` when the
    /// caller named no workspace: the claim check then has nothing to compare and is decided
    /// by the old rules.
    workspace: Option<String>,
    /// Immutable cache scope admitted by the launcher. Background writers use
    /// it for fresh Project checks before paid work and publication.
    cache: Option<crate::cache::WorkspaceCacheLayout>,
    owns: AtomicBool,
    /// Set permanently after this lease, having owned the workspace, observes a live foreign
    /// token. All clones share the verdict and never attempt to reclaim after it is set.
    superseded: AtomicBool,
    /// Set by [`WorkspaceLease::release`] and never cleared: this process is going away, so it
    /// must not take the workspace back — a background pass still finishing during shutdown
    /// would otherwise see the record it just removed as "nobody owns this" and re-claim it.
    released: AtomicBool,
    /// When ownership was last ATTEMPTED, and the lock the attempt itself runs under. It paces
    /// the next attempt and nothing else: an attempt that could not take the lock file, or
    /// whose write failed, still happened — which is what the pacing is about — and it answered
    /// nothing.
    checked_at: Mutex<Option<Instant>>,
    /// Whether a verdict has been ESTABLISHED: a check that completed and produced an answer.
    ///
    /// Apart from `checked_at` in two ways that both matter. It is a fact about the answer
    /// rather than about the attempt, so a refresh that answered nothing leaves it where it was
    /// instead of publishing the initial value as a checked one. And it is an atomic, so
    /// reading it takes no lock: the attempts hold `checked_at` across a file lock and a record
    /// read — seconds of it, by design — and a reader that had to take that lock would queue
    /// behind I/O it is forbidden to do itself.
    established: AtomicBool,
    /// The live owner the last refused claim found; `None` once a claim succeeds.
    busy: Mutex<Option<BusyOwner>>,
    /// Told whenever an ownership check finds something other than what the last one found.
    /// Called with lease locks held: an observer records and wakes, it never waits.
    observers: Mutex<Vec<OwnershipObserver>>,
    /// What the last reported check found; `None` before the first.
    last_check: Mutex<Option<OwnershipCheck>>,
    /// Set on the lease handed out when a managed claim could not be made at all. Such a lease
    /// lets the search index work as before, but the graph is not opened over it: two
    /// processes that could not coordinate must not both hold the graph file.
    coordination_failed: bool,
    /// Every lease read or claim that went to disk, by the thread that made it.
    ///
    /// Threads rather than a count, and that is the whole point: "the answer came back quickly"
    /// is not the statement a request path has to make — a read that beats a bound on an idle
    /// machine is the same read under load — and a count cannot tell a request that read the
    /// lease from a background owner that did, while background reads are not merely allowed
    /// but are how the verdict this daemon serves stays current.
    #[cfg(test)]
    disk_check_threads: Mutex<Vec<String>>,
    /// When [`WorkspaceLease::publish_checkpointed`] last restamped the record.
    ///
    /// On the shared inner, not on one call: a hot consumer opens a NEW fence on every
    /// pass, so a window scoped to a single fence would reset on each of them and never
    /// hold anything back.
    stamped_at: Mutex<Option<Instant>>,
    #[cfg(test)]
    fail_managed_lock: AtomicBool,
    #[cfg(test)]
    fail_managed_read: AtomicBool,
    #[cfg(test)]
    fail_managed_restamp: AtomicBool,
    #[cfg(test)]
    fail_checkpoint_lock: AtomicBool,
    /// Refuse the n-th checkpoint rather than the next one. A production path takes several
    /// checkpoints before the one under test, and a one-shot flag burns on the first.
    #[cfg(test)]
    fail_checkpoint_lock_countdown: std::sync::atomic::AtomicU64,
}

impl WorkspaceLease {
    /// Claim `workspace_root`'s derived caches for this process, taking the generation above
    /// whatever the last owner recorded. A fixture whose lease directory cannot be prepared
    /// yields a coordination-failed lease that refuses every derived-cache write.
    #[cfg(test)]
    pub(crate) fn claim(workspace_root: &Path) -> Self {
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(workspace_root);
        Self::claim_cache(&cache)
    }

    #[cfg(test)]
    pub(crate) fn while_cache_lock_held<T>(
        cache: &crate::cache::WorkspaceCacheLayout,
        run: impl FnOnce() -> T,
    ) -> T {
        cache.ensure().unwrap();
        let _guard = LockGuard::acquire(&cache.lease_lock_path(), LOCK_WAIT).unwrap();
        run()
    }

    /// Hold the cache's lease lock until the returned sender lets go — or until the stand that
    /// asked for it goes away with it still held. For a stand whose question is "did the answer
    /// come back while the lock was held", which is a fact about order, not about a duration.
    #[cfg(test)]
    pub(crate) fn hold_cache_lock_until_released(
        cache: &crate::cache::WorkspaceCacheLayout,
    ) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Sender<()>) {
        cache.ensure().unwrap();
        let path = cache.lease_lock_path();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _guard = LockGuard::acquire(&path, LOCK_WAIT).unwrap();
            ready_tx.send(()).unwrap();
            // Either the stand releases it, or the stand is gone and the sender with it.
            let _ = release_rx.recv();
        });
        ready_rx.recv().unwrap();
        (handle, release_tx)
    }

    #[cfg(test)]
    pub(crate) fn hold_cache_lock_for(
        cache: &crate::cache::WorkspaceCacheLayout,
        duration: Duration,
    ) -> std::thread::JoinHandle<()> {
        cache.ensure().unwrap();
        let path = cache.lease_lock_path();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _guard = LockGuard::acquire(&path, LOCK_WAIT).unwrap();
            ready_tx.send(()).unwrap();
            std::thread::sleep(duration);
        });
        ready_rx.recv().unwrap();
        handle
    }

    /// Claim the derived caches rooted at `cache` for this process.
    pub(crate) fn claim_cache(cache: &crate::cache::WorkspaceCacheLayout) -> Self {
        match Self::try_claim_cache(cache) {
            Ok(lease) => lease,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    root = %cache.root().display(),
                    "could not claim the workspace cache lease; this daemon will not coordinate \
                     with another generation over the same caches, and will not open the graph"
                );
                let mut lease = Self::unmanaged();
                Arc::get_mut(&mut lease.inner)
                    .expect("a lease just built has one holder")
                    .coordination_failed = true;
                lease
            }
        }
    }

    /// A lease over nothing: reference profiles, tests, and every path with no workspace
    /// directory to coordinate through.
    pub(crate) fn unmanaged() -> Self {
        Self {
            inner: Arc::new(Inner {
                path: None,
                cache: None,
                generation: AtomicU64::new(0),
                token: AtomicU64::new(0),
                workspace: None,
                owns: AtomicBool::new(true),
                superseded: AtomicBool::new(false),
                released: AtomicBool::new(false),
                checked_at: Mutex::new(None),
                established: AtomicBool::new(false),
                stamped_at: Mutex::new(None),
                busy: Mutex::new(None),
                observers: Mutex::new(Vec::new()),
                last_check: Mutex::new(None),
                coordination_failed: false,
                #[cfg(test)]
                disk_check_threads: Mutex::new(Vec::new()),
                #[cfg(test)]
                fail_managed_lock: AtomicBool::new(false),
                #[cfg(test)]
                fail_managed_read: AtomicBool::new(false),
                #[cfg(test)]
                fail_managed_restamp: AtomicBool::new(false),
                #[cfg(test)]
                fail_checkpoint_lock: AtomicBool::new(false),
                #[cfg(test)]
                fail_checkpoint_lock_countdown: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    fn try_claim_cache(cache: &crate::cache::WorkspaceCacheLayout) -> io::Result<Self> {
        // The directory is the one thing a lease cannot do without. Everything past it — the
        // lock, the record write — is retried later by `owns_caches`, so a moment's contention
        // does not cost this daemon its place in the coordination for good.
        cache.ensure()?;
        let path = cache.lease_path();
        let inner = Arc::new(Inner {
            path: Some(path),
            cache: Some(cache.clone()),
            generation: AtomicU64::new(UNCLAIMED),
            token: AtomicU64::new(0),
            workspace: cache.workspace().map(workspace_identity),
            owns: AtomicBool::new(false),
            superseded: AtomicBool::new(false),
            released: AtomicBool::new(false),
            checked_at: Mutex::new(None),
            established: AtomicBool::new(false),
            stamped_at: Mutex::new(None),
            busy: Mutex::new(None),
            observers: Mutex::new(Vec::new()),
            last_check: Mutex::new(None),
            coordination_failed: false,
            #[cfg(test)]
            disk_check_threads: Mutex::new(Vec::new()),
            #[cfg(test)]
            fail_managed_lock: AtomicBool::new(false),
            #[cfg(test)]
            fail_managed_read: AtomicBool::new(false),
            #[cfg(test)]
            fail_managed_restamp: AtomicBool::new(false),
            #[cfg(test)]
            fail_checkpoint_lock: AtomicBool::new(false),
            #[cfg(test)]
            fail_checkpoint_lock_countdown: std::sync::atomic::AtomicU64::new(0),
        });
        let lease = Self { inner };
        if !lease.take_generation(claimable) {
            match lease.foreign_workspace() {
                Some((mine, theirs)) => tracing::warn!(
                    cache_dir = %cache.root().display(),
                    ours = mine,
                    theirs,
                    "this cache directory serves another workspace; refusing to claim it — give \
                     this process a separate --cache-dir, or remove the directory if it is no \
                     longer needed"
                ),
                None => match lock_recover(&lease.inner.busy).clone() {
                    Some(owner) => tracing::warn!(
                        cache_dir = %cache.root().display(),
                        owner_pid = owner.pid,
                        owner_version = owner.version.as_deref().unwrap_or("unknown"),
                        "the graph is busy: another live process owns this cache directory and \
                         this version does not take it over (same, newer or unknown version); \
                         give this process a separate --cache-dir, or stop sharing the directory"
                    ),
                    None => tracing::warn!(
                        root = %cache.root().display(),
                        "workspace cache lease is locked by a peer; retrying on the next check"
                    ),
                },
            }
        }
        spawn_heartbeat(Arc::downgrade(&lease.inner));
        Ok(lease)
    }

    /// Take the generation above whatever the record holds, under the lock — but only when
    /// `claimable` accepts the record found THERE, not the one that was read outside it. A
    /// startup claim accepts anything; a reclaim accepts only a workspace that is still free
    /// (no record, or one whose owner stopped reporting), so a daemon that claimed while we
    /// waited for the lock is not outbid on the strength of an observation that has expired.
    ///
    /// `false` when the lock, the predicate, or the write did not go through — the caller stays
    /// as it was (an unclaimed lease owns nothing) and tries again at its next check.
    fn take_generation(&self, claimable: impl Fn(Option<&LeaseRecord>) -> bool) -> bool {
        let mut checked_at = lock_recover(&self.inner.checked_at);
        self.take_generation_locked(&mut checked_at, claimable)
    }

    /// [`Self::take_generation`] with the process-local lifecycle lock already held.
    fn take_generation_locked(
        &self,
        checked_at: &mut Option<Instant>,
        claimable: impl Fn(Option<&LeaseRecord>) -> bool,
    ) -> bool {
        #[cfg(test)]
        self.note_disk_check();
        if self.inner.released.load(Ordering::SeqCst)
            || self.inner.superseded.load(Ordering::SeqCst)
        {
            return false;
        }
        let Some(path) = self.inner.path.as_deref() else {
            return false;
        };
        let Some(dir) = path.parent() else { return false };
        // Recreated, not just used: `.build` is a cache directory users are told they may
        // delete, and a daemon that could not put the lock file back would report non-ownership
        // for the rest of its life — every live daemon stuck read-only over a workspace nobody
        // owns, with no way to recreate what they are all waiting for.
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
        let Ok(_guard) = LockGuard::acquire(&dir.join(LEASE_LOCK_FILE), LOCK_WAIT) else {
            return false;
        };
        // Re-read UNDER the lock, where `release` also runs: a claim that started before this
        // process decided to leave must not complete afterwards. It would put a record on disk
        // that this daemon will never heartbeat (the release stops that) and never remove (the
        // release already ran) — a live-looking claim on a workspace nobody is maintaining,
        // blocking every other daemon until it goes stale.
        if self.inner.released.load(Ordering::SeqCst) {
            return false;
        }
        let found = match read_record_result(path) {
            Ok(found) => found,
            // A record that will not read says nothing about whether its owner lives. Only one
            // nobody has rewritten for longer than a live owner's heartbeat allows is taken.
            Err(_) if written_before_stale(path) => None,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "the workspace lease record will not read; not taking the workspace from \
                     an owner that may be alive"
                );
                *lock_recover(&self.inner.busy) = None;
                return false;
            }
        };
        // A cache directory belongs to ONE workspace, and the check sits under the same lock
        // that serializes claims: of two workspaces racing for a fresh directory the loser
        // reads the winner's name and refuses, with no window where both claim it. Staleness
        // is not consulted — a dead owner's cache is still that workspace's derived state,
        // and adopting it would serve this one from another one's graph (github#272).
        if let (Some(mine), Some(theirs)) =
            (self.inner.workspace.as_deref(), found.as_ref().and_then(|r| r.workspace.as_deref()))
        {
            if mine != theirs {
                *lock_recover(&self.inner.busy) = found.as_ref().map(|record| BusyOwner {
                    pid: record.pid,
                    version: record.version.clone(),
                    workspace: record.workspace.clone(),
                });
                // The answer IS given — this process does not own the cache — so publish it
                // instead of leaving the status surface to answer "unknown" forever.
                self.establish(false);
                return false;
            }
        }
        if !claimable(found.as_ref()) {
            *lock_recover(&self.inner.busy) =
                found.filter(|record| !is_stale(record)).map(|record| BusyOwner {
                    pid: record.pid,
                    version: record.version,
                    workspace: record.workspace,
                });
            return false;
        }
        *lock_recover(&self.inner.busy) = None;
        let generation = found.map(|r| r.generation).unwrap_or(0) + 1;
        let token = new_token();
        if write_record(path, generation, token, self.inner.workspace.as_deref()).is_err() {
            return false;
        }
        self.inner.generation.store(generation, Ordering::SeqCst);
        self.inner.token.store(token, Ordering::SeqCst);
        self.establish(true);
        *checked_at = Some(Instant::now());
        tracing::info!(
            generation,
            path = %path.display(),
            "claimed the workspace derived-cache lease"
        );
        true
    }

    /// Whether this daemon may write the workspace's derived caches. The verdict is cached for
    /// [`VERDICT_TTL`], so gating a write path on it costs an atomic load in the common case.
    pub(crate) fn owns_caches(&self) -> bool {
        if self.inner.coordination_failed
            || self.inner.released.load(Ordering::SeqCst)
            || self.inner.superseded.load(Ordering::SeqCst)
        {
            return false;
        }
        let Some(path) = self.inner.path.as_deref() else {
            return true;
        };
        let mut checked_at = lock_recover(&self.inner.checked_at);
        if self.inner.released.load(Ordering::SeqCst)
            || self.inner.superseded.load(Ordering::SeqCst)
        {
            return false;
        }
        match *checked_at {
            Some(at) if at.elapsed() < VERDICT_TTL => {
                return self.inner.owns.load(Ordering::SeqCst)
            }
            _ => *checked_at = Some(Instant::now()),
        }
        // A claim that could not be written at startup is retried here rather than leaving this
        // daemon permanently outside the coordination — which, since an unclaimed lease never
        // owns anything, would otherwise mean it never maintains the caches at all.
        if self.inner.generation.load(Ordering::SeqCst) == UNCLAIMED {
            return self.take_generation_locked(&mut checked_at, claimable);
        }
        self.settle(self.recheck_locked(path, &mut checked_at))
    }

    /// Whether the lifecycle lock is held right now. Test-only: it is how a stand puts a
    /// request against a background check that is inside that lock, and nothing in production
    /// decides anything on a `try_lock`.
    #[cfg(test)]
    pub(crate) fn lifecycle_is_busy(&self) -> bool {
        matches!(self.inner.checked_at.try_lock(), Err(std::sync::TryLockError::WouldBlock))
    }

    /// The threads this lease's disk answers were asked on, in order. Noted on exactly two
    /// paths: the ownership checks and the publication fence. `release` takes the lock file
    /// too and is not noted — it runs as this process leaves, not while a request is served.
    #[cfg(test)]
    pub(crate) fn disk_check_threads(&self) -> Vec<String> {
        lock_recover(&self.inner.disk_check_threads).clone()
    }

    #[cfg(test)]
    fn note_disk_check(&self) {
        lock_recover(&self.inner.disk_check_threads)
            .push(std::thread::current().name().unwrap_or("<unnamed>").to_owned());
    }

    /// Whether any check has established this lease's ownership verdict yet.
    ///
    /// An unmanaged lease owns everything by construction and needs no check; a managed one
    /// whose startup claim did not go through has a cached value that means nothing until a
    /// check answers, and a caller publishing that value would publish a guess.
    ///
    /// Lock-free, and that is not an optimization: the checks that produce the verdict hold the
    /// lifecycle lock across their file lock and their record read, so asking this under that
    /// lock would make the asker wait out somebody else's I/O.
    pub(crate) fn ownership_was_checked(&self) -> bool {
        self.inner.path.is_none() || self.inner.established.load(Ordering::SeqCst)
    }

    /// Record a verdict a check actually produced: what it says, and that there now IS one.
    ///
    /// Every producer goes through here — the claim that wrote its record, the recheck that read
    /// one, the fence that found a foreign token, the release — so "answered" and "attempted"
    /// cannot drift apart again.
    fn establish(&self, owns: bool) {
        self.inner.owns.store(owns, Ordering::SeqCst);
        self.inner.established.store(true, Ordering::SeqCst);
        self.report(if self.terminal_outcome::<(), ()>().is_some() {
            OwnershipCheck::Lost
        } else if owns {
            OwnershipCheck::Owned
        } else {
            OwnershipCheck::Unknown
        });
    }

    /// Watch this lease's ownership checks. An observer added after the workspace was lost
    /// hears so at once.
    ///
    /// A new observer is told the last check's finding at once, under the same lock the next
    /// report takes: nothing reported before it joined is lost, and nothing reported after it
    /// joined reaches it out of order.
    pub(crate) fn observe(&self, observer: OwnershipObserver) {
        let last = lock_recover(&self.inner.last_check);
        let found = if self.terminal_outcome::<(), ()>().is_some() {
            Some(OwnershipCheck::Lost)
        } else {
            *last
        };
        lock_recover(&self.inner.observers).push(Arc::clone(&observer));
        if let Some(found) = found {
            observer(found);
        }
        drop(last);
    }

    /// Tell the observers what a check found, when it differs from the last finding. Delivered
    /// with the finding's lock held, so two checks finishing together cannot reach an observer
    /// in the opposite order from the one they were recorded in.
    fn report(&self, check: OwnershipCheck) {
        let mut last = lock_recover(&self.inner.last_check);
        if *last == Some(check) || *last == Some(OwnershipCheck::Lost) {
            return;
        }
        *last = Some(check);
        for observer in lock_recover(&self.inner.observers).iter() {
            observer(check);
        }
    }

    /// Last process-local ownership verdict, without lock-file or lease-record I/O.
    pub(crate) fn owns_caches_cached(&self) -> bool {
        !self.inner.coordination_failed
            && !self.inner.released.load(Ordering::SeqCst)
            && !self.inner.superseded.load(Ordering::SeqCst)
            && (self.inner.path.is_none() || self.inner.owns.load(Ordering::SeqCst))
    }

    /// Ownership as of NOW, bypassing the cached verdict.
    ///
    /// For a caller whose next act writes something a takeover would poison, where up to
    /// [`VERDICT_TTL`] of stale "yes" is too generous — the graph, which decides here whether
    /// to start a build or claim a reload. It costs one small read, so it belongs on paths that
    /// run per pass, not per query. This narrows the window; it does not fence it (only
    /// [`Self::with_ownership`] does), which is the right trade where the write is a vector
    /// that a re-embed can replace rather than a rename that destroys another daemon's build.
    pub(crate) fn owns_caches_now(&self) -> bool {
        if self.inner.coordination_failed
            || self.inner.released.load(Ordering::SeqCst)
            || self.inner.superseded.load(Ordering::SeqCst)
        {
            return false;
        }
        let Some(path) = self.inner.path.as_deref() else {
            return true;
        };
        let mut checked_at = lock_recover(&self.inner.checked_at);
        if self.inner.released.load(Ordering::SeqCst)
            || self.inner.superseded.load(Ordering::SeqCst)
        {
            return false;
        }
        *checked_at = Some(Instant::now());
        if self.inner.generation.load(Ordering::SeqCst) == UNCLAIMED {
            return self.take_generation_locked(&mut checked_at, claimable);
        }
        self.settle(self.recheck_locked(path, &mut checked_at))
    }

    /// What a check answered, published — or, when it answered nothing, the conservative "not
    /// now" its caller needs, with no verdict published on the strength of an attempt.
    fn settle(&self, answer: Option<bool>) -> bool {
        match answer {
            Some(owns) => {
                self.establish(owns);
                owns
            }
            None => {
                self.report(OwnershipCheck::Unknown);
                false
            }
        }
    }

    /// Publish one already-prepared value through a single visibility point.
    pub(crate) fn publish_short<P, T, E>(
        &self,
        prepared: &mut P,
        commit: impl FnOnce(&mut P) -> Result<T, E>,
    ) -> LeaseOperationOutcome<T, E> {
        self.publish_checkpointed_inner(true, |_| ControlFlow::Continue(commit(prepared)))
    }

    /// Run one indivisible transaction with cooperative liveness and terminal checkpoints.
    pub(crate) fn publish_checkpointed<T, E>(
        &self,
        write: impl FnOnce(&mut dyn FnMut() -> ControlFlow<()>) -> ControlFlow<(), Result<T, E>>,
    ) -> LeaseOperationOutcome<T, E> {
        self.publish_checkpointed_inner(false, write)
    }

    fn publish_checkpointed_inner<T, E>(
        &self,
        force_initial_restamp: bool,
        write: impl FnOnce(&mut dyn FnMut() -> ControlFlow<()>) -> ControlFlow<(), Result<T, E>>,
    ) -> LeaseOperationOutcome<T, E> {
        if let Some(outcome) = self.terminal_outcome() {
            return outcome;
        }
        let Some(path) = self.inner.path.as_deref() else {
            let mut stopped = None;
            let mut checkpoint = || {
                if let Some(outcome) = self.terminal_outcome() {
                    stopped = Some(outcome);
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            return map_checkpointed_result(write(&mut checkpoint), stopped);
        };
        let mut checked_at = lock_recover(&self.inner.checked_at);
        if let Some(outcome) = self.terminal_outcome() {
            return outcome;
        }
        let Some(dir) = path.parent() else {
            return LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(
                io::Error::new(io::ErrorKind::InvalidInput, "lease path has no parent"),
            ));
        };
        #[cfg(test)]
        if self.inner.fail_managed_lock.swap(false, Ordering::SeqCst) {
            return LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(
                io::Error::other("injected managed lock failure"),
            ));
        }
        let lock_path = dir.join(LEASE_LOCK_FILE);
        // A fence is this lease's disk too: it takes the lock file and restamps the record.
        // Noting it here is what lets a stand say WHICH thread asked — the ownership checks
        // alone leave a request that fences invisible, and a bounded wait on a held lock is
        // not a hang that a timeout would catch.
        #[cfg(test)]
        self.note_disk_check();
        let guard = match LockGuard::acquire(&lock_path, LOCK_WAIT) {
            Ok(guard) => guard,
            Err(error) if is_lock_contention(&error) => {
                return LeaseOperationOutcome::TransientRefusal
            }
            Err(error) => {
                return LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error))
            }
        };
        if let Some(outcome) = self.terminal_outcome() {
            return outcome;
        }
        if self.inner.generation.load(Ordering::SeqCst) == UNCLAIMED {
            return LeaseOperationOutcome::TransientRefusal;
        }
        #[cfg(test)]
        if self.inner.fail_managed_read.swap(false, Ordering::SeqCst) {
            return LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(
                io::Error::other("injected managed read failure"),
            ));
        }
        let record = match read_record_result(path) {
            Ok(Some(record)) => record,
            Ok(None) => return LeaseOperationOutcome::TransientRefusal,
            Err(error) => {
                return LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error))
            }
        };
        let mine = self.inner.token.load(Ordering::SeqCst);
        if record.token != mine {
            if !is_stale(&record) {
                self.latch_superseded(&record);
                self.establish(false);
                *checked_at = Some(Instant::now());
                return LeaseOperationOutcome::Superseded;
            }
            self.establish(false);
            *checked_at = Some(Instant::now());
            return LeaseOperationOutcome::TransientRefusal;
        }
        if let Err(error) = self.restamp(path, force_initial_restamp) {
            return LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error));
        }

        let mut guard = Some(guard);
        let mut stopped = None;
        let mut checkpoint = || {
            if let Some(outcome) = self.terminal_outcome() {
                stopped = Some(outcome);
                return ControlFlow::Break(());
            }
            drop(guard.take());
            #[cfg(test)]
            CHECKPOINT_UNLOCK_HOOK.with(|slot| {
                if let Some(hook) = slot.borrow_mut().take() {
                    hook();
                }
            });
            #[cfg(test)]
            #[allow(deprecated, reason = "test fault injection retains Rust 1.91 compatibility")]
            if self.inner.fail_checkpoint_lock.swap(false, Ordering::SeqCst)
                || self.inner.fail_checkpoint_lock_countdown.fetch_update(
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                    |left| (left > 0).then(|| left - 1),
                ) == Ok(1)
            {
                stopped = Some(LeaseOperationOutcome::TransientRefusal);
                return ControlFlow::Break(());
            }
            guard = match LockGuard::acquire(&lock_path, LOCK_WAIT) {
                Ok(guard) => Some(guard),
                Err(error) if is_lock_contention(&error) => {
                    stopped = Some(LeaseOperationOutcome::TransientRefusal);
                    return ControlFlow::Break(());
                }
                Err(error) => {
                    stopped = Some(LeaseOperationOutcome::OperationError(
                        LeaseOperationError::Lease(error),
                    ));
                    return ControlFlow::Break(());
                }
            };
            if let Some(outcome) = self.terminal_outcome() {
                stopped = Some(outcome);
                return ControlFlow::Break(());
            }
            let record = match read_record_result(path) {
                Ok(Some(record)) => record,
                Ok(None) => {
                    stopped = Some(LeaseOperationOutcome::TransientRefusal);
                    return ControlFlow::Break(());
                }
                Err(error) => {
                    stopped = Some(LeaseOperationOutcome::OperationError(
                        LeaseOperationError::Lease(error),
                    ));
                    return ControlFlow::Break(());
                }
            };
            if record.token != mine {
                if !is_stale(&record) {
                    self.latch_superseded(&record);
                    self.establish(false);
                    *checked_at = Some(Instant::now());
                    stopped = Some(LeaseOperationOutcome::Superseded);
                } else {
                    self.establish(false);
                    *checked_at = Some(Instant::now());
                    stopped = Some(LeaseOperationOutcome::TransientRefusal);
                }
                return ControlFlow::Break(());
            }
            if let Err(error) = self.restamp(path, false) {
                stopped =
                    Some(LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error)));
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        };
        map_checkpointed_result(write(&mut checkpoint), stopped)
    }

    fn terminal_outcome<T, E>(&self) -> Option<LeaseOperationOutcome<T, E>> {
        if self.inner.coordination_failed {
            Some(LeaseOperationOutcome::Released)
        } else if self.inner.superseded.load(Ordering::SeqCst) {
            Some(LeaseOperationOutcome::Superseded)
        } else if self.inner.released.load(Ordering::SeqCst) {
            Some(LeaseOperationOutcome::Released)
        } else {
            None
        }
    }

    fn restamp(&self, path: &Path, force: bool) -> io::Result<()> {
        let mut stamped = lock_recover(&self.inner.stamped_at);
        let now = Instant::now();
        if !force && stamped.is_some_and(|last| now.duration_since(last) < CHECKPOINT_MIN_INTERVAL)
        {
            return Ok(());
        }
        #[cfg(test)]
        if self.inner.fail_managed_restamp.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected managed restamp failure"));
        }
        write_record(
            path,
            self.inner.generation.load(Ordering::SeqCst),
            self.inner.token.load(Ordering::SeqCst),
            self.inner.workspace.as_deref(),
        )?;
        *stamped = Some(now);
        Ok(())
    }

    /// The live owner that kept this lease from claiming the workspace, while it does.
    pub(crate) fn busy_owner(&self) -> Option<BusyOwner> {
        lock_recover(&self.inner.busy).clone()
    }

    /// The refused claim's workspace pair when the cache belongs to ANOTHER workspace:
    /// `(ours, theirs)`. `None` for every other refusal, which the busy-owner message covers.
    pub(crate) fn foreign_workspace(&self) -> Option<(String, String)> {
        let mine = self.inner.workspace.clone()?;
        let theirs = lock_recover(&self.inner.busy).as_ref()?.workspace.clone()?;
        (mine != theirs).then_some((mine, theirs))
    }

    /// Whether this lease coordinates through a directory at all.
    pub(crate) fn is_managed(&self) -> bool {
        self.inner.path.is_some()
    }

    /// Whether this lease stands in for a managed claim that could not be made.
    pub(crate) fn coordination_failed(&self) -> bool {
        self.inner.coordination_failed
    }

    /// Re-read the declared Project against this lease's frozen cache scope.
    /// Unmanaged/reference leases preserve their existing no-scope behavior.
    pub(crate) fn scope_matches_project(&self) -> bool {
        let Some(cache) = self.inner.cache.as_ref() else { return true };
        let Some(root) = cache.workspace() else { return true };
        crate::project::at(root).is_ok_and(|project| cache.verify_project(&project).is_ok())
    }

    pub(crate) fn is_superseded(&self) -> bool {
        self.inner.superseded.load(Ordering::SeqCst)
    }

    pub(crate) fn is_released(&self) -> bool {
        self.inner.released.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn hold_file_lock_for_test(&self) -> impl Send {
        let path = self.inner.path.as_deref().expect("test lease is managed");
        LockGuard::acquire(
            &path.parent().expect("lease path has a parent").join(LEASE_LOCK_FILE),
            LOCK_WAIT,
        )
        .expect("test acquires lease file lock")
    }

    #[cfg(test)]
    pub(crate) fn invalidate_verdict_for_test(&self) {
        *lock_recover(&self.inner.checked_at) = None;
    }

    #[cfg(test)]
    pub(crate) fn hold_lifecycle_lock_for_test(
        &self,
    ) -> std::sync::MutexGuard<'_, Option<Instant>> {
        lock_recover(&self.inner.checked_at)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_checkpoint_lock_for_test(&self) {
        self.inner.fail_checkpoint_lock.store(true, Ordering::SeqCst);
    }

    /// Refuse the `nth` checkpoint this lease admits (1 = the next one).
    #[cfg(test)]
    pub(crate) fn fail_checkpoint_lock_after_for_test(&self, nth: u64) {
        self.inner.fail_checkpoint_lock_countdown.store(nth, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_restamp_for_test(&self) {
        self.inner.fail_managed_restamp.store(true, Ordering::SeqCst);
    }

    /// Release this process's record on a clean exit.
    ///
    /// Ownership survives a crash by design — the heartbeat is what tells the others the owner
    /// is gone, and it takes [`STALE_AFTER`] to conclude that. A process that exits on purpose
    /// knows better and says so, so a fresh process can claim without waiting that window out.
    /// A previously superseded lease remains terminal. Only OUR record is removed: a generation
    /// that took the workspace over in the meantime keeps it.
    pub(crate) fn release(&self) {
        self.inner.released.store(true, Ordering::SeqCst);
        self.report(OwnershipCheck::Lost);
        let mut checked_at = lock_recover(&self.inner.checked_at);
        // Handing the workspace back IS a verdict, and one a status answer must be able to
        // publish at once: this daemon owns nothing from here on.
        self.establish(false);
        *checked_at = Some(Instant::now());
        let Some(path) = self.inner.path.as_deref() else {
            return;
        };
        // Under the lock from the start, because a claim may be in flight on another thread and
        // the two must not interleave: taking the lock first means either the claim completes
        // and this removes the record it just wrote, or this runs first and the claim finds the
        // `released` flag and abandons. Either way no record is left behind that nobody owns.
        // The flag itself is set unconditionally — "this process is leaving" holds whether or
        // not the workspace was still ours, and a demoted lease that skipped it could reclaim
        // an abandoned workspace during shutdown and start writing caches on the way out.
        let guard = path
            .parent()
            .and_then(|dir| LockGuard::acquire(&dir.join(LEASE_LOCK_FILE), LOCK_WAIT).ok());
        let Some(_guard) = guard else { return };
        let mine = self.inner.token.load(Ordering::SeqCst);
        if read_record(path).is_some_and(|record| record.token == mine) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// This daemon's ownership generation; `None` when unmanaged. The generation is a
    /// coordination detail rather than an agent-facing fact — what a client needs to know is
    /// whether the backend still maintains the caches, which `owns_caches` answers — so it is
    /// carried in the claim log line and asserted here.
    #[cfg(test)]
    fn generation(&self) -> Option<u64> {
        self.inner.path.as_ref().map(|_| self.inner.generation.load(Ordering::SeqCst))
    }

    /// Re-read the record and decide ownership.
    ///
    /// The record IS the ownership: this daemon owns the workspace exactly while the record
    /// names its generation. Any other live record — higher OR lower — belongs to somebody
    /// else, and comparing numbers instead would break the moment `.build` is cleared: an
    /// older daemon would restore its own lower generation, the newer one would read that as
    /// "below mine, so still mine", and both would write the caches. A record whose owner
    /// stopped reporting, or none at all, means the workspace is free: claim it afresh under
    /// the lock, where two daemons doing the same thing get distinct generations and the loser
    /// demotes at its next check.
    /// The verdict this check produced, or `None` when it produced none: there was no record to
    /// read, and the claim that would have settled the question could not be made either.
    fn recheck_locked(&self, path: &Path, checked_at: &mut Option<Instant>) -> Option<bool> {
        #[cfg(test)]
        self.note_disk_check();
        let mine = self.inner.token.load(Ordering::SeqCst);
        match read_record(path) {
            Some(record) if record.token == mine => Some(true),
            Some(record) if !is_stale(&record) => {
                self.latch_superseded(&record);
                Some(false)
            }
            found => {
                let abandoned = found.map(|r| r.generation);
                let claimed = self.take_generation_locked(checked_at, |under_lock| {
                    under_lock.is_none_or(is_stale) // still free once we hold the lock
                });
                if claimed {
                    tracing::info!(
                        generation = self.inner.generation.load(Ordering::SeqCst),
                        abandoned,
                        "this workspace's derived caches were left unowned; claiming them"
                    );
                }
                // A workspace whose claim this attempt could not take is not an answer about
                // ownership: the attempt is paced, and the next one asks again.
                claimed.then_some(true)
            }
        }
    }

    fn latch_superseded(&self, owner: &LeaseRecord) {
        if self.inner.generation.load(Ordering::SeqCst) == UNCLAIMED {
            return;
        }
        if !self.inner.superseded.swap(true, Ordering::SeqCst) {
            self.report(OwnershipCheck::Lost);
            tracing::info!(
                mine = self.inner.generation.load(Ordering::SeqCst),
                owner = owner.generation,
                owner_pid = owner.pid,
                "another daemon generation now owns this workspace's derived caches; this one \
                 is permanently superseded"
            );
        }
    }
}

fn map_checkpointed_result<T, E>(
    result: ControlFlow<(), Result<T, E>>,
    stopped: Option<LeaseOperationOutcome<T, E>>,
) -> LeaseOperationOutcome<T, E> {
    match result {
        ControlFlow::Continue(Ok(value)) => LeaseOperationOutcome::Applied(value),
        ControlFlow::Continue(Err(error)) => {
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(error))
        }
        ControlFlow::Break(()) => {
            stopped.expect("checkpointed publication may break only after a failed checkpoint")
        }
    }
}

/// Restamp the owner's record for as long as this process holds the lease. Ownership must not
/// depend on a daemon happening to write something, so this runs on its own thread rather than
/// off the gated paths; it holds a [`Weak`], so the thread ends with the state that owns the
/// lease.
fn spawn_heartbeat(inner: Weak<Inner>) {
    let spawned =
        std::thread::Builder::new().name("bsl-cache-lease".to_owned()).spawn(move || loop {
            // Sleep in slices so the thread notices a dropped lease promptly instead of
            // outliving a test's temporary directory by a whole interval.
            let mut waited = Duration::ZERO;
            while waited < HEARTBEAT_INTERVAL {
                std::thread::sleep(Duration::from_secs(1));
                waited += Duration::from_secs(1);
                let Some(inner) = inner.upgrade() else { return };
                if inner.released.load(Ordering::SeqCst) {
                    return;
                }
                let lease = WorkspaceLease { inner };
                let _ = lease.owns_caches();
                if lease.is_superseded() {
                    return;
                }
            }
            let Some(inner) = inner.upgrade() else { return };
            if inner.released.load(Ordering::SeqCst) {
                return;
            }
            let lease = WorkspaceLease { inner };
            // Re-deciding ownership here, not just when a write path asks, promptly latches a
            // live foreign owner even while this daemon only serves reads.
            if !lease.owns_caches() {
                continue;
            }
            // One normal tick makes one short admission attempt. `publish_short` performs the
            // restamp before its visibility callback, so contention is left for the next tick
            // instead of creating a hidden retry loop here.
            let _ = heartbeat_tick(&lease);
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the cache-lease heartbeat");
    }
}

fn heartbeat_tick(lease: &WorkspaceLease) -> LeaseOperationOutcome<(), io::Error> {
    lease.publish_short(&mut (), |_| Ok(()))
}

fn read_record_result(path: &Path) -> io::Result<Option<LeaseRecord>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_record(path: &Path) -> Option<LeaseRecord> {
    read_record_result(path).ok().flatten()
}

/// Replace the record atomically: a reader takes no lock, so it must never observe a
/// half-written file. The temp name carries the pid so two writers cannot share one.
fn write_record(
    path: &Path,
    generation: u64,
    token: u64,
    workspace: Option<&str>,
) -> io::Result<()> {
    let record = LeaseRecord {
        generation,
        token,
        pid: std::process::id(),
        heartbeat_secs: now_secs(),
        version: Some(PROGRAM_VERSION.to_owned()),
        workspace: workspace.map(str::to_owned),
    };
    let body = serde_json::to_string(&record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Whether the record file at `path` was last written longer ago than a live owner's heartbeat
/// allows — the only staleness a record that will not parse can still show.
fn written_before_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > STALE_AFTER)
}

fn is_stale(record: &LeaseRecord) -> bool {
    now_secs().saturating_sub(record.heartbeat_secs) > STALE_AFTER.as_secs()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// An exclusive cross-process lock on a file, held until dropped — and released by the OS when
/// a crashed holder's handle closes, so no file or recorded pid has to be cleaned up after it.
pub(crate) struct ExclusiveFileLock {
    _guard: LockGuard,
}

impl ExclusiveFileLock {
    /// The lock, or `None` while another holder has it.
    pub(crate) fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        match LockGuard::try_acquire(path) {
            Ok(guard) => Ok(Some(Self { _guard: guard })),
            Err(error) if is_lock_contention(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// An advisory cross-process lock held for the duration of a claim, released when dropped
/// (including on a crash, since both platforms release on handle close).
struct LockGuard {
    _file: File,
}

impl LockGuard {
    /// Take the lock, retrying until `budget` runs out. Both platforms poll rather than block
    /// so a wedged peer degrades a claim into an unmanaged lease instead of hanging startup.
    fn acquire(path: &Path, budget: Duration) -> io::Result<Self> {
        let deadline = Instant::now() + budget;
        loop {
            match Self::try_acquire(path) {
                Ok(guard) => return Ok(guard),
                Err(e) if Instant::now() >= deadline => return Err(e),
                Err(e) if is_lock_contention(&e) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(e),
            }
        }
    }

    #[cfg(unix)]
    fn try_acquire(path: &Path) -> io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let file =
            OpenOptions::new().create(true).read(true).write(true).truncate(false).open(path)?;
        // SAFETY: `flock` takes a live descriptor and a flag word; `file` owns the descriptor
        // for the whole call and the lock is released when it closes.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }

    /// Windows has no `flock`, but an open with an empty share mode is exclusive by itself:
    /// a second opener fails with a sharing violation until the first handle closes.
    #[cfg(windows)]
    fn try_acquire(path: &Path) -> io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .share_mode(0)
            .open(path)?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn is_lock_contention(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
}

#[cfg(windows)]
fn is_lock_contention(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33))
}

#[cfg(not(any(unix, windows)))]
fn is_lock_contention(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_cache_scope_lease_admission_detects_project_drift() {
        let workspace = tempfile::tempdir().unwrap();
        let cache_parent = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("Configuration.xml"), "<Configuration/>").unwrap();
        let project = crate::project::at(workspace.path()).unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_project(
            &project,
            Some(&cache_parent.path().join("cache")),
            cache_parent.path(),
            None,
        )
        .unwrap();
        let lease = WorkspaceLease::claim_cache(&cache);
        assert!(lease.scope_matches_project());

        std::fs::write(
            workspace.path().join("bsl-analyzer.toml"),
            "[source]\nexclude = [\"generated\"]\n",
        )
        .unwrap();
        assert!(!lease.scope_matches_project());
    }

    fn publish_test<T>(
        lease: &WorkspaceLease,
        write: impl FnOnce() -> T,
    ) -> LeaseOperationOutcome<T, std::convert::Infallible> {
        let mut write = Some(write);
        lease.publish_short(&mut write, |write| {
            Ok((write.take().expect("test publication runs once"))())
        })
    }

    /// Before any check has succeeded, the cached verdict is an INITIAL value, not an answer.
    ///
    /// A claim that could not be written at startup — the cache lock held by a peer for the
    /// Every state a verdict can be in, and which of them is one.
    ///
    /// The distinction this pins is "answered" against "attempted": an unmanaged lease owns by
    /// construction, a claim that wrote its record answers `true`, a takeover and a release
    /// answer `false` — and an attempt that could not take the lock answers nothing at all and
    /// must leave the question open rather than republish whatever the value happened to be.
    #[test]
    fn the_established_verdict_is_the_one_a_check_answered() {
        // Unmanaged: nothing to check, and it owns everything it is asked about.
        let unmanaged = WorkspaceLease::unmanaged();
        assert!(unmanaged.ownership_was_checked(), "an unmanaged lease needs no check");
        assert!(unmanaged.owns_caches_cached());

        let dir = tempfile::tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());

        // A claim that wrote its record: checked, and it owns.
        let owner = WorkspaceLease::claim_cache(&cache);
        assert!(owner.ownership_was_checked(), "a claim that went through is an answer");
        assert!(owner.owns_caches_cached());

        // A takeover: checked, and it does not.
        let newer = WorkspaceLease::claim_cache(&cache);
        assert!(!owner.owns_caches_now(), "the older generation must observe the takeover");
        assert!(owner.ownership_was_checked());
        assert!(!owner.owns_caches_cached(), "a superseded daemon owns nothing");

        // Handing the workspace back: checked, and it does not.
        newer.release();
        assert!(newer.ownership_was_checked(), "a release is an answer about this daemon");
        assert!(!newer.owns_caches_cached());

        // And an attempt that answered nothing leaves the question where it was.
        let other = tempfile::tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(other.path());
        let unknown =
            WorkspaceLease::while_cache_lock_held(&cache, || WorkspaceLease::claim_cache(&cache));
        // The premise is read off DISK, not off the flag this half is about: a defect that
        // records attempts as answers must reach the assertion after the refresh rather than
        // fail the setup before it.
        assert!(!cache.lease_path().exists(), "the startup claim wrote a record after all");
        let before = unknown.disk_check_threads().len();
        assert!(
            !WorkspaceLease::while_cache_lock_held(&cache, || unknown.owns_caches_now()),
            "a refresh that cannot take the lock answers `not now`",
        );
        assert!(
            unknown.disk_check_threads().len() > before,
            "the refresh never reached the claim it is supposed to retry",
        );
        assert!(!cache.lease_path().exists(), "the refresh wrote a record after all");
        assert!(
            !unknown.ownership_was_checked(),
            "an attempt that answered nothing was recorded as the answer",
        );
        // The retry that CAN take it is what closes the question.
        assert!(unknown.owns_caches_now(), "the claim is retried and taken on the next check");
        assert!(unknown.ownership_was_checked());
        assert!(unknown.owns_caches_cached());
        unknown.release();
    }

    /// A refresh that answered nothing leaves the verdict a check DID answer standing.
    ///
    /// The other half of the same line, and the opposite damage: over an unknown verdict,
    /// recording the attempt invents a `false` nobody checked; over an established one it
    /// destroys a `true` this daemon is still entitled to — the record is gone from disk, which
    /// is exactly when a re-claim is owed, and the daemon that answers `no` about itself stops
    /// maintaining caches it still owns.
    #[test]
    fn a_failed_refresh_keeps_the_verdict_the_last_check_established() {
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        let lease = WorkspaceLease::claim_cache(&cache);
        // Premises off disk, for the same reason as above.
        assert!(cache.lease_path().exists(), "the startup claim wrote no record to refresh");
        std::fs::remove_file(cache.lease_path()).expect("the record is this daemon's to remove");

        // The refresh a heartbeat makes: the record this daemon knows is gone — a re-claim is
        // owed — and the lock that re-claim needs is held by a peer for longer than the wait.
        let before = lease.disk_check_threads().len();
        let answer = WorkspaceLease::while_cache_lock_held(&cache, || lease.owns_caches_now());
        let attempts = lease.disk_check_threads().len() - before;

        assert!(!answer, "a refresh that could take no lock answers `not now`");
        assert!(
            attempts >= 2,
            "the refresh made {attempts} disk attempts: it never reached the re-claim whose \
             failure this stand is about",
        );
        assert!(!cache.lease_path().exists(), "the refresh wrote a record after all");
        assert!(lease.ownership_was_checked(), "an established verdict was un-established");
        assert!(
            lease.owns_caches_cached(),
            "an attempt that answered nothing replaced the verdict a check had established",
        );
        lease.release();
    }

    /// Reading the published verdict takes no lock, so a request never queues behind the I/O a
    /// check is doing under the lifecycle lock — which is seconds of it whenever a peer holds
    /// the lock file.
    #[test]
    fn the_published_verdict_is_readable_while_a_check_holds_the_lifecycle_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        let lease = WorkspaceLease::claim_cache(&cache);
        assert!(lease.ownership_was_checked());

        // A real check, held inside the lifecycle lock by a lock file it cannot take.
        let held = WorkspaceLease::hold_cache_lock_for(&cache, Duration::from_secs(3));
        std::fs::remove_file(cache.lease_path()).expect("the record is this daemon's to remove");
        let checking = {
            let lease = lease.clone();
            std::thread::spawn(move || lease.owns_caches_now())
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lease.lifecycle_is_busy() {
            assert!(Instant::now() < deadline, "the check never reached the lifecycle lock");
            std::thread::yield_now();
        }

        let asking = Instant::now();
        let checked = lease.ownership_was_checked();
        let published = lease.owns_caches_cached();
        let waited = asking.elapsed();

        assert!(
            waited < Duration::from_millis(100),
            "reading the published verdict waited {waited:?} on a check that is doing I/O",
        );
        assert!(checked, "the verdict established before this check is still established");
        assert!(published, "and it still says what it said");

        held.join().unwrap();
        let _ = checking.join();
        // Whatever that check ended up doing — it either re-claimed the record it found gone,
        // or ran out its wait and answered nothing — the published verdict is still a verdict
        // somebody answered. An attempt is not one.
        assert!(
            lease.ownership_was_checked() && lease.owns_caches_cached(),
            "an attempt that answered nothing overwrote the verdict a check had established",
        );
        lease.release();
    }

    /// moment — leaves the lease managed, un-owned and UNCHECKED, and the comment on
    /// `owns_caches` says plainly that such a claim is retried on the next check. Until that
    /// retry runs the cached accessor answers `false`, which is what this daemon actually
    /// knows: it asked for the workspace and did not get it. A status answer says exactly
    /// that and waits for a background check to say otherwise — going to the lease from the
    /// request itself to find out is a file lock a peer may hold for seconds, paid by the
    /// caller, for a verdict the next background pass brings anyway.
    #[test]
    fn a_cached_ownership_verdict_before_any_check_succeeded_is_not_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        let lease =
            WorkspaceLease::while_cache_lock_held(&cache, || WorkspaceLease::claim_cache(&cache));

        assert!(!lease.ownership_was_checked(), "no check has succeeded yet");
        assert!(
            !lease.owns_caches_cached(),
            "and the cached verdict is still its initial value, which is the whole problem",
        );

        // The lock is free again, so the retry the comment promises can run.
        assert!(lease.owns_caches_now(), "the claim is retried and taken on the next check");
        assert!(lease.ownership_was_checked(), "and from here the cached accessor is an answer");
        assert!(lease.owns_caches_cached());
    }

    /// The cache of one workspace, built the way the CLI builds an explicit one: the root is
    /// stated by the caller, and so is the workspace the root serves.
    fn explicit_cache(
        cache_dir: &std::path::Path,
        workspace: &std::path::Path,
    ) -> crate::cache::WorkspaceCacheLayout {
        crate::cache::WorkspaceCacheLayout::from_root(cache_dir.to_path_buf())
            .with_workspace(workspace.to_path_buf())
    }

    /// A cache directory belongs to ONE workspace, and the refusal happens on a FRESH one —
    /// before any graph or search index exists. The reverted attempt checked for
    /// `bsl-graph.db`, which a daemon does not write until long after it claims the lease;
    /// that window is exactly what this input holds open (github#272).
    #[test]
    fn a_second_workspace_does_not_take_a_foreign_cache() {
        let cache_dir = tempfile::tempdir().unwrap();
        let first_ws = tempfile::tempdir().unwrap();
        let second_ws = tempfile::tempdir().unwrap();

        let first = WorkspaceLease::claim_cache(&explicit_cache(cache_dir.path(), first_ws.path()));
        assert!(first.owns_caches(), "the first workspace claims a fresh cache");

        let second =
            WorkspaceLease::claim_cache(&explicit_cache(cache_dir.path(), second_ws.path()));
        assert!(!second.owns_caches_now(), "a foreign workspace claimed the cache");
        assert!(!second.owns_caches_now(), "the refusal did not survive a retry");
        assert!(first.owns_caches_now(), "the refusal cost the first owner its cache");
        let (mine, theirs) = second.foreign_workspace().expect("the refusal names both workspaces");
        assert_ne!(mine, theirs);
        assert!(second.busy_owner().is_some(), "the status answer can say who holds it");
        assert!(second.ownership_was_checked(), "the refusal is an answer, not a silence");
        assert!(
            !second.owns_caches_cached(),
            "and the published verdict says this process owns nothing",
        );
    }

    /// Staleness does not make a foreign cache adoptable: the dead owner's derived state
    /// still belongs to ITS workspace, and serving this one from it would answer from
    /// another configuration's graph.
    #[test]
    fn a_stale_foreign_record_still_refuses() {
        let cache_dir = tempfile::tempdir().unwrap();
        let ours = tempfile::tempdir().unwrap();
        let layout = explicit_cache(cache_dir.path(), ours.path());

        let stale_foreign = LeaseRecord {
            generation: 7,
            token: 99,
            pid: 424242,
            heartbeat_secs: 0,
            version: Some(PROGRAM_VERSION.to_owned()),
            workspace: Some("/elsewhere/a-foreign-project".to_owned()),
        };
        std::fs::write(layout.lease_path(), serde_json::to_vec(&stale_foreign).unwrap()).unwrap();

        let lease = WorkspaceLease::claim_cache(&layout);

        assert!(!lease.owns_caches_now(), "a stale foreign record was adopted");
    }

    /// A record of an older program names no workspace: it proves nothing, so it is adopted —
    /// and the claim's own write names this daemon's workspace, closing the migration window
    /// for every writing path that follows.
    #[test]
    fn a_record_without_an_owner_is_adopted_and_named() {
        let cache_dir = tempfile::tempdir().unwrap();
        let ours = tempfile::tempdir().unwrap();
        let layout = explicit_cache(cache_dir.path(), ours.path());
        let nameless = LeaseRecord {
            generation: 3,
            token: 7,
            pid: 424242,
            heartbeat_secs: 0,
            version: None,
            workspace: None,
        };
        std::fs::write(layout.lease_path(), serde_json::to_vec(&nameless).unwrap()).unwrap();

        let lease = WorkspaceLease::claim_cache(&layout);

        assert!(lease.owns_caches_now(), "a nameless old record was not adopted");
        let record = read_record(&layout.lease_path()).expect("the claim wrote a record");
        assert_eq!(
            record.workspace.as_deref(),
            Some(workspace_identity(ours.path()).as_str()),
            "the adopted record still names no workspace",
        );
    }

    /// The owner survives every rewrite of the record: a restamp that dropped it would reopen
    /// the cache to the next foreign claim, so this pins the field across a checkpointed
    /// publish rather than only across the first claim.
    #[test]
    fn the_workspace_owner_survives_a_restamp() {
        let cache_dir = tempfile::tempdir().unwrap();
        let ours = tempfile::tempdir().unwrap();
        let third_ws = tempfile::tempdir().unwrap();
        let layout = explicit_cache(cache_dir.path(), ours.path());
        let lease = WorkspaceLease::claim_cache(&layout);
        assert!(lease.owns_caches());

        let outcome = lease.publish_short(&mut (), |_| Ok::<(), ()>(()));
        assert!(matches!(outcome, LeaseOperationOutcome::Applied(())), "{outcome:?}");

        let record = read_record(&layout.lease_path()).expect("the restamp kept a record");
        assert_eq!(record.workspace.as_deref(), Some(workspace_identity(ours.path()).as_str()));

        // And the surviving field still refuses the foreign workspace.
        let foreign =
            WorkspaceLease::claim_cache(&explicit_cache(cache_dir.path(), third_ws.path()));
        assert!(!foreign.owns_caches_now());
    }

    /// The identity folds path spellings — `..`, a trailing `.` — while bind-mounts stay out
    /// of canonicalization's reach; the check then errs by refusing, never by serving.
    #[test]
    fn the_identity_folds_path_spellings() {
        let workspace = tempfile::tempdir().unwrap();
        let through_dot = workspace.path().join(".");

        assert_eq!(workspace_identity(workspace.path()), workspace_identity(&through_dot));
    }

    /// Two roots whose names differ only in bytes that are not UTF-8 are two workspaces: a
    /// lossy spelling would fold both into one identity and let one claim the other's cache.
    #[cfg(unix)]
    #[test]
    fn roots_differing_in_non_utf8_bytes_are_distinct_identities() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let base = tempfile::tempdir().unwrap();
        let first = base.path().join(OsStr::from_bytes(b"ws-\xff"));
        let second = base.path().join(OsStr::from_bytes(b"ws-\xfe"));
        if std::fs::create_dir_all(&first).is_err() || std::fs::create_dir_all(&second).is_err() {
            // A filesystem that enforces UTF-8 names cannot hold the pair at all.
            return;
        }

        assert_ne!(workspace_identity(&first), workspace_identity(&second));
    }

    fn checkpoint_test<T>(
        lease: &WorkspaceLease,
        write: impl FnOnce(&mut dyn FnMut() -> ControlFlow<()>) -> ControlFlow<(), T>,
    ) -> LeaseOperationOutcome<T, std::convert::Infallible> {
        lease.publish_checkpointed(|checkpoint| {
            write(checkpoint).map_continue(Ok::<_, std::convert::Infallible>)
        })
    }

    #[test]
    fn explicit_cache_layout_holds_lease_outside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let layout = crate::cache::WorkspaceCacheLayout::from_root(cache.path().to_path_buf());

        let lease = WorkspaceLease::claim_cache(&layout);

        assert!(lease.owns_caches());
        assert!(layout.lease_path().exists());
        assert!(!workspace.path().join(".build").exists());
        lease.release();
    }

    fn record_at(path: &Path) -> LeaseRecord {
        read_record(path).expect("the lease record is readable")
    }

    fn lease_path(root: &Path) -> PathBuf {
        crate::cache::workspace_cache_dir(root).join(LEASE_FILE)
    }

    fn unclaimed_lease(root: &Path) -> WorkspaceLease {
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        WorkspaceLease {
            inner: Arc::new(Inner {
                path: Some(cache.lease_path()),
                cache: Some(cache.clone()),
                generation: AtomicU64::new(UNCLAIMED),
                token: AtomicU64::new(0),
                workspace: None,
                owns: AtomicBool::new(false),
                superseded: AtomicBool::new(false),
                released: AtomicBool::new(false),
                checked_at: Mutex::new(None),
                established: AtomicBool::new(false),
                stamped_at: Mutex::new(None),
                busy: Mutex::new(None),
                observers: Mutex::new(Vec::new()),
                last_check: Mutex::new(None),
                coordination_failed: false,
                #[cfg(test)]
                disk_check_threads: Mutex::new(Vec::new()),
                fail_managed_lock: AtomicBool::new(false),
                fail_managed_read: AtomicBool::new(false),
                fail_managed_restamp: AtomicBool::new(false),
                fail_checkpoint_lock: AtomicBool::new(false),
                #[cfg(test)]
                fail_checkpoint_lock_countdown: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    #[test]
    fn callback_error_preserves_operation_origin() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let mut prepared = ();
        let outcome = lease.publish_short(&mut prepared, |_| Err::<(), _>("store failed"));
        assert!(matches!(
            outcome,
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation("store failed"))
        ));
    }

    #[test]
    fn contention_unclaimed_and_missing_are_transient() {
        let busy_dir = tempfile::tempdir().unwrap();
        let busy = WorkspaceLease::claim(busy_dir.path());
        let held = busy.hold_file_lock_for_test();
        let mut prepared = ();
        assert!(matches!(
            busy.publish_short(&mut prepared, |_| Ok::<_, ()>(())),
            LeaseOperationOutcome::TransientRefusal
        ));
        drop(held);

        let unclaimed_dir = tempfile::tempdir().unwrap();
        let unclaimed = unclaimed_lease(unclaimed_dir.path());
        assert!(matches!(
            unclaimed.publish_short(&mut prepared, |_| Ok::<_, ()>(())),
            LeaseOperationOutcome::TransientRefusal
        ));

        let missing_dir = tempfile::tempdir().unwrap();
        let missing = WorkspaceLease::claim(missing_dir.path());
        std::fs::remove_file(lease_path(missing_dir.path())).unwrap();
        assert!(matches!(
            missing.publish_short(&mut prepared, |_| Ok::<_, ()>(())),
            LeaseOperationOutcome::TransientRefusal
        ));
    }

    #[test]
    fn managed_lease_io_error_is_not_transient() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let mut prepared = ();

        lease.inner.fail_managed_lock.store(true, Ordering::SeqCst);
        assert!(matches!(
            lease.publish_short(&mut prepared, |_| Ok::<_, ()>(())),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(_))
        ));
        lease.inner.fail_managed_read.store(true, Ordering::SeqCst);
        assert!(matches!(
            lease.publish_short(&mut prepared, |_| Ok::<_, ()>(())),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(_))
        ));
        std::fs::write(lease_path(dir.path()), "not a lease record").unwrap();
        assert!(matches!(
            lease.publish_short(&mut prepared, |_| Ok::<_, ()>(())),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(_))
        ));
    }

    #[test]
    fn heartbeat_refusal_waits_for_normal_tick() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let held = lease.hold_file_lock_for_test();

        assert!(matches!(heartbeat_tick(&lease), LeaseOperationOutcome::TransientRefusal));
        drop(held);
        assert!(matches!(heartbeat_tick(&lease), LeaseOperationOutcome::Applied(())));
    }

    #[test]
    fn publish_short_restamp_failure_skips_commit_and_success_commits_once() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let mut commits = 0_u8;
        lease.inner.fail_managed_restamp.store(true, Ordering::SeqCst);
        assert!(matches!(
            lease.publish_short(&mut commits, |commits| {
                *commits += 1;
                Ok::<_, ()>(())
            }),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(_))
        ));
        assert_eq!(commits, 0);
        assert!(matches!(
            lease.publish_short(&mut commits, |commits| {
                *commits += 1;
                Ok::<_, ()>(())
            }),
            LeaseOperationOutcome::Applied(())
        ));
        assert_eq!(commits, 1);
    }

    #[test]
    fn live_foreign_token_returns_superseded() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let foreign = LeaseRecord {
            generation: lease.generation().unwrap() + 1,
            token: new_token(),
            pid: 424242,
            heartbeat_secs: now_secs(),
            version: Some(PROGRAM_VERSION.to_owned()),
            workspace: None,
        };
        std::fs::write(lease_path(dir.path()), serde_json::to_string(&foreign).unwrap()).unwrap();
        let mut prepared = ();
        assert!(matches!(
            lease.publish_short(&mut prepared, |_| Ok::<_, ()>(())),
            LeaseOperationOutcome::Superseded
        ));
    }

    #[test]
    fn pre_admission_release_skips_callback() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        lease.release();
        let mut commits = 0_u8;
        assert!(matches!(
            lease.publish_short(&mut commits, |commits| {
                *commits += 1;
                Ok::<_, ()>(())
            }),
            LeaseOperationOutcome::Released
        ));
        assert_eq!(commits, 0);
    }

    #[test]
    fn checkpointed_atomic_publish_rolls_back_at_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let worker_lease = lease.clone();
        let releaser_lease = lease.clone();
        let observer = lease.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_lease.publish_checkpointed(|checkpoint| {
                let mut pending = vec!["row"];
                entered_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                if checkpoint().is_break() {
                    pending.clear();
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(Ok::<_, ()>(pending))
            })
        });
        entered_rx.recv().unwrap();
        let releaser = std::thread::spawn(move || releaser_lease.release());
        while !observer.is_released() {
            std::thread::yield_now();
        }
        continue_tx.send(()).unwrap();
        assert!(matches!(worker.join().unwrap(), LeaseOperationOutcome::Released));
        releaser.join().unwrap();
    }

    #[test]
    fn checkpoint_restamp_error_rolls_back_as_operation_error() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let operation_lease = lease.clone();
        let mut rolled_back = false;
        let outcome = lease.publish_checkpointed(|checkpoint| {
            operation_lease.inner.fail_managed_restamp.store(true, Ordering::SeqCst);
            *lock_recover(&operation_lease.inner.stamped_at) = None;
            if checkpoint().is_break() {
                rolled_back = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(Ok::<_, ()>(()))
        });
        assert!(rolled_back);
        assert!(matches!(
            outcome,
            LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(_))
        ));
    }

    #[test]
    fn checkpoint_observes_live_foreign_token_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let root = dir.path().to_path_buf();
        let newer = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured = std::sync::Arc::clone(&newer);
        CHECKPOINT_UNLOCK_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                *lock_recover(&captured) = Some(WorkspaceLease::claim(&root));
            }));
        });

        let mut rolled_back = false;
        let outcome = lease.publish_checkpointed(|checkpoint| {
            if checkpoint().is_break() {
                rolled_back = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(Ok::<_, ()>(()))
        });

        assert!(rolled_back, "foreign ownership stops the transaction at its checkpoint");
        assert!(matches!(outcome, LeaseOperationOutcome::Superseded));
        assert!(newer.lock().unwrap().is_some(), "the newer claim wrote a real lease record");
    }

    #[test]
    fn first_terminal_cause_survives_release() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        lease.inner.superseded.store(true, Ordering::SeqCst);
        lease.inner.released.store(true, Ordering::SeqCst);
        let mut prepared = ();
        assert!(matches!(
            lease.publish_short(&mut prepared, |_| Ok::<_, ()>(())),
            LeaseOperationOutcome::Superseded
        ));
    }

    #[test]
    fn checkpointed_operation_outlives_stale_after_without_self_supersession() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let stale_mine = LeaseRecord {
            generation: lease.generation().unwrap(),
            token: lease.inner.token.load(Ordering::SeqCst),
            pid: std::process::id(),
            heartbeat_secs: now_secs() - STALE_AFTER.as_secs() - 1,
            version: Some(PROGRAM_VERSION.to_owned()),
            workspace: None,
        };
        std::fs::write(lease_path(dir.path()), serde_json::to_string(&stale_mine).unwrap())
            .unwrap();
        let outcome = lease.publish_checkpointed(|checkpoint| {
            assert!(checkpoint().is_continue());
            ControlFlow::Continue(Ok::<_, ()>(()))
        });
        assert!(matches!(outcome, LeaseOperationOutcome::Applied(())));
        assert!(!lease.is_superseded());
        assert!(!is_stale(&record_at(&lease_path(dir.path()))));
    }

    /// The newest claim owns the workspace: the daemon that started first stops writing derived
    /// caches, the one that started last keeps writing. Reverse the comparison in `recheck` and
    /// the draining generation would keep ownership while the client-facing one goes read-only.
    #[test]
    fn the_newest_claim_owns_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let first = WorkspaceLease::claim(dir.path());
        assert!(first.owns_caches(), "the only daemon owns the caches");

        let second = WorkspaceLease::claim(dir.path());
        assert_eq!(second.generation(), Some(first.generation().unwrap() + 1));
        assert!(second.owns_caches(), "the newest claim owns the caches");

        // The verdict is cached, so the demotion lands on the first re-read.
        std::thread::sleep(VERDICT_TTL);
        assert!(!first.owns_caches(), "the superseded generation stops writing");
        assert!(second.owns_caches());
    }

    /// The fence a long build's publish needs: ownership must hold FOR the write, not merely
    /// before it. A daemon whose record has been outbid runs nothing, however recently its
    /// cached verdict said otherwise — otherwise a build that started while it owned the
    /// workspace would rename itself over what the new owner had just published.
    #[test]
    fn publish_fence_latches_supersession() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        assert!(matches!(
            publish_test(&lease, || "written"),
            LeaseOperationOutcome::Applied("written")
        ));

        let newer = LeaseRecord {
            generation: lease.generation().unwrap() + 1,
            token: new_token(),
            pid: 424242,
            heartbeat_secs: now_secs(),
            version: Some(PROGRAM_VERSION.to_owned()),
            workspace: None,
        };
        std::fs::write(lease_path(dir.path()), serde_json::to_string(&newer).unwrap()).unwrap();

        // No sleep: the fence reads the record itself rather than trusting the cached verdict,
        // which still says this daemon owns the workspace.
        assert!(lease.owns_caches(), "the cached verdict has not expired yet");
        assert!(
            matches!(publish_test(&lease, || "written"), LeaseOperationOutcome::Superseded),
            "the write is refused anyway"
        );
        assert!(lease.is_superseded(), "the live foreign token is terminal");
    }

    #[test]
    fn typed_fence_distinguishes_transient_and_terminal_refusals() {
        let busy_dir = tempfile::tempdir().unwrap();
        let busy = WorkspaceLease::claim(busy_dir.path());
        let held = busy.hold_file_lock_for_test();
        assert!(matches!(
            publish_test(&busy, || "written"),
            LeaseOperationOutcome::TransientRefusal
        ));
        drop(held);

        let unclaimed_dir = tempfile::tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(unclaimed_dir.path());
        cache.ensure().unwrap();
        let unclaimed = WorkspaceLease {
            inner: Arc::new(Inner {
                path: Some(cache.lease_path()),
                cache: Some(cache.clone()),
                generation: AtomicU64::new(UNCLAIMED),
                token: AtomicU64::new(0),
                workspace: None,
                owns: AtomicBool::new(false),
                superseded: AtomicBool::new(false),
                released: AtomicBool::new(false),
                checked_at: Mutex::new(None),
                established: AtomicBool::new(false),
                stamped_at: Mutex::new(None),
                busy: Mutex::new(None),
                observers: Mutex::new(Vec::new()),
                last_check: Mutex::new(None),
                coordination_failed: false,
                #[cfg(test)]
                disk_check_threads: Mutex::new(Vec::new()),
                fail_managed_lock: AtomicBool::new(false),
                fail_managed_read: AtomicBool::new(false),
                fail_managed_restamp: AtomicBool::new(false),
                fail_checkpoint_lock: AtomicBool::new(false),
                #[cfg(test)]
                fail_checkpoint_lock_countdown: std::sync::atomic::AtomicU64::new(0),
            }),
        };
        assert!(matches!(
            publish_test(&unclaimed, || "written"),
            LeaseOperationOutcome::TransientRefusal
        ));

        let foreign_dir = tempfile::tempdir().unwrap();
        let foreign = WorkspaceLease::claim(foreign_dir.path());
        let newer = LeaseRecord {
            generation: foreign.generation().unwrap() + 1,
            token: new_token(),
            pid: 424242,
            heartbeat_secs: now_secs(),
            version: Some(PROGRAM_VERSION.to_owned()),
            workspace: None,
        };
        std::fs::write(lease_path(foreign_dir.path()), serde_json::to_string(&newer).unwrap())
            .unwrap();
        assert!(matches!(publish_test(&foreign, || "written"), LeaseOperationOutcome::Superseded));
        assert!(foreign.is_superseded());

        let released_dir = tempfile::tempdir().unwrap();
        let released = WorkspaceLease::claim(released_dir.path());
        released.release();
        assert!(matches!(publish_test(&released, || "written"), LeaseOperationOutcome::Released));
    }

    #[test]
    fn checkpoint_refreshes_heartbeat_and_preserves_callback_errors() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let path = lease_path(dir.path());
        let record = LeaseRecord {
            generation: lease.generation().unwrap(),
            token: lease.inner.token.load(Ordering::SeqCst),
            pid: std::process::id(),
            heartbeat_secs: 0,
            version: Some(PROGRAM_VERSION.to_owned()),
            workspace: None,
        };
        std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();

        let outcome = checkpoint_test(&lease, |checkpoint| {
            assert_eq!(checkpoint(), ControlFlow::Continue(()));
            assert!(record_at(&path).heartbeat_secs > 0);
            ControlFlow::Continue(Err::<(), _>("store failed"))
        });

        assert!(matches!(outcome, LeaseOperationOutcome::Applied(Err("store failed"))));
    }

    /// The window's UPPER bound, asserted on the constant rather than by waiting.
    ///
    /// A test that sleeps for the window cannot see how large the window is: it derives
    /// its own duration from the value under test and passes at any size. The bound is
    /// arithmetic, so it belongs here as arithmetic — throttling raises the record's
    /// worst-case age from the gap between checkpoints to that gap plus the window, and
    /// `is_stale` compares strictly, so a one-second window is what keeps every gap that
    /// is safe today (up to 59 s) exactly as safe.
    #[test]
    fn the_restamp_window_cannot_make_a_safe_checkpoint_gap_stale() {
        let largest_gap_safe_today = STALE_AFTER - Duration::from_secs(1);
        assert!(
            CHECKPOINT_MIN_INTERVAL + largest_gap_safe_today <= STALE_AFTER,
            "a {CHECKPOINT_MIN_INTERVAL:?} window lets a {largest_gap_safe_today:?} gap go stale"
        );
    }

    /// The hot consumer opens a NEW fence on every pass, so the window has to live on
    /// the lease. Scoped to one fence it would reset on each pass and hold nothing back.
    ///
    /// The observable is the record's modification time, not `!is_stale`: `is_stale`
    /// compares strictly against `STALE_AFTER`, so a gap of `STALE_AFTER - window` never
    /// makes a record stale at any window size, and a control phrased over it would pass
    /// on a window of sixty seconds just as happily as on one second.
    #[test]
    fn the_restamp_window_spans_separate_fences() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let path = lease_path(dir.path());
        let stamp = || std::fs::metadata(&path).unwrap().modified().unwrap();
        let restamp = |lease: &WorkspaceLease| {
            checkpoint_test(lease, |checkpoint| {
                assert_eq!(checkpoint(), ControlFlow::Continue(()));
                ControlFlow::Continue(())
            })
        };

        assert!(matches!(restamp(&lease), LeaseOperationOutcome::Applied(())));
        let first = stamp();

        std::thread::sleep(Duration::from_millis(50));
        assert!(matches!(restamp(&lease), LeaseOperationOutcome::Applied(())));
        assert_eq!(stamp(), first, "a second fence inside the window restamped the record");

        std::thread::sleep(CHECKPOINT_MIN_INTERVAL + Duration::from_millis(100));
        assert!(matches!(restamp(&lease), LeaseOperationOutcome::Applied(())));
        assert!(stamp() > first, "a fence past the window did not restamp the record");
    }

    /// Throttling the restamp must not throttle the stop check. The two live in one
    /// closure, and holding the whole closure back would let a callback keep publishing
    /// over a new owner for the length of the window.
    #[test]
    fn a_throttled_checkpoint_still_reports_a_release() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let releaser = lease.clone();
        let observer = lease.clone();

        let outcome = checkpoint_test(&lease, |checkpoint| {
            // The first call is never throttled, so the second is the one under test:
            // it falls inside the window and must still answer Break.
            assert_eq!(checkpoint(), ControlFlow::Continue(()));
            // `release` sets the flag before it goes for the lock this fence is holding,
            // so waiting on the flag costs nothing while waiting on the call would block
            // for the whole lock timeout.
            std::thread::spawn(move || releaser.release());
            while !observer.is_released() {
                std::thread::yield_now();
            }
            if checkpoint().is_break() {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });

        assert!(matches!(outcome, LeaseOperationOutcome::Released));
    }

    #[test]
    fn release_during_checkpointed_callback_rolls_back_as_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let worker_lease = lease.clone();
        let releaser_lease = lease.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (check_tx, check_rx) = std::sync::mpsc::channel();
        let rolled_back = Arc::new(AtomicBool::new(false));
        let worker_rolled_back = Arc::clone(&rolled_back);

        let worker = std::thread::spawn(move || {
            checkpoint_test(&worker_lease, |checkpoint| {
                let mut transaction = vec!["pending"];
                entered_tx.send(()).unwrap();
                check_rx.recv().unwrap();
                if checkpoint().is_break() {
                    transaction.clear();
                    worker_rolled_back.store(true, Ordering::SeqCst);
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(transaction)
            })
        });
        entered_rx.recv().unwrap();
        let releaser = std::thread::spawn(move || releaser_lease.release());
        while !lease.is_released() {
            std::thread::yield_now();
        }
        check_tx.send(()).unwrap();

        assert!(matches!(worker.join().unwrap(), LeaseOperationOutcome::Released));
        assert!(rolled_back.load(Ordering::SeqCst));
        releaser.join().unwrap();
    }

    #[test]
    fn release_waits_for_only_the_admitted_batch_and_refuses_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let worker_lease = lease.clone();
        let releaser_lease = lease.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            publish_test(&worker_lease, || {
                entered_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                "first batch"
            })
        });
        entered_rx.recv().unwrap();
        let releaser = std::thread::spawn(move || {
            releaser_lease.release();
            released_tx.send(()).unwrap();
        });
        while !lease.is_released() {
            std::thread::yield_now();
        }
        assert!(released_rx.try_recv().is_err(), "release waits for the admitted batch");
        finish_tx.send(()).unwrap();
        assert!(matches!(worker.join().unwrap(), LeaseOperationOutcome::Applied("first batch")));
        released_rx.recv().unwrap();
        releaser.join().unwrap();
        let calls = AtomicU64::new(0);
        assert!(matches!(
            publish_test(&lease, || calls.fetch_add(1, Ordering::SeqCst)),
            LeaseOperationOutcome::Released
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn takeover_between_batches_preserves_the_first_and_terminates_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let applied = Arc::new(Mutex::new(Vec::new()));
        let first = Arc::clone(&applied);
        assert!(matches!(
            publish_test(&lease, || first.lock().unwrap().extend(0..64)),
            LeaseOperationOutcome::Applied(())
        ));
        let _newer = WorkspaceLease::claim(dir.path());
        let second = Arc::clone(&applied);
        assert!(matches!(
            publish_test(&lease, || second.lock().unwrap().push(64)),
            LeaseOperationOutcome::Superseded
        ));
        assert_eq!(applied.lock().unwrap().len(), 64);
    }

    /// Generations restart at 1 whenever the record is deleted, so the number cannot be the
    /// identity: with three daemons alive and the record wiped, the one that reclaims is
    /// reassigned a generation an older daemon still holds. If ownership compared numbers, both
    /// would recognise the record as their own and write the caches for good — the exact state
    /// the lease exists to prevent. The token is what tells them apart.
    #[test]
    fn a_reused_generation_does_not_make_two_daemons_owners() {
        let dir = tempfile::tempdir().unwrap();
        let first = WorkspaceLease::claim(dir.path()); // generation 1
        let second = WorkspaceLease::claim(dir.path()); // generation 2
        let third = WorkspaceLease::claim(dir.path()); // generation 3
        assert_eq!(first.generation(), Some(1), "the numbering this scenario turns on");

        let first_heartbeat = first.hold_lifecycle_lock_for_test();
        let third_heartbeat = third.hold_lifecycle_lock_for_test();
        std::fs::remove_file(lease_path(dir.path())).unwrap();
        std::thread::sleep(VERDICT_TTL);

        // The middle daemon reclaims first and is handed generation 1 — the number the FIRST
        // daemon is still running under.
        assert!(second.owns_caches());
        assert_eq!(second.generation(), Some(1), "the wipe restarted the numbering");

        drop(first_heartbeat);
        drop(third_heartbeat);
        assert!(!first.owns_caches(), "the generation it shares is not its claim");
        assert!(!third.owns_caches(), "and the newest daemon gave the workspace up too");
        assert!(
            matches!(publish_test(&first, || "published"), LeaseOperationOutcome::Superseded),
            "its writes are refused"
        );
    }

    /// `.build` is a cache directory users are told they may delete, and deleting it takes the
    /// lock file with it. Every live daemon would then fail to claim forever — all read-only
    /// over a workspace nobody owns — unless the claim puts the directory back.
    #[test]
    fn deleting_the_whole_cache_directory_does_not_strand_every_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        std::fs::remove_dir_all(crate::cache::workspace_cache_dir(dir.path())).unwrap();

        std::thread::sleep(VERDICT_TTL);
        assert!(lease.owns_caches(), "the daemon recreates what it needs and carries on");
        assert!(lease_path(dir.path()).exists(), "and the record is back on disk");
    }

    /// A claim that loses the lock must not leave the daemon outside the coordination for good:
    /// an unclaimed lease owns nothing (so it cannot be a second writer), and it keeps trying,
    /// so a moment's contention costs a check interval rather than the daemon's whole life.
    #[test]
    fn transient_unclaimed_is_not_superseded() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = crate::cache::ensure_workspace_cache_dir(dir.path()).unwrap();
        let held = LockGuard::acquire(&cache_dir.join(LEASE_LOCK_FILE), LOCK_WAIT).unwrap();

        let lease = WorkspaceLease::claim(dir.path());
        assert_eq!(lease.generation(), Some(UNCLAIMED), "the claim could not be written");
        assert!(!lease.owns_caches(), "and an unclaimed lease writes nothing");
        assert!(!lease.is_superseded(), "temporary lock contention is not supersession");

        drop(held);
        std::thread::sleep(VERDICT_TTL);
        assert!(lease.owns_caches(), "the retry claims the workspace once the lock frees");
        assert_eq!(record_at(&lease_path(dir.path())).generation, lease.generation().unwrap());
        lease.release();
        assert!(lease.is_released());
        assert!(!lease.is_superseded(), "shutdown remains distinct from supersession");
    }

    /// Once a daemon has actually observed a live foreign owner, that observation is terminal:
    /// neither the owner's clean exit nor a third claim lets the old process write again.
    #[test]
    fn observed_foreign_owner_is_permanent() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = WorkspaceLease::claim(dir.path());
        let short_lived = WorkspaceLease::claim(dir.path());

        std::thread::sleep(VERDICT_TTL);
        assert!(!daemon.owns_caches(), "the newer claim demoted the daemon");
        assert!(daemon.is_superseded());

        short_lived.release();
        let third = WorkspaceLease::claim(dir.path());
        third.release();
        std::fs::remove_dir_all(crate::cache::workspace_cache_dir(dir.path())).unwrap();

        assert!(!daemon.owns_caches(), "a superseded daemon never reclaims");
        assert!(!daemon.owns_caches_now());
        assert!(matches!(publish_test(&daemon, || "published"), LeaseOperationOutcome::Superseded));
        assert!(!daemon.take_generation(|_| true));
        assert!(!crate::cache::workspace_cache_dir(dir.path()).exists(), "no disk I/O after latch");
    }

    #[test]
    fn observed_owner_race_cannot_reclaim() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let contender = lease.clone();
        let newer = WorkspaceLease::claim(dir.path());
        let mut lifecycle = lock_recover(&lease.inner.checked_at);
        let owner = read_record(&lease_path(dir.path())).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let reclaim = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(contender.take_generation(|_| true)).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "reclaim waits behind the lifecycle observation"
        );
        lease.latch_superseded(&owner);
        *lifecycle = Some(Instant::now());
        drop(lifecycle);
        newer.release();

        assert!(!done_rx.recv().unwrap(), "a clone cannot race the terminal latch");
        reclaim.join().unwrap();
        assert!(lease.is_superseded());
        assert!(read_record(&lease_path(dir.path())).is_none());
    }

    /// Releasing is final for the process that did it: a background pass still finishing during
    /// shutdown must not read the removed record as "nobody owns this" and claim the workspace
    /// back — the daemon it was handed to would then be silently demoted by a process on its
    /// way out.
    #[test]
    fn a_released_lease_never_takes_the_workspace_back() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        lease.release();

        std::thread::sleep(VERDICT_TTL);
        assert!(!lease.owns_caches(), "a released lease stays released");
        assert!(
            read_record(&lease_path(dir.path())).is_none(),
            "and it does not re-create the record it just handed back",
        );
    }

    /// A claim in flight when the process decides to leave must not complete afterwards. The
    /// record it would write is one this daemon will never heartbeat and never remove — the
    /// release has already run — so every other daemon would treat the workspace as live and
    /// owned until it went stale a minute later.
    #[test]
    fn a_claim_does_not_complete_after_the_process_has_released() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = crate::cache::ensure_workspace_cache_dir(dir.path()).unwrap();
        let held = LockGuard::acquire(&cache_dir.join(LEASE_LOCK_FILE), LOCK_WAIT).unwrap();

        // The claim cannot get the lock, so the lease starts out owning nothing.
        let lease = WorkspaceLease::claim(dir.path());
        assert_eq!(lease.generation(), Some(UNCLAIMED));

        lease.release();
        drop(held);

        std::thread::sleep(VERDICT_TTL);
        assert!(!lease.owns_caches(), "a released lease does not go on to claim");
        assert!(
            read_record(&lease_path(dir.path())).is_none(),
            "and leaves no record for other daemons to wait out",
        );
    }

    /// Releasing must only ever drop OUR record: a daemon that took the workspace over while
    /// we were shutting down keeps it, rather than being silently unclaimed by our exit.
    #[test]
    fn releasing_leaves_a_newer_owners_record_alone() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let path = lease_path(dir.path());
        let newer = LeaseRecord {
            generation: lease.generation().unwrap() + 1,
            token: new_token(),
            pid: 424242,
            heartbeat_secs: now_secs(),
            version: Some(PROGRAM_VERSION.to_owned()),
            workspace: None,
        };
        std::fs::write(&path, serde_json::to_string(&newer).unwrap()).unwrap();

        lease.release();
        assert_eq!(record_at(&path).generation, newer.generation, "the newer owner still holds it");
    }

    /// A cache whose lease directory cannot be prepared refuses derived-cache writes, while an
    /// intentionally unmanaged lease still allows unrelated reference-profile work.
    #[test]
    fn workspace_cache_coordination_failed_blocks_derived_writes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, "").unwrap();

        let lease = WorkspaceLease::claim(&file);
        assert_eq!(lease.generation(), None, "nothing was claimed");
        assert!(lease.coordination_failed());
        assert!(!lease.owns_caches());
        assert!(!lease.owns_caches_cached());
        assert!(!lease.owns_caches_now());
        let mut wrote = false;
        assert!(matches!(
            lease.publish_short(&mut wrote, |value| {
                *value = true;
                Ok::<_, std::io::Error>(())
            }),
            LeaseOperationOutcome::Released
        ));
        assert!(!wrote, "publication fence must refuse writes after claim failure");
        assert!(WorkspaceLease::unmanaged().owns_caches(), "reference work remains unmanaged");
    }

    /// A stale record that was never observed while live is not evidence of supersession. The
    /// current daemon may reclaim it, preserving recovery after crashes and missed brief claims.
    #[test]
    fn non_witness_states_remain_reclaimable() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let mine = lease.generation().unwrap();

        // A newer daemon claims, then dies: its record survives with a heartbeat that stops.
        let path = lease_path(dir.path());
        let ghost = LeaseRecord {
            generation: mine + 5,
            token: new_token(),
            pid: 424242,
            heartbeat_secs: now_secs() - STALE_AFTER.as_secs() - 1,
            version: Some(PROGRAM_VERSION.to_owned()),
            workspace: None,
        };
        std::fs::write(&path, serde_json::to_string(&ghost).unwrap()).unwrap();

        assert!(lease.owns_caches_now(), "an abandoned workspace is taken back");
        assert!(!lease.is_superseded());
        assert_eq!(lease.generation(), Some(mine + 6), "the reclaim steps above the ghost");
        assert_eq!(record_at(&path).generation, mine + 6);

        let corrupt_dir = tempfile::tempdir().unwrap();
        let corrupt = WorkspaceLease::claim(corrupt_dir.path());
        std::fs::write(lease_path(corrupt_dir.path()), "not a lease record").unwrap();
        assert!(!corrupt.owns_caches_now(), "a corrupt record may still belong to a live owner");
        std::fs::OpenOptions::new()
            .write(true)
            .open(lease_path(corrupt_dir.path()))
            .unwrap()
            .set_modified(SystemTime::now() - STALE_AFTER - Duration::from_secs(5))
            .unwrap();
        assert!(corrupt.owns_caches_now(), "a corrupt record remains recoverable once stale");
        assert!(!corrupt.is_superseded());

        let brief_dir = tempfile::tempdir().unwrap();
        let incumbent = WorkspaceLease::claim(brief_dir.path());
        let brief = WorkspaceLease::claim(brief_dir.path());
        brief.release();
        assert!(incumbent.owns_caches_now(), "an unobserved brief claim leaves no terminal proof");
        assert!(!incumbent.is_superseded());
    }

    /// A live newer owner is NOT reclaimed: its record keeps a current heartbeat, so the
    /// superseded daemon stays read-only however often it checks.
    #[test]
    fn a_heartbeating_owner_is_never_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let path = lease_path(dir.path());
        let owner = LeaseRecord {
            generation: lease.generation().unwrap() + 1,
            token: new_token(),
            pid: 424242,
            heartbeat_secs: now_secs(),
            version: Some(PROGRAM_VERSION.to_owned()),
            workspace: None,
        };
        std::fs::write(&path, serde_json::to_string(&owner).unwrap()).unwrap();

        std::thread::sleep(VERDICT_TTL);
        assert!(!lease.owns_caches());
        std::thread::sleep(VERDICT_TTL);
        assert!(!lease.owns_caches(), "a live owner keeps the workspace");
        assert_eq!(record_at(&path).generation, owner.generation, "its record is untouched");
    }

    /// Clearing `.build` (a cache wipe) removes the record. A lone daemon must take the
    /// workspace back rather than read the absence as a demotion.
    #[test]
    fn a_wiped_record_is_reclaimed_by_the_live_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let path = lease_path(dir.path());
        std::fs::remove_file(&path).unwrap();

        std::thread::sleep(VERDICT_TTL);
        assert!(lease.owns_caches());
        assert_eq!(record_at(&path).generation, lease.generation().unwrap());
    }

    /// The wipe must not hand the workspace to BOTH daemons. Ownership is the record's identity,
    /// not a comparison of numbers: if the superseded daemon could restore its own lower
    /// generation, the newer one would read that as "below mine, so still mine" and the two
    /// would publish over each other — precisely the state the lease exists to prevent.
    #[test]
    fn a_wiped_record_never_leaves_two_daemons_owning_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let older = WorkspaceLease::claim(dir.path());
        let newer = WorkspaceLease::claim(dir.path());
        std::thread::sleep(VERDICT_TTL);
        assert!(!older.owns_caches() && newer.owns_caches(), "the newest claim owns it");

        std::fs::remove_file(lease_path(dir.path())).unwrap();
        std::thread::sleep(VERDICT_TTL);

        // Whoever gets there first takes it; the point is that the other one gives it up.
        let older_owns = older.owns_caches();
        let newer_owns = newer.owns_caches();
        assert!(
            older_owns != newer_owns,
            "exactly one daemon owns the workspace after the wipe (older={older_owns}, \
             newer={newer_owns})",
        );
        // And the loser's writes are refused at the fence, not merely by its cached verdict.
        let loser = if older_owns { &newer } else { &older };
        assert!(matches!(publish_test(loser, || "published"), LeaseOperationOutcome::Superseded));
    }
    /// The graph access lock is the operating system's, not a file's or a pid's: another
    /// process holding it keeps this one out, and that process dying — no clean exit, no
    /// cleanup — hands it over.
    #[test]
    fn the_access_lock_of_a_crashed_process_is_free_without_cleanup() {
        const CHILD: &str = "BSL_ACCESS_LOCK_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let _held = ExclusiveFileLock::try_acquire(Path::new(&path))
                .unwrap()
                .expect("the child takes a free lock");
            println!("held");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.access.lock");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_lease::tests::the_access_lock_of_a_crashed_process_is_free_without_cleanup",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(CHILD, &path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains("held") {
            line.clear();
            assert_ne!(std::io::BufRead::read_line(&mut reader, &mut line).unwrap(), 0);
        }
        assert!(ExclusiveFileLock::try_acquire(&path).unwrap().is_none(), "held by the child");

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(path.exists(), "nothing cleaned the file up");
        assert!(ExclusiveFileLock::try_acquire(&path).unwrap().is_some(), "free after the crash");
    }

    /// A record that will not read is not a free workspace: it is taken only once nobody has
    /// rewritten it for longer than a live owner's heartbeat allows.
    #[test]
    fn an_unreadable_record_is_taken_only_once_it_has_gone_stale() {
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());
        cache.ensure().unwrap();
        std::fs::write(cache.lease_path(), b"{ not a record").unwrap();

        let lease = WorkspaceLease::claim(dir.path());
        assert!(!lease.owns_caches_now(), "an owner that may be alive keeps the workspace");

        std::fs::OpenOptions::new()
            .write(true)
            .open(cache.lease_path())
            .unwrap()
            .set_modified(SystemTime::now() - STALE_AFTER - Duration::from_secs(5))
            .unwrap();
        assert!(lease.owns_caches_now(), "a record nobody has kept up is taken");
    }

    /// An observer that joins after a check could not answer hears so at once, instead of
    /// waiting for a finding that differs from one it never received.
    #[test]
    fn a_late_observer_hears_the_last_finding() {
        let dir = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease::claim(dir.path());
        let held = lease.hold_file_lock_for_test();
        std::fs::remove_file(lease_path(dir.path())).unwrap();
        assert!(!lease.owns_caches_now());
        drop(held);

        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        lease.observe(Arc::new(move |check| lock_recover(&sink).push(check)));
        assert_eq!(*lock_recover(&heard), vec![OwnershipCheck::Unknown]);
        lease.release();
        assert_eq!(*lock_recover(&heard), vec![OwnershipCheck::Unknown, OwnershipCheck::Lost]);
    }

    fn foreign_owner(root: &Path, version: Option<&str>, heartbeat_secs: u64) -> LeaseRecord {
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let record = LeaseRecord {
            generation: 3,
            token: new_token(),
            pid: 424242,
            heartbeat_secs,
            version: version.map(str::to_owned),
            workspace: None,
        };
        std::fs::write(lease_path(root), serde_json::to_string(&record).unwrap()).unwrap();
        record
    }

    /// A second live process of the same version does not take the workspace, whatever its
    /// key: the first keeps serving, and the second says who holds the directory.
    #[test]
    fn a_live_owner_of_the_same_version_keeps_the_workspace_until_it_stops_reporting() {
        let dir = tempfile::tempdir().unwrap();
        let owner = foreign_owner(dir.path(), Some(PROGRAM_VERSION), now_secs());

        let second = WorkspaceLease::claim(dir.path());
        assert!(!second.owns_caches_now());
        assert!(!second.is_superseded(), "refused, not superseded: it never owned anything");
        assert_eq!(
            second.busy_owner(),
            Some(BusyOwner {
                pid: 424242,
                version: Some(PROGRAM_VERSION.to_owned()),
                workspace: None,
            })
        );
        assert_eq!(record_at(&lease_path(dir.path())).token, owner.token, "the record stands");

        foreign_owner(dir.path(), Some(PROGRAM_VERSION), 0);
        assert!(second.owns_caches_now(), "a directory its owner left is taken without cleanup");
        assert_eq!(second.busy_owner(), None);
    }

    /// Only a newer program takes a live owner's workspace; a version this program cannot
    /// compare is not permission.
    #[test]
    fn only_an_older_live_owner_is_outbid() {
        for (theirs, taken) in
            [(Some("0.0.1"), true), (Some("999.0.0"), false), (Some("dev"), false), (None, false)]
        {
            let dir = tempfile::tempdir().unwrap();
            foreign_owner(dir.path(), theirs, now_secs());
            let lease = WorkspaceLease::claim(dir.path());
            assert_eq!(lease.owns_caches_now(), taken, "owner version {theirs:?}");
            assert_eq!(lease.busy_owner().is_some(), !taken, "owner version {theirs:?}");
        }
    }

    /// A lease handed out because no managed claim could be made at all says so.
    #[test]
    fn a_claim_that_cannot_be_made_is_marked_as_uncoordinated() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"file").unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::from_root(blocker.join("cache"));
        let lease = WorkspaceLease::claim_cache(&cache);
        assert!(lease.coordination_failed());
        assert!(!WorkspaceLease::unmanaged().coordination_failed());
    }
}
