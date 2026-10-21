//! Generation-fenced progress; snapshots never wait on indexing work.
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

static NEXT_PASS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPassState {
    Waiting,
    Running,
    Ready,
    Disabled,
    Failed,
    Cancelled,
    Superseded,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPhase {
    Initializing,
    Parsing,
    LexicalIndexing,
    Embedding,
    Persisting,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexCounters {
    pub total_files: usize,
    pub total_chunks: Option<usize>,
    pub total_batches: Option<usize>,
    pub done_chunks: usize,
    pub done_batches: usize,
}
#[derive(Debug, Clone)]
pub struct IndexProgressSnapshot {
    pub active: bool,
    pub state: IndexPassState,
    pub phase: Option<IndexPhase>,
    pub pass_id: Option<String>,
    pub counters: Option<IndexCounters>,
}
#[derive(Debug)]
struct Record {
    generation: u64,
    /// Whether the owner of `generation` has let go of the pass. A helper may record a
    /// terminal state while the owner keeps working, so liveness follows ownership, not state.
    released: bool,
    snapshot: IndexProgressSnapshot,
}
#[derive(Debug)]
pub struct IndexProgress {
    active: AtomicBool,
    record: Mutex<Record>,
}
impl Default for IndexProgress {
    fn default() -> Self {
        Self {
            active: AtomicBool::new(false),
            record: Mutex::new(Record {
                generation: 0,
                released: true,
                snapshot: IndexProgressSnapshot {
                    active: false,
                    state: IndexPassState::Waiting,
                    phase: None,
                    pass_id: None,
                    counters: None,
                },
            }),
        }
    }
}
impl IndexProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
    /// Suspend the current attempt's liveness during a retry backoff.
    pub fn pause_pass(self: &Arc<Self>) -> PausedPass {
        let mut record = self.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = record.generation;
        record.snapshot.active = false;
        self.active.store(false, Ordering::Relaxed);
        PausedPass { progress: Arc::clone(self), generation }
    }
    pub fn snapshot(&self) -> Option<IndexProgressSnapshot> {
        self.record.try_lock().ok().map(|record| record.snapshot.clone())
    }
    pub fn percent(&self) -> usize {
        self.snapshot()
            .and_then(|s| s.counters)
            .filter(|c| c.total_chunks.is_some_and(|t| t > 0 && c.done_chunks <= t))
            .map_or(0, |c| {
                ((c.done_chunks as u128 * 100) / c.total_chunks.unwrap() as u128) as usize
            })
    }
    pub fn reset(&self) {
        let mut record = self.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Consume a generation so callbacks from the reset attempt cannot publish again.
        record.generation = 0;
        record.released = true;
        record.snapshot = IndexProgressSnapshot {
            active: false,
            state: IndexPassState::Waiting,
            phase: None,
            pass_id: None,
            counters: None,
        };
        self.active.store(false, Ordering::Relaxed);
    }
    pub fn begin_pass(self: &Arc<Self>) -> ActivePass {
        self.begin_pass_with_counter(&NEXT_PASS)
    }

    fn begin_pass_with_counter(self: &Arc<Self>, sequence: &AtomicU64) -> ActivePass {
        // Formatting/process identity lookup occurs outside the bounded record lock.
        let prefix =
            blake3::hash(crate::lifecycle::process_id().as_bytes()).to_hex()[..32].to_owned();
        // Keep the checked update compatible with Rust 1.91; try_update needs 1.95.
        let mut current = sequence.load(Ordering::Relaxed);
        let generation = loop {
            let Some(next) = current.checked_add(1) else {
                break None;
            };
            match sequence.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break Some(next),
                Err(observed) => current = observed,
            }
        };
        let pass_id = generation.map(|n| format!("{prefix}:{n}"));
        let mut record = self.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(generation) = generation {
            record.generation = generation;
            record.released = false;
            record.snapshot = IndexProgressSnapshot {
                active: true,
                state: IndexPassState::Running,
                phase: Some(IndexPhase::Initializing),
                pass_id,
                counters: None,
            };
            self.active.store(true, Ordering::Relaxed);
        } else {
            record.snapshot = IndexProgressSnapshot {
                active: false,
                state: IndexPassState::Unknown,
                phase: None,
                pass_id: None,
                counters: None,
            };
            self.active.store(false, Ordering::Relaxed);
        }
        ActivePass {
            token: IndexPassToken { progress: Arc::clone(self), generation },
            finished: false,
        }
    }
}
/// Restores liveness only while the suspended attempt's owner still holds the pass.
pub struct PausedPass {
    progress: Arc<IndexProgress>,
    generation: u64,
}
impl Drop for PausedPass {
    fn drop(&mut self) {
        let mut record =
            self.progress.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if record.generation == self.generation && !record.released {
            record.snapshot.active = true;
            self.progress.active.store(true, Ordering::Relaxed);
        }
    }
}
#[derive(Debug, Clone)]
pub struct IndexPassToken {
    progress: Arc<IndexProgress>,
    generation: Option<u64>,
}
impl IndexPassToken {
    fn update(&self, update: impl FnOnce(&mut IndexProgressSnapshot)) {
        let mut record =
            self.progress.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.generation != Some(record.generation)
            || record.snapshot.state != IndexPassState::Running
        {
            return;
        }
        update(&mut record.snapshot);
    }
    fn release(&self) {
        let mut record =
            self.progress.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.generation == Some(record.generation) {
            record.released = true;
            record.snapshot.active = false;
            self.progress.active.store(false, Ordering::Relaxed);
        }
    }
    /// Whether this attempt still owns a running pass. Waits for the record instead of
    /// sampling it, so a concurrent status reader cannot make the answer false.
    pub fn is_running(&self) -> bool {
        let record = self.progress.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.generation == Some(record.generation)
            && record.snapshot.state == IndexPassState::Running
    }
    pub fn phase(&self, phase: IndexPhase) {
        self.update(|s| {
            s.phase = Some(phase);
            s.counters = None;
        });
    }
    pub fn set_totals(&self, files: usize, chunks: usize, batches: usize) {
        self.update(|s| {
            s.phase = Some(IndexPhase::Embedding);
            s.counters = Some(IndexCounters {
                total_files: files,
                total_chunks: Some(chunks),
                total_batches: Some(batches),
                done_chunks: 0,
                done_batches: 0,
            });
        });
    }
    /// Extend the batch total for a pass that plans its requests one owner at a time: the
    /// byte-bounded split of an owner is known only once its missing texts are.
    pub fn add_batches(&self, batches: usize) {
        self.update(|s| {
            if let Some(c) = s.counters.as_mut() {
                match c.total_batches.map(|total| total.checked_add(batches)) {
                    Some(Some(total)) => c.total_batches = Some(total),
                    Some(None) => s.counters = None,
                    None => {}
                }
            }
        });
    }
    pub fn advance(&self, chunks: usize, batches: usize) {
        self.update(|s| {
            if let Some(c) = s.counters.as_mut() {
                match (c.done_chunks.checked_add(chunks), c.done_batches.checked_add(batches)) {
                    (Some(chunks), Some(batches))
                        if c.total_chunks.is_none_or(|total| chunks <= total)
                            && c.total_batches.is_none_or(|total| batches <= total) =>
                    {
                        c.done_chunks = chunks;
                        c.done_batches = batches;
                    }
                    _ => s.counters = None,
                }
            }
        });
    }
    pub fn finish(&self, state: IndexPassState) {
        self.update(|s| {
            s.state = state;
            s.phase = None;
            s.counters = None;
        });
    }
}
pub struct ActivePass {
    token: IndexPassToken,
    finished: bool,
}
impl ActivePass {
    pub fn token(&self) -> IndexPassToken {
        self.token.clone()
    }
    pub fn finish(&mut self, state: IndexPassState) {
        self.token.finish(state);
        self.token.release();
        self.finished = true;
    }
}
impl Drop for ActivePass {
    fn drop(&mut self) {
        if !self.finished {
            self.token.finish(IndexPassState::Failed);
            self.token.release();
        }
    }
}

