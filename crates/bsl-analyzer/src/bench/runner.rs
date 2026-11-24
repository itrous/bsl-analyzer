//! Single-point benchmark execution.
//!
//! One invocation = one booted workspace = one measurement point; process-cold
//! isolation is the orchestrator's job (`scripts/bench/run-matrix.sh` spawns a
//! fresh process per point). Inside the process: boot → resolve target → one
//! timed cold call → K timed warm repeats. The result invariant is enforced on
//! every observation, so a run can never silently measure a no-op.
//!
//! Measurement boundaries follow the plan's boundary table: features whose
//! logic lives in `ide::Analysis` are timed at that API; handler-boundary
//! features (`semantic_tokens_full`, `code_action`, `diagnostics_pull`) are
//! timed at the LSP handler with a pre-built frozen context (context
//! construction is reported separately as `ctx_build_ns` — production pays it
//! per request in dispatch, and Tier B measures that end-to-end).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use base_db::SourceDatabase as _;
use line_index::LineIndex;
use lsp_types::Url;
use rustc_hash::FxHashMap;
use syntax::{TextRange, TextSize};

use crate::bench::manifest::{self, EditPatch, FeatureSpec, Target};
use crate::bench::report::{
    percentile_ns, CallHierarchyIndexReport, EditPhases, FamilyChurn, MemoryReport,
    ParseOutcomeDelta, PointReport, RecomputeReport, MODULES_CAP, REPORT_SCHEMA_VERSION,
};
use crate::frozen_context::{FrozenFilePaths, LatencyRequestContext};
use crate::global_state::GlobalState;
use crate::handlers::{notification, request};
use crate::smoke::{bootstrap_smoke, Budgets, Scenario, SmokeArgs};

pub const DEFAULT_WARM_ITERATIONS: usize = 20;
pub const DEFAULT_BOOT_BUDGET_MS: u64 = 120_000;

#[cfg(test)]
#[path = "unread_module_tests.rs"]
mod unread_module_tests;

