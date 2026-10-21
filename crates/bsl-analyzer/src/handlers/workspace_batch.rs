//! Deferred whole-project diagnostics batch — Stream B.
//!
//! rust-analyzer's flycheck analogue for BSL: after the workspace has loaded, every
//! in-scope *closed* file is analysed off the critical path and its diagnostics are
//! pushed, so the Problems panel fills across the whole extension (or configuration)
//! without the open document ever waiting on a whole-workspace pull. Open files are
//! excluded here — they are served live by the interactive per-document stream — so a
//! file is covered by exactly one mechanism and never double-reported.
//!
//! The sweep is **chunked and event-loop-driven**. The file set is frozen once; the
//! event loop dispatches one worker per chunk, and between chunks — on the main thread,
//! with the worker's Salsa snapshot already dropped — it may trim the Salsa LRU and
//! release the worker's parser green-node cache. This bounds resident memory: a single
//! long-lived snapshot never crosses a revision boundary, so Salsa would never trim it
//! and the memos of every file in the corpus would pile up (OOM on a large `all`
//! sweep). Chunking makes the trim points, mirroring the CLI's chunked analyze.
//!
//! A boundary's trim can be **budget-gated**: with a non-zero [`batch_mem_budget_mb`]
//! a boundary trims only while the process is over the budget (a trim cancels
//! in-flight interactive requests and evicts shared memos later chunks re-derive, so
//! skipping it buys time at the price of working set stacking). The final trim runs
//! once on the shrunk sweep LRU profile (parse / lowered-body caps — the sweep's
//! trees are pure batch working set nothing will read again), leaving a lean
//! post-sweep resident set; the interactive caps are restored immediately after.
//!
//! The diff against the last push and the "is this file still closed" handoff check are
//! applied on the event-loop thread against authoritative state, so a file opened
//! mid-batch is handed cleanly to the interactive stream.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use base_db::DiagnosticsConfigInput;
use line_index::LineIndex;
use salsa::Database as _;

#[cfg(test)]
static G1_BATCH_PAUSE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static G1_BATCH_ENTERED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static G1_BATCH_CANCEL_OBSERVED: AtomicU64 = AtomicU64::new(0);