#[cfg(test)]
mod indexing_pass_lifecycle {
    use super::*;

    const HANG_WATCHDOG: std::time::Duration =
        test_utils::wait::hang_guard(std::time::Duration::from_secs(60));
    #[test]
    fn a_pause_restores_only_its_running_attempt() {
        let progress = IndexProgress::new();
        let mut first = progress.begin_pass();
        let paused = progress.pause_pass();
        assert!(!progress.is_active());
        drop(paused);
        assert!(progress.is_active());
        let stale_pause = progress.pause_pass();
        first.finish(IndexPassState::Ready);
        let mut next = progress.begin_pass();
        let next_pause = progress.pause_pass();
        drop(stale_pause);
        assert!(!progress.is_active());
        next.finish(IndexPassState::Ready);
        drop(next_pause);
        assert!(!progress.is_active());
    }
    #[test]
    fn a_pause_restores_liveness_of_a_pass_a_helper_already_failed() {
        // A helper records a terminal failure while the owning pass keeps embedding; the
        // broker must still see the pass alive after a retry backoff inside it.
        let progress = IndexProgress::new();
        let mut pass = progress.begin_pass();
        pass.token().finish(IndexPassState::Failed);
        drop(progress.pause_pass());
        assert!(progress.is_active());
        assert!(progress.snapshot().unwrap().active);
        pass.finish(IndexPassState::Ready);
        assert!(!progress.is_active());
        drop(progress.pause_pass());
        assert!(!progress.is_active());
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Failed);
    }
    #[test]
    #[ignore = "необязательный тест: попытка синхронного mutex wait не наблюдаема без временного окна; запуск: cargo test -p bsl-search --lib progress::indexing_pass_lifecycle::a_running_check_waits_out_a_concurrent_reader -- --exact --ignored"]
    fn a_running_check_waits_out_a_concurrent_reader() {
        let progress = IndexProgress::new();
        let mut pass = progress.begin_pass();
        let token = pass.token();
        let reader = progress.record.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let check = std::thread::spawn(move || {
            started_tx.send(()).expect("test observes the running check");
            result_tx.send(token.is_running()).expect("test receives running result");
        });
        started_rx.recv_timeout(HANG_WATCHDOG).expect("running check starts");
        assert!(progress.snapshot().is_none(), "a sample under contention is unavailable");
        drop(reader);
        assert!(result_rx.recv_timeout(HANG_WATCHDOG).expect("running check completes"));
        check.join().expect("running check thread exits");
        let stale = pass.token();
        pass.token().finish(IndexPassState::Failed);
        assert!(!stale.is_running());
        pass.finish(IndexPassState::Failed);
        let _next = progress.begin_pass();
        assert!(!stale.is_running());
    }
    #[test]
    fn stale_callback_and_drop_cannot_finish_new_pass() {
        let progress = IndexProgress::new();
        let old = progress.begin_pass();
        let stale = old.token();
        let mut new = progress.begin_pass();
        let current = new.token();
        current.set_totals(2, 4, 2);
        current.advance(2, 1);
        stale.finish(IndexPassState::Cancelled);
        drop(old);
        let sample = progress.snapshot().unwrap();
        assert_eq!(sample.state, IndexPassState::Running);
        assert_eq!(sample.counters.unwrap().done_chunks, 2);
        current.phase(IndexPhase::Persisting);
        assert!(progress.snapshot().unwrap().counters.is_none());
        new.finish(IndexPassState::Ready);
        assert!(!progress.is_active());
        let terminal = progress.snapshot().unwrap();
        assert_eq!(terminal.state, IndexPassState::Ready);
        assert_eq!(terminal.pass_id, sample.pass_id);
    }
    #[test]
    fn concurrent_pass_ids_are_unique_until_exhaustion() {
        let sequence = Arc::new(AtomicU64::new(u64::MAX - 4));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let mut starts = Vec::new();
        let mut workers = Vec::new();
        for _ in 0..8 {
            let sequence = Arc::clone(&sequence);
            let ready_tx = ready_tx.clone();
            let result_tx = result_tx.clone();
            let (start_tx, start_rx) = std::sync::mpsc::channel();
            starts.push(start_tx);
            workers.push(std::thread::spawn(move || {
                let progress = IndexProgress::new();
                ready_tx.send(()).expect("test receives worker readiness");
                start_rx.recv_timeout(HANG_WATCHDOG).expect("test releases pass-id worker");
                let _pass = progress.begin_pass_with_counter(&sequence);
                result_tx
                    .send(progress.snapshot().unwrap().pass_id)
                    .expect("test receives pass id");
            }));
        }
        drop(ready_tx);
        drop(result_tx);
        for _ in 0..8 {
            ready_rx.recv_timeout(HANG_WATCHDOG).expect("all pass-id workers are ready");
        }
        for start in starts {
            start.send(()).expect("pass-id worker waits for release");
        }
        let ids: std::collections::HashSet<_> = (0..8)
            .filter_map(|_| {
                result_rx.recv_timeout(HANG_WATCHDOG).expect("pass-id worker completes")
            })
            .collect();
        for worker in workers {
            worker.join().expect("pass-id worker exits");
        }
        assert_eq!(ids.len(), 4);
        assert_eq!(sequence.load(Ordering::Relaxed), u64::MAX);
    }
    #[test]
    fn contention_overflow_and_reset_are_fail_closed() {
        let progress = IndexProgress::new();
        let pass = progress.begin_pass();
        let token = pass.token();
        let lock = progress.record.lock().unwrap();
        assert!(progress.snapshot().is_none());
        drop(lock);
        progress.reset();
        token.finish(IndexPassState::Ready);
        drop(pass);
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Waiting);
        let sequence = AtomicU64::new(u64::MAX);
        let overflow = progress.begin_pass_with_counter(&sequence);
        let snapshot = progress.snapshot().unwrap();
        assert_eq!(snapshot.state, IndexPassState::Unknown);
        assert!(snapshot.pass_id.is_none());
        assert!(snapshot.counters.is_none());
        assert!(!progress.is_active());
        drop(overflow);
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Unknown);
    }
    #[test]
    fn terminal_failure_survives_cleanup() {
        let progress = IndexProgress::new();
        let pass = progress.begin_pass();
        pass.token().finish(IndexPassState::Superseded);
        assert!(progress.is_active());
        drop(pass);
        assert!(!progress.is_active());
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Superseded);
        let _pass = progress.begin_pass();
        assert_eq!(progress.snapshot().unwrap().state, IndexPassState::Running);
    }
    /// Without the extension, a byte-bounded split that outgrows the count-only estimate
    /// would make `advance` drop the counters as inconsistent.
    #[test]
    fn a_plan_made_per_owner_extends_the_batch_total() {
        let progress = IndexProgress::new();
        let mut pass = progress.begin_pass();
        let token = pass.token();
        let counters = || progress.snapshot().unwrap().counters.unwrap();
        token.set_totals(2, 5, 0);
        token.add_batches(2);
        token.advance(3, 2);
        token.add_batches(1);
        token.advance(2, 1);
        assert_eq!(counters().total_batches, Some(3));
        assert_eq!((counters().done_chunks, counters().done_batches), (5, 3));
        token.advance(0, 1);
        assert!(progress.snapshot().unwrap().counters.is_none(), "overrun drops counters");
        token.set_totals(1, 1, usize::MAX);
        token.add_batches(1);
        assert!(progress.snapshot().unwrap().counters.is_none(), "overflow drops counters");
        pass.finish(IndexPassState::Ready);
        token.add_batches(1);
        assert!(progress.snapshot().unwrap().counters.is_none());
    }
}