// Arms one action for the single instant a test cannot reach from outside: after
// the index is built, before the observation reopens the batches. Every other
// instant is reachable by calling `boot`, `resolve_target` and `execute_once` in
// sequence, so this is the only stage the seam offers.
//
// One-shot by construction: the call takes the action out and never puts it back.
// There is no standing registration, so there is nothing to leak, replace, restore
// or carry across a thread — a seam that kept one had to answer all four, and none
// of those questions has a caller here.
//
// An armed action is spent by the very call it was armed for — arming sits
// immediately before that call, so there is no window in which one can be left
// over. Thread-local storage is the second line rather than the first: libtest
// gives each test its own thread even under `--test-threads=1`.
#[cfg(test)]
thread_local! {
    static BETWEEN_INDEX_PASSES: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn arm_between_index_passes(action: impl FnOnce() + 'static) {
    BETWEEN_INDEX_PASSES.with(|slot| slot.set(Some(Box::new(action))));
}

#[cfg(test)]
fn run_between_index_passes_hook() {
    if let Some(action) = BETWEEN_INDEX_PASSES.with(std::cell::Cell::take) {
        action();
    }
}

#[derive(Debug)]
pub enum RunError {
    /// Manifest unreadable / invalid / point id unknown → CLI exit 2.
    Manifest(String),
    /// Invariant or file-hash violation: a no-op or drifted target → CLI exit 3.
    Invariant(String),
    /// Everything else (boot failure, missing file, …) → CLI exit 1.
    Other(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Manifest(msg) => write!(f, "manifest: {msg}"),
            RunError::Invariant(msg) => write!(f, "invariant: {msg}"),
            RunError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Uninstrumented nanosecond latency (cold + warm repeats).
    Latency,
    /// Salsa churn window (`BSL_SALSA_EVENTS=1` must be set before boot;
    /// timings are instrumented and must not be compared with latency runs).
    Recompute,
    /// RSS bracketing with a phase-local sampler and the normative trim
    /// protocol.
    Memory,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RunMode::Latency => "latency",
            RunMode::Recompute => "recompute",
            RunMode::Memory => "memory",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunArgs {
    pub source_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub point_id: String,
    pub mode: RunMode,
    pub warm_iterations: usize,
    pub boot_budget_ms: u64,
    /// Settle time after allocator purge before reading trimmed RSS. The
    /// normative value is 2000 ms (jemalloc returns pages asynchronously);
    /// tests shrink it.
    pub trim_settle_ms: u64,
}

/// A feature invocation's observable outcome: result cardinality plus a
/// canonical, order-normalized digest input. Digest lines are built from
/// `Debug` renderings *after* the timed call returns, so summarization cost
/// never pollutes the latency sample.
pub(crate) struct Observation {
    pub(crate) count: usize,
    pub(crate) digest_input: String,
    call_hierarchy_index: Option<CallHierarchyIndexObservation>,
}

struct CallHierarchyIndexObservation {
    method_count: usize,
    unique_pair_count: usize,
    reverse_target_count: usize,
    estimated_heap_bytes: usize,
    batch_size: usize,
    build_duration_ns: u64,
}

impl Observation {
    fn from_lines(count: usize, mut lines: Vec<String>) -> Self {
        lines.sort();
        Observation { count, digest_input: lines.join("\n"), call_hierarchy_index: None }
    }

    pub(crate) fn digest_hex(&self) -> String {
        manifest::hash_text(&self.digest_input)
    }
}

pub(crate) struct BenchEnv {
    pub(crate) state: GlobalState,
    // Keeps the client-message drain thread alive for the whole measurement.
    _drain_handle: std::thread::JoinHandle<()>,
    pub(crate) boot_ms: u64,
    pub(crate) boot_rss_bytes: Option<u64>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) ctx_build_ns: Option<u64>,
    /// Cached frozen VFS path table for handler contexts: freezing is O(files)
    /// (25k+ on ERP), and paths only change when the runner itself applies an
    /// edit — which resets this cache.
    frozen_paths: Option<FrozenFilePaths>,
}

pub(crate) struct ResolvedTarget {
    pub(crate) file_id: vfs::FileId,
    pub(crate) url: Url,
    pub(crate) text: String,
}

struct CallHierarchyBuildContext {
    source_root_id: base_db::SourceRootId,
    source_root: base_db::SourceRoot,
    modules: Vec<ide::ModuleId>,
    paths: FxHashMap<vfs::FileId, String>,
    file_paths: HashMap<vfs::FileId, PathBuf>,
    configs: Arc<ide::WorkspaceConfigsSnapshot>,
    config_cache: Arc<ide::GraphConfigCache>,
    workspace_root: PathBuf,
    /// Modules no pass could read. Non-empty fails the run: a measurement over a
    /// silently smaller graph is not a measurement of this workspace.
    unread: Arc<std::sync::Mutex<std::collections::BTreeSet<PathBuf>>>,
}

pub(crate) fn boot(source_dir: &Path, boot_budget_ms: u64) -> Result<BenchEnv, RunError> {
    // Canonicalize first: VFS paths and file URLs require absolute paths, and
    // the CLI default source dir is `.`.
    let source_dir = std::fs::canonicalize(source_dir).map_err(|e| {
        RunError::Other(format!("cannot canonicalize workspace root {}: {e}", source_dir.display()))
    })?;
    let args = SmokeArgs {
        source_dir: source_dir.clone(),
        scenarios: Vec::<Scenario>::new(),
        budgets: Budgets { boot_vfs_done_ms: boot_budget_ms, ..Budgets::default() },
        json: false,
    };
    let bootstrap = bootstrap_smoke(&args).map_err(RunError::Other)?;
    Ok(BenchEnv {
        state: bootstrap.state,
        _drain_handle: bootstrap._drain_handle,
        boot_ms: bootstrap.boot.vfs_done_ms,
        boot_rss_bytes: bootstrap.boot.rss_bytes_post_boot,
        workspace_root: source_dir,
        ctx_build_ns: None,
        frozen_paths: None,
    })
}

pub(crate) fn resolve_target(
    env: &BenchEnv,
    relative_path: &str,
    expected_hash: Option<&str>,
) -> Result<ResolvedTarget, RunError> {
    let abs = env.workspace_root.join(relative_path);
    let file_id = lookup_file_id(&env.state, &abs).ok_or_else(|| {
        RunError::Other(format!("target file not in workspace VFS: {}", abs.display()))
    })?;
    let url = Url::from_file_path(&abs)
        .map_err(|_| RunError::Other(format!("cannot build file URL for {}", abs.display())))?;
    let text = env.state.analysis_host.analysis().file_text(file_id);
    if let Some(expected) = expected_hash {
        let actual = manifest::hash_text(&text);
        if !expected.eq_ignore_ascii_case(&actual) {
            return Err(RunError::Invariant(format!(
                "file_hash mismatch for {relative_path}: manifest {expected}, workspace {actual} \
                 — the workspace drifted since discovery"
            )));
        }
    }
    Ok(ResolvedTarget { file_id, url, text })
}

fn lookup_file_id(state: &GlobalState, abs: &Path) -> Option<vfs::FileId> {
    let vfs = state.vfs.read();
    if let Some(id) = vfs.file_id(&vfs::VfsPath::new(abs.to_path_buf())) {
        return Some(id);
    }
    let canonical = std::fs::canonicalize(abs).ok()?;
    vfs.file_id(&vfs::VfsPath::new(canonical))
}

pub fn run_point(args: &RunArgs) -> Result<PointReport, RunError> {
    let manifest = manifest::load(&args.manifest_path).map_err(RunError::Manifest)?;
    let target = manifest
        .targets
        .iter()
        .find(|t| t.id == args.point_id)
        .ok_or_else(|| {
            RunError::Manifest(format!("point `{}` not found in manifest", args.point_id))
        })?
        .clone();

    let mut env = boot(&args.source_dir, args.boot_budget_ms)?;
    let resolved = resolve_target(&env, &target.relative_path, Some(&target.file_hash))?;

    match args.mode {
        RunMode::Latency => match &target.spec {
            FeatureSpec::Edit { patch, edit_kind, followup } => run_edit_point(
                args,
                &mut env,
                &target,
                &resolved,
                std::slice::from_ref(patch),
                *edit_kind,
                followup,
            ),
            FeatureSpec::EditBurst { patches, edit_kind, followup } => {
                run_edit_point(args, &mut env, &target, &resolved, patches, *edit_kind, followup)
            }
            spec => run_plain_point(args, &mut env, &target, &resolved, spec),
        },
        RunMode::Recompute => run_recompute_point(args, &mut env, &target, &resolved),
        RunMode::Memory => run_memory_point(args, &mut env, &target, &resolved),
    }
}

/// Mode B: open a fresh salsa-event window around exactly the observed work.
/// Plain specs profile the cold execution (what a cold feature recomputes);
/// edit specs profile the post-`didChange` request (the incremental churn).
/// The window report is read before any further revision bump, so every key
/// decodes in the revision that produced it.
fn run_recompute_point(
    args: &RunArgs,
    env: &mut BenchEnv,
    target: &Target,
    resolved: &ResolvedTarget,
) -> Result<PointReport, RunError> {
    if !env.state.analysis_host.raw_database().salsa_events_reset() {
        return Err(RunError::Other(
            "recompute mode needs salsa event counters: set BSL_SALSA_EVENTS=1 before boot \
             (the CLI does this automatically for --mode recompute)"
                .to_string(),
        ));
    }

    match &target.spec {
        FeatureSpec::Edit { .. } | FeatureSpec::EditBurst { .. } => {
            let (patches, edit_kind, followup) = edit_parts(&target.spec);
            open_document(env, resolved)?;
            ensure_overlay(env, resolved, followup)?;
            // Settle to a steady warm state so the window sees only
            // edit-caused churn, not first-request cache fills.
            for _ in 0..3 {
                let _ = execute_once(env, resolved, followup)?;
            }

            let PreparedEdits { changes, post } = prepare_edits(env, resolved, patches)?;
            env.state.analysis_host.raw_database().salsa_events_reset();
            let window_start = Instant::now();
            send_edits(env, changes)?;
            let (_, obs) = execute_once(env, &post, followup)?;
            let window_ns = window_start.elapsed().as_nanos() as u64;
            verify_applied(env, &post)?;

            let recompute = collect_recompute(env)?;
            // Only the whole-window duration is meaningful here; the
            // per-phase latency split belongs to mode A.
            let phases = EditPhases {
                edit_kind: edit_kind.as_str().to_string(),
                warm_before_p50_ns: None,
                edit_apply_ns: None,
                after_edit_ns: window_ns,
                burst_len: burst_len(&target.spec),
                parse_outcome: None,
                slab_verify: None,
            };
            finish_mode_report(
                args,
                env,
                target,
                &obs,
                window_ns,
                Some(phases),
                Some(recompute),
                None,
            )
        }
        spec => {
            // First-request-after-boot profile: boot may already have memoized
            // parts of the pipeline (dependency preload), and that is exactly
            // what a real first request sees. Reuse shows up as `validate`
            // counts in `families`; `distinct_keys`/`modules` cover only what
            // actually re-executed. A sparse key set therefore means "mostly
            // served from boot-warm memos", not a broken window.
            ensure_overlay(env, resolved, spec)?;
            env.state.analysis_host.raw_database().salsa_events_reset();
            let (ns, obs) = execute_once(env, resolved, spec)?;
            let recompute = collect_recompute(env)?;
            finish_mode_report(args, env, target, &obs, ns, None, Some(recompute), None)
        }
    }
}

fn collect_recompute(env: &BenchEnv) -> Result<RecomputeReport, RunError> {
    let db = env.state.analysis_host.raw_database();
    // Counter reads come first: key-name resolution below touches only salsa
    // inputs today, but if it ever grows a tracked-query read, taking the
    // per-family/global snapshots beforehand keeps them clean by construction.
    let rows = db
        .salsa_event_report()
        .ok_or_else(|| RunError::Other("salsa event report unavailable".to_string()))?;
    let global = db
        .salsa_event_global()
        .ok_or_else(|| RunError::Other("salsa global counters unavailable".to_string()))?;
    let window = db
        .salsa_key_event_window()
        .ok_or_else(|| RunError::Other("salsa event window unavailable".to_string()))?;
    // The report keeps the window as counts; the keys themselves are the
    // thing to look at when a family executes where it should validate.
    if let Ok(path) = std::env::var("BSL_BENCH_KEY_DUMP") {
        let mut dump = String::new();
        for row in &window.rows {
            use std::fmt::Write as _;
            let _ = writeln!(dump, "{}\t{}\t{}", row.execute, row.discard_stale, row.name);
        }
        std::fs::write(&path, dump)
            .map_err(|e| RunError::Other(format!("key dump {path}: {e}")))?;
    }

    let families: Vec<FamilyChurn> = rows
        .into_iter()
        .filter(|r| r.execute + r.validate + r.did_discard + r.discard_stale + r.intern_new > 0)
        .map(|r| FamilyChurn {
            name: r.name,
            execute: r.execute,
            validate: r.validate,
            did_discard: r.did_discard,
            discard_stale: r.discard_stale,
            intern_new: r.intern_new,
            intern_reuse: r.intern_reuse,
            intern_validate: r.intern_validate,
            block_on: r.block_on,
        })
        .collect();

    // Key names render as `query(<path>[#m<local>])`; the path between the
    // parens is the module attribution. Fallback-rendered keys (no decodable
    // path) are simply not module-attributable and are skipped here — they
    // still count in `distinct_keys`.
    let mut modules: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for row in &window.rows {
        if let Some(open) = row.name.find('(') {
            if let Some(close) = row.name.rfind(')') {
                if close > open + 1 {
                    let inner = &row.name[open + 1..close];
                    // Fallback keys render as `name(Id(n))` today; the digit
                    // guard also survives a Debug-format change to `name(n)`.
                    let is_fallback =
                        inner.starts_with("Id(") || inner.chars().all(|c| c.is_ascii_digit());
                    if !is_fallback && !inner.is_empty() {
                        let path = inner.split('#').next().unwrap_or(inner);
                        modules.insert(path.to_string());
                    }
                }
            }
        }
    }
    let distinct_modules = modules.len();
    let mut modules: Vec<String> = modules.into_iter().collect();
    let modules_truncated = modules.len() > MODULES_CAP;
    modules.truncate(MODULES_CAP);

    Ok(RecomputeReport {
        families,
        distinct_keys: window.distinct_keys,
        distinct_modules,
        modules,
        modules_truncated,
        check_cancellation: global.check_cancellation,
        set_cancellation: global.set_cancellation,
        discard_accumulated: global.discard_accumulated,
    })
}

/// Mode C: bracket one execution with RSS readings — before, phase-local peak
/// (in-process sampler), after — then the normative trim protocol.
fn run_memory_point(
    args: &RunArgs,
    env: &mut BenchEnv,
    target: &Target,
    resolved: &ResolvedTarget,
) -> Result<PointReport, RunError> {
    if matches!(target.spec, FeatureSpec::CallHierarchyIndexBuild { .. }) {
        return run_call_hierarchy_index_memory_point(args, env, target, resolved);
    }

    let rss_before = read_rss_checked()?;

    // The sampler is stopped before any error propagates — a leaked sampler
    // thread would spin every 5 ms until process exit.
    let (result, (phase_peak, sample_count)) = match &target.spec {
        FeatureSpec::Edit { .. } | FeatureSpec::EditBurst { .. } => {
            let (patches, _, followup) = edit_parts(&target.spec);
            open_document(env, resolved)?;
            ensure_overlay(env, resolved, followup)?;
            let _ = execute_once(env, resolved, followup)?;
            let PreparedEdits { changes, post } = prepare_edits(env, resolved, patches)?;
            let sampler = RssSampler::start();
            let t = Instant::now();
            let result = send_edits(env, changes).and_then(|_| execute_once(env, &post, followup));
            let ns = t.elapsed().as_nanos() as u64;
            let sampled = sampler.stop();
            verify_applied(env, &post)?;
            (result.map(|(_, obs)| (ns, obs)), sampled)
        }
        spec => {
            ensure_overlay(env, resolved, spec)?;
            let sampler = RssSampler::start();
            let result = execute_once(env, resolved, spec);
            (result, sampler.stop())
        }
    };
    let (ns, obs) = result?;
    let rss_after = read_rss_checked()?;
    let ingredient_counts: Vec<(String, usize)> =
        crate::mem_report::salsa_memory_rows(env.state.analysis_host.raw_database())
            .into_iter()
            .take(20)
            .map(|(name, count, ..)| (name.to_string(), count))
            .collect();

    let rss_after_trim = trim_and_settle(env, false, args.trim_settle_ms)?;
    let rss_after_deep_trim = trim_and_settle(env, true, args.trim_settle_ms)?;
    let vm_hwm = crate::mem_report::process_peak_rss_bytes();

    let memory = MemoryReport {
        rss_before_bytes: rss_before,
        phase_peak_bytes: phase_peak,
        sample_count,
        peak_is_lower_bound: sample_count < 3,
        rss_after_bytes: rss_after,
        rss_after_trim_bytes: rss_after_trim,
        rss_after_deep_trim_bytes: rss_after_deep_trim,
        vm_hwm_bytes: vm_hwm,
        ingredient_counts,
    };
    finish_mode_report(args, env, target, &obs, ns, None, None, Some(memory))
}

fn run_call_hierarchy_index_memory_point(
    args: &RunArgs,
    env: &mut BenchEnv,
    target: &Target,
    resolved: &ResolvedTarget,
) -> Result<PointReport, RunError> {
    let boot_rss_bytes = env
        .boot_rss_bytes
        .ok_or_else(|| RunError::Other("cannot read the boot resident set size".to_string()))?;
    let pre_build_rss_bytes = read_rss_checked()?;

    let sampler = RssSampler::start();
    let result = execute_once(env, resolved, &target.spec);
    let (phase_peak, sample_count) = sampler.stop();
    let (ns, observation) = result?;
    let post_build_rss_bytes = read_rss_checked()?;
    let ingredient_counts: Vec<(String, usize)> =
        crate::mem_report::salsa_memory_rows(env.state.analysis_host.raw_database())
            .into_iter()
            .take(20)
            .map(|(name, count, ..)| (name.to_string(), count))
            .collect();

    let post_trim_rss_bytes = trim_and_settle(env, false, args.trim_settle_ms)?;
    let rss_after_deep_trim_bytes = trim_and_settle(env, true, args.trim_settle_ms)?;
    let vm_hwm_bytes = crate::mem_report::process_peak_rss_bytes();

    let index = observation.call_hierarchy_index.as_ref().ok_or_else(|| {
        RunError::Other("call hierarchy index benchmark did not return build metrics".to_string())
    })?;
    let call_hierarchy_index = CallHierarchyIndexReport {
        method_count: index.method_count,
        unique_pair_count: index.unique_pair_count,
        reverse_target_count: index.reverse_target_count,
        estimated_heap_bytes: index.estimated_heap_bytes,
        batch_size: index.batch_size,
        build_duration_ns: index.build_duration_ns,
        boot_rss_bytes,
        pre_build_rss_bytes,
        post_build_rss_bytes,
        post_trim_rss_bytes,
        vm_hwm_bytes,
        digest: observation.digest_hex(),
    };
    let memory = MemoryReport {
        rss_before_bytes: pre_build_rss_bytes,
        phase_peak_bytes: phase_peak,
        sample_count,
        peak_is_lower_bound: sample_count < 3,
        rss_after_bytes: post_build_rss_bytes,
        rss_after_trim_bytes: post_trim_rss_bytes,
        rss_after_deep_trim_bytes,
        vm_hwm_bytes,
        ingredient_counts,
    };
    let mut report =
        finish_mode_report(args, env, target, &observation, ns, None, None, Some(memory))?;
    report.call_hierarchy_index = Some(call_hierarchy_index);
    Ok(report)
}

fn read_rss_checked() -> Result<u64, RunError> {
    crate::smoke::read_rss_bytes()
        .ok_or_else(|| RunError::Other("cannot read the resident set size".to_string()))
}

/// The normative trim protocol: close overlays, trim salsa (`enforce_lru` or
/// `enforce_lru_deep`), release the shared green-node arena on the main and
/// every rayon worker thread, purge the allocator, then let the async page
/// return settle before reading RSS.
fn trim_and_settle(env: &mut BenchEnv, deep: bool, settle_ms: u64) -> Result<u64, RunError> {
    for uri in env.state.mem_docs.uris() {
        env.state.mem_docs.remove(&uri);
    }
    let db = env.state.analysis_host.raw_database_mut();
    if deep {
        ide::sweep_lru_deep(db);
    } else {
        db.enforce_lru();
    }
    syntax::clear_shared_node_cache();
    rayon::broadcast(|_| syntax::clear_shared_node_cache());
    profile::purge_allocator();
    std::thread::sleep(std::time::Duration::from_millis(settle_ms));
    read_rss_checked()
}

/// In-process resident-size sampler: a dedicated thread polling the kernel
/// every 5 ms while the measured phase runs. 1 Hz external sampling misses
/// short-lived peaks; even 5 ms only bounds them from below, which the report
/// makes explicit via `sample_count` / `peak_is_lower_bound`.
pub(crate) struct RssSampler {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<(u64, u64)>,
}

impl RssSampler {
    fn start() -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut max = 0u64;
            let mut count = 0u64;
            loop {
                if let Some(rss) = crate::smoke::read_rss_bytes() {
                    max = max.max(rss);
                    count += 1;
                }
                if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            (max, count)
        });
        RssSampler { stop, handle }
    }

    fn stop(self) -> (u64, u64) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.handle.join().unwrap_or((0, 0))
    }
}

