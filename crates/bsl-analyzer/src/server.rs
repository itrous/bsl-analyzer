use std::path::PathBuf;

use anyhow::{Context, Result};
use crossbeam_channel::{select, Receiver};
use lsp_server::{Connection, Message, Notification, Request};
use lsp_types::{
    notification::{Exit, Notification as _},
    request::Shutdown,
    CodeActionProviderCapability, DiagnosticOptions, DiagnosticServerCapabilities,
    FoldingRangeProviderCapability, InitializeParams, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensServerCapabilities, ServerCapabilities,
    SignatureHelpOptions, TextDocumentSyncCapability, TextDocumentSyncKind,
    WorkDoneProgressOptions,
};

use crate::{
    global_state::GlobalState,
    handlers::{NotificationDispatcher, RequestDispatcher},
    locale::parse_lsp_locale,
    lsp::{PositionEncoding, Progress},
};

pub fn main_loop(connection: Connection) -> Result<()> {
    tracing::info!("BSL Analyzer LSP server starting");

    // A request that unwinds on a propagated cancellation is told apart from one
    // that unwinds on a real panic by this counter; without the hook every
    // propagated unwind would read as "nothing panicked".
    crate::panic_watch::install();

    let (initialize_id, initialize_params) =
        connection.initialize_start().context("Failed to start initialization")?;

    // Keep the raw capabilities: the workspace-diagnostic refresh capability must be read from
    // the wire directly, because the pinned `lsp-types` maps it to the key `diagnostic` while the
    // LSP 3.17 spec (and clients like VS Code) send `diagnostics`.
    let raw_capabilities = initialize_params.get("capabilities").cloned().unwrap_or_default();

    let initialize_params: InitializeParams =
        serde_json::from_value(initialize_params).context("Failed to parse InitializeParams")?;

    tracing::info!(
        "Client info: {:?}",
        initialize_params.client_info.as_ref().map(|info| &info.name)
    );

    let position_encoding = PositionEncoding::negotiate(&initialize_params.capabilities);
    let supports_insert_text_mode_adjust_indentation =
        client_supports_insert_text_mode_adjust_indentation(&initialize_params.capabilities);
    let supports_code_description =
        client_supports_code_description(&initialize_params.capabilities);
    let supports_diagnostic_tags =
        crate::lsp::to_proto::ClientTags::from_capabilities(&initialize_params.capabilities);
    let supports_workspace_edit_document_changes =
        client_supports_workspace_edit_document_changes(&initialize_params.capabilities);
    let folding_range_limit = client_folding_range_limit(&initialize_params.capabilities);

    // The pull diagnostic provider is opt-in per configuration, so the scope must be
    // known before capabilities are advertised — before the VFS loader (and its own
    // config load) starts. Read it directly from the workspace root; `Off` leaves the
    // server on push-only diagnostics with no provider advertised.
    let workspace_root = extract_workspace_root(&initialize_params);
    let early_config = workspace_root.as_ref().and_then(|root| {
        match project_model::ProjectConfig::load(root) {
            Ok(config) => config,
            Err(e) => {
                // Capability advertisement and help selection only; the workspace
                // load below rejects the broken config loudly.
                tracing::warn!(error = %e, "project config unreadable; capabilities use defaults");
                None
            }
        }
    });
    // Before any request can reach a platform lookup: the configured help source
    // must serve the first answer, not a lazily fixed default.
    let help_root =
        workspace_root.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    platform_help::bootstrap(early_config.as_ref(), &help_root);
    let workspace_diagnostics_scope =
        early_config.map(|config| config.features.workspace_diagnostics).unwrap_or_default();

    let server_capabilities = server_capabilities(position_encoding, workspace_diagnostics_scope);

    let initialize_result = lsp_types::InitializeResult {
        capabilities: server_capabilities,
        server_info: Some(lsp_types::ServerInfo {
            name: "bsl-analyzer".to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
    };

    let mut initialize_value = serde_json::to_value(initialize_result)?;
    if let Some(object) = initialize_value.as_object_mut() {
        object.insert(
            "offsetEncoding".to_string(),
            serde_json::json!([position_encoding.as_offset_encoding()]),
        );
    }

    connection
        .initialize_finish(initialize_id, initialize_value)
        .context("Failed to finish initialization")?;

    tracing::info!("LSP server initialized");

    let mut state = GlobalState::new(connection.sender);
    state.position_encoding = position_encoding;
    state.supports_insert_text_mode_adjust_indentation =
        supports_insert_text_mode_adjust_indentation;
    state.supports_code_description = supports_code_description;
    state.supports_diagnostic_tags = supports_diagnostic_tags;
    state.supports_workspace_edit_document_changes = supports_workspace_edit_document_changes;
    state.folding_range_limit = folding_range_limit;
    // Suppress push publishing only when the client will actually pull, so a
    // pull-capable client does not render open-buffer diagnostics twice; a client
    // that cannot pull keeps push even with the feature enabled.
    state.pull_diagnostics_active = workspace_diagnostics_scope.is_enabled()
        && client_supports_pull_diagnostics(&initialize_params.capabilities);
    state.supports_workspace_diagnostic_refresh =
        client_supports_workspace_diagnostic_refresh(&raw_capabilities);

    state.init_empty_source_root();

    state.lsp_locale = initialize_params.locale.as_deref().map(parse_lsp_locale);
    if let Some(locale) = state.lsp_locale {
        tracing::info!(?locale, "client supplied LSP locale");
    }

    if let Some(ref root) = workspace_root {
        if let Err(e) = state.set_workspace_root(root.clone()) {
            state.show_error_message(format!(
                "bsl-analyzer: invalid project, analysis unavailable: {e}"
            ));
            // Mirror the no-workspace path: nothing will load, so diagnostics
            // must not stay deferred behind a boot window that never closes.
            state.update_diagnostics_config();
            state.vfs_done = true;
        }
    } else {
        tracing::warn!("No workspace root provided by client");
        state.update_diagnostics_config();
        // Without a workspace root no VFS loader runs, so no `Finished` event
        // will ever flip this flag — mark the (empty) workspace as loaded or
        // diagnostics would stay deferred forever.
        state.vfs_done = true;
    }

    run_event_loop(&mut state, &connection.receiver)?;

    tracing::info!("LSP server shutting down");
    Ok(())
}

/// Whether the client advertised `InsertTextMode::ADJUST_INDENTATION` in its
/// completion-item capabilities. Used to ask the client to indent multi-line
/// snippet continuation lines to the cursor column (the snippet bodies carry
/// only relative nesting).
fn client_supports_insert_text_mode_adjust_indentation(
    caps: &lsp_types::ClientCapabilities,
) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|td| td.completion.as_ref())
        .and_then(|c| c.completion_item.as_ref())
        .and_then(|ci| ci.insert_text_mode_support.as_ref())
        .is_some_and(|s| s.value_set.contains(&lsp_types::InsertTextMode::ADJUST_INDENTATION))
}

/// Whether the client renders `Diagnostic.codeDescription`. Without it the link to the
/// standard travels only in the message suffix: publishing a property the client did not
/// ask for is what the capability exists to prevent.
fn client_supports_code_description(caps: &lsp_types::ClientCapabilities) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|td| td.publish_diagnostics.as_ref())
        .and_then(|pd| pd.code_description_support)
        .unwrap_or(false)
}

/// Whether the client honors versioned `WorkspaceEdit.documentChanges`. When it does,
/// the rename handler returns `TextDocumentEdit`s carrying each open document's version
/// so the client can reject edits computed against a since-superseded buffer; otherwise
/// the server must fall back to the unversioned `changes` map.
fn client_supports_workspace_edit_document_changes(caps: &lsp_types::ClientCapabilities) -> bool {
    caps.workspace
        .as_ref()
        .and_then(|w| w.workspace_edit.as_ref())
        .and_then(|we| we.document_changes)
        .unwrap_or(false)
}

/// The client's `rangeLimit` for folding ranges, if it named one. The handler
/// truncates an over-long `textDocument/foldingRange` response to it.
fn client_folding_range_limit(caps: &lsp_types::ClientCapabilities) -> Option<u32> {
    caps.text_document.as_ref()?.folding_range.as_ref()?.range_limit
}

/// Whether the client advertised pull-diagnostics support (`textDocument/diagnostic`).
/// When it did and the feature is enabled, the server serves diagnostics by pull and
/// suppresses push to avoid double-reporting open buffers.
fn client_supports_pull_diagnostics(caps: &lsp_types::ClientCapabilities) -> bool {
    caps.text_document.as_ref().and_then(|td| td.diagnostic.as_ref()).is_some()
}

/// Whether the client advertised `workspace.diagnostics.refreshSupport`, so the server may ask it
/// to re-pull workspace diagnostics after background state changes. Read from the raw capabilities
/// and accepting both the spec key `diagnostics` and the `lsp-types` key `diagnostic`.
fn client_supports_workspace_diagnostic_refresh(raw_capabilities: &serde_json::Value) -> bool {
    let workspace = &raw_capabilities["workspace"];
    ["diagnostics", "diagnostic"]
        .iter()
        .any(|key| workspace[key]["refreshSupport"].as_bool().unwrap_or(false))
}

fn extract_workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    #[allow(deprecated)]
    if let Some(ref root_uri) = params.root_uri {
        if let Ok(path) = root_uri.to_file_path() {
            return Some(path);
        }
        tracing::warn!("Failed to convert root_uri to path: {}", root_uri);
        None
    } else {
        params.root_path.as_ref().map(PathBuf::from)
    }
}

fn run_event_loop(state: &mut GlobalState, receiver: &Receiver<Message>) -> Result<()> {
    loop {
        select! {
            recv(receiver) -> msg => {
                state.note_loop_activity();
                refresh_diagnostics_baseline(state);
                handle_lsp_msg(state, msg?)?;
                while let Ok(msg) = receiver.try_recv() {
                    handle_lsp_msg(state, msg)?;
                }
            }

            recv(&state.loader_receiver) -> msg => {
                state.note_loop_activity();
                refresh_diagnostics_baseline(state);
                handle_loader_msg(state, msg?)?;
            }

            recv(&state.task_pool.receiver) -> task => {
                state.note_loop_activity();
                refresh_diagnostics_baseline(state);
                handle_task(state, task?)?;
                while let Ok(task) = state.task_pool.receiver.try_recv() {
                    handle_task(state, task)?;
                }
            }

            default(IDLE_TRIM_TICK) => {
                handle_idle_tick(state);
            }
        }

        if state.shutdown_requested {
            state.call_hierarchy_index.shutdown();
            break;
        }

        // The capacity gate keeps this from spinning: a schedule that finds the
        // pool still saturated would requeue the same URI, and the loop only
        // re-runs on a genuine wake (a finished worker frees a slot and posts
        // its result task).
        if !state.pending_diagnostics_uris.is_empty() && state.task_pool.pool.has_capacity() {
            for uri in std::mem::take(&mut state.pending_diagnostics_uris) {
                crate::handlers::schedule_diagnostics(state, &uri);
            }
        }

        // A queued analysis-scope (re)build starts as soon as a worker frees up;
        // launched before the batch so the batch's own scope gate sees the newest
        // loading state instead of sweeping under a stale filter.
        state.maybe_spawn_scope_build();

        // Launch the deferred whole-project diagnostics batch (Stream B) once a
        // worker frees up. Interactive per-file scheduling above is drained first so
        // the batch never contends ahead of the file the user is editing; its own
        // guards make this a no-op unless a run is pending and the pool has capacity.
        crate::handlers::spawn_workspace_batch(state);
        state.spawn_pending_call_hierarchy_index_builds();
    }

    Ok(())
}