/// Hold the real dispatched batch worker immediately before its actual diagnostics
/// computation, so a test can prove the interactive response arrives while work is live.
#[cfg(test)]
pub(crate) fn g1_batch_barrier(pause: bool) {
    if pause {
        G1_BATCH_ENTERED.store(false, Ordering::Release);
        G1_BATCH_CANCEL_OBSERVED.store(0, Ordering::Release);
    }
    G1_BATCH_PAUSE.store(pause, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn g1_batch_entered() -> bool {
    G1_BATCH_ENTERED.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn g1_batch_is_paused() -> bool {
    G1_BATCH_PAUSE.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn g1_batch_cancel_observed() -> u64 {
    G1_BATCH_CANCEL_OBSERVED.load(Ordering::Acquire)
}

#[cfg(test)]
fn g1_wait_at_batch_barrier(analysis: &ide::Analysis, cancellation_baseline: u64) {
    if !G1_BATCH_PAUSE.load(Ordering::Acquire) {
        return;
    }
    G1_BATCH_ENTERED.store(true, Ordering::Release);
    tracing::info!(g1_marker = "workspace_batch_worker_entered");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    analysis.attached(|analysis| {
        while G1_BATCH_PAUSE.load(Ordering::Acquire) {
            let observed = analysis
                .database()
                .salsa_event_global()
                .map_or(cancellation_baseline, |events| events.set_cancellation);
            if observed > cancellation_baseline {
                G1_BATCH_CANCEL_OBSERVED.store(observed, Ordering::Release);
                // Keep the snapshot attached while polling so Salsa observes the live
                // writer's cancellation flag and unwinds through its real checkpoint.
                analysis.database().unwind_if_revision_cancelled();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "test did not release workspace batch barrier"
            );
            std::thread::yield_now();
        }
    });
}

use crate::frozen_context::FrozenFilePaths;
use crate::global_state::{
    GlobalState, Task, WorkspaceBatchItem, WorkspaceBatchOutcome, WorkspaceBatchPlan,
};
use crate::lsp::PositionEncoding;

/// Files analysed per chunk, overridable via `BSL_BATCH_CHUNK` (mirrors the CLI's
/// `BSL_SALSA_CHUNK`). The chunk is the sweep's memory-vs-time dial: a boundary is
/// where a trim *can* run, so a smaller chunk caps the peak lower (less working set
/// accumulates between trims) but pays more boundary overhead and re-derivation of
/// evicted shared memos — measured ≈ +7% wall for ≈ −20% peak at half this size on a
/// 21k-file corpus. The default favours wall time; lower it on memory-tight machines.
fn batch_chunk_size() -> usize {
    std::env::var("BSL_BATCH_CHUNK")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(500)
}

/// Memory budget in megabytes the sweep keeps the process under, overridable via
/// `BSL_BATCH_MEM_BUDGET_MB`. Chunk boundaries under budget skip the trim, which
/// buys wall time (skipped evictions, no re-derivation of shared memos, no
/// interactive cancellations) at the price of working set stacking across the
/// skipped boundaries: RSS overshoots the budget by several chunks' worth before
/// the next trim lands, and once RSS crosses the corpus' durable floor the tail
/// trims every boundary anyway. Measured on a 21k-file corpus: ≈ −3% wall for
/// ≈ +27% peak. That trade is opt-in — the `0` default trims at every boundary,
/// keeping the peak deterministic at roughly one chunk's working set above the
/// post-trim floor.
fn batch_mem_budget_mb() -> usize {
    std::env::var("BSL_BATCH_MEM_BUDGET_MB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0)
}

/// Whether the process is over the sweep's memory budget. Measures RSS where
/// readable — the number the user (and the OOM killer) actually sees — falling back
/// to the allocator's live bytes, which trail RSS by the allocator's retention. A
/// zero budget always reads as over (trim at every opportunity), and so does an
/// unreadable measurement (platforms with neither source), degrading to the
/// always-trim behaviour rather than to unbounded growth.
pub(crate) fn over_mem_budget(budget_mb: usize) -> bool {
    if budget_mb == 0 {
        return true;
    }
    if let Some(rss_bytes) = crate::mem_report::process_rss_bytes() {
        return (rss_bytes / 1048576) as usize > budget_mb;
    }
    let allocated_mb = profile::memory_usage().allocated.megabytes();
    allocated_mb <= 0 || allocated_mb as usize > budget_mb
}

/// Threads the batch's bounded rayon pool uses. Defaults to half the cores (rounded down,
/// at least one) so a whole-project sweep leaves the other half for interactive requests;
/// overridable via `BSL_BATCH_WORKERS`. Scales up on a big build machine, down on a laptop.
fn batch_worker_count() -> usize {
    if let Some(n) = std::env::var("BSL_BATCH_WORKERS").ok().and_then(|v| v.parse::<usize>().ok()) {
        return n.max(1);
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    (cores / 2).max(1)
}

/// Drive the deferred whole-project diagnostics batch (Stream B): resume the active
/// sweep by dispatching its next chunk, or start a fresh sweep if one was requested.
/// Idempotent and cheap when idle: a no-op unless the workspace has loaded, no chunk is
/// already running, the pool has a free worker, and either a sweep is in progress or
/// `workspace_batch_dirty` is set.
pub fn spawn_workspace_batch(state: &mut GlobalState) {
    if state.workspace_batch_in_flight || !state.vfs_done || !state.task_pool.pool.has_capacity() {
        return;
    }

    // While the vendor-diff scope is still being computed, a sweep would publish
    // the full unfiltered result set the feature exists to prevent — and be torn
    // down moments later. Stay dirty; the scope's completion re-drives the loop.
    if state.analysis_scope.is_loading() {
        return;
    }

    // Resume the active sweep; otherwise build a fresh one if a (re)run was requested.
    if state.workspace_batch_plan.is_none() {
        if !state.workspace_batch_dirty {
            return;
        }
        state.workspace_batch_dirty = false;
        match build_batch_plan(state) {
            Some(plan) => state.workspace_batch_plan = Some(plan),
            None => return,
        }
    }

    dispatch_next_chunk(state);
}

/// Enumerate the in-scope closed file set and freeze it into a fresh sweep plan. Returns
/// `None` when the feature is off or there is nothing to sweep. Bumps the batch
/// generation (so any late chunk from a prior sweep is dropped) and resets the
/// reported-set the completion reconcile diffs against.
fn build_batch_plan(state: &mut GlobalState) -> Option<WorkspaceBatchPlan> {
    let scope =
        state.project.as_ref().map(|p| p.config.features.workspace_diagnostics).unwrap_or_default();
    if !scope.is_enabled() {
        return None;
    }

    let ext_roots: Vec<PathBuf> = state
        .project
        .as_ref()
        .map(|p| p.extension_paths().iter().map(|(_, path)| path.clone()).collect())
        .unwrap_or_default();

    let file_paths = {
        let analysis = state.analysis_host.analysis();
        FrozenFilePaths::freeze_database(analysis.database())
    };

    // In-scope closed files only: open buffers are owned by the interactive stream.
    // Sorted for a stable, log-friendly sweep order.
    let ext_roots_ref: Vec<&Path> = ext_roots.iter().map(|p| p.as_path()).collect();
    // Vendor-diff file-gate: files with no changed lines are excluded up front so
    // the sweep never spends provider setup on thousands of guaranteed-empty files.
    let analysis_scope = state.analysis_scope.current();
    let mut file_ids: Vec<vfs::FileId> = file_paths
        .iter()
        .filter(|&(file_id, path)| {
            !state.open_files.contains(&file_id)
                && crate::handlers::request::path_in_workspace_scope(path, scope, &ext_roots_ref)
                && analysis_scope.as_ref().is_none_or(|s| s.is_file_in_scope(path))
        })
        .map(|(file_id, _)| file_id)
        .collect();
    file_ids.sort_by_key(|f| f.0);

    if file_ids.is_empty() {
        // Nothing in scope to sweep (every in-scope file is open, or the last closed one
        // was deleted / left scope). A prior sweep may have pushed diagnostics for those
        // files — clear them so they do not linger, since no plan means no finalize.
        state.clear_all_batch_pushed();
        return None;
    }

    let chunk_size = batch_chunk_size();
    let total = file_ids.len();
    let num_chunks = total.div_ceil(chunk_size);

    // Bounded pool the chunks compute on. A failure to spawn threads degrades to the serial
    // per-file loop rather than losing the sweep.
    let workers = batch_worker_count();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|i| format!("bsl-batch-{i}"))
        .build()
        .map(Arc::new)
        .map_err(|err| tracing::warn!(?err, "workspace batch: pool build failed; running serially"))
        .ok();

    state.workspace_batch_generation = state.workspace_batch_generation.wrapping_add(1);
    state.batch_reported.clear();
    let generation = state.workspace_batch_generation;
    let analysis_guard = state.note_analysis_spawned();

    tracing::info!(scope = ?scope, files = total, chunks = num_chunks, workers, "workspace diagnostics batch spawned");

    Some(WorkspaceBatchPlan {
        generation,
        file_ids: Arc::new(file_ids),
        file_paths,
        config: state.diagnostics_config().clone(),
        diagnostics_baseline: Arc::clone(&state.diagnostics_baseline),
        workspace_root: state.workspace_root.clone(),
        position_encoding: state.position_encoding,
        supports_code_description: state.supports_code_description,
        supports_diagnostic_tags: state.supports_diagnostic_tags,
        chunk_size,
        pool,
        next_chunk: 0,
        num_chunks,
        mem_budget_mb: batch_mem_budget_mb(),
        chunks_since_trim: 0,
        chunk_retries: 0,
        started_at: Instant::now(),
        analysis_guard,
    })
}

/// Dispatch the plan's current chunk to a single background worker. The worker computes
/// its chunk's diagnostics on a fresh Salsa snapshot, releases its parser green-node
/// cache, and returns a [`Task::WorkspaceBatchChunk`]; the event loop applies the push,
/// trims the LRU, and advances the cursor.
fn dispatch_next_chunk(state: &mut GlobalState) {
    let (
        generation,
        chunk_index,
        config,
        diagnostics_baseline,
        workspace_root,
        file_paths,
        position_encoding,
        supports_code_description,
        supports_diagnostic_tags,
        pool,
        chunk,
    ) = {
        let plan = state.workspace_batch_plan.as_ref().expect("plan present when dispatching");
        let start = plan.next_chunk * plan.chunk_size;
        let end = (start + plan.chunk_size).min(plan.file_ids.len());
        let chunk: Vec<vfs::FileId> = plan.file_ids[start..end].to_vec();
        (
            plan.generation,
            plan.next_chunk,
            plan.config.clone(),
            Arc::clone(&plan.diagnostics_baseline),
            plan.workspace_root.clone(),
            plan.file_paths.clone(),
            plan.position_encoding,
            plan.supports_code_description,
            plan.supports_diagnostic_tags,
            plan.pool.clone(),
            chunk,
        )
    };

    let analysis = state.analysis_host.analysis();
    #[cfg(test)]
    let cancellation_baseline =
        analysis.database().salsa_event_global().map_or(0, |events| events.set_cancellation);
    state.workspace_batch_token = Some(analysis.database().cancellation_token());

    state.workspace_batch_in_flight = true;

    let spawned = state.task_pool.pool.try_spawn(move || {
        let started_at = Instant::now();
        // Catch everything so the worker always reports back and never wedges the batch.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            g1_wait_at_batch_barrier(&analysis, cancellation_baseline);
            compute_chunk(
                &analysis,
                &chunk,
                &config,
                (&diagnostics_baseline, workspace_root.as_deref()),
                &file_paths,
                (
                    position_encoding,
                    crate::lsp::to_proto::CodeDescriptions::from_client_support(
                        supports_code_description,
                    ),
                    supports_diagnostic_tags,
                ),
                pool.as_deref(),
            )
        }));

        // Release the parser green-node cache (a thread-local not owned by Salsa) so the
        // chunk's parsed trees are not pinned across chunks. Clear both this driver thread
        // and — since the parallel compute parses on the pool's threads — every pool
        // worker. Carrying these caches across chunks was measured a wash on compute time
        // while inflating the peak by pinning every chunk's trees since the last trim.
        // The Salsa parse memos are trimmed separately by the event loop via
        // `enforce_lru`, which needs the exclusive `&mut` this snapshot blocks — hence it
        // runs there, after this worker (and its snapshots) are gone.
        syntax::clear_shared_node_cache();
        if let Some(pool) = pool.as_deref() {
            pool.broadcast(|_| syntax::clear_shared_node_cache());
        }

        let outcome = match result {
            Ok(items) => {
                tracing::debug!(
                    chunk = chunk_index,
                    files = items.len(),
                    elapsed_ms = started_at.elapsed().as_millis() as u64,
                    "workspace batch chunk computed",
                );
                WorkspaceBatchOutcome::Computed(items)
            }
            // Classify the unwind. `PendingWrite` is a concurrent edit's revision bump —
            // retry the chunk once it settles. `PropagatedPanic` is ambiguous under
            // parallelism (a sibling blocked on an edit-cancelled query gets it too, not
            // only a real panic), so retry it under a bounded budget. A non-cancellation
            // panic is deterministic — skip the chunk rather than loop forever.
            Err(payload) => match payload.downcast_ref::<salsa::Cancelled>() {
                Some(salsa::Cancelled::PendingWrite) => {
                    tracing::debug!(chunk = chunk_index, "workspace batch chunk cancelled by an edit; will retry");
                    WorkspaceBatchOutcome::Cancelled
                }
                Some(_) => {
                    tracing::debug!(chunk = chunk_index, "workspace batch chunk unwound on a propagated panic; will retry within budget");
                    WorkspaceBatchOutcome::Propagated
                }
                None => {
                    tracing::error!(chunk = chunk_index, "workspace batch chunk panicked; skipping");
                    WorkspaceBatchOutcome::Failed
                }
            },
        };
        Task::WorkspaceBatchChunk { generation, outcome }
    });

    if spawned.is_err() {
        // Unreachable after the capacity check in `spawn_workspace_batch`; leave the plan
        // in place and retry on the next loop turn rather than lose the sweep.
        tracing::warn!("task pool rejected workspace batch chunk; will retry");
        state.workspace_batch_in_flight = false;
        state.workspace_batch_token = None;
    }
}

/// Compute one chunk's diagnostics on the worker's snapshot. Each closed file has no
/// overlay, so its text is read disk-backed from the db; that read can panic if the file
/// was deleted or rewritten mid-sweep, which is caught and skips just that file (mirroring
/// the pull sweep), while a `salsa::Cancelled` keeps unwinding to abort the whole chunk.
fn compute_chunk(
    analysis: &ide::Analysis,
    chunk: &[vfs::FileId],
    config: &DiagnosticsConfigInput,
    baseline: (&ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot, Option<&Path>),
    file_paths: &FrozenFilePaths,
    // Согласованное с клиентом представление: кодировка позиций, приём
    // `codeDescription` и множество принимаемых `Diagnostic.tags`. Тройка, а не
    // отдельные параметры, — все величины приходят из одного рукопожатия и всегда
    // передаются вместе.
    projection: (
        PositionEncoding,
        crate::lsp::to_proto::CodeDescriptions,
        crate::lsp::to_proto::ClientTags,
    ),
    pool: Option<&rayon::ThreadPool>,
) -> Vec<WorkspaceBatchItem> {
    // Diagnostics — the heavy per-file work — run in parallel on the bounded pool when one
    // is available, else serially. The LSP conversion below stays serial (cheap relative to
    // diagnostics), reading disk-backed text from the shared snapshot.
    let computed = match pool {
        Some(pool) => analysis.workspace_diagnostics_parallel(chunk, config.clone(), pool),
        None => analysis.workspace_diagnostics(chunk, config.clone()),
    };
    let baseline_applies = crate::diagnostics_baseline::applies_under_scope(config.scope.is_some());
    computed
        .into_iter()
        .filter_map(|(file_id, diagnostics)| {
            let uri = file_paths.url_for_file_id(file_id).ok()?;
            let converted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let text = analysis.file_text(file_id);
                let diagnostics = match (
                    baseline.1.filter(|_| baseline_applies),
                    file_paths.path_for_file_id(file_id),
                ) {
                    (Some(root), Some(path)) => crate::diagnostics_baseline::active_for_file(
                        baseline.0,
                        root,
                        path,
                        &text,
                        diagnostics.to_vec(),
                    ),
                    _ => diagnostics.to_vec(),
                };
                let result_id = crate::lsp::diagnostics_result_id(&diagnostics);
                let line_index = LineIndex::new(&text);
                let diagnostics = crate::lsp::to_proto::diagnostics_with_encoding(
                    &line_index,
                    &text,
                    &diagnostics,
                    projection.0,
                    projection.1,
                    projection.2,
                );
                (result_id, diagnostics)
            }));
            let (result_id, lsp) = match converted {
                Ok(converted) => converted,
                Err(payload) if payload.is::<salsa::Cancelled>() => {
                    std::panic::resume_unwind(payload)
                }
                Err(_) => {
                    tracing::warn!(
                        file_id = file_id.0,
                        "workspace batch: skipping file after a text-read panic"
                    );
                    return None;
                }
            };
            Some(WorkspaceBatchItem { uri, result_id, diagnostics: lsp })
        })
        .collect()
}

