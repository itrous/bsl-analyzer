use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::{atomic::AtomicUsize, Arc},
};

use clap::ValueEnum;
use ide_db::metadata;

#[derive(Debug, Clone, Default, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Console,
    Jsonl,
}

struct FileTiming {
    path: PathBuf,
    duration: std::time::Duration,
}

struct ProfilingStats {
    diagnostic_name: String,
    total: std::time::Duration,
    count: usize,
    max: std::time::Duration,
    max_file: PathBuf,
}

impl ProfilingStats {
    fn from_timings(diagnostic_name: String, timings: Vec<FileTiming>) -> Option<Self> {
        if timings.is_empty() {
            return None;
        }

        let total: std::time::Duration = timings.iter().map(|t| t.duration).sum();
        let count = timings.len();
        let max_timing = timings.iter().max_by_key(|t| t.duration)?;

        Some(Self {
            diagnostic_name,
            total,
            count,
            max: max_timing.duration,
            max_file: max_timing.path.clone(),
        })
    }

    fn average(&self) -> std::time::Duration {
        if self.count == 0 {
            std::time::Duration::ZERO
        } else {
            self.total / self.count as u32
        }
    }

    fn print(&self) {
        println!();
        println!("=== Diagnostic Profiling: {} ===", self.diagnostic_name);
        println!("Files analyzed: {}", self.count);
        println!("Total time:     {:.3}ms", self.total.as_secs_f64() * 1000.0);
        println!("Average time:   {:.3}ms", self.average().as_secs_f64() * 1000.0);
        println!("Max time:       {:.3}ms", self.max.as_secs_f64() * 1000.0);
        println!("Max file:       {}", self.max_file.display());
    }
}

/// Source bytes one analysis chunk may hold beside its file cap. The chunk's
/// working set (syntax trees, lowered bodies, inference) scales with its bytes,
/// so this is what bounds the peak when a run of giant modules shares a chunk.
const CHUNK_BYTES: u64 = 32 << 20;

/// Best-effort human-readable message from a caught panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic during analysis".to_string()
    }
}

/// CLI inputs selecting the analysis scope, applied in priority order:
/// `--diff-filter` JSON > `--git-diff <ref>` > `--changed-files` > `[analysis].diff_base`.
/// (`--git-diff` and `--changed-files` are mutually exclusive at the clap level:
/// two competing explicit sources are an input error, not a silent preference.)
struct ScopeCliArgs {
    incremental: bool,
    changed_files: Option<Vec<PathBuf>>,
    git_diff: Option<String>,
    diff_filter: Option<PathBuf>,
}

#[cfg(test)]
fn collect_bsl_files(source_roots: &[PathBuf]) -> Result<Vec<PathBuf>, walkdir::Error> {
    collect_bsl_files_in_scope(source_roots, &project_model::ExcludedPaths::default())
}

/// The analysis file universe: every module body under the source roots, first
/// spelling wins across overlapping roots. A directory the project excluded is not
/// entered, under whatever spelling the walk reaches it.
fn collect_bsl_files_in_scope(
    source_roots: &[PathBuf],
    exclusions: &project_model::ExcludedPaths,
) -> Result<Vec<PathBuf>, walkdir::Error> {
    let exclusions = exclusions.respelled_under(source_roots);
    let mut bsl_files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in source_roots {
        let mut resolved = project_model::ResolvedDirs::default();
        let walk =
            walkdir::WalkDir::new(root).follow_links(true).into_iter().filter_entry(|entry| {
                !(entry.file_type().is_dir() || entry.depth() == 0 || entry.path_is_symlink())
                    || !exclusions.prunes_walked_dir(
                        entry.path(),
                        entry.path_is_symlink(),
                        &mut resolved,
                    )
            });
        for entry in walk {
            let entry = entry?;
            if entry.file_type().is_file() && project_model::is_bsl_source_path(entry.path()) {
                let path = entry.path().to_path_buf();
                if seen.insert(path.clone()) {
                    bsl_files.push(path);
                }
            }
        }
    }
    Ok(bsl_files)
}

#[allow(clippy::too_many_arguments)]
pub fn analyze(
    source_dir: PathBuf,
    workspace_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    reporters: Vec<String>,
    quiet: bool,
    incremental: bool,
    changed_files: Option<Vec<PathBuf>>,
    git_diff: Option<String>,
    workers: Option<usize>,
    format: OutputFormat,
    only_diagnostic: Option<String>,
    diff_filter_path: Option<PathBuf>,
    ignored_authors: Vec<String>,
    source_set: super::source_set::SourceSetArgs,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    analyze_salsa(
        source_dir,
        workspace_dir,
        output_dir,
        config_path,
        reporters,
        quiet,
        workers,
        format,
        only_diagnostic,
        ScopeCliArgs { incremental, changed_files, git_diff, diff_filter: diff_filter_path },
        ignored_authors,
        source_set,
        None,
    )
}

pub fn diagnostics_baseline(
    command: super::diagnostics_baseline::DiagnosticsBaselineCommand,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = command.common().clone();
    analyze_salsa(
        args.source_dir,
        None,
        None,
        args.config,
        Vec::new(),
        true,
        None,
        OutputFormat::Console,
        None,
        ScopeCliArgs { incremental: false, changed_files: None, git_diff: None, diff_filter: None },
        Vec::new(),
        super::source_set::SourceSetArgs::default(),
        Some(command),
    )
}