/// How long the event loop must stay silent before an idle tick fires. Salsa evicts
/// beyond the LRU caps only at a revision boundary, so a session that loads, navigates
/// cross-module and then sits without edits retains every touched file's memos — and
/// jemalloc returns their pages only after they are actually freed. The tick is the
/// only trim trigger outside edits and workspace-batch chunk boundaries.
const IDLE_TRIM_TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// Consecutive idle ticks before the trim escalates from the interactive LRU profile
/// to the deep sweep profile. The deep trim also evicts the open files' parse trees
/// and lowered bodies — they re-derive cheaply on the next interaction, but not for
/// free, so it waits until the session looks abandoned rather than merely paused.
const IDLE_TRIM_DEEP_TICKS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleTrimKind {
    /// `enforce_lru` on the interactive caps: drops the cross-module navigation tail
    /// beyond each query's cap, keeps the open files' working set.
    Shallow,
    /// `enforce_lru_deep`: one eviction pass on the small sweep caps, then the
    /// interactive caps are restored — the resident set shrinks to a cold floor.
    Deep,
}

/// Kill switch: `BSL_IDLE_TRIM=0` (or `off`) disables the idle trim entirely, leaving
/// eviction to edits and workspace-batch boundaries as before.
fn idle_trim_disabled() -> bool {
    std::env::var("BSL_IDLE_TRIM").is_ok_and(|v| v == "0" || v.eq_ignore_ascii_case("off"))
}

/// Opt-in live-session memory observability (`BSL_MEM_REPORT_IDLE=1`): print the
/// salsa memory/event tables to stderr on the first tick of each idle period (the
/// state the preceding editing burst left behind) and again right after an idle
/// trim (what the trim reclaimed). Off by default — the tables are ~50 lines per
/// snapshot and only useful while measuring a session.
fn idle_mem_report_enabled() -> bool {
    matches!(std::env::var("BSL_MEM_REPORT_IDLE").as_deref(), Ok("1"))
}

