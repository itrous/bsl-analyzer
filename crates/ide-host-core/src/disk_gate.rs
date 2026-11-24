//! Admission for disk-backed workspace reads and a filesystem transaction.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// One owner's admission point, shared by its requests and background disk readers.
#[derive(Clone, Default)]
pub struct WorkspaceDiskGate(Arc<Gate>);

#[derive(Default)]
struct Gate {
    state: Mutex<State>,
    drained: Condvar,
    participant_violation: Arc<AtomicBool>,
}

type MutationFilter = dyn Fn(&Path, Option<[u8; 32]>) -> bool + Send + Sync;

#[derive(Default)]
struct State {
    readers: BTreeMap<u64, Vec<salsa::CancellationToken>>,
    next_reader: u64,
    epoch: u64,
    writer: bool,
    recovery: bool,
    observation: Option<Arc<()>>,
    mutation_filter: Option<Arc<MutationFilter>>,
}

/// A reader refused while a filesystem transaction or its recovery owns the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceDiskBusy;

impl fmt::Display for WorkspaceDiskBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("workspace disk transaction is in progress or requires recovery")
    }
}

impl std::error::Error for WorkspaceDiskBusy {}

/// Why an exclusive filesystem transaction could not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDiskWriteError {
    Busy,
    RecoveryRequired,
    Cancelled,
    TimedOut,
}

impl fmt::Display for WorkspaceDiskWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "another workspace disk transaction is running",
            Self::RecoveryRequired => "workspace disk transaction requires recovery",
            Self::Cancelled => "workspace disk transaction admission was cancelled",
            Self::TimedOut => "workspace disk readers did not finish within the admission budget",
        })
    }
}

impl std::error::Error for WorkspaceDiskWriteError {}

impl fmt::Debug for WorkspaceDiskGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state();
        f.debug_struct("WorkspaceDiskGate")
            .field("readers", &state.readers.len())
            .field("epoch", &state.epoch)
            .field("writer", &state.writer)
            .field("observing", &state.observation.is_some())
            .field("recovery", &state.recovery)
            .finish()
    }
}

impl WorkspaceDiskGate {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Admit a complete disk-reading phase; keep the ticket until all its work has stopped.
    pub fn try_read(&self) -> Result<WorkspaceDiskRead, WorkspaceDiskBusy> {
        let mut state = self.state();
        if state.writer || state.recovery {
            return Err(WorkspaceDiskBusy);
        }
        let id = state.next_reader;
        state.next_reader = state.next_reader.wrapping_add(1);
        state.readers.insert(id, Vec::new());
        Ok(WorkspaceDiskRead(Arc::new(Read { gate: self.clone(), id, epoch: state.epoch })))
    }

    /// Wait for a paused background reader without losing its pending work or ignoring shutdown.
    pub fn read_until_cancelled(&self, cancelled: &dyn Fn() -> bool) -> Option<WorkspaceDiskRead> {
        loop {
            if cancelled() {
                return None;
            }
            if let Ok(read) = self.try_read() {
                return Some(read);
            }
            let state = self.state();
            if state.writer || state.recovery {
                drop(self.0.drained.wait_timeout(state, Duration::from_millis(10)));
            }
        }
    }

