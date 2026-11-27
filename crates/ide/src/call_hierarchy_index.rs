use std::time::{Duration, Instant};

use hir::{graph_index::GraphIndex, CallHierarchyReverseIndex, ModuleId};

use crate::graph::BatchDbOpener;

mod pairs;
mod reproject;
mod target_index;

pub use reproject::{reproject_call_hierarchy_index_modules, CallHierarchyIndexModuleProjection};

/// The default number of batch databases the extraction pass keeps in flight.
/// Extra lanes overlap one batch's serial windows (database open, drop, and the
/// tail where a batch's largest module finishes alone) with another batch's
/// parallel region, at the cost of one extra batch's transient residency per
/// lane. Every lane runs a full-width rayon pool, so past ~cores/4 lanes stop
/// overlapping stalls and start thrashing caches: on a 6C/12T ERP bench 3 lanes
/// beat 2 by ~15% and 4 regressed. Hence cores/4, clamped to [1, 3].
pub fn default_build_concurrency() -> usize {
    std::thread::available_parallelism().map_or(1, |threads| threads.get() / 4).clamp(1, 3)
}

/// Borrowed inputs for a bounded call-hierarchy build.
#[derive(Clone, Copy)]
pub struct CallHierarchyIndexBuildRequest<'a> {
    pub modules: &'a [ModuleId],
    pub batch_size: usize,
    /// Upper bound on concurrently live batch databases during extraction
    /// (clamped to at least 1). Peak RSS grows roughly linearly with it.
    pub concurrency: usize,
}

impl<'a> CallHierarchyIndexBuildRequest<'a> {
    pub fn new(modules: &'a [ModuleId], batch_size: usize) -> Self {
        Self { modules, batch_size, concurrency: default_build_concurrency() }
    }

    pub const fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency;
        self
    }
}

/// The two passes that access a fresh batch database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallHierarchyBatchPhase {
    Index,
    MethodPairs,
}

/// A lifecycle boundary for one fresh batch database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallHierarchyBatchEventKind {
    Started,
    DatabaseDropped,
    Completed,
}

/// A timestamped batch lifecycle event retained for build diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallHierarchyBatchEvent {
    pub phase: CallHierarchyBatchPhase,
    pub kind: CallHierarchyBatchEventKind,
    pub batch_index: usize,
    pub module_count: usize,
    pub pair_count: usize,
    pub elapsed: Duration,
}

/// Resident-set sample captured after a batch database and parser caches are released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallHierarchyRssSample {
    pub phase: CallHierarchyBatchPhase,
    pub batch_index: usize,
    pub bytes: Option<usize>,
}

/// Compact call-hierarchy data and measurements produced by a bounded build.
pub struct CallHierarchyIndexBuildResult {
    pub index: CallHierarchyReverseIndex,
    pub target_index: GraphIndex,
    pub method_count: usize,
    pub pair_count: usize,
    pub batch_events: Vec<CallHierarchyBatchEvent>,
    pub rss_samples: Vec<CallHierarchyRssSample>,
    pub elapsed: Duration,
    pub estimated_heap_bytes: usize,
}

/// Failures possible before a complete compact index is produced.
#[derive(Debug, thiserror::Error)]
pub enum CallHierarchyIndexBuildError {
    #[error("failed to build the call-hierarchy rayon pool: {0}")]
    ThreadPool(#[from] rayon::ThreadPoolBuildError),
    #[error(
        "notify projection for module {0:?} was incomplete; refusing to publish call hierarchy"
    )]
    IncompleteNotifyProjection(ModuleId),
    #[error("missing layout hash for indexed module {0:?}")]
    MissingLayoutHash(ModuleId),
}

trait BatchObserver {
    fn on_event(&mut self, event: &CallHierarchyBatchEvent);
    fn on_node_caches_cleared(&mut self, phase: CallHierarchyBatchPhase, batch_index: usize);
}

struct NoopBatchObserver;

impl BatchObserver for NoopBatchObserver {
    fn on_event(&mut self, _: &CallHierarchyBatchEvent) {}

    fn on_node_caches_cleared(&mut self, _: CallHierarchyBatchPhase, _: usize) {}
}

struct BatchLifecycle<'a> {
    started: Instant,
    observer: &'a mut dyn BatchObserver,
    events: &'a mut Vec<CallHierarchyBatchEvent>,
    rss_samples: &'a mut Vec<CallHierarchyRssSample>,
}

impl<'a> BatchLifecycle<'a> {
    fn started(&mut self, phase: CallHierarchyBatchPhase, batch_index: usize, module_count: usize) {
        self.record(phase, CallHierarchyBatchEventKind::Started, batch_index, module_count, 0);
    }

    fn database_dropped(
        &mut self,
        phase: CallHierarchyBatchPhase,
        batch_index: usize,
        module_count: usize,
        pair_count: usize,
    ) {
        self.record(
            phase,
            CallHierarchyBatchEventKind::DatabaseDropped,
            batch_index,
            module_count,
            pair_count,
        );
    }

    /// Record the node-cache clear checkpoint. The clear itself already happened —
    /// `run_batch_db` (or the pipelined lane) performs it right after the batch
    /// database drop — so this only notifies the observer and samples RSS.
    fn node_caches_cleared(&mut self, phase: CallHierarchyBatchPhase, batch_index: usize) {
        self.observer.on_node_caches_cleared(phase, batch_index);
        self.rss_samples.push(rss_sample(phase, batch_index));
    }