fn run_plain_point(
    args: &RunArgs,
    env: &mut BenchEnv,
    target: &Target,
    resolved: &ResolvedTarget,
    spec: &FeatureSpec,
) -> Result<PointReport, RunError> {
    ensure_overlay(env, resolved, spec)?;

    let (cold_ns, cold_obs) = execute_once(env, resolved, spec)?;
    let mut warm_ns = Vec::with_capacity(args.warm_iterations);
    for _ in 0..args.warm_iterations {
        let (ns, obs) = execute_once(env, resolved, spec)?;
        if obs.count != cold_obs.count {
            return Err(RunError::Invariant(format!(
                "unstable result: cold count {} vs warm count {}",
                cold_obs.count, obs.count
            )));
        }
        warm_ns.push(ns);
    }

    finish_report(args, env, target, &cold_obs, cold_ns, warm_ns, None)
}

/// The patches, kind and followup of an `Edit` / `EditBurst` spec. Panics on
/// any other spec: callers dispatch on the variant first.
fn edit_parts(spec: &FeatureSpec) -> (&[EditPatch], manifest::EditKind, &FeatureSpec) {
    match spec {
        FeatureSpec::Edit { patch, edit_kind, followup } => {
            (std::slice::from_ref(patch), *edit_kind, followup)
        }
        FeatureSpec::EditBurst { patches, edit_kind, followup } => (patches, *edit_kind, followup),
        other => unreachable!("edit_parts on a non-edit spec {}", other.feature_name()),
    }
}

