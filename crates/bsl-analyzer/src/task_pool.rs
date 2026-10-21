use std::num::NonZeroUsize;
use std::sync::OnceLock;
use std::thread::available_parallelism;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};

const DEFAULT_MAX_WORKERS: usize = 8;

const WORKER_COUNT_ENV: &str = "BSL_LSP_WORKERS";

const BACKPRESSURE_FACTOR: usize = 4;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Why a [`TaskPool::try_spawn`] did not enqueue its job. In both cases the job
/// was dropped (its captures released) before returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrySpawnError {
    /// The bounded job queue is full.
    Full,
    /// All workers exited and the job channel is closed.
    Disconnected,
}

pub struct TaskPool<T> {
    pub sender: Sender<T>,
    job_sender: Sender<Job>,
}

pub struct Handle<T> {
    pub pool: TaskPool<T>,
    pub receiver: Receiver<T>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl<T: Send + 'static> Handle<T> {
    /// Run `f` once on every worker thread. A barrier rendezvous parks each worker
    /// that ran its copy until all of them have, so one worker cannot consume
    /// several of the jobs while another never sees one. For clearing per-thread
    /// state — e.g. the parser's thread-local green-node cache — that the event
    /// loop cannot reach across threads.
    ///
    /// All-or-nothing: the jobs are enqueued only when the queue has room for
    /// every one of them, so a partially-spawned rendezvous can never park a
    /// subset of the workers forever (the check is race-free because the event
    /// loop is the queue's single producer). Meant for quiescent moments: while
    /// the rendezvous forms, parked workers serve no other jobs.
    pub fn try_broadcast(&self, f: impl Fn() + Send + Sync + 'static) -> Result<(), TrySpawnError> {
        let workers = self.workers.len();
        let slack = self
            .pool
            .job_sender
            .capacity()
            .map(|cap| cap.saturating_sub(self.pool.job_sender.len()))
            .unwrap_or(usize::MAX);
        if slack < workers {
            return Err(TrySpawnError::Full);
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
        let f = std::sync::Arc::new(f);
        for _ in 0..workers {
            let barrier = std::sync::Arc::clone(&barrier);
            let f = std::sync::Arc::clone(&f);
            self.pool.try_spawn_detached(move || {
                f();
                barrier.wait();
            })?;
        }
        Ok(())
    }
}

impl<T: Send + 'static> TaskPool<T> {
    pub fn new_with_handle() -> Handle<T> {
        Self::new_with_workers(worker_count())
    }

    pub fn new_with_workers(workers: usize) -> Handle<T> {
        let workers = workers.max(1);
        let (sender, receiver) = unbounded();
        let capacity = workers.saturating_mul(BACKPRESSURE_FACTOR).max(workers);
        let (job_sender, job_receiver) = bounded::<Job>(capacity);

        tracing::info!(workers, capacity, "Starting bounded task pool");

        let mut handles = Vec::with_capacity(workers);
        for worker_idx in 0..workers {
            let job_rx = job_receiver.clone();
            let handle = std::thread::Builder::new()
                .name(format!("bsl-task-pool-{worker_idx}"))
                .spawn(move || worker_loop(job_rx))
                .expect("failed to spawn task pool worker");
            handles.push(handle);
        }

        Handle { pool: TaskPool { sender, job_sender }, receiver, workers: handles }
    }