fn diagnostic_author_lines(start_line: usize, end_line: usize, end_column: usize) -> (u32, u32) {
    let end_line = if end_line > start_line && end_column == 0 { end_line - 1 } else { end_line };
    (start_line as u32 + 1, end_line as u32 + 1)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageProof {
    pub total: usize,
    pub analyzed: usize,
    pub failed: bool,
    pub unreadable: bool,
    pub cancelled: bool,
    pub truncated: bool,
    pub out_of_scope: bool,
    pub stale: bool,
    pub reload_incomplete: bool,
    pub selected_diagnostic: bool,
    pub author_filter: bool,
}

impl CoverageProof {
    pub fn require_full(&self) -> Result<(), String> {
        let full = self.analyzed == self.total
            && !self.failed
            && !self.unreadable
            && !self.cancelled
            && !self.truncated
            && !self.out_of_scope
            && !self.stale
            && !self.reload_incomplete
            && !self.selected_diagnostic
            && !self.author_filter;
        if full {
            Ok(())
        } else {
            Err(format!("full diagnostics coverage required: {self:?}"))
        }
    }
}

fn analysis_is_full(
    scope_active: bool,
    incremental: bool,
    selected_diagnostic: bool,
    author_filter: bool,
    has_errors: bool,
) -> bool {
    !scope_active && !incremental && !selected_diagnostic && !author_filter && !has_errors
}

/// Build the author filter from the CLI list (which replaces the config list,
/// mirroring how `--git-diff` overrides `[analysis].diff_base`). A configured
/// filter that cannot work — no repository, shallow clone, unborn HEAD — is a
/// hard error: silently reporting vendor findings would defeat the filter.
fn build_author_filter(
    source_dir: &Path,
    cli_authors: &[String],
    config_authors: &[String],
) -> Result<Option<Arc<vcs::AuthorFilter>>, Box<dyn Error + Send + Sync>> {
    let effective = if cli_authors.is_empty() { config_authors } else { cli_authors };
    if effective.is_empty() {
        return Ok(None);
    }
    let filter = vcs::AuthorFilter::new(source_dir, effective.to_vec())
        .map_err(|e| format!("author filter: {e}"))?;
    tracing::info!(
        authors = effective.len(),
        head = %filter.head_identity(),
        "author filter active"
    );
    Ok(Some(Arc::new(filter)))
}

/// Build the effective [`base_db::AnalysisScope`]. A configured-but-broken git
/// source (missing repo/ref) is a hard error: silently analyzing everything
/// would defeat the point of the filter in CI.
fn build_scope(
    source_dir: &Path,
    args: &ScopeCliArgs,
    config_diff_base: Option<&str>,
) -> Result<Option<Arc<base_db::AnalysisScope>>, Box<dyn Error + Send + Sync>> {
    use base_db::AnalysisScope;

    if let Some(path) = &args.diff_filter {
        let content = std::fs::read_to_string(path)?;
        let report: vcs::DiffReport = serde_json::from_str(&content)?;
        tracing::info!(
            path = %path.display(),
            files = report.files.len(),
            base = %report.base_ref,
            "analysis scope from external diff report"
        );
        return Ok(Some(Arc::new(AnalysisScope::from_relative_report(
            report.base_ref,
            report.files.into_iter().map(|(path, change)| (path, change.hunks)),
        ))));
    }

    if let Some(base) = &args.git_diff {
        return Ok(Some(Arc::new(scope_from_git(source_dir, base)?)));
    }

    if let Some(files) = &args.changed_files {
        // The scope matches by suffix, so a spelling the walk never produces matches
        // nothing. An absolute path written through a symlinked ancestor is one such
        // spelling (`/tmp` and `$TMPDIR` on macOS, a linked checkout in CI): the walk
        // reports the resolved path, and an incremental run would otherwise report
        // success over a file it never analyzed.
        //
        // The resolved spelling is ADDED, not substituted. Resolving in place anchors a
        // relative entry — the ordinary form, and what `git diff --name-only` prints — at
        // the process directory, which is not where the file being analyzed usually is;
        // that entry would then match nothing at all. Both spellings name the same file,
        // and the run counts walked files in scope rather than scope entries.
        let mut scope_paths = Vec::with_capacity(files.len());
        for path in files {
            scope_paths.push(path.clone());
            match path.canonicalize() {
                Ok(resolved) if resolved != *path => scope_paths.push(resolved),
                _ => {}
            }
        }
        return Ok(Some(Arc::new(AnalysisScope::from_whole_files("changed-files", scope_paths))));
    }

    if let Some(base) = config_diff_base {
        return Ok(Some(Arc::new(scope_from_git(source_dir, base)?)));
    }

    if args.incremental {
        tracing::warn!(
            "--incremental without --changed-files/--git-diff and no [analysis].diff_base: analyzing everything"
        );
    }

    Ok(None)
}

fn scope_from_git(
    source_dir: &Path,
    base: &str,
) -> Result<base_db::AnalysisScope, Box<dyn Error + Send + Sync>> {
    let diff = vcs::generate_workdir_diff_report(source_dir, base, true)
        .map_err(|e| format!("analysis scope from git ref '{base}' failed: {e}"))?;
    Ok(base_db::AnalysisScope::from_report(
        diff.report.base_ref,
        &diff.workdir,
        diff.report.files.into_iter().map(|(path, change)| (path, change.hunks)),
    ))
}

#[allow(clippy::too_many_arguments)]
fn analyze_salsa(
    source_dir: PathBuf,
    workspace_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    reporters: Vec<String>,
    quiet: bool,
    workers: Option<usize>,
    format: OutputFormat,
    only_diagnostic: Option<String>,
    scope_args: ScopeCliArgs,
    ignored_authors: Vec<String>,
    source_set: super::source_set::SourceSetArgs,
    baseline_command: Option<super::diagnostics_baseline::DiagnosticsBaselineCommand>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    use std::{
        panic::{catch_unwind, AssertUnwindSafe},
        sync::atomic::Ordering,
        time::Instant,
    };

    use base_db::SourceDatabase;
    use ide::jsonl::FileMetrics;
    use ide::{DiagnosticsConfig, DiagnosticsContext, RootDatabaseImpl};
    use ide_db::AnalysisProvider;
    use indicatif::{ProgressBar, ProgressStyle};
    use rayon::prelude::*;
    use vfs::FileId;

    use bsl_analyzer::reporters::{AnalysisResults, FileAnalysis, ReporterRegistry};

    let _span = tracing::info_span!("cli_analyze").entered();

    let profiling_enabled = only_diagnostic.is_some();
    let jsonl = matches!(format, OutputFormat::Jsonl);

    // Honour `--workers` by sizing the global rayon pool before any rayon use
    // (metadata load and the per-chunk `par_iter` both draw from it). Must run
    // before `load_metadata`, which initialises the pool on first parallel read;
    // best-effort because the pool can only be built once per process.
    if let Some(w) = workers.filter(|w| *w > 0) {
        if let Err(e) = rayon::ThreadPoolBuilder::new().num_threads(w).build_global() {
            tracing::warn!("Failed to set worker count to {w}: {e}");
        }
    }

    tracing::info!("Analyzing project: {:?}", source_dir);
    tracing::info!("Reporters: {:?}", reporters);
    tracing::info!("Quiet mode: {}", quiet);
    if let Some(ref diag) = only_diagnostic {
        tracing::info!("Profiling diagnostic: {}", diag);
    }

    let start = Instant::now();
    let slab_verify = std::env::var("BSL_SLAB_VERIFY").is_ok_and(|v| v == "1");

    // Absolute walk paths are required for scope matching: a native git scope
    // keys files by absolute path, so a relative `-s .` would match nothing.
    let source_dir = source_dir
        .canonicalize()
        .map_err(|e| format!("source directory {}: {e}", source_dir.display()))?;

    let workspace_dir = workspace_dir.unwrap_or_else(|| source_dir.clone());
    let output_dir = output_dir.unwrap_or_else(|| PathBuf::from("."));

    tracing::info!("Loading project configuration");
    let mut proj_config = if let Some(ref cfg) = config_path {
        project_model::ProjectConfig::load_from_file(cfg)?
    } else {
        project_model::ProjectConfig::load(&source_dir)?.unwrap_or_default()
    };

    // Applied before anything reads the config: `configuration_path` and
    // `load_metadata` below run on this value, and the path the first produces
    // becomes the interned configuration input the diagnostics resolve through.
    source_set.resolve(&source_dir)?.apply_to(&mut proj_config);
    bsl_analyzer::help_bootstrap::bootstrap_for_config(&proj_config, &source_dir);

    let scope = build_scope(&source_dir, &scope_args, proj_config.analysis.diff_base.as_deref())?;
    let author_filter =
        build_author_filter(&source_dir, &ignored_authors, &proj_config.analysis.ignored_authors)?;

    let _metadata = proj_config.load_metadata(&source_dir)?;
    let configuration_path = proj_config.resolve_configuration_path(&source_dir)?;

    // Scope the file walk to the configuration source root (+ extension roots)
    // instead of the raw `-s` dir, so vendored/build copies such as
    // `.build/vendor` are not analyzed as a duplicate configuration.
    let project = project_model::Project::with_config(&source_dir, proj_config.clone())?;
    if baseline_command.is_some() && project.diagnostics_baseline()?.is_none() {
        return Err("[diagnostics.baseline].path is not configured".into());
    }
    if let Some(command) = &baseline_command {
        command.preflight(&project)?;
    }
    // Validate and retain one immutable snapshot before JSONL can emit its start event.
    // A malformed configured file therefore fails the run without a partial report.
    let loaded_baseline = if baseline_command.is_none() {
        let snapshot =
            ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load(&project);
        if let ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::Error {
            detail,
            ..
        } = &snapshot
        {
            return Err(detail.clone().into());
        }
        Some(snapshot)
    } else {
        None
    };
    // Deliberately not gated on `--quiet`: this explains why the findings that
    // follow are wrong, and suppressing it leaves the run looking merely broken.
    for notice in [project.standalone_extension_notice(), project.standalone_external_notice()]
        .into_iter()
        .flatten()
    {
        eprintln!("warning: {notice}");
    }
    let source_roots = project.source_roots();

    tracing::info!("Creating database");
    let mut db = RootDatabaseImpl::default();
    bsl_analyzer::features_state::apply_project_config_to_db(&mut db, &project.config);
    bsl_analyzer::features_state::apply_compatibility_mode_to_db(&mut db, Some(&project));

    // Register the base + extension configuration roots (with their dependency
    // topology) so per-file resolution — and the `&ИзменениеИКонтроль` effective
    // merge (`effective_target` → `pair_base_module_path`) — can pair an extension
    // module to its base. Mirrors the LSP workspace loader; without this
    // `all_config_paths` is empty and extension merging silently never activates.
    db.set_workspace_configs_snapshot(ide_db::metadata::WorkspaceConfigsSnapshot::from_project(
        &project,
    ));

    tracing::info!("Finding BSL files in {:?}", source_roots);
    let bsl_files = collect_bsl_files_in_scope(&source_roots, project.source_exclusions())?;

    tracing::info!(
        "Found {} BSL files across {} source root(s)",
        bsl_files.len(),
        source_roots.len()
    );

    tracing::info!("Loading files into database");
    let mut file_set = vfs::FileSet::new();
    let mut all_file_ids: Vec<(FileId, PathBuf)> = Vec::new();

    for (idx, path) in bsl_files.iter().enumerate() {
        let file_id = FileId(idx as u32);
        let vfs_path = vfs::VfsPath::new(path.clone());
        file_set.insert(file_id, vfs_path);
        all_file_ids.push((file_id, path.clone()));
    }

    let file_ids: Vec<(FileId, PathBuf)> = if let Some(ref scope) = scope {
        let filtered: Vec<_> =
            all_file_ids.iter().filter(|(_, path)| scope.is_file_in_scope(path)).cloned().collect();
        tracing::info!(
            "Analysis scope (base '{}'): {} of {} files",
            scope.base_ref(),
            filtered.len(),
            all_file_ids.len()
        );
        if !quiet && !jsonl {
            println!(
                "Analysis scope (base '{}'): {} of {} files",
                scope.base_ref(),
                filtered.len(),
                all_file_ids.len()
            );
        }
        filtered
    } else {
        all_file_ids.clone()
    };

    let file_set_arc = Arc::new(file_set);
    let source_root_id = base_db::SourceRootId(0);
    let source_root = base_db::SourceRoot::new_local((*file_set_arc).clone());
    db.set_source_root(source_root_id, source_root);

    // Disk-backed: record each file's content revision and drop the text (re-read on
    // demand by `file_text_query` under the salsa LRU, verified against the revision),
    // via the shared resident-load primitive. A file that cannot be read aborts the
    // run, preserving the previous `?` behaviour.
    let unreadable =
        ide_host_core::register_files_disk_backed(&mut db, source_root_id, &all_file_ids);
    if let Some((path, err)) = unreadable.into_iter().next() {
        return Err(format!("failed to read BSL file {}: {err}", path.display()).into());
    }
    ide::warm_batch_config_roots(&db, &file_ids);

    tracing::info!(
        "Files loaded, starting parallel diagnostics (threads: {})",
        rayon::current_num_threads()
    );

    let progress = if !quiet && !jsonl {
        let pb = ProgressBar::new(file_ids.len() as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );
        Some(pb)
    } else {
        None
    };

    let diagnostics = proj_config.diagnostics.rules_json();
    let mut config = DiagnosticsConfig::from_project_json(
        &diagnostics,
        proj_config.output.resolve_locale().unwrap_or_default(),
    );

    if let Some(ref diag_name) = only_diagnostic {
        config.apply_cli_filters(std::slice::from_ref(diag_name), &[]);
    }

    config.scope = scope.clone();

    tracing::info!(
        disabled = config.disabled.len(),
        only_enabled = ?config.only_enabled.as_ref().map(|v| v.len()),
        params = config.parameters.len(),
        locale = ?config.locale,
        "Loaded DiagnosticsConfig"
    );
    let config = Arc::new(config);
    let processed = Arc::new(AtomicUsize::new(0));
    // Author-filter accounting. Blame errors other than the tolerated
    // not-in-HEAD class are fatal for the run (a partially filtered report is
    // worse than a failed one), but rayon workers cannot abort the pool —
    // the first error is parked here and re-raised after the chunk loop.
    let author_seen = AtomicUsize::new(0);
    let author_dropped = AtomicUsize::new(0);
    let author_blame_us = std::sync::atomic::AtomicU64::new(0);
    let author_fatal: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let progress_arc = Arc::new(progress);
    let workspace_dir_arc = Arc::new(workspace_dir.clone());
    // Process in chunks, trimming salsa's LRU caches and the parser's green-node
    // cache between chunks. A single-revision batch never trips salsa's automatic
    // (revision-boundary) eviction, so without this every file's heavy memos
    // (parse/HIR/inference) stay resident at once. Each query is trimmed only to
    // its own `lru` cap, so the durable cross-module currency (method return
    // types, generously capped) is largely retained while the heavy per-file
    // memos fall out once their chunk is done.
    // Chunk size is the peak-memory <-> wall-time knob (the in-chunk working set
    // is the peak, bounded only by this). On ERP 500 trims peak RSS ~25% vs 1000
    // (~6.0 GB vs ~8.1 GB) for ~+5% wall, so it is the default; override via
    // `BSL_SALSA_CHUNK` (larger = faster + more RSS, smaller = leaner + slower).
    // The count cap alone leaves the peak to chance: the working set scales with
    // the chunk's source bytes, and 500 files of a large configuration run from
    // under a megabyte to over a hundred, so a byte cap closes the chunks that a
    // run of giant modules would otherwise fill.
    let chunk_size = std::env::var("BSL_SALSA_CHUNK")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(500);
    let chunks = stdx::batch::chunks_by_budget(
        &file_ids,
        |(_, path)| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        stdx::batch::BatchBudget::files(chunk_size).with_bytes(CHUNK_BYTES),
    );
    // Only read complexity into the JSONL `metrics` field when BOTH complexity
    // diagnostics are enabled: then both queries are warm during the chunk and
    // reading them is a salsa cache hit (free). Requiring both (not either) is
    // deliberate — `module_cyclomatic_query` builds CFGs, so if only the
    // cognitive diagnostic ran the cyclomatic read would be a cold recompute.
    // When either is off (e.g. `--only-diagnostic`, or disabled in config) the
    // whole metrics block is skipped, keeping the run cost flat.
    let emit_metrics = jsonl
        && !config.is_disabled(ide::DiagnosticCode::CognitiveComplexity)
        && !config.is_disabled(ide::DiagnosticCode::CyclomaticComplexity);

    type FileResult =
        (Option<FileAnalysis>, Option<FileTiming>, Option<FileMetrics>, Option<String>);
    let mut results: Vec<FileResult> = Vec::with_capacity(file_ids.len());
    let report_mem = std::env::var_os("BSL_MEM_REPORT").is_some();
    let chunk_profile = std::env::var_os("BSL_CHUNK_PROFILE").is_some();
    // For modules with >= N methods, warm body inference in parallel before
    // computing diagnostics, so a giant module does not stall its chunk
    // single-threaded. On by default: union ordering is deterministic, so the
    // output is identical, wall drops ~24% on large configs and peak RSS is
    // unchanged. `BSL_PRIME_GIANTS=<N>` overrides the threshold, `0` disables.
    let prime_giants: Option<usize> = match std::env::var("BSL_PRIME_GIANTS") {
        Ok(value) => value.parse::<usize>().ok().filter(|n| *n > 0),
        Err(_) => Some(700),
    };
    let num_chunks = chunks.len();

    // The JSONL contract opens with a `start` event before any analysis. Salsa
    // emits the `file`/`done` events as a batch once the chunked run finishes
    // (it collects and evicts per chunk rather than emitting live), so only the
    // opener is hoisted here; content and ordering still mirror the JSONL contract.
    if jsonl {
        use ide::jsonl::StartEvent;

        println!("{}", serde_json::to_string(&StartEvent::new(file_ids.len()))?);
    }

    for (chunk_idx, &chunk) in chunks.iter().enumerate() {
        // Per-chunk timing to separate the parallel phase from the serial tail
        // (the slowest single file's straggler wait) and the single-threaded
        // `enforce_lru` trim — the two work-starvation sources between chunks.
        let max_file_us = std::sync::atomic::AtomicU64::new(0);
        let par_start = std::time::Instant::now();
        let mut chunk_results: Vec<FileResult> = {
            let config_path_input = configuration_path
                .as_ref()
                .map(|path| metadata::intern_configuration_path(&db, &path.to_string_lossy(), 0));
            chunk
                .par_iter()
                .map_with(db.clone(), |db_snapshot, (file_id, path)| {
                    if let Some(min) = prime_giants {
                        db_snapshot.prime_module_inference(*file_id, min);
                    }
                    let provider = ide_db::SalsaProvider::with_file_set(
                        db_snapshot,
                        config_path_input,
                        Some(&file_set_arc),
                    );
                    let ctx = DiagnosticsContext::new(&config, *file_id, &provider);

                    let file_start = std::time::Instant::now();
                    // Standalone pass, then the `&ИзменениеИКонтроль` effective merge — both
                    // reuse the run-global `config_path_input` + `file_set` so the effective
                    // pass resolves metadata/cross-module context exactly as the standalone one.
                    let diagnostics = match catch_unwind(AssertUnwindSafe(|| {
                        let standalone = ide::compute_diagnostics(&ctx);
                        // Under `BSL_SLAB_VERIFY=1` the memoised assembly is run
                        // as well, so that its self-check covers every module of
                        // the configuration; the batch answer stays the standalone one.
                        if slab_verify {
                            let _ = ide::file_diagnostics(db_snapshot, *file_id, &config);
                        }
                        ide::apply_extension_merge(
                            db_snapshot,
                            *file_id,
                            &config,
                            config_path_input,
                            Some(&file_set_arc),
                            standalone,
                        )
                    })) {
                        Ok(diags) => diags,
                        Err(e) => {
                            let message = panic_message(e.as_ref());
                            tracing::error!("Panic analyzing {:?}: {}", path, message);
                            return (None, None, None, Some(message));
                        }
                    };

                    // Read the already-memoised complexity for the JSONL `metrics`
                    // field: when the complexity diagnostics are enabled they have
                    // just run, so these queries are warm here (a salsa cache hit).
                    // Computed before the per-chunk LRU eviction, since at the
                    // output stage the memos are gone. `emit_metrics` is false for
                    // non-JSONL output and when complexity is off, so neither the
                    // reporter path nor a filtered profiling run pays anything.
                    // Wrapped in its own panic boundary (like the diagnostics pass)
                    // so a query panic drops only this file's metrics, not the chunk.
                    let metrics = if emit_metrics {
                        catch_unwind(AssertUnwindSafe(|| {
                            let hir = provider.module_hir_metrics(*file_id);
                            let cyclomatic = provider.module_cyclomatic(*file_id);
                            FileMetrics {
                                functions: hir.len(),
                                complexity: cyclomatic.total(),
                                cognitive_complexity: hir.total_cognitive(),
                            }
                        }))
                        .ok()
                    } else {
                        None
                    };
                    let elapsed = file_start.elapsed();
                    max_file_us.fetch_max(elapsed.as_micros() as u64, Ordering::Relaxed);

                    let timing = if profiling_enabled {
                        Some(FileTiming { path: path.clone(), duration: elapsed })
                    } else {
                        None
                    };

                    let count = processed.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Some(ref pb) = &*progress_arc {
                        pb.set_position(count as u64);
                        pb.set_message(format!(
                            "{:.0} files/sec",
                            count as f64 / start.elapsed().as_secs_f64()
                        ));
                    }

                    let file_analysis = if !diagnostics.is_empty() {
                        let file_text = match fs::read_to_string(path) {
                            Ok(text) => text,
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to read file {:?} for diagnostic conversion: {}",
                                    path,
                                    e
                                );
                                return (
                                    None,
                                    timing,
                                    metrics,
                                    Some(format!(
                                        "failed to read file for diagnostic conversion: {e}"
                                    )),
                                );
                            }
                        };

                        let file_line_index = line_index::LineIndex::new(&file_text);

                        let diagnostic_outputs: Vec<_> = diagnostics
                            .iter()
                            .map(|d| d.to_output_with_index(&file_text, &file_line_index))
                            .collect();

                        if diagnostic_outputs.is_empty() {
                            None
                        } else {
                            // Capture the source line per finding while the text is
                            // still hot (this pass runs in parallel across files),
                            // so reporters that need a line-shift-stable fingerprint
                            // do not re-read the file or rescan its text per finding.
                            let source_lines: Vec<_> = file_text.lines().collect();
                            let line_snippets: Vec<String> = diagnostic_outputs
                                .iter()
                                .map(|d| {
                                    ide::diagnostics_baseline::diagnostic_line_snippet(
                                        &source_lines,
                                        d.start_line,
                                    )
                                })
                                .collect();
                            // Counted here, over the FULL set: suppression by the
                            // baseline removes entries later, and a reporter counting
                            // afterwards would give an active finding the ordinal — and
                            // so the fingerprint — of a suppressed one.
                            let mut seen: std::collections::HashMap<(&str, &str), u32> =
                                std::collections::HashMap::new();
                            let occurrences: Vec<u32> = diagnostic_outputs
                                .iter()
                                .zip(line_snippets.iter())
                                .map(|(diagnostic, snippet)| {
                                    let ordinal = seen
                                        .entry((diagnostic.code.as_str(), snippet.as_str()))
                                        .or_insert(0);
                                    let current = *ordinal;
                                    *ordinal += 1;
                                    current
                                })
                                .collect();
                            Some(FileAnalysis {
                                path: path.clone(),
                                relative_path: path
                                    .strip_prefix(&*workspace_dir_arc)
                                    .unwrap_or(path)
                                    .to_path_buf(),
                                diagnostics: diagnostic_outputs,
                                line_snippets,
                                occurrences,
                            })
                        }
                    } else {
                        None
                    };

                    (file_analysis, timing, metrics, None)
                })
                .collect()
        };
        let par_wall = par_start.elapsed();
        results.append(&mut chunk_results);

        // Snapshot the salsa memory map at its high-water mark: the final chunk's
        // memos are all still live here, before the trim below evicts them. Without
        // this the only report (post-loop) reflects the post-eviction trough, which
        // understates the working set by an order of magnitude.
        if report_mem && chunk_idx + 1 == num_chunks {
            bsl_analyzer::mem_report::print_salsa_memory_report(
                &db,
                "PEAK (last chunk, pre-eviction)",
            );
            bsl_analyzer::mem_report::print_salsa_event_report(
                &db,
                "PEAK (last chunk, pre-eviction)",
            );
            bsl_analyzer::mem_report::print_salsa_key_event_report(
                &db,
                "PEAK (last chunk, pre-eviction)",
            );
        }

        // `config_path_input` (which borrows `&db`) is now out of scope, so the
        // exclusive `&mut db` borrow is free: trim the salsa memos beyond their
        // caps, then release the parser's thread-local green-node caches (which
        // salsa does not own) on the driver and every rayon worker.
        let evict_start = std::time::Instant::now();
        db.enforce_lru();
        syntax::clear_shared_node_cache();
        rayon::broadcast(|_| syntax::clear_shared_node_cache());
        profile::purge_allocator();
        if chunk_profile {
            eprintln!(
                "chunk {:>3}/{}: files={} par={:.2}s straggler_max={:.2}s evict={:.2}s",
                chunk_idx + 1,
                num_chunks,
                chunk.len(),
                par_wall.as_secs_f64(),
                max_file_us.load(Ordering::Relaxed) as f64 / 1e6,
                evict_start.elapsed().as_secs_f64(),
            );
        }
    }

    if let Some(ref pb) = &*progress_arc {
        pb.finish_with_message("Analysis complete");
    }

    let elapsed = start.elapsed();
    if slab_verify {
        eprintln!("slab verify mismatches: {}", ide::slab_verify_mismatches());
    }

    if report_mem {
        bsl_analyzer::mem_report::print_salsa_memory_report(&db, "TROUGH (post-eviction)");
        bsl_analyzer::mem_report::print_salsa_event_report(&db, "TROUGH (post-eviction)");
        // No per-key report here: eviction runs no new queries, so the hot keys are
        // identical to PEAK's, and the per-key decode is meaningful only at the peak
        // working set. Keeping it at PEAK also keeps the decode inside the single
        // revision that executed the keys, without depending on eviction leaving the
        // revision untouched.
    }

    // Results are appended chunk-by-chunk in `file_ids` order, and rayon's
    // indexed `par_iter().collect()` preserves within-chunk order, so
    // `file_analyses[i]` / `metrics_list[i]` / `errors_list[i]` correspond to
    // `file_ids[i]` — the JSONL path relies on this alignment to pair each file
    // with its metrics and error.
    let mut file_analyses: Vec<Option<FileAnalysis>> = Vec::with_capacity(results.len());
    let mut all_timings: Vec<FileTiming> = Vec::new();
    let mut metrics_list: Vec<Option<FileMetrics>> = Vec::with_capacity(results.len());
    let mut errors_list: Vec<Option<String>> = Vec::with_capacity(results.len());
    for (analysis, timing, metrics, error) in results {
        file_analyses.push(analysis);
        if let Some(timing) = timing {
            all_timings.push(timing);
        }
        metrics_list.push(metrics);
        errors_list.push(error);
    }

    let baseline_summary = if baseline_command.is_none() {
        let presentation_filter = scope_args.incremental || only_diagnostic.is_some();
        let full = analysis_is_full(
            scope.is_some(),
            scope_args.incremental,
            only_diagnostic.is_some(),
            author_filter.is_some(),
            errors_list.iter().any(Option::is_some),
        );
        let coverage = if full {
            ide::diagnostics_baseline::DiagnosticsBaselineCoverage::Full
        } else {
            let completed_files =
                if scope.is_some() || author_filter.is_some() || presentation_filter {
                    std::collections::BTreeSet::new()
                } else {
                    file_ids
                        .iter()
                        .zip(&errors_list)
                        .filter(|(_, error)| error.is_none())
                        .filter_map(|((_, path), _)| path.strip_prefix(&source_dir).ok())
                        .map(|path| path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
                        .collect()
                };
            ide::diagnostics_baseline::DiagnosticsBaselineCoverage::Partial { completed_files }
        };
        let all_project_files = file_ids
            .iter()
            .filter_map(|(_, path)| path.strip_prefix(&source_dir).ok())
            .map(|path| path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
            .collect();
        let can_prove_partition_completion =
            scope.is_none() && !presentation_filter && author_filter.is_none();
        Some(super::diagnostics_baseline::classify_files_with_loaded(
            &project,
            &mut file_analyses,
            coverage,
            can_prove_partition_completion.then_some(&all_project_files),
            loaded_baseline.as_ref().expect("normal analysis loads one baseline snapshot"),
        )?)
    } else {
        None
    };

    // Baseline classification deliberately precedes the author presentation filter.
    // Blame is expensive already, so the extra file read keeps the hot diagnostics path
    // simple while preserving the same parallelism for projects that enable the filter.
    if let Some(filter) = author_filter.as_deref() {
        file_analyses
            .par_iter_mut()
            .zip(file_ids.par_iter())
            .zip(errors_list.par_iter_mut())
            .for_each(|((analysis, (_, path)), error_slot)| {
                let Some(file) = analysis.as_mut() else { return };
                let bytes = match fs::read(path) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        let message =
                            format!("author filter: cannot read {}: {error}", path.display());
                        *analysis = None;
                        *error_slot = Some(message.clone());
                        let mut fatal = author_fatal.lock().unwrap();
                        if fatal.is_none() {
                            *fatal = Some(message);
                        }
                        return;
                    }
                };
                let blame_start = std::time::Instant::now();
                let keep = match filter.lines_kept_cached(path, &bytes) {
                    Ok(keep) => keep,
                    Err(error) => {
                        let message = format!("author filter: {error}");
                        *analysis = None;
                        *error_slot = Some(message.clone());
                        let mut fatal = author_fatal.lock().unwrap();
                        if fatal.is_none() {
                            *fatal = Some(message);
                        }
                        return;
                    }
                };
                author_blame_us
                    .fetch_add(blame_start.elapsed().as_micros() as u64, Ordering::Relaxed);
                let before = file.diagnostics.len();
                let kept: Vec<_> = file
                    .diagnostics
                    .iter()
                    .map(|diagnostic| {
                        let (start, end) = diagnostic_author_lines(
                            diagnostic.start_line,
                            diagnostic.end_line,
                            diagnostic.end_column,
                        );
                        keep.range_survives(start, end)
                    })
                    .collect();
                file.retain_findings(|index| kept[index]);
                author_seen.fetch_add(before, Ordering::Relaxed);
                author_dropped.fetch_add(before - file.diagnostics.len(), Ordering::Relaxed);
                if file.diagnostics.is_empty() {
                    *analysis = None;
                }
            });
    }

    // Both the fatal verdict and the counters are read AFTER the filter pass that
    // produces them; reading them earlier reports an empty run and turns a
    // fail-closed abort into a report with silently missing findings.
    //
    // A fatal blame error aborts the run, but only AFTER the JSONL stream is
    // closed properly (`file` events + `done`): consumers must never see a
    // start-only stream. Per-file errors are already recorded in the results.
    let author_fatal_msg = author_fatal.lock().unwrap().take();
    if author_filter.is_some() && author_fatal_msg.is_none() {
        let seen = author_seen.load(Ordering::Relaxed);
        let dropped = author_dropped.load(Ordering::Relaxed);
        let blame_secs = author_blame_us.load(Ordering::Relaxed) as f64 / 1e6;
        tracing::info!(seen, dropped, blame_secs, "author filter applied");
        if !quiet && !jsonl {
            println!(
                "Author filter: dropped {dropped} of {seen} finding(s) (blame {blame_secs:.1}s)"
            );
        }
    }

    let total_diagnostics: usize =
        file_analyses.iter().flatten().map(|f| f.diagnostics.len()).sum();

    // JSONL emits one `file` event per analyzed file (clean files included),
    // closed by `done`, mirroring the `--format jsonl` contract (the `start`
    // event was already emitted before analysis). `metrics` carries the real
    // cyclomatic/cognitive complexity; `error` is set for files whose analysis
    // panicked or whose text could not be read (so a crashed file is not
    // silently reported as clean), and those are tallied into `done`'s
    // `failed_files`.
    if jsonl {
        use ide::jsonl::{DoneEvent, FileEvent};

        let mut failed_files = 0;
        for (i, (_, path)) in file_ids.iter().enumerate() {
            let diagnostics =
                file_analyses[i].as_ref().map(|f| f.diagnostics.clone()).unwrap_or_default();
            let error = errors_list[i].clone();
            if error.is_some() {
                failed_files += 1;
            }
            let file_event = FileEvent::new(
                path.display().to_string(),
                diagnostics,
                metrics_list[i].clone(),
                error,
            );
            println!("{}", serde_json::to_string(&file_event)?);
        }

        let done_event =
            DoneEvent::new(elapsed.as_secs_f64(), file_ids.len(), total_diagnostics, failed_files)
                .with_baseline(baseline_summary.clone().unwrap_or_else(
                    ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled,
                ));
        println!("{}", serde_json::to_string(&done_event)?);

        if let Some(message) = author_fatal_msg {
            return Err(message.into());
        }
        tracing::info!("JSONL analysis complete");
        return Ok(());
    }

    if let Some(message) = author_fatal_msg {
        return Err(message.into());
    }

    let all_diagnostics: Vec<FileAnalysis> = file_analyses.into_iter().flatten().collect();

    if let Some(command) = baseline_command {
        let output_format = command.output_format();
        let proof = CoverageProof {
            total: all_file_ids.len(),
            analyzed: file_ids.len(),
            failed: errors_list.iter().any(Option::is_some),
            unreadable: false,
            cancelled: false,
            truncated: false,
            out_of_scope: scope.is_some(),
            stale: false,
            reload_incomplete: false,
            selected_diagnostic: only_diagnostic.is_some(),
            author_filter: author_filter.is_some(),
        };
        let result =
            super::diagnostics_baseline::apply(&project, &all_diagnostics, &proof, command)?;
        match output_format {
            super::diagnostics_baseline::DiagnosticsBaselineOutputFormat::Text => {
                println!("{result}");
            }
            super::diagnostics_baseline::DiagnosticsBaselineOutputFormat::Json => {
                println!("{}", serde_json::to_string(&result)?);
            }
        }
        if !result.success {
            return Err("diagnostics baseline differs from current diagnostics".into());
        }
        return Ok(());
    }

    let analysis_results = AnalysisResults {
        files_analyzed: bsl_files.len(),
        files_with_issues: all_diagnostics.len(),
        total_diagnostics,
        elapsed_secs: elapsed.as_secs_f64(),
        diagnostics: all_diagnostics,
        source_dir: source_dir.clone(),
        workspace_dir: workspace_dir.clone(),
        baseline: baseline_summary
            .unwrap_or_else(ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled),
    };

    std::fs::create_dir_all(&output_dir)?;

    let registry = ReporterRegistry::new();
    let reporter_keys = if reporters.is_empty() { vec!["console".to_string()] } else { reporters };

    for key in &reporter_keys {
        match registry.get(key) {
            Some(reporter) => {
                if let Err(e) = reporter.report(&analysis_results, &output_dir) {
                    tracing::error!("Reporter '{}' failed: {}", key, e);
                    eprintln!("Error: Reporter '{}' failed: {}", key, e);
                }
            }
            None => {
                eprintln!("Error: Unknown reporter '{}'", key);
                eprintln!("Valid reporters: {}", registry.keys().join(", "));
                return Err(format!("Unknown reporter: {}", key).into());
            }
        }
    }

    if let Some(diag_name) = only_diagnostic {
        if let Some(stats) = ProfilingStats::from_timings(diag_name, all_timings) {
            stats.print();
        }
    }

    tracing::info!("Analysis complete");

    Ok(())
}