fn burst_len(spec: &FeatureSpec) -> Option<usize> {
    match spec {
        FeatureSpec::EditBurst { patches, .. } => Some(patches.len()),
        _ => None,
    }
}

/// The text after `patch`, or a manifest error when the range does not name a
/// substring of `text`.
fn apply_patch(text: &str, patch: &EditPatch) -> Result<String, RunError> {
    let (start, end) = (patch.range.start as usize, patch.range.end as usize);
    if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return Err(RunError::Manifest(format!(
            "patch range {start}..{end} out of bounds or splits a UTF-8 char (file len {})",
            text.len()
        )));
    }
    let mut patched = String::with_capacity(text.len() + patch.new_text.len());
    patched.push_str(&text[..start]);
    patched.push_str(&patch.new_text);
    patched.push_str(&text[end..]);
    Ok(patched)
}

/// Build the `didChange` an editor would send for `patch`: a ranged content
/// change in the server's negotiated position encoding, not a full-text
/// replacement. A full-text change would take `mem_docs` down its
/// replace-everything branch, so the stand would never observe the ranged
/// path a real keystroke exercises.
fn ranged_change(
    env: &BenchEnv,
    text: &str,
    url: &Url,
    patch: &EditPatch,
    version: i32,
) -> Result<lsp_types::DidChangeTextDocumentParams, RunError> {
    let line_index = LineIndex::new(text);
    let range = TextRange::new(TextSize::from(patch.range.start), TextSize::from(patch.range.end));
    let lsp_range = crate::lsp::to_proto::range_with_encoding(
        &line_index,
        text,
        range,
        env.state.position_encoding,
    )
    .ok_or_else(|| {
        RunError::Manifest(format!(
            "patch range {}..{} has no LSP position in the document",
            patch.range.start, patch.range.end
        ))
    })?;
    Ok(lsp_types::DidChangeTextDocumentParams {
        text_document: lsp_types::VersionedTextDocumentIdentifier { uri: url.clone(), version },
        content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
            range: Some(lsp_range),
            range_length: None,
            text: patch.new_text.clone(),
        }],
    })
}