    /// Enqueue a job without ever blocking the caller. The job queue is bounded
    /// for backpressure, and the event loop is the only producer: were it to
    /// block here while workers are saturated, the whole server (including VFS
    /// load draining and progress reporting) would stall. On `Full` the job —
    /// together with everything it captured, e.g. a Salsa db clone — is dropped
    /// immediately, so the caller's thread never parks while holding resources
    /// that a pending db write would wait on; the caller applies its own
    /// overflow policy (decline the request, requeue, or skip).
    pub fn try_spawn<F>(&self, task: F) -> Result<(), TrySpawnError>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let sender = self.sender.clone();
        let job: Job = Box::new(move || {
            let result = task();
            let _ = sender.send(result);
        });
        match self.job_sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(crossbeam_channel::TrySendError::Full(_)) => Err(TrySpawnError::Full),
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                tracing::warn!("task pool job channel closed; dropping task");
                Err(TrySpawnError::Disconnected)
            }
        }
    }

    /// Enqueue a raw job that posts no completion message back to the event loop.
    /// For thread-maintenance work (clearing a worker's thread-local caches) whose
    /// completion must not count as loop activity — a result `T` would wake the
    /// loop and reset the idle clock that scheduled the job in the first place.
    fn try_spawn_detached<F>(&self, job: F) -> Result<(), TrySpawnError>
    where
        F: FnOnce() + Send + 'static,
    {
        match self.job_sender.try_send(Box::new(job)) {
            Ok(()) => Ok(()),
            Err(crossbeam_channel::TrySendError::Full(_)) => Err(TrySpawnError::Full),
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                tracing::warn!("task pool job channel closed; dropping detached job");
                Err(TrySpawnError::Disconnected)
            }
        }
    }

    /// Whether the job queue can accept another job right now. The event loop is
    /// the single producer, so between this check and a subsequent
    /// [`Self::try_spawn`] the queue can only drain — a `true` here guarantees
    /// the spawn succeeds, letting callers skip building a job (and cloning the
    /// Salsa db into it) when the pool is saturated.
    pub fn has_capacity(&self) -> bool {
        !self.job_sender.is_full()
    }

    pub fn capacity(&self) -> Option<usize> {
        self.job_sender.capacity()
    }
}

impl<T: Send + 'static> Default for TaskPool<T> {
    fn default() -> Self {
        Self::new_with_handle().pool
    }
}

fn worker_loop(job_receiver: Receiver<Job>) {
    while let Ok(job) = job_receiver.recv() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
    }
}

