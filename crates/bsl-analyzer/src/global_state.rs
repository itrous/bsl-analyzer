use std::collections::{BTreeSet, HashMap};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};

use base_db::{DiagnosticsConfigInput, Locale};
use crossbeam_channel::{Receiver, Sender};
use hir::CallHierarchyReverseIndex;
use ide::Analysis;
use lsp_server::{Message, ReqQueue, Response};
use lsp_types::{PublishDiagnosticsParams, Url};
use parking_lot::RwLock;
use project_model::Project;
use rustc_hash::FxHashSet;
use vfs::loader::Handle;
use vfs::{loader, Vfs};

use crate::analysis_host::AnalysisHost;
use crate::call_hierarchy_index_state::CallHierarchyIndexState;
use crate::lsp::to_proto::ClientTags;
use crate::lsp::{PositionEncoding, Progress};
use crate::mem_docs::MemDocs;
use crate::task_pool;

/// How long background analysis must stay busy before the "Analyzing" indicator
/// appears. Fast per-file opens finish within this window and show nothing.
const ANALYSIS_PROGRESS_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a baseline error read off an empty input is held before it is announced.
///
/// Long enough for a truncating write to land its bytes even on a loaded machine, so
/// the look that follows sees what the author saved; short enough that a file which is
/// really empty is still reported promptly. The hold is paid off the loop, as a task,
/// so a loop answering keystrokes never waits it out.
pub(crate) const DIAGNOSTICS_BASELINE_HOLD: std::time::Duration =
    std::time::Duration::from_millis(250);

/// Bound on the one incoming-call request allowed to await a compact-index build.
#[derive(Debug, Clone, Copy)]
pub struct CallHierarchyWaitPolicy {
    pub timeout: std::time::Duration,
}

impl Default for CallHierarchyWaitPolicy {
    fn default() -> Self {
        Self { timeout: std::time::Duration::from_secs(2) }
    }
}

/// Defers the call-hierarchy lifecycle allocation until the first ready prepare request.
#[derive(Debug, Default)]
pub struct DeferredCallHierarchyIndexState {
    inner: OnceLock<CallHierarchyIndexState>,
}

impl DeferredCallHierarchyIndexState {
    pub(crate) fn active(&self) -> Option<&CallHierarchyIndexState> {
        self.inner.get()
    }

    pub(crate) fn ensure(&self) -> CallHierarchyIndexState {
        self.inner.get_or_init(CallHierarchyIndexState::default).clone()
    }
}

impl Deref for DeferredCallHierarchyIndexState {
    type Target = CallHierarchyIndexState;

    fn deref(&self) -> &Self::Target {
        self.inner.get_or_init(CallHierarchyIndexState::default)
    }
}

#[derive(Debug)]
pub enum Task {
    DependenciesPreloaded {
        file_id: vfs::FileId,
        count: usize,
    },
    DiagnosticsReady {
        uri: Url,
        diagnostics: Vec<lsp_types::Diagnostic>,
        generation: u64,
        completed_at: std::time::Instant,
    },
    DiagnosticsCancelled {
        generation: u64,
        completed_at: std::time::Instant,
    },
    PreloadExternalFiles {
        files: Vec<vfs::FileId>,
    },
    RequestResult {
        response: Response,
    },
    /// Debounce timer fired for a background-analysis busy burst. If the burst
    /// (`epoch`) is still the current one and still running, it promotes to a
    /// `window/workDoneProgress` "Analyzing" indicator; otherwise it is a no-op.
    AnalysisProgressTick {
        epoch: u64,
    },
    /// One background analysis job finished (returned normally or unwound).
    /// Posted by [`AnalysisGuard`] on drop so the in-flight counter is always
    /// balanced even if the job panicked before producing its result task.
    AnalysisJobFinished,
    /// A background vendor-diff analysis-scope build finished. Applied on the
    /// event loop only when `generation` matches the in-flight build.
    AnalysisScopeReady {
        generation: u64,
        result: Result<Arc<base_db::AnalysisScope>, String>,
        /// Resolved (base, HEAD) OIDs at build time; the idle tick compares
        /// against them to notice ref-only movement with no file events.
        identity: Option<(String, String)>,
    },
    /// One chunk of the deferred whole-project diagnostics batch (Stream B) finished on
    /// a worker; the [`WorkspaceBatchOutcome`] says what to do next. The event loop
    /// applies the push (diffed against the last one, skipping files opened since the
    /// batch started — those belong to the interactive stream), trims the Salsa LRU (the
    /// worker's snapshot is dropped by the time this arrives) and advances the sweep,
    /// finalizing when the plan's file set is exhausted.
    WorkspaceBatchChunk {
        generation: u64,
        outcome: WorkspaceBatchOutcome,
    },
    /// A compact reverse call index build completed on a worker. Publication is
    /// generation-checked on the event loop, never through Salsa.
    CallHierarchyIndexBuilt {
        source_root: base_db::SourceRootId,
        generation: u64,
        index: Arc<CallHierarchyReverseIndex>,
    },
    /// A compact reverse call index build failed before publication.
    CallHierarchyIndexFailed {
        source_root: base_db::SourceRootId,
        generation: u64,
        reason: String,
    },
    /// A compact reverse call index build gave its generation back without publishing.
    /// `reason` says why — a panic, a lost generation, an exhausted catch-up budget —
    /// because the loop's answer is the same for all of them and nothing else would
    /// tell them apart.
    CallHierarchyIndexSuperseded {
        source_root: base_db::SourceRootId,
        generation: u64,
        reason: &'static str,
    },
    /// A successful `callHierarchy/prepare` authorized this generation to build.
    CallHierarchyIndexBuildRequested {
        source_root: base_db::SourceRootId,
        generation: u64,
    },
    /// The hold on a baseline error read off an empty input ran out: look at the
    /// baseline again and announce the error if the input still reads the same.
    /// `hold` says which hold the look belongs to.
    DiagnosticsBaselineRecheck {
        hold: u64,
    },
}

/// What a finished batch chunk carries back to the event loop.
#[derive(Debug)]
pub enum WorkspaceBatchOutcome {
    /// The chunk computed; each item is a closed file's URI, the hash of its computed
    /// diagnostics, and the LSP diagnostics.
    Computed(Vec<WorkspaceBatchItem>),
    /// A concurrent edit's revision bump cancelled the chunk mid-flight
    /// (`salsa::Cancelled::PendingWrite`). The plan and cursor are kept and the SAME
    /// chunk is retried once the edit settles, so coverage is not lost. Never counts as a
    /// fault (an edit is not a defect), so it is retried without bound.
    Cancelled,
    /// A worker unwound on `salsa::Cancelled::PropagatedPanic` — ambiguous under
    /// parallelism: it is raised both for a genuine deterministic panic in a shared query
    /// AND for a sibling worker blocked on a query owned by a thread that unwound on a
    /// (transient) edit cancellation. Retried like [`Self::Cancelled`] but with a bounded
    /// budget, so a transient cascade recovers while a real deterministic panic is skipped
    /// after the budget is spent rather than looping forever.
    Propagated,
    /// The chunk unwound on a stray non-cancellation panic — deterministic, so retrying
    /// would loop forever. The chunk is skipped (cursor advanced) and its files go
    /// uncovered until the next full sweep.
    Failed,
}

/// One file's result within a deferred whole-project diagnostics batch (Stream B).
#[derive(Debug)]
pub struct WorkspaceBatchItem {
    pub uri: Url,
    pub result_id: String,
    pub diagnostics: Vec<lsp_types::Diagnostic>,
}

/// An in-progress whole-project diagnostics sweep (Stream B). The file set is frozen
/// once at the start; the event loop walks it chunk by chunk, dispatching one worker
/// per chunk and trimming the Salsa LRU between chunks so only a single chunk's working
/// set is resident at a time (a single long-lived snapshot never crosses a revision
/// boundary, so Salsa would otherwise never trim — the whole reason the batch is
/// chunked). `next_chunk` is the cursor into `file_ids`; on completion `batch_pushed`
/// is reconciled against `batch_reported` to clear files that left scope.
pub struct WorkspaceBatchPlan {
    pub generation: u64,
    pub file_ids: Arc<Vec<vfs::FileId>>,
    pub file_paths: crate::frozen_context::FrozenFilePaths,
    pub config: DiagnosticsConfigInput,
    pub diagnostics_baseline: Arc<ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot>,
    pub workspace_root: Option<PathBuf>,
    pub position_encoding: PositionEncoding,
    pub supports_code_description: bool,
    /// Tags the client accepts (`tagSupport`), captured with the same handshake as
    /// `supports_code_description`; the batch projects through the same rules.
    pub supports_diagnostic_tags: ClientTags,
    pub chunk_size: usize,
    /// Bounded rayon pool (≈ `ncpu/2`) the chunk computes on, so the batch never saturates
    /// the cores interactive requests need. `None` if pool creation failed — the sweep
    /// then falls back to the serial per-file loop. Shared into each chunk worker.
    pub pool: Option<Arc<rayon::ThreadPool>>,
    pub next_chunk: usize,
    pub num_chunks: usize,
    /// Memory budget in megabytes the sweep tries to stay under, measured as process
    /// RSS where readable (allocator live bytes as the fallback). A chunk boundary
    /// trims only while the measurement exceeds this (a boundary under budget is
    /// nearly free, and the retained memos accelerate later chunks); `0` trims at
    /// every opportunity.
    pub mem_budget_mb: usize,
    /// Chunks that completed over the memory budget without a trim. The trim is
    /// deferred while interactive analysis is in flight (it would cancel those
    /// requests), so this can exceed one; a force threshold bounds it. Reset to zero
    /// on each trim and whenever the heap drops back under budget.
    pub chunks_since_trim: u32,
    /// Consecutive `PropagatedPanic` unwinds of the current chunk. That variant is
    /// ambiguous (a transient edit-cancellation cascade or a genuine deterministic panic),
    /// so the chunk is retried up to a budget and then skipped. Reset when the cursor
    /// advances (a computed or skipped chunk).
    pub chunk_retries: u32,
    pub started_at: std::time::Instant,
    /// Held for the batch's whole lifetime so the "Analyzing" indicator shows once for
    /// the sweep rather than flickering per chunk; dropped when the plan is cleared.
    pub analysis_guard: AnalysisGuard,
}

/// RAII token for one in-flight background analysis job. It is moved into the
/// spawned closure; whether the job returns normally or panics, dropping it posts
/// a [`Task::AnalysisJobFinished`] back to the event loop. This guarantees the
/// in-flight counter is decremented (and the "Analyzing" indicator cannot stick
/// open) without depending on the job's own result task being produced.
pub struct AnalysisGuard {
    sender: Sender<Task>,
}

impl Drop for AnalysisGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(Task::AnalysisJobFinished);
    }
}

pub struct GlobalState {
    pub sender: Sender<Message>,
    pub req_queue: ReqQueue<(), ()>,
    pub analysis_host: AnalysisHost,
    pub vfs: Arc<RwLock<Vfs>>,
    pub mem_docs: MemDocs,
    pub workspace_root: Option<PathBuf>,
    pub project: Option<Project>,
    /// The project's `[source].exclude`, also spelled under each source root the
    /// loader walks — the one test every way into the VFS asks.
    pub(crate) source_exclusions: stdx::path_exclusion::ExcludedPaths,
    pub diagnostics_baseline: Arc<ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot>,
    pub(crate) diagnostics_baseline_notification_ledger: BTreeSet<String>,
    /// Baseline errors seen once on an input read as empty and not yet announced.
    ///
    /// A truncating write is empty between its `open` and its `write`, and a load that
    /// lands there reads content nobody saved. The ledger keys errors by their content,
    /// so announcing that reading and then the bytes the author meant would report one
    /// save twice. Such an error is held here instead, and a look scheduled
    /// [`DIAGNOSTICS_BASELINE_HOLD`] later announces it only if the input still reads
    /// the same: a file that is really empty is reported once, late by the hold; a
    /// write caught halfway is reported once, for the bytes it ended with.
    pub(crate) diagnostics_baseline_held: BTreeSet<String>,
    /// Which hold is current. A look carries the hold it was scheduled for, and a look
    /// that arrives for an earlier hold is dropped: a hold that ended and a new one that
    /// started in the meantime is owed its whole wait, not the remainder of the old one.
    pub(crate) diagnostics_baseline_hold: u64,
    pub shutdown_requested: bool,

    pub loader_receiver: Receiver<loader::Message>,
    pub loader: Box<dyn loader::Handle>,
    pub vfs_progress_config_version: u32,
    pub vfs_done: bool,
    /// When the baseline's ground was last compared with disk.
    ///
    /// The comparison is a handful of stats, and it stands on the loop's hottest path —
    /// one keystroke is one message. A floor keeps it off every message without weakening
    /// what it buys: a baseline that moved is noticed within the floor, which is orders
    /// of magnitude sooner than the watcher's own arming window it exists to cover.
    pub(crate) diagnostics_baseline_checked_at: Option<std::time::Instant>,
    pub task_pool: task_pool::Handle<Task>,
    /// Non-Salsa lifecycle for compact call-hierarchy reverse indexes. Snapshots
    /// clone this handle for readers; orchestration remains on GlobalState.
    pub call_hierarchy_index: DeferredCallHierarchyIndexState,
    pub call_hierarchy_index_rebuilds: FxHashSet<base_db::SourceRootId>,
    pub call_hierarchy_wait_policy: CallHierarchyWaitPolicy,
    pub(crate) next_request_id: AtomicI32,
    pub(crate) diagnostics_config: DiagnosticsConfigInput,

    /// Vendor-diff analysis-scope lifecycle (`[analysis].diff_base`); the
    /// effective scope is mirrored into `diagnostics_config.scope`.
    pub analysis_scope: crate::analysis_scope::ScopeState,
    /// Monotonic guard: only the newest in-flight scope build may publish.
    pub(crate) scope_generation: u64,
    /// A scope rebuild was requested (save / external change / config reload)
    /// and waits for a free worker; coalesces bursts into one build.
    pub(crate) scope_build_queued: bool,
    /// Open documents edited since their last save: their buffer differs from
    /// the disk state the scope was diffed against, so they conservatively
    /// count as whole-file in scope until saved or closed.
    pub scope_dirty_docs: std::collections::HashSet<Url>,
    /// Resolved (base, HEAD) OIDs the current scope was built against; the
    /// idle tick rebuilds when the live refs no longer match (ref-only moves).
    pub(crate) scope_ref_identity: Option<(String, String)>,
    /// The last scope failure shown via `window/showMessage`, so a persistent
    /// failure warns once instead of on every save-triggered rebuild.
    pub(crate) scope_warning_shown: Option<String>,
    /// Whether the "ignored_authors is not applied in LSP" notice went out,
    /// so config reloads do not repeat it.
    pub(crate) author_warning_shown: bool,

    pub(crate) lsp_locale: Option<Locale>,
    pub position_encoding: PositionEncoding,
    /// Negotiated at `initialize`: whether the client honors
    /// `InsertTextMode::ADJUST_INDENTATION`, so completion snippet continuation
    /// lines can be indented to the cursor column by the client.
    pub supports_insert_text_mode_adjust_indentation: bool,
    /// Negotiated at `initialize`: whether the client renders
    /// `Diagnostic.codeDescription`, so the standard's link may be attached as a
    /// property instead of travelling only inside the message.
    pub supports_code_description: bool,
    /// Negotiated at `initialize`: which `Diagnostic.tags` the client accepts
    /// (`publishDiagnostics.tagSupport`). `ClientTags::NONE` means the property is
    /// not published at all.
    pub supports_diagnostic_tags: ClientTags,
    /// Negotiated at `initialize`: whether the client honors versioned
    /// `WorkspaceEdit.documentChanges`. When it does, rename edits carry the open
    /// document's version so the client rejects them if the buffer moved on after
    /// the snapshot; otherwise the server falls back to unversioned `changes`.
    pub supports_workspace_edit_document_changes: bool,
    /// Negotiated at `initialize`: the client's `rangeLimit` for folding ranges.
    /// When the response would exceed it, the handler drops the deepest folds
    /// first, so the outer skeleton survives (github#56).
    pub folding_range_limit: Option<u32>,
    /// True when the pull diagnostic provider is advertised (config opt-in) *and* the
    /// client advertised `textDocument/diagnostic` support — i.e. the client drives
    /// diagnostics by pulling. In that mode push publishing is suppressed so a
    /// pull-capable client does not render each open buffer's diagnostics twice (once
    /// from the pull report, once from `publishDiagnostics`). A client that opted the
    /// feature on but cannot pull keeps push, so it is never left without diagnostics.
    pub pull_diagnostics_active: bool,
    /// Whether the client advertised `workspace.diagnostics.refreshSupport`, so the server may
    /// ask it to re-pull workspace diagnostics once background state changes (e.g. after the
    /// initial workspace load finishes and results first become available).
    pub supports_workspace_diagnostic_refresh: bool,
    /// Per-URI publish generation. Each scheduled diagnostics computation gets the
    /// next generation for ITS uri; a completed task publishes only if it is still
    /// the latest for that uri. Keyed per-uri (not a single global counter) so
    /// scheduling several documents in one batch — e.g. refreshing all open docs
    /// after an external change — does not let one document's newer generation
    /// discard another document's result.
    pub diagnostics_generation: HashMap<Url, u64>,
    /// Documents whose diagnostics should be (re)scheduled by the event loop
    /// once it finishes the current message: didChange debouncing lands here,
    /// and so does a schedule that found the task pool saturated — the loop
    /// retries when a worker frees a queue slot. Deduplicated by URI.
    pub pending_diagnostics_uris: Vec<Url>,