/// The `didChange`s of an edit point, built before any measurement window
/// opens, and the target they leave behind.
///
/// Building them costs the benchmark a text clone, a patched copy and a
/// `LineIndex` per patch — tens of megabytes on a 10 MB module — none of
/// which the server pays for a keystroke. Preparing everything up front keeps
/// that bookkeeping out of every window: the per-`didChange` timer of mode
/// A, the salsa-event window of mode B and the RSS sampler of mode C.
struct PreparedEdits {
    changes: Vec<lsp_types::DidChangeTextDocumentParams>,
    post: ResolvedTarget,
}

fn prepare_edits(
    env: &BenchEnv,
    resolved: &ResolvedTarget,
    patches: &[EditPatch],
) -> Result<PreparedEdits, RunError> {
    let mut text = resolved.text.clone();
    let mut changes = Vec::with_capacity(patches.len());
    for (i, patch) in patches.iter().enumerate() {
        let patched = apply_patch(&text, patch)?;
        let version = 2 + i as i32;
        changes.push(ranged_change(env, &text, &resolved.url, patch, version)?);
        text = patched;
    }
    Ok(PreparedEdits {
        changes,
        post: ResolvedTarget { file_id: resolved.file_id, url: resolved.url.clone(), text },
    })
}

/// Push the prepared `didChange`s through the real handler, one notification
/// per patch, and return the summed handler time. Nothing but the handler
/// runs between a timer's start and stop.
fn send_edits(
    env: &mut BenchEnv,
    changes: Vec<lsp_types::DidChangeTextDocumentParams>,
) -> Result<u64, RunError> {
    let mut apply_ns = 0u64;
    for change_params in changes {
        let apply_start = Instant::now();
        notification::handle_did_change(&mut env.state, change_params)
            .map_err(|e| RunError::Other(format!("didChange failed: {e}")))?;
        apply_ns += apply_start.elapsed().as_nanos() as u64;
        env.frozen_paths = None;
    }
    Ok(apply_ns)
}