    fn completed(
        &mut self,
        phase: CallHierarchyBatchPhase,
        batch_index: usize,
        module_count: usize,
        pair_count: usize,
    ) {
        self.record(
            phase,
            CallHierarchyBatchEventKind::Completed,
            batch_index,
            module_count,
            pair_count,
        );
    }

    fn record(
        &mut self,
        phase: CallHierarchyBatchPhase,
        kind: CallHierarchyBatchEventKind,
        batch_index: usize,
        module_count: usize,
        pair_count: usize,
    ) {
        let event = CallHierarchyBatchEvent {
            phase,
            kind,
            batch_index,
            module_count,
            pair_count,
            elapsed: self.started.elapsed(),
        };
        self.observer.on_event(&event);
        self.events.push(event);
    }
}

fn rss_sample(phase: CallHierarchyBatchPhase, batch_index: usize) -> CallHierarchyRssSample {
    CallHierarchyRssSample { phase, batch_index, bytes: current_rss_bytes() }
}

fn current_rss_bytes() -> Option<usize> {
    std::fs::read_to_string("/proc/self/status").ok()?.lines().find_map(|line| {
        let kibibytes =
            line.strip_prefix("VmRSS:")?.split_whitespace().next()?.parse::<usize>().ok()?;
        kibibytes.checked_mul(1024)
    })
}

/// Builds a compact reverse call index while keeping Salsa and parser residency to one batch.
pub fn build_call_hierarchy_index(
    request: CallHierarchyIndexBuildRequest<'_>,
    open_batch: &mut BatchDbOpener<'_>,
) -> Result<CallHierarchyIndexBuildResult, CallHierarchyIndexBuildError> {
    let mut observer = NoopBatchObserver;
    build_call_hierarchy_index_with_observer(request, open_batch, &mut observer)
}

fn build_call_hierarchy_index_with_observer(
    request: CallHierarchyIndexBuildRequest<'_>,
    open_batch: &mut BatchDbOpener<'_>,
    observer: &mut dyn BatchObserver,
) -> Result<CallHierarchyIndexBuildResult, CallHierarchyIndexBuildError> {
    let started = Instant::now();
    let batch_size = request.batch_size.max(1);
    let _build_span = tracing::info_span!(
        "call_hierarchy_index_build",
        module_count = request.modules.len(),
        batch_size,
        implementation = "compact_reverse_index",
        workspace_call_graph = false,
    )
    .entered();
    tracing::debug!(
        phase = "pass1",
        module_count = request.modules.len(),
        batch_size,
        "call hierarchy compact index build started"
    );
    let pool = rayon::ThreadPoolBuilder::new().build().map_err(|error| {
        tracing::warn!(
            phase = "pass1",
            failure_reason = %error,
            "call hierarchy compact index build failed"
        );
        error
    })?;
    let mut events = Vec::new();
    let mut rss_samples = Vec::new();
    let (index, target_index, method_count) = {
        let mut lifecycle = BatchLifecycle {
            started,
            observer,
            events: &mut events,
            rss_samples: &mut rss_samples,
        };
        let (graph_index, method_count, intents) =
            target_index::build_graph_index_extracting_intents(
                request,
                open_batch,
                &pool,
                &mut lifecycle,
            )?;
        reject_incomplete_notify_projection(&intents)?;
        let index = pairs::resolve_and_fold_method_pairs(
            request,
            open_batch,
            &pool,
            &graph_index,
            &intents,
            &mut lifecycle,
        )?;

        (index, graph_index, method_count)
    };
    let pair_count = index.len();
    let estimated_heap_bytes = index.estimated_heap_bytes();
    tracing::debug!(
        phase = "pass2",
        method_count,
        pair_count,
        estimated_heap_bytes,
        "call hierarchy compact index build completed"
    );
    Ok(CallHierarchyIndexBuildResult {
        index,
        target_index,
        method_count,
        pair_count,
        batch_events: events,
        rss_samples,
        elapsed: started.elapsed(),
        estimated_heap_bytes,
    })
}

/// The incoming index is published atomically only if every module's semantic
/// notify projection completed. A projection that stopped at its work budget
/// (`notify_projection_incomplete`) read only part of its registrations, so no
/// reverse index may be built from it. Returning an error lets the service mark
/// the generation Failed, and incoming requests then remain `null` instead of
/// serving a partial reverse index as Ready. Named unknown-array gaps
/// (`notify_dynamic_signature_analysis_incomplete`) are an intentional
/// conservative boundary and do not reject; the catch-up path screens
/// reprojected modules through the same contract (see
/// [`reproject_call_hierarchy_index_modules`]).
fn reject_incomplete_notify_projection(
    intents: &[(ModuleId, hir::call_graph::ModuleCallSummary)],
) -> Result<(), CallHierarchyIndexBuildError> {
    if let Some((module, _)) =
        intents.iter().find(|(_, summary)| summary.notify_projection_incomplete)
    {
        return Err(CallHierarchyIndexBuildError::IncompleteNotifyProjection(*module));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