#[cfg(test)]
mod shared_owner_tests {
    use super::*;
    use crate::global_state::GlobalState;
    use crate::lsp::to_proto::{ClientTags, CodeDescriptions};
    use ide::Analysis;
    use lsp_types::NumberOrString;
    use mcp_server::{SharedState, WorkspaceHandle, WorkspaceRead};

    fn wait_for_owner_analysis(owner: &WorkspaceHandle) -> Analysis {
        let deadline =
            Instant::now() + test_utils::wait::hang_guard(std::time::Duration::from_secs(20));
        loop {
            match owner.request_analysis() {
                WorkspaceRead::Ready(analysis) => return analysis,
                WorkspaceRead::Loading | WorkspaceRead::Busy => {
                    assert!(Instant::now() < deadline, "shared owner did not become ready");
                    std::thread::yield_now();
                }
                WorkspaceRead::RejectedScope(reason) | WorkspaceRead::Failed(reason) => {
                    panic!("shared owner failed to load: {reason}");
                }
                WorkspaceRead::Disabled => panic!("shared owner is disabled"),
            }
        }
    }

    /// A retry compares the revision a request ran at with the current one; both must come
    /// from the store requests run on. Without an editor view that is the owner's database,
    /// and the session's fallback counts revisions of its own.
    #[test]
    fn a_shared_request_revision_reads_the_store_requests_run_on() {
        let root = tempfile::tempdir().expect("create shared workspace");
        std::fs::write(root.path().join("bsl-analyzer.toml"), "[source]\nroot = \".\"\n")
            .expect("write project config");
        std::fs::write(root.path().join("Module.bsl"), "Процедура П() КонецПроцедуры\n")
            .expect("write source");
        let project = mcp_server::project::at(root.path()).expect("load test workspace project");
        let cache_parent = tempfile::tempdir().expect("create disposable workspace cache");
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(cache_parent.path()),
            root.path(),
            None,
        )
        .expect("create explicit disposable cache");
        let backend = SharedState::shared_workspace_with_cache_and_prefixes(
            root.path().to_path_buf(),
            cache,
            None,
        )
        .expect("create disk-only owner for test workspace");
        let owner = backend.workspace_handle();
        drop(wait_for_owner_analysis(&owner));
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let state = GlobalState::new_shared(sender, owner);
        assert!(state.analysis_host.view().is_none());