/// `handle_did_change` swallows a rejected edit (logs and returns Ok), so a
/// dropped patch would silently re-measure unchanged text — compare the
/// document with the text the patches must have produced. The check also
/// pins the ranged conversion: a wrong column lands a patch elsewhere and the
/// texts differ. It clones the whole document, so it runs after the window
/// closes, never inside it.
fn verify_applied(env: &BenchEnv, post: &ResolvedTarget) -> Result<(), RunError> {
    let applied = env.state.mem_docs.get(&post.url);
    if applied.as_deref() != Some(post.text.as_str()) {
        return Err(RunError::Other(
            "didChange did not apply: document text differs from the patched text".to_string(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, reason = "two call sites; grouping would only rename the args")]
fn run_edit_point(
    args: &RunArgs,
    env: &mut BenchEnv,
    target: &Target,
    resolved: &ResolvedTarget,
    patches: &[EditPatch],
    edit_kind: manifest::EditKind,
    followup: &FeatureSpec,
) -> Result<PointReport, RunError> {
    // Resident overlay for the document; the edits themselves flow through
    // the real didChange path.
    open_document(env, resolved)?;
    ensure_overlay(env, resolved, followup)?;

    // Steady warm baseline of the followup before the edit.
    let _ = execute_once(env, resolved, followup)?;
    let mut warm_before = Vec::with_capacity(args.warm_iterations);
    for _ in 0..args.warm_iterations {
        let (ns, _) = execute_once(env, resolved, followup)?;
        warm_before.push(ns);
    }

    // Followup offsets are validated to precede every patch, so the pre-edit
    // resolved target stays valid; only the text snapshot is refreshed.
    let PreparedEdits { changes, post } = prepare_edits(env, resolved, patches)?;
    let parse_before = env.state.analysis_host.raw_database().parse_stats();
    let slab_verify_before = ide::slab_verify_mismatches();
    let edit_apply_ns = send_edits(env, changes)?;
    verify_applied(env, &post)?;

    let (after_edit_ns, after_obs) = execute_once(env, &post, followup)?;
    let parse_outcome = ParseOutcomeDelta::between(
        parse_before,
        env.state.analysis_host.raw_database().parse_stats(),
    );
    let slab_verify = std::env::var("BSL_SLAB_VERIFY")
        .is_ok_and(|v| v == "1")
        .then(|| ide::slab_verify_mismatches() - slab_verify_before);
    let mut warm_after = Vec::with_capacity(args.warm_iterations);
    for _ in 0..args.warm_iterations {
        let (ns, obs) = execute_once(env, &post, followup)?;
        if obs.count != after_obs.count {
            return Err(RunError::Invariant(format!(
                "unstable post-edit result: after-edit count {} vs warm count {}",
                after_obs.count, obs.count
            )));
        }
        warm_after.push(ns);
    }

    let phases = EditPhases {
        edit_kind: edit_kind.as_str().to_string(),
        warm_before_p50_ns: Some(percentile_ns(&warm_before, 50)),
        edit_apply_ns: Some(edit_apply_ns),
        after_edit_ns,
        burst_len: burst_len(&target.spec),
        parse_outcome: Some(parse_outcome),
        slab_verify,
    };
    finish_report(args, env, target, &after_obs, after_edit_ns, warm_after, Some(phases))
}

#[allow(clippy::too_many_arguments, reason = "flat report assembly; grouping would only rename")]
fn finish_mode_report(
    args: &RunArgs,
    env: &BenchEnv,
    target: &Target,
    observation: &Observation,
    cold_ns: u64,
    edit: Option<EditPhases>,
    recompute: Option<RecomputeReport>,
    memory: Option<MemoryReport>,
) -> Result<PointReport, RunError> {
    build_report(args, env, target, observation, cold_ns, Vec::new(), edit, recompute, memory)
}

fn finish_report(
    args: &RunArgs,
    env: &BenchEnv,
    target: &Target,
    observation: &Observation,
    cold_ns: u64,
    warm_ns: Vec<u64>,
    edit: Option<EditPhases>,
) -> Result<PointReport, RunError> {
    build_report(args, env, target, observation, cold_ns, warm_ns, edit, None, None)
}

#[allow(clippy::too_many_arguments, reason = "flat report assembly; grouping would only rename")]
fn build_report(
    args: &RunArgs,
    env: &BenchEnv,
    target: &Target,
    observation: &Observation,
    cold_ns: u64,
    warm_ns: Vec<u64>,
    edit: Option<EditPhases>,
    recompute: Option<RecomputeReport>,
    memory: Option<MemoryReport>,
) -> Result<PointReport, RunError> {
    let digest = observation.digest_hex();
    let invariant = target.expect.check(observation.count, &digest);
    let report = PointReport {
        schema_version: REPORT_SCHEMA_VERSION,
        point_id: target.id.clone(),
        feature: target.spec.feature_name().to_string(),
        mode: args.mode.as_str().to_string(),
        workspace_root: env.workspace_root.display().to_string(),
        relative_path: target.relative_path.clone(),
        boot_ms: env.boot_ms,
        ctx_build_ns: env.ctx_build_ns,
        cold_ns,
        warm_p50_ns: percentile_ns(&warm_ns, 50),
        warm_p95_ns: percentile_ns(&warm_ns, 95),
        warm_ns,
        observed_count: observation.count,
        digest,
        invariant_ok: invariant.is_ok(),
        invariant_error: invariant.as_ref().err().cloned(),
        edit,
        recompute,
        memory,
        call_hierarchy_index: None,
    };
    match invariant {
        Ok(()) => Ok(report),
        Err(e) => Err(RunError::Invariant(format!(
            "{} [{}]: {e}\nreport: {}",
            report.point_id,
            report.feature,
            serde_json::to_string(&report).unwrap_or_default()
        ))),
    }
}

/// didOpen overlay for handler-boundary features, per the plan's boundary
/// table. Runs the real `didOpen` handler so the document becomes a resident
/// VFS overlay with a matching salsa text input — a raw `mem_docs` insert
/// would leave `file_text` disk-backed and trip the lazy-text revision guard
/// on the first post-edit read. Core `ide::Analysis` features run without an
/// overlay.
pub(crate) fn ensure_overlay(
    env: &mut BenchEnv,
    resolved: &ResolvedTarget,
    spec: &FeatureSpec,
) -> Result<(), RunError> {
    let needs = match spec {
        FeatureSpec::SemanticTokensFull
        | FeatureSpec::CodeAction { .. }
        | FeatureSpec::DiagnosticsPull => true,
        FeatureSpec::Burst { sequence } => sequence.iter().any(|s| {
            matches!(
                s,
                FeatureSpec::SemanticTokensFull
                    | FeatureSpec::CodeAction { .. }
                    | FeatureSpec::DiagnosticsPull
            )
        }),
        _ => false,
    };
    if needs {
        open_document(env, resolved)?;
    }
    Ok(())
}

/// Inert didOpen (idempotent per URL): replicates the resident-overlay steps
/// of `handle_did_open` — mem_docs insert, open-file marking *before*
/// `process_changes`, VFS contents, and the explicit overlay re-pin — while
/// deliberately skipping `preload_dependencies` and `schedule_diagnostics`.
/// The real handler warms exactly the caches the timed call is about to read
/// (dependency preload runs synchronously) and spawns concurrent pool tasks,
/// which would turn the "process-cold" number into a warmed, jittery one. The
/// production didOpen pipeline itself is Tier B's measurement, not Tier A's.
pub(crate) fn open_document(env: &mut BenchEnv, resolved: &ResolvedTarget) -> Result<(), RunError> {
    use base_db::SourceDatabase as _;

    if env.state.mem_docs.get(&resolved.url).is_some() {
        return Ok(());
    }
    let state = &mut env.state;
    state.mem_docs.insert(resolved.url.clone(), resolved.text.clone(), 1);
    state.open_files.insert(resolved.file_id);
    {
        let path = resolved
            .url
            .to_file_path()
            .map_err(|()| RunError::Other(format!("not a file URI: {}", resolved.url)))?;
        let mut vfs = state.vfs.write();
        vfs.set_file_contents(
            vfs::VfsPath::new(path),
            Some(std::sync::Arc::from(resolved.text.as_str())),
        );
    }
    state.process_changes(false);

    // Same re-pin as handle_did_open: a content-identical open produces no
    // change event, leaving the file disk-backed; pin the overlay explicitly.
    if state.analysis_host.raw_database().try_file_text_input(resolved.file_id).is_none() {
        state.analysis_host.request_cancellation();
        let db = state.analysis_host.raw_database_mut();
        ide_host_core::set_file_text_source(
            db,
            resolved.file_id,
            ide_host_core::FileTextSource::Overlay(&resolved.text),
        );
    }
    env.frozen_paths = None;
    Ok(())
}

/// One timed invocation. Only the entrypoint call sits between the `Instant`
/// reads; observation summarization happens afterwards.
pub(crate) fn execute_once(
    env: &mut BenchEnv,
    resolved: &ResolvedTarget,
    spec: &FeatureSpec,
) -> Result<(u64, Observation), RunError> {
    let file_id = resolved.file_id;
    match spec {
        FeatureSpec::Hover { offset } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.hover(file_id, *offset, ide::Locale::default());
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|h| format!("{h:?}")).collect();
            Ok((ns, Observation::from_lines(r.is_some() as usize, lines)))
        }
        FeatureSpec::Completion { offset } => {
            let root = env.state.workspace_root.clone();
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.completions(file_id, *offset, root, ide::Locale::default());
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|c| format!("{c:?}")).collect();
            Ok((ns, Observation::from_lines(r.len(), lines)))
        }
        FeatureSpec::GotoDefinition { offset } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.goto_definition(file_id, *offset);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|n| format!("{n:?}")).collect();
            Ok((ns, Observation::from_lines(r.is_some() as usize, lines)))
        }
        FeatureSpec::TypeDefinition { offset } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.type_definition(file_id, *offset);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|n| format!("{n:?}")).collect();
            Ok((ns, Observation::from_lines(r.is_some() as usize, lines)))
        }
        FeatureSpec::References { offset } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.find_references(file_id, *offset);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|l| format!("{l:?}")).collect();
            Ok((ns, Observation::from_lines(r.len(), lines)))
        }
        FeatureSpec::Rename { offset, new_name } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.rename(file_id, *offset, new_name);
            let ns = t.elapsed().as_nanos() as u64;
            let obs = match r {
                Ok(locations) => {
                    let lines = locations.iter().map(|l| format!("{l:?}")).collect();
                    Observation::from_lines(locations.len(), lines)
                }
                Err(e) => Observation::from_lines(0, vec![format!("rename error: {e:?}")]),
            };
            Ok((ns, obs))
        }
        FeatureSpec::CallHierarchyPrepare { offset } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.prepare_call_hierarchy(file_id, *offset);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|i| format!("{i:?}")).collect();
            Ok((ns, Observation::from_lines(r.is_some() as usize, lines)))
        }
        FeatureSpec::CallHierarchyIncoming { offset } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let source_root =
                a.database().file_source_root_input(file_id).source_root_id(a.database());
            let r =
                env.state.call_hierarchy_index.current(source_root).and_then(|index| {
                    a.call_hierarchy_incoming_from_index(file_id, *offset, index)
                });
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().flatten().map(|c| format!("{c:?}")).collect();
            Ok((ns, Observation::from_lines(r.as_ref().map_or(0, Vec::len), lines)))
        }
        FeatureSpec::CallHierarchyOutgoing { offset } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.call_hierarchy_outgoing(file_id, *offset);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|c| format!("{c:?}")).collect();
            Ok((ns, Observation::from_lines(r.len(), lines)))
        }
        FeatureSpec::CallHierarchyIndexBuild { batch_size } => {
            if *batch_size == 0 {
                return Err(RunError::Manifest(
                    "call_hierarchy_index_build batch_size must be greater than zero".to_string(),
                ));
            }
            let context = call_hierarchy_build_context(env, resolved)?;
            let mut open_batch =
                |batch: &[ide::ModuleId]| open_call_hierarchy_batch(&context, batch);
            let built = ide::build_call_hierarchy_index(
                ide::CallHierarchyIndexBuildRequest::new(&context.modules, *batch_size),
                &mut open_batch,
            )
            .map_err(|err| RunError::Other(format!("call hierarchy index build failed: {err}")))?;
            let build_duration_ns = built.elapsed.as_nanos() as u64;
            #[cfg(test)]
            run_between_index_passes_hook();
            let observation = call_hierarchy_index_observation(&built, &context, *batch_size)?;
            // Checked AFTER both passes, so a file that became unreadable between them
            // fails the run exactly like one that was unreadable from the start.
            let unread = context.unread.lock().expect("bench unread set is never poisoned");
            if !unread.is_empty() {
                return Err(RunError::Other(format!(
                    "call hierarchy index build read {} module(s) as empty because their bytes \
                     could not be read; the measurement would cover a smaller graph than the \
                     workspace: {}",
                    unread.len(),
                    unread.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                )));
            }
            drop(unread);
            Ok((build_duration_ns, observation))
        }
        FeatureSpec::InlayHints { range } => {
            let text_range = match range {
                Some(r) => TextRange::new(TextSize::from(r.start), TextSize::from(r.end)),
                None => {
                    TextRange::new(TextSize::from(0), TextSize::from(resolved.text.len() as u32))
                }
            };
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.inlay_hints(file_id, text_range);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|h| format!("{h:?}")).collect();
            Ok((ns, Observation::from_lines(r.len(), lines)))
        }
        FeatureSpec::SelectionRange { offsets } => {
            let sizes: Vec<TextSize> = offsets.iter().map(|&o| TextSize::from(o)).collect();
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.selection_ranges(file_id, &sizes);
            let ns = t.elapsed().as_nanos() as u64;
            let count = r.iter().map(|chain| chain.len()).sum();
            let lines = r.iter().map(|chain| format!("{chain:?}")).collect();
            Ok((ns, Observation::from_lines(count, lines)))
        }
        FeatureSpec::DocumentSymbol => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.document_symbols(file_id);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|s| format!("{s:?}")).collect();
            Ok((ns, Observation::from_lines(r.len(), lines)))
        }
        FeatureSpec::FoldingRange => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.folding_ranges(file_id);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|f| format!("{f:?}")).collect();
            Ok((ns, Observation::from_lines(r.len(), lines)))
        }
        FeatureSpec::SignatureHelp { offset } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.signature_help(file_id, *offset);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|s| format!("{s:?}")).collect();
            Ok((ns, Observation::from_lines(r.is_some() as usize, lines)))
        }
        FeatureSpec::WorkspaceSymbol { query } => {
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.workspace_symbols(query);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.candidates.iter().map(|s| format!("{s:?}")).collect();
            Ok((ns, Observation::from_lines(r.candidates.len(), lines)))
        }
        FeatureSpec::DiagnosticsPush => {
            let config = env.state.diagnostics_config.clone();
            let a = env.state.analysis_host.analysis();
            let t = Instant::now();
            let r = a.file_diagnostics_cached(file_id, config);
            let ns = t.elapsed().as_nanos() as u64;
            let lines = r.iter().map(|d| format!("{d:?}")).collect();
            Ok((ns, Observation::from_lines(r.len(), lines)))
        }
        FeatureSpec::SemanticTokensFull => {
            let ctx = build_latency_ctx(env);
            let params = lsp_types::SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: lsp_types::TextDocumentIdentifier { uri: resolved.url.clone() },
            };
            let t = Instant::now();
            let r = request::handle_semantic_tokens_full(&ctx, params)
                .map_err(|e| RunError::Other(format!("semantic tokens handler failed: {e}")))?;
            let ns = t.elapsed().as_nanos() as u64;
            let count = match &r {
                Some(lsp_types::SemanticTokensResult::Tokens(tokens)) => tokens.data.len(),
                Some(lsp_types::SemanticTokensResult::Partial(partial)) => partial.data.len(),
                None => 0,
            };
            let lines = vec![format!("{r:?}")];
            Ok((ns, Observation::from_lines(count, lines)))
        }
        FeatureSpec::CodeAction { range } => {
            let lsp_range = offset_range_to_lsp(&resolved.text, range.start, range.end)
                .ok_or_else(|| {
                    RunError::Manifest(format!(
                        "code_action range {}..{} out of bounds",
                        range.start, range.end
                    ))
                })?;
            let ctx = build_latency_ctx(env);
            let params = lsp_types::CodeActionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: resolved.url.clone() },
                range: lsp_range,
                context: lsp_types::CodeActionContext::default(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let t = Instant::now();
            let r = request::handle_code_action(&ctx, params)
                .map_err(|e| RunError::Other(format!("code action handler failed: {e}")))?;
            let ns = t.elapsed().as_nanos() as u64;
            let actions = r.unwrap_or_default();
            let lines = actions.iter().map(|a| format!("{a:?}")).collect();
            Ok((ns, Observation::from_lines(actions.len(), lines)))
        }
        FeatureSpec::DiagnosticsPull => {
            let ctx = build_latency_ctx(env);
            let params = lsp_types::DocumentDiagnosticParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: resolved.url.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let t = Instant::now();
            let r = request::handle_document_diagnostic(&ctx, params)
                .map_err(|e| RunError::Other(format!("pull diagnostics handler failed: {e}")))?;
            let ns = t.elapsed().as_nanos() as u64;
            let count = pull_report_count(&r);
            let lines = vec![format!("{r:?}")];
            Ok((ns, Observation::from_lines(count, lines)))
        }
        FeatureSpec::Burst { sequence } => {
            let mut total_ns = 0u64;
            let mut count = 0usize;
            let mut lines = Vec::new();
            for inner in sequence {
                let (ns, obs) = execute_once(env, resolved, inner)?;
                total_ns += ns;
                count += obs.count;
                lines.push(format!("{}:{}", inner.feature_name(), obs.digest_input));
            }
            Ok((total_ns, Observation::from_lines(count, lines)))
        }
        FeatureSpec::Edit { .. } | FeatureSpec::EditBurst { .. } => Err(RunError::Other(
            "edit points are executed by run_edit_point, not execute_once".to_string(),
        )),
    }
}