#[cfg(test)]
mod analyze_walk_tests {
    use super::{analysis_is_full, diagnostic_author_lines, CoverageProof};

    #[test]
    fn author_filter_uses_half_open_diagnostic_end() {
        assert_eq!(diagnostic_author_lines(0, 1, 0), (1, 1));
        assert_eq!(diagnostic_author_lines(0, 1, 1), (1, 2));
    }

    #[test]
    fn diagnostics_baseline_full_gate_rejects_every_incomplete_proof() {
        let full = CoverageProof { total: 2, analyzed: 2, ..CoverageProof::default() };
        assert!(full.require_full().is_ok());

        let mut proofs = Vec::new();
        proofs.push(CoverageProof { analyzed: 1, ..full.clone() });
        macro_rules! denied {
            ($($field:ident),+ $(,)?) => {$({
                let mut proof = full.clone();
                proof.$field = true;
                proofs.push(proof);
            })+};
        }
        denied!(
            failed,
            unreadable,
            cancelled,
            truncated,
            out_of_scope,
            stale,
            reload_incomplete,
            selected_diagnostic,
            author_filter,
        );
        assert!(proofs.iter().all(|proof| proof.require_full().is_err()));
    }

    #[test]
    fn analyze_baseline_partial_scope_marks_every_cli_filter() {
        assert!(analysis_is_full(false, false, false, false, false));
        for inputs in [
            (true, false, false, false, false),
            (false, true, false, false, false),
            (false, false, true, false, false),
            (false, false, false, true, false),
            (false, false, false, false, true),
        ] {
            assert!(!analysis_is_full(inputs.0, inputs.1, inputs.2, inputs.3, inputs.4));
        }
    }