/// Memory budget in megabytes under which the idle trim is skipped, overridable via
/// `BSL_IDLE_TRIM_MEM_BUDGET_MB`. The `0` default always trims: an idle server has
/// nothing in flight to cancel, eviction beyond the caps is the point, and the cost
/// is bounded re-derivation on the next interaction.
fn idle_trim_mem_budget_mb() -> usize {
    std::env::var("BSL_IDLE_TRIM_MEM_BUDGET_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
}

/// Decide whether this idle tick trims, and how hard. `None` unless the workspace has
/// loaded, no workspace-batch sweep is active (its chunks run between loop wakes and
/// hold a db snapshot the trim would block on; the sweep also has its own trim
/// schedule), every interactive snapshot has drained, the process is over the idle
/// budget, and the stage has not already run for this idle period. Pure so the
/// schedule is testable with an injected budget signal.
fn idle_trim_kind(state: &GlobalState, over_budget: bool) -> Option<IdleTrimKind> {
    let batch_active = state.workspace_batch_plan.is_some() || state.workspace_batch_in_flight;
    if !state.vfs_done
        || batch_active
        || !state.interactive_analysis_quiescent()
        || !over_budget
        || state.idle_ticks == 0
    {
        return None;
    }
    if state.idle_ticks >= IDLE_TRIM_DEEP_TICKS && !state.idle_deep_trimmed {
        return Some(IdleTrimKind::Deep);
    }
    if !state.idle_shallow_trimmed {
        return Some(IdleTrimKind::Shallow);
    }
    None
}

/// Take the baseline whatever comes next will be answered from, reloading it first when
/// the ground moved under it.
///
/// The watcher is armed asynchronously, after the loader has already announced the load
/// finished; a baseline write landing before that raises no event, and the snapshot the
/// initial load produced would otherwise answer for the life of the process. Asking here
/// puts the LSP server on the resident's discipline: the reading a load produced is
/// compared with disk before anything is answered from it — and every branch of the loop
/// answers, the loader's finalize by publishing the documents opened during the load.
fn refresh_diagnostics_baseline(state: &mut GlobalState) {
    if state.refresh_diagnostics_baseline() {
        answer_from_the_reloaded_baseline(state);
    }
}

/// The baseline in hand changed: everything answered from the old one is answered again.
fn answer_from_the_reloaded_baseline(state: &mut GlobalState) {
    state.reset_workspace_batch();
    state.analysis_host.request_cancellation();
    let uris = state.opened_document_uris();
    invalidate_diagnostics(state, &uris);
    for uri in uris {
        crate::handlers::notification::schedule_diagnostics(state, &uri);
    }
    state.mark_workspace_batch_dirty();
    state.request_workspace_diagnostic_refresh();
}

/// Handle one idle tick: advance the idle clock and, when [`idle_trim_kind`] says so,
/// trim the Salsa LRU, release this thread's shared green-node cache, and purge the
/// allocator so the freed pages actually leave RSS instead of sitting in jemalloc's
/// retention. Runs on the event-loop thread, which owns `&mut` to the database; the
/// quiescence gate guarantees no live snapshot, so the exclusive borrow cannot block.
fn handle_idle_tick(state: &mut GlobalState) {
    state.idle_ticks = state.idle_ticks.saturating_add(1);

    // Ref-only git movement (fetch/rebase/reset) produces no file events; the
    // idle tick is the only place that notices it.
    state.check_scope_ref_drift();

    // Ahead of the trim kill-switch on purpose: measuring raw accumulation with
    // `BSL_IDLE_TRIM=0` still needs the idle snapshot.
    if idle_mem_report_enabled() && state.idle_ticks == 1 {
        let db = state.analysis_host.raw_database();
        const LABEL: &str = "idle (first tick after activity)";
        crate::mem_report::print_salsa_memory_report(db, LABEL);
        crate::mem_report::print_salsa_event_report(db, LABEL);
    }

    if idle_trim_disabled() {
        return;
    }

    let over_budget = crate::handlers::workspace_batch::over_mem_budget(idle_trim_mem_budget_mb());
    let Some(kind) = idle_trim_kind(state, over_budget) else {
        return;
    };

    let db = state.analysis_host.raw_database_mut();
    match kind {
        IdleTrimKind::Shallow => db.enforce_lru(),
        IdleTrimKind::Deep => ide::sweep_lru_deep(db),
    }
    syntax::clear_shared_node_cache();

    if kind == IdleTrimKind::Deep {
        // The green-node caches are thread-local and never evict, so the parse trees
        // this thread's clear just released stay pinned by every task-pool worker
        // that ever parsed — reach them through a pool broadcast. Detached jobs, so
        // their completion does not read as loop activity and reset the idle clock.
        // The workers clear asynchronously; pages they free after the purge below
        // are returned by jemalloc's decay instead.
        if let Err(err) = state.task_pool.try_broadcast(syntax::clear_shared_node_cache) {
            tracing::debug!(?err, "idle trim: worker green-node cache clear skipped");
        }
        // The raw-spelling side of the NormName pool is an accelerator cache
        // that grows with edit churn; dropping it never invalidates ids.
        intern::trim_raw_cache();
    }
    profile::purge_allocator();

    if idle_mem_report_enabled() {
        let db = state.analysis_host.raw_database();
        let label = format!("idle post-trim ({kind:?})");
        crate::mem_report::print_salsa_memory_report(db, &label);
        crate::mem_report::print_salsa_event_report(db, &label);
    }

    state.idle_shallow_trimmed = true;
    if kind == IdleTrimKind::Deep {
        state.idle_deep_trimmed = true;
    }
    tracing::info!(?kind, idle_ticks = state.idle_ticks, "idle Salsa LRU trim");
}

fn handle_lsp_msg(state: &mut GlobalState, msg: Message) -> Result<()> {
    match msg {
        Message::Request(req) => {
            if state.shutdown_requested {
                tracing::warn!("Received request after shutdown: {}", req.method);
                return Ok(());
            }
            handle_request(state, req)?;
        }
        Message::Notification(not) => {
            handle_notification(state, not)?;
        }
        Message::Response(resp) => {
            tracing::warn!("Unexpected response: {:?}", resp);
        }
    }
    Ok(())
}

fn handle_loader_msg(state: &mut GlobalState, msg: vfs::loader::Message) -> Result<()> {
    match msg {
        vfs::loader::Message::Progress { n_total, n_done, config_version: _, dir: _ } => {
            use vfs::loader::LoadingProgress;
            match n_done {
                LoadingProgress::Finished => {
                    let finalize_start = std::time::Instant::now();
                    tracing::info!("VFS loading complete");

                    // Boot-phase memory breakdown. Source batches were drained
                    // into Salsa incrementally as they streamed, so the text
                    // high-water (`boot_peak_text_bytes`) stayed near one loader
                    // chunk and this final `process_changes` is a near no-op; the
                    // remaining resident growth is the metadata substrate.
                    let mb = |b: u64| b / (1024 * 1024);
                    let rss_peak = state.boot_peak_rss_bytes;
                    let text_high_water = state.boot_peak_text_bytes;

                    state.process_changes(true);
                    let rss_after_load = crate::smoke::read_rss_bytes().unwrap_or(0);

                    state.init_source_root();
                    // Reopen the whole-config loader gate closed at
                    // `set_workspace_root`: the bootstrap and warm-up below run
                    // against the now-complete workspace, and the input flip
                    // invalidates anything resolved against the boot-window stub.
                    state.analysis_host.raw_database_mut().set_workspace_load_complete(true);
                    state.bootstrap_metadata_substrate();

                    state.report_progress(
                        "Loading",
                        Progress::Report,
                        Some("Loading metadata...".into()),
                        Some(0.95),
                    );
                    state.warm_metadata_cache();
                    let rss_steady = crate::smoke::read_rss_bytes().unwrap_or(0);

                    tracing::info!(
                        rss_peak_mb = mb(rss_peak),
                        rss_after_load_mb = mb(rss_after_load),
                        rss_steady_mb = mb(rss_steady),
                        text_high_water_mb = mb(text_high_water),
                        metadata_resident_mb = mb(rss_steady.saturating_sub(rss_after_load)),
                        "boot memory profile: source text drained incrementally during load",
                    );

                    state.degraded_files_count = state.skipped_bsl.len();
                    let extra_violations = state.assert_total_vfs_invariant();
                    if extra_violations > 0 {
                        tracing::error!(
                            extra_violations,
                            "total-VFS invariant violated unexpectedly — B1/B2-A missed a path",
                        );
                    }
                    if state.degraded_files_count > 0 {
                        tracing::warn!(
                            degraded = state.degraded_files_count,
                            "{} BSL paths skipped (unreadable or non-UTF-8)",
                            state.degraded_files_count,
                        );
                    }

                    state.vfs_done = true;
                    state.report_progress("Loading", Progress::End, Some("Done".into()), Some(1.0));

                    // Documents opened during the load had their dependency
                    // preload and diagnostics deferred; replay both now that
                    // the source root and metadata substrate are complete.
                    for uri in state.mem_docs.uris() {
                        if let Ok(file_id) = state.vfs_file_for_url(&uri) {
                            crate::handlers::notification::preload_dependencies(state, file_id);
                        }
                        crate::handlers::notification::schedule_diagnostics(state, &uri);
                    }

                    state.request_semantic_tokens_refresh();
                    // Diagnostics for closed files only become available now; nudge a pull-capable
                    // client to re-pull the open document (workspace pull stays unadvertised so a
                    // long-poll client does not stall the open file behind a whole-workspace sweep).
                    state.request_workspace_diagnostic_refresh();
                    // Whole-project coverage for closed files ships as a deferred push batch
                    // (Stream B), started off the critical path now that analysis is ready.
                    state.mark_workspace_batch_dirty();

                    tracing::info!(
                        elapsed_ms = finalize_start.elapsed().as_millis() as u64,
                        "vfs_done finalize complete (process_changes + init_source_root + warm_metadata)",
                    );
                }
                LoadingProgress::Scanning => {
                    tracing::info!("VFS scanning workspace...");
                    state.report_progress(
                        "Loading",
                        Progress::Begin,
                        Some("Scanning workspace...".into()),
                        None,
                    );
                }
                LoadingProgress::Started => {
                    tracing::info!("VFS loading started: {} files", n_total);
                    state.report_progress(
                        "Loading",
                        Progress::Report,
                        Some(format!("Loading {} files...", n_total)),
                        Some(0.0),
                    );
                }
                LoadingProgress::Progress(done) => {
                    tracing::debug!(done, n_total, "VFS loading progress");
                    let fraction = Progress::fraction(done, n_total);
                    state.report_progress(
                        "Loading",
                        Progress::Report,
                        Some(format!("{}/{}", done, n_total)),
                        Some(fraction),
                    );
                }
            }
        }
        vfs::loader::Message::Loaded { files } | vfs::loader::Message::Changed { files } => {
            let count = files.len();
            handle_vfs_msg(state, files, state.vfs_done)?;
            tracing::debug!(count, vfs_done = state.vfs_done, "streamed VFS batch");
        }
        vfs::loader::Message::WatchOnly { files } => {
            const VFS_WATCH_ONLY_BATCH: usize = 64;
            // A batch the loader produced under the previous configuration can still
            // arrive after a reload; the current exclusions decide.
            let mut resolved = stdx::path_exclusion::ResolvedDirs::default();
            let files: Vec<_> = files
                .into_iter()
                .filter(|path| !state.is_source_excluded(path.as_ref(), &mut resolved))
                .collect();
            let count = files.len();
            let baseline_paths = state.diagnostics_baseline.observation_paths();
            for chunk in files.chunks(VFS_WATCH_ONLY_BATCH) {
                let mut vfs = state.vfs.write();
                for path in chunk {
                    vfs.register_watch_only(vfs::VfsPath::new(path.as_path()));
                }
            }
            // Metadata XML is watch-only (content not loaded into Salsa), so its
            // edits arrive here, NOT through `process_changes`. After the initial
            // scan these are live changes — e.g. a `git pull` touching XML —
            // each of which invalidates the configuration of its owning root.
            // Refresh open documents so their diagnostics reflect the new metadata.
            if state.vfs_done {
                let changed: Vec<std::path::PathBuf> = files
                    .iter()
                    .filter(|path| {
                        let path: &std::path::Path = path.as_ref();
                        !baseline_paths.iter().any(|baseline| baseline == path)
                    })
                    .map(|path| AsRef::<std::path::Path>::as_ref(path).to_path_buf())
                    .collect();
                let baseline_changed =
                    changed.len() != files.len() && state.reload_diagnostics_baseline();
                if baseline_changed {
                    state.reset_workspace_batch();
                }
                // Per-MDO substrate: re-discover the affected roots and re-read only
                // the changed/new XML, so resolve_metadata_object reflects the edit at
                // MDO granularity (and a disk-backed file_text never reads stale bytes).
                if baseline_changed || !changed.is_empty() {
                    state.analysis_host.request_cancellation();
                }
                if !changed.is_empty() {
                    state.refresh_metadata_substrate(&changed);
                    // Coarse path: load_configuration is still the authoritative consumer
                    // until the per-MDO resolvers are migrated, so keep bumping its root.
                    state
                        .analysis_host
                        .raw_database_mut()
                        .bump_config_for_paths(changed.iter().map(|p| p.as_path()));
                    state.supersede_call_hierarchy_index(base_db::BSL_SOURCE_ROOT);
                }
                if baseline_changed || !changed.is_empty() {
                    let uris = state.opened_document_uris();
                    if baseline_changed {
                        invalidate_diagnostics(state, &uris);
                    }
                    for uri in uris {
                        crate::handlers::notification::schedule_diagnostics(state, &uri);
                    }
                }
                if baseline_changed {
                    state.mark_workspace_batch_dirty();
                    state.request_workspace_diagnostic_refresh();
                }
            }
            tracing::debug!(count, vfs_done = state.vfs_done, "registered WatchOnly batch",);
        }
        vfs::loader::Message::RemovedRecursive { paths } => {
            // A removed directory subtree (a watch backend reported only the
            // directory, not each child). Expand it against the file set and
            // tombstone descendants; ignore during the initial scan.
            if state.vfs_done {
                let n = paths.len();
                let bsl_removed = state.remove_directories(&paths);
                // A removed subtree can also drop metadata MDOs; re-discovering the
                // owning roots tombstones them from the per-MDO listings.
                let removed: Vec<std::path::PathBuf> = paths
                    .iter()
                    .map(|p| AsRef::<std::path::Path>::as_ref(p).to_path_buf())
                    .collect();
                let meta_changed = state.refresh_metadata_substrate(&removed);
                if bsl_removed || meta_changed {
                    for uri in state.opened_document_uris() {
                        crate::handlers::notification::schedule_diagnostics(state, &uri);
                    }
                }
                tracing::info!(removed_paths = n, "processed removed directory subtree");
            }
        }
    }
    Ok(())
}

fn invalidate_diagnostics(state: &mut GlobalState, uris: &[lsp_types::Url]) {
    for uri in uris {
        if let Some(token) = state.diagnostics_tokens.remove(uri) {
            token.cancel();
        }
        *state.diagnostics_generation.entry(uri.clone()).or_default() += 1;
    }
}

fn handle_task(state: &mut GlobalState, task: crate::global_state::Task) -> Result<()> {
    use crate::global_state::Task;

    match task {
        Task::DiagnosticsReady { uri, diagnostics, generation, completed_at } => {
            let current = state.diagnostics_generation.get(&uri).copied().unwrap_or(0);
            // Publish only the latest schedule for this uri (`== current`) and only
            // while it is still open — a result that finished after the document was
            // closed must not be published for a closed file.
            if generation == current && state.mem_docs.contains(&uri) {
                let publish_delay_ms = completed_at.elapsed().as_millis() as u64;
                let diagnostic_count = diagnostics.len();
                let allocated_mb = profile::memory_usage().allocated.megabytes();
                tracing::info!(
                    %uri,
                    generation,
                    publish_delay_ms,
                    diagnostic_count,
                    allocated_mb,
                    "publishing diagnostics",
                );
                let params =
                    lsp_types::PublishDiagnosticsParams { uri, diagnostics, version: None };
                let notification =
                    Notification::new("textDocument/publishDiagnostics".to_string(), params);
                state.sender.send(notification.into())?;
            } else {
                tracing::debug!(generation, current, "discarding stale diagnostics");
            }
        }
        Task::DiagnosticsCancelled { generation, completed_at } => {
            tracing::debug!(
                generation,
                publish_delay_ms = completed_at.elapsed().as_millis() as u64,
                "diagnostics cancelled",
            );
        }
        Task::DependenciesPreloaded { file_id, count } => {
            tracing::debug!(file_id = file_id.0, count, "dependencies preloaded");
            state.preload_tokens.remove(&file_id);
            state.preload_external_tokens.remove(&file_id);
        }
        Task::RequestResult { response } => {
            state.request_tokens.remove(&response.id);
            state.respond(response);
        }
        Task::PreloadExternalFiles { files } => {
            if files.is_empty() {
                return Ok(());
            }
            // Cache warming only — when the pool is saturated, skip it rather
            // than park the event loop in the bounded job queue.
            if !state.task_pool.pool.has_capacity() {
                tracing::debug!("task pool saturated; external preload skipped");
                return Ok(());
            }
            // Queued before a reload may have excluded some of them: an excluded file is
            // not warmed, whatever id it still carries.
            let mut resolved = stdx::path_exclusion::ResolvedDirs::default();
            let files: Vec<_> = files
                .into_iter()
                .filter(|&file_id| !state.is_file_id_excluded(file_id, &mut resolved))
                .collect();
            if files.is_empty() {
                return Ok(());
            }
            let file_count = files.len();
            let file_ids: Vec<u32> = files.iter().map(|f| f.0).collect();
            tracing::debug!(?file_ids, "preloading external files from semantic highlighting");

            let analysis = state.analysis_host.analysis();
            let task = analysis.warm_caches_task(&files);
            let first_file = files[0];

            if let Some(prev) = state.preload_external_tokens.remove(&first_file) {
                prev.cancel();
            }
            state.preload_external_tokens.insert(first_file, task.cancellation_token());

            let analysis_guard = state.note_analysis_spawned();
            let spawned = state.task_pool.pool.try_spawn(move || {
                let _analysis_guard = analysis_guard;
                let count = match salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                    task.run()
                })) {
                    Ok(count) => {
                        tracing::debug!(count = file_count, "external files preloaded");
                        count
                    }
                    Err(_) => {
                        tracing::debug!(file_id = first_file.0, "external file preload cancelled");
                        0
                    }
                };
                Task::DependenciesPreloaded { file_id: first_file, count }
            });
            if spawned.is_err() {
                // Unreachable after the capacity check above; a skipped warm-up
                // costs only latency.
                tracing::debug!(
                    file_id = first_file.0,
                    "task pool rejected external preload job; skipped"
                );
                state.preload_external_tokens.remove(&first_file);
            }
        }
        Task::AnalysisProgressTick { epoch } => {
            state.handle_analysis_progress_tick(epoch);
        }
        Task::AnalysisJobFinished => {
            state.note_analysis_finished();
        }
        Task::DiagnosticsBaselineRecheck { hold } => {
            if state.recheck_diagnostics_baseline(hold) {
                answer_from_the_reloaded_baseline(state);
            }
        }
        Task::AnalysisScopeReady { generation, result, identity } => {
            state.handle_analysis_scope_ready(generation, result, identity);
        }
        Task::WorkspaceBatchChunk { generation, outcome } => {
            apply_workspace_batch_completion(state, generation, outcome)?;
        }
        Task::CallHierarchyIndexBuilt { source_root, generation, index } => {
            if !state.call_hierarchy_index.is_ready_generation(source_root, generation)
                && !state.call_hierarchy_index.publish(source_root, generation, index)
            {
                tracing::debug!(?source_root, generation, "discarding stale call hierarchy index");
            }
        }
        Task::CallHierarchyIndexFailed { source_root, generation, reason } => {
            if !state.call_hierarchy_index.fail(source_root, generation, reason) {
                tracing::debug!(
                    ?source_root,
                    generation,
                    "discarding stale call hierarchy failure"
                );
            }
        }
        Task::CallHierarchyIndexSuperseded { source_root, generation, reason } => {
            tracing::debug!(?source_root, generation, reason, "call hierarchy index superseded");
            if state.call_hierarchy_index.finish_superseded(source_root, generation) {
                state.schedule_call_hierarchy_index_build(source_root);
            }
        }
        Task::CallHierarchyIndexBuildRequested { source_root, generation } => {
            if state.call_hierarchy_index.is_prepared(source_root, generation) {
                tracing::debug!(
                    ?source_root,
                    generation,
                    "scheduling prepared call hierarchy index"
                );
                state.schedule_call_hierarchy_index_build(source_root);
            } else {
                tracing::debug!(
                    ?source_root,
                    generation,
                    "discarding stale call hierarchy prepare"
                );
            }
        }
    }
    Ok(())
}