fn call_hierarchy_build_context(
    env: &BenchEnv,
    resolved: &ResolvedTarget,
) -> Result<CallHierarchyBuildContext, RunError> {
    let db = env.state.analysis_host.raw_database();
    let source_root_id = db.file_source_root_input(resolved.file_id).source_root_id(db);
    let source_root = db.source_root_input(source_root_id).root(db).clone();
    let modules: Vec<ide::ModuleId> = source_root.iter().map(ide::ModuleId::new).collect();
    let mut paths = FxHashMap::default();
    let mut file_paths = HashMap::with_capacity(modules.len());

    for module in &modules {
        let path = source_root
            .file_set()
            .path_for_file(&module.file_id)
            .ok_or_else(|| {
                RunError::Other(format!(
                    "call hierarchy index: source-root file {:?} has no disk path",
                    module.file_id
                ))
            })?
            .as_path()
            .to_path_buf();
        paths.insert(module.file_id, path.to_string_lossy().replace('\\', "/"));
        file_paths.insert(module.file_id, path);
    }

    Ok(CallHierarchyBuildContext {
        source_root_id,
        source_root,
        modules,
        paths,
        file_paths,
        configs: db.workspace_configs_snapshot(),
        config_cache: Arc::new(ide::GraphConfigCache::default()),
        workspace_root: env.workspace_root.clone(),
        unread: Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new())),
    })
}