fn worker_count() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        if let Ok(raw) = std::env::var(WORKER_COUNT_ENV) {
            match raw.parse::<usize>() {
                Ok(parsed) => {
                    let clamped = parsed.max(1);
                    tracing::info!(workers = clamped, raw = %raw, "Resolved BSL_LSP_WORKERS");
                    return clamped;
                }
                Err(_) => tracing::warn!(
                    raw = %raw,
                    "BSL_LSP_WORKERS is not a valid usize; falling back to default"
                ),
            }
        }
        let cpus = available_parallelism().map(NonZeroUsize::get).unwrap_or(1);
        cpus.clamp(1, DEFAULT_MAX_WORKERS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Bound of a wait for a result that a regression would never deliver; a hang
    /// watchdog, not a latency measure.
    const HANG_WATCHDOG: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));

    #[test]
    fn bounded_pool_executes_more_jobs_than_workers() {
        let workers = 2;
        let handle = TaskPool::<usize>::new_with_workers(workers);
        let counter = Arc::new(AtomicUsize::new(0));

        let jobs = 20;
        let mut received = 0;
        for _ in 0..jobs {
            // On a full queue, drain a result (a worker finished, freeing a
            // slot) and retry — the non-blocking contract pushes the wait to
            // the caller.
            loop {
                let counter = Arc::clone(&counter);
                let job = move || {
                    let v = counter.fetch_add(1, Ordering::SeqCst);
                    // Slow enough to fill the queue in the timing profile; either way the loop
                    // takes both turns a full queue and a free one can give it.
                    if cfg!(timing_contract) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    v + 1
                };
                match handle.pool.try_spawn(job) {
                    Ok(()) => break,
                    Err(TrySpawnError::Full) => {
                        handle
                            .receiver
                            .recv_timeout(HANG_WATCHDOG)
                            .expect("a result must arrive while the queue is full");
                        received += 1;
                    }
                    Err(TrySpawnError::Disconnected) => panic!("pool must be alive"),
                }
            }
        }

        while received < jobs {
            match handle.receiver.recv_timeout(HANG_WATCHDOG) {
                Ok(_) => received += 1,
                Err(err) => panic!("missing result after {received} of {jobs}: {err}"),
            }
        }
        assert_eq!(counter.load(Ordering::SeqCst), jobs);
    }

    #[test]
    fn try_broadcast_reaches_every_worker_exactly_once() {
        let workers = 4;
        let handle = TaskPool::<usize>::new_with_workers(workers);
        let (tx, rx) = unbounded();
        handle
            .try_broadcast(move || {
                let _ = tx.send(std::thread::current().id());
            })
            .expect("an idle pool accepts a broadcast");

        let ids: std::collections::HashSet<_> = (0..workers)
            .map(|_| {
                rx.recv_timeout(test_utils::wait::hang_guard(std::time::Duration::from_secs(10)))
                    .expect("every worker runs its copy")
            })
            .collect();
        assert_eq!(ids.len(), workers, "the barrier must fan the job out to distinct workers");

        // The rendezvous released: the pool still serves ordinary jobs.
        handle.pool.try_spawn(|| 7).expect("pool alive after broadcast");
        assert_eq!(
            handle
                .receiver
                .recv_timeout(test_utils::wait::hang_guard(std::time::Duration::from_secs(10))),
            Ok(7),
            "workers must be released after the rendezvous"
        );
    }

    #[test]
    fn try_broadcast_refuses_partial_spawn_when_queue_lacks_room() {
        let handle = TaskPool::<usize>::new_with_workers(1);
        // Park the single worker so enqueued jobs stay queued.
        let (gate_tx, gate_rx) = bounded::<()>(0);
        handle
            .pool
            .try_spawn(move || {
                let _ = gate_rx.recv();
                0
            })
            .expect("first job fits");
        while handle.pool.try_spawn(|| 1).is_ok() {}

        assert_eq!(
            handle.try_broadcast(|| ()).unwrap_err(),
            TrySpawnError::Full,
            "a full queue must refuse the whole broadcast, not enqueue part of it"
        );
        drop(gate_tx);
    }

    #[test]
    fn try_spawn_reports_full_and_recovers() {
        let handle = TaskPool::<usize>::new_with_workers(1);
        // Park the single worker on a gate so the queue can only fill up.
        let (gate_tx, gate_rx) = bounded::<()>(0);
        handle
            .pool
            .try_spawn(move || {
                let _ = gate_rx.recv();
                0
            })
            .expect("first job fits");

        let mut accepted = 1;
        let mut saw_full = false;
        for _ in 0..32 {
            match handle.pool.try_spawn(|| 1) {
                Ok(()) => accepted += 1,
                Err(TrySpawnError::Full) => {
                    saw_full = true;
                    break;
                }
                Err(TrySpawnError::Disconnected) => panic!("pool must be alive"),
            }
        }
        assert!(saw_full, "a bounded queue must report Full instead of blocking");
        assert!(!handle.pool.has_capacity());

        drop(gate_tx);
        for i in 0..accepted {
            handle
                .receiver
                .recv_timeout(HANG_WATCHDOG)
                .unwrap_or_else(|err| panic!("accepted job {i} must still run: {err}"));
        }
        assert!(handle.pool.has_capacity(), "draining the queue restores capacity");
    }

    #[test]
    fn job_channel_capacity_scales_with_workers() {
        for workers in [1usize, 4, 8] {
            let handle = TaskPool::<()>::new_with_workers(workers);
            assert_eq!(
                handle.pool.capacity(),
                Some(workers * BACKPRESSURE_FACTOR),
                "queue capacity must be workers × BACKPRESSURE_FACTOR for backpressure"
            );
        }
    }

    #[test]
    fn panicking_job_does_not_kill_pool() {
        // The panic count is process-wide and `dispatch` asserts on it; this
        // guard keeps an intentional panic out of that assertion's window.
        let _guard = crate::panic_watch::panic_test_guard();
        let handle = TaskPool::<&'static str>::new_with_workers(1);
        handle.pool.try_spawn(|| panic!("intentional")).unwrap();
        handle.pool.try_spawn(|| "ok").unwrap();
        let result = handle
            .receiver
            .recv_timeout(HANG_WATCHDOG)
            .expect("second job should run after panicking peer");
        assert_eq!(result, "ok");
    }
}