    /// Close admission, cancel existing request tokens, and wait for their owned tickets.
    pub fn begin_write(
        &self,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<WorkspaceDiskWrite, WorkspaceDiskWriteError> {
        self.begin(timeout, cancelled, false, None, None)
    }

    /// Admit an explicitly authorized recovery without bypassing active-reader draining.
    pub fn begin_recovery(
        &self,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<WorkspaceDiskWrite, WorkspaceDiskWriteError> {
        self.begin(timeout, cancelled, true, None, None)
    }

    /// Reserve one commit's mutation scope while its snapshot is prepared and reads remain open.
    /// The filter runs under the admission lock and must not call back into this gate.
    pub fn observe_document_changes(
        &self,
        filter: impl Fn(&Path, Option<[u8; 32]>) -> bool + Send + Sync + 'static,
    ) -> Result<WorkspaceDiskObservation, WorkspaceDiskWriteError> {
        let mut state = self.state();
        if state.writer || state.observation.is_some() {
            return Err(WorkspaceDiskWriteError::Busy);
        }
        if state.recovery {
            return Err(WorkspaceDiskWriteError::RecoveryRequired);
        }
        let owner = Arc::new(());
        state.observation = Some(Arc::clone(&owner));
        state.mutation_filter = Some(Arc::new(filter));
        self.0.participant_violation.store(false, Ordering::Release);
        Ok(WorkspaceDiskObservation { gate: self.clone(), owner })
    }

    /// Install the writer's mutation scope atomically with closing read admission.
    /// The filter runs under the admission lock and must not call back into this gate.
    pub fn begin_write_observing(
        &self,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
        filter: impl Fn(&Path, Option<[u8; 32]>) -> bool + Send + Sync + 'static,
    ) -> Result<WorkspaceDiskWrite, WorkspaceDiskWriteError> {
        self.begin(timeout, cancelled, false, Some(Arc::new(filter)), None)
    }

    /// Authorize recovery with its bound mutation scope before draining readers.
    /// The filter runs under the admission lock and must not call back into this gate.
    pub fn begin_recovery_observing(
        &self,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
        filter: impl Fn(&Path, Option<[u8; 32]>) -> bool + Send + Sync + 'static,
    ) -> Result<WorkspaceDiskWrite, WorkspaceDiskWriteError> {
        self.begin(timeout, cancelled, true, Some(Arc::new(filter)), None)
    }

    fn begin(
        &self,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
        recovery: bool,
        filter: Option<Arc<MutationFilter>>,
        observation: Option<&Arc<()>>,
    ) -> Result<WorkspaceDiskWrite, WorkspaceDiskWriteError> {
        if cancelled() {
            return Err(WorkspaceDiskWriteError::Cancelled);
        }
        let mut state = self.state();
        let owns_observation = match (&state.observation, observation) {
            (Some(current), Some(owner)) => Arc::ptr_eq(current, owner),
            (None, None) => true,
            _ => false,
        };
        if state.writer || !owns_observation {
            return Err(WorkspaceDiskWriteError::Busy);
        }
        if state.recovery && !recovery {
            return Err(WorkspaceDiskWriteError::RecoveryRequired);
        }
        state.writer = true;
        if filter.is_some() || (observation.is_none() && !state.recovery) {
            state.mutation_filter = filter;
        }
        if observation.is_none() {
            self.0.participant_violation.store(false, Ordering::Release);
        }
        state.epoch = state.epoch.wrapping_add(1);
        for token in state.readers.values().flatten() {
            token.cancel();
        }
        let writer = WorkspaceDiskWrite {
            gate: self.clone(),
            writing: false,
            finished: false,
            released: false,
        };
        let started = Instant::now();
        loop {
            if cancelled() {
                drop(state);
                return Err(WorkspaceDiskWriteError::Cancelled);
            }
            if state.readers.is_empty() {
                return Ok(writer);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                drop(state);
                return Err(WorkspaceDiskWriteError::TimedOut);
            }
            let waited =
                self.0.drained.wait_timeout(state, remaining.min(Duration::from_millis(10)));
            state = waited.unwrap_or_else(|poisoned| poisoned.into_inner()).0;
        }
    }

    /// Whether a preparation still belongs to the readable disk epoch that produced it.
    pub fn is_current(&self, epoch: u64) -> bool {
        let state = self.state();
        !state.writer && !state.recovery && state.epoch == epoch
    }

    /// The transaction epoch, including while disk read admission is closed.
    pub fn epoch(&self) -> u64 {
        self.state().epoch
    }

    /// Whether requests must refrain from reading the workspace disk.
    pub fn is_frozen(&self) -> bool {
        let state = self.state();
        state.writer || state.recovery
    }

    /// Whether an interrupted source mutation still needs explicit recovery.
    pub fn recovery_required(&self) -> bool {
        self.state().recovery
    }

    /// The applying journal observes mutations reported by every participating editor session.
    pub fn participant_violation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0.participant_violation)
    }