        let request = salsa::plumbing::current_revision(state.analysis_host.analysis().database());

        assert_eq!(state.analysis_host.request_revision(), request);
        assert_ne!(
            salsa::plumbing::current_revision(state.analysis_host.raw_database()),
            request,
            "without a view the fallback database is a different store"
        );
    }

    #[test]
    fn shared_batch_reports_closed_disk_file_from_owner_without_session_vfs_copy() {
        let root = tempfile::tempdir().expect("create shared workspace");
        std::fs::write(
            root.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\n\n[features]\nworkspaceDiagnostics = \"all\"\n",
        )
        .expect("write project config");
        let closed_file = root.path().join("Closed.bsl");
        std::fs::write(
            &closed_file,
            "Процедура Ошибка() Экспорт\nЕсли НЕ Тогда\nКонецЕсли;\nКонецПроцедуры\n",
        )
        .expect("write closed source");

        let project = mcp_server::project::at(root.path()).expect("load test workspace project");
        let cache_parent = tempfile::tempdir().expect("create disposable workspace cache");
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(cache_parent.path()),
            root.path(),
            None,
        )
        .expect("create explicit disposable cache");
        let backend = SharedState::shared_workspace_with_cache_and_prefixes(
            root.path().to_path_buf(),
            cache,
            None,
        )
        .expect("create disk-only owner for test workspace");
        let owner = backend.workspace_handle();
        drop(wait_for_owner_analysis(&owner));
        let project = match owner.project() {
            WorkspaceRead::Ready(project) => project,
            WorkspaceRead::RejectedScope(reason) | WorkspaceRead::Failed(reason) => {
                panic!("workspace project failed to load: {reason}");
            }
            WorkspaceRead::Loading | WorkspaceRead::Busy | WorkspaceRead::Disabled => {
                panic!("workspace project should be available during owner loading");
            }
        };

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new_shared(sender, owner.clone());
        state.vfs_done = true;
        state.workspace_root = Some(root.path().to_path_buf());
        state.project = Some(project);
        state.update_diagnostics_config();
        assert!(
            FrozenFilePaths::freeze(&state.vfs.read()).is_empty(),
            "the session VFS deliberately contains no closed disk files"
        );

        let plan = build_batch_plan(&mut state).expect("enabled batch includes the owner file");
        let expected_uri = lsp_types::Url::from_file_path(&closed_file).unwrap();
        assert_eq!(plan.file_ids.len(), 1);
        let file_id = plan.file_ids[0];
        assert_eq!(plan.file_paths.url_for_file_id(file_id).unwrap(), expected_uri);

        let analysis = state.analysis_host.analysis();
        let owner_token = analysis.database().cancellation_token();
        state.workspace_batch_token = Some(owner_token.clone());
        let items = compute_chunk(
            &analysis,
            &[file_id],
            &plan.config,
            (&plan.diagnostics_baseline, plan.workspace_root.as_deref()),
            &plan.file_paths,
            (
                plan.position_encoding,
                CodeDescriptions::from_client_support(plan.supports_code_description),
                ClientTags::NONE,
            ),
            None,
        );
        assert_eq!(items.len(), 1, "the owner-backed batch produced the closed file result");
        assert_eq!(items[0].uri, expected_uri);
        assert!(
            items[0].diagnostics.iter().any(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String("ParseError".to_owned()))
            }),
            "closed source should report its parser diagnostic: {:?}",
            items[0].diagnostics
        );
        drop(analysis);
        assert!(
            !owner_token.is_cancelled(),
            "dropping the request Analysis must not cancel the owner token"
        );

        let next_analysis = wait_for_owner_analysis(&owner);
        assert_eq!(
            FrozenFilePaths::freeze_database(next_analysis.database())
                .file_id_for_url(&expected_uri)
                .unwrap(),
            file_id,
            "the owner serves the same accepted file after the request snapshot is dropped"
        );
        drop(next_analysis);
        drop(state);
        drop(owner);
        drop(backend);
    }

    #[cfg(windows)]
    #[test]
    fn shared_admission_open_uri_case_alias_resolves_after_external_file_deletion() {
        let root = tempfile::tempdir().expect("create shared workspace");
        std::fs::write(
            root.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\n[platform_help]\nsource = \"bundled\"\n",
        )
        .expect("write project config");
        let path = root.path().join("Alias.bsl");
        let text = "Процедура ОткрытыйДокумент()\nКонецПроцедуры\n";
        std::fs::write(&path, text).expect("write open source");

        let project = mcp_server::project::at(root.path()).expect("load test workspace project");
        let cache_parent = tempfile::tempdir().expect("create disposable workspace cache");
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(cache_parent.path()),
            root.path(),
            None,
        )
        .expect("create explicit disposable cache");
        let backend = SharedState::shared_workspace_with_cache_and_prefixes(
            root.path().to_path_buf(),
            cache,
            None,
        )
        .expect("create disk-only owner for test workspace");
        let owner = backend.workspace_handle();
        drop(wait_for_owner_analysis(&owner));
        let project = match owner.project() {
            WorkspaceRead::Ready(project) => project,
            WorkspaceRead::RejectedScope(reason) | WorkspaceRead::Failed(reason) => {
                panic!("workspace project failed to load: {reason}");
            }
            WorkspaceRead::Loading | WorkspaceRead::Busy | WorkspaceRead::Disabled => {
                panic!("workspace project should be available during owner loading");
            }
        };

        let original_component = root.path().file_name().unwrap().to_string_lossy();
        let alias_component = original_component
            .chars()
            .map(|ch| {
                if ch.is_ascii_lowercase() {
                    ch.to_ascii_uppercase()
                } else if ch.is_ascii_uppercase() {
                    ch.to_ascii_lowercase()
                } else {
                    ch
                }
            })
            .collect::<String>();
        assert_ne!(original_component, alias_component, "fixture path has a case alias");
        let alias_path = root.path().with_file_name(alias_component).join("aLIAS.BSL");
        let alias_uri = lsp_types::Url::from_file_path(alias_path).unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new_shared(sender, owner.clone());
        state.vfs_done = true;
        state.workspace_root = Some(root.path().to_path_buf());
        state.project = Some(project);
        crate::handlers::notification::handle_did_open(
            &mut state,
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: alias_uri.clone(),
                    language_id: "bsl".to_owned(),
                    version: 1,
                    text: text.to_owned(),
                },
            },
        )
        .expect("open the case-aliased URI through the shared VFS");
        let file_id = crate::lsp::file_id(&state, &alias_uri).expect("resolve the opened URI");

        std::fs::remove_file(&path).expect("delete the disk file while its overlay is open");
        assert_eq!(
            crate::lsp::file_id(&state, &alias_uri).unwrap(),
            file_id,
            "the exact open URI binding survives parent and filename case changes after deletion"
        );
        assert_eq!(state.vfs_file_for_url(&alias_uri).unwrap(), file_id);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.file_id_for_url(&alias_uri).unwrap(), file_id);
        state.open_files.remove(&file_id);
        assert!(
            state.snapshot().file_id_for_url(&alias_uri).is_err(),
            "an unadmitted buffer must not answer through its retained URI binding"
        );
        state.open_files.insert(file_id);
        let analysis = state.analysis_host.view().expect("open editor view").analysis();
        assert_eq!(&*analysis.file_text(file_id), text, "the open overlay remains authoritative");
        drop(analysis);

        crate::handlers::notification::handle_did_close(
            &mut state,
            lsp_types::DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: alias_uri.clone() },
            },
        )
        .expect("close the alias URI");
        assert!(!state.mem_docs.contains(&alias_uri));
        assert!(!state.open_file_uri_ids.contains_key(&alias_uri));
        assert!(state.analysis_host.view().is_none(), "closed overlay is released");
        assert!(
            state.snapshot().file_id_for_url(&alias_uri).is_err(),
            "the closed URI no longer has an active alias binding"
        );
        drop(state);
        drop(owner);
        drop(backend);
    }
}
