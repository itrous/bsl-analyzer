use super::retry_window::{RetryDecision, RetryOwner, RetryWindow};
#[cfg(test)]
use super::MAX_RESIDENT_PREFETCH_PATHS_PER_QUERY;
use super::{SharedSearchEngine, SharedState};
use crate::change_hub::WorkspaceChangeHub;
use crate::graph::GraphState;
use bsl_search::SearchEngine;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct SearchDriftPlan {
    dirty_paths: Vec<PathBuf>,
    removed_paths: Vec<PathBuf>,
    removed_subtrees: Vec<PathBuf>,
    context_paths: Vec<PathBuf>,
    mark_all_context: bool,
    rewalk_paths: Vec<PathBuf>,
    reconcile_present: Option<std::collections::HashSet<PathBuf>>,
    dirty_keys: Vec<bsl_search::FileKey>,
    removed_keys: Vec<bsl_search::FileKey>,
    context_keys: Vec<bsl_search::FileKey>,
    full_rescan: bool,
    roots_epoch: u64,
    preparation_error: Option<String>,
    snapshot_outcome: Option<SnapshotPreparationOutcome>,
    snapshot_paths: Vec<PathBuf>,
    nudge_rebuild: bool,
    nudge_project_reload: bool,
    dirty_cursor: usize,
    removed_cursor: usize,
    context_cursor: usize,
}

enum SnapshotPreparationOutcome {
    OperationError(String),
    TransientRefusal,
    Superseded,
    Released,
}

enum ReferencingFilesOutcome {
    Applied(std::collections::HashSet<PathBuf>),
    OperationError(String),
    TransientRefusal,
    Superseded,
    Released,
}

#[derive(Default)]
struct RescanDebt {
    streak: u32,
    next_allowed: Option<std::time::Instant>,
}

impl RescanDebt {
    fn required(&self) -> bool {
        self.next_allowed.is_some()
    }

    fn record_failure(&mut self, now: std::time::Instant) {
        let delay = super::overlay_retry::retry_delay(self.streak);
        self.streak = self.streak.saturating_add(1);
        self.next_allowed = Some(now + delay);
    }

    fn waiting(&self, now: std::time::Instant) -> bool {
        #[cfg(test)]
        if FORCE_RESCAN_DEBT_DUE.load(Ordering::SeqCst) {
            return false;
        }
        self.next_allowed.is_some_and(|next| now < next)
    }