/// Chunks the batch may leave un-trimmed while over the memory budget with interactive
/// analysis in flight, before forcing a trim anyway — so resident memory stays bounded
/// even under sustained load. Boundaries under budget do not count: skipping their trim
/// is the intended steady state, not a deferral.
const WORKSPACE_BATCH_FORCE_TRIM_CHUNKS: u32 = 2;

/// How many times a chunk may unwind on `PropagatedPanic` before it is skipped. A
/// transient edit-cancellation cascade clears within a retry or two once the edit settles;
/// a genuine deterministic panic keeps unwinding and is skipped once the budget is spent,
/// rather than looping forever.
const WORKSPACE_BATCH_MAX_PROPAGATED_RETRIES: u32 = 3;

/// Handle one chunk of the deferred whole-project batch (Stream B) completing on a
/// worker. On a computed chunk: publish its diffed diagnostics and advance the sweep. On
/// a cancellation: keep the plan and retry the same chunk. On a deterministic failure:
/// skip the chunk. See [`WorkspaceBatchOutcome`].
fn apply_workspace_batch_completion(
    state: &mut GlobalState,
    generation: u64,
    outcome: crate::global_state::WorkspaceBatchOutcome,
) -> Result<()> {
    use crate::global_state::WorkspaceBatchOutcome;

    // The chunk worker has returned, so its snapshot is dropped and the singleton
    // in-flight flag/token are free again — clear them regardless of the outcome so the
    // batch never wedges.
    state.workspace_batch_in_flight = false;
    state.workspace_batch_token = None;

    // Drop a chunk from a superseded sweep (a reset bumped the generation): the plan is
    // already gone or replaced, so there is nothing to advance and applying it could
    // republish diagnostics the new configuration no longer includes.
    if generation != state.workspace_batch_generation || state.workspace_batch_plan.is_none() {
        return Ok(());
    }

    match outcome {
        // A concurrent edit cancelled this chunk. Keep the plan and cursor; the
        // bottom-of-loop dispatch retries the SAME chunk once the edit settles. Salsa
        // memoization resumes it cheaply. `PendingWrite` is transient, so this cannot
        // loop forever — a deterministic unwind arrives as `Failed` instead.
        WorkspaceBatchOutcome::Cancelled => Ok(()),
        // Ambiguous under parallelism: a transient edit-cancellation cascade (retry) or a
        // real deterministic panic in a shared query (skip). Disambiguate by budget — retry
        // a few times, and if it keeps unwinding treat it as deterministic and skip.
        WorkspaceBatchOutcome::Propagated => {
            let over_budget = {
                let plan = state.workspace_batch_plan.as_mut().expect("plan present after gate");
                plan.chunk_retries += 1;
                plan.chunk_retries > WORKSPACE_BATCH_MAX_PROPAGATED_RETRIES
            };
            if over_budget {
                tracing::warn!(
                    "workspace batch chunk exceeded propagated-panic retry budget; skipping"
                );
                advance_workspace_batch(state)
            } else {
                Ok(())
            }
        }
        // A deterministic panic: skip the chunk (its files go uncovered until the next
        // full sweep) rather than retry it forever. Advance as if it had computed empty.
        WorkspaceBatchOutcome::Failed => advance_workspace_batch(state),
        WorkspaceBatchOutcome::Computed(items) => {
            apply_workspace_batch_chunk(state, generation, items)?;
            advance_workspace_batch(state)
        }
    }
}

/// Advance the sweep past a completed (or skipped) chunk: move the cursor, trim the Salsa
/// LRU + parser caches when the process is over the sweep's memory budget (deferred
/// while interactive analysis is in flight — the trim cancels those requests — but
/// forced periodically and always on finish), and finalize when the file set is
/// exhausted. The final trim runs once on the shrunk sweep LRU profile, leaving a lean
/// post-sweep resident set.
fn advance_workspace_batch(state: &mut GlobalState) -> Result<()> {
    // `enforce_lru` cancels any in-flight interactive request exactly like an edit and
    // blocks until their snapshots drop, so only trim when no interactive analysis is in
    // flight — unless the sweep just finished (final cleanup) or too many over-budget
    // chunks have gone un-trimmed (memory valve). Computed before the mutable plan
    // borrow below.
    let quiescent = state.interactive_analysis_quiescent();
    let over_budget = {
        let plan = state.workspace_batch_plan.as_ref().expect("plan present after gate");
        crate::handlers::workspace_batch::over_mem_budget(plan.mem_budget_mb)
    };

    let (finished, do_trim) = {
        let plan = state.workspace_batch_plan.as_mut().expect("plan present after gate");
        advance_plan_cursor(plan, over_budget, quiescent)
    };

    if do_trim {
        // The worker's snapshot is dropped (its return value is the task just handled),
        // so the exclusive `&mut db` borrow is free. Salsa only trims at a revision
        // boundary, so without this a whole-corpus sweep accumulates every file's memos
        // and OOMs (the budget upstream decides *whether* memory needs releasing; this
        // block is *how*). Also release the parser's thread-local green-node cache on
        // this thread (the worker released its own before returning).
        let db = state.analysis_host.raw_database_mut();
        if finished {
            // Deep final trim: the sweep's parse trees and lowered bodies are pure
            // batch working set nothing will read again, so this leaves a lean
            // post-sweep resident set instead of a full interactive window of dead
            // files. Mid-sweep trims deliberately stay on the interactive caps:
            // running the whole sweep on the shrunk profile was measured to cost
            // several percent of wall time (each trim evicts hot shared parses that
            // the next chunk re-derives) for only a marginal peak reduction.
            ide::sweep_lru_deep(db);
        } else {
            db.enforce_lru();
        }
        syntax::clear_shared_node_cache();
    }

    if finished {
        let (files, elapsed_ms) = {
            let plan = state.workspace_batch_plan.as_ref().expect("plan present");
            (plan.file_ids.len(), plan.started_at.elapsed().as_millis() as u64)
        };
        finalize_workspace_batch(state)?;
        state.workspace_batch_plan = None;
        tracing::info!(files, elapsed_ms, "workspace diagnostics batch complete");
    }

    Ok(())
}

/// Move the sweep cursor past one completed (or skipped) chunk and decide whether this
/// boundary trims: always on finish (final cleanup), otherwise only while over the
/// memory budget — immediately when interactive analysis is quiescent, else once the
/// deferral valve fills. Boundaries under budget skip the trim by design (the retained
/// memos accelerate later chunks) and reset the valve rather than counting toward it.
/// Pure state transition on the plan, so the scheduling matrix is testable with an
/// injected budget/quiescence signal.
fn advance_plan_cursor(
    plan: &mut crate::global_state::WorkspaceBatchPlan,
    over_budget: bool,
    quiescent: bool,
) -> (bool, bool) {
    plan.next_chunk += 1;
    plan.chunk_retries = 0;
    if over_budget {
        plan.chunks_since_trim += 1;
    } else {
        plan.chunks_since_trim = 0;
    }
    let finished = plan.next_chunk >= plan.num_chunks;
    let do_trim = finished
        || (over_budget
            && (quiescent || plan.chunks_since_trim >= WORKSPACE_BATCH_FORCE_TRIM_CHUNKS));
    if do_trim {
        plan.chunks_since_trim = 0;
    }
    (finished, do_trim)
}

/// Reconcile a fully completed sweep: any file we had pushed but that this sweep never
/// reported is gone from scope (deleted or the scope narrowed), so clear its stale
/// diagnostics. Then reset the reported-set for the next sweep.
fn finalize_workspace_batch(state: &mut GlobalState) -> Result<()> {
    let stale: Vec<lsp_types::Url> = state
        .batch_pushed
        .keys()
        .filter(|uri| !state.batch_reported.contains(*uri))
        .cloned()
        .collect();
    for uri in stale {
        state.batch_pushed.remove(&uri);
        let params =
            lsp_types::PublishDiagnosticsParams { uri, diagnostics: vec![], version: None };
        let notification = Notification::new("textDocument/publishDiagnostics".to_string(), params);
        state.sender.send(notification.into())?;
    }
    state.batch_reported.clear();
    Ok(())
}

/// Apply one chunk of the deferred whole-project batch (Stream B) on the event-loop
/// thread, where `mem_docs` and `batch_pushed` are authoritative. A file opened since
/// the batch started is skipped (the interactive stream owns it); otherwise the file is
/// pushed only when its diagnostics hash changed, and cleared when it went clean.
fn apply_workspace_batch_chunk(
    state: &mut GlobalState,
    generation: u64,
    items: Vec<crate::global_state::WorkspaceBatchItem>,
) -> Result<()> {
    // Drop a chunk from a superseded sweep (a reset or a newer batch bumped the
    // generation): applying it could republish diagnostics the current configuration
    // no longer includes.
    if generation != state.workspace_batch_generation {
        return Ok(());
    }
    for item in items {
        // Record every file this sweep reported, so the completion reconcile can clear
        // entries that vanished from scope (deleted / scope narrowed) between sweeps.
        state.batch_reported.insert(item.uri.clone());

        // Handoff: a file opened mid-batch is served live by the interactive stream.
        // Skipping here (and never recording it) keeps the two streams from
        // double-reporting; `handle_did_open` already cleared any earlier batch push.
        if state.mem_docs.contains(&item.uri) {
            continue;
        }

        if item.diagnostics.is_empty() {
            // Publish an empty report only to clear a file that previously had some;
            // never spam clean files that never carried batch diagnostics.
            if state.batch_pushed.remove(&item.uri).is_some() {
                let params = lsp_types::PublishDiagnosticsParams {
                    uri: item.uri,
                    diagnostics: vec![],
                    version: None,
                };
                let notification =
                    Notification::new("textDocument/publishDiagnostics".to_string(), params);
                state.sender.send(notification.into())?;
            }
            continue;
        }

        // Diff-push: republish only when the diagnostics hash moved since the last batch.
        if state.batch_pushed.get(&item.uri) == Some(&item.result_id) {
            continue;
        }
        state.batch_pushed.insert(item.uri.clone(), item.result_id);
        let params = lsp_types::PublishDiagnosticsParams {
            uri: item.uri,
            diagnostics: item.diagnostics,
            version: None,
        };
        let notification = Notification::new("textDocument/publishDiagnostics".to_string(), params);
        state.sender.send(notification.into())?;
    }
    Ok(())
}