    /// Classify a mutation against the accepted commit's scope, including before writer admission.
    pub fn report_document_change(&self, path: &Path, editor_hash: Option<[u8; 32]>) -> bool {
        let state = self.state();
        if !state.writer && state.observation.is_none() {
            return false;
        }
        if state.mutation_filter.as_ref().is_some_and(|filter| !filter(path, editor_hash)) {
            return false;
        }
        self.0.participant_violation.store(true, Ordering::Release);
        true
    }
}

/// One accepted commit's mutation observation; dropping before handoff leaves reads open.
#[derive(Debug)]
pub struct WorkspaceDiskObservation {
    gate: WorkspaceDiskGate,
    owner: Arc<()>,
}

impl WorkspaceDiskObservation {
    /// Drain readers without discarding changes already observed during commit preparation.
    pub fn begin_write(
        self,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<WorkspaceDiskWrite, WorkspaceDiskWriteError> {
        self.gate.begin(timeout, cancelled, false, None, Some(&self.owner))
    }

    /// Hand this commit's observation to an explicitly authorized recovery writer.
    pub fn begin_recovery(
        self,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<WorkspaceDiskWrite, WorkspaceDiskWriteError> {
        self.gate.begin(timeout, cancelled, true, None, Some(&self.owner))
    }
}

impl Drop for WorkspaceDiskObservation {
    fn drop(&mut self) {
        let mut state = self.gate.state();
        if !state.writer
            && state.observation.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, &self.owner))
        {
            state.observation = None;
            if !state.recovery {
                state.mutation_filter = None;
            }
        }
    }
}

/// An owned read admission; clones retain the same active operation until the last drop.
#[derive(Clone, Debug)]
pub struct WorkspaceDiskRead(Arc<Read>);

#[derive(Debug)]
struct Read {
    gate: WorkspaceDiskGate,
    id: u64,
    epoch: u64,
}

impl WorkspaceDiskRead {
    /// Register the request's existing Salsa token, including late fan-out workers.
    pub fn register_cancel(&self, token: salsa::CancellationToken) {
        let mut state = self.0.gate.state();
        if state.writer || state.recovery {
            token.cancel();
        }
        if let Some(tokens) = state.readers.get_mut(&self.0.id) {
            tokens.push(token);
        }
    }

    /// The disk epoch against which this operation was admitted.
    pub fn epoch(&self) -> u64 {
        self.0.epoch
    }

    /// Whether a filesystem transaction has superseded this admitted read.
    pub fn is_current(&self) -> bool {
        self.0.gate.is_current(self.0.epoch)
    }
}

impl Drop for Read {
    fn drop(&mut self) {
        self.gate.state().readers.remove(&self.id);
        self.gate.0.drained.notify_all();
    }
}

/// Exclusive disk ownership; dropping after the first source write requires recovery.
#[derive(Debug)]
pub struct WorkspaceDiskWrite {
    gate: WorkspaceDiskGate,
    writing: bool,
    finished: bool,
    released: bool,
}

impl WorkspaceDiskWrite {
    /// Bind input paths and proven clean reload hashes before the first source mutation.
    /// The filter runs under the admission lock and must not call back into this gate.
    pub fn observe_document_changes(
        &mut self,
        filter: impl Fn(&Path, Option<[u8; 32]>) -> bool + Send + Sync + 'static,
    ) {
        self.gate.state().mutation_filter = Some(Arc::new(filter));
    }
    /// Mark the exact boundary immediately before the first source namespace mutation.
    pub fn mark_writing(&mut self) {
        self.writing = true;
    }

    /// Verify that a terminal rescan is being performed by this ticket's owner.
    pub fn is_for(&self, gate: &WorkspaceDiskGate) -> bool {
        Arc::ptr_eq(&self.gate.0, &gate.0)
    }

    /// Reopen admission only after the owner has published its terminal disk rescan.
    pub fn finish(mut self) {
        self.finished = true;
    }

    /// Complete publication and reject a participant mutation atomically with reopening reads.
    pub fn finish_if_unviolated(self) -> Result<(), WorkspaceDiskBusy> {
        self.finish_with_publication(|| Ok(())).map_err(|_| WorkspaceDiskBusy)
    }