    fn wait_for(&self, now: std::time::Instant, idle: Duration) -> Duration {
        #[cfg(test)]
        if FORCE_RESCAN_DEBT_DUE.load(Ordering::SeqCst) && self.required() {
            return Duration::ZERO;
        }
        self.next_allowed.map_or(idle, |next| next.saturating_duration_since(now).min(idle))
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

impl SearchDriftPlan {
    fn complete(&self) -> bool {
        self.dirty_cursor == self.dirty_keys.len()
            && self.removed_cursor == self.removed_keys.len()
            && self.context_cursor == self.context_keys.len()
    }
}

/// Test seam: force a reconcile walk (the overflow rescan and the boot store reconcile) to count as
/// errored, so a test can assert the reconcile is skipped (a partial walk must never be treated as
/// authoritative and delete healthy files) — and, at boot, that a Clean init downgrades to a prime.
#[cfg(test)]
pub(super) static FORCE_REWALK_WALK_ERROR: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static FORCE_DRIFT_APPLY_ERROR_ENGINE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
static FORCE_RESCAN_DEBT_DUE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One slice of waiting for the watch to arm, and the whole budget for it.
///
/// The slice is only how often the wait comes up for air; the budget is what actually
/// bounds it. Ten minutes is far more than the initial walk of a large configuration
/// takes and still finite — a boot that waited forever would never publish an engine at
/// all, and one that gave up after a single slice abandons the workspace whose walk
/// merely outlasted a minute.
const WATCH_READY_SLICE: Duration = Duration::from_secs(60);
const WATCH_READY_BUDGET: Duration = Duration::from_secs(600);

/// How long the boot waits for the watch, in one slice and in total. A parameter
/// rather than the two constants read directly, so a test can drive the slow-start path
/// without standing through a production-sized slice.
#[derive(Debug, Clone, Copy)]
pub(super) struct WatchWaitPolicy {
    slice: Duration,
    budget: Duration,
}

impl WatchWaitPolicy {
    pub(super) const PRODUCTION: Self =
        Self { slice: WATCH_READY_SLICE, budget: WATCH_READY_BUDGET };

    #[cfg(test)]
    pub(super) fn new(slice: Duration, budget: Duration) -> Self {
        Self { slice, budget }
    }
}

impl SharedState {
    /// Wait for the watch to arm; `false` means it will not, or not within the budget.
    ///
    /// Three readiness answers, two decisions. `Failed` is permanent, so waiting out the
    /// budget over it would only delay a boot that has to happen either way. `NotYet` says
    /// nothing has gone wrong yet — a long initial walk looks exactly like this — so the
    /// wait resumes until the budget is spent.
    pub(super) fn await_watch(hub: &WorkspaceChangeHub, policy: WatchWaitPolicy) -> bool {
        let deadline = std::time::Instant::now() + policy.budget;
        loop {
            // The slice never outlives the budget. Asked for a whole slice at the very end
            // of one, the hub answers a slice past the deadline the caller was promised —
            // and with a production slice that overshoot is a minute, long enough to be
            // mistaken for a hub that is still arming.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match hub.watch_readiness(policy.slice.min(remaining)) {
                crate::change_hub::WatchReadiness::Armed => return true,
                crate::change_hub::WatchReadiness::Failed => {
                    tracing::warn!(
                        "workspace change hub could not be set up; search overlay stays in scan mode"
                    );
                    return false;
                }
                crate::change_hub::WatchReadiness::NotYet => {
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            budget_secs = policy.budget.as_secs(),
                            "workspace change hub did not arm within the budget; search overlay stays in scan mode for this run of the daemon"
                        );
                        return false;
                    }
                }
            }
        }
    }

    /// Drive the search overlay from the change hub. Search is one sink among
    /// several: it drains its own cursor and applies the shared drift classification
    /// (stateless policy) — `.bsl` bodies marked dirty, deleted `.bsl` removed from the
    /// store, `.xml` metadata resolved to the affected documents' context. The raw
    /// (non-canonical) path is used so the strip against the configured source root
    /// still matches when that root has symlinks.
    ///
    /// Started by the boot, after the engine is published and only once the watch is up.
    /// Existing any earlier would mean draining events into an engine that is not there:
    /// every apply below no-ops on `None` and the batch is gone for good. The cursor is
    /// older than the boot's own read of disk, so the stream begins strictly before the
    /// baseline it corrects and nothing falls between the two.
    ///
    /// Returns whether the thread started. Until it does, the cursor belongs to the caller.
    pub(super) fn spawn_search_sink(
        hub: WorkspaceChangeHub,
        cursor: crate::change_hub::SinkCursor,
        engine: SharedSearchEngine,
        graph: GraphState,
        overlay_retry: Option<Arc<super::overlay_retry::OverlayRetry>>,
        root_drift_epoch: Arc<AtomicU64>,
        lease: crate::workspace_lease::WorkspaceLease,
    ) -> bool {
        std::thread::Builder::new()
            .name("bsl-search-overlay-watch".to_owned())
            .spawn(move || {
                let mut cursor = cursor;
                let mut generation = 0u64;
                let mut enable_retry = RetryWindow::new(RetryOwner::ChangeHub);
                let mut enable_rescan_debt = false;
                // Watcher mode is one-way in the store and doubles as "skip the full
                // rescan", so the only place it can be asked for safely is inside the
                // consumer that feeds it: a running thread is a feeder that exists.
                loop {
                    match Self::apply_workspace_search(&engine, &lease, |engine| {
                        engine.enable_workspace_watcher_mode();
                        Ok(())
                    }) {
                        super::WorkspaceSearchApply::Applied(()) => break,
                        super::WorkspaceSearchApply::TransientRefusal => {
                            match enable_retry
                                .refused(std::time::Instant::now(), Duration::from_secs(2))
                            {
                                RetryDecision::RetryAfter(delay) => std::thread::sleep(delay),
                                RetryDecision::Stop(_) => loop {
                                    generation =
                                        hub.wait_for_change(generation, Duration::from_secs(30));
                                    let batch = hub.materialize(cursor);
                                    if !batch.entries.is_empty() || batch.rescan_required {
                                        enable_retry.observe_external_work(true);
                                        break;
                                    }
                                },
                            }
                        }
                        super::WorkspaceSearchApply::Superseded
                        | super::WorkspaceSearchApply::Released => {
                            enable_retry.terminal();
                            hub.unsubscribe(cursor);
                            return;
                        }
                        super::WorkspaceSearchApply::OperationError(error) => {
                            enable_retry.operation_error();
                            enable_rescan_debt = true;
                            tracing::warn!(
                                "could not enable workspace watcher mode; sink stays dormant until fresh work: {error}"
                            );
                            loop {
                                generation =
                                    hub.wait_for_change(generation, Duration::from_secs(30));
                                let batch = hub.materialize(cursor);
                                if !batch.entries.is_empty() || batch.rescan_required {
                                    enable_retry = RetryWindow::new(RetryOwner::ChangeHub);
                                    break;
                                }
                            }
                        }
                    }
                }
                tracing::info!("search overlay sink subscribed to workspace change hub");

                let mut pending = None;
                let mut drift_retry = None;
                let mut rescan_debt = RescanDebt::default();
                if enable_rescan_debt {
                    rescan_debt.record_failure(std::time::Instant::now());
                }
                loop {
                    if pending.is_none() {
                        generation = hub.wait_for_change(
                            generation,
                            rescan_debt
                                .wait_for(std::time::Instant::now(), Duration::from_secs(30)),
                        );
                        let batch = hub.materialize(cursor);
                        if rescan_debt.waiting(std::time::Instant::now()) {
                            let fresh = !batch.entries.is_empty() || batch.rescan_required;
                            if fresh {
                                if Self::root_transition_relevant_drift(
                                    &batch.entries,
                                    true,
                                    &graph,
                                ) {
                                    root_drift_epoch.fetch_add(1, Ordering::SeqCst);
                                }
                                graph.nudge_rebuild();
                                graph.nudge_project_reload();
                                hub.acknowledge(&batch);
                                cursor = batch.cursor;
                                if let Some(retry) = &overlay_retry {
                                    retry.kick_fresh();
                                }
                            }
                            continue;
                        }
                        let fresh = !batch.entries.is_empty() || batch.rescan_required;
                        let rescan_required = batch.rescan_required || rescan_debt.required();
                        let root_relevant = Self::root_transition_relevant_drift(
                            &batch.entries,
                            rescan_required,
                            &graph,
                        );
                        let plan = Self::prepare_search_drift(
                            &engine,
                            &batch.entries,
                            rescan_required,
                            &graph,
                        );
                        pending = Some((batch, plan, fresh, root_relevant));
                        drift_retry = Some(RetryWindow::new(RetryOwner::Drift));
                    }
                    let (batch, plan, fresh, root_relevant) = pending.as_mut().unwrap();
                    if *root_relevant {
                        root_drift_epoch.fetch_add(1, Ordering::SeqCst);
                        *root_relevant = false;
                    }
                    let applied = Self::apply_prepared_search_drift(&engine, &lease, plan, &graph);
                    if matches!(&applied, super::WorkspaceSearchApply::Applied(false)) {
                        continue;
                    }
                    // Nudge after the mark attempt so a successful build captures its seq bound.
                    // This remains independent of the apply outcome: an error still gets a graph
                    // catch-up, while a transient retry nudges again and records a follow-up build.
                    if plan.nudge_rebuild {
                        graph.nudge_rebuild();
                    }
                    if plan.nudge_project_reload {
                        graph.nudge_project_reload();
                    }
                    match applied {
                        super::WorkspaceSearchApply::Applied(true) => {
                            if let Some(retry) = drift_retry.as_mut() {
                                retry.complete();
                            }
                            if plan.full_rescan {
                                rescan_debt.clear();
                            }
                        }
                        super::WorkspaceSearchApply::TransientRefusal => {
                            let retry = drift_retry.as_mut().expect("pending drift owns a budget");
                            let delay = super::overlay_retry::retry_delay(retry.streak());
                            if let RetryDecision::RetryAfter(delay) =
                                retry.refused(std::time::Instant::now(), delay)
                            {
                                std::thread::sleep(delay);
                                continue;
                            }
                            tracing::warn!(
                                "search drift lease retry budget exhausted; advancing the hub cursor"
                            );
                            rescan_debt.record_failure(std::time::Instant::now());
                        }
                        super::WorkspaceSearchApply::OperationError(error) => {
                            if let Some(retry) = drift_retry.as_mut() {
                                retry.operation_error();
                            }
                            tracing::warn!(
                                "search drift apply failed; advancing the hub cursor: {error}"
                            );
                            rescan_debt.record_failure(std::time::Instant::now());
                        }
                        super::WorkspaceSearchApply::Superseded
                        | super::WorkspaceSearchApply::Released => {
                            if let Some(retry) = drift_retry.as_mut() {
                                retry.terminal();
                            }
                            hub.unsubscribe(cursor);
                            return;
                        }
                        super::WorkspaceSearchApply::Applied(false) => unreachable!(),
                    }
                    hub.acknowledge(batch);
                    cursor = batch.cursor;
                    // Root-transition retry is independent of new file events. This loop
                    // already owns the bounded wake, so no second timer/thread is needed.
                    graph.flush_pending_search_roots_refresh();
                    // Only GENUINE drift kicks the retry driver (and resets its backoff):
                    // this loop also wakes on the bare 30-second timeout with an empty
                    // batch, and an unconditional kick would zero the backoff each tick.
                    if *fresh {
                        if let Some(retry) = &overlay_retry {
                            retry.kick_fresh();
                        }
                    }
                    pending = None;
                    drift_retry = None;
                }
            })
            .is_ok()
    }

    /// Whether a drained batch can invalidate a root-transition filesystem snapshot. Source and
    /// metadata files, analyzer config, subtree loss and detail-losing rescans are relevant.
    /// `MaybeRemoved` is conservative because a vanished path cannot be stat-ed to distinguish a
    /// file from a directory (including directories whose names contain a dot).
    fn root_transition_relevant_drift(
        entries: &[crate::change_hub::ChangeEntry],
        rescan_required: bool,
        graph: &GraphState,
    ) -> bool {
        rescan_required
            || entries.iter().any(|entry| {
                matches!(
                    entry.kind,
                    crate::change_hub::ChangeKind::MaybeRemoved
                        | crate::change_hub::ChangeKind::SubtreeRemoved
                ) || project_model::file_role(&entry.canonical) != project_model::FileRole::Ignored
                    || project_model::file_role(&entry.raw) != project_model::FileRole::Ignored
                    || graph.is_workspace_config_path(&entry.canonical)
                    || graph.is_workspace_config_path(&entry.raw)
            })
    }

    /// Apply one drained batch to the search overlay. Extracted from the sink loop so it
    /// is unit-testable without driving the thread. On overflow (exact paths lost) it
    /// re-walks the whole tree; otherwise it classifies (stateless policy) and applies
    /// each bucket: `.bsl` bodies dirty, deleted `.bsl` removed, `.xml` → affected context.
    #[cfg(test)]
    pub(super) fn apply_search_drift(
        engine: &SharedSearchEngine,
        entries: &[crate::change_hub::ChangeEntry],
        rescan_required: bool,
        graph: &GraphState,
    ) {
        let mut plan = Self::prepare_search_drift(engine, entries, rescan_required, graph);
        let result = loop {
            let result = Self::apply_prepared_search_drift(
                engine,
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                &mut plan,
                graph,
            );
            if !matches!(&result, super::WorkspaceSearchApply::Applied(false)) {
                break result;
            }
        };
        if plan.nudge_rebuild {
            graph.nudge_rebuild();
        }
        if plan.nudge_project_reload {
            graph.nudge_project_reload();
        }
        debug_assert!(matches!(result, super::WorkspaceSearchApply::Applied(true)));
    }

    fn prepare_search_drift(
        engine: &SharedSearchEngine,
        entries: &[crate::change_hub::ChangeEntry],
        rescan_required: bool,
        graph: &GraphState,
    ) -> SearchDriftPlan {
        let class =
            crate::drift_classify::classify_drift(entries, &std::collections::HashSet::new(), None);
        let mut plan = SearchDriftPlan::default();
        plan.removed_paths.extend(class.bsl_removed.iter().map(|path| path.raw.clone()));
        plan.removed_subtrees.extend(
            entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.kind,
                        crate::change_hub::ChangeKind::SubtreeRemoved
                            | crate::change_hub::ChangeKind::MaybeRemoved
                    )
                })
                .map(|entry| entry.raw.clone()),
        );

        if rescan_required {
            tracing::warn!(
                "workspace change hub overflowed; re-marking all workspace .bsl paths dirty for the search overlay"
            );
            plan.mark_all_context = true;
            plan.full_rescan = true;
            plan.nudge_project_reload = true;
            Self::prepare_search_rewalk(engine, &mut plan);
            Self::materialize_search_drift(engine, &mut plan);
            return plan;
        }

        plan.dirty_paths.extend(class.bsl_modified.iter().map(|path| path.raw.clone()));
        if !class.xml_paths.is_empty() {
            let roots = {
                let Ok(guard) = engine.lock() else { return plan };
                guard.as_ref().and_then(|engine| engine.workspace_roots().cloned())
            };
            let mut mark_whole = false;
            for path in &class.xml_paths {
                if is_root_descriptor(roots.as_ref(), &path.raw) {
                    mark_whole = true;
                } else if let Some(subtree) = owned_module_subtree(&path.raw) {
                    plan.context_paths.extend(walk_bsl_files(&subtree));
                }
            }
            let snapshot_paths: Vec<_> =
                class.xml_paths.iter().map(|path| path.raw.clone()).collect();
            match Self::resolve_referencing_module_files(graph, &snapshot_paths) {
                ReferencingFilesOutcome::Applied(paths) => plan.context_paths.extend(paths),
                ReferencingFilesOutcome::OperationError(error) => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::OperationError(error));
                    plan.full_rescan = true;
                }
                ReferencingFilesOutcome::TransientRefusal => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::TransientRefusal);
                    plan.snapshot_paths = snapshot_paths;
                }
                ReferencingFilesOutcome::Superseded => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::Superseded)
                }
                ReferencingFilesOutcome::Released => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::Released)
                }
            }
            plan.mark_all_context |= mark_whole;
            plan.nudge_rebuild = mark_whole || !plan.context_paths.is_empty();
        }
        if entries.iter().any(|entry| {
            graph.is_workspace_config_path(&entry.canonical)
                || graph.is_workspace_config_path(&entry.raw)
        }) {
            plan.mark_all_context = true;
            plan.nudge_project_reload = true;
        }
        if class.structural_rescan {
            Self::prepare_search_rewalk(engine, &mut plan);
        }
        Self::materialize_search_drift(engine, &mut plan);
        plan
    }

    fn materialize_search_drift(engine: &SharedSearchEngine, plan: &mut SearchDriftPlan) {
        let Ok(guard) = engine.lock() else {
            plan.preparation_error = Some("search engine lock poisoned".to_owned());
            return;
        };
        let Some(engine) = guard.as_ref() else { return };
        plan.roots_epoch = engine.workspace_roots_epoch();

        plan.dirty_keys.extend(
            plan.dirty_paths
                .iter()
                .chain(&plan.rewalk_paths)
                .filter_map(|path| engine.workspace_file_key(path)),
        );
        plan.removed_keys
            .extend(plan.removed_paths.iter().filter_map(|path| engine.workspace_file_key(path)));
        plan.context_keys
            .extend(plan.context_paths.iter().filter_map(|path| engine.workspace_file_key(path)));

        let prepared = (|| -> Result<(), bsl_search::SearchError> {
            if plan.mark_all_context {
                plan.context_keys.extend(engine.known_workspace_keys()?);
                plan.mark_all_context = false;
            }
            plan.removed_keys.extend(engine.vanished_workspace_keys(&plan.removed_subtrees)?);
            if let Some(present) = &plan.reconcile_present {
                let present: std::collections::HashSet<_> =
                    present.iter().filter_map(|path| engine.workspace_file_key(path)).collect();
                plan.removed_keys
                    .extend(engine.known_workspace_keys()?.difference(&present).cloned());
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            plan.preparation_error = Some(error.to_string());
        }
        plan.removed_keys.sort_unstable();
        plan.removed_keys.dedup();
        plan.dirty_keys.sort_unstable();
        plan.dirty_keys.dedup();
        plan.context_keys.sort_unstable();
        plan.context_keys.dedup();
    }

    fn prepare_search_rewalk(engine: &SharedSearchEngine, plan: &mut SearchDriftPlan) {
        let Some((declared, excluded)) = Self::registered_roots_and_exclusions(engine) else {
            return;
        };
        let set = project_model::SourceSet::scan_excluding(&declared, &excluded);
        let present: std::collections::HashSet<_> = set
            .files
            .iter()
            .filter(|file| file.role == project_model::FileRole::Source)
            .map(|file| file.walked.clone())
            .collect();
        plan.rewalk_paths.extend(present.iter().cloned());
        let incomplete = !set.clean();
        #[cfg(test)]
        let incomplete =
            incomplete || FORCE_REWALK_WALK_ERROR.load(std::sync::atomic::Ordering::SeqCst);
        if incomplete {
            tracing::warn!(
                unreadable = set.unreadable,
                canonical_fallbacks = set.canonical_fallbacks,
                "search rescan walk incomplete; skipping reconcile to avoid deleting healthy files"
            );
        } else {
            plan.reconcile_present = Some(present);
        }
    }

    fn apply_prepared_search_drift(
        shared: &SharedSearchEngine,
        lease: &crate::workspace_lease::WorkspaceLease,
        plan: &mut SearchDriftPlan,
        _graph: &GraphState,
    ) -> super::WorkspaceSearchApply<bool, bsl_search::SearchError> {
        if matches!(plan.snapshot_outcome, Some(SnapshotPreparationOutcome::TransientRefusal)) {
            match Self::resolve_referencing_module_files(_graph, &plan.snapshot_paths) {
                ReferencingFilesOutcome::Applied(paths) => {
                    plan.context_paths.extend(paths);
                    plan.snapshot_outcome = None;
                    plan.snapshot_paths.clear();
                    Self::materialize_search_drift(shared, plan);
                }
                ReferencingFilesOutcome::OperationError(error) => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::OperationError(error));
                }
                ReferencingFilesOutcome::TransientRefusal => {}
                ReferencingFilesOutcome::Superseded => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::Superseded)
                }
                ReferencingFilesOutcome::Released => {
                    plan.snapshot_outcome = Some(SnapshotPreparationOutcome::Released)
                }
            }
        }
        if let Some(outcome) = plan.snapshot_outcome.as_ref() {
            return match outcome {
                SnapshotPreparationOutcome::OperationError(error) => {
                    super::WorkspaceSearchApply::OperationError(bsl_search::SearchError::Index(
                        error.clone(),
                    ))
                }
                SnapshotPreparationOutcome::TransientRefusal => {
                    super::WorkspaceSearchApply::TransientRefusal
                }
                SnapshotPreparationOutcome::Superseded => super::WorkspaceSearchApply::Superseded,
                SnapshotPreparationOutcome::Released => super::WorkspaceSearchApply::Released,
            };
        }
        #[cfg(test)]
        let force_apply_error =
            FORCE_DRIFT_APPLY_ERROR_ENGINE.load(Ordering::SeqCst) == Arc::as_ptr(shared) as usize;
        // A pass with nothing to apply must not touch the lease. The fence takes the
        // lock file, re-reads the record and restamps it through `checkpoint`, and all
        // three land inside the workspace when the cache sits under it — so an empty
        // pass would publish the very event that woke it and wake itself again.
        //
        // The fence is also where a superseded generation learns it lost the caches, so
        // this moves that discovery from "every wake" to "every wake that has work" — the
        // right place for it: a pass with nothing to write has nothing to publish over a
        // new owner, and it goes straight back to sleep.
        if plan.preparation_error.is_none() && plan.complete() {
            // Still epoch-checked, because "empty" is itself a verdict of the roots the
            // plan was prepared against: a file under a root registered since then maps
            // to no key yet, so the plan comes out empty and must be replanned, not
            // reported as nothing to do. Reading the epoch takes the engine lock, which
            // this pass takes anyway — what the early return avoids is the LEASE fence.
            // Anything other than a confirmed match falls through to the full path, whose
            // answer for a missing or poisoned engine is already the right one.
            let current = shared.lock().ok().and_then(|guard| {
                guard.as_ref().map(bsl_search::SearchEngine::workspace_roots_epoch)
            });
            if current == Some(plan.roots_epoch) {
                return super::WorkspaceSearchApply::Applied(true);
            }
        }
        let dirty_start = plan.dirty_cursor;
        let dirty_end =
            (dirty_start + bsl_search::WORKSPACE_APPLY_BATCH_ROWS).min(plan.dirty_keys.len());
        let mut remaining = bsl_search::WORKSPACE_APPLY_BATCH_ROWS - (dirty_end - dirty_start);
        let removed_start = plan.removed_cursor;
        let removed_end = (removed_start + remaining).min(plan.removed_keys.len());
        remaining -= removed_end - removed_start;
        let context_start = plan.context_cursor;
        let context_end = (context_start + remaining).min(plan.context_keys.len());

        let outcome = Self::apply_workspace_search(shared, lease, |engine| {
            if engine.workspace_roots_epoch() != plan.roots_epoch {
                return Err(bsl_search::SearchError::Index(
                    "workspace roots changed after search drift preparation".to_owned(),
                ));
            }
            if let Some(error) = &plan.preparation_error {
                return Err(bsl_search::SearchError::Index(error.clone()));
            }
            #[cfg(test)]
            if force_apply_error {
                return Err(bsl_search::SearchError::Index(
                    "forced drift apply failure".to_owned(),
                ));
            }
            let mut checkpoint = || std::ops::ControlFlow::Continue(());
            match engine.apply_prepared_workspace_drift_batch(
                &plan.dirty_keys[dirty_start..dirty_end],
                &plan.removed_keys[removed_start..removed_end],
                &plan.context_keys[context_start..context_end],
                &mut checkpoint,
            ) {
                std::ops::ControlFlow::Continue(result) => result,
                std::ops::ControlFlow::Break(()) => {
                    unreachable!("short publication checkpoint always continues")
                }
            }
        });
        match outcome {
            super::WorkspaceSearchApply::Applied(_) => {
                plan.dirty_cursor = dirty_end;
                plan.removed_cursor = removed_end;
                plan.context_cursor = context_end;
                super::WorkspaceSearchApply::Applied(plan.complete())
            }
            super::WorkspaceSearchApply::TransientRefusal => {
                super::WorkspaceSearchApply::TransientRefusal
            }
            super::WorkspaceSearchApply::Superseded => super::WorkspaceSearchApply::Superseded,
            super::WorkspaceSearchApply::Released => super::WorkspaceSearchApply::Released,
            super::WorkspaceSearchApply::OperationError(error) => {
                super::WorkspaceSearchApply::OperationError(error)
            }
        }
    }

    /// Reverse-look-up the workspace modules that READ any changed MDO, returning the graph's
    /// own absolute spelling of each. A metadata change alters the `graph_context` of every
    /// module that reads the object — not just its owned modules — and the persisted graph is
    /// the only record of who reads what.
    ///
    /// Absolute, because attributing a path to its root is one procedure and it lives on the
    /// root table: a caller that relativised the path itself would be a second one, and being
    /// able to strip only ONE root's prefix is exactly how the modules of every other root
    /// used to fall out of the result.
    ///
    /// The graph keeps these paths as strings, so under a root whose name holds bytes no
    /// `str` can carry they come back rendered: such a path attributes to nothing, or — with
    /// roots nested inside one another — to an ancestor, under whose key the mark normally
    /// finds no row. Either way the module itself waits for a wider mark, and that is the
    /// deliberate answer: a rendering fits several roots at once, and a key guessed from it
    /// would name a file that did not change.
    ///
    /// Queries the CURRENTLY PUBLISHED graph via [`GraphState::snapshot`], which gates on a
    /// published build and opens the read-only db off the graph's inner lock. Pre-drift edges
    /// are exactly right here: the set of referencing modules is defined by OTHER modules'
    /// bodies, which this `.xml` edit did not touch — the follow-up rebuild only re-renders the
    /// contexts marked here, it never changes who references the object. No published graph yet
    /// (or an `.xml` that maps to no MDO node — a form/command/config-root descriptor) → an
    /// empty set, so referencing marks are simply skipped and the owned marks + nudge still fire;
    /// a later publish consumes whatever marks then exist. Degrades, never blocks or errors.
    ///
    /// Off-lock throughout: opens the graph db once and runs one index-backed inbound-edge
    /// query per resolved MDO node id, so a batch of N `.xml` edits does at most N indexed
    /// queries, never a table scan.
    fn resolve_referencing_module_files(
        graph: &GraphState,
        xml_paths: &[PathBuf],
    ) -> ReferencingFilesOutcome {
        use crate::workspace_lease::{LeaseOperationError, LeaseOperationOutcome};

        let mut files = std::collections::HashSet::new();
        let mdo_ids: Vec<String> =
            xml_paths.iter().filter_map(|path| xml_to_mdo_id(path)).collect();
        if mdo_ids.is_empty() {
            return ReferencingFilesOutcome::Applied(files);
        }
        let snapshot = match graph.snapshot_blocking() {
            LeaseOperationOutcome::Applied(Some(snapshot)) => snapshot,
            LeaseOperationOutcome::Applied(None) => return ReferencingFilesOutcome::Applied(files),
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                crate::graph::BackgroundSnapshotError::Changed,
            )) => {
                return ReferencingFilesOutcome::OperationError(
                    "background graph snapshot changed during preparation".to_owned(),
                );
            }
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                crate::graph::BackgroundSnapshotError::Operation(error),
            )) => {
                return ReferencingFilesOutcome::OperationError(format!(
                    "background graph snapshot failed: {error}"
                ));
            }
            LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error)) => {
                return ReferencingFilesOutcome::OperationError(format!(
                    "background graph snapshot lease failed: {error}"
                ));
            }
            LeaseOperationOutcome::TransientRefusal => {
                return ReferencingFilesOutcome::TransientRefusal
            }
            LeaseOperationOutcome::Superseded => return ReferencingFilesOutcome::Superseded,
            LeaseOperationOutcome::Released => return ReferencingFilesOutcome::Released,
        };
        for mdo_id in mdo_ids {
            match snapshot.graph.referencing_files(&mdo_id) {
                Ok(found) => files.extend(found.into_iter().map(PathBuf::from)),
                Err(error) => {
                    return ReferencingFilesOutcome::OperationError(format!(
                        "referencing-files lookup failed for {mdo_id}: {error}"
                    ))
                }
            }
        }
        ReferencingFilesOutcome::Applied(files)
    }

    /// Re-mark every workspace `.bsl` dirty for the search overlay, then reconcile the
    /// store against what is actually on disk. Used when the change hub overflowed or a
    /// subtree was removed and the exact changed paths are no longer known, so the overlay
    /// must reconsider the whole tree. Marking alone only covers files that STILL exist; a
    /// file deleted during the lost window would keep its FTS rows and vectors forever, so
    /// the reconcile diffs the walked (present) set against the stored set and removes the
    /// gone paths. The walk covers EVERY registered root, through the shared source-set walk,
    /// and runs OUTSIDE the engine lock; the reconcile takes the lock only for its bounded
    /// O(stored) store writes.
    #[cfg(test)]
    fn rewalk_workspace_bsl_dirty(engine: &SharedSearchEngine) {
        let Some(declared) = Self::registered_roots(engine) else { return };
        let set = project_model::SourceSet::scan(&declared);
        let mut present: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for file in &set.files {
            if file.role != project_model::FileRole::Source {
                continue;
            }
            present.insert(file.walked.clone());
            Self::mark_search_path_dirty(engine, &file.walked);
        }
        let incomplete = !set.clean();
        #[cfg(test)]
        let incomplete =
            incomplete || FORCE_REWALK_WALK_ERROR.load(std::sync::atomic::Ordering::SeqCst);
        // An incomplete scan is NOT authoritative: `present` is missing healthy files, so
        // reconciling against it would delete them from the store. Marking the found files dirty
        // already happened above regardless.
        if incomplete {
            tracing::warn!(
                unreadable = set.unreadable,
                canonical_fallbacks = set.canonical_fallbacks,
                "search rescan walk incomplete; skipping reconcile to avoid deleting healthy files"
            );
            return;
        }
        if let Ok(mut guard) = engine.lock() {
            if let Some(engine) = guard.as_mut() {
                match engine.reconcile_workspace_files(&present) {
                    Ok(removed) if removed > 0 => {
                        tracing::info!(
                            removed,
                            "search rescan reconciled deleted files out of the index"
                        )
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("search rescan reconcile failed: {e}"),
                }
            }
        }
    }

    /// The declared spelling of every root the engine indexes, read under a brief lock so the
    /// walk itself runs with none held. Reading the table rather than a path captured at startup
    /// is what keeps the walk and the store's keys speaking of the same universe: a walk narrower
    /// than the table makes the reconcile below delete the roots it never visited.
    /// Test-side wrapper: production always states its exclusions, so the form that
    /// narrows by nothing is not reachable there by construction.
    #[cfg(test)]
    fn registered_roots(engine: &SharedSearchEngine) -> Option<Vec<PathBuf>> {
        Self::registered_roots_and_exclusions(engine).map(|(roots, _)| roots)
    }

    /// The registered roots together with the subtrees a walk of them must skip.
    ///
    /// Returned as a pair, and read under the one lock: a caller that fetched the roots
    /// and the holes separately could pair a fresh root set with a stale hole, and the
    /// walk would then read the cache of a workspace it no longer serves.
    fn registered_roots_and_exclusions(
        engine: &SharedSearchEngine,
    ) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
        let guard = engine.lock().ok()?;
        let engine = guard.as_ref()?;
        let roots = engine.workspace_roots()?;
        Some((
            roots.entries().map(|(_, declared)| declared.to_path_buf()).collect(),
            roots.excluded().to_vec(),
        ))
    }

    /// Reconcile the just-indexed workspace store against on-disk truth at BOOT, on the still-owned
    /// engine (no shared lock held), BEFORE the overlay-init decision is applied. A boot index step
    /// (`index_directory_deferred` / `index_directory_fts`, or a fused parse ingest) only re-ingests
    /// files that EXIST now — it never removes rows for a `.bsl` DELETED while the daemon was down.
    /// So a store row for a vanished file survives, and an [`OverlayInit::Clean`] — which asserts the
    /// store already equals the working tree — would serve that ghost forever. This walks the source
    /// tree (error-aware) and, on a CLEAN walk, calls [`SearchEngine::reconcile_workspace_files`] to
    /// remove every stored-but-gone path (tombstone + overlay dirty + incremental vector eviction —
    /// the same removal path the overflow rescan ships).
    ///
    /// Returns whether the store was PROVEN reconciled: `false` on any walk error OR a reconcile
    /// failure. A partial walk's `present` set is short, so trusting it would delete healthy rows —
    /// hence the S1 gate (skip reconcile on any walk error) is kept verbatim. And because a failed
    /// walk could not prove reconciliation, the caller must NOT stay Clean: it downgrades to a prime,
    /// whose own scan lazily hides files it finds missing. A prime's scan may itself be incomplete
    /// after a walk error, but a prime never ASSERTS a clean store the way `Clean` does — it only
    /// serves what it can see and hides the rest — so it is the strictly safer degraded default,
    /// matching the pre-existing behavior for a store that could not be reconciled.
    pub(super) fn reconcile_boot_store_with_disk_fenced(
        engine: &mut SearchEngine,
        lease: &crate::workspace_lease::WorkspaceLease,
    ) -> Option<bool> {
        let Some(roots) = engine.workspace_roots() else { return Some(false) };
        let declared: Vec<PathBuf> =
            roots.entries().map(|(_, declared)| declared.to_path_buf()).collect();
        let excluded = roots.excluded().to_vec();
        let set = project_model::SourceSet::scan_excluding(&declared, &excluded);
        let present: std::collections::HashSet<PathBuf> = set
            .files
            .iter()
            .filter(|file| file.role == project_model::FileRole::Source)
            .map(|file| file.walked.clone())
            .collect();
        let incomplete = !set.clean();
        #[cfg(test)]
        let incomplete =
            incomplete || FORCE_REWALK_WALK_ERROR.load(std::sync::atomic::Ordering::SeqCst);
        if incomplete {
            tracing::warn!(
                unreadable = set.unreadable,
                canonical_fallbacks = set.canonical_fallbacks,
                "search boot reconcile walk incomplete; priming the overlay instead of clean-init"
            );
            return Some(false);
        }
        match engine
            .reconcile_workspace_files_fenced(&present, |apply| Self::startup_apply(lease, apply))
        {
            Ok(bsl_search::FenceOutcome::Applied(removed)) => {
                if removed > 0 {
                    tracing::info!(
                        removed,
                        "search boot reconciled deleted files out of the store"
                    );
                }
                Some(true)
            }
            Ok(bsl_search::FenceOutcome::Superseded | bsl_search::FenceOutcome::Released) => None,
            Ok(bsl_search::FenceOutcome::TransientRefusal) => {
                unreachable!("startup_apply retries transient refusals")
            }
            Err(e) => {
                tracing::warn!("search boot reconcile failed; priming the overlay instead: {e}");
                Some(false)
            }
        }
    }
    /// Mark one path dirty in the search overlay if it is a `.bsl` file. Filtering
    /// on the consumer side keeps the hub itself extension-agnostic.
    #[cfg(test)]
    fn mark_search_path_dirty(engine: &SharedSearchEngine, path: &Path) {
        if !project_model::is_bsl_source_path(path) {
            return;
        }
        if let Ok(guard) = engine.lock() {
            if let Some(engine) = guard.as_ref() {
                if let Err(e) = engine.mark_workspace_path_dirty(path) {
                    tracing::warn!(path = ?path, "failed to mark workspace file dirty: {e}");
                }
            }
        }
    }
}