fn handle_vfs_msg(
    state: &mut GlobalState,
    files: Vec<(paths::AbsPathBuf, Option<Vec<u8>>)>,
    sync_to_salsa: bool,
) -> Result<()> {
    use std::sync::Arc;

    const VFS_WRITE_MINI_BATCH: usize = 16;

    let mut converted: Vec<(vfs::VfsPath, Option<Arc<str>>)> = Vec::with_capacity(files.len());
    let mut resolved = stdx::path_exclusion::ResolvedDirs::default();
    for (path, contents) in files {
        let std_path: &std::path::Path = path.as_ref();
        // Decided by the current exclusions, not by the configuration the loader was
        // running under when it produced the batch: a reload may have narrowed it since.
        if state.is_source_excluded(std_path, &mut resolved) {
            continue;
        }
        let vfs_path = vfs::VfsPath::new(std_path);

        // An open editor buffer is authoritative for unsaved content, so a
        // disk-sourced change here must not clobber its overlay.
        if state.is_open_document_path(std_path, &vfs_path) {
            continue;
        }

        let contents_str =
            contents.and_then(|bytes| base_db::decode_disk_bytes(&bytes).map(Arc::from));

        if project_model::is_bsl_source_path(std_path) {
            let mutated = if contents_str.is_some() {
                state.skipped_bsl.remove(&path)
            } else if state.skipped_bsl.insert(path.clone()) {
                tracing::warn!(
                    path = %path,
                    "BSL file unreadable by VFS; recorded as skipped",
                );
                true
            } else {
                false
            };
            if mutated {
                state.degraded_files_count = state.skipped_bsl.len();
            }
        }

        converted.push((vfs_path, contents_str));
    }

    for chunk in converted.chunks(VFS_WRITE_MINI_BATCH) {
        let mut vfs = state.vfs.write();
        for (vfs_path, contents_str) in chunk {
            vfs.set_file_contents(vfs_path.clone(), contents_str.clone());
        }
    }

    if !sync_to_salsa {
        // Sample the streaming high-water BEFORE draining: this batch's text was
        // just written to the VFS and prior batches have already drained, so the
        // pending text is ~one loader chunk and RSS is at its per-batch peak.
        let (_pending_files, pending_text_bytes) = state.vfs.read().pending_change_bytes();
        state.boot_peak_text_bytes = state.boot_peak_text_bytes.max(pending_text_bytes as u64);
        if let Some(rss) = crate::smoke::read_rss_bytes() {
            state.boot_peak_rss_bytes = state.boot_peak_rss_bytes.max(rss);
            tracing::debug!(
                rss_mb = rss / (1024 * 1024),
                pending_text_mb = pending_text_bytes / (1024 * 1024),
                "boot load: batch buffered, draining to Salsa"
            );
        }

        // Boot phase: drain THIS batch into Salsa now, suppressing the
        // metadata/config reload (the post-load `init_source_root` +
        // `bootstrap_metadata_substrate` rebuild the source root and metadata
        // substrate once). Closed files are recorded by content revision and
        // their text dropped, so the whole corpus never piles up as resident
        // text in `Vfs::changes` — draining per batch keeps the load-time text
        // high-water at ~one chunk instead of the entire corpus held until a
        // single end-of-load flush.
        state.process_changes(true);
        return Ok(());
    }

    let outcome = state.process_changes(false);

    // External (non-open) file changes — e.g. a `git pull` touching modules or
    // metadata — invalidate the Salsa cache but do not by themselves re-publish
    // diagnostics for documents the editor already has open. Reschedule those so
    // their diagnostics reflect the new disk state. Open files are filtered out of
    // this batch above (their editor buffer is authoritative), so this fires only
    // for genuine external changes, not for saves of open buffers.
    // A config reload may have changed or disabled the workspace-diagnostics scope. Tear
    // down the batch's published state first — this clears stale pushes even when the new
    // scope is `off`, where `mark_workspace_batch_dirty` below would no-op and leave them
    // lingering — then re-arm below if the feature is still enabled. Mirrors the save path.
    if outcome.config_file_changed || outcome.diagnostics_baseline_changed {
        state.reset_workspace_batch();
    }

    if outcome.config_file_changed || outcome.affects_open_documents {
        tracing::info!(
            config_file_changed = outcome.config_file_changed,
            "external change: scheduling diagnostics refresh for all open documents"
        );
        for uri in state.opened_document_uris() {
            crate::handlers::notification::schedule_diagnostics(state, &uri);
        }
        // A `.bsl`/metadata change on disk (e.g. `git pull`) can alter diagnostics for
        // closed in-scope files too; re-arm the deferred batch so its coverage — not
        // just open documents — reflects the new disk state.
        state.mark_workspace_batch_dirty();
        // The same external batch may be a checkout/pull that moved the git state
        // the vendor-diff scope was computed against.
        state.request_scope_rebuild();
        if outcome.diagnostics_baseline_changed {
            state.request_workspace_diagnostic_refresh();
        }
    }

    Ok(())
}

fn handle_request(state: &mut GlobalState, req: Request) -> Result<()> {
    use lsp_types::request::{
        CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare,
        CodeActionRequest, Completion, DocumentDiagnosticRequest, DocumentHighlightRequest,
        DocumentSymbolRequest, FoldingRangeRequest, Formatting, GotoDefinition, GotoTypeDefinition,
        HoverRequest, InlayHintRequest, OnTypeFormatting, PrepareRenameRequest, RangeFormatting,
        References, Rename, Request as _, SelectionRangeRequest, SemanticTokensFullRequest,
        SignatureHelpRequest, WorkspaceDiagnosticRequest, WorkspaceSymbolRequest,
    };

    tracing::info!("INCOMING REQUEST: method={} id={:?}", req.method, req.id);

    // A whole-workspace sweep can run for minutes; keep at most one in-flight so several do not
    // monopolize the shared latency pool and starve interactive requests. A newer sweep cancels
    // the previous one's token, which unwinds it (via Salsa) and frees its worker.
    let workspace_diagnostic_id =
        (req.method == WorkspaceDiagnosticRequest::METHOD).then(|| req.id.clone());
    if workspace_diagnostic_id.is_some() {
        if let Some(prev) = state.active_workspace_diagnostic.take() {
            if let Some(token) = state.request_tokens.get(&prev) {
                tracing::info!(prev_id = ?prev, "superseding previous workspace/diagnostic sweep");
                token.cancel();
            }
        }
    }

    if state.vfs_done && req.method == CallHierarchyPrepare::METHOD {
        state.call_hierarchy_index.ensure();
    }

    RequestDispatcher { req: Some(req), global_state: state }
        .on_sync_mut::<Shutdown>(|state, ()| {
            state.shutdown_requested = true;
            Ok(())
        })
        .on_latency::<GotoDefinition>(crate::handlers::handle_goto_definition)
        .on_latency::<GotoTypeDefinition>(crate::handlers::handle_type_definition)
        .on_latency::<References>(crate::handlers::handle_find_references)
        .on_latency::<PrepareRenameRequest>(crate::handlers::handle_prepare_rename)
        .on_latency::<Rename>(crate::handlers::handle_rename)
        .on_latency::<CallHierarchyPrepare>(crate::handlers::handle_prepare_call_hierarchy)
        .on_waiting_latency::<CallHierarchyIncomingCalls>(
            crate::handlers::handle_call_hierarchy_incoming,
        )
        .on_latency::<CallHierarchyOutgoingCalls>(crate::handlers::handle_call_hierarchy_outgoing)
        .on_latency::<InlayHintRequest>(crate::handlers::handle_inlay_hint)
        .on_latency::<WorkspaceSymbolRequest>(crate::handlers::handle_workspace_symbol)
        .on_latency::<SelectionRangeRequest>(crate::handlers::handle_selection_range)
        .on_latency::<DocumentHighlightRequest>(crate::handlers::handle_document_highlight)
        .on_latency::<FoldingRangeRequest>(crate::handlers::handle_folding_range)
        .on_latency::<HoverRequest>(crate::handlers::handle_hover)
        .on_latency::<Completion>(crate::handlers::handle_completion)
        .on_latency::<SemanticTokensFullRequest>(crate::handlers::handle_semantic_tokens_full)
        .on_latency::<DocumentSymbolRequest>(crate::handlers::handle_document_symbol)
        .on_latency::<CodeActionRequest>(crate::handlers::handle_code_action)
        .on_latency::<DocumentDiagnosticRequest>(crate::handlers::handle_document_diagnostic)
        .on_latency::<WorkspaceDiagnosticRequest>(crate::handlers::handle_workspace_diagnostic)
        .on_latency::<SignatureHelpRequest>(crate::handlers::handle_signature_help)
        .on_sync::<Formatting>(crate::handlers::handle_formatting)
        .on_sync::<RangeFormatting>(crate::handlers::handle_range_formatting)
        .on_sync::<OnTypeFormatting>(crate::handlers::handle_on_type_formatting)
        .finish();

    // Record the now-dispatched sweep as the active one (its token was registered by
    // `on_latency`), so the next sweep can supersede it. A declined request leaves a stale id
    // whose token lookup simply misses — harmless.
    if let Some(id) = workspace_diagnostic_id {
        state.active_workspace_diagnostic = Some(id);
    }

    Ok(())
}

fn handle_notification(state: &mut GlobalState, not: Notification) -> Result<()> {
    use lsp_types::notification::{
        Cancel, DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument,
        DidSaveTextDocument,
    };

    if not.method == Exit::METHOD {
        tracing::info!("Received exit notification");
        state.shutdown_requested = true;
        return Ok(());
    }

    NotificationDispatcher { not: Some(not), global_state: state }
        .on_sync_mut::<DidOpenTextDocument>(crate::handlers::handle_did_open)?
        .on_sync_mut::<DidChangeTextDocument>(crate::handlers::handle_did_change)?
        .on_sync_mut::<DidCloseTextDocument>(crate::handlers::handle_did_close)?
        .on_sync_mut::<DidSaveTextDocument>(crate::handlers::handle_did_save)?
        .on_sync_mut::<Cancel>(crate::handlers::handle_cancel)?
        .finish();

    Ok(())
}