    /// Persist terminal publication under the same admission lock as the final mutation check.
    pub fn finish_with_publication(
        mut self,
        publish: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut state = self.gate.state();
        self.writing = true;
        if self.gate.0.participant_violation.load(Ordering::Acquire) {
            return Err("exclusive session changed before terminal publication".into());
        }
        publish()?;
        state.writer = false;
        state.recovery = false;
        state.observation = None;
        state.mutation_filter = None;
        self.released = true;
        self.gate.0.drained.notify_all();
        Ok(())
    }

    /// Keep admission closed when a source conflict prevents a proven terminal state.
    pub fn require_recovery(mut self) {
        self.writing = true;
    }
}

impl Drop for WorkspaceDiskWrite {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let mut state = self.gate.state();
        state.writer = false;
        state.observation = None;
        if self.finished {
            state.recovery = false;
            state.mutation_filter = None;
        } else if self.writing {
            state.recovery = true;
        } else if !state.recovery {
            state.mutation_filter = None;
        }
        self.gate.0.drained.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn observation_keeps_reads_open_and_only_its_owner_can_enter_the_writer() {
        let gate = WorkspaceDiskGate::default();
        let observation =
            gate.observe_document_changes(|path, _| path == Path::new("subject")).unwrap();
        let reader = gate.try_read().unwrap();
        assert!(reader.is_current());
        assert!(!gate.is_frozen());
        assert!(matches!(
            gate.observe_document_changes(|_, _| true),
            Err(WorkspaceDiskWriteError::Busy)
        ));
        assert!(matches!(
            gate.begin_write(Duration::ZERO, &|| false),
            Err(WorkspaceDiskWriteError::Busy)
        ));
        let other_gate = WorkspaceDiskGate::default();
        let other = other_gate.observe_document_changes(|_, _| true).unwrap();
        assert!(matches!(
            gate.begin(Duration::ZERO, &|| false, false, None, Some(&other.owner)),
            Err(WorkspaceDiskWriteError::Busy)
        ));
        assert!(!gate.report_document_change(Path::new("unrelated"), None));
        assert!(gate.report_document_change(Path::new("subject"), Some([1; 32])));
        drop(reader);
        let writer = observation.begin_write(Duration::from_secs(1), &|| false).unwrap();
        assert!(gate.participant_violation().load(Ordering::Acquire));
        drop(writer);
        assert!(!gate.recovery_required());
        assert!(gate.try_read().is_ok());
        drop(gate.observe_document_changes(|_, _| true).unwrap());
        assert!(!gate.report_document_change(Path::new("subject"), None));
    }

    #[test]
    fn cancelled_timed_out_and_dropped_observations_release_the_commit_reservation() {
        let gate = WorkspaceDiskGate::default();
        let _reader = gate.try_read().unwrap();
        for cancel in [true, false] {
            let observation = gate.observe_document_changes(|_, _| true).unwrap();
            assert_eq!(
                observation.begin_write(Duration::ZERO, &|| cancel).unwrap_err(),
                if cancel {
                    WorkspaceDiskWriteError::Cancelled
                } else {
                    WorkspaceDiskWriteError::TimedOut
                }
            );
            assert!(gate.try_read().is_ok());
            assert!(!gate.recovery_required());
            drop(gate.observe_document_changes(|_, _| true).unwrap());
        }
        assert!(!gate.report_document_change(Path::new("subject"), None));
    }

    #[test]
    fn writer_handoff_waits_for_classification_and_keeps_its_violation() {
        let gate = WorkspaceDiskGate::default();
        let (entered, started) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let resumed = Mutex::new(resumed);
        let observation = gate
            .observe_document_changes(move |_, _| {
                entered.send(()).unwrap();
                resumed.lock().unwrap().recv().unwrap();
                true
            })
            .unwrap();
        let reporting = gate.clone();
        let report = std::thread::spawn(move || {
            reporting.report_document_change(Path::new("subject"), None)
        });
        started.recv().unwrap();
        let classification_holds_admission =
            matches!(gate.0.state.try_lock(), Err(std::sync::TryLockError::WouldBlock));
        let (attempted, attempting) = mpsc::channel();
        let (completed, completion) = mpsc::channel();
        let handoff = std::thread::spawn(move || {
            attempted.send(()).unwrap();
            completed.send(observation.begin_write(Duration::from_secs(1), &|| false)).unwrap();
        });
        attempting.recv().unwrap();
        resume.send(()).unwrap();
        assert!(report.join().unwrap());
        let writer = completion.recv().unwrap().unwrap();
        handoff.join().unwrap();
        assert!(classification_holds_admission);
        assert!(gate.participant_violation().load(Ordering::Acquire));
        drop(writer);
        assert!(gate.try_read().is_ok());
    }