/// Prefetch resident snapshots for the overlay's dirty paths and feed them into the
/// incremental reindex, so a following query serves chunks cut from the SHARED resident
/// parse instead of a second disk read+parse. Called at the top of a code-search request,
/// before the query acquires the engine lock.
///
/// Bounded to [`MAX_RESIDENT_PREFETCH_PATHS_PER_QUERY`] paths per call.
///
/// Lock discipline: the resident read must never overlap the engine lock. So this
/// reads the dirty-path list and the source handle under a brief engine lock, RELEASES it,
/// fetches the snapshots with NO lock held, then applies them under a second brief engine
/// lock that only touches the overlay cache (never the resident). A resident that is
/// absent/loading, or a path it cannot serve, is simply missing from the map and the
/// reindex disk-reads it — so search never regresses when the resident is unavailable.
#[cfg(test)]
pub(super) fn prefetch_resident_overlay(
    engine: &SharedSearchEngine,
    lease: &crate::workspace_lease::WorkspaceLease,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), crate::tools::search::Withdrawn> {
    use crate::tools::search::{try_acquire_engine, AcquireFailure, Withdrawn};

    // Once terminal, every later query is a pure resident-snapshot read. This atomic check avoids
    // resident work and, importantly, never reopens the lease file after supersession is latched.
    if lease.is_superseded() || lease.is_released() {
        return Ok(());
    }
    let (source, roots, dirty) = {
        // The request's own acquire, not a plain `lock()`: this runs BEFORE the search's own
        // acquire, and a cancelled request must not park here behind a long holder. A stall
        // or a poisoned lock skips the prefetch — the search's acquire reports it.
        let guard = match try_acquire_engine(engine, cancel) {
            Ok(guard) => guard,
            Err(AcquireFailure::Cancelled) => return Err(Withdrawn),
            Err(AcquireFailure::Poisoned | AcquireFailure::TimedOut) => return Ok(()),
        };
        let Some(engine) = guard.as_ref() else { return Ok(()) };
        let Some(source) = engine.module_snapshot_source() else { return Ok(()) };
        // The overlay keys dirty files by (root, path relative to that root);
        // resolving them for the resident needs the same table.
        let Some(roots) = engine.workspace_roots().cloned() else {
            return Ok(());
        };
        match engine.workspace_overlay_dirty_paths() {
            Ok(dirty) => (source, roots, dirty),
            Err(e) => {
                tracing::debug!("overlay dirty-path read failed: {e}");
                return Ok(());
            }
        }
    };
    if dirty.is_empty() {
        return Ok(());
    }

    if cancel.is_cancelled() {
        return Err(Withdrawn);
    }
    // Search and diagnostics drain independent hub cursors and a query never polls drift on
    // its own, so the resident is usually BEHIND disk on the just-edited files. Reconcile
    // pending drift FIRST — off the engine lock, resident lock only (I3 holds) — so the
    // snapshot text below matches disk and the byte-compare hits instead of falling back to a
    // disk read. A resident rebuild in flight is skipped inside the drain, never blocking here.
    source.catch_up();

    // Resident reads run OFF the engine lock. The `!Send` parses stay in this local map on
    // the calling thread and never cross a thread or an await boundary.
    let mut snapshots: std::collections::HashMap<bsl_search::FileKey, bsl_search::ModuleSnapshot> =
        std::collections::HashMap::new();
    // Cap the per-query resident prefetch: a branch switch can dirty thousands of paths, and
    // fetching+reindexing them all on the query thread would be unbounded work. Serve at most
    // this many from the shared parse per query; the remainder STAY dirty and are picked up by
    // the query's own lazy disk refresh and by later queries' prefetches. The cap is the whole
    // budget — no separate time budget needed.
    for key in dirty.iter().take(MAX_RESIDENT_PREFETCH_PATHS_PER_QUERY) {
        // Per path: each resident read is a synchronous parse, and a cancelled request
        // must not pay for the rest of the batch. What is left stays dirty for the next
        // query, exactly as the cap leaves it.
        if cancel.is_cancelled() {
            return Err(Withdrawn);
        }
        // Resolve the dirty key to an ABSOLUTE path through its own root before handing it to
        // the resident: the resident is indexed under the OUTER workspace root, so a bare
        // root-relative path would be re-joined against that root and silently miss on every
        // nested config — and on every extension. The map stays keyed by the store key, which
        // is what `reindex_dirty_from_snapshots` looks up.
        let Some(abs_path) = roots.resolve(key) else {
            continue;
        };
        if let bsl_search::SnapshotFetch::Fetched(snapshot) =
            source.text_and_parse(&abs_path.to_string_lossy())
        {
            snapshots.insert(key.clone(), snapshot);
        }
    }
    if snapshots.is_empty() {
        return Ok(());
    }

    let mut guard = match try_acquire_engine(engine, cancel) {
        Ok(guard) => guard,
        Err(AcquireFailure::Cancelled) => return Err(Withdrawn),
        // Skipped, not failed: the fetched snapshots stay dirty for the next query. Logged
        // all the same — a poisoned lock is the trace of an earlier panic, and this was the
        // one place on the request path that used to record it.
        Err(AcquireFailure::Poisoned) => {
            tracing::debug!("resident-fed overlay reindex skipped: engine lock poisoned");
            return Ok(());
        }
        Err(AcquireFailure::TimedOut) => {
            tracing::debug!("resident-fed overlay reindex skipped: engine lock held past the cap");
            return Ok(());
        }
    };
    if let super::WorkspaceSearchApply::OperationError(error) =
        SharedState::apply_to_engine(&mut guard, lease, |engine, _checkpoint| {
            std::ops::ControlFlow::Continue(engine.reindex_dirty_from_snapshots(&snapshots))
        })
    {
        tracing::debug!("resident-fed overlay reindex failed: {error}");
    }
    Ok(())
}

/// The owned-module subtree of a metadata descriptor `.xml`: `<Dir>/<Name>/` beside a
/// `<Dir>/<Name>.xml`, when that directory exists. Every `.bsl` under it (object /
/// manager / recordset / form / command modules, or a common-module / service body) is
/// owned by the object the descriptor defines — so the path convention covers ordinary
/// MDOs (which carry no substrate back-link) and common-modules/services alike, with no
/// resident lookup and no resident/engine lock coupling.
fn owned_module_subtree(xml: &Path) -> Option<PathBuf> {
    let stem = xml.file_stem()?;
    let subtree = xml.parent()?.join(stem);
    subtree.is_dir().then_some(subtree)
}

/// Every `.bsl` file under `dir`.
fn walk_bsl_files(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .filter(|p| project_model::is_bsl_source_path(p))
        .collect()
}

/// Map a metadata descriptor `.xml` at `<KindPlural>/<Name>.xml` to its graph MDO node id
/// `mdo/<EnglishType>/<Name>` (the id the fused build encodes, verified against
/// `ide::GraphRowEncoder`). `None` when the parent directory is not a known metadata-kind
/// plural — a form/command descriptor, an `Ext/…` file, or a configuration-root descriptor —
/// since those carry no `mdo/` node and thus no inbound read edges to reverse-look-up. The
/// `<KindPlural>` → [`bsl_metadata::MdoType`] mapping reuses the canonical
/// [`bsl_metadata::MdoType::from_plural`] table rather than duplicating a directory map.
fn xml_to_mdo_id(xml: &Path) -> Option<String> {
    let name = xml.file_stem()?.to_str()?;
    let kind_dir = xml.parent()?.file_name()?.to_str()?;
    let mdo_type = bsl_metadata::MdoType::from_plural(kind_dir)?;
    Some(format!("mdo/{}/{name}", mdo_type.english_name()))
}