    pub diagnostics_tokens: HashMap<Url, salsa::CancellationToken>,
    pub preload_tokens: HashMap<vfs::FileId, salsa::CancellationToken>,

    pub preload_external_tokens: HashMap<vfs::FileId, salsa::CancellationToken>,

    pub request_tokens: HashMap<lsp_server::RequestId, salsa::CancellationToken>,
    /// The in-flight `workspace/diagnostic` request, if any. A whole-workspace sweep can run
    /// for minutes; letting several pile onto the shared latency pool would starve interactive
    /// requests. Tracking the active one lets a newer sweep cancel the previous so at most one
    /// occupies a worker at a time.
    pub active_workspace_diagnostic: Option<lsp_server::RequestId>,

    /// Deferred whole-project diagnostics batch (Stream B), rust-analyzer's
    /// flycheck analogue: closed in-scope files are analysed off the critical path
    /// and their diagnostics pushed, so the Problems panel fills without the open
    /// document ever waiting on a whole-workspace sweep.
    ///
    /// `workspace_batch_dirty` requests a (re)run; the event loop spawns the batch
    /// once the pool has a free worker and none is in flight. `workspace_batch_in_flight`
    /// guards against launching a second concurrent batch. `batch_pushed` maps each
    /// URI currently showing non-empty batch diagnostics to the hash last pushed for
    /// it, so a re-run republishes only what changed and can clear a file that went
    /// clean (or was opened). `workspace_batch_token` cancels the in-flight batch on
    /// shutdown or when the feature is turned off.
    pub workspace_batch_dirty: bool,
    pub workspace_batch_in_flight: bool,
    pub batch_pushed: HashMap<Url, String>,
    pub workspace_batch_token: Option<salsa::CancellationToken>,
    /// Monotonic id of the current batch sweep. Bumped on every spawn and on a
    /// config-reload reset, and carried on the batch's chunks/done: the event loop
    /// drops any chunk whose generation is stale, so a chunk computed against an old
    /// snapshot (e.g. queued just before a reload turned the feature off) can never
    /// republish diagnostics under the new configuration.
    pub workspace_batch_generation: u64,
    /// URIs the in-flight batch has reported so far (this generation). On a fully
    /// completed sweep, any `batch_pushed` entry missing from this set no longer
    /// exists in scope — the file was deleted or the scope narrowed — so its stale
    /// diagnostics are cleared. Reset at the start of each sweep.
    pub batch_reported: FxHashSet<Url>,
    /// The in-progress sweep, if any. `Some` means a chunked batch is walking its
    /// frozen file set; the event loop resumes it (dispatch next chunk) until the
    /// cursor is exhausted, then clears it. A fresh sweep is only built when this is
    /// `None` and `workspace_batch_dirty` is set. Cleared on a config-reload reset.
    pub workspace_batch_plan: Option<WorkspaceBatchPlan>,

    pub last_progress_report: std::time::Instant,

    pub skipped_bsl: FxHashSet<paths::AbsPathBuf>,

    pub degraded_files_count: usize,

    /// File ids of currently-open editor documents, resolved at didOpen time.
    /// `process_changes` keys text storage on this (overlay for open files,
    /// disk-backed revision for closed) — by `FileId`, not by `Url`, so client
    /// URL encoding/casing can't misclassify an open buffer as closed and route
    /// unsaved edits to a stale disk read.
    pub open_files: FxHashSet<vfs::FileId>,

    /// High-water mark of process RSS observed while streaming the initial VFS
    /// load (before `vfs_done`). Sampled per loaded batch, just before that
    /// batch is drained into Salsa, so it captures the true streaming peak.
    pub boot_peak_rss_bytes: u64,

    /// High-water mark of source text buffered in `Vfs::changes` during the
    /// initial load, sampled per batch before it drains. With incremental
    /// draining this stays near one loader chunk; a regression that reverted to
    /// a single end-of-load flush would show it climb to the whole-corpus size.
    pub boot_peak_text_bytes: u64,

    /// Number of background analysis jobs (per-file dependency preload + type
    /// inference / diagnostics) currently in flight. Drives the debounced
    /// "Analyzing" work-done progress: it begins when this stays positive past
    /// the debounce and ends when it returns to zero.
    analysis_in_flight: u32,

    /// Whether the "Analyzing" work-done progress has an open Begin awaiting its
    /// End (i.e. the indicator is currently shown).
    analysis_progress_active: bool,

    /// Monotonic id of the current busy burst, bumped on each rising edge from
    /// zero in-flight. A debounce tick only promotes to a visible indicator if
    /// its captured epoch still matches, so a stale timer from an already-ended
    /// burst is ignored.
    analysis_epoch: u64,

    /// Consecutive event-loop idle ticks (the `select!` timeout fired with no
    /// message in between). Salsa only evicts beyond the LRU caps at a revision
    /// boundary, so a session that loads, navigates cross-module and then sits
    /// without edits retains every touched file's memos forever; the idle trim
    /// keyed off this counter is what closes that gap. Reset on every loop wake.
    pub(crate) idle_ticks: u32,
    /// Whether the shallow idle trim already ran for the current idle period, so
    /// a quiet server is not re-trimmed (each trim is a Salsa write that bumps
    /// the revision and forces re-validation on the next interaction). Reset on
    /// every loop wake.
    pub(crate) idle_shallow_trimmed: bool,
    /// Same latch for the deep (sweep-profile) idle trim.
    pub(crate) idle_deep_trimmed: bool,
}

impl GlobalState {
    pub fn new(sender: Sender<Message>) -> Self {
        let (loader_sender, loader_receiver) = crossbeam_channel::bounded(4);
        let loader = vfs_notify::NotifyHandle::spawn(loader_sender);
        Self::with_loader(sender, Box::new(loader), loader_receiver)
    }

    /// Construct with an injected VFS loader. This is the observation seam for
    /// workspace-(re)load tests: a recording [`loader::Handle`] sees exactly the
    /// configs [`Self::set_workspace_root`] emits (include roots, watch set),
    /// without arming a live filesystem watcher.
    pub fn with_loader(
        sender: Sender<Message>,
        loader: Box<dyn loader::Handle>,
        loader_receiver: Receiver<loader::Message>,
    ) -> Self {
        Self {
            sender,
            req_queue: ReqQueue::default(),
            analysis_host: AnalysisHost::default(),
            vfs: Arc::new(RwLock::new(Vfs::default())),
            mem_docs: MemDocs::default(),
            workspace_root: None,
            project: None,
            source_exclusions: Default::default(),
            diagnostics_baseline: Arc::new(
                ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::Disabled,
            ),
            diagnostics_baseline_notification_ledger: BTreeSet::new(),
            diagnostics_baseline_held: BTreeSet::new(),
            diagnostics_baseline_hold: 0,
            shutdown_requested: false,
            loader,
            loader_receiver,
            vfs_progress_config_version: 0,
            vfs_done: false,
            diagnostics_baseline_checked_at: None,
            task_pool: task_pool::TaskPool::new_with_handle(),
            call_hierarchy_index: DeferredCallHierarchyIndexState::default(),
            call_hierarchy_index_rebuilds: FxHashSet::default(),
            call_hierarchy_wait_policy: CallHierarchyWaitPolicy::default(),
            next_request_id: AtomicI32::new(1),
            diagnostics_config: DiagnosticsConfigInput::from_raw(
                Vec::<String>::new(),
                Vec::<String>::new(),
                Vec::<(String, String)>::new(),
                false,
                hir::dataflow::DEFAULT_MAX_ITERATIONS,
                Locale::default(),
                true,
            ),
            analysis_scope: crate::analysis_scope::ScopeState::default(),
            scope_generation: 0,
            scope_build_queued: false,
            scope_dirty_docs: std::collections::HashSet::new(),
            scope_ref_identity: None,
            scope_warning_shown: None,
            author_warning_shown: false,
            lsp_locale: None,
            position_encoding: PositionEncoding::default(),
            supports_insert_text_mode_adjust_indentation: false,
            supports_code_description: false,
            supports_diagnostic_tags: ClientTags::NONE,
            supports_workspace_edit_document_changes: false,
            folding_range_limit: None,
            pull_diagnostics_active: false,
            supports_workspace_diagnostic_refresh: false,
            diagnostics_generation: HashMap::new(),
            pending_diagnostics_uris: Vec::new(),
            active_workspace_diagnostic: None,
            workspace_batch_dirty: false,
            workspace_batch_in_flight: false,
            batch_pushed: HashMap::new(),
            workspace_batch_token: None,
            workspace_batch_generation: 0,
            batch_reported: FxHashSet::default(),
            workspace_batch_plan: None,
            diagnostics_tokens: HashMap::new(),
            preload_tokens: HashMap::new(),
            preload_external_tokens: HashMap::new(),
            request_tokens: HashMap::new(),
            last_progress_report: std::time::Instant::now(),
            skipped_bsl: FxHashSet::default(),
            degraded_files_count: 0,
            open_files: FxHashSet::default(),
            boot_peak_rss_bytes: 0,
            boot_peak_text_bytes: 0,
            analysis_in_flight: 0,
            analysis_progress_active: false,
            analysis_epoch: 0,
            idle_ticks: 0,
            idle_shallow_trimmed: false,
            idle_deep_trimmed: false,
        }
    }

    /// Note an event-loop wake (an LSP message, a loader event, or a finished
    /// task): the session is not idle, and whatever the wake computed may have
    /// created memos worth trimming later — so the idle-trim clock and its
    /// once-per-idle-period latches restart.
    pub(crate) fn note_loop_activity(&mut self) {
        self.idle_ticks = 0;
        self.idle_shallow_trimmed = false;
        self.idle_deep_trimmed = false;
    }

    /// Record that a background analysis job was just spawned. On the rising edge
    /// from idle it arms a one-shot debounce: a short-lived timer thread posts an
    /// [`Task::AnalysisProgressTick`] back to the event loop, so a burst that
    /// finishes within the debounce window never shows an indicator (no flicker),
    /// while a longer one promotes to a visible "Analyzing" progress.
    #[must_use = "the returned AnalysisGuard must be moved into the analysis job so \
                  it decrements the in-flight counter when the job ends"]
    pub fn note_analysis_spawned(&mut self) -> AnalysisGuard {
        self.analysis_in_flight += 1;
        if self.analysis_in_flight == 1 {
            self.analysis_epoch = self.analysis_epoch.wrapping_add(1);
            let epoch = self.analysis_epoch;
            let sender = self.task_pool.pool.sender.clone();
            if let Err(err) = std::thread::Builder::new()
                .name("bsl-analysis-debounce".to_owned())
                .spawn(move || {
                    std::thread::sleep(ANALYSIS_PROGRESS_DEBOUNCE);
                    let _ = sender.send(Task::AnalysisProgressTick { epoch });
                })
            {
                tracing::debug!(?err, "could not spawn analysis-progress debounce thread");
            }
        }
        AnalysisGuard { sender: self.task_pool.pool.sender.clone() }
    }

    /// True when no interactive db snapshot is in flight — the batch LRU trim cancels
    /// and blocks on every live snapshot, so it is deferred until this holds. Two pools
    /// hold snapshots: background analysis jobs (per-file diagnostics, preload), counted
    /// by `analysis_in_flight` and discounted by the batch's own plan guard; and latency
    /// request workers (hover, completion, the `workspace/diagnostic` pull, …), each of
    /// which owns a token in `request_tokens` for its lifetime. A request token can
    /// linger briefly after its worker's snapshot drops, so this is conservative — it may
    /// defer a trim that is already safe, never trim while a snapshot is live.
    pub fn interactive_analysis_quiescent(&self) -> bool {
        let batch_guard = u32::from(self.workspace_batch_plan.is_some());
        self.analysis_in_flight <= batch_guard && self.request_tokens.is_empty()
    }

    /// Record that a background analysis job finished. When the last one drains,
    /// end the "Analyzing" indicator if it was shown.
    pub fn note_analysis_finished(&mut self) {
        self.analysis_in_flight = self.analysis_in_flight.saturating_sub(1);
        if self.analysis_in_flight == 0 && self.analysis_progress_active {
            self.analysis_progress_active = false;
            self.report_progress("Analyzing", Progress::End, None, None);
        }
    }

    /// Handle a debounce tick: show the "Analyzing" indicator only if this is
    /// still the current busy burst, work is still in flight, and nothing is
    /// shown yet.
    pub fn handle_analysis_progress_tick(&mut self, epoch: u64) {
        if epoch == self.analysis_epoch
            && self.analysis_in_flight > 0
            && !self.analysis_progress_active
        {
            self.analysis_progress_active = true;
            self.report_progress("Analyzing", Progress::Begin, Some("Analyzing…".to_owned()), None);
        }
    }

    /// Queue a document for diagnostics (re)scheduling at the bottom of the
    /// event loop, deduplicated by URI.
    pub fn enqueue_pending_diagnostics(&mut self, uri: Url) {
        if !self.pending_diagnostics_uris.contains(&uri) {
            self.pending_diagnostics_uris.push(uri);
        }
    }

    /// Request a (re)run of the deferred whole-project diagnostics batch. A no-op
    /// unless the feature is enabled; the event loop launches it once a worker is
    /// free. Only a workspace-diagnostics scope other than `Off` arms it, so the
    /// default push-only configuration never runs a batch.
    pub fn mark_workspace_batch_dirty(&mut self) {
        let enabled = self
            .project
            .as_ref()
            .is_some_and(|p| p.config.features.workspace_diagnostics.is_enabled());
        if enabled {
            self.workspace_batch_dirty = true;
        }
    }

    /// Hand a file off from the batch (Stream B) to the interactive stream (A) when
    /// it is opened: if the batch had published diagnostics for it, clear them so the
    /// open document is the sole owner (pull, or push for a non-pull client) and the
    /// two streams never double-report the same file.
    /// `window/showMessage` with error severity — for failures the user must
    /// see without digging through server logs (e.g. a broken project config).
    pub fn show_error_message(&self, message: String) {
        let params = lsp_types::ShowMessageParams { typ: lsp_types::MessageType::ERROR, message };
        let notification = lsp_server::Notification::new("window/showMessage".to_string(), params);
        let _ = self.sender.send(notification.into());
    }

    /// `window/showMessage` with warning severity — for a workspace that loads
    /// but will produce misleading results, which the user cannot infer from
    /// the diagnostics themselves.
    pub fn show_warning_message(&self, message: String) {
        let params = lsp_types::ShowMessageParams { typ: lsp_types::MessageType::WARNING, message };
        let notification = lsp_server::Notification::new("window/showMessage".to_string(), params);
        let _ = self.sender.send(notification.into());
    }

    pub fn clear_batch_push_for(&mut self, uri: &Url) {
        if self.batch_pushed.remove(uri).is_some() {
            let params =
                PublishDiagnosticsParams { uri: uri.clone(), diagnostics: vec![], version: None };
            let notification = lsp_server::Notification::new(
                "textDocument/publishDiagnostics".to_string(),
                params,
            );
            let _ = self.sender.send(notification.into());
        }
    }

    /// Tear down the whole-project batch's published state: cancel any in-flight
    /// sweep and clear every file it pushed (publishing empty reports). Used when the
    /// configuration is reloaded — the scope may have changed or been turned off, so
    /// stale batch diagnostics must not linger. The caller re-arms the batch afterward
    /// if the feature is still enabled.
    pub fn reset_workspace_batch(&mut self) {
        if let Some(token) = self.workspace_batch_token.take() {
            token.cancel();
        }
        // Drop the in-progress plan so a re-arm rebuilds the file set against the new
        // configuration/scope rather than resuming the stale one.
        self.workspace_batch_plan = None;
        // Invalidate any chunk still in flight from the pre-reset sweep: bumping the
        // generation makes the event loop drop those chunks instead of republishing
        // diagnostics that the new configuration may no longer include.
        self.workspace_batch_generation = self.workspace_batch_generation.wrapping_add(1);
        self.clear_all_batch_pushed();
    }