    #[test]
    fn handoff_keeps_the_strict_filter_until_the_writer_has_a_reload_basis() {
        let gate = WorkspaceDiskGate::default();
        let observation = gate.observe_document_changes(|_, _| true).unwrap();
        let writer = observation.begin_write(Duration::from_secs(1), &|| false).unwrap();
        assert!(gate.report_document_change(Path::new("subject"), Some([1; 32])));
        drop(writer);
        let observation = gate.observe_document_changes(|_, _| true).unwrap();
        let mut writer = observation.begin_write(Duration::from_secs(1), &|| false).unwrap();
        writer.observe_document_changes(|_, hash| hash != Some([1; 32]));
        writer.mark_writing();
        assert!(!gate.report_document_change(Path::new("subject"), Some([1; 32])));
        writer.finish_if_unviolated().unwrap();
        assert!(gate.try_read().is_ok());
    }

    #[test]
    fn a_paused_background_reader_can_be_cancelled_without_entering_the_workspace() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let gate = WorkspaceDiskGate::default();
        let writer = gate.begin_write(Duration::from_secs(1), &|| false).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let waiting = gate.clone();
        let stop = Arc::clone(&stopped);
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(waiting.read_until_cancelled(&|| stop.load(Ordering::Acquire))).unwrap();
        });
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        stopped.store(true, Ordering::Release);
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_none());
        worker.join().unwrap();
        writer.finish();
        assert!(gate.read_until_cancelled(&|| false).is_some());
    }

    #[test]
    fn writer_cancels_and_drains_owned_readers_before_admitting_writes() {
        let gate = WorkspaceDiskGate::default();
        let reader = gate.try_read().unwrap();
        let retained = reader.clone();
        let token = salsa::CancellationToken::default();
        reader.register_cancel(token.clone());
        let old_epoch = reader.epoch();
        let (tx, rx) = mpsc::channel();
        let writing = gate.clone();
        let worker = std::thread::spawn(move || {
            tx.send(writing.begin_write(Duration::from_secs(5), &|| false)).unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while !token.is_cancelled() {
            assert!(Instant::now() < deadline, "writer never cancelled the active reader");
            std::thread::yield_now();
        }
        assert!(gate.try_read().is_err());
        drop(reader);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        drop(retained);
        let writer = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        worker.join().unwrap();
        assert!(gate.try_read().is_err());
        writer.finish();
        assert!(gate.try_read().is_ok());
        assert!(!gate.is_current(old_epoch));
    }

    #[test]
    fn cancelled_or_timed_out_preflight_reopens_admission_without_recovery() {
        let gate = WorkspaceDiskGate::default();
        let _reader = gate.try_read().unwrap();
        assert!(matches!(
            gate.begin_write(Duration::ZERO, &|| false),
            Err(WorkspaceDiskWriteError::TimedOut)
        ));
        assert!(gate.try_read().is_ok());
        assert!(matches!(
            gate.begin_write(Duration::from_secs(1), &|| true),
            Err(WorkspaceDiskWriteError::Cancelled)
        ));
        assert!(!gate.recovery_required());
        assert!(gate.try_read().is_ok());
    }

    #[test]
    fn participating_mutations_are_scoped_and_checked_atomically_at_terminal_release() {
        let gate = WorkspaceDiskGate::default();
        assert!(!gate.report_document_change(Path::new("subject"), None));
        let writer = gate
            .begin_write_observing(Duration::from_secs(1), &|| false, |path, hash| {
                path == Path::new("subject") && hash != Some([1; 32])
            })
            .unwrap();
        assert!(!gate.report_document_change(Path::new("other"), None));
        assert!(!gate.report_document_change(Path::new("subject"), Some([1; 32])));
        assert!(gate.report_document_change(Path::new("subject"), None));
        assert!(writer.finish_if_unviolated().is_err());
        assert!(gate.recovery_required());
        assert!(gate.try_read().is_err());
        let writer = gate.begin_recovery(Duration::from_secs(1), &|| false).unwrap();
        assert!(!gate.participant_violation().load(Ordering::Acquire));
        writer.finish_if_unviolated().unwrap();
        assert!(gate.try_read().is_ok());
        assert!(!gate.report_document_change(Path::new("subject"), None));
    }

    #[test]
    fn terminal_publication_waits_for_mutation_classification() {
        for participating in [true, false] {
            let gate = WorkspaceDiskGate::default();
            let (entered, started) = mpsc::channel();
            let (resume, resumed) = mpsc::channel();
            let resumed = Mutex::new(resumed);
            let writer = gate
                .begin_write_observing(Duration::from_secs(1), &|| false, move |_, _| {
                    entered.send(()).unwrap();
                    resumed.lock().unwrap().recv().unwrap();
                    participating
                })
                .unwrap();
            let reporting = gate.clone();
            let report = std::thread::spawn(move || {
                reporting.report_document_change(Path::new("subject"), None)
            });
            started.recv().unwrap();
            let classification_holds_admission =
                matches!(gate.0.state.try_lock(), Err(std::sync::TryLockError::WouldBlock));
            let published = Arc::new(AtomicBool::new(false));
            let publication = Arc::clone(&published);
            let (attempted, attempting) = mpsc::channel();
            let (completed, completion) = mpsc::channel();
            let terminal = std::thread::spawn(move || {
                attempted.send(()).unwrap();
                completed
                    .send(writer.finish_with_publication(|| {
                        publication.store(true, Ordering::Release);
                        Ok(())
                    }))
                    .unwrap();
            });
            attempting.recv().unwrap();
            resume.send(()).unwrap();
            let reported = report.join().unwrap();
            let result = completion.recv().unwrap();
            terminal.join().unwrap();
            assert!(classification_holds_admission);
            assert_eq!(reported, participating);
            assert_eq!(result.is_err(), participating);
            assert_eq!(published.load(Ordering::Acquire), !participating);
            assert_eq!(gate.participant_violation().load(Ordering::Acquire), participating);
            assert_eq!(gate.recovery_required(), participating);
            assert_eq!(gate.try_read().is_err(), participating);
        }
    }

    #[test]
    fn failed_durable_publication_keeps_reads_closed_and_does_not_clear_mutation_evidence() {
        let gate = WorkspaceDiskGate::default();
        let writer = gate.begin_write(Duration::from_secs(1), &|| false).unwrap();
        assert_eq!(
            writer.finish_with_publication(|| Err("cache failure".into())),
            Err("cache failure".into())
        );
        assert!(gate.recovery_required());
        let writer = gate.begin_recovery(Duration::from_secs(1), &|| false).unwrap();
        gate.participant_violation().store(true, Ordering::Release);
        let called = AtomicBool::new(false);
        assert!(writer
            .finish_with_publication(|| {
                called.store(true, Ordering::Release);
                Ok(())
            })
            .is_err());
        assert!(!called.load(Ordering::Acquire));
        assert!(gate.recovery_required());
        assert!(gate.try_read().is_err());
    }

    #[test]
    fn source_mutation_latches_recovery_until_exclusive_recovery_finishes() {
        let gate = WorkspaceDiskGate::default();
        drop(gate.begin_write(Duration::from_secs(1), &|| false).unwrap());
        assert!(gate.try_read().is_ok());

        let mut writer = gate.begin_write(Duration::from_secs(1), &|| false).unwrap();
        writer.mark_writing();
        writer.mark_writing();
        drop(writer);
        assert!(gate.recovery_required());
        assert!(gate.try_read().is_err());
        assert!(matches!(
            gate.begin_write(Duration::from_secs(1), &|| false),
            Err(WorkspaceDiskWriteError::RecoveryRequired)
        ));
        drop(gate.begin_recovery(Duration::from_secs(1), &|| false).unwrap());
        assert!(gate.recovery_required());
        gate.begin_recovery(Duration::from_secs(1), &|| false).unwrap().finish();
        assert!(!gate.recovery_required());
        assert!(gate.try_read().is_ok());
    }
}
