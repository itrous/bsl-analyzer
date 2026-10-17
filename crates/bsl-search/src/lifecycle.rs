//! Bounded diagnostic observations. These are deliberately not a transaction log.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

mod startup;
pub use startup::{startup_snapshot, with_startup_roots};

pub const TARGET: &str = "bsl_vector_lifecycle";
pub const MAX_RECORD_BYTES: usize = 8192;
pub const SUMMARY_FILES: u64 = 128;
pub const MAX_EXAMPLES: usize = 10;
static PROCESS: OnceLock<String> = OnceLock::new();
static SEQUENCE: AtomicU64 = AtomicU64::new(1);
static OPERATION: AtomicU64 = AtomicU64::new(1);

/// The executable installs a random UUID before constructing any stores.
pub fn set_process_id(id: String) {
    let _ = PROCESS.set(bounded(&id, 64));
}

pub(crate) fn process_id() -> &'static str {
    PROCESS.get_or_init(|| format!("{}-{}", std::process::id(), now().as_nanos()))
}

fn now() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Unchanged,
    MissingRecord,
    HashChanged,
    HashCleared,
    HashLookupError,
    ReadError,
    ContextChanged,
    FileDeleted,
    SchemaReset,
    EmbedTextVersion,
    RootTransition,
    ModeTransition,
    ExplicitRebuild,
    ArtifactMissing,
    ArtifactStale,
    ArtifactInvalid,
    GenerationMissing,
    Startup,
    Embedding,
    Unknown,
}