/// Opens one batch and RECORDS the modules whose bytes could not be read, into the
/// shared set on `context`.
///
/// A module registered with empty text lowers to no methods at all, so a measurement
/// taken over it silently reports a smaller, faster graph than the workspace really
/// has — the run would look better precisely because it analysed less. The set is
/// shared rather than returned because both passes open batches (the build closure
/// and the observation below), and a per-pass report would miss whichever pass the
/// file became unreadable in.
fn open_call_hierarchy_batch(
    context: &CallHierarchyBuildContext,
    batch: &[ide::ModuleId],
) -> ide::RootDatabaseImpl {
    let batch_files: Vec<(vfs::FileId, PathBuf)> = batch
        .iter()
        .map(|module| (module.file_id, context.file_paths[&module.file_id].clone()))
        .collect();
    let mut db = ide::RootDatabaseImpl::default();
    db.set_graph_config_cache(Arc::clone(&context.config_cache));
    db.set_source_root(context.source_root_id, context.source_root.clone());
    let unreadable =
        ide_host_core::register_files_disk_backed(&mut db, context.source_root_id, &batch_files);
    if !unreadable.is_empty() {
        let mut seen = context.unread.lock().expect("bench unread set is never poisoned");
        seen.extend(unreadable.into_iter().map(|(path, _err)| path));
    }
    db.set_workspace_configs_snapshot((*context.configs).clone());
    ide::warm_batch_config_roots(&db, &batch_files);
    db
}

fn call_hierarchy_index_observation(
    built: &ide::CallHierarchyIndexBuildResult,
    context: &CallHierarchyBuildContext,
    batch_size: usize,
) -> Result<Observation, RunError> {
    if batch_size == 0 {
        return Err(RunError::Manifest(
            "call_hierarchy_index_build batch_size must be greater than zero".to_string(),
        ));
    }

    let mut graph_index = hir::graph_index::GraphIndex::new();
    for batch in context.modules.chunks(batch_size) {
        {
            let db = open_call_hierarchy_batch(context, batch);
            for &module in batch {
                graph_index.add_module(&db, module);
            }
        }
        syntax::clear_shared_node_cache();
        rayon::broadcast(|_| syntax::clear_shared_node_cache());
    }
    let reverse_target_count = graph_index
        .method_nodes()
        .filter(|target| !built.index.callers(*target).is_empty())
        .count();
    let digest = hir::call_hierarchy_method_digest(
        &built.index,
        &graph_index,
        &context.paths,
        Some(&context.workspace_root),
    );
    let digest_input = digest
        .rows()
        .iter()
        .map(|(target, caller)| format!("{target}\t{caller}"))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(Observation {
        count: built.pair_count,
        digest_input,
        call_hierarchy_index: Some(CallHierarchyIndexObservation {
            method_count: built.method_count,
            unique_pair_count: built.pair_count,
            reverse_target_count,
            estimated_heap_bytes: built.estimated_heap_bytes,
            batch_size,
            build_duration_ns: built.elapsed.as_nanos() as u64,
        }),
    })
}

fn pull_report_count(result: &lsp_types::DocumentDiagnosticReportResult) -> usize {
    use lsp_types::{DocumentDiagnosticReport, DocumentDiagnosticReportResult};
    match result {
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => {
            full.full_document_diagnostic_report.items.len()
        }
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(_)) => 0,
        DocumentDiagnosticReportResult::Partial(_) => 0,
    }
}

/// Frozen request context, mirroring what dispatch builds per request. Built
/// outside the timed section; the first build's cost is recorded once.
fn build_latency_ctx(env: &mut BenchEnv) -> LatencyRequestContext {
    let t = Instant::now();
    // Freezing the path table is O(files) — cache it across warm iterations;
    // `open_document` / the edit path reset the cache when paths could move.
    let file_paths = env
        .frozen_paths
        .get_or_insert_with(|| FrozenFilePaths::freeze(&env.state.vfs.read()))
        .clone();
    let state = &env.state;
    let disk_read = state.try_disk_read().expect("benchmark workspace has readable disk");
    let ctx = LatencyRequestContext {
        disk_read,
        analysis: state.analysis_host.analysis(),
        workspace_root: state.workspace_root.clone(),
        project: state.project.clone(),
        diagnostics_baseline: Arc::clone(&state.diagnostics_baseline),
        diagnostics_config: state.diagnostics_config.clone(),
        position_encoding: state.position_encoding,
        supports_code_description: state.supports_code_description,
        supports_diagnostic_tags: state.supports_diagnostic_tags,
        supports_insert_text_mode_adjust_indentation: state
            .supports_insert_text_mode_adjust_indentation,
        supports_workspace_edit_document_changes: state.supports_workspace_edit_document_changes,
        folding_range_limit: state.folding_range_limit,
        task_sender: state.task_pool.pool.sender.clone(),
        call_hierarchy_index: state.call_hierarchy_index.ensure(),
        call_hierarchy_wait_policy: state.call_hierarchy_wait_policy,
        client_sender: state.sender.clone(),
        mem_docs: state.mem_docs.freeze(),
        file_paths,
        scope_dirty_docs: state.scope_dirty_docs.clone(),
    };
    let ns = t.elapsed().as_nanos() as u64;
    env.ctx_build_ns.get_or_insert(ns);
    ctx
}

/// Byte offsets → LSP UTF-16 range (the runner's manifests store byte offsets;
/// handler params speak LSP positions).
fn offset_range_to_lsp(text: &str, start: u32, end: u32) -> Option<lsp_types::Range> {
    let index = LineIndex::new(text);
    let start = crate::lsp::to_proto::position_utf16(&index, text, TextSize::from(start))?;
    let end = crate::lsp::to_proto::position_utf16(&index, text, TextSize::from(end))?;
    Some(lsp_types::Range { start, end })
}