    /// Clear every file the batch has published (emit an empty report for each) and reset
    /// the reported-set. Used on a config reset and when a fresh sweep finds nothing in
    /// scope — a prior sweep's diagnostics for now-deleted / out-of-scope files must not
    /// linger.
    pub fn clear_all_batch_pushed(&mut self) {
        self.batch_reported.clear();
        for (uri, _) in std::mem::take(&mut self.batch_pushed) {
            let params = PublishDiagnosticsParams { uri, diagnostics: vec![], version: None };
            let notification = lsp_server::Notification::new(
                "textDocument/publishDiagnostics".to_string(),
                params,
            );
            let _ = self.sender.send(notification.into());
        }
    }

    pub fn assert_total_vfs_invariant(&self) -> usize {
        use base_db::SourceDatabase;

        let db = self.analysis_host.raw_database();
        let source_root_id = base_db::SourceRootId(0);
        let source_root_input = db.source_root_input(source_root_id);
        let source_root = source_root_input.root(db);
        let file_set = source_root.file_set();

        let mut violations = 0;
        for fid in file_set.iter() {
            if !ide_db::is_bsl_source(file_set, fid) {
                continue;
            }
            // "Loaded" now means a content revision is registered (disk-backed or
            // overlay), not that an overlay text is resident — closed files are
            // disk-backed and legitimately have no `FileTextInput`.
            if db.try_file_revision_input(fid).is_none() {
                tracing::warn!(file_id = fid.0, "BSL fid in SourceRoot has no content revision");
                violations += 1;
            }
        }
        violations
    }

    fn next_request_id(&self) -> lsp_server::RequestId {
        lsp_server::RequestId::from(self.next_request_id.fetch_add(1, Ordering::SeqCst))
    }

    pub fn report_progress(
        &self,
        title: &str,
        state: Progress,
        message: Option<String>,
        fraction: Option<f64>,
    ) {
        let percentage = fraction.map(|f| (f * 100.0) as u32);
        let token = lsp_types::ProgressToken::String(format!("bslAnalyzer/{title}"));

        let work_done_progress = match state {
            Progress::Begin => {
                let params = lsp_types::WorkDoneProgressCreateParams { token: token.clone() };
                let request = lsp_server::Request::new(
                    self.next_request_id(),
                    "window/workDoneProgress/create".to_string(),
                    params,
                );
                self.sender.send(request.into()).ok();

                lsp_types::WorkDoneProgress::Begin(lsp_types::WorkDoneProgressBegin {
                    title: title.into(),
                    cancellable: Some(false),
                    message,
                    percentage,
                })
            }
            Progress::Report => {
                lsp_types::WorkDoneProgress::Report(lsp_types::WorkDoneProgressReport {
                    cancellable: Some(false),
                    message,
                    percentage,
                })
            }
            Progress::End => {
                lsp_types::WorkDoneProgress::End(lsp_types::WorkDoneProgressEnd { message })
            }
        };

        let notification = lsp_server::Notification::new(
            "$/progress".to_string(),
            lsp_types::ProgressParams {
                token,
                value: lsp_types::ProgressParamsValue::WorkDone(work_done_progress),
            },
        );
        self.sender.send(notification.into()).ok();
    }

    pub fn respond(&mut self, response: Response) {
        if let Err(e) = self.sender.send(response.into()) {
            tracing::error!("Failed to send response: {}", e);
        }
    }

    pub fn request_semantic_tokens_refresh(&mut self) {
        use lsp_server::Request;

        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let request = Request::new(
            lsp_server::RequestId::from(id),
            "workspace/semanticTokens/refresh".to_string(),
            (),
        );

        if let Err(e) = self.sender.send(request.into()) {
            tracing::error!("Failed to send semantic tokens refresh request: {}", e);
        } else {
            tracing::info!("Requested client to refresh semantic tokens");
        }
    }

    /// Ask the client to re-pull all workspace diagnostics. Sent once the initial workspace load
    /// finishes so a pull-capable client refreshes the Problems panel with now-available results
    /// without the user re-triggering. No-op unless pull is active and the client supports it.
    pub fn request_workspace_diagnostic_refresh(&self) {
        if !self.pull_diagnostics_active || !self.supports_workspace_diagnostic_refresh {
            return;
        }
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let request = lsp_server::Request::new(
            lsp_server::RequestId::from(id),
            "workspace/diagnostic/refresh".to_string(),
            (),
        );
        if let Err(e) = self.sender.send(request.into()) {
            tracing::error!("Failed to send workspace diagnostic refresh request: {}", e);
        } else {
            tracing::info!("Requested client to refresh workspace diagnostics");
        }
    }

    pub fn snapshot(&self) -> GlobalStateSnapshot {
        GlobalStateSnapshot {
            analysis: self.analysis_host.analysis(),
            vfs: Arc::clone(&self.vfs),
            mem_docs: self.mem_docs.clone(),
            workspace_root: self.workspace_root.clone(),
            project: self.project.clone(),
            diagnostics_baseline: Arc::clone(&self.diagnostics_baseline),
            diagnostics_config: self.diagnostics_config.clone(),
            position_encoding: self.position_encoding,
            vfs_done: self.vfs_done,
            task_sender: self.task_pool.pool.sender.clone(),
            call_hierarchy_index: self.call_hierarchy_index.ensure(),
            scope_dirty_docs: self.scope_dirty_docs.clone(),
        }
    }
}

pub struct GlobalStateSnapshot {
    pub analysis: Analysis,
    pub vfs: Arc<RwLock<Vfs>>,
    pub mem_docs: MemDocs,
    pub workspace_root: Option<PathBuf>,
    pub project: Option<Project>,
    pub diagnostics_baseline: Arc<ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot>,
    pub diagnostics_config: DiagnosticsConfigInput,
    pub position_encoding: PositionEncoding,
    pub vfs_done: bool,
    pub task_sender: Sender<Task>,
    pub call_hierarchy_index: CallHierarchyIndexState,
    /// Open documents edited since their last save; they count as whole-file
    /// in scope (the disk-derived diff no longer describes their buffer).
    pub scope_dirty_docs: std::collections::HashSet<Url>,
}

impl GlobalStateSnapshot {
    pub fn file_id_for_url(&self, url: &Url) -> anyhow::Result<vfs::FileId> {
        let path = url.to_file_path().map_err(|_| anyhow::anyhow!("Invalid file URL: {}", url))?;
        if !project_model::is_bsl_source_path(&path) {
            return Err(anyhow::anyhow!("File is not BSL, request unsupported: {}", url));
        }
        // An excluded document keeps the FileId it had before the exclusion, tombstoned;
        // it must not be answered for as if it were still analysed.
        if self.project.as_ref().is_some_and(|project| project.is_source_excluded(&path)) {
            return Err(anyhow::anyhow!("File is excluded by [source].exclude: {}", url));
        }

        let vfs_path = vfs::VfsPath::new(path);

        let vfs = self.vfs.read();
        vfs.file_id(&vfs_path).ok_or_else(|| anyhow::anyhow!("File not in VFS: {}", url))
    }
}

#[cfg(test)]
mod vfs_race_tests {
    use super::*;