pub fn hash_reason(old: Result<Option<&[u8]>, ()>, new: &[u8]) -> Reason {
    match old {
        Err(()) => Reason::HashLookupError,
        Ok(None) => Reason::MissingRecord,
        Ok(Some([])) => Reason::HashCleared,
        Ok(Some(old)) if old == new => Reason::Unchanged,
        Ok(Some(_)) => Reason::HashChanged,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Started,
    Progress,
    Committed,
    RolledBack,
    Cancelled,
    Refused,
    Failed,
    Unknown,
    NoOp,
    Completed,
    Interrupted,
    Skipped,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Counts {
    pub sqlite_vectors_removed: Option<u64>,
    pub overlay_vectors_removed: Option<u64>,
    pub overlay_cache_entries_removed: Option<u64>,
    pub hashes_cleared: u64,
    pub embeddings_written: u64,
}

impl Default for Counts {
    fn default() -> Self {
        Self {
            sqlite_vectors_removed: Some(0),
            overlay_vectors_removed: Some(0),
            overlay_cache_entries_removed: Some(0),
            hashes_cleared: 0,
            embeddings_written: 0,
        }
    }
}

impl Counts {
    fn add(&mut self, other: &Self) {
        fn sum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            a.zip(b).map(|(a, b)| a.saturating_add(b))
        }
        self.sqlite_vectors_removed =
            sum(self.sqlite_vectors_removed, other.sqlite_vectors_removed);
        self.overlay_vectors_removed =
            sum(self.overlay_vectors_removed, other.overlay_vectors_removed);
        self.overlay_cache_entries_removed =
            sum(self.overlay_cache_entries_removed, other.overlay_cache_entries_removed);
        self.hashes_cleared = self.hashes_cleared.saturating_add(other.hashes_cleared);
        self.embeddings_written = self.embeddings_written.saturating_add(other.embeddings_written);
    }

    pub fn unavailable() -> Self {
        Self {
            sqlite_vectors_removed: None,
            overlay_vectors_removed: None,
            overlay_cache_entries_removed: None,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub state: &'static str,
    pub files: Option<u64>,
    pub chunks: Option<u64>,
    pub vectors: Option<u64>,
    pub overlay_vectors: Option<u64>,
    pub overlay_cache_entries: Option<u64>,
    pub schema_version: Option<i64>,
    pub text_version: Option<i64>,
    pub generation: Option<i64>,
    pub file_identity: Option<String>,
    pub identity_stable: bool,
    pub build_version: Option<String>,
    pub build_sha: Option<String>,
    pub mode: Option<String>,
    pub roots: Vec<String>,
    pub roots_count: usize,
    pub roots_digest: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Record {
    pub event_version: u8,
    pub timestamp_ms: u128,
    pub process_id: String,
    pub pid: u32,
    pub event_seq: u64,
    pub operation_id: u64,
    pub parent_operation_id: Option<u64>,
    pub store_id: String,
    pub store_path: String,
    pub store_role: &'static str,
    pub kind: &'static str,
    pub reason: Reason,
    pub outcome: Outcome,
    pub count_quality: &'static str,
    pub counts: Counts,
    pub committed_totals: Option<Counts>,
    pub outcomes: BTreeMap<Outcome, u64>,
    pub dropped: u64,
    pub gap_reason: Option<&'static str>,
    pub files: u64,
    pub reasons: BTreeMap<Reason, u64>,
    pub examples: Vec<String>,
    pub snapshot: Option<Snapshot>,
    pub pending: Option<u64>,
    pub version_kind: Option<&'static str>,
    pub from_version: Option<i64>,
    pub to_version: Option<i64>,
    pub old_hash: Option<String>,
    pub new_hash: Option<String>,
    pub truncated: bool,
}

impl Record {
    pub fn new(path: &Path, kind: &'static str, reason: Reason) -> Self {
        // Normalize without opening the store or resolving symlinks on mutation paths.
        // Keep `..`: collapsing it would misidentify paths through a symlink.
        let normalized = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let path = normalized.as_path();
        let path_string = path.to_string_lossy();
        Self {
            event_version: 1,
            timestamp_ms: now().as_millis(),
            process_id: process_id().to_owned(),
            pid: std::process::id(),
            event_seq: 0,
            operation_id: OPERATION.fetch_add(1, Ordering::Relaxed),
            parent_operation_id: Context::current().map(|context| {
                context.0.lock().unwrap_or_else(|e| e.into_inner()).record.operation_id
            }),
            store_id: blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex().to_string(),
            store_path: bounded(&path_string, 512),
            store_role: match path.file_name().and_then(|name| name.to_str()) {
                Some("reference-search.db") => "reference",
                Some("baseline-sync.db" | "reference-baseline-sync.db") => "baseline",
                _ => "workspace",
            },
            kind,
            reason,
            outcome: Outcome::Started,
            count_quality: "exact",
            counts: Counts::default(),
            committed_totals: None,
            outcomes: BTreeMap::new(),
            dropped: 0,
            gap_reason: None,
            files: 0,
            reasons: BTreeMap::new(),
            examples: Vec::new(),
            snapshot: None,
            pending: None,
            version_kind: None,
            from_version: None,
            to_version: None,
            old_hash: None,
            new_hash: None,
            truncated: path_string.len() > 512,
        }
    }

    /// Encodes into a fixed buffer; JSON escaping cannot defeat the byte bound.
    pub fn encode(&mut self) -> Vec<u8> {
        for text in [&mut self.process_id, &mut self.store_path, &mut self.store_id] {
            if text.len() > 512 {
                *text = bounded(text, 512);
                self.truncated = true;
            }
        }
        for text in [&mut self.old_hash, &mut self.new_hash].into_iter().flatten() {
            if text.len() > 64 {
                *text = bounded(text, 64);
                self.truncated = true;
            }
        }
        if let Some(snapshot) = self.snapshot.as_mut() {
            for text in [
                &mut snapshot.file_identity,
                &mut snapshot.build_version,
                &mut snapshot.build_sha,
                &mut snapshot.mode,
                &mut snapshot.roots_digest,
            ]
            .into_iter()
            .flatten()
            {
                if text.len() > 256 {
                    *text = bounded(text, 256);
                    self.truncated = true;
                }
            }
        }
        self.timestamp_ms = now().as_millis();
        self.event_seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.count_quality =
            if std::iter::once(&self.counts).chain(self.committed_totals.as_ref()).any(|counts| {
                counts.sqlite_vectors_removed.is_none()
                    || counts.overlay_vectors_removed.is_none()
                    || counts.overlay_cache_entries_removed.is_none()
            }) {
                "unavailable"
            } else {
                "exact"
            };
        loop {
            let mut buffer = FixedBuffer { bytes: [0; MAX_RECORD_BYTES], len: 0 };
            if serde_json::to_writer(&mut buffer, &self).is_ok() && buffer.len < MAX_RECORD_BYTES {
                buffer.bytes[buffer.len] = b'\n';
                return buffer.bytes[..buffer.len + 1].to_vec();
            }
            self.truncated = true;
            if self.examples.pop().is_some() {
                continue;
            }
            if let Some(snapshot) = self.snapshot.as_mut() {
                if snapshot.roots.pop().is_some() {
                    continue;
                }
            }
            // Once examples/roots are gone, bound every remaining caller-supplied string.
            // Dropping the optional snapshot also bounds JSON escaping in its metadata.
            self.snapshot = None;
            self.store_path = bounded(&self.store_path, 64);
            self.store_id = bounded(&self.store_id, 64);
            self.process_id = bounded(&self.process_id, 64);
            self.kind = "truncated_record";
            self.store_role = "unknown";
            self.gap_reason = None;
            self.version_kind = None;
            self.old_hash = None;
            self.new_hash = None;
        }
    }

    /// A started record for the next interval: none of the interval's own tallies repeat in it.
    fn intent(&self) -> Self {
        let mut intent = self.clone();
        intent.outcome = Outcome::Started;
        intent.counts = Counts::default();
        intent.files = 0;
        intent.outcomes.clear();
        intent.reasons.clear();
        intent.examples.clear();
        intent
    }

    pub fn emit(&mut self, debug: bool) {
        if debug {
            if tracing::enabled!(target: "bsl_vector_lifecycle", tracing::Level::DEBUG) {
                let bytes = self.encode();
                let text = String::from_utf8_lossy(&bytes);
                tracing::debug!(target: "bsl_vector_lifecycle", record = text.trim_end_matches('\n'));
            }
        } else if tracing::enabled!(target: "bsl_vector_lifecycle", tracing::Level::INFO) {
            let bytes = self.encode();
            let text = String::from_utf8_lossy(&bytes);
            tracing::info!(target: "bsl_vector_lifecycle", record = text.trim_end_matches('\n'));
            if self.outcome == Outcome::Progress {
                self.intent().emit(false);
            }
        }
    }
}

pub fn journal_gap_record(dropped: u64, reason: &'static str) -> Vec<u8> {
    let mut record = Record::new(Path::new("journal"), "journal_gap", Reason::Unknown);
    record.store_role = "journal";
    record.outcome = Outcome::Unknown;
    record.dropped = dropped;
    record.gap_reason = Some(match reason {
        "queue_overflow" | "partial_record" | "corrupt_segment" | "write_failure" => reason,
        _ => "unknown",
    });
    record.encode()
}

struct FixedBuffer {
    bytes: [u8; MAX_RECORD_BYTES],
    len: usize,
}
impl Write for FixedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.bytes.len() - self.len {
            return Err(io::ErrorKind::WriteZero.into());
        }
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn bounded(text: &str, bytes: usize) -> String {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Captured explicitly before dispatching work; entering is only for synchronous scopes.
#[derive(Clone)]
pub struct Context(Arc<Mutex<Summary>>, tracing::Dispatch, Option<Reason>);
thread_local! {
    static CURRENT: RefCell<Option<Context>> = const { RefCell::new(None) };
    static DECISION: std::cell::Cell<Option<Reason>> = const { std::cell::Cell::new(None) };
}

pub fn with_reason<T>(reason: Reason, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Reason>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DECISION.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(DECISION.with(|slot| slot.replace(Some(reason))));
    f()
}

impl Context {
    pub fn current() -> Option<Self> {
        CURRENT.with(|slot| slot.borrow().clone()).map(|mut context| {
            context.2 = DECISION.with(|slot| slot.get());
            context
        })
    }
    pub fn in_scope<T>(&self, f: impl FnOnce() -> T) -> T {
        struct Restore(Option<Context>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(CURRENT.with(|slot| slot.replace(Some(self.clone()))));
        tracing::dispatcher::with_default(&self.1, || match self.2 {
            Some(reason) => with_reason(reason, f),
            None => f(),
        })
    }
}

struct Summary {
    record: Record,
    totals: Counts,
    since: Instant,
}
impl Summary {
    fn flush(&mut self, outcome: Outcome) -> Record {
        let mut event = self.record.clone();
        event.committed_totals = Some(self.totals.clone());
        if event.reason == Reason::Embedding {
            event.pending =
                event.pending.map(|count| count.saturating_sub(self.totals.embeddings_written));
        }
        event.outcome = outcome;
        self.record.files = 0;
        self.record.counts = Counts::default();
        self.record.outcomes.clear();
        self.record.reasons.clear();
        self.record.examples.clear();
        self.since = Instant::now();
        event
    }
}

/// One producer operation, aggregating its child transactions instead of INFO per file.
pub struct Batch {
    context: Context,
    finished: bool,
}
impl Batch {
    pub fn new(path: &Path, reason: Reason) -> Self {
        let mut record = Record::new(
            path,
            if reason == Reason::Embedding { "embedding_pass" } else { "mutation_summary" },
            reason,
        );
        record.emit(false);
        Self {
            context: Context(
                Arc::new(Mutex::new(Summary {
                    record,
                    totals: Counts::default(),
                    since: Instant::now(),
                })),
                tracing::dispatcher::get_default(Clone::clone),
                None,
            ),
            finished: false,
        }
    }
    pub fn context(&self) -> Context {
        self.context.clone()
    }
    pub fn pending(&self, count: u64) {
        let mut record = {
            let mut summary = self.context.0.lock().unwrap_or_else(|e| e.into_inner());
            summary.record.pending = Some(count);
            summary.record.intent()
        };
        record.emit(false);
    }
    pub fn finish(mut self, outcome: Outcome) {
        self.finish_inner(outcome);
    }
    fn finish_inner(&mut self, outcome: Outcome) {
        let mut record = self.context.0.lock().unwrap_or_else(|e| e.into_inner()).flush(outcome);
        record.emit(false);
        self.finished = true;
    }
}

/// Only the hashes and identity already read by the decision owner are observed.
pub fn decision(
    path: &Path,
    key: &crate::FileKey,
    reason: Reason,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
) {
    let parent = Context::current();
    let mut record = Record::new(path, "decision", reason);
    record.outcome = Outcome::NoOp;
    record.examples.push(bounded(&format!("{}:{}", key.root_id, key.path), 256));
    let debug = tracing::enabled!(target: "bsl_vector_lifecycle", tracing::Level::DEBUG);
    if debug {
        fn hex(bytes: &[u8]) -> String {
            bytes.iter().take(32).map(|b| format!("{b:02x}")).collect()
        }
        record.old_hash = old.map(hex);
        record.new_hash = new.map(hex);
    }
    let event = parent.as_ref().and_then(|parent| {
        let mut summary = parent.0.lock().unwrap_or_else(|e| e.into_inner());
        record.parent_operation_id = Some(summary.record.operation_id);
        summary.record.files += 1;
        *summary.record.reasons.entry(reason).or_default() += 1;
        if summary.record.examples.len() < MAX_EXAMPLES {
            summary.record.examples.push(record.examples[0].clone());
        }
        if summary.record.files >= SUMMARY_FILES
            || summary.since.elapsed() >= Duration::from_secs(1)
        {
            Some(summary.flush(Outcome::Progress))
        } else {
            None
        }
    });
    record.emit(parent.is_some());
    if let Some(mut event) = event {
        event.emit(false);
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        if !self.finished {
            self.finish_inner(Outcome::Interrupted);
        }
    }
}

/// Observation owned by the existing transaction, never by a wrapper guessing its outcome.
pub struct Mutation {
    pub record: Record,
    parent: Option<Context>,
    observed_decision: bool,
    finished: bool,
}
impl Mutation {
    pub fn new(path: &Path, reason: Reason) -> Self {
        let reason =
            if matches!(reason, Reason::Unknown | Reason::HashChanged | Reason::FileDeleted) {
                DECISION.with(|slot| slot.get()).unwrap_or(reason)
            } else {
                reason
            };
        let parent = Context::current();
        let mut record = Record::new(path, "mutation", reason);
        if let Some(parent) = &parent {
            record.parent_operation_id =
                Some(parent.0.lock().unwrap_or_else(|e| e.into_inner()).record.operation_id);
        }
        record.emit(parent.is_some());
        Self {
            record,
            parent,
            observed_decision: DECISION.with(|slot| slot.get().is_some()),
            finished: false,
        }
    }
    pub fn finish(mut self, outcome: Outcome, counts: Counts) {
        self.finish_inner(outcome, counts);
    }
    fn finish_inner(&mut self, outcome: Outcome, counts: Counts) {
        self.record.outcome = outcome;
        self.record.counts = counts;
        self.record.emit(self.parent.is_some());
        if let Some(parent) = &self.parent {
            let event = {
                let mut summary = parent.0.lock().unwrap_or_else(|e| e.into_inner());
                if outcome == Outcome::Committed {
                    summary.record.counts.add(&self.record.counts);
                    summary.totals.add(&self.record.counts);
                }
                *summary.record.outcomes.entry(outcome).or_default() += 1;
                if !self.observed_decision {
                    summary.record.files += self.record.files.max(1);
                    *summary.record.reasons.entry(self.record.reason).or_default() += 1;
                }
                for example in &self.record.examples {
                    if summary.record.examples.len() < MAX_EXAMPLES {
                        summary.record.examples.push(bounded(example, 256));
                    }
                }
                if summary.record.files >= SUMMARY_FILES
                    || summary.since.elapsed() >= Duration::from_secs(1)
                {
                    Some(summary.flush(Outcome::Progress))
                } else {
                    None
                }
            };
            if let Some(mut event) = event {
                event.emit(false);
            }
        }
        self.finished = true;
    }
}
impl Drop for Mutation {
    fn drop(&mut self) {
        if !self.finished {
            self.finish_inner(Outcome::Unknown, Counts::unavailable());
        }
    }
}

#[cfg(test)]
pub(crate) use test_utils::with_subscriber as test_with_subscriber;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_decisions_and_json_bound() {
        assert_eq!(hash_reason(Err(()), b"x"), Reason::HashLookupError);
        assert_eq!(hash_reason(Ok(None), b"x"), Reason::MissingRecord);
        assert_eq!(hash_reason(Ok(Some(b"")), b"x"), Reason::HashCleared);
        assert_eq!(hash_reason(Ok(Some(b"x")), b"x"), Reason::Unchanged);
        assert_eq!(hash_reason(Ok(Some(b"y")), b"x"), Reason::HashChanged);
        let mut record = Record::new(Path::new("db"), "mutation", Reason::ContextChanged);
        record.examples = vec!["\u{0}💡".repeat(1000); 10];
        let bytes = record.encode();
        assert!(bytes.len() <= MAX_RECORD_BYTES);
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["truncated"], true);
        assert_eq!(json["counts"]["sqlite_vectors_removed"], 0);
        record.store_id = "\u{0}".repeat(512);
        record.snapshot = Some(Snapshot {
            build_version: Some("\u{0}".repeat(256)),
            build_sha: Some("\u{0}".repeat(256)),
            mode: Some("\u{0}".repeat(256)),
            roots_digest: Some("\u{0}".repeat(256)),
            ..Snapshot::default()
        });
        assert!(record.encode().len() <= MAX_RECORD_BYTES);
    }

    #[test]
    fn store_identity_normalizes_relative_paths_and_distinguishes_owned_roles() {
        let relative = Record::new(Path::new("./search.db"), "startup_snapshot", Reason::Startup);
        let absolute = Record::new(
            &std::env::current_dir().unwrap().join("search.db"),
            "startup_snapshot",
            Reason::Startup,
        );
        assert_eq!(relative.store_id, absolute.store_id);
        assert_eq!(relative.store_path, absolute.store_path);
        for (name, role) in [
            ("search.db", "workspace"),
            ("reference-search.db", "reference"),
            ("baseline-sync.db", "baseline"),
            ("reference-baseline-sync.db", "baseline"),
        ] {
            assert_eq!(
                Record::new(Path::new(name), "startup_snapshot", Reason::Startup).store_role,
                role
            );
        }
    }

    #[test]
    fn aggregation_and_explicit_worker_context() {
        let records = capture_with(|seen| {
            let batch = Batch::new(Path::new("db"), Reason::ExplicitRebuild);
            let context = batch.context();
            let worker = context.clone();
            std::thread::spawn(move || {
                worker.in_scope(|| {
                    for _ in 0..127 {
                        let mut mutation = Mutation::new(Path::new("db"), Reason::HashChanged);
                        mutation.record.examples.push("file.bsl".into());
                        mutation.finish(
                            Outcome::Committed,
                            Counts { sqlite_vectors_removed: Some(2), ..Counts::default() },
                        );
                    }
                })
            })
            .join()
            .unwrap();
            let state = context.0.lock().unwrap();
            assert_eq!(state.totals.sqlite_vectors_removed, Some(254));
            assert_eq!(progress_files(&seen.lock().unwrap()) + state.record.files, 127);
            if cfg!(timing_contract) {
                assert_eq!(state.record.files, 127);
                assert_eq!(state.record.examples.len(), 10);
                assert_eq!(state.record.counts.sqlite_vectors_removed, Some(254));
            }
            drop(state);
            context.in_scope(|| {
                Mutation::new(Path::new("db"), Reason::HashChanged)
                    .finish(Outcome::RolledBack, Counts::default())
            });
            let files = context.0.lock().unwrap().record.files;
            assert_eq!(progress_files(&seen.lock().unwrap()) + files, 128);
            if cfg!(timing_contract) {
                assert_eq!(files, 0);
            }
            assert!(Context::current().is_none());
            batch.finish(Outcome::Cancelled);
        });
        assert_summary_invariant(&records, 0, 128);
    }

    #[test]
    fn subscriber_gets_bounded_summaries_and_terminal_committed_totals() {
        let records = capture(|| {
            let batch = Batch::new(Path::new("db"), Reason::ExplicitRebuild);
            let context = batch.context();
            std::thread::spawn(move || {
                context.in_scope(|| {
                    for _ in 0..10_000 {
                        let mut mutation = Mutation::new(Path::new("db"), Reason::HashChanged);
                        mutation.record.examples.push("module.bsl".into());
                        mutation.finish(
                            Outcome::Committed,
                            Counts { sqlite_vectors_removed: Some(1), ..Counts::default() },
                        );
                    }
                })
            })
            .join()
            .unwrap();
            batch.finish(Outcome::Cancelled);
        });
        assert_summary_invariant(&records, 0, 10_000);
        if cfg!(timing_contract) {
            assert_eq!(records.len(), 1 + 2 * (10_000 / 128) + 1);
        }
        let terminal = records.last().unwrap();
        assert_eq!(terminal["committed_totals"]["sqlite_vectors_removed"], 10_000);
        assert_eq!(terminal["outcome"], "cancelled");
        assert!(records.iter().all(|r| r["examples"].as_array().unwrap().len() <= 10));
        assert!(records.iter().all(|r| r["operation_id"] == terminal["operation_id"]));
        // Like `files` and `counts`, `outcomes` covers only its own summary interval, so the
        // summaries of one operation add up to the mutations it ran.
        let summaries = records.iter().filter(|r| r["outcome"] != "started");
        let (files, committed) = summaries.fold((0, 0), |(files, committed), r| {
            assert_eq!(r["outcomes"]["committed"].as_u64().unwrap_or(0), r["files"]);
            (
                files + r["files"].as_u64().unwrap(),
                committed + r["outcomes"]["committed"].as_u64().unwrap_or(0),
            )
        });
        assert_eq!((files, committed), (10_000, 10_000));
    }

    #[test]
    fn summary_invariant_survives_an_expired_interval() {
        check_summary_after_interval_expiry(|context| {
            context.0.lock().unwrap().since = Instant::now() - Duration::from_millis(1100);
        });
    }

    #[test]
    #[ignore = "необязательный тест: таймер std::time::Instant после реальной паузы; запуск: cargo test -p bsl-search lifecycle::tests::summary_invariant_survives_a_real_timer_pause -- --exact --ignored"]
    fn summary_invariant_survives_a_real_timer_pause() {
        check_summary_after_interval_expiry(|_| {
            std::thread::sleep(Duration::from_millis(1100));
        });
    }

    fn check_summary_after_interval_expiry(expire: impl FnOnce(&Context)) {
        let mutations = SUMMARY_FILES + 2;
        let records = capture(|| {
            let batch = Batch::new(Path::new("db"), Reason::ExplicitRebuild);
            let context = batch.context();
            context.in_scope(|| {
                let commit = || {
                    let mut mutation = Mutation::new(Path::new("db"), Reason::HashChanged);
                    mutation.record.examples.push("module.bsl".into());
                    mutation.finish(
                        Outcome::Committed,
                        Counts { sqlite_vectors_removed: Some(2), ..Counts::default() },
                    );
                };
                commit();
                batch.pending(mutations - 1);
                expire(&context);
                commit();
                for _ in 2..mutations {
                    commit();
                }
            });
            batch.finish(Outcome::Completed);
        });
        assert_summary_invariant(&records, 1, mutations);
        // An early timer flush and the remaining full size interval both emit progress.
        let progress = records.iter().filter(|r| r["outcome"] == "progress").count();
        assert!(progress >= 2, "{records:?}");
        let old_size_only_count = 1 + 1 + 2 * (mutations / SUMMARY_FILES) + 1;
        assert_ne!(records.len() as u64, old_size_only_count);
        let terminal = records.last().unwrap();
        assert_eq!(terminal["outcome"], "completed");
        assert_eq!(terminal["committed_totals"]["sqlite_vectors_removed"], 2 * mutations);
    }

    #[test]
    fn unavailable_totals_keep_their_quality_after_flush_and_at_termination() {
        let records = capture(|| {
            let batch = Batch::new(Path::new("db"), Reason::ExplicitRebuild);
            batch.context().in_scope(|| {
                Mutation::new(Path::new("db"), Reason::HashChanged)
                    .finish(Outcome::Committed, Counts::unavailable());
                for _ in 1..SUMMARY_FILES {
                    Mutation::new(Path::new("db"), Reason::HashChanged)
                        .finish(Outcome::Committed, Counts::default());
                }
            });
            batch.finish(Outcome::Cancelled);
        });
        assert_summary_invariant(&records, 0, SUMMARY_FILES);
        assert_eq!(records[0]["outcome"], "started");
        for (record, next) in records.iter().zip(&records[1..]) {
            if record["outcome"] == "progress" {
                assert_eq!(next["outcome"], "started", "{next}");
            }
        }
        for record in &records[1..] {
            assert!(record["committed_totals"]["sqlite_vectors_removed"].is_null());
            assert_eq!(record["count_quality"], "unavailable");
        }
        let terminal = records.last().unwrap();
        assert_eq!(terminal["outcome"], "cancelled");
        assert_eq!(terminal["counts"]["sqlite_vectors_removed"], 0);
        if cfg!(timing_contract) {
            assert_eq!(records.len(), 4); // Initial intent, progress, next intent, terminal.
            assert_eq!(records[2]["outcome"], "started");
            assert_eq!(records[3]["outcome"], "cancelled");
        }
    }

    #[test]
    fn started_intents_carry_no_interval_data() {
        let records = capture(|| {
            let batch = Batch::new(Path::new("db"), Reason::ExplicitRebuild);
            batch.context().in_scope(|| {
                // Pending arrives mid-interval here; its intent must not repeat that interval.
                Mutation::new(Path::new("db"), Reason::HashChanged)
                    .finish(Outcome::Committed, Counts::default());
                batch.pending(7);
                for _ in 1..SUMMARY_FILES {
                    Mutation::new(Path::new("db"), Reason::HashChanged)
                        .finish(Outcome::Committed, Counts::default());
                }
            });
            batch.finish(Outcome::Completed);
        });
        assert_summary_invariant(&records, 1, SUMMARY_FILES);
        let started: Vec<_> = records.iter().filter(|r| r["outcome"] == "started").collect();
        if cfg!(timing_contract) {
            assert_eq!(started.len(), 3); // Initial, pending, after the progress summary.
        }
        for record in started {
            assert_eq!(record["files"], 0, "{record}");
            assert!(record["outcomes"].as_object().unwrap().is_empty(), "{record}");
            assert!(record["reasons"].as_object().unwrap().is_empty(), "{record}");
            assert!(record["examples"].as_array().unwrap().is_empty(), "{record}");
        }
    }

    #[test]
    fn scoped_capture_survives_first_callsite_on_unsubscribed_thread() {
        const CHILD: &str = "BSL_LIFECYCLE_CAPTURE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "lifecycle::tests::scoped_capture_survives_first_callsite_on_unsubscribed_thread"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            // A drifted name would select no test and still exit successfully.
            assert!(output.status.success() && stdout.contains("1 passed"), "{stdout}");
            return;
        }
        let records = capture(|| {
            std::thread::spawn(|| {
                Record::new(Path::new("fixture.db"), "capture_probe", Reason::Startup).emit(false);
            })
            .join()
            .unwrap();
            Record::new(Path::new("fixture.db"), "capture_probe", Reason::Startup).emit(false);
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["kind"], "capture_probe");
    }

    /// Holds however often the one-second summary timer fires, unlike exact record counts,
    /// which assume that only the size limit flushes a summary.
    fn assert_summary_invariant(
        records: &[serde_json::Value],
        pending_calls: usize,
        mutations: u64,
    ) {
        let started: Vec<_> = records.iter().filter(|r| r["outcome"] == "started").collect();
        let progress = records.iter().filter(|r| r["outcome"] == "progress").count();
        assert_eq!(started.len(), 1 + pending_calls + progress, "{records:?}");
        for record in records {
            assert!(record["files"].as_u64().unwrap() <= SUMMARY_FILES, "{record}");
        }
        let files: u64 = records
            .iter()
            .filter(|r| r["outcome"] != "started")
            .map(|r| r["files"].as_u64().unwrap())
            .sum();
        assert_eq!(files, mutations, "{records:?}");
        assert!(progress as u64 + 1 >= mutations.div_ceil(SUMMARY_FILES), "{records:?}");
        for record in started {
            assert_eq!(record["files"], 0, "{record}");
            assert_eq!(
                record["counts"],
                serde_json::to_value(Counts::default()).unwrap(),
                "{record}"
            );
            assert!(record["outcomes"].as_object().unwrap().is_empty(), "{record}");
            assert!(record["reasons"].as_object().unwrap().is_empty(), "{record}");
            assert!(record["examples"].as_array().unwrap().is_empty(), "{record}");
        }
    }

    fn progress_files(records: &[serde_json::Value]) -> u64 {
        records
            .iter()
            .filter(|r| r["outcome"] == "progress")
            .map(|r| r["files"].as_u64().unwrap())
            .sum()
    }

    fn capture(f: impl FnOnce()) -> Vec<serde_json::Value> {
        capture_with(|_| f())
    }

    /// Like `capture`, but `f` can read the records emitted so far.
    fn capture_with(f: impl FnOnce(&Mutex<Vec<serde_json::Value>>)) -> Vec<serde_json::Value> {
        use tracing_subscriber::prelude::*;
        struct Capture(Arc<Mutex<Vec<serde_json::Value>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct Visitor<'a>(&'a mut Vec<serde_json::Value>);
                impl tracing::field::Visit for Visitor<'_> {
                    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {
                    }
                    fn record_str(&mut self, field: &tracing::field::Field, text: &str) {
                        if field.name() == "record" {
                            self.0.push(serde_json::from_str(text).unwrap());
                        }
                    }
                }
                if event.metadata().target() == TARGET {
                    event.record(&mut Visitor(&mut self.0.lock().unwrap()));
                }
            }
        }
        let records = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(
            Capture(records.clone()).with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        );
        test_with_subscriber(subscriber, || f(&records));
        Arc::try_unwrap(records).unwrap().into_inner().unwrap()
    }
}