/// Whether `xml` is the root descriptor of a source tree — `Configuration.xml`,
/// `ConfigDumpInfo.xml`, a plugin's own root descriptor. Such a change can shift ANY
/// module's context, so it is answered conservatively with a whole-collection mark rather
/// than a resolvable owned subtree.
///
/// Three independent signs, because no one of them covers the class:
///
/// - the path attributes to the TOP LEVEL of a registered root. Ranked by both spellings,
///   so an aliased delivery answers the same as a canonical one; and it is the only sign
///   that recognises a root which is not a 1C dump at all and therefore carries no
///   `Configuration.xml`;
/// - the descriptor's own directory CONTAINS a `Configuration.xml` — the same disk probe
///   by which the project model tells an extension from an ordinary directory. The root
///   table deliberately omits the roots it rejected (one inside the configuration, one
///   whose identifier was taken), and a tree nobody declared is not in it either, so a
///   question asked of the table alone leaves their descriptors unrecognised;
/// - the file's own name is `Configuration.xml`. What is left when the descriptor itself
///   is what vanished: the neighbour the sign above looks for is the file now gone.
fn is_root_descriptor(roots: Option<&bsl_search::WorkspaceRoots>, xml: &Path) -> bool {
    let at_root_of_a_registered_root = roots
        .and_then(|roots| roots.key_of_path(xml))
        .is_some_and(|key| Path::new(&key.path).components().count() == 1);
    if at_root_of_a_registered_root {
        return true;
    }
    let beside_a_configuration_xml = xml.parent().is_some_and(|dir| {
        bsl_conventions::find_child_ci(
            dir,
            bsl_conventions::ConventionalName::ConfigurationXml.canonical(),
        )
        .is_some_and(|found| found.is_file())
    });
    beside_a_configuration_xml
        || xml.file_name().and_then(|n| n.to_str()).and_then(bsl_conventions::conventional_of)
            == Some(bsl_conventions::ConventionalName::ConfigurationXml)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        write_common_module, write_common_module_tree, EnvVarGuard, ENV_LOCK,
    };
    use super::{
        SearchDriftPlan, SharedState, SnapshotPreparationOutcome, FORCE_REWALK_WALK_ERROR,
        MAX_RESIDENT_PREFETCH_PATHS_PER_QUERY,
    };
    use crate::state::types::OverlayInit;
    use bsl_search::{IndexedDocument, SearchEngine};
    use std::fs;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[test]
    fn continuous_events_do_not_reset_rescan_debt_backoff() {
        let start = std::time::Instant::now();
        let mut debt = super::RescanDebt::default();
        debt.record_failure(start);
        assert!(!debt.waiting(start), "the first retry is immediate");

        debt.record_failure(start);
        let next = debt.next_allowed;
        for offset in 1..30 {
            assert!(debt.waiting(start + std::time::Duration::from_secs(offset)));
            assert_eq!(debt.next_allowed, next, "fresh wakeups do not move the deadline");
        }
        assert!(!debt.waiting(start + std::time::Duration::from_secs(30)));

        debt.clear();
        assert!(!debt.required(), "a converged full rescan retires the one debt slot");
    }

    /// The idle pass is the hot one: with the cache under the watched tree, every
    /// lock take and every lease restamp it performs is an event that wakes it again.
    /// The observable is the lock file, not the lease record — the record carries a
    /// whole-second stamp, so a rewrite inside the same second leaves it byte-identical
    /// and an assertion on its contents would hold over a pass that did write.
    #[test]
    fn an_empty_drift_plan_never_takes_the_lease_fence() {
        let dir = tempdir().unwrap();
        let shared: super::super::SharedSearchEngine = Arc::new(Mutex::new(Some(
            SearchEngine::fts_only(&dir.path().join("search.db")).unwrap(),
        )));
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let lock = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path()).lease_lock_path();
        std::fs::remove_file(&lock).unwrap();

        let mut empty = SearchDriftPlan::default();
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &lease,
                &mut empty,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::Applied(true)
        ));
        assert!(!lock.exists(), "an empty plan took the lease fence");

        // Positive control: a plan with work must still take it, or the assertion
        // above would hold on an apply that does nothing at all.
        let mut work = SearchDriftPlan {
            dirty_keys: vec![bsl_search::FileKey::configuration("src/a.bsl")],
            ..Default::default()
        };
        SharedState::apply_prepared_search_drift(
            &shared,
            &lease,
            &mut work,
            &crate::graph::GraphState::disabled(),
        );
        assert!(lock.exists(), "a plan with work skipped the lease fence");
    }

    #[test]
    fn superseded_daemon_cannot_mutate_shared_search() {
        struct Provider;
        impl bsl_search::GraphContextProvider for Provider {
            fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
                Some("graph".to_owned())
            }
        }

        #[derive(Clone, Copy, Debug)]
        enum Family {
            Watcher,
            Dirty,
            Context,
            Delete,
            Subtree,
            Reconcile,
            Resident,
            Roots,
            Provider,
            ContextRefresh,
        }

        for family in [
            Family::Watcher,
            Family::Dirty,
            Family::Context,
            Family::Delete,
            Family::Subtree,
            Family::Reconcile,
            Family::Resident,
            Family::Roots,
            Family::Provider,
            Family::ContextRefresh,
        ] {
            let dir = tempdir().unwrap();
            let source = dir.path().join("Module.bsl");
            fs::write(&source, "Процедура П()\nКонецПроцедуры").unwrap();
            let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
            engine.set_workspace_root(dir.path().to_path_buf());
            engine.index_directory_fts(dir.path()).unwrap();
            let shared: super::super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
            let old = crate::workspace_lease::WorkspaceLease::claim(dir.path());
            let _newer = crate::workspace_lease::WorkspaceLease::claim(dir.path());
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let calls_in_apply = Arc::clone(&calls);
            let empty = std::collections::HashSet::new();
            let prefixes = vec![dir.path().to_path_buf()];

            let outcome = SharedState::apply_workspace_search(&shared, &old, |engine| {
                calls_in_apply.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match family {
                    Family::Watcher => engine.enable_workspace_watcher_mode(),
                    Family::Dirty => {
                        engine.mark_workspace_path_dirty(&source).unwrap();
                    }
                    Family::Context => {
                        engine.mark_workspace_context_dirty().unwrap();
                    }
                    Family::Delete => {
                        engine.remove_workspace_path(&source).unwrap();
                    }
                    Family::Subtree => {
                        engine.remove_vanished_under(&prefixes).unwrap();
                    }
                    Family::Reconcile => {
                        engine.reconcile_workspace_files(&empty).unwrap();
                    }
                    Family::Resident => {
                        engine.reindex_dirty_from_snapshots(&Default::default()).unwrap();
                    }
                    Family::Roots => {
                        engine
                            .initialize_workspace_roots(
                                bsl_search::WorkspaceRoots::build(dir.path(), dir.path(), &[]).0,
                            )
                            .unwrap();
                    }
                    Family::Provider => {
                        engine
                            .replace_published_graph_context_provider(Arc::new(Provider))
                            .unwrap();
                    }
                    Family::ContextRefresh => {
                        engine.refresh_dirty_contexts(&Provider, i64::MAX).unwrap();
                    }
                }
                Ok::<_, bsl_search::SearchError>(())
            });
            assert!(
                matches!(outcome, crate::state::WorkspaceSearchApply::Superseded),
                "{family:?}"
            );
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "{family:?}");
            let guard = shared.lock().unwrap();
            let engine = guard.as_ref().unwrap();
            assert_eq!(engine.file_count().unwrap(), 1, "{family:?}");
            assert!(engine.workspace_overlay_dirty_paths_snapshot().unwrap().is_empty());
            assert!(engine.context_dirty_paths("code").unwrap().is_empty());
        }

        let dir = tempdir().unwrap();
        let shared: super::super::SharedSearchEngine = Arc::new(Mutex::new(Some(
            SearchEngine::fts_only(&dir.path().join("search.db")).unwrap(),
        )));
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let held = lease.hold_file_lock_for_test();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        assert!(matches!(
            SharedState::apply_workspace_search(&shared, &lease, |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
            crate::state::WorkspaceSearchApply::TransientRefusal
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(held);
        assert!(matches!(
            SharedState::apply_workspace_search(&shared, &lease, |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
            crate::state::WorkspaceSearchApply::Applied(())
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let old = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let _newer = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let mut plan = SearchDriftPlan {
            // Non-empty on purpose: an idle plan never reaches the fence, and this
            // asserts what the FENCE does when the lease has been taken over.
            dirty_keys: vec![bsl_search::FileKey::configuration("src/a.bsl")],
            ..Default::default()
        };
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &old,
                &mut plan,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::Superseded
        ));

        let retry_lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let _force_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        super::FORCE_DRIFT_APPLY_ERROR_ENGINE
            .store(Arc::as_ptr(&shared) as usize, std::sync::atomic::Ordering::SeqCst);
        let mut failing_plan = SearchDriftPlan {
            dirty_keys: vec![bsl_search::FileKey::configuration("src/a.bsl")],
            ..Default::default()
        };
        let failed = SharedState::apply_prepared_search_drift(
            &shared,
            &retry_lease,
            &mut failing_plan,
            &crate::graph::GraphState::disabled(),
        );
        super::FORCE_DRIFT_APPLY_ERROR_ENGINE.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(failed, crate::state::WorkspaceSearchApply::OperationError(_)));
    }

    #[test]
    fn drift_apply_keeps_its_cursor_on_refusal_and_advances_in_bounded_slices() {
        let dir = tempdir().unwrap();
        let shared: super::super::SharedSearchEngine = Arc::new(Mutex::new(Some(
            SearchEngine::fts_only(&dir.path().join("search.db")).unwrap(),
        )));
        let lease = crate::workspace_lease::WorkspaceLease::claim(dir.path());
        let mut plan = SearchDriftPlan {
            dirty_keys: (0..=bsl_search::WORKSPACE_APPLY_BATCH_ROWS)
                .map(|index| bsl_search::FileKey::configuration(format!("P{index}.bsl")))
                .collect(),
            ..Default::default()
        };

        let held = lease.hold_file_lock_for_test();
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &lease,
                &mut plan,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::TransientRefusal
        ));
        assert_eq!(plan.dirty_cursor, 0, "a refused fence consumes none of the plan");
        drop(held);

        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &lease,
                &mut plan,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::Applied(false)
        ));
        assert_eq!(plan.dirty_cursor, bsl_search::WORKSPACE_APPLY_BATCH_ROWS);
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &lease,
                &mut plan,
                &crate::graph::GraphState::disabled(),
            ),
            crate::state::WorkspaceSearchApply::Applied(true)
        ));
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_dirty_paths_snapshot()
                .unwrap()
                .len(),
            bsl_search::WORKSPACE_APPLY_BATCH_ROWS + 1
        );
    }

    #[test]
    fn durable_drift_error_advances_cursor_and_coalesces_debt() {
        use crate::change_hub::{test_support::eventually, WorkspaceChangeHub};
        use std::time::Duration;

        struct ResetForcedError;
        impl Drop for ResetForcedError {
            fn drop(&mut self) {
                super::FORCE_DRIFT_APPLY_ERROR_ENGINE.store(0, std::sync::atomic::Ordering::SeqCst);
                super::FORCE_RESCAN_DEBT_DUE.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _reset = ResetForcedError;
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let a = workspace.join("A.bsl");
        fs::write(&a, "Procedure Old()\nEndProcedure").unwrap();

        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        let shared: super::super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let cursor = hub.subscribe();
        let lease = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&shared),
            graph.clone(),
            None,
            Arc::new(AtomicU64::new(0)),
            lease.clone(),
        ));

        let write_and_wait = |path: &std::path::Path, text: &str| {
            let before = hub.events_seen();
            fs::write(path, text).unwrap();
            assert!(eventually(Duration::from_secs(5), || hub.events_seen() > before));
            assert!(eventually(Duration::from_secs(15), || {
                hub.materialize(cursor).entries.is_empty()
            }));
        };

        super::FORCE_DRIFT_APPLY_ERROR_ENGINE
            .store(Arc::as_ptr(&shared) as usize, std::sync::atomic::Ordering::SeqCst);
        write_and_wait(&workspace.join("Configuration.xml"), "<Configuration/>");
        assert_ne!(
            graph.status(),
            crate::graph::GraphStatus::Idle,
            "the graph nudge survives a search OperationError"
        );
        write_and_wait(&workspace.join("B.bsl"), "Procedure B()\nEndProcedure");
        fs::remove_file(&a).unwrap();
        write_and_wait(&a, "Procedure New()\nEndProcedure");
        assert!(
            shared
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_dirty_paths_snapshot()
                .unwrap()
                .is_empty(),
            "durable errors advance without pretending their writes applied"
        );

        super::FORCE_RESCAN_DEBT_DUE.store(true, std::sync::atomic::Ordering::SeqCst);
        write_and_wait(&workspace.join("C.bsl"), "Procedure C()\nEndProcedure");
        assert!(
            shared
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .workspace_overlay_dirty_paths_snapshot()
                .unwrap()
                .is_empty(),
            "an OperationError during the due full rescan keeps one debt slot without partial apply"
        );
        super::FORCE_DRIFT_APPLY_ERROR_ENGINE.store(0, std::sync::atomic::Ordering::SeqCst);
        write_and_wait(&workspace.join("C.bsl"), "Procedure C2()\nEndProcedure");
        assert!(
            eventually(Duration::from_secs(15), || {
                let recovered = shared
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .workspace_overlay_dirty_paths_snapshot()
                    .unwrap();
                ["A.bsl", "B.bsl", "C.bsl"]
                    .into_iter()
                    .all(|path| recovered.contains_key(&bsl_search::FileKey::configuration(path)))
            }),
            "the one recovery rescan converges to the final disk state"
        );

        lease.release();
        hub.shutdown();
        shared.lock().unwrap().as_ref().unwrap().initialize_workspace_overlay_clean().unwrap();
        let next_hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(next_hub.wait_until_watching(Duration::from_secs(5)));
        let next_cursor = next_hub.subscribe();
        let next_lease = crate::workspace_lease::WorkspaceLease::claim(&workspace);
        assert!(SharedState::spawn_search_sink(
            next_hub.clone(),
            next_cursor,
            Arc::clone(&shared),
            graph,
            None,
            Arc::new(AtomicU64::new(0)),
            next_lease.clone(),
        ));
        let before = next_hub.events_seen();
        fs::write(workspace.join("D.bsl"), "Procedure D()\nEndProcedure").unwrap();
        assert!(eventually(Duration::from_secs(5), || next_hub.events_seen() > before));
        assert!(
            eventually(Duration::from_secs(15), || {
                shared
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .workspace_overlay_dirty_paths_snapshot()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    == vec![bsl_search::FileKey::configuration("D.bsl")]
            }),
            "multiple failures coalesce into one rescan slot; the next change stays incremental"
        );

        next_lease.release();
        next_hub.shutdown();
    }

    #[test]
    fn failed_search_marking_still_sends_topology_nudges() {
        durable_drift_error_advances_cursor_and_coalesces_debt();
    }

    #[test]
    fn rescan_debt_uses_current_disk_after_recreate() {
        durable_drift_error_advances_cursor_and_coalesces_debt();
    }

    #[test]
    fn root_transition_epoch_ignores_unrelated_files_and_tracks_keyspace_drift() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let graph = crate::graph::GraphState::for_workspace(root.clone());
        let entry = |path: std::path::PathBuf, kind| crate::change_hub::ChangeEntry {
            canonical: path.clone(),
            raw: path,
            kind,
            seq: 1,
        };

        assert!(!SharedState::root_transition_relevant_drift(
            &[entry(root.join("notes.txt"), crate::change_hub::ChangeKind::MaybeChanged)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("notes.txt"), crate::change_hub::ChangeKind::MaybeRemoved)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("Module.bsl"), crate::change_hub::ChangeKind::MaybeChanged)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("Configuration.xml"), crate::change_hub::ChangeKind::MaybeChanged,)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("Sub.v1"), crate::change_hub::ChangeKind::MaybeRemoved)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("bsl-analyzer.toml"), crate::change_hub::ChangeKind::MaybeChanged,)],
            false,
            &graph,
        ));
        assert!(SharedState::root_transition_relevant_drift(
            &[entry(root.join("gone"), crate::change_hub::ChangeKind::SubtreeRemoved)],
            false,
            &graph,
        ));
    }

    #[test]
    fn drift_keys_are_replanned_after_workspace_roots_change() {
        let dir = tempdir().unwrap();
        let configuration = dir.path().join("cf");
        let extension = dir.path().join("ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let file = extension.join("Module.bsl");
        fs::write(&file, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine
            .initialize_workspace_roots(
                bsl_search::WorkspaceRoots::build(dir.path(), &configuration, &[]).0,
            )
            .unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let shared: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let entry = crate::change_hub::ChangeEntry {
            canonical: file.clone(),
            raw: file.clone(),
            kind: crate::change_hub::ChangeKind::MaybeChanged,
            seq: 1,
        };
        let graph = crate::graph::GraphState::disabled();
        let mut plan =
            SharedState::prepare_search_drift(&shared, std::slice::from_ref(&entry), false, &graph);

        let expected_key = {
            let mut guard = shared.lock().unwrap();
            let engine = guard.as_mut().unwrap();
            engine.set_workspace_roots(
                bsl_search::WorkspaceRoots::build(
                    dir.path(),
                    &configuration,
                    std::slice::from_ref(&extension),
                )
                .0,
            );
            engine.workspace_file_key(&file).unwrap()
        };

        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                &mut plan,
                &graph,
            ),
            crate::state::WorkspaceSearchApply::OperationError(_)
        ));
        let mut replanned =
            SharedState::prepare_search_drift(&shared, std::slice::from_ref(&entry), false, &graph);
        assert!(matches!(
            SharedState::apply_prepared_search_drift(
                &shared,
                &crate::workspace_lease::WorkspaceLease::unmanaged(),
                &mut replanned,
                &graph,
            ),
            crate::state::WorkspaceSearchApply::Applied(true)
        ));
        assert!(shared
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_overlay_dirty_paths_snapshot()
            .unwrap()
            .contains_key(&expected_key));
    }

    /// Entering the event stream costs the overlay nothing. The window a reconcile used to
    /// pay for is not a window any more: the baseline is taken after the watch is up and the
    /// cursor is older than both, so there is nothing for a rescan to recover — and a rescan
    /// is not cheap. It re-walks every root, canonicalizes and stats every file, and marks
    /// them all dirty, which a later refresh pays for by reading each one off disk in full.
    #[test]
    fn a_boot_entering_event_mode_does_not_rescan() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        // Written BEFORE the watch arms, so no event ever reports it: whatever ends up in the
        // dirty set got there from a rescan and from nothing else.
        fs::write(workspace.join("Module.bsl"), "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let (hub, hold) =
            WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(workspace.clone())]);
        let cursor = hub.subscribe();
        hold.release();
        assert!(hub.wait_until_watching(Duration::from_secs(5)), "the watch must arm");
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&engine_arc),
            crate::graph::GraphState::disabled(),
            None,
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        ));
        std::thread::sleep(Duration::from_millis(500));

        let snapshot = {
            let guard = engine_arc.lock().unwrap();
            guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
        };
        assert!(
            snapshot.is_empty(),
            "a start that observed nothing must cost the overlay nothing: {snapshot:?}",
        );
    }

    /// The cursor is subscribed before the boot reads disk and long before there is an engine
    /// to feed, so changes landing in between are not lost — they wait in the accumulator
    /// until a sink exists to drain them. The sink applies everything to a published engine
    /// or it does not exist: a drain into an absent engine no-ops path by path and the batch
    /// is gone for good.
    #[test]
    fn an_event_before_the_engine_exists_still_reaches_the_overlay() {
        use crate::change_hub::WorkspaceChangeHub;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)), "the watch must arm");
        let cursor = hub.subscribe();

        // Happens while the boot would still be reading disk: no engine yet, and the only
        // record of it is the cursor's own backlog.
        fs::write(workspace.join("Module.bsl"), "Процедура П()\nКонецПроцедуры").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while hub.events_seen() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(hub.events_seen() > 0, "the hub observed the write");

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&engine_arc),
            crate::graph::GraphState::disabled(),
            None,
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        ));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut delivered = false;
        while Instant::now() < deadline {
            let snapshot = {
                let guard = engine_arc.lock().unwrap();
                guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
            };
            if snapshot.keys().any(|key| key.path.ends_with("Module.bsl")) {
                delivered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(delivered, "a change that predates the engine is still the overlay's to apply");
    }

    /// A workspace whose initial walk outlasts one slice of patience is an ordinary large
    /// configuration, not a failure — and a boot that gave up on it would leave the search
    /// overlay in scan mode for the whole life of the daemon.
    #[test]
    fn the_boot_keeps_waiting_while_the_hub_is_still_starting() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        let releaser = {
            let hold = hold.shared();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                hold.release();
            })
        };
        let started = Instant::now();
        let armed = SharedState::await_watch(
            &hub,
            super::WatchWaitPolicy::new(Duration::from_millis(5), Duration::from_secs(30)),
        );
        let waited = started.elapsed();
        releaser.join().unwrap();

        assert!(armed, "a hub that is merely slow to start must not be given up on");
        assert!(
            waited >= Duration::from_millis(100),
            "the wait has to have crossed several expired slices to prove it resumed: {waited:?}"
        );
    }

    /// The budget is what bounds the wait — not a count of slices, which is what a naive
    /// "try twice" implementation would bound it by and which no test of mere termination
    /// can tell apart. Measured with a slice far shorter than the budget, so an
    /// implementation stopping after any small number of slices returns far too early.
    #[test]
    fn the_boot_gives_up_on_the_budget_and_not_before() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let (hub, hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        let started = Instant::now();
        let armed = SharedState::await_watch(
            &hub,
            super::WatchWaitPolicy::new(Duration::from_millis(5), Duration::from_millis(400)),
        );
        let waited = started.elapsed();
        hold.release();

        assert!(!armed, "a hub that never arms must not hold the thread for ever");
        assert!(
            waited >= Duration::from_millis(400),
            "giving up before the budget abandons a workspace that was merely slow: {waited:?}"
        );
        assert!(waited < Duration::from_secs(10), "and it must give up: {waited:?}");
    }

    /// The budget is a ceiling on the whole wait, so the last slice is cut to whatever is
    /// left of it. Asked for a full slice at the very end of one, the hub answers a slice
    /// past the deadline the caller was promised — a minute, at the production slice, which
    /// reads exactly like a hub that is still arming.
    #[test]
    fn the_wait_never_overshoots_its_budget_by_a_slice() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let (hub, _hold) = WorkspaceChangeHub::start_targets_held(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        let started = Instant::now();
        let armed = SharedState::await_watch(
            &hub,
            super::WatchWaitPolicy::new(Duration::from_millis(400), Duration::from_millis(20)),
        );
        let waited = started.elapsed();

        assert!(!armed, "the hub is held short of arming");
        assert!(
            waited < Duration::from_millis(200),
            "a budget of 20ms must not be spent as a 400ms slice: {waited:?}"
        );
    }

    /// A permanent failure is answered at once. Waiting out a ten-minute budget over a hub
    /// that has already said it will never arm only delays a boot that has to happen anyway.
    #[test]
    fn the_boot_does_not_wait_out_a_permanent_failure() {
        use crate::change_hub::{WatchTarget, WorkspaceChangeHub};
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let hub = WorkspaceChangeHub::start_with_unstartable_thread(vec![WatchTarget::recursive(
            dir.path().to_path_buf(),
        )]);

        let started = Instant::now();
        assert!(!SharedState::await_watch(
            &hub,
            super::WatchWaitPolicy::new(Duration::from_millis(50), Duration::from_secs(600))
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a permanent failure is not something to spend a budget on"
        );
    }

    #[test]
    fn search_sink_marks_only_bsl_paths_dirty() {
        use crate::change_hub::WorkspaceChangeHub;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let hub = WorkspaceChangeHub::start(vec![workspace.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)), "the watch must arm");
        // A second cursor observes the raw accumulator independently of the sink.
        let observer = hub.subscribe();

        // Subscribed by the caller, as the boot does: the sink is handed a cursor that
        // already covers everything from here on.
        let cursor = hub.subscribe();
        assert!(SharedState::spawn_search_sink(
            hub.clone(),
            cursor,
            Arc::clone(&engine_arc),
            crate::graph::GraphState::disabled(),
            None,
            Arc::new(AtomicU64::new(0)),
            crate::workspace_lease::WorkspaceLease::unmanaged(),
        ));

        let bsl = workspace.join("Module.bsl");
        std::fs::write(&bsl, "Процедура П()\nКонецПроцедуры").unwrap();
        let xml = workspace.join("Configuration.xml");
        std::fs::write(&xml, "<Configuration/>").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut dirty_has_bsl = false;
        while Instant::now() < deadline {
            let snapshot = {
                let guard = engine_arc.lock().unwrap();
                guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
            };
            if snapshot.keys().any(|key| key.path.ends_with("Module.bsl")) {
                dirty_has_bsl = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(dirty_has_bsl, "the .bsl change is marked dirty for the search overlay");

        let snapshot = {
            let guard = engine_arc.lock().unwrap();
            guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
        };
        assert!(
            !snapshot.keys().any(|key| key.path.ends_with("Configuration.xml")),
            "search ignores non-.bsl paths",
        );
        let watcher_mode = {
            let guard = engine_arc.lock().unwrap();
            guard.as_ref().unwrap().workspace_overlay_stats().unwrap().unwrap().watcher_mode
        };
        assert!(watcher_mode, "a running sink is what puts the overlay into watcher mode");

        // The hub itself accepted the .xml change; only the consumer filtered it.
        // The event is asynchronous, so poll the observer cursor until it lands.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observer = observer;
        let mut saw_xml = false;
        while Instant::now() < deadline {
            let batch = hub.drain(observer);
            observer = batch.cursor;
            if batch.entries.iter().any(|e| e.raw.ends_with("Configuration.xml")) {
                saw_xml = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(saw_xml, "the accumulator carries the .xml change for other consumers");
    }

    /// On a hub overflow the exact changed paths are lost, so the sink re-walks the
    /// workspace and marks every `.bsl` dirty (and nothing else), restoring the
    /// old unbounded watcher's guarantee that no `.bsl` change is dropped.
    #[test]
    fn search_sink_rewalks_all_bsl_on_overflow() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        // Watcher mode makes `mark_workspace_path_dirty` record into the dirty set.
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        // A nested tree of `.bsl` plus a non-`.bsl` file that must NOT be marked.
        let nested = workspace.join("CommonModules").join("Модуль");
        fs::create_dir_all(&nested).unwrap();
        let a = workspace.join("A.bsl");
        let b = nested.join("B.bsl");
        fs::write(&a, "Процедура П()\nКонецПроцедуры").unwrap();
        fs::write(&b, "Процедура П()\nКонецПроцедуры").unwrap();
        fs::write(workspace.join("Configuration.xml"), "<Configuration/>").unwrap();

        SharedState::rewalk_workspace_bsl_dirty(&engine_arc);

        let snapshot = {
            let guard = engine_arc.lock().unwrap();
            guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap()
        };
        assert!(snapshot.keys().any(|key| key.path.ends_with("A.bsl")), "top-level .bsl re-marked");
        assert!(snapshot.keys().any(|key| key.path.ends_with("B.bsl")), "nested .bsl re-marked");
        assert!(
            !snapshot.keys().any(|key| key.path.ends_with("Configuration.xml")),
            "non-.bsl paths are left alone",
        );
    }

    /// The rescan walk feeds `reconcile_workspace_files`, which deletes every stored key it
    /// does not find on disk. So a walk narrower than the engine's root table is not merely
    /// incomplete — it is destructive: the first hub overflow would wipe every extension's rows
    /// while the files sit untouched on disk. The walk must therefore cover the SAME roots the
    /// table knows, and both halves are checked: the extension's file gets marked (the walk
    /// reached it) and its row survives (the reconcile did not disown it).
    #[test]
    fn an_overflow_rescan_covers_every_registered_root() {
        let dir = tempdir().unwrap();
        // The extension lives OUTSIDE the workspace directory: a walk that quietly used the
        // workspace instead of the root table would still cover an extension nested inside it,
        // and the check would pass while covering nothing it claims to.
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = dir.path().join("outside-ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        fs::write(configuration.join("A.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(extension.join("B.bsl"), "Процедура Вторая()\nКонецПроцедуры").unwrap();

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let (roots, _rejected) = bsl_search::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        );
        // A root outside the workspace is identified by its absolute spelling, so the expected
        // key is read from the table rather than spelled out here.
        let extension_key = roots
            .root_of(&extension.join("B.bsl"), &extension.join("B.bsl").canonicalize().unwrap())
            .expect("the extension's file has an owner");
        engine.set_workspace_roots(roots);
        engine.enable_workspace_watcher_mode();
        // Seed both rows directly: the boot indexers cannot write an extension's row yet, and
        // this test is about the WALK, not about who wrote the row.
        engine.store().upsert_file("", "A.bsl", b"hash-a", "code").unwrap();
        engine
            .store()
            .upsert_file(&extension_key.root_id, &extension_key.path, b"hash-b", "code")
            .unwrap();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        SharedState::rewalk_workspace_bsl_dirty(&engine_arc);

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let snapshot = engine.workspace_overlay_dirty_paths_snapshot().unwrap();
        assert!(
            snapshot.keys().any(|key| *key == extension_key),
            "the rescan walk reaches the extension's file: {snapshot:?}",
        );
        let stored: Vec<String> = engine
            .store()
            .all_files_in_collection("code")
            .unwrap()
            .into_iter()
            .map(|(key, _hash)| format!("{}:{}", key.root_id, key.path))
            .collect();
        assert!(
            stored
                .iter()
                .any(|row| *row == format!("{}:{}", extension_key.root_id, extension_key.path)),
            "the reconcile keeps the extension's row: {stored:?}",
        );
        assert!(stored.iter().any(|row| row == ":A.bsl"), "and the configuration's: {stored:?}");
    }

    /// The walk reads the engine's root table at each call rather than a set captured when the
    /// sink started. A captured copy would keep walking yesterday's roots for the daemon's whole
    /// life, and — because the reconcile deletes stored keys the walk did not find — would erase
    /// any root added to the table afterwards.
    #[test]
    fn the_rescan_walk_follows_the_table_rather_than_a_captured_root() {
        let dir = tempdir().unwrap();
        // The extension lives OUTSIDE the workspace directory: a walk that quietly used the
        // workspace instead of the root table would still cover an extension nested inside it,
        // and the check would pass while covering nothing it claims to.
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = dir.path().join("outside-ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        fs::write(configuration.join("A.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(extension.join("B.bsl"), "Процедура Вторая()\nКонецПроцедуры").unwrap();

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let (configuration_only, _) =
            bsl_search::WorkspaceRoots::build(&workspace, &configuration, &[]);
        engine.set_workspace_roots(configuration_only);
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        SharedState::rewalk_workspace_bsl_dirty(&engine_arc);
        {
            let guard = engine_arc.lock().unwrap();
            let snapshot =
                guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap();
            assert!(
                !snapshot.keys().any(|key| key.path.ends_with("B.bsl")),
                "the undeclared tree is outside the walk while the table says so",
            );
        }

        {
            let mut guard = engine_arc.lock().unwrap();
            let engine = guard.as_mut().unwrap();
            let (both, _) = bsl_search::WorkspaceRoots::build(
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );
            engine.set_workspace_roots(both);
            engine.enable_workspace_watcher_mode();
        }
        SharedState::rewalk_workspace_bsl_dirty(&engine_arc);

        let guard = engine_arc.lock().unwrap();
        let snapshot = guard.as_ref().unwrap().workspace_overlay_dirty_paths_snapshot().unwrap();
        assert!(
            snapshot.keys().any(|key| key.path.ends_with("B.bsl")),
            "the next walk covers the root the table gained: {snapshot:?}",
        );
    }

    /// A root `.xml` descriptor can shift any module's graph context, so it marks the whole
    /// collection. "Root" here means the CONFIGURATION's root — the base every stored relative
    /// path is spelled against — and it is not the project directory: a configuration commonly
    /// sits in a subdirectory of it. Comparing against the project directory instead leaves the
    /// descriptor unrecognised and silently serves the stale context.
    #[test]
    fn a_root_xml_of_a_nested_configuration_marks_the_whole_collection() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let configuration = workspace.join("src").join("cf");
        fs::create_dir_all(&configuration).unwrap();
        let module = configuration.join("CommonModules").join("Общий").join("Ext");
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("Module.bsl"), "Процедура Первая()\nКонецПроцедуры").unwrap();

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        let (roots, _) = bsl_search::WorkspaceRoots::build(&workspace, &configuration, &[]);
        engine.set_workspace_roots(roots);
        engine.index_directory_fts(&configuration).unwrap();
        assert!(engine.file_count().unwrap() > 0, "the fixture indexes a document");
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let descriptor = configuration.join("Configuration.xml");
        fs::write(&descriptor, "<Configuration/>").unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: descriptor.clone(),
                raw: descriptor.clone(),
                kind: ChangeKind::MaybeChanged,
                seq: 1,
            }],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let marked = guard.as_ref().unwrap().store().context_dirty_paths("code").unwrap();
        assert!(
            !marked.is_empty(),
            "the configuration's root descriptor marks every document's context",
        );
    }

    /// A deleted `.bsl` is removed from the workspace store so it stops appearing in
    /// results — closing the pre-existing gap where a deleted file lingered in FTS.
    #[test]
    fn search_sink_removes_deleted_bsl_from_results() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "Removed.bsl".to_owned(),
                    symbol_name: "УдаляемаяПроцедура".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура УдаляемаяПроцедура()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                }],
                None,
            )
            .unwrap();
        assert_eq!(engine.file_count().unwrap(), 1);
        assert!(
            !engine.text_search("УдаляемаяПроцедура", 10, Some("code")).unwrap().is_empty(),
            "the indexed file is initially found",
        );
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        // The file is gone from disk: classification re-stats it (stats are truth) → removed.
        let removed = workspace.join("Removed.bsl");
        let entry = ChangeEntry {
            canonical: removed.clone(),
            raw: removed,
            kind: ChangeKind::MaybeRemoved,
            seq: 1,
        };
        SharedState::apply_search_drift(
            &engine_arc,
            &[entry],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert_eq!(engine.file_count().unwrap(), 0, "the deleted file is dropped from the store");
        assert!(
            engine.text_search("УдаляемаяПроцедура", 10, Some("code")).unwrap().is_empty(),
            "the deleted file no longer appears in FTS results",
        );
    }

    /// An `.xml` metadata edit marks only the owned modules (the sibling `<Dir>/<Name>/`
    /// subtree) context-dirty via the store side table; unrelated modules are untouched
    /// and nothing is marked dirty — proving the resolver walks the owned subtree only,
    /// never the whole workspace.
    #[test]
    fn search_sink_xml_marks_only_owned_modules_context_dirty() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        // An MDO descriptor with an owned module, plus an unrelated object elsewhere.
        let owned = workspace.join("Catalogs/Товары/Ext/ObjectModule.bsl");
        fs::create_dir_all(owned.parent().unwrap()).unwrap();
        fs::write(&owned, "Процедура П()\nКонецПроцедуры").unwrap();
        let unrelated = workspace.join("Catalogs/Другой/Ext/ObjectModule.bsl");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, "Процедура П()\nКонецПроцедуры").unwrap();
        let xml = workspace.join("Catalogs/Товары.xml");
        fs::write(&xml, "<MetaDataObject/>").unwrap();

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let entry = ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };
        SharedState::apply_search_drift(
            &engine_arc,
            &[entry],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration(
                "Catalogs/Товары/Ext/ObjectModule.bsl"
            )),
            "the owned module is marked context-dirty: {dirty:?}",
        );
        assert!(
            !dirty.contains(&bsl_search::FileKey::configuration(
                "Catalogs/Другой/Ext/ObjectModule.bsl"
            )),
            "an unrelated object's module is left untouched: {dirty:?}",
        );
        assert_eq!(dirty.len(), 1, "only the owned subtree is marked, not the whole tree");
        // The xml path is metadata context, not a body edit: nothing is marked dirty and
        // no whole-workspace walk ran.
        let snapshot = engine.workspace_overlay_dirty_paths_snapshot().unwrap();
        assert!(snapshot.is_empty(), "an xml edit marks no body dirty and triggers no walk");
    }

    /// An analyzer-config edit (`dependsOn` and friends) can re-shape the extension
    /// topology with not a single `.xml` touched — the graph context of EVERY indexed
    /// document may be stale, so the sink must mark the whole collection dirty.
    /// Revert-proof: drop the config-file branch in `apply_search_drift` and nothing
    /// is marked (the classifier ignores non-`.bsl`/`.xml` paths).
    #[test]
    fn search_sink_config_edit_marks_whole_collection_context_dirty() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "CommonModules/А/Ext/Module.bsl".to_owned(),
                    symbol_name: "П".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура П()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                }],
                None,
            )
            .unwrap();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());

        let nested_toml = workspace.join("nested/bsl-analyzer.toml");
        fs::create_dir_all(nested_toml.parent().unwrap()).unwrap();
        fs::write(&nested_toml, "[source]\nroot = \".\"\n").unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: nested_toml.clone(),
                raw: nested_toml,
                kind: ChangeKind::MaybeChanged,
                seq: 1,
            }],
            false,
            &graph,
        );
        assert!(
            engine_arc
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .context_dirty_paths("code")
                .unwrap()
                .is_empty(),
            "a nested namesake config must not reshape the workspace root table",
        );

        let toml = workspace.join("bsl-analyzer.toml");
        fs::write(&toml, "[source]\nroot = \".\"\n").unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: toml.clone(),
                raw: toml,
                kind: ChangeKind::MaybeChanged,
                seq: 2,
            }],
            false,
            &graph,
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("CommonModules/А/Ext/Module.bsl")),
            "a root config edit must mark every indexed document context-dirty: {dirty:?}",
        );
    }

    /// A hub rescan (overflow / re-arm) destroyed per-path detail — a config edit
    /// may be among the lost events, so the sink must conservatively mark the whole
    /// collection context-dirty, not only re-mark `.bsl` bodies.
    #[test]
    fn search_sink_rescan_marks_whole_collection_context_dirty() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        // The module exists on disk: the rescan's `.bsl` rewalk prunes store rows
        // whose file is gone, and a pruned row cannot carry a context mark.
        let on_disk = workspace.join("CommonModules/Б/Ext/Module.bsl");
        fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        fs::write(&on_disk, "Процедура П()\nКонецПроцедуры").unwrap();

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "CommonModules/Б/Ext/Module.bsl".to_owned(),
                    symbol_name: "П".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 1,
                    text: "Процедура П()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                }],
                None,
            )
            .unwrap();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        SharedState::apply_search_drift(
            &engine_arc,
            &[],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("CommonModules/Б/Ext/Module.bsl")),
            "a rescan must conservatively mark every indexed document context-dirty: {dirty:?}",
        );
    }

    /// A metadata `.xml` edit marks BOTH the object's owned modules (path convention) AND the
    /// REFERENCING modules — those whose `graph_context` embeds a read of the object — resolved
    /// through the persisted graph's inbound read edges. A module that references nothing about
    /// the object is left untouched.
    ///
    /// Revert-proof: drop the `resolve_referencing_module_files` call in
    /// `mark_xml_affected_context_dirty` and the referencing module `Б` is no longer marked —
    /// the referencing assertion fails.
    #[test]
    fn search_sink_xml_marks_owned_and_referencing_modules_context_dirty() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        fs::write(workspace.join("Configuration.xml"), "<Configuration/>").unwrap();

        // Catalog Х with an OWNED object module (A), resolved by path convention.
        let xml = workspace.join("Catalogs/Х.xml");
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(
            &xml,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Х</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#,
        )
        .unwrap();
        let owned_a = workspace.join("Catalogs/Х/Ext/ObjectModule.bsl");
        fs::create_dir_all(owned_a.parent().unwrap()).unwrap();
        fs::write(&owned_a, "Процедура П() Экспорт\nКонецПроцедуры").unwrap();

        // Referencing common module Б reads the catalog (manager access + query) → inbound
        // read edges into `mdo/Catalog/Х`. Non-referencing module В reads nothing about it.
        write_common_module(
            &workspace,
            "Б",
            "&НаСервере\nПроцедура ЧитаетХ() Экспорт\nСправочники.Х.СоздатьЭлемент();\nЗапрос = \"ВЫБРАТЬ Код ИЗ Справочник.Х\";\nКонецПроцедуры",
        );
        write_common_module(
            &workspace,
            "В",
            "&НаСервере\nПроцедура НичегоНеЧитает() Экспорт\nВозврат;\nКонецПроцедуры",
        );

        // Build + publish the graph so the reverse lookup has real inbound edges to read.
        let out = crate::cache::graph_db_path(&workspace);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        let sync_project = crate::graph::ProjectSnapshot::load(&workspace);
        let sync_universe = crate::graph::universe::ScannedUniverse::scan(&sync_project.scan_roots);
        let summary = crate::graph_db::build_graph_database(
            &sync_project,
            &sync_universe,
            &out,
            100,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_owned(),
            },
        )
        .expect("graph builds");
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        graph.adopt_prebuilt(1, crate::graph_db::GraphFp::default(), summary.modules, None);

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let entry = ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };
        SharedState::apply_search_drift(&engine_arc, &[entry], false, &graph);

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("Catalogs/Х/Ext/ObjectModule.bsl")),
            "the owned module is marked context-dirty: {dirty:?}",
        );
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("CommonModules/Б/Ext/Module.bsl")),
            "the referencing module (reads the catalog) is marked context-dirty: {dirty:?}",
        );
        assert!(
            !dirty.contains(&bsl_search::FileKey::configuration("CommonModules/В/Ext/Module.bsl")),
            "a module that references nothing about the catalog is left untouched: {dirty:?}",
        );
    }

    #[test]
    fn background_snapshot_failures_require_rescan_instead_of_empty_xml_success() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(workspace.join("Configuration.xml"), "<Configuration/>").unwrap();
        let xml = workspace.join("Catalogs/Х.xml");
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(&xml, "<MetaDataObject/>").unwrap();
        write_common_module(
            &workspace,
            "Б",
            "&НаСервере\nПроцедура ЧитаетХ() Экспорт\nСправочники.Х.СоздатьЭлемент();\nКонецПроцедуры",
        );

        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(&workspace);
        cache.ensure().unwrap();
        let project = crate::graph::ProjectSnapshot::load(&workspace);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        let summary = crate::graph_db::build_graph_database(
            &project,
            &universe,
            &cache.graph_db_path(),
            100,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_owned(),
            },
        )
        .unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = crate::graph::GraphState::for_workspace_with_cache(workspace.clone(), cache)
            .with_lease(lease.clone());
        graph.adopt_prebuilt(1, crate::graph_db::GraphFp::default(), summary.modules, None);

        let mut engine = SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[IndexedDocument {
                    collection: "code".to_owned(),
                    root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
                    path: "CommonModules/Б/Ext/Module.bsl".to_owned(),
                    symbol_name: "ЧитаетХ".to_owned(),
                    kind: "procedure".to_owned(),
                    line_start: 0,
                    line_end: 2,
                    text: "Процедура ЧитаетХ()\nКонецПроцедуры".to_owned(),
                    content_hash: "h".to_owned(),
                    graph_context: None,
                }],
                None,
            )
            .unwrap();
        engine.initialize_workspace_overlay_clean().unwrap();
        let engine: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        let entry = crate::change_hub::ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: crate::change_hub::ChangeKind::MaybeChanged,
            seq: 1,
        };
        let occupied: Vec<_> = (0..crate::graph::SNAPSHOT_POOL_CAP)
            .map(|_| graph.snapshot().expect("published descriptor"))
            .collect();

        for failure in [
            crate::graph::BackgroundSnapshotFailure::Changed,
            crate::graph::BackgroundSnapshotFailure::Open,
        ] {
            graph.set_background_snapshot_failure_for_test(Some(failure));
            let mut plan = SharedState::prepare_search_drift(
                &engine,
                std::slice::from_ref(&entry),
                false,
                &graph,
            );
            assert!(plan.full_rescan, "snapshot failure must request recovery rescan");
            assert!(matches!(
                plan.snapshot_outcome,
                Some(SnapshotPreparationOutcome::OperationError(_))
            ));
            assert!(matches!(
                SharedState::apply_prepared_search_drift(&engine, &lease, &mut plan, &graph),
                crate::state::WorkspaceSearchApply::OperationError(_)
            ));
        }

        // The third way the background snapshot can fail to answer, and the one the failure
        // injector cannot produce: the pool is empty and the fallback cannot take the lease
        // because a peer holds it. A refused lease is not an empty answer — it must leave the
        // same recovery debt as a broken one, or an `.xml` edit silently resolves to "no
        // referencing modules" and the modules that read the changed object keep stale context.
        graph.set_background_snapshot_failure_for_test(None);
        {
            let _held = lease.hold_file_lock_for_test();
            let contended = SharedState::prepare_search_drift(
                &engine,
                std::slice::from_ref(&entry),
                false,
                &graph,
            );
            assert!(matches!(
                contended.snapshot_outcome,
                Some(SnapshotPreparationOutcome::TransientRefusal)
            ));
        }

        drop(occupied);
        let mut recovery =
            SharedState::prepare_search_drift(&engine, std::slice::from_ref(&entry), true, &graph);
        assert!(matches!(
            SharedState::apply_prepared_search_drift(&engine, &lease, &mut recovery, &graph),
            crate::state::WorkspaceSearchApply::Applied(true)
        ));
        assert!(engine
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .context_dirty_paths("code")
            .unwrap()
            .contains(&bsl_search::FileKey::configuration("CommonModules/Б/Ext/Module.bsl")));
        lease.release();
    }

    /// An `.xml` edit BEFORE any graph is published degrades: owned modules are still marked
    /// (path convention needs no graph) and referencing resolution is silently skipped — no
    /// error, no panic. The reverse lookup only rides a published graph.
    #[test]
    fn search_sink_xml_referencing_degrades_without_published_graph() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        fs::write(workspace.join("Configuration.xml"), "<Configuration/>").unwrap();
        let xml = workspace.join("Catalogs/Х.xml");
        fs::create_dir_all(xml.parent().unwrap()).unwrap();
        fs::write(&xml, "<MetaDataObject/>").unwrap();
        let owned_a = workspace.join("Catalogs/Х/Ext/ObjectModule.bsl");
        fs::create_dir_all(owned_a.parent().unwrap()).unwrap();
        fs::write(&owned_a, "Процедура П() Экспорт\nКонецПроцедуры").unwrap();
        // A would-be referencing module exists on disk but there is NO published graph, so it
        // is not discoverable and must not be marked.
        write_common_module(
            &workspace,
            "Б",
            "&НаСервере\nПроцедура ЧитаетХ() Экспорт\nСправочники.Х.СоздатьЭлемент();\nКонецПроцедуры",
        );

        // A workspace graph that has never been built → `snapshot()` returns None.
        let graph = crate::graph::GraphState::for_workspace(workspace.clone());

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let entry = ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };
        SharedState::apply_search_drift(&engine_arc, &[entry], false, &graph);

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("Catalogs/Х/Ext/ObjectModule.bsl")),
            "the owned module is still marked without a published graph: {dirty:?}",
        );
        assert!(
            !dirty.contains(&bsl_search::FileKey::configuration("CommonModules/Б/Ext/Module.bsl")),
            "referencing resolution is skipped with no published graph: {dirty:?}",
        );
    }

    /// ANY `.xml` directly at the workspace root (not only `Configuration.xml`), with no
    /// owned-module subtree, conservatively marks the whole collection context-dirty — a
    /// root descriptor change can shift any module's context.
    #[test]
    fn search_sink_root_xml_marks_whole_collection_context_dirty() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        let doc = |path: &str, sym: &str| IndexedDocument {
            collection: "code".to_owned(),
            root_id: bsl_search::CONFIGURATION_ROOT_ID.to_owned(),
            path: path.to_owned(),
            symbol_name: sym.to_owned(),
            kind: "procedure".to_owned(),
            line_start: 0,
            line_end: 1,
            text: format!("Процедура {sym}()\nКонецПроцедуры"),
            content_hash: "h".to_owned(),
            graph_context: None,
        };
        engine
            .sync_indexed_documents_in_collection(
                "code",
                &[doc("A.bsl", "Ааа"), doc("B.bsl", "Ббб")],
                None,
            )
            .unwrap();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        // A root `.xml` NOT named Configuration.xml, with no sibling `<stem>/` subtree.
        let xml = workspace.join("SomePlugin.xml");
        fs::write(&xml, "<Root/>").unwrap();
        let entry = ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };
        SharedState::apply_search_drift(
            &engine_arc,
            &[entry],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        let dirty = engine.context_dirty_paths("code").unwrap();
        assert_eq!(dirty.len(), 2, "a root .xml marks every indexed file: {dirty:?}");
        assert!(
            dirty.contains(&bsl_search::FileKey::configuration("A.bsl"))
                && dirty.contains(&bsl_search::FileKey::configuration("B.bsl"))
        );
    }

    /// Root descriptors of the roots other than the configuration's, and of the trees the
    /// root table deliberately does not hold. A change to any of them can shift the graph
    /// context of any module, and the modules of every root have been in the index since
    /// the table gained them — so each must reach the whole collection.
    mod root_descriptors {
        use super::*;
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::FileKey;
        use std::path::{Path, PathBuf};

        /// Two documents, one per root, so a whole-collection mark is distinguishable from a
        /// mark that reached one root only.
        fn doc(root_id: &str, path: &str) -> IndexedDocument {
            IndexedDocument {
                collection: "code".to_owned(),
                root_id: root_id.to_owned(),
                path: path.to_owned(),
                symbol_name: format!("Символ{path}"),
                kind: "procedure".to_owned(),
                line_start: 0,
                line_end: 1,
                text: "Процедура П()\nКонецПроцедуры".to_owned(),
                content_hash: "h".to_owned(),
                graph_context: None,
            }
        }

        fn engine_over(
            db_path: &Path,
            workspace: &Path,
            configuration: &Path,
            extensions: &[PathBuf],
        ) -> (super::super::SharedSearchEngine, Vec<String>) {
            let (roots, _) =
                bsl_search::WorkspaceRoots::build(workspace, configuration, extensions);
            let ids: Vec<String> = roots.ids().map(str::to_owned).collect();
            let mut engine = SearchEngine::fts_only(db_path).unwrap();
            engine.set_workspace_roots(roots);
            let docs: Vec<IndexedDocument> =
                ids.iter().map(|id| doc(id, "CommonModules/М/Ext/Module.bsl")).collect();
            engine.sync_indexed_documents_in_collection("code", &docs, None).unwrap();
            (Arc::new(Mutex::new(Some(engine))), ids)
        }

        fn drift(engine: &super::super::SharedSearchEngine, paths: &[&Path]) {
            let entries: Vec<ChangeEntry> = paths
                .iter()
                .enumerate()
                .map(|(i, path)| ChangeEntry {
                    canonical: path.to_path_buf(),
                    raw: path.to_path_buf(),
                    kind: ChangeKind::MaybeChanged,
                    seq: i as u64 + 1,
                })
                .collect();
            SharedState::apply_search_drift(
                engine,
                &entries,
                false,
                &crate::graph::GraphState::disabled(),
            );
        }

        fn marks(engine: &super::super::SharedSearchEngine) -> std::collections::HashSet<FileKey> {
            let guard = engine.lock().unwrap();
            guard.as_ref().unwrap().context_dirty_paths("code").unwrap()
        }

        /// A dump root: the descriptor that makes the project model call a directory an
        /// extension at all.
        fn dump_root(at: &Path) -> PathBuf {
            std::fs::create_dir_all(at).unwrap();
            std::fs::write(at.join("Configuration.xml"), "<Configuration/>").unwrap();
            at.to_path_buf()
        }

        /// The root descriptor of a REGISTERED extension root. Recognising only the
        /// configuration's root left this one marking nothing at all.
        #[test]
        fn a_root_descriptor_of_an_extension_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            // Outside the workspace on purpose: an extension inside it is covered by the
            // configuration walk by accident, and the fixture would prove nothing.
            let extension = dump_root(&dir.path().join("cfe"));
            let (engine, ids) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );

            let descriptor = extension.join("ConfigDumpInfo.xml");
            std::fs::write(&descriptor, "<ConfigDumpInfo/>").unwrap();
            drift(&engine, &[&descriptor]);

            let dirty = marks(&engine);
            assert_eq!(dirty.len(), ids.len(), "every root's documents are marked: {dirty:?}");
        }

        /// The same descriptor one level down is an ordinary metadata file: it owns at most
        /// its own subtree and must not reach the collection. Without this the root branch
        /// could be "always true" and the tests above would still pass.
        #[test]
        fn a_descriptor_below_a_root_marks_nothing() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let extension = dump_root(&dir.path().join("cfe"));
            let (engine, _) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );

            let mut below = Vec::new();
            for root in [&configuration, &extension] {
                let deep = root.join("Catalogs");
                std::fs::create_dir_all(&deep).unwrap();
                let descriptor = deep.join("ConfigDumpInfo.xml");
                std::fs::write(&descriptor, "<ConfigDumpInfo/>").unwrap();
                below.push(descriptor);
            }
            drift(&engine, &below.iter().map(PathBuf::as_path).collect::<Vec<_>>());

            assert!(marks(&engine).is_empty(), "a descriptor below a root marks nothing");
        }

        /// An extension canonically inside the configuration is REJECTED from the table
        /// (its files carry the configuration's key), so no registered root sits at its
        /// directory — the class the cut names as part of the rule, not an exception.
        #[test]
        fn a_configuration_xml_of_a_rejected_extension_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let nested = dump_root(&configuration.join("nested"));
            let (engine, _) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&nested),
            );

            drift(&engine, &[&nested.join("Configuration.xml")]);

            assert!(!marks(&engine).is_empty(), "the rejected root's descriptor marks the tree");
        }

        /// The same rejected root, a descriptor NOT named `Configuration.xml`. The class is
        /// every root-level descriptor, and a rule keyed on one file name would leave the
        /// rest of it — `ConfigDumpInfo.xml`, a third-party dump's own descriptor — unmarked.
        #[test]
        fn a_root_descriptor_beside_a_configuration_xml_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let nested = dump_root(&configuration.join("nested"));
            let (engine, _) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&nested),
            );

            let descriptor = nested.join("ConfigDumpInfo.xml");
            std::fs::write(&descriptor, "<ConfigDumpInfo/>").unwrap();
            drift(&engine, &[&descriptor]);

            assert!(!marks(&engine).is_empty(), "a descriptor beside a Configuration.xml marks");
        }

        /// The descriptor itself is what vanished, so the tree it stood in can no longer be
        /// recognised by its neighbour — and a removal is exactly the change that shifts
        /// every context.
        #[test]
        fn a_removed_configuration_xml_of_a_rejected_extension_still_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let nested = dump_root(&configuration.join("nested"));
            let (engine, _) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&nested),
            );

            let descriptor = nested.join("Configuration.xml");
            std::fs::remove_file(&descriptor).unwrap();
            drift(&engine, &[&descriptor]);

            assert!(!marks(&engine).is_empty(), "a removed root descriptor still marks the tree");
        }

        /// A root-level descriptor that also has a namesake subtree beside it. The two
        /// answers are not alternatives: the owned subtree is a subset of the collection the
        /// root descriptor reaches, and deciding by whichever branch runs first would leave
        /// the rest of the tree stale.
        #[test]
        fn a_root_descriptor_with_a_namesake_subtree_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            let extension = dump_root(&dir.path().join("cfe"));
            let (engine, ids) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );

            let owned = extension.join("ConfigDumpInfo/Ext");
            std::fs::create_dir_all(&owned).unwrap();
            std::fs::write(owned.join("Module.bsl"), "Процедура П()\nКонецПроцедуры").unwrap();
            let descriptor = extension.join("ConfigDumpInfo.xml");
            std::fs::write(&descriptor, "<ConfigDumpInfo/>").unwrap();
            drift(&engine, &[&descriptor]);

            let dirty = marks(&engine);
            for id in &ids {
                assert!(
                    dirty.contains(&FileKey::new(id, "CommonModules/М/Ext/Module.bsl")),
                    "root {id:?} is marked despite the namesake subtree: {dirty:?}",
                );
            }
        }

        /// The event arrives spelled through an alias while the root is declared by its real
        /// path. Attribution ranks roots by the canonical spelling, so both spellings answer
        /// the same; comparing the delivered spelling alone would recognise neither.
        #[cfg(unix)]
        #[test]
        fn a_root_descriptor_reached_through_an_alias_marks_the_whole_collection() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let configuration = dump_root(&workspace.join("cf"));
            // No `Configuration.xml` in the extension root: with a neighbour present the
            // structural probe would answer through the alias too, and the canonical
            // attribution this pins would go untested.
            let extension = dir.path().join("plain-root");
            std::fs::create_dir_all(&extension).unwrap();
            let alias = workspace.join("link");
            std::os::unix::fs::symlink(&extension, &alias).unwrap();
            let (engine, ids) = engine_over(
                &dir.path().join("search.db"),
                &workspace,
                &configuration,
                std::slice::from_ref(&extension),
            );

            let descriptor = extension.join("SomePlugin.xml");
            std::fs::write(&descriptor, "<Root/>").unwrap();
            drift(&engine, &[&alias.join("SomePlugin.xml")]);

            let dirty = marks(&engine);
            assert_eq!(dirty.len(), ids.len(), "the aliased spelling is attributed: {dirty:?}");
        }
    }
    /// Marks that land on a module of an EXTENSION root. Its rows are keyed by that root,
    /// so a mark spelled against the configuration would name a different file — or no
    /// file at all.
    mod extension_marks {
        use super::*;
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::FileKey;
        use std::path::{Path, PathBuf};

        const MODULE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000002">
        <Properties><Name>{}</Name><Server>true</Server></Properties>
    </CommonModule>
</MetaDataObject>"#;

        fn write(path: &Path, text: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn common_module(root: &Path, name: &str, body: &str) {
            write(&root.join(format!("CommonModules/{name}.xml")), &MODULE_XML.replace("{}", name));
            write(&root.join(format!("CommonModules/{name}/Ext/Module.bsl")), body);
        }

        /// A configuration and an extension side by side, both declared to the project model.
        /// The extension lies OUTSIDE the configuration root: one inside it is rejected from
        /// the root table and its files carry the configuration's key, which would make the
        /// fixture prove nothing.
        fn two_root_workspace(workspace: &Path) -> (PathBuf, PathBuf) {
            let configuration = workspace.join("cf");
            let extension = workspace.join("cfe");
            write(&configuration.join("Configuration.xml"), "<Configuration/>");
            write(&extension.join("Configuration.xml"), "<Configuration/>");
            fs::write(
                workspace.join("bsl-analyzer.toml"),
                "[source]\nroot = \"cf\"\nextensions = [{ name = \"a\", path = \"cfe\" }]\n",
            )
            .unwrap();
            (configuration, extension)
        }

        fn engine_over(
            db_path: &Path,
            workspace: &Path,
            configuration: &Path,
            extension: &Path,
        ) -> super::super::SharedSearchEngine {
            let (roots, rejected) = bsl_search::WorkspaceRoots::build(
                workspace,
                configuration,
                std::slice::from_ref(&extension.to_path_buf()),
            );
            assert!(rejected.is_empty(), "the extension is a root of its own: {rejected:?}");
            let mut engine = SearchEngine::fts_only(db_path).unwrap();
            engine.set_workspace_roots(roots);
            // Rows keyed by the engine's own attribution: the mark is then compared against a
            // key nobody spelled by hand.
            engine.index_unindexed_roots_fts().unwrap();
            engine.enable_workspace_watcher_mode();
            Arc::new(Mutex::new(Some(engine)))
        }

        fn drift(
            engine: &super::super::SharedSearchEngine,
            xml: &Path,
            graph: &crate::graph::GraphState,
        ) {
            SharedState::apply_search_drift(
                engine,
                &[ChangeEntry {
                    canonical: xml.to_path_buf(),
                    raw: xml.to_path_buf(),
                    kind: ChangeKind::MaybeChanged,
                    seq: 1,
                }],
                false,
                graph,
            );
        }

        /// A module of an extension that READS a configuration object. Its context embeds
        /// that object's metadata, so a change to the object's descriptor makes it stale —
        /// and the mark has to carry the extension's root, the one its row carries.
        #[test]
        fn a_referencing_module_of_an_extension_is_marked_under_its_own_root() {
            let dir = tempdir().unwrap();
            let marks = referencing_marks_in(dir.path(), &dir.path().join("ws"));
            let reader = FileKey::new("cfe", "CommonModules/Б/Ext/Module.bsl");
            assert!(
                marks.dirty.contains(&reader),
                "the extension's reader is marked under its own root: {marks:?}",
            );
            assert!(
                !marks.dirty.contains(&FileKey::new("cfe", "CommonModules/В/Ext/Module.bsl")),
                "a module that reads nothing about the object is untouched: {marks:?}",
            );
            assert!(
                marks.rows.contains(&reader),
                "the mark names the key the row lives under: {marks:?}",
            );
            for key in &marks.dirty {
                assert!(
                    marks.rows.contains(key),
                    "no mark names a root the table does not hold: {key:?} vs {marks:?}",
                );
            }
        }

        /// Under a root directory holding bytes no `str` can carry, the graph — which keeps
        /// its file paths as strings — hands back a rendering, and a rendering belongs to no
        /// root. The reader keeps its stale context until a whole-collection mark, and that
        /// is the deliberate answer: the alternative is a key guessed from a rendering that
        /// several different roots fit, and the seam that key would travel is the one
        /// removals resolve through.
        #[cfg(unix)]
        #[test]
        fn a_referencing_module_under_an_unrepresentable_root_is_left_to_a_wider_mark() {
            use std::os::unix::ffi::OsStringExt;
            let dir = tempdir().unwrap();
            let workspace = dir.path().join(std::ffi::OsString::from_vec(b"ws\xff".to_vec()));
            let marks = referencing_marks_in(dir.path(), &workspace);
            assert!(
                marks.dirty.is_empty(),
                "a rendering marks nothing rather than the wrong thing: {marks:?}",
            );
            assert!(!marks.rows.is_empty(), "the files themselves are indexed as always");
        }

        #[derive(Debug)]
        struct Marks {
            dirty: std::collections::HashSet<FileKey>,
            rows: Vec<FileKey>,
        }

        fn referencing_marks_in(dir: &Path, workspace: &Path) -> Marks {
            let (configuration, extension) = two_root_workspace(workspace);

            let xml = configuration.join("Catalogs/Х.xml");
            write(
                &xml,
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Х</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#,
            );
            common_module(
                &extension,
                "Б",
                "&НаСервере\nПроцедура ЧитаетХ() Экспорт\nСправочники.Х.СоздатьЭлемент();\nКонецПроцедуры",
            );
            common_module(
                &extension,
                "В",
                "&НаСервере\nПроцедура НичегоНеЧитает() Экспорт\nВозврат;\nКонецПроцедуры",
            );

            let out = crate::cache::graph_db_path(workspace);
            fs::create_dir_all(out.parent().unwrap()).unwrap();
            let project = crate::graph::ProjectSnapshot::load(workspace);
            let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
            let summary = crate::graph_db::build_graph_database(
                &project,
                &universe,
                &out,
                100,
                &crate::graph_db::GraphMeta {
                    revision: 1,
                    fingerprint: crate::graph_db::GraphFp::default(),
                    files: 0,
                    built_at: "t".to_owned(),
                },
            )
            .expect("graph builds");
            let graph = crate::graph::GraphState::for_workspace(workspace.to_path_buf());
            graph.adopt_prebuilt(1, crate::graph_db::GraphFp::default(), summary.modules, None);

            let engine = engine_over(&dir.join("search.db"), workspace, &configuration, &extension);
            drift(&engine, &xml, &graph);

            let guard = engine.lock().unwrap();
            let engine = guard.as_ref().unwrap();
            Marks {
                dirty: engine.context_dirty_paths("code").unwrap(),
                rows: engine
                    .store()
                    .all_files_in_collection("code")
                    .unwrap()
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect(),
            }
        }

        /// The owned modules of an extension's own object: resolved by path convention, so no
        /// graph is needed — but the key still has to be the extension's.
        #[test]
        fn an_owned_module_of_an_extension_is_marked_under_its_own_root() {
            let dir = tempdir().unwrap();
            let workspace = dir.path().join("ws");
            let (configuration, extension) = two_root_workspace(&workspace);
            let owned = extension.join("Catalogs/Т/Ext/ObjectModule.bsl");
            write(&owned, "Процедура П()\nКонецПроцедуры");
            let xml = extension.join("Catalogs/Т.xml");
            write(&xml, "<MetaDataObject/>");

            let engine =
                engine_over(&dir.path().join("search.db"), &workspace, &configuration, &extension);
            drift(&engine, &xml, &crate::graph::GraphState::disabled());

            let guard = engine.lock().unwrap();
            let dirty = guard.as_ref().unwrap().context_dirty_paths("code").unwrap();
            assert!(
                dirty.contains(&FileKey::new("cfe", "Catalogs/Т/Ext/ObjectModule.bsl")),
                "the extension's owned module is marked under its own root: {dirty:?}",
            );
        }
    }

    /// An `.xml` drift whose owned module is marked context-dirty must NUDGE the graph to
    /// catch up — otherwise a search-only user (who never triggers a `graph` tool freshness
    /// check) leaves the marks unresolved forever. Asserting the graph left `Idle` with NO
    /// graph tool call. Disable the `graph.nudge_rebuild()` call → the graph stays `Idle` and
    /// this fails.
    #[test]
    fn search_sink_xml_drift_nudges_graph_to_catch_up() {
        use crate::change_hub::{ChangeEntry, ChangeKind};

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");

        // An MDO descriptor with an owned module so the xml resolves to a real dirty mark.
        let owned = workspace.join("Catalogs/Товары/Ext/ObjectModule.bsl");
        fs::create_dir_all(owned.parent().unwrap()).unwrap();
        fs::write(&owned, "Процедура П()\nКонецПроцедуры").unwrap();
        let xml = workspace.join("Catalogs/Товары.xml");
        fs::write(&xml, "<MetaDataObject/>").unwrap();

        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        let graph = crate::graph::GraphState::for_workspace(workspace.clone());
        assert_eq!(graph.status(), crate::graph::GraphStatus::Idle, "graph starts idle");

        let entry = ChangeEntry {
            canonical: xml.clone(),
            raw: xml,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };
        SharedState::apply_search_drift(&engine_arc, &[entry], false, &graph);

        assert_ne!(
            graph.status(),
            crate::graph::GraphStatus::Idle,
            "the xml drift nudged the graph to catch up without any graph tool call",
        );
    }
    /// A batch that demands a full reconcile still carries the exact paths it knows about,
    /// and the deletions among them are the one thing the re-walk cannot recover: an
    /// incomplete walk skips the reconcile precisely so it does not evict healthy files,
    /// leaving a deleted file in the index. Applying the delivered removals costs nothing
    /// and is exact — including a vanished directory, whose descendants no drain can name.
    #[test]
    fn a_rescan_batch_removes_the_deletions_it_delivered_even_when_the_walk_is_incomplete() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        // Toggles the process-global `FORCE_REWALK_WALK_ERROR` seam; serialize against the
        // other tests that read it.
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            let mut index = |path: &str, name: &str| {
                store
                    .reindex_file(
                        bsl_search::CONFIGURATION_ROOT_ID,
                        path,
                        b"h",
                        &[Chunk {
                            kind: ChunkKind::Procedure,
                            name: name.to_owned(),
                            is_export: true,
                            annotations: vec![],
                            line_start: 0,
                            line_end: 1,
                            text: format!("Процедура {name}()\nКонецПроцедуры"),
                        }],
                        None,
                    )
                    .unwrap();
            };
            index("Gone.bsl", "Ушедшая");
            index("Dropped/One.bsl", "ПерваяИзПоддерева");
            index("Dropped/Two.bsl", "ВтораяИзПоддерева");
            index("Kept.bsl", "Оставшаяся");
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        assert_eq!(engine.file_count().unwrap(), 4, "all four files are indexed");
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let gone = workspace.join("Gone.bsl");
        let dropped = workspace.join("Dropped");
        let entries = [
            ChangeEntry {
                canonical: gone.clone(),
                raw: gone,
                kind: ChangeKind::MaybeRemoved,
                seq: 1,
            },
            ChangeEntry {
                canonical: dropped.clone(),
                raw: dropped,
                kind: ChangeKind::SubtreeRemoved,
                seq: 2,
            },
        ];
        SharedState::apply_search_drift(
            &engine_arc,
            &entries,
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        for token in ["Ушедшая", "ПерваяИзПоддерева", "ВтораяИзПоддерева"]
        {
            assert!(
                engine.text_search(token, 10, Some("code")).unwrap().is_empty(),
                "{token} was delivered as deleted and must not answer searches",
            );
        }
        assert!(
            !engine.text_search("Оставшаяся", 10, Some("code")).unwrap().is_empty(),
            "a file nobody reported deleted is untouched",
        );
    }

    /// An event says what was true when it fired, not what is true when it is consumed: a
    /// directory removed and restored (a checkout, an editor's atomic replace) arrives as a
    /// removal for a subtree that exists again. The classifier re-stats every path for this
    /// reason, and a subtree removal must too — it deletes far more at once, and the walk
    /// that would restore it is skipped exactly when it is incomplete.
    #[test]
    fn a_subtree_removal_for_a_directory_that_came_back_deletes_nothing() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        fs::create_dir_all(workspace.join("Restored")).unwrap();
        fs::write(workspace.join("Restored/Alive.bsl"), "Процедура Живущая()\nКонецПроцедуры")
            .unwrap();
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Restored/Alive.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Живущая".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Живущая()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        // The walk cannot vouch for anything, so nothing would restore a wrong deletion.
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let restored = workspace.join("Restored");
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: restored.clone(),
                raw: restored,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            !engine.text_search("Живущая", 10, Some("code")).unwrap().is_empty(),
            "a directory that is on disk when the removal is applied keeps its files",
        );
    }

    /// A vanished directory has to leave the index whether or not the batch also demanded a
    /// full reconcile. The ordinary branch answers a subtree removal with a re-walk, and
    /// that re-walk refuses to reconcile when it is incomplete — deliberately, so it cannot
    /// evict healthy files. Then nothing removes the descendants: the classifier calls a
    /// subtree removal structural and skips it, and no event ever names them.
    #[test]
    fn a_vanished_directory_leaves_the_index_when_the_ordinary_branch_cannot_walk() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        fs::write(workspace.join("Kept.bsl"), "Процедура Уцелевшая()\nКонецПроцедуры").unwrap();
        {
            let mut store = Store::open(&db_path).unwrap();
            let mut index = |path: &str, name: &str| {
                store
                    .reindex_file(
                        bsl_search::CONFIGURATION_ROOT_ID,
                        path,
                        b"h",
                        &[Chunk {
                            kind: ChunkKind::Procedure,
                            name: name.to_owned(),
                            is_export: true,
                            annotations: vec![],
                            line_start: 0,
                            line_end: 1,
                            text: format!("Процедура {name}()\nКонецПроцедуры"),
                        }],
                        None,
                    )
                    .unwrap();
            };
            index("Dropped/One.bsl", "ПерваяУшедшая");
            index("Kept.bsl", "Уцелевшая");
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        assert_eq!(engine.file_count().unwrap(), 2, "both files are indexed");
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        // The re-walk this branch performs cannot vouch for anything, so its reconcile —
        // the only other thing that would remove the descendants — is skipped.
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let dropped = workspace.join("Dropped");
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: dropped.clone(),
                raw: dropped,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("ПерваяУшедшая", 10, Some("code")).unwrap().is_empty(),
            "the descendants of a vanished directory stop answering searches",
        );
        assert!(
            !engine.text_search("Уцелевшая", 10, Some("code")).unwrap().is_empty(),
            "and nothing else is touched",
        );
    }

    /// The hub names a vanished path a subtree only when it has no extension — it cannot ask
    /// a path that is gone what it used to be. A directory with a dot in its name therefore
    /// arrives as an ordinary removal, and the classifier drops it for being neither `.bsl`
    /// nor `.xml`. Its files would then have nobody to remove them.
    #[test]
    fn a_vanished_directory_with_a_dotted_name_still_loses_its_files() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Dropped.v1/One.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "ИзВерсии".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура ИзВерсии()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        // The hub calls this `MaybeRemoved`, because `Dropped.v1` looks like it has an
        // extension — the one thing it can tell about a path that no longer exists.
        let dropped = workspace.join("Dropped.v1");
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: dropped.clone(),
                raw: dropped,
                kind: ChangeKind::MaybeRemoved,
                seq: 1,
            }],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("ИзВерсии", 10, Some("code")).unwrap().is_empty(),
            "a dotted directory name does not save its files from a deletion",
        );
    }

    /// A directory whose name ends in `.bsl` looks exactly like a file to everything that
    /// can only read the name — the hub, and any filter written in terms of extensions.
    /// Deciding per KEY sidesteps the question: a real file simply has nothing under it.
    #[test]
    fn a_vanished_directory_named_like_a_module_still_loses_its_files() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Модули.bsl/One.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "ИзПапкиМодули".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура ИзПапкиМодули()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let dropped = workspace.join("Модули.bsl");
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: dropped.clone(),
                raw: dropped,
                kind: ChangeKind::MaybeRemoved,
                seq: 1,
            }],
            false,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("ИзПапкиМодули", 10, Some("code")).unwrap().is_empty(),
            "a module-looking directory name does not save its files",
        );
    }

    /// A directory that is back proves only that the NAME is taken again, not that the
    /// files under it survived: a checkout can restore it with a different set entirely.
    /// Judging the whole subtree by the directory keeps the ones that are truly gone.
    #[test]
    fn a_directory_that_came_back_with_other_files_loses_the_ones_that_did_not() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        let restored = workspace.join("Restored");
        fs::create_dir_all(&restored).unwrap();
        fs::write(restored.join("Stays.bsl"), "Процедура Оставшаяся()\nКонецПроцедуры").unwrap();
        {
            let mut store = Store::open(&db_path).unwrap();
            let mut index = |path: &str, name: &str| {
                store
                    .reindex_file(
                        bsl_search::CONFIGURATION_ROOT_ID,
                        path,
                        b"h",
                        &[Chunk {
                            kind: ChunkKind::Procedure,
                            name: name.to_owned(),
                            is_export: true,
                            annotations: vec![],
                            line_start: 0,
                            line_end: 1,
                            text: format!("Процедура {name}()\nКонецПроцедуры"),
                        }],
                        None,
                    )
                    .unwrap();
            };
            index("Restored/Stays.bsl", "Оставшаяся");
            index("Restored/Gone.bsl", "Пропавшая");
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: restored.clone(),
                raw: restored,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("Пропавшая", 10, Some("code")).unwrap().is_empty(),
            "the file that did not come back is gone from the index",
        );
        assert!(
            !engine.text_search("Оставшаяся", 10, Some("code")).unwrap().is_empty(),
            "the one that did is untouched",
        );
    }

    /// A name taken by something that is not a directory is not the subtree coming back: a
    /// file holds no files, so its descendants are gone as surely as if nothing were there.
    #[test]
    fn a_subtree_replaced_by_a_file_is_still_removed() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Replaced/One.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "ПодЗамену".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура ПодЗамену()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        // The directory is gone and a plain file now carries its name.
        let replaced = workspace.join("Replaced");
        fs::write(&replaced, "не каталог").unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: replaced.clone(),
                raw: replaced,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("ПодЗамену", 10, Some("code")).unwrap().is_empty(),
            "a name taken by a file cannot hold the subtree's files",
        );
    }

    /// The hub decides a path vanished by following links, so a subtree reached through a
    /// link whose target is deleted is gone as far as it is concerned. Asking about the link
    /// itself would answer "still there" and silently drop the removal it delivered.
    #[cfg(unix)]
    #[test]
    fn a_subtree_reached_through_a_dangling_link_is_removed() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};

        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let outside = tempdir().unwrap();
        let target = outside.path().join("real");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("A.bsl"), "Процедура Внешняя()\nКонецПроцедуры").unwrap();
        let link = workspace.join("Linked");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Linked/A.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Внешняя".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Внешняя()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        // The target goes; the link stays behind, pointing at nothing.
        fs::remove_dir_all(&target).unwrap();
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: link.clone(),
                raw: link,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            engine.text_search("Внешняя", 10, Some("code")).unwrap().is_empty(),
            "a subtree whose target is deleted stops answering searches",
        );
    }

    /// "Could not check" is not "is gone". A subtree whose parent is momentarily unreadable
    /// answers `PermissionDenied`, and treating that as proof of deletion clears rows,
    /// overlay entries and vectors for files that are on disk — while the walk that would
    /// restore them is skipped for exactly the same reason.
    #[cfg(unix)]
    #[test]
    fn a_subtree_removal_that_cannot_be_verified_deletes_nothing() {
        use crate::change_hub::{ChangeEntry, ChangeKind};
        use bsl_search::{Chunk, ChunkKind, Store};
        use std::os::unix::fs::PermissionsExt;

        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        let blocked = workspace.join("Blocked");
        fs::create_dir_all(blocked.join("Gone")).unwrap();
        fs::write(blocked.join("Gone/A.bsl"), "Процедура Недоступная()\nКонецПроцедуры").unwrap();
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Blocked/Gone/A.bsl",
                    b"h",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "Недоступная".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура Недоступная()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&blocked).is_ok() {
            // Running as root: permissions cannot make the parent unreadable.
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let gone = blocked.join("Gone");
        SharedState::apply_search_drift(
            &engine_arc,
            &[ChangeEntry {
                canonical: gone.clone(),
                raw: gone,
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }],
            true,
            &crate::graph::GraphState::disabled(),
        );
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert!(
            !engine.text_search("Недоступная", 10, Some("code")).unwrap().is_empty(),
            "a subtree that could not be checked keeps its files",
        );
    }

    /// A partial rescan walk (an error mid-walk) must NOT reconcile: `present` is missing healthy
    /// files, so deleting stored files against it would evict live data. Only a clean walk
    /// reconciles. Reverting the walk-error guard deletes the stored file on the errored walk.
    #[test]
    fn rescan_walk_error_skips_reconcile_and_keeps_stored_files() {
        use bsl_search::{Chunk, ChunkKind, Store};

        // This test toggles the process-global `FORCE_REWALK_WALK_ERROR` seam; serialize against the
        // boot-reconcile tests (which read it) so its forced error can't leak into their walk.
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let db_path = dir.path().join("search.db");
        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    bsl_search::CONFIGURATION_ROOT_ID,
                    "Gone.bsl",
                    b"ha",
                    &[Chunk {
                        kind: ChunkKind::Procedure,
                        name: "П".to_owned(),
                        is_export: true,
                        annotations: vec![],
                        line_start: 0,
                        line_end: 1,
                        text: "Процедура П()\nКонецПроцедуры".to_owned(),
                    }],
                    None,
                )
                .unwrap();
        }
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        assert_eq!(engine.file_count().unwrap(), 1, "the stored file is present");
        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));

        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }

        // Errored walk: reconcile is skipped, so the stored (disk-absent) file SURVIVES.
        {
            FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
            let _reset = ResetWalkErr;
            SharedState::rewalk_workspace_bsl_dirty(&engine_arc);
            assert_eq!(
                engine_arc.lock().unwrap().as_ref().unwrap().file_count().unwrap(),
                1,
                "a partial walk must not reconcile healthy files out of the store",
            );
        }

        // Clean walk: the stored-but-absent file is reconciled out.
        SharedState::rewalk_workspace_bsl_dirty(&engine_arc);
        assert_eq!(
            engine_arc.lock().unwrap().as_ref().unwrap().file_count().unwrap(),
            0,
            "a clean walk reconciles the deleted file out",
        );
    }
    /// The overlay keys dirty paths relative to the ENGINE root (the nested config source root),
    /// while the resident is indexed under the OUTER workspace root. `prefetch_resident_overlay`
    /// must resolve each dirty rel to an absolute path against the engine root before asking the
    /// resident, so a nested config (every real workspace) actually gets a resident-fed reindex.
    /// Reverting the absolute-join (passing the rel verbatim) leaves the resident-fed count at 0.
    #[test]
    fn prefetch_resident_overlay_feeds_nested_config_from_resident() {
        use crate::diagnostics_state::{
            DiagnosticsState, DiagnosticsStatus, ResidentModuleSnapshotSource,
        };
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let outer = dir.path().to_path_buf();
        let cf = outer.join("src").join("cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(
            cf.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &cf,
            "Сервер",
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n",
        );
        let module = cf.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");

        // Overlay engine rooted at the NESTED config root, so `source_path != outer`.
        let mut engine = SearchEngine::fts_only(&outer.join("search.db")).unwrap();
        engine.set_workspace_root(cf.clone());
        engine.enable_workspace_watcher_mode();
        engine.prime_workspace_overlay().unwrap();

        // The file grows on disk so the reindex genuinely rebuilds it (fingerprint differs).
        fs::write(
            &module,
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n\
             Процедура Ещё() Экспорт КонецПроцедуры\n",
        )
        .unwrap();

        // The resident is built against the OUTER root AFTER the edit, so it holds the new bytes.
        let diagnostics = DiagnosticsState::for_workspace(outer.clone());
        diagnostics.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(diagnostics.status(), DiagnosticsStatus::Ready { .. }) {
            assert!(Instant::now() < deadline, "the resident did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }

        let source: Arc<dyn bsl_search::ModuleSnapshotSource> =
            Arc::new(ResidentModuleSnapshotSource::new(diagnostics.clone()));
        engine.set_module_snapshot_source(source);
        assert!(
            engine.mark_workspace_path_dirty(&module).unwrap(),
            "the nested module marks dirty"
        );

        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        SharedState::prefetch_resident_overlay_fenced(
            &engine_arc,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("an uncancelled prefetch completes");

        let fed = engine_arc
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_overlay_resident_fed_count()
            .unwrap();
        assert_eq!(
            fed, 1,
            "a nested-config dirty path must be served from the resident's shared parse",
        );
    }

    /// Search and diagnostics drain independent hub cursors, so a just-edited file leaves the
    /// resident BEHIND disk. `prefetch_resident_overlay` must catch the resident up on pending
    /// drift FIRST, so the snapshot text matches disk and the reindex is resident-fed rather than
    /// falling back to a disk read. Reverting the `catch_up` call leaves the resident stale, the
    /// byte-compare misses, and the resident-fed count stays 0.
    #[test]
    fn prefetch_resident_overlay_catches_up_stale_resident_before_reading() {
        use crate::change_hub::WorkspaceChangeHub;
        use crate::diagnostics_state::{
            DiagnosticsState, DiagnosticsStatus, ResidentModuleSnapshotSource,
        };
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(
            root.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &root,
            "Сервер",
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n",
        );
        let module = root.join("CommonModules").join("Сервер").join("Ext").join("Module.bsl");

        let hub = WorkspaceChangeHub::start(vec![root.clone()]);
        assert!(hub.wait_until_watching(Duration::from_secs(5)), "the hub must arm");
        let mut observer = hub.subscribe();

        let mut engine = SearchEngine::fts_only(&root.join("search.db")).unwrap();
        engine.set_workspace_root(root.clone());
        engine.enable_workspace_watcher_mode();
        engine.prime_workspace_overlay().unwrap();

        // Resident built at v1, wired to the SAME hub, but it never polls drift on its own.
        let diagnostics =
            DiagnosticsState::for_workspace(root.clone()).with_change_hub(hub.clone());
        diagnostics.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(diagnostics.status(), DiagnosticsStatus::Ready { .. }) {
            assert!(Instant::now() < deadline, "the resident did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }

        let source: Arc<dyn bsl_search::ModuleSnapshotSource> =
            Arc::new(ResidentModuleSnapshotSource::new(diagnostics.clone()));
        engine.set_module_snapshot_source(source);

        // Edit on disk (v2, longer): the resident's recorded revision is now stale.
        std::thread::sleep(Duration::from_millis(10));
        fs::write(
            &module,
            "&НаСервере\nФункция Ч() Экспорт Возврат 2; КонецФункции\n\
             Процедура Ещё() Экспорт КонецПроцедуры\n",
        )
        .unwrap();
        assert!(engine.mark_workspace_path_dirty(&module).unwrap());

        // Wait until the hub delivered the edit, so the diagnostics cursor drains it in `catch_up`.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut delivered = false;
        while Instant::now() < deadline {
            let batch = hub.drain(observer);
            observer = batch.cursor;
            if batch.entries.iter().any(|e| e.raw.to_string_lossy().ends_with("Module.bsl")) {
                delivered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(delivered, "the hub delivered the edit");

        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        SharedState::prefetch_resident_overlay_fenced(
            &engine_arc,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("an uncancelled prefetch completes");

        let fed = engine_arc
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .workspace_overlay_resident_fed_count()
            .unwrap();
        assert_eq!(
            fed, 1,
            "catch_up must reconcile the stale resident so the snapshot matches disk (fed reindex)",
        );
    }

    /// The per-query prefetch is capped: marking N + k paths dirty serves exactly N from the
    /// shared parse in one prefetch, and the remaining k stay dirty for the lazy disk path / a
    /// later prefetch. This bounds the query-path work S2 adds.
    #[test]
    fn prefetch_resident_overlay_caps_paths_per_query() {
        use bsl_search::{ModuleSnapshot, ModuleSnapshotSource, SnapshotFetch};

        struct DiskFakeSource;
        impl ModuleSnapshotSource for DiskFakeSource {
            fn text_and_parse(&self, path: &str) -> SnapshotFetch {
                match std::fs::read_to_string(path) {
                    Ok(text) => {
                        let root = parser::parse(&text).syntax_node();
                        SnapshotFetch::Fetched(ModuleSnapshot { text: text.into(), root })
                    }
                    Err(_) => SnapshotFetch::Unavailable,
                }
            }
        }

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let mut engine = SearchEngine::fts_only(&workspace.join("search.db")).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.enable_workspace_watcher_mode();
        engine.prime_workspace_overlay().unwrap();
        engine.set_module_snapshot_source(Arc::new(DiskFakeSource));

        let extra = 3usize;
        let total = MAX_RESIDENT_PREFETCH_PATHS_PER_QUERY + extra;
        for i in 0..total {
            let rel = format!("Module{i}.bsl");
            fs::write(workspace.join(&rel), format!("Процедура П{i}()\nКонецПроцедуры\n")).unwrap();
            assert!(engine.mark_workspace_path_dirty(workspace.join(&rel)).unwrap());
        }

        let engine_arc: super::SharedSearchEngine = Arc::new(Mutex::new(Some(engine)));
        SharedState::prefetch_resident_overlay_fenced(
            &engine_arc,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("an uncancelled prefetch completes");

        let guard = engine_arc.lock().unwrap();
        let engine = guard.as_ref().unwrap();
        assert_eq!(
            engine.workspace_overlay_resident_fed_count().unwrap(),
            MAX_RESIDENT_PREFETCH_PATHS_PER_QUERY,
            "exactly the per-query cap is served from the shared parse",
        );
        assert_eq!(
            engine.workspace_overlay_dirty_paths().unwrap().len(),
            extra,
            "paths beyond the cap stay dirty for the lazy disk path / a later prefetch",
        );
    }
    /// Unit proof of the shared boot reconcile that every Clean branch funnels through
    /// ([`SharedState::reconcile_boot_store_with_disk`]): a store row for a file DELETED while the
    /// daemon was down is reconciled out, while a present file is kept, and the helper reports the
    /// store PROVEN reconciled. The fused / standalone-deferred / FTS-cold Clean branches all call
    /// this exact helper after their index step, so proving it here proves the deletion is removed on
    /// each — without standing up a full graph build for the fused path. Store-level `file_count` is
    /// asserted so the removal is real, not overlay-hidden.
    #[test]
    fn boot_reconcile_removes_deleted_file_keeps_present() {
        // The boot reconcile reads the process-global `FORCE_REWALK_WALK_ERROR` seam; serialize
        // against the walk-error tests that toggle it so a concurrent set can't force a false error.
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        write_common_module_tree(
            &workspace,
            "Улетевший",
            "&НаСервере\nФункция ИсчезнувшийСимвол() Экспорт Возврат 1; КонецФункции\n",
        );
        write_common_module_tree(
            &workspace,
            "Постоянный",
            "&НаСервере\nФункция ЖивойСимвол() Экспорт Возврат 1; КонецФункции\n",
        );

        let db_path = dir.path().join("search.db");
        let mut engine = SearchEngine::fts_only(&db_path).unwrap();
        engine.set_workspace_root(workspace.clone());
        engine.index_directory_fts(&workspace).unwrap();
        assert_eq!(engine.file_count().unwrap(), 2, "both modules are indexed");

        // The Улетевший module vanishes while the daemon is down.
        fs::remove_dir_all(workspace.join("CommonModules").join("Улетевший")).unwrap();
        fs::remove_file(workspace.join("CommonModules").join("Улетевший.xml")).unwrap();

        let reconciled = SharedState::reconcile_boot_store_with_disk_fenced(
            &mut engine,
            &crate::workspace_lease::WorkspaceLease::unmanaged(),
        )
        .expect("the unmanaged reconcile fence cannot refuse");
        assert!(reconciled, "a clean walk proves the store reconciled");
        assert_eq!(
            engine.file_count().unwrap(),
            1,
            "the deleted file's rows are reconciled out of the store",
        );
        let files: Vec<String> = engine
            .store()
            .all_files_in_collection("code")
            .unwrap()
            .into_iter()
            .map(|(key, _hash)| key.path)
            .collect();
        assert!(
            files.iter().any(|p| p.contains("Постоянный")) && files.len() == 1,
            "only the present module survives: {files:?}",
        );
    }
    /// A walk error at boot cannot prove the store was reconciled, so a Clean branch must DOWNGRADE
    /// to a prime rather than assert a false clean. Force the reconcile walk to error and drive a
    /// cold FTS-only boot (otherwise Clean) through the real init path: it must select Prime.
    /// Reverting the downgrade (staying Clean on a failed walk) fails this.
    #[test]
    fn boot_walk_error_downgrades_clean_to_prime() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _embedding_url = EnvVarGuard::unset("EMBEDDING_URL");
        let _embedding_model = EnvVarGuard::unset("EMBEDDING_MODEL");

        let dir = tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        fs::write(
            workspace.join("Configuration.xml"),
            "<Configuration><Name>Конфа</Name></Configuration>",
        )
        .unwrap();
        write_common_module_tree(
            &workspace,
            "Сервер",
            "&НаСервере\nФункция Ч() Экспорт Возврат 1; КонецФункции\n",
        );
        struct ResetWalkErr;
        impl Drop for ResetWalkErr {
            fn drop(&mut self) {
                FORCE_REWALK_WALK_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        FORCE_REWALK_WALK_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        let _reset = ResetWalkErr;

        let init = SharedState::init_workspace_search_engine_unmanaged(
            &workspace,
            None,
            crate::state::WorkspaceSearchMode::SqliteLocal,
            None,
            &crate::graph::GraphState::disabled(),
        )
        .expect("cold FTS-only init produces an engine");
        assert!(
            matches!(init.overlay_init, OverlayInit::Prime),
            "a boot whose reconcile walk errored must prime, not assert a false clean",
        );
    }
}