    #[test]
    fn test_empty_source_root_init() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        use base_db::{SourceDatabase, SourceRootId};
        let db = state.analysis_host.raw_database_mut();
        let sr = db.source_root_input(SourceRootId(0));
        assert!(!sr.root(db).is_library());
        assert_eq!(sr.root(db).file_set().len(), 0);
    }

    #[test]
    fn call_hierarchy_index_lifecycle_is_deferred_until_prepare() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let state = GlobalState::new(sender);

        let source_root = base_db::SourceRootId(0);

        assert!(state.call_hierarchy_index.active().is_none());

        let index = state.call_hierarchy_index.ensure();

        assert_eq!(index.generation(source_root), None);
        assert_eq!(index.next_generation(source_root), Some(1));
        assert!(state.call_hierarchy_index.active().is_some());
    }

    #[test]
    fn set_workspace_root_closes_load_gate_only_for_initial_load() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        assert!(state.analysis_host.raw_database().workspace_load_complete());

        let tmp = tempfile::tempdir().expect("tempdir");

        // Initial load (vfs_done = false): the gate closes until the finalize.
        state.set_workspace_root(tmp.path().to_path_buf()).expect("valid test workspace");
        assert!(
            !state.analysis_host.raw_database().workspace_load_complete(),
            "the initial load must close the whole-config loader gate"
        );

        // A live reload (vfs_done = true) must not degrade running analysis.
        state.vfs_done = true;
        state.analysis_host.raw_database_mut().set_workspace_load_complete(true);
        state.set_workspace_root(tmp.path().to_path_buf()).expect("valid test workspace");
        assert!(
            state.analysis_host.raw_database().workspace_load_complete(),
            "a live workspace reload must keep the gate open"
        );
    }

    #[test]
    fn reload_evicts_closed_bsl_and_watch_only_xml_newly_excluded_from_the_vfs() {
        use base_db::SourceDatabase;
        use salsa::Database as _;

        #[derive(Debug)]
        struct SilentLoader;
        impl loader::Handle for SilentLoader {
            fn spawn(_sender: loader::Sender) -> Self
            where
                Self: Sized,
            {
                unreachable!("injected loader")
            }
            fn set_config(&mut self, _config: loader::Config) {}
            fn invalidate(&mut self, _path: paths::AbsPathBuf) {}
            fn load_sync(&mut self, _path: &paths::AbsPath) -> Option<Vec<u8>> {
                None
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \".\"\n").unwrap();
        std::fs::create_dir_all(root.join("generated")).unwrap();
        let hidden_bsl = root.join("generated/Hidden.bsl");
        let open_hidden_bsl = root.join("generated/OpenHidden.bsl");
        let hidden_xml = root.join("generated/Hidden.xml");
        let allowed_bsl = root.join("Allowed.bsl");
        for path in [&hidden_bsl, &open_hidden_bsl, &hidden_xml, &allowed_bsl] {
            std::fs::write(path, "x").unwrap();
        }

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let (_loader_tx, loader_receiver) = crossbeam_channel::bounded(4);
        let mut state = GlobalState::with_loader(sender, Box::new(SilentLoader), loader_receiver);
        state.init_empty_source_root();
        state.set_workspace_root(root.to_path_buf()).unwrap();
        let (hidden_bsl_id, open_hidden_id, hidden_xml_id, allowed_id) = {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(vfs::VfsPath::new(hidden_bsl.clone()), Some(Arc::from("x")));
            let hidden_bsl_id = vfs.file_id(&vfs::VfsPath::new(hidden_bsl.clone())).unwrap();
            vfs.set_file_contents(
                vfs::VfsPath::new(open_hidden_bsl.clone()),
                Some(Arc::from("disk")),
            );
            let open_hidden_id = vfs.file_id(&vfs::VfsPath::new(open_hidden_bsl.clone())).unwrap();
            let hidden_xml_id = vfs.register_watch_only(vfs::VfsPath::new(hidden_xml.clone()));
            vfs.set_file_contents(vfs::VfsPath::new(allowed_bsl.clone()), Some(Arc::from("x")));
            let allowed_id = vfs.file_id(&vfs::VfsPath::new(allowed_bsl.clone())).unwrap();
            (hidden_bsl_id, open_hidden_id, hidden_xml_id, allowed_id)
        };
        state.process_changes(false);
        state.init_source_root();
        let open_uri = Url::from_file_path(&open_hidden_bsl).unwrap();
        state.mem_docs.insert(open_uri.clone(), "unsaved buffer".to_owned(), 1);
        state.open_files.insert(open_hidden_id);
        state.vfs.write().set_file_contents(
            vfs::VfsPath::new(open_hidden_bsl.clone()),
            Some(Arc::from("unsaved buffer")),
        );
        state.process_changes(false);
        assert!(state.vfs.read().exists(hidden_bsl_id));
        assert!(state.vfs.read().exists(hidden_xml_id));
        let diagnostics_db = state.analysis_host.raw_database().clone();
        let diagnostics_token = diagnostics_db.cancellation_token();
        drop(diagnostics_db);
        state.diagnostics_tokens.insert(open_uri.clone(), diagnostics_token.clone());
        let preload_db = state.analysis_host.raw_database().clone();
        let preload_token = preload_db.cancellation_token();
        drop(preload_db);
        state.preload_external_tokens.insert(hidden_bsl_id, preload_token.clone());

        std::fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\nexclude = [\"generated\"]\n",
        )
        .unwrap();
        assert!(state.reload_project_config());

        let vfs = state.vfs.read();
        assert!(!vfs.exists(hidden_bsl_id), "excluded BSL remained live");
        assert!(!vfs.exists(open_hidden_id), "excluded open BSL remained live");
        assert!(!vfs.exists(hidden_xml_id), "excluded watch-only XML remained registered");
        assert!(vfs.exists(allowed_id), "allowed sibling was evicted");
        drop(vfs);
        let db = state.analysis_host.raw_database();
        let root0 = db.source_root_input(base_db::BSL_SOURCE_ROOT).root(db);
        assert!(
            root0.file_set().file_for_path(&vfs::VfsPath::new(hidden_bsl)).is_none(),
            "excluded BSL remained in FileSet"
        );
        assert!(
            root0.file_set().file_for_path(&vfs::VfsPath::new(allowed_bsl)).is_some(),
            "allowed BSL left FileSet"
        );
        assert!(state.mem_docs.contains(&open_uri), "reload discarded the editor buffer");
        assert!(!state.open_files.contains(&open_hidden_id));
        assert!(!state.diagnostics_tokens.contains_key(&open_uri));
        assert!(diagnostics_token.is_cancelled(), "excluded diagnostics kept running");
        assert!(!state.preload_external_tokens.contains_key(&hidden_bsl_id));
        assert!(preload_token.is_cancelled(), "excluded preload kept running");
        assert!(
            state.vfs_file_for_url(&open_uri).is_err(),
            "the event-loop lookup readmitted an excluded editor URI"
        );
        assert!(
            state.snapshot().file_id_for_url(&open_uri).is_err(),
            "an async request snapshot admitted an excluded editor URI"
        );

        std::fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \".\"\n").unwrap();
        assert!(state.reload_project_config());
        let readmitted_id = state.vfs_file_for_url(&open_uri).unwrap();
        assert_eq!(readmitted_id, open_hidden_id, "the tombstoned file identity changed");
        assert!(state.vfs.read().exists(readmitted_id));
        assert!(state.open_files.contains(&readmitted_id));
        assert_eq!(
            &*state.analysis_host.raw_database_mut().file_text(readmitted_id),
            "unsaved buffer",
            "removing the exclusion did not restore the editor overlay"
        );

        let diagnostics_db = state.analysis_host.raw_database().clone();
        let empty_diagnostics_token = diagnostics_db.cancellation_token();
        drop(diagnostics_db);
        state.diagnostics_tokens.insert(open_uri.clone(), empty_diagnostics_token.clone());
        let preload_db = state.analysis_host.raw_database().clone();
        let empty_preload_token = preload_db.cancellation_token();
        drop(preload_db);
        state.preload_external_tokens.insert(allowed_id, empty_preload_token.clone());

        std::fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\nexclude = [\".\"]\n",
        )
        .unwrap();
        assert!(state.reload_project_config());
        assert!(state.project.as_ref().unwrap().source_roots().is_empty());
        let vfs = state.vfs.read();
        for file_id in [hidden_bsl_id, open_hidden_id, hidden_xml_id, allowed_id] {
            assert!(!vfs.exists(file_id), "a stale VFS entry survived the empty universe");
        }
        drop(vfs);
        {
            let db = state.analysis_host.raw_database();
            let root0 = db.source_root_input(base_db::BSL_SOURCE_ROOT).root(db);
            assert!(root0.file_set().iter().next().is_none(), "the empty universe kept a FileSet");
        }
        assert!(state.mem_docs.contains(&open_uri), "the empty universe discarded the buffer");
        assert!(!state.open_files.contains(&open_hidden_id));
        assert!(empty_diagnostics_token.is_cancelled());
        assert!(empty_preload_token.is_cancelled());

        std::fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \".\"\n").unwrap();
        assert!(
            state.project.as_ref().unwrap().source_roots().is_empty(),
            "an unwatched config reloaded without didSave"
        );
        crate::handlers::handle_did_save(
            &mut state,
            lsp_types::DidSaveTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: lsp_types::Url::from_file_path(root.join("bsl-analyzer.toml")).unwrap(),
                },
                text: None,
            },
        )
        .expect("didSave must restore a fully excluded workspace");
        let readmitted_id = state.vfs_file_for_url(&open_uri).unwrap();
        assert_eq!(readmitted_id, open_hidden_id);
        assert!(state.open_files.contains(&readmitted_id));
        assert_eq!(
            &*state.analysis_host.raw_database_mut().file_text(readmitted_id),
            "unsaved buffer",
            "didSave did not restore the preserved editor overlay"
        );
    }

    /// A workspace whose root is itself a configuration extension must be called
    /// out to the client. Nothing else in the protocol explains why the findings
    /// that follow report valid calls into the main configuration as unresolved,
    /// and the LSP has no source-set flags to make the shape obvious either.
    #[test]
    fn a_workspace_root_that_is_an_extension_warns_the_client() {
        #[derive(Debug)]
        struct SilentLoader;
        impl loader::Handle for SilentLoader {
            fn spawn(_sender: loader::Sender) -> Self
            where
                Self: Sized,
            {
                unreachable!("injected via with_loader, never spawned")
            }
            fn set_config(&mut self, _config: loader::Config) {}
            fn invalidate(&mut self, _path: paths::AbsPathBuf) {}
            fn load_sync(&mut self, _path: &paths::AbsPath) -> Option<Vec<u8>> {
                None
            }
        }

        fn configuration(root: &std::path::Path, extension: bool) {
            let purpose = if extension {
                "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
            } else {
                ""
            };
            std::fs::write(
                root.join("Configuration.xml"),
                format!(
                    "<MetaDataObject><Configuration><Properties>{purpose}</Properties>\
                     </Configuration></MetaDataObject>"
                ),
            )
            .unwrap();
        }

        fn warnings(root: &std::path::Path) -> Vec<String> {
            let (sender, receiver) = crossbeam_channel::unbounded();
            let (_loader_tx, loader_receiver) = crossbeam_channel::bounded(4);
            let mut state =
                GlobalState::with_loader(sender, Box::new(SilentLoader), loader_receiver);

            state.set_workspace_root(root.to_path_buf()).expect("the project must load");

            std::iter::from_fn(|| receiver.try_recv().ok())
                .filter_map(|message| match message {
                    Message::Notification(n) if n.method == "window/showMessage" => {
                        let params: lsp_types::ShowMessageParams =
                            serde_json::from_value(n.params).ok()?;
                        (params.typ == lsp_types::MessageType::WARNING).then_some(params.message)
                    }
                    _ => None,
                })
                .collect()
        }

        let ext = tempfile::tempdir().expect("tempdir");
        configuration(ext.path(), true);
        let main = tempfile::tempdir().expect("tempdir");
        configuration(main.path(), false);

        let warned = warnings(ext.path());
        assert_eq!(warned.len(), 1, "exactly one warning: {warned:?}");
        assert!(
            warned[0].contains("configuration extension analyzed without"),
            "the warning must name the shape: {warned:?}"
        );

        // A main configuration carries the neighbouring compatibility-mode element
        // and must not be mistaken for an extension.
        assert!(warnings(main.path()).is_empty(), "a main configuration stays silent");
    }

    /// A live workspace reload that adds an extension root must emit a loader
    /// config whose include set covers the new root (and still watches the config
    /// files) — otherwise the VFS never scans the extension and no analysis sees
    /// it. Observed through an injected recording loader, not a live watcher.
    #[test]
    fn live_reload_emits_a_loader_config_covering_a_new_extension_root() {
        #[derive(Debug)]
        struct RecordingLoader(std::sync::mpsc::Sender<loader::Config>);
        impl loader::Handle for RecordingLoader {
            fn spawn(_sender: loader::Sender) -> Self
            where
                Self: Sized,
            {
                unreachable!("injected via with_loader, never spawned")
            }
            fn set_config(&mut self, config: loader::Config) {
                let _ = self.0.send(config);
            }
            fn invalidate(&mut self, _path: paths::AbsPathBuf) {}
            fn load_sync(&mut self, _path: &paths::AbsPath) -> Option<Vec<u8>> {
                None
            }
        }
        let includes = |config: &loader::Config| -> Vec<String> {
            config
                .load
                .iter()
                .flat_map(|entry| match entry {
                    loader::Entry::Directories(dirs) => {
                        dirs.include.iter().map(|p| p.to_string()).collect::<Vec<_>>()
                    }
                    loader::Entry::Files(_) | loader::Entry::WatchOnlyFiles(_) => Vec::new(),
                })
                .collect()
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // The extension lives OUTSIDE the workspace tree, so its presence in the
        // loader config can only come from the declared topology — never from the
        // source-root scan covering it incidentally.
        let ext_tmp = tempfile::tempdir().expect("tempdir");
        let ext = ext_tmp.path();
        std::fs::write(ext.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::create_dir_all(root.join("generated")).unwrap();
        let descriptorless_hidden = root.join("generated/Hidden.bsl");
        std::fs::write(&descriptorless_hidden, "").unwrap();
        std::fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\nexclude = [\"generated\"]\n",
        )
        .unwrap();

        let (configs_tx, configs_rx) = std::sync::mpsc::channel();
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let (_loader_tx, loader_receiver) = crossbeam_channel::bounded(4);
        let mut state = GlobalState::with_loader(
            sender,
            Box::new(RecordingLoader(configs_tx)),
            loader_receiver,
        );
        state.init_empty_source_root();

        state.set_workspace_root(root.to_path_buf()).expect("initial load");
        let first = configs_rx.try_recv().expect("the initial load emits a loader config");
        let env_path = root.join(".env");
        assert!(
            first.load.iter().any(|entry| match entry {
                loader::Entry::Files(files) =>
                    files.iter().any(|path| path.as_path() == env_path.as_path()),
                _ => false,
            }),
            "project-local .env must stay watched even before it exists"
        );
        let source_entry = first
            .load
            .iter()
            .find_map(|entry| match entry {
                loader::Entry::Directories(dirs) => Some(dirs),
                _ => None,
            })
            .expect("descriptorless fallback still emits a directory scan");
        assert!(source_entry.extensions.iter().any(|ext| ext == "bsl"));
        assert!(source_entry.rules.iter().any(|rule| {
            rule.load_mode == loader::LoadMode::WatchOnly
                && rule.extensions.iter().any(|ext| ext == "xml")
        }));
        for fixed in [".git", "build", ".vscode"] {
            assert!(
                source_entry
                    .exclude
                    .iter()
                    .any(|path| AsRef::<std::path::Path>::as_ref(path) == root.join(fixed)),
                "adding [source].exclude dropped the legacy LSP exclusion {fixed}"
            );
        }
        assert!(
            source_entry.hard_exclude.is_excluded(&root.join("generated/Hidden.bsl")),
            "the descriptorless LSP loader dropped [source].exclude"
        );
        assert!(
            !source_entry.hard_exclude.is_excluded(&root.join("generated2/Allowed.bsl")),
            "the LSP loader widened a component-scoped exclusion"
        );
        assert!(
            state.is_source_excluded_resolved(&descriptorless_hidden),
            "the descriptorless loader received the scope but live LSP state did not"
        );
        let hidden_uri = Url::from_file_path(&descriptorless_hidden).unwrap();
        crate::handlers::handle_did_open(
            &mut state,
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: hidden_uri.clone(),
                    language_id: "bsl".to_owned(),
                    version: 1,
                    text: "Процедура Несохраненная() КонецПроцедуры".to_owned(),
                },
            },
        )
        .unwrap();
        assert!(
            state.vfs.read().file_id(&vfs::VfsPath::new(descriptorless_hidden.clone())).is_none(),
            "descriptorless didOpen bypassed the project exclusion"
        );
        assert!(!state.diagnostics_tokens.contains_key(&hidden_uri));

        let ext_str = ext.to_string_lossy().into_owned();
        assert!(
            !includes(&first).iter().any(|p| p.contains(&ext_str)),
            "the undeclared extension is not scanned initially"
        );

        std::fs::write(
            root.join("bsl-analyzer.toml"),
            format!(
                "[source]\nroot = \".\"\nexclude = [\"generated\"]\nextensions = [{{ name = \"a\", path = {ext:?} }}]\n"
            ),
        )
        .unwrap();
        state.vfs_done = true;
        state.set_workspace_root(root.to_path_buf()).expect("live reload");
        let second = configs_rx.try_recv().expect("the reload emits a fresh loader config");
        assert!(
            includes(&second).iter().any(|p| p.contains(&ext_str)),
            "the reload's loader config must include the newly-declared extension root: {:?}",
            includes(&second),
        );
        assert!(
            second.load.iter().any(|e| matches!(e, loader::Entry::Files(_))),
            "the analyzer config files stay watched after the reload"
        );
        assert!(second.version > first.version, "each loader config carries a fresh version");

        std::fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\nexclude = [\".\"]\n",
        )
        .unwrap();
        state.set_workspace_root(root.to_path_buf()).expect("exclude the whole project");
        let third = configs_rx.try_recv().expect("the whole-root exclusion emits a config");
        assert!(
            third.load.iter().all(|entry| !matches!(entry, loader::Entry::Files(_))),
            "a config file below an excluded project root remained watched"
        );
        assert!(includes(&third).is_empty(), "an excluded source root remained in the loader");
        assert!(
            third.load.iter().all(|entry| matches!(
                entry,
                loader::Entry::Directories(dirs) if dirs.include.is_empty()
            )),
            "explicit Files or WatchOnlyFiles bypassed a whole-project exclusion: {:?}",
            third.load
        );
        assert!(state.project.as_ref().unwrap().source_roots().is_empty());

        std::fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \".\"\n").unwrap();
        assert!(
            state.project.as_ref().unwrap().source_roots().is_empty(),
            "a disk edit reloaded an unwatched excluded config without didSave"
        );
        crate::handlers::handle_did_save(
            &mut state,
            lsp_types::DidSaveTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: lsp_types::Url::from_file_path(root.join("bsl-analyzer.toml")).unwrap(),
                },
                text: None,
            },
        )
        .expect("didSave of an unwatched excluded config must reload it");
        let fourth = configs_rx.try_recv().expect("didSave emits a restored loader config");
        assert!(!includes(&fourth).is_empty(), "removing exclude did not restore source scanning");
        assert!(
            fourth.load.iter().any(|entry| matches!(entry, loader::Entry::Files(_))),
            "removing exclude did not restore config-file watching"
        );
    }

    /// Every file the project is derived from must re-derive it when it changes.
    /// `.env` carries the shared-configuration root, so it is such a file; watching
    /// it without reacting leaves the analyzer on the old base until a restart.
    /// The analyzer's own config file is the control: both inputs, one outcome.
    #[test]
    fn a_disk_edit_of_any_project_input_reloads_the_project() {
        fn edited(state: &mut GlobalState, path: &std::path::Path, text: &str) -> bool {
            {
                let mut vfs = state.vfs.write();
                vfs.set_file_contents(vfs::VfsPath::new(path.to_path_buf()), Some(Arc::from(text)));
            }
            state.process_changes(false).config_file_changed
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \".\"\n").unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.set_workspace_root(root.to_path_buf()).expect("initial load");

        assert!(
            edited(
                &mut state,
                &root.join(project_model::CONFIG_FILE_NAMES[0]),
                "[source]\nroot = \".\"\n",
            ),
            "control: an analyzer-config edit re-derives the project"
        );
        assert!(
            edited(
                &mut state,
                &root.join(project_model::PROJECT_ENV_FILE_NAME),
                "ONEC_CONFIGURATIONS_ROOT=/opt/configurations\n",
            ),
            "a .env edit is a project input too, so it must re-derive the project"
        );
    }

    /// Saving a project input from the editor reloads the project. Hand-editing
    /// `.env` is the intended way to move the shared configuration root, so a save
    /// of it must behave like a save of the analyzer's config file — the control.
    #[test]
    fn saving_any_project_input_reloads_the_project() {
        fn shared(root: &std::path::Path, version: &str) {
            let dir = root.join("UT11").join(version);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Configuration.xml"), "<Configuration/>").unwrap();
        }
        fn saved(state: &mut GlobalState, path: &std::path::Path) {
            crate::handlers::handle_did_save(
                state,
                lsp_types::DidSaveTextDocumentParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: lsp_types::Url::from_file_path(path).unwrap(),
                    },
                    text: None,
                },
            )
            .unwrap();
        }
        let base = |state: &GlobalState| -> std::path::PathBuf {
            state
                .project
                .as_ref()
                .and_then(|project| project.configuration_path())
                .expect("the shared configuration resolves")
                .to_path_buf()
        };

        // The process environment wins over `.env`, so a machine that exports the
        // variable would resolve a different base and fail this test for a reason
        // that has nothing to do with the reload being measured.
        assert!(
            std::env::var_os(project_model::SHARED_CONFIGURATIONS_ROOT_ENV).is_none(),
            "unset {} to run this test",
            project_model::SHARED_CONFIGURATIONS_ROOT_ENV
        );
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("configurations");
        shared(&store, "1.0.0");
        shared(&store, "2.0.0");
        let other = tmp.path().join("other-configurations");
        shared(&other, "1.0.0");
        shared(&other, "2.0.0");

        let root = tmp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let toml = |version: &str| {
            format!("[source.configuration]\nid = \"UT11\"\nversion = \"{version}\"\n")
        };
        std::fs::write(root.join("bsl-analyzer.toml"), toml("1.0.0")).unwrap();
        let env_path = root.join(project_model::PROJECT_ENV_FILE_NAME);
        std::fs::write(&env_path, format!("ONEC_CONFIGURATIONS_ROOT={}\n", store.display()))
            .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.set_workspace_root(root.clone()).expect("initial load");
        assert_eq!(base(&state), store.join("UT11").join("1.0.0"));

        // Control: saving the analyzer's own config file reloads the project.
        std::fs::write(root.join("bsl-analyzer.toml"), toml("2.0.0")).unwrap();
        saved(&mut state, &root.join("bsl-analyzer.toml"));
        assert_eq!(
            base(&state),
            store.join("UT11").join("2.0.0"),
            "control: a saved analyzer config moves the resolved base"
        );

        // The subject: the same must hold for the file that carries the store root.
        std::fs::write(&env_path, format!("ONEC_CONFIGURATIONS_ROOT={}\n", other.display()))
            .unwrap();
        saved(&mut state, &env_path);
        assert_eq!(
            base(&state),
            other.join("UT11").join("2.0.0"),
            "a saved .env must move the resolved base too"
        );
    }

    #[test]
    fn test_lsp_before_loader() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);

        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("user.bsl");
        let file_id = state.vfs_file_for_url(&uri).unwrap();
        // Open document: text lives in the resident overlay, not on disk (this
        // synthetic file has no disk path for the disk-backed path to read).
        state.open_files.insert(file_id);

        {
            let vfs_path = vfs::VfsPath::new(uri.to_file_path().unwrap());
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(vfs_path, Some(Arc::from("Процедура Test() КонецПроцедуры")));
        }

        state.process_changes(false);

        let text1 = {
            use base_db::SourceDatabase;
            let db = state.analysis_host.raw_database_mut();
            db.file_text(file_id)
        };
        assert!(text1.contains("Test"));

        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                vfs::VfsPath::new("/loader.bsl"),
                Some(Arc::from("// Loader file")),
            );
        }

        state.process_changes(false);
        state.init_source_root();

        let text2 = {
            use base_db::SourceDatabase;
            let db = state.analysis_host.raw_database_mut();
            db.file_text(file_id)
        };
        assert!(text2.contains("Test"));
        assert_eq!(text1, text2, "file lost after merge!");
    }

    #[test]
    fn external_changes_report_affects_open_documents() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        // No pending VFS changes → nothing to refresh.
        let empty = state.process_changes(false);
        assert!(!empty.affects_open_documents);
        assert!(!empty.config_file_changed);

        // A closed .bsl file changing on disk affects analysis of other (open) docs.
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                vfs::VfsPath::new("/cf/CommonModules/М/Ext/Module.bsl"),
                Some(Arc::from("Процедура А() Экспорт КонецПроцедуры")),
            );
        }
        let bsl = state.process_changes(false);
        assert!(bsl.affects_open_documents, "a .bsl change must mark open docs for refresh");
        assert!(!bsl.config_file_changed);

        // A metadata XML change likewise affects open docs (cross-config metadata).
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                vfs::VfsPath::new("/cf/Catalogs/Товары.xml"),
                Some(Arc::from("<MetaDataObject/>")),
            );
        }
        let meta = state.process_changes(false);
        assert!(
            meta.affects_open_documents,
            "a metadata XML change must mark open docs for refresh"
        );
        assert!(!meta.config_file_changed);

        // A suppressed batch (initial sync) must not claim metadata changes.
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                vfs::VfsPath::new("/cf/Catalogs/Услуги.xml"),
                Some(Arc::from("<MetaDataObject/>")),
            );
        }
        let suppressed = state.process_changes(true);
        assert!(
            !suppressed.affects_open_documents,
            "a suppressed initial-sync batch bumps nothing, so it reports no refresh"
        );
    }

    #[test]
    fn call_hierarchy_workspace_changes_journal_bodies_and_supersede_structure() {
        use crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId;

        // Given: a loaded source root with an active compact-index build. The
        // workspace root must exist and be valid: a config-file change only
        // counts as structural after its project reload succeeds.
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;
        state.init_empty_source_root();
        let tmp = tempfile::tempdir().unwrap();
        state.set_workspace_root(tmp.path().to_path_buf()).expect("valid empty workspace");
        let module_path = vfs::VfsPath::new("/cf/CommonModules/М/Ext/Module.bsl");
        state.vfs.write().set_file_contents(
            module_path.clone(),
            Some(Arc::from("Процедура А() КонецПроцедуры")),
        );
        state.process_changes(false);
        let module_id = state.vfs.read().file_id(&module_path).expect("module FileId");
        let root = base_db::SourceRootId(0);
        assert!(state.call_hierarchy_index.start_build(root, 1, CallHierarchyIndexSnapshotId(1)));
        let body_cancellation =
            state.call_hierarchy_index.cancellation(root, 1).expect("active build cancellation");

        // When: a body edit arrives, followed by file, XML, and config structure changes.
        state
            .vfs
            .write()
            .set_file_contents(module_path, Some(Arc::from("Процедура А()\nКонецПроцедуры")));
        state.process_changes(false);
        let body_journal = state.call_hierarchy_index.journal_files(root, 1);
        assert_eq!(body_journal, Some(vec![module_id]));
        assert!(!body_cancellation.is_cancelled());

        let added_path = vfs::VfsPath::new("/cf/CommonModules/Новый/Ext/Module.bsl");
        state
            .vfs
            .write()
            .set_file_contents(added_path.clone(), Some(Arc::from("Процедура Б() КонецПроцедуры")));
        state.process_changes(false);
        assert!(body_cancellation.is_cancelled());
        assert!(state.call_hierarchy_index.finish_superseded(root, 1));

        assert!(state.call_hierarchy_index.start_build(root, 2, CallHierarchyIndexSnapshotId(2)));
        state.vfs.write().set_file_contents(added_path, None);
        state.process_changes(false);
        assert!(state.call_hierarchy_index.finish_superseded(root, 2));

        assert!(state.call_hierarchy_index.start_build(root, 3, CallHierarchyIndexSnapshotId(3)));
        state.vfs.write().set_file_contents(
            vfs::VfsPath::new("/cf/Catalogs/Товары.xml"),
            Some(Arc::from("<MetaDataObject/>")),
        );
        state.process_changes(false);
        assert!(state.call_hierarchy_index.finish_superseded(root, 3));

        assert!(state.call_hierarchy_index.start_build(root, 4, CallHierarchyIndexSnapshotId(4)));
        state.vfs.write().set_file_contents(
            vfs::VfsPath::new("/cf/bsl-analyzer.toml"),
            Some(Arc::from("[source]")),
        );
        state.process_changes(false);

        // Then: only the body edit remains non-cancelling; every structural change queues one rebuild.
        assert!(state.call_hierarchy_index.finish_superseded(root, 4));
        assert!(state.call_hierarchy_index_rebuilds.contains(&root));
        assert_eq!(state.call_hierarchy_index_rebuilds.len(), 1);
    }

    #[test]
    fn bootstrap_metadata_substrate_resolves_and_reloads_per_mdo() {
        use ide_db::metadata::resolve_metadata_object;

        fn catalog_xml(name: &str, uuid: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="{uuid}">
        <Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_bootstrap_meta_{}_{}",
            std::process::id(),
            line!()
        ));
        let catalogs = root.join("Catalogs");
        std::fs::create_dir_all(&catalogs).unwrap();
        std::fs::write(
            catalogs.join("Справочник1.xml"),
            catalog_xml("Справочник1", "00000000-0000-0000-0000-000000000001"),
        )
        .unwrap();
        std::fs::write(
            catalogs.join("Товары.xml"),
            catalog_xml("Товары", "00000000-0000-0000-0000-000000000002"),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![(None, root.clone())]);

        state.bootstrap_metadata_substrate();

        let root_key = root.to_string_lossy().to_string();
        let db = state.analysis_host.raw_database();
        let listing = db.metadata_listing(&root_key).expect("listing set for the config root");

        let c1 = resolve_metadata_object(
            db,
            listing,
            bsl_metadata::MdoType::Catalog,
            "Справочник1".to_string(),
        )
        .expect("Справочник1 resolves from disk via the bootstrap");
        assert_eq!(c1.name, "Справочник1");
        let tovary_before = resolve_metadata_object(
            db,
            listing,
            bsl_metadata::MdoType::Catalog,
            "Товары".to_string(),
        )
        .expect("Товары resolves");
        assert_eq!(tovary_before.name, "Товары");

        // Edit Товары on disk, re-run the bootstrap (mirrors a reload). Товары
        // re-parses; the sibling Справочник1 stays memoised (per-MDO granularity).
        std::fs::write(
            catalogs.join("Товары.xml"),
            catalog_xml("Товары", "00000000-0000-0000-0000-0000000000ff"),
        )
        .unwrap();
        state.bootstrap_metadata_substrate();

        let db = state.analysis_host.raw_database();
        let listing = db.metadata_listing(&root_key).unwrap();
        let c1_after = resolve_metadata_object(
            db,
            listing,
            bsl_metadata::MdoType::Catalog,
            "Справочник1".to_string(),
        )
        .unwrap();
        assert!(
            Arc::ptr_eq(&c1, &c1_after),
            "a content edit to Товары must not re-parse the sibling Справочник1"
        );
        let tovary_after = resolve_metadata_object(
            db,
            listing,
            bsl_metadata::MdoType::Catalog,
            "Товары".to_string(),
        )
        .unwrap();
        assert_eq!(tovary_after.name, "Товары");
        assert!(
            !Arc::ptr_eq(&tovary_before, &tovary_after),
            "Товары re-parses after its XML changed on disk"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refresh_metadata_substrate_is_incremental_and_tracks_structure() {
        use ide_db::metadata::resolve_metadata_object;

        fn catalog_xml(name: &str, uuid: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="{uuid}">
        <Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#
            )
        }

        let cat = bsl_metadata::MdoType::Catalog;
        let root = std::env::temp_dir().join(format!(
            "bsl_refresh_meta_{}_{}",
            std::process::id(),
            line!()
        ));
        let catalogs = root.join("Catalogs");
        std::fs::create_dir_all(&catalogs).unwrap();
        let write = |name: &str, uuid: &str| {
            std::fs::write(catalogs.join(format!("{name}.xml")), catalog_xml(name, uuid)).unwrap()
        };
        write("Справочник1", "00000000-0000-0000-0000-000000000001");
        write("Товары", "00000000-0000-0000-0000-000000000002");

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![(None, root.clone())]);
        state.bootstrap_metadata_substrate();

        let root_key = root.to_string_lossy().to_string();
        let resolve = |state: &GlobalState, name: &str| {
            let db = state.analysis_host.raw_database();
            let listing = db.metadata_listing(&root_key).unwrap();
            resolve_metadata_object(db, listing, cat, name.to_string())
        };

        let c1 = resolve(&state, "Справочник1").expect("Справочник1");
        let tovary = resolve(&state, "Товары").expect("Товары");

        // CONTENT edit to Товары: re-read only it. Товары re-parses; Справочник1
        // stays memoised (no full re-bootstrap, no sibling churn).
        write("Товары", "00000000-0000-0000-0000-0000000000ff");
        state.refresh_metadata_substrate(&[catalogs.join("Товары.xml")]);
        assert!(
            Arc::ptr_eq(&c1, &resolve(&state, "Справочник1").unwrap()),
            "a content edit to Товары must not re-resolve the sibling"
        );
        assert!(
            !Arc::ptr_eq(&tovary, &resolve(&state, "Товары").unwrap()),
            "Товары re-parses after its content changed"
        );
        let c1 = resolve(&state, "Справочник1").unwrap();

        // STRUCTURE add: a brand-new catalog appears and resolves; the absent-key
        // miss from before is invalidated through config_index.
        assert!(resolve(&state, "Услуги").is_none(), "Услуги absent before the add");
        write("Услуги", "00000000-0000-0000-0000-000000000003");
        state.refresh_metadata_substrate(&[catalogs.join("Услуги.xml")]);
        assert_eq!(resolve(&state, "Услуги").expect("Услуги after add").name, "Услуги");
        assert!(
            Arc::ptr_eq(&c1, &resolve(&state, "Справочник1").unwrap()),
            "a structure add must not re-resolve an untouched sibling"
        );

        // STRUCTURE remove: deleting a catalog tombstones it (resolve -> None).
        std::fs::remove_file(catalogs.join("Товары.xml")).unwrap();
        state.refresh_metadata_substrate(&[catalogs.join("Товары.xml")]);
        assert!(resolve(&state, "Товары").is_none(), "removed catalog resolves to None");
        assert_eq!(resolve(&state, "Услуги").expect("Услуги still present").name, "Услуги");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_metadata_object_for_file_matches_merged_visible_configuration() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::ConfigsDatabase;

        let cat = bsl_metadata::MdoType::Catalog;
        let root = std::env::temp_dir().join(format!(
            "bsl_resolve_parity_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(main_root.join("Catalogs")).unwrap();
        std::fs::create_dir_all(ext_root.join("Catalogs")).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        // Same catalog in both roots; the extension adopts it (ObjectBelonging), so
        // the resolved object goes through the main + extension overlay path.
        std::fs::write(
            main_root.join("Catalogs/Номенклатура.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Номенклатура</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#,
        )
        .unwrap();
        std::fs::write(
            ext_root.join("Catalogs/Номенклатура.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000002">
        <Properties><ObjectBelonging>Adopted</ObjectBelonging><Name>Номенклатура</Name></Properties>
    </Catalog>
</MetaDataObject>"#,
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        // A .bsl file living in the extension's scope: it must see the merged object.
        // Allocate via the VFS *after* the bootstrap so its FileId does not collide
        // with the ones the bootstrap already interned for the catalog XMLs.
        let bsl_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let per_mdo = db
            .resolve_metadata_object_for_file(file_id, cat, "Номенклатура")
            .expect("per-MDO resolve finds the merged catalog");
        let whole = db.merged_visible_configuration(file_id).expect("merged config loads");
        let from_whole =
            whole.find_metadata_object(cat, "Номенклатура").expect("merged config has the catalog");

        assert_eq!(
            &*per_mdo, from_whole,
            "per-MDO resolve (with extension overlay) must equal the merged whole-config lookup"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_event_subscription_for_file_matches_merged_visible_configuration() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::ConfigsDatabase;

        fn event_subscription_xml(name: &str, event: &str, handler: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="00000000-0000-0000-0000-000000000061">
        <Properties>
            <Name>{name}</Name>
            <Source><Type>CatalogRef.Номенклатура</Type></Source>
            <Event>{event}</Event>
            <Handler>{handler}</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_event_subscription_parity_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(main_root.join("EventSubscriptions")).unwrap();
        std::fs::create_dir_all(ext_root.join("EventSubscriptions")).unwrap();
        std::fs::create_dir_all(&ext_root).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            main_root.join("EventSubscriptions/ПередЗаписью.xml"),
            event_subscription_xml(
                "ПередЗаписью",
                "BeforeWrite",
                "CommonModule.ПодпискиНаСобытия.ПередЗаписью",
            ),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("EventSubscriptions/ПередЗаписью.xml"),
            event_subscription_xml(
                "ПередЗаписью",
                "BeforeWriteExtension",
                "CommonModule.РасширениеПодписки.ПередЗаписью",
            ),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("EventSubscriptions/ТолькоРасширение.xml"),
            event_subscription_xml(
                "ТолькоРасширение",
                "AfterWrite",
                "CommonModule.РасширениеПодписки.ТолькоРасширение",
            ),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        let bsl_path = ext_root.join("EventSubscriptionConsumer.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let per_kind = db
            .resolve_event_subscription_for_file(file_id, "ПередЗаписью")
            .expect("per-kind resolve finds the event subscription visible to the file");
        let whole = db.merged_visible_configuration(file_id).expect("merged config loads");
        let from_whole = whole
            .find_event_subscription("ПередЗаписью")
            .expect("merged config has the event subscription");

        assert_eq!(
            &*per_kind, from_whole,
            "per-kind event-subscription resolve must equal the merged whole-config lookup"
        );
        assert_eq!(per_kind.event(), "BeforeWrite");
        assert!(
            db.resolve_event_subscription_for_file(file_id, "ТолькоРасширение").is_some(),
            "an extension file sees subscriptions declared by its own extension"
        );
        assert_eq!(
            db.main_event_subscription_names_for_file(file_id),
            vec!["ПередЗаписью".to_string()],
            "the separate main-only accessor must keep its old scope"
        );
        assert_eq!(
            db.event_subscription_names(file_id),
            vec!["ПередЗаписью".to_string(), "ТолькоРасширение".to_string()]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_scheduled_job_for_file_matches_merged_visible_configuration() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::ConfigsDatabase;

        fn scheduled_job_xml(name: &str, method_name: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <ScheduledJob uuid="00000000-0000-0000-0000-000000000073">
        <Properties>
            <Name>{name}</Name>
            <MethodName>{method_name}</MethodName>
            <Use>true</Use>
            <Predefined>false</Predefined>
        </Properties>
    </ScheduledJob>
</MetaDataObject>"#
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_scheduled_job_parity_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(main_root.join("ScheduledJobs")).unwrap();
        std::fs::create_dir_all(ext_root.join("ScheduledJobs")).unwrap();
        std::fs::create_dir_all(&ext_root).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            main_root.join("ScheduledJobs/РегламентноеЗадание1.xml"),
            scheduled_job_xml(
                "РегламентноеЗадание1",
                "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедура",
            ),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("ScheduledJobs/РегламентноеЗадание1.xml"),
            scheduled_job_xml(
                "РегламентноеЗадание1",
                "CommonModule.РасширениеПодписки.НеУстаревшаяПроцедура",
            ),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("ScheduledJobs/ТолькоРасширение.xml"),
            scheduled_job_xml(
                "ТолькоРасширение",
                "CommonModule.РасширениеПодписки.ТолькоРасширение",
            ),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        let bsl_path = ext_root.join("ScheduledJobConsumer.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let per_kind = db
            .resolve_scheduled_job_for_file(file_id, "РегламентноеЗадание1")
            .expect("per-kind resolve finds the scheduled job visible to the file");
        let whole = db.merged_visible_configuration(file_id).expect("merged config loads");
        let from_whole = whole
            .find_scheduled_job("РегламентноеЗадание1")
            .expect("merged config has the scheduled job");

        assert_eq!(
            &*per_kind, from_whole,
            "per-kind scheduled-job resolve must equal the merged whole-config lookup"
        );
        assert_eq!(per_kind.method_name(), "CommonModule.ПервыйОбщийМодуль.НеУстаревшаяПроцедура");
        assert!(
            db.resolve_scheduled_job_for_file(file_id, "ТолькоРасширение").is_none(),
            "extension-only scheduled jobs stay invisible until merged whole-config supports them"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_role_for_file_matches_merged_visible_configuration() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::ConfigsDatabase;

        fn role_xml(name: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000082">
        <Properties>
            <Name>{name}</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#
            )
        }

        fn rights_xml(set_for_new_objects: bool, object_name: &str, condition: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>{set_for_new_objects}</setForNewObjects>
    <setForAttributesByDefault>false</setForAttributesByDefault>
    <independentRightsOfChildObjects>false</independentRightsOfChildObjects>
    <object>
        <name>{object_name}</name>
        <right>
            <name>Read</name>
            <value>true</value>
            <restrictionByCondition>
                <condition>{condition}</condition>
            </restrictionByCondition>
        </right>
    </object>
</Rights>"#
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_role_parity_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(main_root.join("Roles/ТестоваяРоль/Ext")).unwrap();
        std::fs::create_dir_all(ext_root.join("Roles/ТестоваяРоль/Ext")).unwrap();
        std::fs::create_dir_all(ext_root.join("Roles/ТолькоРасширение/Ext")).unwrap();
        std::fs::create_dir_all(&ext_root).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(main_root.join("Roles/ТестоваяРоль.xml"), role_xml("ТестоваяРоль")).unwrap();
        std::fs::write(
            main_root.join("Roles/ТестоваяРоль/Ext/Rights.xml"),
            rights_xml(
                false,
                "Catalog.Контрагенты",
                "Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.Организации)",
            ),
        )
        .unwrap();
        std::fs::write(ext_root.join("Roles/ТестоваяРоль.xml"), role_xml("ТестоваяРоль")).unwrap();
        std::fs::write(
            ext_root.join("Roles/ТестоваяРоль/Ext/Rights.xml"),
            rights_xml(
                false,
                "Catalog.Контрагенты",
                "Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.ФизическиеЛица)",
            ),
        )
        .unwrap();
        std::fs::write(ext_root.join("Roles/ТолькоРасширение.xml"), role_xml("ТолькоРасширение"))
            .unwrap();
        std::fs::write(
            ext_root.join("Roles/ТолькоРасширение/Ext/Rights.xml"),
            rights_xml(false, "Catalog.Контрагенты", "Контрагенты.Код = \"01\""),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        let bsl_path = ext_root.join("RoleConsumer.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let per_kind = db
            .resolve_role_for_file(file_id, "ТестоваяРоль")
            .expect("role resolves through the bootstrapped per-kind substrate");
        let whole = db.merged_visible_configuration(file_id).expect("merged config loads");
        let from_whole = whole.find_role("ТестоваяРоль").expect("merged config has the role");

        assert_eq!(
            &*per_kind, from_whole,
            "per-kind role resolve must equal the merged whole-config lookup"
        );
        assert_eq!(per_kind.data().objects()[0].name, "Контрагенты");
        assert!(
            db.resolve_role_for_file(file_id, "ТолькоРасширение").is_none(),
            "extension-only roles stay invisible until merged whole-config supports them"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rights_xml_edit_updates_role_parity() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::ConfigsDatabase;

        fn role_xml(name: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000083">
        <Properties>
            <Name>{name}</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#
            )
        }

        fn rights_xml(set_for_new_objects: bool, object_name: &str, condition: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>{set_for_new_objects}</setForNewObjects>
    <setForAttributesByDefault>false</setForAttributesByDefault>
    <independentRightsOfChildObjects>false</independentRightsOfChildObjects>
    <object>
        <name>{object_name}</name>
        <right>
            <name>Read</name>
            <value>true</value>
            <restrictionByCondition>
                <condition>{condition}</condition>
            </restrictionByCondition>
        </right>
    </object>
</Rights>"#
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_role_rights_refresh_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(main_root.join("Roles/ТестоваяРоль/Ext")).unwrap();
        std::fs::create_dir_all(&ext_root).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(main_root.join("Roles/ТестоваяРоль.xml"), role_xml("ТестоваяРоль")).unwrap();
        let rights_path = main_root.join("Roles/ТестоваяРоль/Ext/Rights.xml");
        std::fs::write(
            &rights_path,
            rights_xml(
                false,
                "Catalog.Контрагенты",
                "Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.Организации)",
            ),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        let bsl_path = ext_root.join("RoleConsumer.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let resolved_before = db
            .resolve_role_for_file(file_id, "ТестоваяРоль")
            .expect("role resolves before the rights edit");
        assert!(!resolved_before.data().set_for_new_objects());

        std::fs::write(
            &rights_path,
            rights_xml(
                true,
                "Catalog.Контрагенты",
                "Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.ФизическиеЛица)",
            ),
        )
        .unwrap();
        assert!(state.refresh_metadata_substrate(std::slice::from_ref(&rights_path)));

        let resolved_after = state
            .analysis_host
            .raw_database()
            .resolve_role_for_file(file_id, "ТестоваяРоль")
            .expect("role re-resolves after the rights edit");
        let whole = state
            .analysis_host
            .raw_database()
            .merged_visible_configuration(file_id)
            .expect("merged config loads");
        let from_whole = whole.find_role("ТестоваяРоль").expect("merged config has the role");

        assert_eq!(&*resolved_after, from_whole, "role parity must stay aligned after refresh");
        assert!(resolved_after.data().set_for_new_objects());
        assert_eq!(
            resolved_after.data().objects()[0].restrictions,
            vec!["Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.ФизическиеЛица)".to_string()]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn bootstrapped_common_module_resolves_by_name_and_by_body_file() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use bsl_metadata::traits::MdObject;

        let root = std::env::temp_dir().join(format!(
            "bsl_common_module_{}_{}",
            std::process::id(),
            line!()
        ));
        let cf = root.join("src/cf");
        let cm_dir = cf.join("CommonModules");
        std::fs::create_dir_all(cm_dir.join("МойМодуль/Ext")).unwrap();
        std::fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            cm_dir.join("МойМодуль.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000021">
        <Properties><Name>МойМодуль</Name><Global>true</Global><Server>true</Server><Privileged>true</Privileged></Properties>
    </CommonModule>
</MetaDataObject>"#,
        )
        .unwrap();
        let bsl_path = cm_dir.join("МойМодуль/Ext/Module.bsl");
        std::fs::write(&bsl_path, "Функция Ф() Экспорт КонецФункции").unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        // Intern the module body into the VFS BEFORE the bootstrap, mirroring the
        // real boot order (`init_source_root` runs before
        // `bootstrap_metadata_substrate`). Discovery then resolves the body's FileId
        // through the same interner the analyzer uses — the path-normalization parity
        // the by-body reverse index depends on.
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let bsl_file = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        {
            let mut file_set = vfs::file_set::FileSet::new();
            file_set.insert(bsl_file, bsl_vfs_path);
            let db = state.analysis_host.raw_database_mut();
            db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
            db.set_file_source_root(bsl_file, BSL_SOURCE_ROOT);
            db.set_file_text(bsl_file, "Функция Ф() Экспорт КонецФункции");
        }

        state.analysis_host.raw_database_mut().set_all_config_paths(vec![(None, cf.clone())]);
        state.bootstrap_metadata_substrate();

        let db = state.analysis_host.raw_database();

        // By-name resolution through the per-common-module substrate, flags intact.
        let by_name = db
            .resolve_common_module_for_file(bsl_file, "МойМодуль")
            .expect("common module resolves by name through the bootstrapped substrate");
        assert!(by_name.is_global() && by_name.is_privileged(), "metadata flags survive parsing");
        // Case-insensitive, like the whole-config lookup.
        assert!(db.resolve_common_module_for_file(bsl_file, "моймодуль").is_some());

        // By-body reverse index: the FileId the analyzer holds for Module.bsl maps
        // back to its common module — proves discovery interned the same path/FileId.
        let by_file = db
            .common_module_for_file_id(bsl_file)
            .expect("Module.bsl resolves to its common module via the reverse index");
        assert_eq!(by_file.name(), "МойМодуль");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn bootstrapped_common_module_scopes_base_everywhere_extension_private() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};

        // Common-module visibility (same scoping as metadata objects): a main-config
        // common module is visible everywhere, but an extension's common module is
        // visible only within that extension — a sibling extension's modules are not.
        let root =
            std::env::temp_dir().join(format!("bsl_cm_xext_{}_{}", std::process::id(), line!()));
        let base = root.join("src/cf");
        let ext_a = root.join("src/cfe/A");
        let ext_b = root.join("src/cfe/B");
        for dir in [&base, &ext_a, &ext_b] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("Configuration.xml"), "<Configuration/>").unwrap();
        }
        let common_module = |dir: &std::path::Path, name: &str, uuid: &str| {
            std::fs::create_dir_all(dir.join(format!("CommonModules/{name}/Ext"))).unwrap();
            std::fs::write(
                dir.join(format!("CommonModules/{name}.xml")),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="{uuid}"><Properties><Name>{name}</Name><Server>true</Server></Properties></CommonModule>
</MetaDataObject>"#
                ),
            )
            .unwrap();
            std::fs::write(
                dir.join(format!("CommonModules/{name}/Ext/Module.bsl")),
                "Процедура П() Экспорт КонецПроцедуры",
            )
            .unwrap();
        };
        common_module(&base, "ОбщийБаза", "00000000-0000-0000-0000-000000000ba1");
        common_module(&ext_b, "ОбщийБ", "00000000-0000-0000-0000-0000000000b1");

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, base.clone()),
            (Some("A".to_string()), ext_a.clone()),
            (Some("B".to_string()), ext_b.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        // A .bsl file living in extension A (after bootstrap, so its FileId does not
        // collide with the bootstrap-interned XML ids).
        let a_bsl = ext_a.join("CommonModules/М/Ext/Module.bsl");
        let a_vp = vfs::VfsPath::new(a_bsl.to_string_lossy().as_ref());
        let a_file = state.vfs.write().alloc_file_id(a_vp.clone());
        {
            let db = state.analysis_host.raw_database_mut();
            let mut fs = vfs::file_set::FileSet::new();
            fs.insert(a_file, a_vp);
            db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(fs));
            db.set_file_source_root(a_file, BSL_SOURCE_ROOT);
            db.set_file_text(a_file, "Процедура Т() КонецПроцедуры");
        }

        let db = state.analysis_host.raw_database();
        assert!(
            db.resolve_common_module_for_file(a_file, "ОбщийБаза").is_some(),
            "a main-config common module must be visible to a file in extension A"
        );
        assert!(
            db.resolve_common_module_for_file(a_file, "ОбщийБ").is_none(),
            "a sibling extension B's common module must NOT be visible to a file in extension A"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_metadata_object_for_file_scopes_base_and_extensions() {
        // Pins the 1C visibility rules for per-MDO resolution:
        //  - a file in the base config sees only the base (extensions invisible);
        //  - a file in extension A sees base + A merged (A priority), never B;
        //  - extensions do not bleed into each other.
        let cat = bsl_metadata::MdoType::Catalog;
        let root = std::env::temp_dir().join(format!(
            "bsl_resolve_scope_{}_{}",
            std::process::id(),
            line!()
        ));
        let base = root.join("src/cf");
        let ext_a = root.join("src/cfe/A");
        let ext_b = root.join("src/cfe/B");
        for dir in [&base, &ext_a, &ext_b] {
            std::fs::create_dir_all(dir.join("Catalogs")).unwrap();
            std::fs::write(dir.join("Configuration.xml"), "<Configuration/>").unwrap();
        }
        let catalog = |name: &str, uuid: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="{uuid}"><Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties></Catalog>
</MetaDataObject>"#
            )
        };
        std::fs::write(
            base.join("Catalogs/Общий.xml"),
            catalog("Общий", "00000000-0000-0000-0000-000000000001"),
        )
        .unwrap();
        std::fs::write(
            ext_a.join("Catalogs/ТолькоА.xml"),
            catalog("ТолькоА", "00000000-0000-0000-0000-00000000000a"),
        )
        .unwrap();
        std::fs::write(
            ext_b.join("Catalogs/ТолькоБ.xml"),
            catalog("ТолькоБ", "00000000-0000-0000-0000-00000000000b"),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, base.clone()),
            (Some("A".to_string()), ext_a.clone()),
            (Some("B".to_string()), ext_b.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        // Allocate one .bsl file per scope (after the bootstrap, so FileIds don't
        // collide with the catalog XML ids).
        let mut make_file = |dir: &std::path::Path| {
            use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
            let p = dir.join("CommonModules/М/Ext/Module.bsl");
            let vp = vfs::VfsPath::new(p.to_string_lossy().as_ref());
            let fid = state.vfs.write().alloc_file_id(vp.clone());
            let db = state.analysis_host.raw_database_mut();
            let sr = db.source_root_input(BSL_SOURCE_ROOT).root(db);
            let mut fs = sr.file_set().clone();
            fs.insert(fid, vp);
            db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(fs));
            db.set_file_source_root(fid, BSL_SOURCE_ROOT);
            db.set_file_text(fid, "Процедура Т() КонецПроцедуры");
            fid
        };
        let base_file = make_file(&base);
        let a_file = make_file(&ext_a);
        let b_file = make_file(&ext_b);

        let db = state.analysis_host.raw_database();
        let r = |fid: vfs::FileId, name: &str| {
            db.resolve_metadata_object_for_file(fid, cat, name).map(|m| m.name.clone())
        };

        // Base file: only base objects, no extension objects.
        assert_eq!(r(base_file, "Общий").as_deref(), Some("Общий"));
        assert_eq!(r(base_file, "ТолькоА"), None, "base must not see extension A's object");
        assert_eq!(r(base_file, "ТолькоБ"), None, "base must not see extension B's object");

        // Extension A: base + A, never B.
        assert_eq!(r(a_file, "Общий").as_deref(), Some("Общий"), "A sees base objects");
        assert_eq!(r(a_file, "ТолькоА").as_deref(), Some("ТолькоА"), "A sees its own object");
        assert_eq!(r(a_file, "ТолькоБ"), None, "A must not see extension B's object");

        // Extension B: base + B, never A.
        assert_eq!(r(b_file, "ТолькоБ").as_deref(), Some("ТолькоБ"), "B sees its own object");
        assert_eq!(r(b_file, "ТолькоА"), None, "B must not see extension A's object");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn bootstrap_resolves_register_per_mdo() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};

        // The real designer fixture (guaranteed-parseable register XML).
        let root = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer"
        ));

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![(None, root.clone())]);
        state.bootstrap_metadata_substrate();

        let bsl_path = root.join("CommonModules/М/Ext/Module.bsl");
        let vp = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let fid = state.vfs.write().alloc_file_id(vp.clone());
        let db = state.analysis_host.raw_database_mut();
        let mut fs = vfs::file_set::FileSet::new();
        fs.insert(fid, vp);
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(fs));
        db.set_file_source_root(fid, BSL_SOURCE_ROOT);
        db.set_file_text(fid, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        // Register resolves through the bootstrapped per-MDO path...
        let reg = db
            .resolve_register_for_file(
                fid,
                bsl_metadata::MdoType::InformationRegister,
                "РегистрСведений1",
            )
            .expect("register resolves per-MDO from the fixture");
        assert_eq!(reg.name(), "РегистрСведений1");
        // ...and an object kind does not resolve through the register query.
        assert!(
            db.resolve_register_for_file(fid, bsl_metadata::MdoType::Catalog, "Справочник1",)
                .is_none(),
            "a catalog must not resolve as a register"
        );
    }

    #[test]
    fn init_source_root_excludes_metadata_from_root0() {
        use base_db::{SourceDatabase, BSL_SOURCE_ROOT};

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        let bsl_a = "/proj/CommonModules/М/Ext/Module.bsl";
        let bsl_b = "/proj/Catalogs/Товары/Ext/ObjectModule.bsl";
        let xml_a = "/proj/Catalogs/Товары.xml";
        let xml_b = "/proj/Catalogs/Товары/Ext/Predefined.xml";
        {
            let mut vfs = state.vfs.write();
            for p in [bsl_a, bsl_b, xml_a, xml_b] {
                vfs.set_file_contents(vfs::VfsPath::new(p), Some(Arc::from("x")));
            }
        }
        state.process_changes(false);
        state.init_source_root();

        let db = state.analysis_host.raw_database_mut();
        let bsl_root = db.source_root_input(BSL_SOURCE_ROOT).root(db);

        let in_root = |root: &base_db::SourceRoot, p: &str| {
            root.file_set().file_for_path(&vfs::VfsPath::new(p)).is_some()
        };

        // root(0) holds the .bsl sources and never the metadata XML; metadata files
        // belong to the bootstrap-owned metadata root(1).
        assert!(in_root(bsl_root, bsl_a) && in_root(bsl_root, bsl_b), "bsl in root(0)");
        assert!(!in_root(bsl_root, xml_a) && !in_root(bsl_root, xml_b), "xml NOT in root(0)");
    }

    #[test]
    fn remove_directories_tombstones_descendants_only() {
        use base_db::{SourceDatabase, SourceRootId};

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        let inside_a = "proj/Catalogs/X/Ext/ObjectModule.bsl";
        let inside_b = "proj/Catalogs/X/Forms/F/Ext/Form/Module.bsl";
        let outside = "proj/Catalogs/Y/Ext/ObjectModule.bsl";
        {
            let mut vfs = state.vfs.write();
            for p in [inside_a, inside_b, outside] {
                vfs.set_file_contents(
                    vfs::VfsPath::new(crate::test_uri::file_path(p)),
                    Some(Arc::from("Процедура А() КонецПроцедуры")),
                );
            }
        }
        state.process_changes(false);

        let in_file_set = |state: &GlobalState, p: &str| {
            let db = state.analysis_host.raw_database();
            let sr = db.source_root_input(SourceRootId(0)).root(db);
            sr.file_set().file_for_path(&vfs::VfsPath::new(crate::test_uri::file_path(p))).is_some()
        };
        assert!(in_file_set(&state, inside_a) && in_file_set(&state, outside), "baseline loaded");

        // Remove the directory subtree "Catalogs/X".
        let removed =
            vec![paths::AbsPathBuf::assert_utf8(crate::test_uri::file_path("proj/Catalogs/X"))];
        let refreshed = state.remove_directories(&removed);

        assert!(refreshed, "a subtree removal should request an open-document refresh");
        assert!(!in_file_set(&state, inside_a), "descendant ObjectModule must be tombstoned");
        assert!(!in_file_set(&state, inside_b), "nested descendant must be tombstoned");
        assert!(in_file_set(&state, outside), "a sibling directory must be untouched");
    }

    #[test]
    fn is_open_document_path_detects_open_by_file_id() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        let p = "/proj/CommonModules/М/Ext/Module.bsl";
        let vfs_path = vfs::VfsPath::new(p);
        // Mark the file open by FileId ONLY (no mem_docs URL entry) — this models a
        // watcher URL that doesn't byte-match the client's didOpen URL.
        let file_id = state.vfs.write().alloc_file_id(vfs_path.clone());
        state.open_files.insert(file_id);

        assert!(
            state.is_open_document_path(std::path::Path::new(p), &vfs_path),
            "an open file must be recognized by its FileId even without a mem_docs URL"
        );

        let other = "/proj/CommonModules/Other/Ext/Module.bsl";
        assert!(
            !state.is_open_document_path(std::path::Path::new(other), &vfs::VfsPath::new(other)),
            "an unknown file is not open"
        );
    }

    #[test]
    fn closed_file_disk_backed_and_overlay_to_disk_transition() {
        use base_db::SourceDatabase;

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("mod.bsl");
        std::fs::write(&path, "Процедура А() КонецПроцедуры").expect("write");

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        let uri = Url::from_file_path(&path).unwrap();
        let file_id = state.vfs_file_for_url(&uri).unwrap();

        // 1) A closed file (not in open_files) is disk-backed: its text is read
        //    from disk on demand, not stored as a resident overlay.
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                vfs::VfsPath::new(path.clone()),
                Some(Arc::from("Процедура А() КонецПроцедуры")),
            );
        }
        state.process_changes(false);
        {
            let db = state.analysis_host.raw_database_mut();
            assert!(db.try_file_text(file_id).is_none(), "closed file must have no overlay");
            assert_eq!(&*db.file_text(file_id), "Процедура А() КонецПроцедуры");
        }

        // 2) Opening it stores an authoritative overlay (an unsaved edit not yet
        //    on disk).
        state.open_files.insert(file_id);
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                vfs::VfsPath::new(path.clone()),
                Some(Arc::from("Процедура Б() КонецПроцедуры")),
            );
        }
        state.process_changes(false);
        {
            let db = state.analysis_host.raw_database_mut();
            assert_eq!(&*db.file_text(file_id), "Процедура Б() КонецПроцедуры");
        }

        // 3) Disk changes under us, then the file closes: re-keying to disk must
        //    clear the stale overlay and read the new disk bytes WITHOUT a
        //    revision-mismatch panic (regression guard for the overlay-clear).
        std::fs::write(&path, "Процедура В() КонецПроцедуры").expect("rewrite");
        state.open_files.remove(&file_id);
        {
            let disk = std::fs::read_to_string(&path).unwrap();
            let db = state.analysis_host.raw_database_mut();
            db.set_file_revision_from_disk(file_id, base_db::content_revision(&disk));
        }
        {
            let db = state.analysis_host.raw_database_mut();
            assert!(
                db.try_file_text(file_id).is_none(),
                "overlay must be cleared on disk transition"
            );
            assert_eq!(&*db.file_text(file_id), "Процедура В() КонецПроцедуры");
        }
    }

    fn progress_kinds(receiver: &Receiver<Message>) -> Vec<String> {
        let mut kinds = Vec::new();
        while let Ok(msg) = receiver.try_recv() {
            if let Message::Notification(not) = msg {
                if not.method == "$/progress" {
                    if let Some(kind) =
                        not.params.get("value").and_then(|v| v.get("kind")).and_then(|k| k.as_str())
                    {
                        kinds.push(kind.to_owned());
                    }
                }
            }
        }
        kinds
    }

    #[test]
    fn analysis_progress_shows_after_debounce_and_ends_when_idle() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);

        // Two concurrent jobs in flight; the rising edge armed one debounce epoch.
        // The guards model the in-flight jobs; completion is simulated below via
        // the same `note_analysis_finished` the guard's task triggers on the loop.
        let _guard_a = state.note_analysis_spawned();
        let _guard_b = state.note_analysis_spawned();
        let epoch = state.analysis_epoch;

        // A tick from a different burst must not show anything.
        state.handle_analysis_progress_tick(epoch.wrapping_sub(1));
        assert!(!state.analysis_progress_active, "stale-epoch tick must not show the indicator");

        // The current burst's tick promotes to a visible Begin.
        state.handle_analysis_progress_tick(epoch);
        assert!(state.analysis_progress_active);

        // A duplicate same-epoch tick must not re-Begin.
        state.handle_analysis_progress_tick(epoch);

        // The indicator stays while any job remains and ends only when the last drains.
        state.note_analysis_finished();
        assert!(state.analysis_progress_active, "indicator stays while work remains");
        state.note_analysis_finished();
        assert!(!state.analysis_progress_active, "indicator ends when the last job drains");

        // Exactly one Begin and one End, despite the duplicate tick.
        assert_eq!(progress_kinds(&receiver), vec!["begin".to_owned(), "end".to_owned()]);
    }

    #[test]
    fn analysis_progress_skips_indicator_for_fast_burst() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);

        // A burst that finishes before its debounce tick is delivered.
        let _guard = state.note_analysis_spawned();
        let epoch = state.analysis_epoch;
        state.note_analysis_finished();
        assert_eq!(state.analysis_in_flight, 0);

        // A `finished` while nothing is shown must not emit an End.
        state.handle_analysis_progress_tick(epoch);
        assert!(!state.analysis_progress_active);
        assert!(progress_kinds(&receiver).is_empty(), "a fast burst emits no progress");
    }

    #[test]
    fn analysis_progress_ignores_prior_burst_tick_after_new_burst() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);

        // Burst A starts and finishes within its debounce (no indicator yet).
        let _guard_a = state.note_analysis_spawned();
        let epoch_a = state.analysis_epoch;
        state.note_analysis_finished();
        assert_eq!(state.analysis_in_flight, 0);

        // Burst B starts; its rising edge bumps the epoch.
        let _guard_b = state.note_analysis_spawned();
        let epoch_b = state.analysis_epoch;
        assert_ne!(epoch_a, epoch_b, "a new burst must get a fresh epoch");

        // A's late debounce tick must be ignored — it must not show B's indicator.
        state.handle_analysis_progress_tick(epoch_a);
        assert!(!state.analysis_progress_active, "stale prior-burst tick must not begin");

        // B's own tick shows the indicator; finishing B ends it.
        state.handle_analysis_progress_tick(epoch_b);
        assert!(state.analysis_progress_active);
        state.note_analysis_finished();
        assert!(!state.analysis_progress_active);

        assert_eq!(progress_kinds(&receiver), vec!["begin".to_owned(), "end".to_owned()]);
    }

    #[test]
    fn resolve_http_service_for_file_matches_merged_visible_configuration() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::ConfigsDatabase;

        fn http_service_xml(name: &str, root_url: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <HTTPService uuid="4797cd39-952d-4e4d-9685-014e4d5a8e{root_url_pad}">
        <Properties>
            <Name>{name}</Name>
            <RootURL>{root_url}</RootURL>
        </Properties>
        <ChildObjects>
            <URLTemplate uuid="7124b2c7-d38e-40b9-a934-e6eb9de99340">
                <Properties>
                    <Name>URLTemplate1</Name>
                    <Template>/storage/{{Storage}}/{{ID}}</Template>
                </Properties>
                <ChildObjects>
                    <Method uuid="605f52a9-e95b-4900-9e41-449d7da01348">
                        <Properties>
                            <Name>GET</Name>
                            <HTTPMethod>GET</HTTPMethod>
                            <Handler>{handler_get}</Handler>
                        </Properties>
                    </Method>
                </ChildObjects>
            </URLTemplate>
        </ChildObjects>
    </HTTPService>
</MetaDataObject>"#,
                root_url_pad = if root_url == "http" { "e25" } else { "e26" },
                handler_get = if root_url == "http" { "URLTemplate1GET" } else { "ExtensionGET" }
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_http_service_parity_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(main_root.join("HTTPServices/МойСервис")).unwrap();
        std::fs::create_dir_all(ext_root.join("HTTPServices/МойСервис")).unwrap();
        std::fs::create_dir_all(ext_root.join("HTTPServices/ТолькоРасширение")).unwrap();
        std::fs::create_dir_all(&ext_root).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            main_root.join("HTTPServices/МойСервис.xml"),
            http_service_xml("МойСервис", "http"),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("HTTPServices/МойСервис.xml"),
            http_service_xml("МойСервис", "ext"),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("HTTPServices/ТолькоРасширение.xml"),
            http_service_xml("ТолькоРасширение", "x"),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        let bsl_path = ext_root.join("HTTPServiceConsumer.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let per_kind = db
            .resolve_http_service_for_file(file_id, "МойСервис")
            .expect("HTTP service resolves through the bootstrapped per-kind substrate");
        let whole = db.merged_visible_configuration(file_id).expect("merged config loads");
        let from_whole =
            whole.find_http_service("МойСервис").expect("merged config has the HTTP service");

        assert_eq!(
            &*per_kind, from_whole,
            "per-kind HTTP-service resolve must equal the merged whole-config lookup"
        );
        assert_eq!(per_kind.url_templates()[0].methods()[0].handler(), "URLTemplate1GET");
        assert!(
            db.resolve_http_service_for_file(file_id, "ТолькоРасширение").is_none(),
            "extension-only HTTP services stay invisible until merged whole-config supports them"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_web_service_for_file_matches_merged_visible_configuration() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::ConfigsDatabase;

        fn web_service_xml(name: &str, namespace: &str, procedure: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <WebService uuid="0b4a4c9c-76e9-455c-9471-249051a83{proc_pad}">
        <Properties>
            <Name>{name}</Name>
            <Namespace>{namespace}</Namespace>
        </Properties>
        <ChildObjects>
            <Operation uuid="bc99d837-aee6-40ee-8940-3a81dddf477c">
                <Properties>
                    <Name>Операция1</Name>
                    <ProcedureName>{procedure}</ProcedureName>
                </Properties>
                <ChildObjects/>
            </Operation>
        </ChildObjects>
    </WebService>
</MetaDataObject>"#,
                proc_pad = if procedure.is_empty() { "0d" } else { "1d" }
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_web_service_parity_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(main_root.join("WebServices/МойСервис")).unwrap();
        std::fs::create_dir_all(ext_root.join("WebServices/МойСервис")).unwrap();
        std::fs::create_dir_all(ext_root.join("WebServices/ТолькоРасширение")).unwrap();
        std::fs::create_dir_all(&ext_root).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            main_root.join("WebServices/МойСервис.xml"),
            web_service_xml("МойСервис", "main.com", "Операция1"),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("WebServices/МойСервис.xml"),
            web_service_xml("МойСервис", "ext.com", "Операция1Расширение"),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("WebServices/ТолькоРасширение.xml"),
            web_service_xml("ТолькоРасширение", "x.com", ""),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        let bsl_path = ext_root.join("WebServiceConsumer.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let per_kind = db
            .resolve_web_service_for_file(file_id, "МойСервис")
            .expect("web service resolves through the bootstrapped per-kind substrate");
        let whole = db.merged_visible_configuration(file_id).expect("merged config loads");
        let from_whole =
            whole.find_web_service("МойСервис").expect("merged config has the web service");

        assert_eq!(
            &*per_kind, from_whole,
            "per-kind web-service resolve must equal the merged whole-config lookup"
        );
        assert_eq!(per_kind.namespace(), "main.com");
        assert_eq!(per_kind.operations()[0].procedure_name(), "Операция1");
        assert!(
            db.resolve_web_service_for_file(file_id, "ТолькоРасширение").is_none(),
            "extension-only web services stay invisible until merged whole-config supports them"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolve_integration_service_for_file_matches_merged_visible_configuration() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::ConfigsDatabase;

        fn integration_service_xml(name: &str, handler: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <IntegrationService uuid="c512a1cd-1240-4e46-8bad-8b7b27c5c{handler_pad}">
        <Properties>
            <Name>{name}</Name>
        </Properties>
        <ChildObjects>
            <IntegrationServiceChannel uuid="1ef0581c-b1d8-4115-87f1-7856f6c06bb6">
                <Properties>
                    <Name>input_normal</Name>
                    <MessageDirection>Receive</MessageDirection>
                    <ReceiveMessageProcessing>{handler}</ReceiveMessageProcessing>
                </Properties>
            </IntegrationServiceChannel>
        </ChildObjects>
    </IntegrationService>
</MetaDataObject>"#,
                handler_pad = if handler.is_empty() { "25a" } else { "26a" }
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_integration_service_parity_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(main_root.join("IntegrationServices/ОбменСообщениями")).unwrap();
        std::fs::create_dir_all(ext_root.join("IntegrationServices/ОбменСообщениями")).unwrap();
        std::fs::create_dir_all(ext_root.join("IntegrationServices/ТолькоРасширение")).unwrap();
        std::fs::create_dir_all(&ext_root).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            main_root.join("IntegrationServices/ОбменСообщениями.xml"),
            integration_service_xml("ОбменСообщениями", "ОбработатьСообщениеОбычныйПриоритет"),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("IntegrationServices/ОбменСообщениями.xml"),
            integration_service_xml("ОбменСообщениями", "ОбработатьСообщениеРасширение"),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("IntegrationServices/ТолькоРасширение.xml"),
            integration_service_xml("ТолькоРасширение", ""),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        let bsl_path = ext_root.join("IntegrationServiceConsumer.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let per_kind = db
            .resolve_integration_service_for_file(file_id, "ОбменСообщениями")
            .expect("integration service resolves through the bootstrapped per-kind substrate");
        let whole = db.merged_visible_configuration(file_id).expect("merged config loads");
        let from_whole = whole
            .find_integration_service("ОбменСообщениями")
            .expect("merged config has the integration service");

        assert_eq!(
            &*per_kind, from_whole,
            "per-kind integration-service resolve must equal the merged whole-config lookup"
        );
        assert_eq!(
            per_kind.channels()[0].receive_message_processing(),
            "ОбработатьСообщениеОбычныйПриоритет"
        );
        assert!(
            db.resolve_integration_service_for_file(file_id, "ТолькоРасширение").is_none(),
            "extension-only integration services stay invisible until merged whole-config supports them"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn service_module_file_lookup_matches_path_side_path_for_http_web_integration() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};

        let root = std::env::temp_dir().join(format!(
            "bsl_service_module_file_{}_{}",
            std::process::id(),
            line!()
        ));
        let cf = root.join("src/cf");
        std::fs::create_dir_all(cf.join("Configuration.xml").parent().unwrap()).unwrap();
        std::fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();

        let write_service = |plural: &str, name: &str, body: &str| {
            std::fs::create_dir_all(cf.join(format!("{plural}/{name}/Ext"))).unwrap();
            std::fs::write(cf.join(format!("{plural}/{name}.xml")), body).unwrap();
            std::fs::write(
                cf.join(format!("{plural}/{name}/Ext/Module.bsl")),
                "Процедура Т() КонецПроцедуры",
            )
            .unwrap();
        };
        write_service(
            "HTTPServices",
            "HTTPСервис1",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <HTTPService uuid="4797cd39-952d-4e4d-9685-014e4d5a8e25">
        <Properties><Name>HTTPСервис1</Name><RootURL>http</RootURL></Properties>
    </HTTPService>
</MetaDataObject>"#,
        );
        write_service(
            "WebServices",
            "WebСервис1",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <WebService uuid="0b4a4c9c-76e9-455c-9471-249051a8301d">
        <Properties><Name>WebСервис1</Name><Namespace>test.com</Namespace></Properties>
    </WebService>
</MetaDataObject>"#,
        );
        write_service(
            "IntegrationServices",
            "ОбменСообщениями",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <IntegrationService uuid="c512a1cd-1240-4e46-8bad-8b7b27c5c25a">
        <Properties><Name>ОбменСообщениями</Name></Properties>
    </IntegrationService>
</MetaDataObject>"#,
        );

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        let mut intern_module = |rel: &str| -> vfs::FileId {
            let path = cf.join(rel);
            let vp = vfs::VfsPath::new(path.to_string_lossy().as_ref());
            let fid = state.vfs.write().alloc_file_id(vp.clone());
            let db = state.analysis_host.raw_database_mut();
            let sr = db.source_root_input(BSL_SOURCE_ROOT).root(db);
            let mut fs = sr.file_set().clone();
            fs.insert(fid, vp);
            db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(fs));
            db.set_file_source_root(fid, BSL_SOURCE_ROOT);
            db.set_file_text(fid, "Процедура Т() КонецПроцедуры");
            fid
        };
        let http_module = intern_module("HTTPServices/HTTPСервис1/Ext/Module.bsl");
        let web_module = intern_module("WebServices/WebСервис1/Ext/Module.bsl");
        let isvc_module = intern_module("IntegrationServices/ОбменСообщениями/Ext/Module.bsl");

        state.analysis_host.raw_database_mut().set_all_config_paths(vec![(None, cf.clone())]);
        state.bootstrap_metadata_substrate();

        let db = state.analysis_host.raw_database();
        let http = db
            .http_service_for_file_id(http_module)
            .expect("HTTPServices/<Name>/Ext/Module.bsl reverse-resolves to its HTTP service");
        assert_eq!(http.name(), "HTTPСервис1");
        let web = db
            .web_service_for_file_id(web_module)
            .expect("WebServices/<Name>/Ext/Module.bsl reverse-resolves to its web service");
        assert_eq!(web.name(), "WebСервис1");
        let isvc = db.integration_service_for_file_id(isvc_module).expect(
            "IntegrationServices/<Name>/Ext/Module.bsl reverse-resolves to its integration service",
        );
        assert_eq!(isvc.name(), "ОбменСообщениями");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn http_service_xml_edit_re_parses_only_the_edited_service() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};

        fn http_service_xml(name: &str, root_url: &str, handler: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <HTTPService uuid="4797cd39-952d-4e4d-9685-014e4d5a8e25">
        <Properties>
            <Name>{name}</Name>
            <RootURL>{root_url}</RootURL>
        </Properties>
        <ChildObjects>
            <URLTemplate uuid="7124b2c7-d38e-40b9-a934-e6eb9de99340">
                <Properties><Name>URLTemplate1</Name><Template>/storage</Template></Properties>
                <ChildObjects>
                    <Method uuid="605f52a9-e95b-4900-9e41-449d7da01348">
                        <Properties>
                            <Name>GET</Name>
                            <HTTPMethod>GET</HTTPMethod>
                            <Handler>{handler}</Handler>
                        </Properties>
                    </Method>
                </ChildObjects>
            </URLTemplate>
        </ChildObjects>
    </HTTPService>
</MetaDataObject>"#
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_http_service_refresh_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(&main_root).unwrap();
        std::fs::create_dir_all(&ext_root).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let before_path = main_root.join("HTTPServices/СервисА.xml");
        let after_path = main_root.join("HTTPServices/СервисБ.xml");
        std::fs::create_dir_all(main_root.join("HTTPServices")).unwrap();
        std::fs::write(&before_path, http_service_xml("СервисА", "a", "HandlerA")).unwrap();
        std::fs::write(&after_path, http_service_xml("СервисБ", "b", "HandlerB")).unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        let bsl_path = ext_root.join("HTTPServiceConsumer.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let a_before = db
            .resolve_http_service_for_file(file_id, "СервисА")
            .expect("СервисА resolves before the edit");
        let b_before = db
            .resolve_http_service_for_file(file_id, "СервисБ")
            .expect("СервисБ resolves before the edit");
        assert_eq!(a_before.url_templates()[0].methods()[0].handler(), "HandlerA");

        std::fs::write(&after_path, http_service_xml("СервисБ", "b", "HandlerBReparse")).unwrap();
        assert!(state.refresh_metadata_substrate(std::slice::from_ref(&after_path)));

        let b_after = state
            .analysis_host
            .raw_database()
            .resolve_http_service_for_file(file_id, "СервисБ")
            .expect("СервисБ re-resolves after the edit");
        assert_eq!(
            b_after.url_templates()[0].methods()[0].handler(),
            "HandlerBReparse",
            "edited HTTP service must re-parse through per-service granularity"
        );
        assert!(!Arc::ptr_eq(&b_before, &b_after), "СервисБ re-parses after its XML changed");

        let a_after = state
            .analysis_host
            .raw_database()
            .resolve_http_service_for_file(file_id, "СервисА")
            .expect("СервисА still resolves after the sibling edit");
        assert!(
            Arc::ptr_eq(&a_before, &a_after),
            "a content edit to СервисБ must not re-resolve the sibling СервисА"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn module_metadata_for_http_service_module_survives_sibling_http_service_edit() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::{DefDatabase, ModuleId};

        fn http_service_xml(name: &str, root_url: &str, handler: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <HTTPService uuid="4797cd39-952d-4e4d-9685-014e4d5a8e25">
        <Properties>
            <Name>{name}</Name>
            <RootURL>{root_url}</RootURL>
        </Properties>
        <ChildObjects>
            <URLTemplate uuid="7124b2c7-d38e-40b9-a934-e6eb9de99340">
                <Properties><Name>URLTemplate1</Name><Template>/storage</Template></Properties>
                <ChildObjects>
                    <Method uuid="605f52a9-e95b-4900-9e41-449d7da01348">
                        <Properties>
                            <Name>GET</Name>
                            <HTTPMethod>GET</HTTPMethod>
                            <Handler>{handler}</Handler>
                        </Properties>
                    </Method>
                </ChildObjects>
            </URLTemplate>
        </ChildObjects>
    </HTTPService>
</MetaDataObject>"#
            )
        }

        let root = tempfile::tempdir().expect("tempdir");
        let main_root = root.path().join("src/cf");
        let services_dir = main_root.join("HTTPServices");
        let service_a_module_path = services_dir.join("СервисА/Ext/Module.bsl");
        std::fs::create_dir_all(service_a_module_path.parent().unwrap()).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let service_a_xml = services_dir.join("СервисА.xml");
        let service_b_xml = services_dir.join("СервисБ.xml");
        std::fs::write(&service_a_xml, http_service_xml("СервисА", "service-a", "HandlerA"))
            .unwrap();
        std::fs::write(&service_b_xml, http_service_xml("СервисБ", "service-b", "HandlerB"))
            .unwrap();
        std::fs::write(&service_a_module_path, "Процедура HandlerA() КонецПроцедуры").unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        let service_a_vfs_path =
            vfs::VfsPath::new(service_a_module_path.to_string_lossy().as_ref());
        let service_a_module_file = state.vfs.write().alloc_file_id(service_a_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(service_a_module_file, service_a_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(service_a_module_file, BSL_SOURCE_ROOT);
        db.set_file_text(service_a_module_file, "Процедура HandlerA() КонецПроцедуры");
        db.set_all_config_paths(vec![(None, main_root.clone())]);
        state.bootstrap_metadata_substrate();

        let service_a_module_id = ModuleId::new(service_a_module_file);
        let before_http_service = {
            let metadata = state.analysis_host.raw_database().module_metadata(service_a_module_id);
            assert_eq!(metadata.module_type, bsl_metadata::ModuleType::HTTPServiceModule);
            let http_service = metadata
                .http_service
                .clone()
                .expect("HTTP service module metadata resolves before the sibling edit");
            assert_eq!(http_service.name(), "СервисА");
            assert_eq!(http_service.url_templates()[0].methods()[0].handler(), "HandlerA");
            http_service
        };

        std::fs::write(
            &service_b_xml,
            http_service_xml("СервисБ", "service-b-edited", "HandlerBEdited"),
        )
        .unwrap();
        state.analysis_host.raw_database_mut().bump_config_for_path(&service_b_xml);
        assert!(state.refresh_metadata_substrate(std::slice::from_ref(&service_b_xml)));

        let after_http_service = {
            let metadata = state.analysis_host.raw_database().module_metadata(service_a_module_id);
            assert_eq!(metadata.module_type, bsl_metadata::ModuleType::HTTPServiceModule);
            let http_service = metadata
                .http_service
                .clone()
                .expect("HTTP service module metadata still resolves after the sibling edit");
            assert_eq!(http_service.name(), "СервисА");
            assert_eq!(http_service.url_templates()[0].methods()[0].handler(), "HandlerA");
            http_service
        };

        assert!(
            Arc::ptr_eq(&before_http_service, &after_http_service),
            "editing СервисБ must not re-clone module metadata for sibling HTTP service СервисА"
        );
    }

    #[test]
    fn module_metadata_for_web_service_module_survives_sibling_web_service_edit() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::{DefDatabase, ModuleId};

        fn web_service_xml(name: &str, namespace: &str, procedure: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <WebService uuid="0b4a4c9c-76e9-455c-9471-249051a8301d">
        <Properties>
            <Name>{name}</Name>
            <Namespace>{namespace}</Namespace>
        </Properties>
        <ChildObjects>
            <Operation uuid="bc99d837-aee6-40ee-8940-3a81dddf477c">
                <Properties>
                    <Name>Операция1</Name>
                    <ProcedureName>{procedure}</ProcedureName>
                </Properties>
                <ChildObjects/>
            </Operation>
        </ChildObjects>
    </WebService>
</MetaDataObject>"#
            )
        }

        let root = tempfile::tempdir().expect("tempdir");
        let main_root = root.path().join("src/cf");
        let services_dir = main_root.join("WebServices");
        let service_a_module_path = services_dir.join("СервисА/Ext/Module.bsl");
        std::fs::create_dir_all(service_a_module_path.parent().unwrap()).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let service_a_xml = services_dir.join("СервисА.xml");
        let service_b_xml = services_dir.join("СервисБ.xml");
        std::fs::write(&service_a_xml, web_service_xml("СервисА", "a.example", "ProcedureA"))
            .unwrap();
        std::fs::write(&service_b_xml, web_service_xml("СервисБ", "b.example", "ProcedureB"))
            .unwrap();
        std::fs::write(&service_a_module_path, "Процедура ProcedureA() КонецПроцедуры").unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();

        let service_a_vfs_path =
            vfs::VfsPath::new(service_a_module_path.to_string_lossy().as_ref());
        let service_a_module_file = state.vfs.write().alloc_file_id(service_a_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(service_a_module_file, service_a_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(service_a_module_file, BSL_SOURCE_ROOT);
        db.set_file_text(service_a_module_file, "Процедура ProcedureA() КонецПроцедуры");
        db.set_all_config_paths(vec![(None, main_root.clone())]);
        state.bootstrap_metadata_substrate();

        let service_a_module_id = ModuleId::new(service_a_module_file);
        let before_web_service = {
            let metadata = state.analysis_host.raw_database().module_metadata(service_a_module_id);
            assert_eq!(metadata.module_type, bsl_metadata::ModuleType::WebServiceModule);
            let web_service = metadata
                .web_service
                .clone()
                .expect("web service module metadata resolves before the sibling edit");
            assert_eq!(web_service.name(), "СервисА");
            assert_eq!(web_service.operations()[0].procedure_name(), "ProcedureA");
            web_service
        };

        std::fs::write(
            &service_b_xml,
            web_service_xml("СервисБ", "b-edited.example", "ProcedureBEdited"),
        )
        .unwrap();
        state.analysis_host.raw_database_mut().bump_config_for_path(&service_b_xml);
        assert!(state.refresh_metadata_substrate(std::slice::from_ref(&service_b_xml)));

        let after_web_service = {
            let metadata = state.analysis_host.raw_database().module_metadata(service_a_module_id);
            assert_eq!(metadata.module_type, bsl_metadata::ModuleType::WebServiceModule);
            let web_service = metadata
                .web_service
                .clone()
                .expect("web service module metadata still resolves after the sibling edit");
            assert_eq!(web_service.name(), "СервисА");
            assert_eq!(web_service.operations()[0].procedure_name(), "ProcedureA");
            web_service
        };

        assert!(
            Arc::ptr_eq(&before_web_service, &after_web_service),
            "editing СервисБ must not re-clone module metadata for sibling web service СервисА"
        );
    }

    /// Wave 2e: `resolve_subsystem_for_file` must match the merged whole-config
    /// lookup for a base + own-extension overlay, including a same-name merge
    /// (base content + extension-added content land in one subsystem). An
    /// extension-only subsystem is preserved by `merge_extension_overlay` (added
    /// when no base namesake exists), so it must resolve and match the merged
    /// whole-config entry too. Red until `resolve_subsystem_for_file` exists.
    #[test]
    fn resolve_subsystem_for_file_matches_merged_visible_configuration() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use hir::ConfigsDatabase;

        fn subsystem_xml(name: &str, content: &[&str]) -> String {
            let items = content
                .iter()
                .map(|c| format!("        <xr:Item xsi:type=\"xr:MDObjectRef\">{c}</xr:Item>"))
                .collect::<Vec<_>>()
                .join("\n");
            let items_block =
                if items.is_empty() { String::new() } else { format!("\n{items}\n            ") };
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <Subsystem uuid="00000000-0000-0000-0000-000000000094">
        <Properties>
            <Name>{name}</Name>
            <Content>{items_block}</Content>
        </Properties>
    </Subsystem>
</MetaDataObject>"#
            )
        }

        let root = std::env::temp_dir().join(format!(
            "bsl_subsystem_parity_{}_{}",
            std::process::id(),
            line!()
        ));
        let main_root = root.join("src/cf");
        let ext_root = root.join("src/cfe/X");
        std::fs::create_dir_all(main_root.join("Subsystems")).unwrap();
        std::fs::create_dir_all(ext_root.join("Subsystems")).unwrap();
        std::fs::write(main_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(ext_root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            main_root.join("Subsystems/МояПодсистема.xml"),
            subsystem_xml("МояПодсистема", &["Catalog.Товары"]),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("Subsystems/МояПодсистема.xml"),
            subsystem_xml("МояПодсистема", &["Catalog.Услуги"]),
        )
        .unwrap();
        std::fs::write(
            ext_root.join("Subsystems/ТолькоРасширение.xml"),
            subsystem_xml("ТолькоРасширение", &["Catalog.Услуги"]),
        )
        .unwrap();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);
        state.bootstrap_metadata_substrate();

        let bsl_path = ext_root.join("SubsystemConsumer.bsl");
        let bsl_vfs_path = vfs::VfsPath::new(bsl_path.to_string_lossy().as_ref());
        let file_id = state.vfs.write().alloc_file_id(bsl_vfs_path.clone());
        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_id, bsl_vfs_path);
        let db = state.analysis_host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
        db.set_file_text(file_id, "Процедура Т() КонецПроцедуры");

        let db = state.analysis_host.raw_database();
        let per_kind = db
            .resolve_subsystem_for_file(file_id, "МояПодсистема")
            .expect("per-kind resolve finds the subsystem visible to the file");
        let whole = db.merged_visible_configuration(file_id).expect("merged config loads");
        let from_whole = whole
            .subsystems()
            .iter()
            .find(|s| s.name() == "МояПодсистема")
            .expect("merged config has the subsystem");

        assert_eq!(
            &*per_kind, from_whole,
            "per-kind subsystem resolve must equal the merged whole-config lookup"
        );
        let content_names: Vec<String> =
            per_kind.content().iter().map(|(_, name)| name.to_string()).collect();
        assert!(
            content_names.iter().any(|n| n == "Товары"),
            "base subsystem content survives the overlay merge"
        );
        assert!(
            content_names.iter().any(|n| n == "Услуги"),
            "extension-added content merges into the same-named subsystem"
        );
        let ext_only_per_kind = db
            .resolve_subsystem_for_file(file_id, "ТолькоРасширение")
            .expect("extension-only subsystem is preserved by the overlay and resolves");
        let ext_only_from_whole = whole
            .subsystems()
            .iter()
            .find(|s| s.name() == "ТолькоРасширение")
            .expect("merged config preserves the extension-only subsystem");
        assert_eq!(
            &*ext_only_per_kind, ext_only_from_whole,
            "extension-only subsystem resolve must equal the merged whole-config lookup"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