    #[test]
    fn the_analysis_walk_takes_a_case_variant_module_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("CommonModules/X/Ext")).unwrap();
        std::fs::write(dir.path().join("CommonModules/X/Ext/Module.BSL"), "").unwrap();
        let files = super::collect_bsl_files(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(files.len(), 1, "Module.BSL — тело модуля и входит во вселенную обхода");
    }

    #[cfg(unix)]
    #[test]
    fn the_analysis_walk_prunes_an_excluded_symlink_cycle_before_descent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let hidden = root.join(".tmp");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(root.join("Allowed.bsl"), "").unwrap();
        std::fs::write(hidden.join("Hidden.bsl"), "").unwrap();
        std::os::unix::fs::symlink(&hidden, hidden.join("loop")).unwrap();

        let files = super::collect_bsl_files_in_scope(
            &[root.to_path_buf()],
            &project_model::ExcludedPaths::new([hidden]),
        )
        .expect("an excluded cycle must never be entered");

        assert_eq!(files, [root.join("Allowed.bsl")]);
    }

    #[test]
    fn the_analysis_walk_honours_project_exclusions_before_descent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in ["Allowed.bsl", ".tmp/Hidden.bsl", ".tmp2/Visible.bsl"] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let excluded = project_model::ExcludedPaths::new([root.join(".tmp")]);

        let files = super::collect_bsl_files_in_scope(&[root.to_path_buf()], &excluded).unwrap();

        assert_eq!(files.len(), 2, "{files:?}");
        assert!(files.iter().all(|path| !path.starts_with(root.join(".tmp"))));
        assert!(files.iter().any(|path| path.ends_with(".tmp2/Visible.bsl")));
    }
}