fn server_capabilities(
    position_encoding: PositionEncoding,
    workspace_diagnostics_scope: project_model::WorkspaceDiagnosticsScope,
) -> ServerCapabilities {
    let legend = crate::lsp::semantic_tokens_legend();

    // Only advertise the pull diagnostic provider when the feature is enabled; when
    // `Off` the field stays `None` and the server behaves exactly as before (push-only).
    //
    // `workspace_diagnostics: false` mirrors rust-analyzer: advertising it makes
    // long-poll clients (Neovim) collapse to a single whole-workspace pull and stop
    // pulling the open document, so first paint waits for the entire sweep instead of
    // lighting up the edited file instantly. Whole-project coverage is delivered out of
    // band as a deferred push batch; the open document is served live via
    // `textDocument/diagnostic`. The `workspace/diagnostic` handler still answers an
    // explicit request (e.g. VS Code's background channel), it is just not solicited.
    let diagnostic_provider = workspace_diagnostics_scope.is_enabled().then(|| {
        DiagnosticServerCapabilities::Options(DiagnosticOptions {
            identifier: Some("bsl".to_string()),
            inter_file_dependencies: true,
            workspace_diagnostics: false,
            work_done_progress_options: WorkDoneProgressOptions { work_done_progress: None },
        })
    });

    ServerCapabilities {
        diagnostic_provider,

        position_encoding: Some(position_encoding.as_lsp_kind()),

        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),

        definition_provider: Some(lsp_types::OneOf::Left(true)),
        references_provider: Some(lsp_types::OneOf::Left(true)),

        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),

        completion_provider: Some(lsp_types::CompletionOptions {
            resolve_provider: None,
            trigger_characters: Some(vec![".".to_string()]),
            all_commit_characters: None,
            work_done_progress_options: WorkDoneProgressOptions { work_done_progress: None },
            completion_item: None,
        }),

        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                work_done_progress_options: WorkDoneProgressOptions { work_done_progress: None },
                legend,
                range: None,
                full: Some(SemanticTokensFullOptions::Bool(true)),
            },
        )),

        document_symbol_provider: Some(lsp_types::OneOf::Left(true)),

        document_highlight_provider: Some(lsp_types::OneOf::Left(true)),

        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),

        code_action_provider: Some(CodeActionProviderCapability::Options(
            lsp_types::CodeActionOptions {
                code_action_kinds: Some(vec![
                    lsp_types::CodeActionKind::QUICKFIX,
                    lsp_types::CodeActionKind::new(crate::lsp::to_proto::FIX_ALL_BSL),
                ]),
                work_done_progress_options: WorkDoneProgressOptions { work_done_progress: None },
                resolve_provider: None,
            },
        )),

        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: Some(vec![",".to_string()]),
            work_done_progress_options: WorkDoneProgressOptions { work_done_progress: None },
        }),

        document_formatting_provider: Some(lsp_types::OneOf::Left(true)),

        document_range_formatting_provider: Some(lsp_types::OneOf::Left(true)),

        document_on_type_formatting_provider: Some(lsp_types::DocumentOnTypeFormattingOptions {
            first_trigger_character: ";".to_string(),
            more_trigger_character: Some(vec!["\n".to_string()]),
        }),

        rename_provider: Some(lsp_types::OneOf::Right(lsp_types::RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions { work_done_progress: None },
        })),

        call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),

        inlay_hint_provider: Some(lsp_types::OneOf::Left(true)),

        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),

        selection_range_provider: Some(lsp_types::SelectionRangeProviderCapability::Simple(true)),

        type_definition_provider: Some(lsp_types::TypeDefinitionProviderCapability::Simple(true)),

        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::global_state::Task;
    use std::sync::Arc;

    #[test]
    fn baseline_reload_invalidates_diagnostics_before_pool_scheduling() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        let uri = crate::test_uri::file_uri("workspace/module.bsl");
        state.diagnostics_generation.insert(uri.clone(), 7);
        invalidate_diagnostics(&mut state, std::slice::from_ref(&uri));
        assert_eq!(state.diagnostics_generation[&uri], 8);
    }

    #[test]
    fn folding_range_limit_comes_from_the_client_capabilities() {
        let without = lsp_types::ClientCapabilities::default();
        assert_eq!(client_folding_range_limit(&without), None);

        let with_limit = lsp_types::ClientCapabilities {
            text_document: Some(lsp_types::TextDocumentClientCapabilities {
                folding_range: Some(lsp_types::FoldingRangeClientCapabilities {
                    range_limit: Some(5000),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(client_folding_range_limit(&with_limit), Some(5000));
    }

    #[test]
    fn test_server_capabilities() {
        let caps = server_capabilities(
            PositionEncoding::Utf8,
            project_model::WorkspaceDiagnosticsScope::Off,
        );

        assert_eq!(caps.position_encoding, Some(PositionEncoding::Utf8.as_lsp_kind()));

        match caps.text_document_sync {
            Some(TextDocumentSyncCapability::Kind(kind)) => {
                assert_eq!(kind, TextDocumentSyncKind::INCREMENTAL);
            }
            _ => panic!("Expected incremental text document sync"),
        }

        assert_eq!(caps.document_highlight_provider, Some(lsp_types::OneOf::Left(true)));
        assert_eq!(caps.folding_range_provider, Some(FoldingRangeProviderCapability::Simple(true)));

        match caps.rename_provider {
            Some(lsp_types::OneOf::Right(options)) => {
                assert_eq!(options.prepare_provider, Some(true));
            }
            _ => panic!("Expected rename provider with prepare support"),
        }

        assert_eq!(
            caps.call_hierarchy_provider,
            Some(lsp_types::CallHierarchyServerCapability::Simple(true))
        );

        assert_eq!(caps.inlay_hint_provider, Some(lsp_types::OneOf::Left(true)));

        assert_eq!(caps.workspace_symbol_provider, Some(lsp_types::OneOf::Left(true)));

        assert_eq!(
            caps.selection_range_provider,
            Some(lsp_types::SelectionRangeProviderCapability::Simple(true))
        );

        assert_eq!(
            caps.type_definition_provider,
            Some(lsp_types::TypeDefinitionProviderCapability::Simple(true))
        );
    }

    #[test]
    fn diagnostic_provider_gated_by_config() {
        // Off (default): push-only, no pull provider advertised.
        let off = server_capabilities(
            PositionEncoding::Utf8,
            project_model::WorkspaceDiagnosticsScope::Off,
        );
        assert!(off.diagnostic_provider.is_none());

        // Enabled: the single-document pull provider is advertised, but workspace pull
        // stays off so long-poll clients keep pulling the open document instead of
        // collapsing to a whole-workspace sweep. Whole-project coverage ships as a
        // deferred push batch, not a solicited workspace pull.
        let on = server_capabilities(
            PositionEncoding::Utf8,
            project_model::WorkspaceDiagnosticsScope::Extensions,
        );
        match on.diagnostic_provider {
            Some(DiagnosticServerCapabilities::Options(opts)) => {
                assert!(opts.inter_file_dependencies);
                assert!(!opts.workspace_diagnostics);
                assert_eq!(opts.identifier.as_deref(), Some("bsl"));
            }
            other => panic!("expected diagnostic options, got {other:?}"),
        }
    }

    #[test]
    fn test_position_encoding_prefers_utf8_when_client_supports_it() {
        let caps = lsp_types::ClientCapabilities {
            general: Some(lsp_types::GeneralClientCapabilities {
                position_encodings: Some(vec![
                    lsp_types::PositionEncodingKind::UTF8,
                    lsp_types::PositionEncodingKind::UTF16,
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(PositionEncoding::negotiate(&caps), PositionEncoding::Utf8);
    }

    #[test]
    fn vfs_done_finalize_replays_deferred_diagnostics_for_open_documents() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.init_empty_source_root();
        assert!(!state.vfs_done);
        // Model the boot window `set_workspace_root` opens on a real start.
        state.analysis_host.raw_database_mut().set_workspace_load_complete(false);

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("mod.bsl");
        std::fs::write(&path, "Процедура Тест() КонецПроцедуры").expect("write");
        let uri = lsp_types::Url::from_file_path(&path).unwrap();

        crate::handlers::notification::handle_did_open(
            &mut state,
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: "Процедура Тест() КонецПроцедуры".to_string(),
                },
            },
        )
        .unwrap();

        // Opened while the workspace was still loading: nothing scheduled yet.
        assert_eq!(state.diagnostics_generation.get(&uri), None);
        assert!(!state.diagnostics_tokens.contains_key(&uri));

        handle_loader_msg(
            &mut state,
            vfs::loader::Message::Progress {
                n_total: 1,
                n_done: vfs::loader::LoadingProgress::Finished,
                dir: None,
                config_version: 0,
            },
        )
        .unwrap();

        assert!(state.vfs_done);
        assert!(
            state.analysis_host.raw_database().workspace_load_complete(),
            "the finalize must reopen the whole-config loader gate"
        );
        assert_eq!(state.diagnostics_generation.get(&uri).copied(), Some(1));
        assert!(state.diagnostics_tokens.contains_key(&uri));
    }

    #[test]
    fn late_loader_batches_cannot_admit_files_under_the_current_exclusions() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.init_empty_source_root();
        let dir = tempfile::tempdir().unwrap();
        let hidden_dir = dir.path().join("generated");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        let hidden_bsl = hidden_dir.join("Hidden.bsl");
        let hidden_xml = hidden_dir.join("Hidden.xml");
        let allowed_bsl = dir.path().join("Allowed.bsl");
        let allowed_xml = dir.path().join("Allowed.xml");
        state.source_exclusions = project_model::ExcludedPaths::new([hidden_dir]);
        let abs = |path: std::path::PathBuf| paths::AbsPathBuf::assert_utf8(path);

        handle_loader_msg(
            &mut state,
            vfs::loader::Message::Loaded {
                files: vec![
                    (abs(hidden_bsl.clone()), Some(b"hidden".to_vec())),
                    (abs(allowed_bsl.clone()), Some(b"allowed".to_vec())),
                ],
            },
        )
        .unwrap();
        handle_loader_msg(
            &mut state,
            vfs::loader::Message::Changed {
                files: vec![(abs(hidden_bsl.clone()), Some(b"late".to_vec()))],
            },
        )
        .unwrap();
        handle_loader_msg(
            &mut state,
            vfs::loader::Message::WatchOnly {
                files: vec![abs(hidden_xml.clone()), abs(allowed_xml.clone())],
            },
        )
        .unwrap();

        let vfs = state.vfs.read();
        assert!(vfs.file_id(&vfs::VfsPath::new(hidden_bsl)).is_none());
        assert!(vfs.file_id(&vfs::VfsPath::new(hidden_xml)).is_none());
        assert!(vfs.file_id(&vfs::VfsPath::new(allowed_bsl)).is_some());
        assert!(vfs.file_id(&vfs::VfsPath::new(allowed_xml)).is_some());
    }

    #[test]
    fn late_external_preload_cannot_warm_an_excluded_file_id() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.init_empty_source_root();
        let dir = tempfile::tempdir().unwrap();
        let hidden_dir = dir.path().join("generated");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        let hidden = hidden_dir.join("Hidden.bsl");
        std::fs::write(&hidden, "Процедура Т() КонецПроцедуры").unwrap();
        let hidden_vfs = vfs::VfsPath::new(hidden.clone());
        let hidden_id = {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                hidden_vfs.clone(),
                Some(Arc::from("Процедура Т() КонецПроцедуры")),
            );
            vfs.file_id(&hidden_vfs).unwrap()
        };
        state.process_changes(false);

        handle_task(&mut state, Task::PreloadExternalFiles { files: vec![hidden_id] }).unwrap();
        let allowed_token = state
            .preload_external_tokens
            .remove(&hidden_id)
            .expect("positive control: an allowed file starts cache warming");
        allowed_token.cancel();

        state.source_exclusions = project_model::ExcludedPaths::new([hidden_dir]);
        handle_task(&mut state, Task::PreloadExternalFiles { files: vec![hidden_id] }).unwrap();

        assert!(
            state.preload_external_tokens.is_empty(),
            "a late preload task started analysis for an excluded stale FileId"
        );
    }

    #[test]
    fn handle_task_does_not_publish_diagnostics_for_a_closed_document() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);

        let uri = crate::test_uri::file_uri("gone.bsl");
        // A diagnostics task that finished with the current generation, but for a
        // document that is NOT open (closed before the result arrived).
        state.diagnostics_generation.insert(uri.clone(), 1);
        let task = crate::global_state::Task::DiagnosticsReady {
            uri,
            diagnostics: Vec::new(),
            generation: 1,
            completed_at: std::time::Instant::now(),
        };

        handle_task(&mut state, task).unwrap();

        assert!(
            receiver.try_recv().is_err(),
            "diagnostics for a closed document must not be published"
        );
    }

    #[test]
    fn call_hierarchy_index_state_task_messages_are_generation_checked() {
        // Given: a GlobalState build for one source root.
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        let source_root = base_db::SourceRootId(3);
        let first = Arc::new(hir::CallHierarchyReverseIndex::new());
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            1,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(1),
        ));

        // When: a worker succeeds, then an older worker reports after generation two starts.
        handle_task(
            &mut state,
            Task::CallHierarchyIndexBuilt { source_root, generation: 1, index: Arc::clone(&first) },
        )
        .expect("completion task must be handled");
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            2,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(2),
        ));
        handle_task(
            &mut state,
            Task::CallHierarchyIndexBuilt {
                source_root,
                generation: 1,
                index: Arc::new(hir::CallHierarchyReverseIndex::new()),
            },
        )
        .expect("stale completion task must be handled");

        // Then: the old complete value remains available and the stale task cannot replace it.
        assert!(Arc::ptr_eq(
            &state.call_hierarchy_index.current(source_root).expect("previous ready value remains"),
            &first,
        ));
        handle_task(
            &mut state,
            Task::CallHierarchyIndexFailed {
                source_root,
                generation: 2,
                reason: "replacement failed".to_owned(),
            },
        )
        .expect("failure task must be handled");
        assert_eq!(
            state.call_hierarchy_index.failure_reason(source_root, 2).as_deref(),
            Some("replacement failed")
        );
    }

    #[test]
    fn prepared_call_hierarchy_signal_schedules_one_source_root() {
        // Given: a prepared source-root generation after the VFS boot gate opens.
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.vfs_done = true;
        let source_root = base_db::SourceRootId(5);
        assert!(state.call_hierarchy_index.record_prepare(source_root, 1));

        // When: duplicate prepare signals reach the event loop.
        for _ in 0..2 {
            handle_task(
                &mut state,
                Task::CallHierarchyIndexBuildRequested { source_root, generation: 1 },
            )
            .expect("prepare signal must be handled");
        }

        // Then: scheduling coalesces to one pending root build.
        assert_eq!(state.call_hierarchy_index_rebuilds.len(), 1);
        assert!(state.call_hierarchy_index_rebuilds.contains(&source_root));
    }

    /// Decode the URI + diagnostic count of a `textDocument/publishDiagnostics`
    /// notification pulled off the client channel; panics on anything else.
    fn recv_publish(receiver: &crossbeam_channel::Receiver<Message>) -> (lsp_types::Url, usize) {
        match receiver.try_recv().expect("expected a publishDiagnostics notification") {
            Message::Notification(n) => {
                assert_eq!(n.method, "textDocument/publishDiagnostics");
                let params: lsp_types::PublishDiagnosticsParams =
                    serde_json::from_value(n.params).unwrap();
                (params.uri, params.diagnostics.len())
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    fn dummy_diagnostic() -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range::default(),
            message: "x".to_string(),
            ..Default::default()
        }
    }

    fn batch_item(
        uri: &lsp_types::Url,
        result_id: &str,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> crate::global_state::WorkspaceBatchItem {
        crate::global_state::WorkspaceBatchItem {
            uri: uri.clone(),
            result_id: result_id.to_string(),
            diagnostics,
        }
    }

    /// Install a minimal active sweep plan so `handle_task` will apply chunks: the
    /// completion path drops any chunk that arrives with no plan. `num_chunks` controls
    /// when the sweep finalizes — after that many non-aborted chunks the reconcile runs
    /// and the plan is cleared. Bumps the generation and marks the batch in flight, as a
    /// real dispatch does. The zero memory budget makes every boundary read as over
    /// budget, so trim scheduling is deterministic regardless of the test process' heap.
    fn install_test_plan(state: &mut crate::global_state::GlobalState, num_chunks: usize) {
        state.workspace_batch_generation = state.workspace_batch_generation.wrapping_add(1);
        state.batch_reported.clear();
        let analysis_guard = state.note_analysis_spawned();
        state.workspace_batch_in_flight = true;
        state.workspace_batch_plan = Some(crate::global_state::WorkspaceBatchPlan {
            generation: state.workspace_batch_generation,
            file_ids: std::sync::Arc::new(Vec::new()),
            file_paths: crate::frozen_context::FrozenFilePaths::default(),
            supports_code_description: state.supports_code_description,
            supports_diagnostic_tags: state.supports_diagnostic_tags,
            config: state.diagnostics_config().clone(),
            diagnostics_baseline: std::sync::Arc::clone(&state.diagnostics_baseline),
            workspace_root: state.workspace_root.clone(),
            position_encoding: state.position_encoding,
            chunk_size: 500,
            pool: None,
            next_chunk: 0,
            num_chunks,
            mem_budget_mb: 0,
            chunks_since_trim: 0,
            chunk_retries: 0,
            started_at: std::time::Instant::now(),
            analysis_guard,
        });
    }

    /// Wrap a computed chunk in the state's current generation so `handle_task` applies it.
    fn current_chunk(
        state: &crate::global_state::GlobalState,
        items: Vec<crate::global_state::WorkspaceBatchItem>,
    ) -> Task {
        Task::WorkspaceBatchChunk {
            generation: state.workspace_batch_generation,
            outcome: crate::global_state::WorkspaceBatchOutcome::Computed(items),
        }
    }

    #[test]
    fn workspace_batch_chunk_pushes_and_diffs() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        // A many-chunk sweep so applying several chunks never trips the finalize.
        install_test_plan(&mut state, 100);
        let uri = crate::test_uri::file_uri("a.bsl");

        // First report for a closed file with diagnostics: published and recorded.
        let chunk = current_chunk(&state, vec![batch_item(&uri, "h1", vec![dummy_diagnostic()])]);
        handle_task(&mut state, chunk).unwrap();
        assert_eq!(recv_publish(&receiver), (uri.clone(), 1));
        assert_eq!(state.batch_pushed.get(&uri).map(String::as_str), Some("h1"));

        // Same hash again: diff-push skips the republish.
        let chunk = current_chunk(&state, vec![batch_item(&uri, "h1", vec![dummy_diagnostic()])]);
        handle_task(&mut state, chunk).unwrap();
        assert!(receiver.try_recv().is_err(), "unchanged hash must not republish");

        // Changed hash: republished and the recorded hash advances.
        let chunk = current_chunk(
            &state,
            vec![batch_item(&uri, "h2", vec![dummy_diagnostic(), dummy_diagnostic()])],
        );
        handle_task(&mut state, chunk).unwrap();
        assert_eq!(recv_publish(&receiver), (uri.clone(), 2));
        assert_eq!(state.batch_pushed.get(&uri).map(String::as_str), Some("h2"));
    }

    #[test]
    fn workspace_batch_chunk_from_a_stale_generation_is_dropped() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.workspace_batch_generation = 5;
        let uri = crate::test_uri::file_uri("a.bsl");

        // A chunk tagged with an older generation (its sweep was superseded by a
        // config reset) must never publish or record anything.
        handle_task(
            &mut state,
            Task::WorkspaceBatchChunk {
                generation: 4,
                outcome: crate::global_state::WorkspaceBatchOutcome::Computed(vec![batch_item(
                    &uri,
                    "h1",
                    vec![dummy_diagnostic()],
                )]),
            },
        )
        .unwrap();
        assert!(receiver.try_recv().is_err(), "a stale-generation chunk must not publish");
        assert!(state.batch_pushed.is_empty(), "a stale-generation chunk must not record state");
    }

    #[test]
    fn workspace_batch_completion_clears_files_that_left_scope() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        // A single-chunk sweep: the one chunk both reports and finalizes.
        install_test_plan(&mut state, 1);
        let gone = crate::test_uri::file_uri("gone.bsl");
        let kept = crate::test_uri::file_uri("kept.bsl");

        // Two files were pushed by an earlier sweep.
        state.batch_pushed.insert(gone.clone(), "h1".to_string());
        state.batch_pushed.insert(kept.clone(), "h1".to_string());

        // The final (only) chunk reports just `kept` (unchanged hash → no republish);
        // reaching the cursor end reconciles: `gone`, never reported, is cleared.
        let chunk = current_chunk(&state, vec![batch_item(&kept, "h1", vec![dummy_diagnostic()])]);
        handle_task(&mut state, chunk).unwrap();
        assert_eq!(recv_publish(&receiver), (gone.clone(), 0), "a vanished file is cleared");
        assert!(!state.batch_pushed.contains_key(&gone));
        assert!(state.batch_pushed.contains_key(&kept), "a still-reported file is retained");
        assert!(state.workspace_batch_plan.is_none(), "a finalized sweep clears its plan");
        assert!(!state.workspace_batch_in_flight);
    }

    #[test]
    fn workspace_batch_abort_keeps_pushed_files_and_resumes_the_same_chunk() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        install_test_plan(&mut state, 5);
        let uri = crate::test_uri::file_uri("a.bsl");
        state.batch_pushed.insert(uri.clone(), "h1".to_string());

        // A cancelled chunk (a concurrent edit cancelled it) reported nothing: it must
        // NOT clear the pushed file, must keep the plan, and must NOT advance the cursor
        // (the same chunk is retried), while freeing the in-flight slot for the retry.
        let gen = state.workspace_batch_generation;
        handle_task(
            &mut state,
            Task::WorkspaceBatchChunk {
                generation: gen,
                outcome: crate::global_state::WorkspaceBatchOutcome::Cancelled,
            },
        )
        .unwrap();
        assert!(receiver.try_recv().is_err(), "a cancelled chunk must not clear pushed files");
        assert!(state.batch_pushed.contains_key(&uri));
        assert!(!state.workspace_batch_in_flight, "the in-flight slot is freed for the retry");
        let plan = state.workspace_batch_plan.as_ref().expect("the plan is kept for resume");
        assert_eq!(plan.next_chunk, 0, "an aborted chunk is retried, not skipped");
        assert!(!state.workspace_batch_dirty, "resume is driven by the plan, not a dirty re-arm");
    }

    #[test]
    fn workspace_batch_chunk_clears_a_file_that_went_clean() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        install_test_plan(&mut state, 100);
        let uri = crate::test_uri::file_uri("a.bsl");

        // Prime a pushed file, then re-report it clean: one empty publish clears it.
        state.batch_pushed.insert(uri.clone(), "h1".to_string());
        let chunk = current_chunk(&state, vec![batch_item(&uri, "h0", vec![])]);
        handle_task(&mut state, chunk).unwrap();
        assert_eq!(
            recv_publish(&receiver),
            (uri.clone(), 0),
            "clean file is cleared with an empty report"
        );
        assert!(!state.batch_pushed.contains_key(&uri));

        // A clean file that was never pushed produces no notification.
        let chunk = current_chunk(&state, vec![batch_item(&uri, "h0", vec![])]);
        handle_task(&mut state, chunk).unwrap();
        assert!(receiver.try_recv().is_err(), "a never-pushed clean file must not be published");
    }

    #[test]
    fn workspace_batch_chunk_skips_files_opened_since_the_batch_started() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        install_test_plan(&mut state, 100);
        let uri = crate::test_uri::file_uri("open.bsl");

        // The file was opened while the batch was computing: the interactive stream
        // owns it, so the batch result is dropped and never recorded.
        state.mem_docs.insert(uri.clone(), "Процедура Т() КонецПроцедуры".to_string(), 1);
        let chunk = current_chunk(&state, vec![batch_item(&uri, "h1", vec![dummy_diagnostic()])]);
        handle_task(&mut state, chunk).unwrap();
        assert!(receiver.try_recv().is_err(), "an open file must not be batch-pushed");
        assert!(!state.batch_pushed.contains_key(&uri));
    }

    #[test]
    fn workspace_batch_completion_clears_plan_without_rearming() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);

        // A completed single-chunk sweep clears the in-flight flag and the plan and does
        // not re-arm.
        install_test_plan(&mut state, 1);
        let chunk = current_chunk(&state, Vec::new());
        handle_task(&mut state, chunk).unwrap();
        assert!(!state.workspace_batch_in_flight);
        assert!(state.workspace_batch_plan.is_none(), "a completed sweep clears its plan");
        assert!(!state.workspace_batch_dirty, "a completed sweep does not re-arm");

        // A cancelled chunk keeps the plan (for resume) and frees the in-flight slot.
        install_test_plan(&mut state, 2);
        let gen = state.workspace_batch_generation;
        handle_task(
            &mut state,
            Task::WorkspaceBatchChunk {
                generation: gen,
                outcome: crate::global_state::WorkspaceBatchOutcome::Cancelled,
            },
        )
        .unwrap();
        assert!(!state.workspace_batch_in_flight);
        assert!(state.workspace_batch_plan.is_some(), "a cancelled chunk keeps the plan for retry");
        assert_eq!(
            state.workspace_batch_plan.as_ref().unwrap().next_chunk,
            0,
            "a cancelled chunk is retried, not skipped"
        );
    }

    #[test]
    fn workspace_batch_failed_chunk_is_skipped_not_retried() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);

        // A deterministically failing chunk advances the cursor (skipped) instead of
        // looping forever on it — a two-chunk sweep finalizes after one failure + one
        // more completion.
        install_test_plan(&mut state, 2);
        let gen = state.workspace_batch_generation;
        handle_task(
            &mut state,
            Task::WorkspaceBatchChunk {
                generation: gen,
                outcome: crate::global_state::WorkspaceBatchOutcome::Failed,
            },
        )
        .unwrap();
        assert_eq!(
            state.workspace_batch_plan.as_ref().expect("plan still active").next_chunk,
            1,
            "a failed chunk advances the cursor rather than retrying"
        );

        // The final chunk finalizes the sweep.
        let chunk = current_chunk(&state, Vec::new());
        handle_task(&mut state, chunk).unwrap();
        assert!(state.workspace_batch_plan.is_none(), "the sweep finalizes after the last chunk");
    }

    #[test]
    fn workspace_batch_propagated_panic_retries_within_budget_then_skips() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        // A two-chunk sweep; the first chunk keeps unwinding on a propagated panic.
        install_test_plan(&mut state, 2);
        let gen = state.workspace_batch_generation;

        // Within budget: the cursor holds (retry the same chunk).
        for _ in 0..WORKSPACE_BATCH_MAX_PROPAGATED_RETRIES {
            handle_task(
                &mut state,
                Task::WorkspaceBatchChunk {
                    generation: gen,
                    outcome: crate::global_state::WorkspaceBatchOutcome::Propagated,
                },
            )
            .unwrap();
            assert_eq!(
                state.workspace_batch_plan.as_ref().expect("plan active").next_chunk,
                0,
                "a propagated panic within budget retries the same chunk"
            );
        }

        // Budget exhausted: the chunk is skipped (cursor advances).
        handle_task(
            &mut state,
            Task::WorkspaceBatchChunk {
                generation: gen,
                outcome: crate::global_state::WorkspaceBatchOutcome::Propagated,
            },
        )
        .unwrap();
        assert_eq!(
            state.workspace_batch_plan.as_ref().expect("plan still active").next_chunk,
            1,
            "an over-budget propagated panic skips the chunk"
        );

        // A subsequent computed chunk reset the retry counter and finalizes the sweep.
        let chunk = current_chunk(&state, Vec::new());
        handle_task(&mut state, chunk).unwrap();
        assert!(state.workspace_batch_plan.is_none(), "the sweep finalizes after the last chunk");
    }

    #[test]
    fn interactive_quiescence_accounts_for_pending_requests() {
        use salsa::Database as _;
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);

        // Idle: the trim may run.
        assert!(state.interactive_analysis_quiescent());

        // A pending latency request (hover, the workspace/diagnostic pull, …) holds a db
        // snapshot the trim would block on, so quiescence must report false.
        let token = state.analysis_host.raw_database().cancellation_token();
        let id = lsp_server::RequestId::from(1);
        state.request_tokens.insert(id.clone(), token);
        assert!(
            !state.interactive_analysis_quiescent(),
            "a pending request must defer the batch trim"
        );

        state.request_tokens.remove(&id);
        assert!(state.interactive_analysis_quiescent());
    }

    #[test]
    fn workspace_batch_boundary_under_budget_skips_trim() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        install_test_plan(&mut state, 100);
        let plan = state.workspace_batch_plan.as_mut().unwrap();

        // Under budget: no trim even when fully quiescent, and the valve stays empty.
        assert_eq!(advance_plan_cursor(plan, false, true), (false, false));
        assert_eq!(plan.chunks_since_trim, 0);

        // Over budget but interactive analysis in flight: deferred once, then the
        // valve forces the trim on the next over-budget boundary.
        assert_eq!(advance_plan_cursor(plan, true, false), (false, false));
        assert_eq!(plan.chunks_since_trim, 1);
        assert_eq!(advance_plan_cursor(plan, true, false), (false, true));
        assert_eq!(plan.chunks_since_trim, 0);

        // Over budget and quiescent: trims immediately.
        assert_eq!(advance_plan_cursor(plan, true, true), (false, true));

        // Dropping back under budget resets a partially filled valve.
        assert_eq!(advance_plan_cursor(plan, true, false), (false, false));
        assert_eq!(plan.chunks_since_trim, 1);
        assert_eq!(advance_plan_cursor(plan, false, false), (false, false));
        assert_eq!(plan.chunks_since_trim, 0);
    }

    #[test]
    fn workspace_batch_final_boundary_trims_regardless_of_budget() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        install_test_plan(&mut state, 1);
        let plan = state.workspace_batch_plan.as_mut().unwrap();
        assert_eq!(
            advance_plan_cursor(plan, false, false),
            (true, true),
            "the finish must trim even under budget with interactive analysis in flight"
        );
    }

    /// A loaded, quiescent, over-budget state one idle tick in — the baseline every
    /// negative idle-trim case below perturbs.
    fn idle_state() -> crate::global_state::GlobalState {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.vfs_done = true;
        state.idle_ticks = 1;
        state
    }

    #[test]
    fn idle_trim_first_tick_is_shallow_and_latches() {
        let mut state = idle_state();
        assert_eq!(idle_trim_kind(&state, true), Some(IdleTrimKind::Shallow));

        // The shallow stage runs once per idle period; further ticks stay quiet
        // until the deep threshold.
        state.idle_shallow_trimmed = true;
        state.idle_ticks = 2;
        assert_eq!(idle_trim_kind(&state, true), None);
    }

    #[test]
    fn idle_trim_escalates_to_deep_and_latches() {
        let mut state = idle_state();
        state.idle_shallow_trimmed = true;
        state.idle_ticks = IDLE_TRIM_DEEP_TICKS;
        assert_eq!(idle_trim_kind(&state, true), Some(IdleTrimKind::Deep));

        state.idle_deep_trimmed = true;
        state.idle_ticks = IDLE_TRIM_DEEP_TICKS + 1;
        assert_eq!(idle_trim_kind(&state, true), None, "the deep stage runs once per idle period");
    }

    #[test]
    fn idle_trim_goes_straight_to_deep_when_shallow_never_ran() {
        // The server was busy (not quiescent) through the early ticks and only
        // drained past the deep threshold: deep subsumes shallow.
        let mut state = idle_state();
        state.idle_ticks = IDLE_TRIM_DEEP_TICKS;
        assert_eq!(idle_trim_kind(&state, true), Some(IdleTrimKind::Deep));
    }

    #[test]
    fn idle_trim_defers_to_gates() {
        // Under budget: retention is free performance, keep it.
        assert_eq!(idle_trim_kind(&idle_state(), false), None);

        // Not idle yet.
        let mut state = idle_state();
        state.idle_ticks = 0;
        assert_eq!(idle_trim_kind(&state, true), None);

        // Workspace not loaded.
        let mut state = idle_state();
        state.vfs_done = false;
        assert_eq!(idle_trim_kind(&state, true), None);

        // A latency request holds a db snapshot the trim would block on.
        use salsa::Database as _;
        let mut state = idle_state();
        let token = state.analysis_host.raw_database().cancellation_token();
        state.request_tokens.insert(lsp_server::RequestId::from(1), token);
        assert_eq!(idle_trim_kind(&state, true), None);

        // An active workspace-batch sweep has its own trim schedule, and its
        // in-flight chunk is discounted by the quiescence gate — the idle trim
        // must not run under it.
        let mut state = idle_state();
        install_test_plan(&mut state, 2);
        state.idle_ticks = IDLE_TRIM_DEEP_TICKS;
        assert_eq!(idle_trim_kind(&state, true), None);
    }

    #[test]
    fn workspace_batch_trimmed_boundaries_walk_and_finalize() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        install_test_plan(&mut state, 2);

        // A trimmed boundary (zero budget = always over) walks the real trim path —
        // eviction on the live database — and keeps the plan advancing.
        let chunk = current_chunk(&state, vec![]);
        handle_task(&mut state, chunk).unwrap();
        assert!(state.workspace_batch_plan.is_some());
        assert_eq!(state.workspace_batch_plan.as_ref().unwrap().next_chunk, 1);

        // The final boundary runs the deep trim (sweep caps for one eviction, then
        // restored) and finalizes the sweep.
        state.workspace_batch_in_flight = true;
        let chunk = current_chunk(&state, vec![]);
        handle_task(&mut state, chunk).unwrap();
        assert!(state.workspace_batch_plan.is_none(), "the sweep must finalize on the last chunk");
    }
}
