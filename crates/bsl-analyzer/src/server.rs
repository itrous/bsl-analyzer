use std::path::PathBuf;

use anyhow::{Context, Result};
use crossbeam_channel::{select, Receiver};
use lsp_server::{Connection, Message, Notification, Request, Response};
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
    global_state::{GlobalState, LatencyAttemptOutcome, RequestAttemptKey, RequestLifecyclePhase},
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
    state.supports_semantic_tokens_refresh =
        client_supports_semantic_tokens_refresh(&initialize_params.capabilities);

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
            // Keep the readiness gate closed: this workspace was rejected, so no
            // loader completion can establish a valid analysis substrate.
            state.update_diagnostics_config();
        }
    } else {
        tracing::warn!("No workspace root provided by client");
        state.update_diagnostics_config();
        // Without a workspace root no VFS loader runs, so no `Finished` event
        // will ever flip this flag — mark the (empty) workspace as loaded or
        // diagnostics would stay deferred forever.
        state.vfs_done = true;
    }

    let loop_result = run_event_loop(&mut state, &connection.receiver);
    cancel_latency_requests_on_shutdown(&mut state);
    loop_result?;

    tracing::info!("LSP server shutting down");
    Ok(())
}

/// Start an LSP session backed by an already-owned MCP workspace. The owner is
/// responsible for disk loading and metadata; this session creates an editor
/// storage only when the first admissible document opens.
pub fn main_loop_shared(
    connection: Connection,
    workspace: mcp_server::WorkspaceHandle,
) -> Result<()> {
    crate::panic_watch::install();
    let (initialize_id, initialize_value) =
        connection.initialize_start().context("Failed to start initialization")?;
    run_shared_initialized(connection, workspace, initialize_value, Some(initialize_id), 1, 0, None)
}

/// Resume a private proxy-rebound session without sending another `initialize`.
pub fn main_loop_resumed(
    connection: Connection,
    workspace: mcp_server::WorkspaceHandle,
    initialize_params: serde_json::Value,
    announced_capabilities: Option<serde_json::Value>,
    next_server_request_id: i32,
    progress_generation: u64,
) -> Result<()> {
    crate::panic_watch::install();
    run_shared_initialized(
        connection,
        workspace,
        initialize_params,
        None,
        next_server_request_id,
        progress_generation,
        announced_capabilities,
    )
}

fn run_shared_initialized(
    connection: Connection,
    workspace: mcp_server::WorkspaceHandle,
    initialize_value: serde_json::Value,
    initialize_id: Option<lsp_server::RequestId>,
    next_server_request_id: i32,
    progress_generation: u64,
    announced_capabilities: Option<serde_json::Value>,
) -> Result<()> {
    let raw_capabilities = initialize_value.get("capabilities").cloned().unwrap_or_default();
    let initialize_params: InitializeParams =
        serde_json::from_value(initialize_value).context("Failed to parse InitializeParams")?;
    let position_encoding = PositionEncoding::negotiate(&initialize_params.capabilities);
    let workspace_root = extract_workspace_root(&initialize_params);
    let early_config =
        workspace_root.as_ref().and_then(|root| match project_model::ProjectConfig::load(root) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(%error, "project config unreadable; capabilities use defaults");
                None
            }
        });
    let help_root =
        workspace_root.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    platform_help::bootstrap(early_config.as_ref(), &help_root);
    let diagnostics_scope =
        early_config.map(|config| config.features.workspace_diagnostics).unwrap_or_default();
    let pull_provider_announced =
        announced_capabilities.as_ref().map_or(diagnostics_scope.is_enabled(), |capabilities| {
            !capabilities["diagnosticProvider"].is_null()
        });

    if let Some(initialize_id) = initialize_id {
        let result = lsp_types::InitializeResult {
            capabilities: server_capabilities(position_encoding, diagnostics_scope),
            server_info: Some(lsp_types::ServerInfo {
                name: "bsl-analyzer".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        };
        let mut value = serde_json::to_value(result)?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "offsetEncoding".to_owned(),
                serde_json::json!([position_encoding.as_offset_encoding()]),
            );
        }
        connection
            .initialize_finish(initialize_id, value)
            .context("Failed to finish initialization")?;
    }

    let mut state = GlobalState::new_shared(connection.sender.clone(), workspace.clone());
    state.next_request_id.store(next_server_request_id.max(1), std::sync::atomic::Ordering::SeqCst);
    state.progress_generation = progress_generation;
    state.position_encoding = position_encoding;
    state.supports_insert_text_mode_adjust_indentation =
        client_supports_insert_text_mode_adjust_indentation(&initialize_params.capabilities);
    state.supports_code_description =
        client_supports_code_description(&initialize_params.capabilities);
    state.supports_diagnostic_tags =
        crate::lsp::to_proto::ClientTags::from_capabilities(&initialize_params.capabilities);
    state.supports_workspace_edit_document_changes =
        client_supports_workspace_edit_document_changes(&initialize_params.capabilities);
    state.folding_range_limit = client_folding_range_limit(&initialize_params.capabilities);
    state.pull_diagnostics_active = pull_provider_announced
        && client_supports_pull_diagnostics(&initialize_params.capabilities);
    state.supports_workspace_diagnostic_refresh =
        client_supports_workspace_diagnostic_refresh(&raw_capabilities);
    state.supports_semantic_tokens_refresh =
        client_supports_semantic_tokens_refresh(&initialize_params.capabilities);
    state.init_empty_source_root();
    state.lsp_locale = initialize_params.locale.as_deref().map(parse_lsp_locale);
    state.workspace_root = workspace_root.clone();
    state.vfs_done = matches!(
        workspace.readiness(),
        mcp_server::WorkspaceReadiness::Ready { .. }
            | mcp_server::WorkspaceReadiness::Frozen { .. }
    );
    if let Some(root) = workspace_root {
        if let mcp_server::WorkspaceRead::Ready(project) = workspace.project() {
            state.adopt_shared_project(root, project);
        }
    } else {
        tracing::warn!("No workspace root provided by client");
        state.vfs_done = true;
    }
    if refresh_shared_readiness(&mut state) {
        state.request_semantic_tokens_refresh();
    }

    let loop_result = run_event_loop(&mut state, &connection.receiver);
    cancel_latency_requests_on_shutdown(&mut state);
    loop_result?;
    Ok(())
}

/// Requests may still own Analysis clones when the client disconnects. Cancel their
/// tokens and drop all logical retry state before GlobalState goes away; no queued retry
/// is admitted after the event loop has ended.
fn cancel_latency_requests_on_shutdown(state: &mut GlobalState) {
    if let Some(stop) = &state.shared_bridge_stop {
        stop.store(true, std::sync::atomic::Ordering::Release);
    }
    for (_, token) in state.request_tokens.drain() {
        token.cancel();
    }
    state.request_token_owners.clear();
    state.request_lifecycles.clear();
    state.shared_request_stamps.clear();
    state.pending_latency_retries.clear();
    state.retry_dispatch = None;
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

/// Whether the client advertised `workspace.semanticTokens.refreshSupport`, the only grant the
/// spec gives the server to send `workspace/semanticTokens/refresh`.
fn client_supports_semantic_tokens_refresh(caps: &lsp_types::ClientCapabilities) -> bool {
    caps.workspace
        .as_ref()
        .and_then(|w| w.semantic_tokens.as_ref())
        .and_then(|st| st.refresh_support)
        .unwrap_or(false)
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

pub fn extract_workspace_root(params: &InitializeParams) -> Option<PathBuf> {
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

#[cfg(test)]
mod shared_backend_tests {
    use super::*;

    #[test]
    fn proxy_and_adapter_choose_the_same_explicit_root() {
        let root = tempfile::tempdir().unwrap();
        let explicit = root.path().join("explicit");
        let folder = root.path().join("folder");
        let params: InitializeParams = serde_json::from_value(serde_json::json!({
            "capabilities": {},
            "rootUri": lsp_types::Url::from_file_path(&explicit).unwrap(),
            "workspaceFolders": [{"uri": lsp_types::Url::from_file_path(folder).unwrap(), "name": "other"}],
        })).unwrap();
        assert_eq!(extract_workspace_root(&params), Some(explicit));
        let unsupported: InitializeParams = serde_json::from_value(serde_json::json!({
            "capabilities": {}, "rootUri": "untitled:workspace", "rootPath": root.path(),
        }))
        .unwrap();
        assert_eq!(extract_workspace_root(&unsupported), None);
    }

    /// A shared view change retires the in-flight diagnostics of every open document, not
    /// only of the one that was edited. The retired result is discarded as stale, so the
    /// document whose work was in flight must be queued again or its last edit is never
    /// published; a document whose diagnostics had already finished is left alone.
    #[test]
    fn a_shared_view_change_requeues_diagnostics_it_retired_in_flight() {
        use crate::global_state::Task;
        use salsa::Database as _;

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        let in_flight = crate::test_uri::file_uri("in_flight.bsl");
        let finished = crate::test_uri::file_uri("finished.bsl");
        state.mem_docs.insert(in_flight.clone(), "in flight".into(), 3);
        state.mem_docs.insert(finished.clone(), "finished".into(), 5);
        let token = state.analysis_host.raw_database().cancellation_token();
        state.diagnostics_tokens.insert(in_flight.clone(), token.clone());
        state.diagnostics_generation.insert(in_flight.clone(), 7);
        state
            .diagnostics_tokens
            .insert(finished.clone(), state.analysis_host.raw_database().cancellation_token());
        state.diagnostics_generation.insert(finished.clone(), 2);
        handle_task(
            &mut state,
            Task::DiagnosticsReady {
                uri: finished.clone(),
                diagnostics: Vec::new(),
                version: 5,
                generation: 2,
                completed_at: std::time::Instant::now(),
            },
        )
        .unwrap();

        state.mark_shared_view_changed();
        handle_task(
            &mut state,
            Task::DiagnosticsCancelled {
                uri: in_flight.clone(),
                version: 3,
                generation: 7,
                kind: salsa::Cancelled::Local,
                completed_at: std::time::Instant::now(),
            },
        )
        .unwrap();

        assert!(token.is_cancelled(), "the retired diagnostics kept running");
        assert_eq!(
            state.pending_diagnostics_uris,
            vec![in_flight],
            "only the document whose diagnostics were in flight is queued again"
        );
    }

    /// A retry the client cancels while it waits in the queue never finishes, so nothing
    /// else would drop the stamp its earlier attempt left behind.
    #[test]
    fn cancelling_a_queued_retry_drops_its_shared_stamp() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        let id = lsp_server::RequestId::from(17);
        state.request_lifecycles.insert(
            id.clone(),
            crate::global_state::RequestLifecycle {
                generation: 1,
                method: "textDocument/hover".into(),
                request: None,
                attempt: 1,
                phase: RequestLifecyclePhase::RetryQueued,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: None,
                client_cancelled: false,
            },
        );
        state.shared_request_stamps.insert(
            id.clone(),
            (0, salsa::plumbing::current_revision(state.analysis_host.raw_database())),
        );

        crate::handlers::notification::handle_cancel(
            &mut state,
            lsp_types::CancelParams { id: lsp_types::NumberOrString::Number(17) },
        )
        .unwrap();

        assert!(!state.request_lifecycles.contains_key(&id));
        assert!(state.shared_request_stamps.is_empty(), "the cancelled retry left its stamp");
    }

    #[test]
    fn shared_stamp_rejects_a_success_from_an_old_view_and_preserves_client_cancel() {
        for client_cancelled in [false, true] {
            let (sender, receiver) = crossbeam_channel::unbounded();
            let mut state = GlobalState::new(sender);
            let id = lsp_server::RequestId::from(17);
            let key = RequestAttemptKey { id: id.clone(), generation: 1, attempt: 1 };
            state.request_lifecycles.insert(
                id.clone(),
                crate::global_state::RequestLifecycle {
                    generation: 1,
                    method: "textDocument/rename".into(),
                    request: None,
                    attempt: 1,
                    phase: RequestLifecyclePhase::Running,
                    retry_origin_revision: None,
                    retry_eligible: false,
                    origin: None,
                    client_cancelled,
                },
            );
            state.shared_request_stamps.insert(
                id.clone(),
                (0, salsa::plumbing::current_revision(state.analysis_host.raw_database())),
            );
            state.shared_request_generation = 1;
            let outcome = LatencyAttemptOutcome::Response(Response::new_ok(
                id.clone(),
                serde_json::json!({"stale": true}),
            ));
            handle_latency_request_finished(&mut state, key.clone(), outcome).unwrap();
            let Message::Response(response) = receiver.recv().unwrap() else {
                panic!("expected response")
            };
            assert_eq!(
                response.error.unwrap().code,
                if client_cancelled {
                    lsp_server::ErrorCode::RequestCanceled as i32
                } else {
                    lsp_server::ErrorCode::ContentModified as i32
                }
            );
            assert!(state.shared_request_stamps.is_empty());
            handle_latency_request_finished(
                &mut state,
                key,
                LatencyAttemptOutcome::Response(Response::new_ok(id, serde_json::Value::Null)),
            )
            .unwrap();
            assert!(receiver.try_recv().is_err());
        }
    }
}

fn hold_shared_test_gate(variable: &str) {
    if std::env::var_os("BSL_SHARED_BACKEND_MARKERS").is_none() {
        return;
    }
    let Some(gate) = std::env::var_os(variable).map(PathBuf::from) else { return };
    if std::fs::create_dir_all(&gate).is_err() {
        return;
    }
    let entered = gate.join("entered");
    let Ok(_marker) = std::fs::OpenOptions::new().write(true).create_new(true).open(entered) else {
        return;
    };
    let release = gate.join("release");
    // Held until the test that set the gate releases it, however long that takes on the
    // machine running it; a gate never released is a hang the test runner reports.
    while !release.is_file() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn run_event_loop(state: &mut GlobalState, receiver: &Receiver<Message>) -> Result<()> {
    loop {
        select! {
            recv(receiver) -> msg => {
                state.note_loop_activity();
                refresh_diagnostics_baseline(state);
                handle_lsp_msg(state, msg?)?;
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
            }

            default(IDLE_TRIM_TICK) => {
                handle_idle_tick(state);
            }
        }

        // Every wake services each ready source; a continuous request or task
        // producer cannot starve the other source or the disk writer.
        for task in state.task_pool.receiver.clone().try_iter().take(16) {
            handle_task(state, task)?;
        }
        if let Ok(message) = state.loader_receiver.try_recv() {
            handle_loader_msg(state, message)?;
        }
        for message in receiver.try_iter().take(16) {
            handle_lsp_msg(state, message)?;
        }

        if state.shutdown_requested {
            state.call_hierarchy_index.shutdown();
            break;
        }

        admit_pending_latency_retries(state)?;
        state.schedule_shared_seed();
        if state.shared_seed_running {
            hold_shared_test_gate("BSL_TEST_SHARED_LSP_EVENT_GATE");
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
    // Without an editor view, shared requests run on the owner's store, which the
    // cancellation above cannot reach; a new request generation retries their answers.
    if state.analysis_host.is_shared() {
        state.shared_request_generation = state.shared_request_generation.wrapping_add(1);
    }
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

    state.analysis_host.enforce_lru(kind == IdleTrimKind::Deep);
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
        // The server's own requests (refreshes, progress creation) are fire-and-forget: a client
        // that answers them is following the protocol, and only a refusal is worth a warning.
        Message::Response(resp) => match &resp.error {
            Some(error) => tracing::warn!(id = ?resp.id, ?error, "client refused a server request"),
            None => tracing::debug!(id = ?resp.id, "client answered a server request"),
        },
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
        vfs::loader::Message::Loaded { files } => {
            let count = files.len();
            handle_vfs_msg(state, files, state.vfs_done)?;
            tracing::debug!(count, vfs_done = state.vfs_done, "streamed VFS batch");
        }
        vfs::loader::Message::Changed { files } => {
            for (path, contents) in &files {
                let path: &std::path::Path = path.as_ref();
                if contents.is_none() && !path.exists() && project_model::is_bsl_source_path(path) {
                    let file_id = state.vfs.read().file_id(&vfs::VfsPath::new(path.to_path_buf()));
                    let mut invalidated_uris: Vec<_> = file_id
                        .into_iter()
                        .flat_map(|file_id| {
                            state
                                .open_file_uri_ids
                                .iter()
                                .filter(move |&(_, &open_file_id)| open_file_id == file_id)
                                .map(|(uri, _)| uri.clone())
                        })
                        .collect();
                    if let Ok(uri) = lsp_types::Url::from_file_path(path) {
                        invalidated_uris.push(uri);
                    }
                    invalidated_uris.sort_unstable();
                    invalidated_uris.dedup();
                    for uri in invalidated_uris {
                        state.mem_docs.invalidate_identity(&uri);
                    }
                }
            }
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
                    // The cancellation above dropped in-flight semantic-tokens requests.
                    state.request_semantic_tokens_refresh();
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
                    state.request_semantic_tokens_refresh();
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

fn refresh_shared_readiness(state: &mut GlobalState) -> bool {
    let first_ready_generation = state.shared_owner_generation.is_none();
    let readiness = state.analysis_host.owner().map(|owner| owner.readiness());
    match readiness {
        Some(mcp_server::WorkspaceReadiness::Ready { generation }) => {
            state.shared_configuration_error = None;
            if state.shared_owner_generation != Some(generation) {
                state.shared_owner_generation = Some(generation);
                state.mark_shared_view_changed();
            }
            if let Some(view) = state.analysis_host.view() {
                state.shared_seed_queued =
                    !(state.shared_view_seeded && view.generation() == generation);
            }
            state.vfs_done = state.open_files.is_empty() || state.shared_view_seeded;
        }
        Some(mcp_server::WorkspaceReadiness::Frozen { generation, error }) => {
            if state.shared_configuration_error.as_ref() != Some(&error) {
                state.show_error_message(error.clone());
                state.shared_configuration_error = Some(error);
            }
            state.shared_owner_generation = Some(generation);
            state.shared_seed_queued = state.analysis_host.view().is_some()
                && (!state.shared_view_seeded
                    || state
                        .analysis_host
                        .view()
                        .is_some_and(|view| view.generation() != generation));
            state.vfs_done = state.open_files.is_empty() || state.shared_view_seeded;
        }
        Some(mcp_server::WorkspaceReadiness::RejectedScope(message)) => {
            if !state.project_rejected {
                state.show_error_message(message);
            }
            state.project_rejected = true;
            state.vfs_done = false;
            state.shared_seed_queued = false;
        }
        Some(mcp_server::WorkspaceReadiness::Failed(message)) => {
            state.show_error_message(message);
            state.vfs_done = false;
            state.shared_seed_queued = false;
        }
        _ => state.vfs_done = false,
    }
    first_ready_generation && state.vfs_done && state.shared_owner_generation.is_some()
}

/// Adopt the project the owner accepted, and bring the open documents in line with its
/// exclusions: the ones it now covers leave the view, the ones it no longer covers return.
fn apply_shared_project(
    state: &mut GlobalState,
    root: std::path::PathBuf,
    project: project_model::Project,
) {
    state.adopt_shared_project(root, project);
    let excluded: Vec<_> = state
        .opened_document_uris()
        .into_iter()
        .filter(|uri| crate::handlers::notification::is_excluded_document(state, uri))
        .collect();
    for uri in excluded {
        if let Ok(file_id) = crate::lsp::file_id(state, &uri) {
            if let Some(view) = state.analysis_host.view_mut() {
                view.close(file_id);
            }
            state.open_files.remove(&file_id);
        }
        crate::handlers::notification::withdraw_document_diagnostics(state, &uri);
    }
    readmit_shared_documents(state);
    if state.open_files.is_empty() {
        state.analysis_host.take_view();
        state.shared_view_epoch = state.shared_view_epoch.wrapping_add(1);
    }
}

/// Put back into the view the open documents the exclusions no longer cover, with the
/// editor's buffer as their text: one taken out by an earlier exclusion and one opened while
/// it was in force alike. Only didOpen otherwise puts a document into the view.
fn readmit_shared_documents(state: &mut GlobalState) {
    for uri in state.opened_document_uris() {
        if crate::handlers::notification::is_excluded_document(state, &uri) {
            continue;
        }
        if state.open_file_uri_ids.get(&uri).is_some_and(|id| state.open_files.contains(id)) {
            continue;
        }
        let Some(text) = state.mem_docs.get(&uri) else { continue };
        match state.vfs_file_for_url(&uri) {
            Ok(file_id) => {
                crate::handlers::notification::open_shared_document(state, &uri, file_id, &text)
            }
            Err(error) => {
                tracing::warn!(%uri, %error, "cannot readmit a no longer excluded document")
            }
        }
    }
}

pub(crate) fn handle_task(state: &mut GlobalState, task: crate::global_state::Task) -> Result<()> {
    use crate::global_state::Task;

    match task {
        Task::SharedWorkspaceChanged { generation } => {
            state.shared_generation_pending.store(false, std::sync::atomic::Ordering::Release);
            tracing::debug!(generation, "shared disk workspace generation changed");
            if let (Some(owner), Some(root)) =
                (state.analysis_host.owner().cloned(), state.workspace_root.clone())
            {
                if let mcp_server::WorkspaceRead::Ready(project) = owner.project() {
                    apply_shared_project(state, root, project);
                }
            }
            let changed = state.analysis_host.catch_up();
            if changed {
                state.shared_view_seeded = true;
            }
            // A new owner generation is a disk change, possibly a checkout that moved the
            // git state the vendor-diff scope was computed against.
            state.request_scope_rebuild();
            state.maybe_spawn_scope_build();
            let first_ready_generation = refresh_shared_readiness(state);
            if changed && state.vfs_done {
                crate::handlers::notification::refresh_after_shared_catch_up(state);
            } else if first_ready_generation {
                state.request_semantic_tokens_refresh();
            }
        }
        Task::SharedWorkspaceSeedReady { epoch } => {
            state.shared_seed_running = false;
            let prepared = {
                state
                    .shared_seed_snapshot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
            };
            if epoch == state.shared_view_epoch && !state.open_files.is_empty() {
                if let Some((prepared_epoch, view)) = prepared {
                    if prepared_epoch == epoch {
                        state.analysis_host.request_cancellation();
                        if let Some(current) = state.analysis_host.view_mut() {
                            if current.replace_from_prepared(view) {
                                state.shared_view_seeded = true;
                                state.mark_shared_view_changed();
                                if std::env::var_os("BSL_SHARED_BACKEND_MARKERS").is_some() {
                                    tracing::info!(
                                        target: "bsl_analyzer::shared_backend",
                                        event = "lsp_editor_view_seed_published",
                                        epoch,
                                        "shared LSP editor view seed published"
                                    );
                                }
                            }
                        }
                        state.update_diagnostics_config();
                        state.update_features_config();
                    }
                }
                state.analysis_host.catch_up();
                refresh_shared_readiness(state);
                if state.vfs_done {
                    for uri in state.opened_document_uris() {
                        crate::handlers::schedule_diagnostics(state, &uri);
                    }
                    state.request_semantic_tokens_refresh();
                }
            }
        }
        Task::DiagnosticsReady { uri, diagnostics, version, generation, completed_at } => {
            let current = state.diagnostics_generation.get(&uri).copied().unwrap_or(0);
            // The latest schedule has finished, so its token no longer marks work in flight.
            if generation == current {
                state.diagnostics_tokens.remove(&uri);
            }
            // Publish only the latest schedule for this uri (`== current`) and only
            // while it is still open — a result that finished after the document was
            // closed must not be published for a closed file.
            if generation == current && state.mem_docs.get_version(&uri) == Some(version) {
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
                let params = lsp_types::PublishDiagnosticsParams {
                    uri,
                    diagnostics,
                    version: Some(version),
                };
                let notification =
                    Notification::new("textDocument/publishDiagnostics".to_string(), params);
                state.sender.send(notification.into())?;
            } else {
                tracing::debug!(generation, current, "discarding stale diagnostics");
            }
        }
        Task::DiagnosticsCancelled { uri, version, generation, kind, completed_at } => {
            tracing::debug!(
                generation,
                ?kind,
                publish_delay_ms = completed_at.elapsed().as_millis() as u64,
                "diagnostics cancelled",
            );
            if state.diagnostics_generation.get(&uri) == Some(&generation) {
                state.diagnostics_tokens.remove(&uri);
            }
            // Batch LRU writes can cancel the latest open-buffer diagnostics too.
            if matches!(kind, salsa::Cancelled::PendingWrite | salsa::Cancelled::Local)
                && state.diagnostics_generation.get(&uri) == Some(&generation)
                && state.mem_docs.get_version(&uri) == Some(version)
            {
                state.enqueue_pending_diagnostics(uri);
            }
        }
        Task::DependenciesPreloaded { file_id, count } => {
            tracing::debug!(file_id = file_id.0, count, "dependencies preloaded");
            state.preload_tokens.remove(&file_id);
            state.preload_external_tokens.remove(&file_id);
        }
        Task::LatencyRequestFinished { key, outcome } => {
            handle_latency_request_finished(state, key, outcome)?;
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

const MAX_LATENCY_ATTEMPTS: u8 = 3;

fn handle_latency_request_finished(
    state: &mut GlobalState,
    key: RequestAttemptKey,
    mut outcome: LatencyAttemptOutcome,
) -> Result<()> {
    let Some(lifecycle) = state.request_lifecycles.get(&key.id) else {
        tracing::debug!(request_id = ?key.id, generation = key.generation, attempt = key.attempt, "discarding completion without a live request");
        return Ok(());
    };
    if lifecycle.generation != key.generation
        || lifecycle.attempt != key.attempt
        || lifecycle.phase != RequestLifecyclePhase::Running
    {
        tracing::debug!(
            request_id = ?key.id,
            generation = key.generation,
            attempt = key.attempt,
            "discarding stale latency completion"
        );
        return Ok(());
    }

    if state.request_token_owners.get(&key.id) == Some(&key) {
        state.request_token_owners.remove(&key.id);
        state.request_tokens.remove(&key.id);
    }

    let client_cancelled = lifecycle.client_cancelled;
    let method = lifecycle.method.clone();
    if client_cancelled {
        finish_latency_request(
            state,
            &key,
            Response::new_err(
                key.id.clone(),
                lsp_server::ErrorCode::RequestCanceled as i32,
                "request cancelled".to_string(),
            ),
        );
        return Ok(());
    }

    if matches!(&outcome, LatencyAttemptOutcome::Response(response) if response.error.is_none()) {
        if let Some(&(generation, revision)) = state.shared_request_stamps.get(&key.id) {
            if generation != state.shared_request_generation {
                outcome = LatencyAttemptOutcome::Cancelled {
                    kind: salsa::Cancelled::PendingWrite,
                    revision,
                };
            }
        }
    }

    let response = match outcome {
        LatencyAttemptOutcome::Response(response) => response,
        LatencyAttemptOutcome::Cancelled { kind: salsa::Cancelled::Local, .. } => {
            Response::new_err(
                key.id.clone(),
                lsp_server::ErrorCode::RequestCanceled as i32,
                "request cancelled".to_string(),
            )
        }
        LatencyAttemptOutcome::Cancelled { kind: salsa::Cancelled::PendingWrite, revision } => {
            let current_revision = state.analysis_host.request_revision();
            match queue_latency_retry(
                state,
                &key,
                &method,
                "pending-write",
                revision,
                current_revision,
                false,
            ) {
                RetryQueueResult::Queued => return Ok(()),
                RetryQueueResult::Refused(refusal) => Response::new_err(
                    key.id.clone(),
                    lsp_server::ErrorCode::ContentModified as i32,
                    refusal.message().to_string(),
                ),
            }
        }
        LatencyAttemptOutcome::Panicked { revision } => {
            tracing::error!(request_id = ?key.id, method, attempt = key.attempt,
                revision = ?revision, "propagated LSP unwind followed a real panic");
            Response::new_err(
                key.id.clone(),
                lsp_server::ErrorCode::InternalError as i32,
                "internal handler panic".to_string(),
            )
        }
        LatencyAttemptOutcome::Cancelled { kind: salsa::Cancelled::PropagatedPanic, revision } => {
            let current_revision = state.analysis_host.request_revision();
            match queue_latency_retry(
                state,
                &key,
                &method,
                "propagated-after-write",
                revision,
                current_revision,
                true,
            ) {
                RetryQueueResult::Queued => return Ok(()),
                RetryQueueResult::Refused(refusal) => Response::new_err(
                    key.id.clone(),
                    lsp_server::ErrorCode::ContentModified as i32,
                    refusal.message().to_string(),
                ),
            }
        }
        LatencyAttemptOutcome::Cancelled { .. } => Response::new_err(
            key.id.clone(),
            lsp_server::ErrorCode::RequestCanceled as i32,
            "request cancelled".to_string(),
        ),
    };
    finish_latency_request(state, &key, response);
    Ok(())
}

fn request_origin_matches(
    state: &GlobalState,
    origin: &crate::global_state::RequestOriginGuard,
) -> bool {
    let Ok(uri_path) = origin.uri.to_file_path() else {
        return false;
    };
    let Some(project) = state.project.as_ref() else {
        return false;
    };
    let current_source_roots = project.source_roots();
    let open_file_path = state
        .open_file_uri_ids
        .get(&origin.uri)
        .filter(|file_id| state.open_files.contains(file_id))
        .and_then(|file_id| {
            let vfs = state.vfs.read();
            (file_id.0 < vfs.num_file_ids())
                .then(|| vfs.file_path(*file_id).as_path().to_path_buf())
        });
    let path =
        crate::workspace::canonicalize_shared_path(open_file_path.as_deref().unwrap_or(&uri_path));
    let source_eligible = project_model::is_bsl_source_path(&path)
        && current_source_roots
            .iter()
            .any(|root| path.starts_with(crate::workspace::canonicalize_shared_path(root)))
        && !project.is_source_excluded(&uri_path);
    state.mem_docs.identity(&origin.uri) == Some(origin.identity)
        && state.workspace_root == origin.workspace_root
        && current_source_roots == origin.source_roots
        && source_eligible
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryRefusal {
    MissingOwner,
    StaleOwner,
    ClientCancelled,
    PolicyDisabled,
    RequestUnavailable,
    AttemptLimit,
    /// No open document was captured when the request arrived, so there is nothing
    /// that proves the original position still means the same thing.
    UntrackedOrigin,
    InvalidOrigin,
    NoNewRevision,
}

impl RetryRefusal {
    fn as_str(self) -> &'static str {
        match self {
            Self::MissingOwner => "missing-owner",
            Self::StaleOwner => "stale-owner",
            Self::ClientCancelled => "client-cancelled",
            Self::PolicyDisabled => "policy-disabled",
            Self::RequestUnavailable => "request-unavailable",
            Self::AttemptLimit => "attempt-limit-exhausted",
            Self::UntrackedOrigin => "untracked-origin",
            Self::InvalidOrigin => "invalid-origin",
            Self::NoNewRevision => "no-new-revision",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::AttemptLimit => "content modified: retry limit exhausted",
            Self::UntrackedOrigin => "content modified: document is not open in the project",
            Self::InvalidOrigin => "content modified: document origin changed",
            Self::PolicyDisabled => "content modified: automatic retry is not permitted",
            Self::NoNewRevision => "content modified: no newer workspace revision",
            Self::ClientCancelled => "request cancelled",
            Self::MissingOwner | Self::StaleOwner => "content modified: request ownership changed",
            Self::RequestUnavailable => "content modified: original request is unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryQueueResult {
    Queued,
    Refused(RetryRefusal),
}

fn queue_latency_retry(
    state: &mut GlobalState,
    key: &RequestAttemptKey,
    method: &str,
    reason: &str,
    old_revision: salsa::Revision,
    current_revision: salsa::Revision,
    requires_new_revision: bool,
) -> RetryQueueResult {
    let Some(lifecycle) = state.request_lifecycles.get(&key.id) else {
        tracing::info!(request_id = ?key.id, method, attempt = key.attempt, reason, refusal = "missing-owner", old_revision = ?old_revision, current_revision = ?current_revision, "latency retry refused");
        return RetryQueueResult::Refused(RetryRefusal::MissingOwner);
    };
    let origin_matches =
        lifecycle.origin.as_ref().map(|origin| request_origin_matches(state, origin));
    let refusal = if lifecycle.generation != key.generation
        || lifecycle.attempt != key.attempt
        || lifecycle.phase != RequestLifecyclePhase::Running
    {
        Some(RetryRefusal::StaleOwner)
    } else if lifecycle.client_cancelled {
        Some(RetryRefusal::ClientCancelled)
    } else if !lifecycle.retry_eligible {
        Some(RetryRefusal::PolicyDisabled)
    } else if lifecycle.request.is_none() {
        Some(RetryRefusal::RequestUnavailable)
    } else if lifecycle.attempt >= MAX_LATENCY_ATTEMPTS {
        Some(RetryRefusal::AttemptLimit)
    } else if origin_matches.is_none() {
        Some(RetryRefusal::UntrackedOrigin)
    } else if origin_matches == Some(false) {
        Some(RetryRefusal::InvalidOrigin)
    } else if requires_new_revision && current_revision <= old_revision {
        Some(RetryRefusal::NoNewRevision)
    } else {
        None
    };
    if let Some(refusal) = refusal {
        tracing::info!(request_id = ?key.id, method, attempt = key.attempt, reason, refusal = refusal.as_str(), old_revision = ?old_revision, current_revision = ?current_revision, "latency retry refused");
        return RetryQueueResult::Refused(refusal);
    }

    let attempt = key.attempt + 1;
    let lifecycle = state.request_lifecycles.get_mut(&key.id).expect("checked above");
    lifecycle.attempt = attempt;
    lifecycle.phase = RequestLifecyclePhase::RetryQueued;
    lifecycle.retry_origin_revision = Some(old_revision);
    let retry = RequestAttemptKey { id: key.id.clone(), generation: key.generation, attempt };
    state.pending_latency_retries.push_back(retry);
    tracing::info!(
        request_id = ?key.id,
        method,
        attempt,
        reason,
        old_revision = ?old_revision,
        current_revision = ?current_revision,
        "queued fresh LSP request attempt"
    );
    RetryQueueResult::Queued
}

fn finish_latency_request(state: &mut GlobalState, key: &RequestAttemptKey, response: Response) {
    if close_latency_lifecycle(state, key) {
        state.respond(response);
    }
}

/// Retire the lifecycle of `key`'s attempt, or report that the attempt is no longer the
/// current one and its terminal response is stale.
fn close_latency_lifecycle(state: &mut GlobalState, key: &RequestAttemptKey) -> bool {
    let current = state.request_lifecycles.get(&key.id).is_some_and(|lifecycle| {
        lifecycle.generation == key.generation && lifecycle.attempt == key.attempt
    });
    if !current {
        tracing::debug!(request_id = ?key.id, generation = key.generation, attempt = key.attempt, "discarding stale terminal response");
        return false;
    }

    state
        .pending_latency_retries
        .retain(|pending| pending.id != key.id || pending.generation != key.generation);
    state.request_lifecycles.remove(&key.id);
    state.shared_request_stamps.remove(&key.id);
    if state
        .request_token_owners
        .get(&key.id)
        .is_some_and(|owner| owner.generation == key.generation)
    {
        state.request_token_owners.remove(&key.id);
        state.request_tokens.remove(&key.id);
    }
    if state.active_workspace_diagnostic.as_ref() == Some(&key.id) {
        state.active_workspace_diagnostic = None;
    }
    tracing::info!(
        request_id = ?key.id,
        generation = key.generation,
        attempt = key.attempt,
        "finalized LSP request"
    );
    true
}

pub(crate) fn admit_pending_latency_retries(state: &mut GlobalState) -> Result<()> {
    while state.task_pool.pool.has_capacity() {
        let Some(key) = state.pending_latency_retries.pop_front() else {
            break;
        };
        let Some(lifecycle) = state.request_lifecycles.get(&key.id) else {
            continue;
        };
        if lifecycle.generation != key.generation
            || lifecycle.attempt != key.attempt
            || lifecycle.phase != RequestLifecyclePhase::RetryQueued
        {
            continue;
        }
        let client_cancelled = lifecycle.client_cancelled;
        let origin_matches =
            lifecycle.origin.as_ref().is_some_and(|origin| request_origin_matches(state, origin));
        let old_revision = lifecycle.retry_origin_revision;
        let request = lifecycle.request.clone();
        let method = lifecycle.method.clone();
        if client_cancelled {
            finish_latency_request(
                state,
                &key,
                Response::new_err(
                    key.id.clone(),
                    lsp_server::ErrorCode::RequestCanceled as i32,
                    "request cancelled".to_string(),
                ),
            );
            continue;
        }
        if !origin_matches {
            let current_revision = state.analysis_host.request_revision();
            let refusal = RetryRefusal::InvalidOrigin;
            tracing::info!(
                request_id = ?key.id,
                method,
                attempt = key.attempt,
                reason = "origin-changed-before-admission",
                refusal = refusal.as_str(),
                old_revision = ?old_revision.unwrap_or(current_revision),
                current_revision = ?current_revision,
                "latency retry refused"
            );
            finish_latency_request(
                state,
                &key,
                Response::new_err(
                    key.id.clone(),
                    lsp_server::ErrorCode::ContentModified as i32,
                    refusal.message().to_string(),
                ),
            );
            continue;
        }
        let request = request.expect("eligible retry retains request");
        tracing::info!(
            request_id = ?key.id,
            method,
            attempt = key.attempt,
            "admitting fresh LSP request attempt"
        );
        state.retry_dispatch = Some(key.clone());
        if let Err(error) = handle_request(state, request) {
            state.retry_dispatch = None;
            tracing::warn!(request_id = ?key.id, attempt = key.attempt, ?error, "retry dispatch failed");
            finish_latency_request(
                state,
                &key,
                Response::new_err(
                    key.id.clone(),
                    lsp_server::ErrorCode::ContentModified as i32,
                    "content modified".to_string(),
                ),
            );
        } else if state.retry_dispatch.take().is_some() {
            // The dispatcher answered without admitting the attempt (declined while loading
            // or busy), so only its lifecycle is left to close.
            close_latency_lifecycle(state, &key);
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
        if finished {
            // Deep final trim: the sweep's parse trees and lowered bodies are pure
            // batch working set nothing will read again, so this leaves a lean
            // post-sweep resident set instead of a full interactive window of dead
            // files. Mid-sweep trims deliberately stay on the interactive caps:
            // running the whole sweep on the shrunk profile was measured to cost
            // several percent of wall time (each trim evicts hot shared parses that
            // the next chunk re-derives) for only a marginal peak reduction.
            state.analysis_host.enforce_lru(true);
        } else {
            state.analysis_host.enforce_lru(false);
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
        let opened_file_id = crate::lsp::file_id(state, &item.uri).ok();
        if state.mem_docs.contains(&item.uri)
            || opened_file_id.is_some_and(|file_id| state.open_files.contains(&file_id))
        {
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
        if state
            .batch_pushed
            .get(&item.uri)
            .is_some_and(|pushed| pushed.result_id == item.result_id)
        {
            continue;
        }
        state.batch_pushed.insert(
            item.uri.clone(),
            crate::global_state::BatchPush { result_id: item.result_id, file_id: opened_file_id },
        );
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
        // Semantic-tokens requests issued during the reindex were declined or
        // cancelled; ask the client to pull them again, as the initial-load
        // finalize does.
        state.request_semantic_tokens_refresh();
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
        .on_retryable_latency::<GotoDefinition>(crate::handlers::handle_goto_definition)
        .on_retryable_latency::<GotoTypeDefinition>(crate::handlers::handle_type_definition)
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
        .on_syntax_latency::<SelectionRangeRequest>(crate::handlers::handle_selection_range)
        .on_latency::<DocumentHighlightRequest>(crate::handlers::handle_document_highlight)
        .on_syntax_latency::<FoldingRangeRequest>(crate::handlers::handle_folding_range)
        .on_retryable_latency::<HoverRequest>(crate::handlers::handle_hover)
        .on_retryable_latency::<Completion>(crate::handlers::handle_completion)
        .on_latency::<SemanticTokensFullRequest>(crate::handlers::handle_semantic_tokens_full)
        .on_syntax_latency::<DocumentSymbolRequest>(crate::handlers::handle_document_symbol)
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
        Cancel, DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument,
        DidOpenTextDocument, DidSaveTextDocument,
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
        .on_sync_mut::<DidChangeWatchedFiles>(crate::handlers::handle_did_change_watched_files)?
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
                    lsp_types::CodeActionKind::REFACTOR_REWRITE,
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
    use lsp_types::request::Request as _;
    use salsa::Database as _;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static G1_CANCEL_FLAG_BASELINE: AtomicU64 = AtomicU64::new(0);
    static G1_CANCEL_OBSERVED: AtomicU64 = AtomicU64::new(0);
    static G1_READER_ENTERED: AtomicBool = AtomicBool::new(false);
    static G1_READER_SAW_WRITE: AtomicBool = AtomicBool::new(false);
    static G1_TEST_LOCK: Mutex<()> = Mutex::new(());

    mod lsp_live_vfs_propagated_tests {
        include!("lsp_live_vfs_propagated_tests.rs");
    }

    fn g1_with_tracing<T>(body: impl FnOnce() -> T) -> T {
        let _serial = G1_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(std::io::stderr)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, body)
    }

    fn g1_definition_waiting_for_vfs_write(
        ctx: &crate::frozen_context::LatencyRequestContext,
        params: lsp_types::GotoDefinitionParams,
    ) -> anyhow::Result<Option<lsp_types::GotoDefinitionResponse>> {
        g1_wait_for_salsa_cancellation(ctx);
        crate::handlers::handle_goto_definition(ctx, params)
    }

    fn g1_wait_for_salsa_cancellation(ctx: &crate::frozen_context::LatencyRequestContext) {
        G1_READER_ENTERED.store(true, Ordering::Release);
        let baseline = G1_CANCEL_FLAG_BASELINE.load(Ordering::Acquire);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ctx
            .analysis
            .database()
            .salsa_event_global()
            .is_some_and(|events| events.set_cancellation <= baseline)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "VFS writer never set Salsa's cancellation flag"
            );
            std::thread::yield_now();
        }
        let observed = ctx
            .analysis
            .database()
            .salsa_event_global()
            .expect("G1 installs an event-enabled database in its fixture")
            .set_cancellation;
        assert!(observed > baseline);
        G1_CANCEL_OBSERVED.store(observed, Ordering::Release);
        tracing::info!(g1_marker = "salsa_did_set_cancellation", baseline, observed);
        G1_READER_SAW_WRITE.store(true, Ordering::Release);
    }

    fn g1_hover_waiting_for_vfs_write(
        ctx: &crate::frozen_context::LatencyRequestContext,
        params: lsp_types::HoverParams,
    ) -> anyhow::Result<Option<lsp_types::Hover>> {
        g1_wait_for_salsa_cancellation(ctx);
        crate::handlers::handle_hover(ctx, params)
    }

    fn g1_type_definition_waiting_for_vfs_write(
        ctx: &crate::frozen_context::LatencyRequestContext,
        params: lsp_types::GotoDefinitionParams,
    ) -> anyhow::Result<Option<lsp_types::GotoDefinitionResponse>> {
        g1_wait_for_salsa_cancellation(ctx);
        crate::handlers::handle_type_definition(ctx, params)
    }

    fn g1_completion_waiting_for_vfs_write(
        ctx: &crate::frozen_context::LatencyRequestContext,
        params: lsp_types::CompletionParams,
    ) -> anyhow::Result<Option<lsp_types::CompletionResponse>> {
        g1_wait_for_salsa_cancellation(ctx);
        crate::handlers::handle_completion(ctx, params)
    }

    fn g1_definition(
        ctx: &crate::frozen_context::LatencyRequestContext,
        params: lsp_types::GotoDefinitionParams,
    ) -> anyhow::Result<Option<lsp_types::GotoDefinitionResponse>> {
        crate::handlers::handle_goto_definition(ctx, params)
    }

    fn g1_definition_request(id: i32, uri: lsp_types::Url) -> lsp_server::Request {
        use lsp_types::request::{GotoDefinition, Request as _};
        let params = lsp_types::GotoDefinitionParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                position: lsp_types::Position { line: 4, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        lsp_server::Request::new(
            lsp_server::RequestId::from(id),
            GotoDefinition::METHOD.to_owned(),
            serde_json::to_value(params).unwrap(),
        )
    }

    fn g1_json_request(id: i32, method: &str, params: serde_json::Value) -> lsp_server::Request {
        lsp_server::Request::new(lsp_server::RequestId::from(id), method.to_owned(), params)
    }

    fn g1_position(text: &str, line: u32, needle: &str, after: bool) -> lsp_types::Position {
        let source_line = text.lines().nth(line as usize).expect("fixture line exists");
        let byte_offset = source_line.find(needle).expect("fixture token exists")
            + usize::from(after) * needle.len();
        lsp_types::Position {
            line,
            character: source_line[..byte_offset].encode_utf16().count() as u32,
        }
    }

    fn g1_cancel_then_retry<R>(
        state: &mut crate::global_state::GlobalState,
        request: lsp_server::Request,
        handler: fn(
            &crate::frozen_context::LatencyRequestContext,
            R::Params,
        ) -> anyhow::Result<R::Result>,
        writer_path: &std::path::Path,
        writer_text: &str,
        client_receiver: &crossbeam_channel::Receiver<Message>,
    ) -> serde_json::Value
    where
        R: lsp_types::request::Request,
        R::Params: serde::de::DeserializeOwned + std::fmt::Debug + Send + 'static,
        R::Result: serde::Serialize + Send + 'static,
    {
        G1_READER_ENTERED.store(false, Ordering::Release);
        G1_READER_SAW_WRITE.store(false, Ordering::Release);
        let baseline = state
            .analysis_host
            .raw_database()
            .salsa_event_global()
            .expect("G1 test DB records Salsa events")
            .set_cancellation;
        G1_CANCEL_FLAG_BASELINE.store(baseline, Ordering::Release);
        let id = request.id.clone();
        let task_receiver = state.task_pool.receiver.clone();
        RequestDispatcher { req: Some(request), global_state: state }
            .on_retryable_latency::<R>(handler)
            .finish();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !G1_READER_ENTERED.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "semantic reader did not enter");
            std::thread::yield_now();
        }
        std::fs::write(writer_path, writer_text).expect("write unrelated source file");
        handle_loader_msg(
            state,
            vfs::loader::Message::Changed {
                files: vec![(
                    paths::AbsPathBuf::assert_utf8(writer_path.to_path_buf()),
                    Some(writer_text.as_bytes().to_vec()),
                )],
            },
        )
        .expect("live VFS writer applies unrelated-file change");
        assert!(G1_READER_SAW_WRITE.load(Ordering::Acquire));

        let response = g1_wait_for_result(state, &task_receiver, client_receiver, id);
        assert!(response.error.is_none(), "fresh retry must succeed: {response:?}");
        response.result.expect("successful retry has a semantic result")
    }

    struct G1ReleasePoolProbe(Arc<AtomicBool>);

    impl Drop for G1ReleasePoolProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn g1_three_real_writer_attempts(
        state: &mut crate::global_state::GlobalState,
        request: lsp_server::Request,
        writer_path: &std::path::Path,
        client_receiver: &crossbeam_channel::Receiver<Message>,
    ) -> lsp_server::Response {
        let id = request.id.clone();
        let (entered, release) = crate::handlers::dispatch::g1_arm_pending_write_gate(&id, 3);
        let mut task_receiver = state.task_pool.receiver.clone();
        handle_lsp_msg(state, Message::Request(request))
            .expect("production handler accepts request");

        let mut writer_complete = 0;
        for attempt in 1..=3 {
            assert_eq!(
                entered
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("production attempt reaches controlled test gate"),
                attempt
            );
            let text = format!(
                "Процедура WriterProcedure()\n// successive write {attempt}\nКонецПроцедуры\n"
            );
            std::fs::write(writer_path, &text).expect("write new contents for actual VFS event");
            handle_loader_msg(
                state,
                vfs::loader::Message::Changed {
                    files: vec![(
                        paths::AbsPathBuf::assert_utf8(writer_path.to_path_buf()),
                        Some(text.as_bytes().to_vec()),
                    )],
                },
            )
            .expect("production VFS path applies the writer");
            writer_complete += 1;
            tracing::info!(
                g1_marker = "successive_real_writer_complete",
                request_id = ?id,
                attempt,
                writer_complete
            );

            loop {
                let task = task_receiver
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("cancelled attempt must return a production task");
                handle_task(state, task).expect("production event loop handles the attempt task");
                let retry_queued = state
                    .request_lifecycles
                    .get(&id)
                    .is_some_and(|lifecycle| lifecycle.phase == RequestLifecyclePhase::RetryQueued);
                let terminal = !state.request_lifecycles.contains_key(&id);
                if retry_queued || terminal {
                    break;
                }
            }

            if attempt < 3 {
                assert!(state.request_lifecycles.contains_key(&id));
                assert_eq!(state.pending_latency_retries.len(), 1);

                if attempt == 1 {
                    let original_task_pool = std::mem::replace(
                        &mut state.task_pool,
                        crate::task_pool::TaskPool::new_with_workers(1),
                    );
                    task_receiver = state.task_pool.receiver.clone();
                    let pool_entered = crossbeam_channel::bounded(1);
                    let (pool_entered_tx, pool_entered_rx) = pool_entered;
                    let pool_release = Arc::new(AtomicBool::new(false));
                    let _release_pool_on_unwind = G1ReleasePoolProbe(Arc::clone(&pool_release));
                    let release_flag = Arc::clone(&pool_release);
                    state
                        .task_pool
                        .pool
                        .try_spawn(move || {
                            pool_entered_tx.send(()).unwrap();
                            while !release_flag.load(Ordering::Acquire) {
                                std::thread::yield_now();
                            }
                            Task::AnalysisProgressTick { epoch: u64::MAX }
                        })
                        .expect("one-worker pool accepts blocker");
                    pool_entered_rx
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .expect("capacity blocker occupies the only worker");
                    let mut blockers = Vec::new();
                    for _ in 0..state.task_pool.pool.capacity().unwrap() {
                        let release_flag = Arc::clone(&pool_release);
                        state
                            .task_pool
                            .pool
                            .try_spawn(move || {
                                while !release_flag.load(Ordering::Acquire) {
                                    std::thread::yield_now();
                                }
                                Task::AnalysisProgressTick { epoch: u64::MAX }
                            })
                            .expect("fill the bounded admission queue");
                        blockers.push(());
                    }
                    assert!(!state.task_pool.pool.has_capacity());
                    assert!(!state.interactive_analysis_quiescent());
                    admit_pending_latency_retries(state).unwrap();
                    assert_eq!(state.pending_latency_retries.len(), 1);
                    assert!(entered.try_recv().is_err(), "capacity gate holds attempt two");
                    tracing::info!(
                        g1_marker = "retry_capacity_held",
                        queued = true,
                        quiescent = false,
                        worker_slots = 1
                    );
                    pool_release.store(true, Ordering::Release);
                    for _ in 0..=blockers.len() {
                        let task = task_receiver
                            .recv_timeout(std::time::Duration::from_secs(10))
                            .expect("released blocker emits its task");
                        handle_task(state, task).expect("apply capacity-probe task result");
                    }
                    assert!(state.task_pool.pool.has_capacity());
                    let (drained_tasks, completed_barriers) =
                        g1_drain_task_pool_to_fixed_point(state, &task_receiver);
                    tracing::info!(
                        g1_marker = "retry_capacity_probe_pool_drained",
                        drained_tasks,
                        completed_barriers,
                    );
                    state.task_pool = original_task_pool;
                    task_receiver = state.task_pool.receiver.clone();
                    assert!(
                        state.task_pool.pool.capacity().unwrap_or(0) / 4 >= 2,
                        "restore the original multi-worker request pool after capacity probe"
                    );
                    tracing::info!(
                        g1_marker = "capacity_probe_pool_restored",
                        workers_at_least = 2,
                        capacity = state.task_pool.pool.capacity().unwrap_or(0)
                    );
                }
                admit_pending_latency_retries(state).expect("event loop admits next attempt");
            }
        }

        release.store(true, Ordering::Release);
        crate::handlers::dispatch::g1_disarm_pending_write_gate(&id);
        assert_eq!(writer_complete, 3, "three actual writers overlapped the one logical request");
        assert!(entered.try_recv().is_err(), "no fourth semantic attempt was started");
        assert!(!state.request_lifecycles.contains_key(&id));
        assert!(!state.request_tokens.contains_key(&id));
        assert!(state.pending_latency_retries.is_empty());

        loop {
            if let Ok(Message::Response(response)) = client_receiver.try_recv() {
                if response.id == id {
                    return response;
                }
                panic!("unexpected response {:?} while checking retry exhaustion", response.id);
            }
            let task = task_receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("terminal ContentModified response was not sent");
            handle_task(state, task).expect("production event loop drains final attempt");
        }
    }

    fn g1_wait_for_result(
        state: &mut crate::global_state::GlobalState,
        receiver: &crossbeam_channel::Receiver<Task>,
        client_receiver: &crossbeam_channel::Receiver<Message>,
        id: lsp_server::RequestId,
    ) -> lsp_server::Response {
        loop {
            while let Ok(message) = client_receiver.try_recv() {
                if let Message::Response(response) = message {
                    assert_eq!(
                        response.id, id,
                        "unexpected client response while awaiting request"
                    );
                    return response;
                }
            }
            let task = receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or_else(|error| {
                    let lifecycle = state.request_lifecycles.get(&id);
                    let owner = state.request_token_owners.get(&id);
                    let revision =
                        salsa::plumbing::current_revision(state.analysis_host.raw_database());
                    let salsa_events = state
                        .analysis_host
                        .raw_database()
                        .salsa_event_global()
                        .map(|events| events.set_cancellation);
                    panic!(
                        "request task timed out: {error}; id={id:?} lifecycle={lifecycle:?} owner={owner:?} pool_capacity={} queued_tasks={} pending_retries={} revision={revision:?} salsa_cancellations={salsa_events:?}",
                        state.task_pool.pool.has_capacity(),
                        receiver.len(),
                        state.pending_latency_retries.len(),
                    );
                });
            handle_task(state, task).expect("event loop must apply the request task");
            admit_pending_latency_retries(state).expect("event loop must admit queued retry");
        }
    }

    /// Hang guard for a response that the test has already caused to be sent.
    /// Not an expected wait: notifications may precede the response, so the
    /// reader must not sleep for a particular arrival order.
    const CLIENT_RESPONSE_HANG_GUARD: std::time::Duration =
        test_utils::wait::ceiling(std::time::Duration::from_secs(10));

    /// Bound of a wait for an event that a regression would never deliver. A watchdog
    /// against hangs, not a latency measure: a slow machine must never reach it.
    const HANG_WATCHDOG: std::time::Duration =
        test_utils::wait::ceiling(std::time::Duration::from_secs(60));

    /// Read client messages until the `Response` for `id`.
    ///
    /// `textDocument/publishDiagnostics`, `window/logMessage`, and other
    /// notifications share this channel with responses. Their order relative to
    /// the response depends on which task the event loop finished first, so a
    /// reader that requires the next message to be a `Response` flakes. Server
    /// requests are skipped for the same reason. A `Response` for a different
    /// id is still a test failure.
    fn take_matching_response(
        message: Message,
        id: &lsp_server::RequestId,
    ) -> Option<lsp_server::Response> {
        match message {
            Message::Response(response) if response.id == *id => Some(response),
            Message::Response(response) => {
                panic!("unexpected response {:?} while waiting for {id:?}", response.id)
            }
            Message::Notification(_) | Message::Request(_) => None,
        }
    }

    /// The response was sent on this thread before the read. It must already be
    /// queued: waiting would hide a send that is no longer synchronous. Notifications
    /// already on the channel are still skipped.
    fn recv_queued_client_response(
        receiver: &crossbeam_channel::Receiver<Message>,
        id: lsp_server::RequestId,
    ) -> lsp_server::Response {
        loop {
            match receiver.try_recv() {
                Ok(message) => {
                    if let Some(response) = take_matching_response(message, &id) {
                        return response;
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    panic!("response {id:?} was not already on the client channel")
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    panic!("client channel closed while reading response {id:?}")
                }
            }
        }
    }

    fn recv_client_response(
        receiver: &crossbeam_channel::Receiver<Message>,
        id: lsp_server::RequestId,
    ) -> lsp_server::Response {
        let deadline = std::time::Instant::now() + CLIENT_RESPONSE_HANG_GUARD;
        loop {
            // `recv_timeout(0)` still returns a message that is already queued, so a
            // stream of notifications would never hit the timeout error. Leave once
            // the hang guard has elapsed, even if the channel is not empty.
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for response {id:?}");
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match receiver.recv_timeout(remaining) {
                Ok(message) => {
                    if let Some(response) = take_matching_response(message, &id) {
                        return response;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for response {id:?}")
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    panic!("client channel closed while waiting for response {id:?}")
                }
            }
        }
    }

    /// The previous client read: the next message must already be a `Response`.
    /// Kept so the notification-first order can keep proving this assertion panics.
    fn legacy_next_message_must_be_response(receiver: &crossbeam_channel::Receiver<Message>) {
        let lsp_server::Message::Response(_response) = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("queued cancellation must answer once")
        else {
            panic!("queued cancellation produced a non-response")
        };
    }

    fn queue_publish_diagnostics_then_cancelled_response(
        id: i32,
    ) -> crossbeam_channel::Receiver<Message> {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let uri = lsp_types::Url::parse("file:///Caller.bsl").expect("fixture uri");
        sender
            .send(Message::Notification(Notification::new(
                "textDocument/publishDiagnostics".to_string(),
                lsp_types::PublishDiagnosticsParams { uri, diagnostics: vec![], version: None },
            )))
            .expect("queue diagnostics notification");
        sender
            .send(Message::Notification(Notification::new(
                "window/logMessage".to_string(),
                lsp_types::LogMessageParams {
                    typ: lsp_types::MessageType::INFO,
                    message: "diagnostics published".to_owned(),
                },
            )))
            .expect("queue log notification");
        sender
            .send(Message::Response(Response::new_err(
                lsp_server::RequestId::from(id),
                lsp_server::ErrorCode::RequestCanceled as i32,
                "request cancelled".to_owned(),
            )))
            .expect("queue cancel response");
        receiver
    }

    fn g1_drain_task_pool_to_fixed_point(
        state: &mut crate::global_state::GlobalState,
        receiver: &crossbeam_channel::Receiver<Task>,
    ) -> (usize, usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut drained_tasks = 0;
        let mut completed_barriers = 0;
        loop {
            let capacity = state.task_pool.pool.capacity().expect("bounded task pool");
            assert_eq!(capacity % 4, 0, "task queue capacity is workers × 4");
            let workers = capacity / 4;
            let (ack_tx, ack_rx) = crossbeam_channel::bounded(workers);
            match state.task_pool.try_broadcast(move || {
                ack_tx
                    .send(std::thread::current().id())
                    .expect("quiescence caller receives every worker acknowledgement");
            }) {
                Ok(()) => {
                    let mut worker_ids = std::collections::HashSet::with_capacity(workers);
                    for _ in 0..workers {
                        let remaining =
                            deadline.saturating_duration_since(std::time::Instant::now());
                        let worker_id = ack_rx
                            .recv_timeout(remaining)
                            .expect("every task-pool worker acknowledges its barrier callback");
                        assert!(
                            worker_ids.insert(worker_id),
                            "each task-pool worker acknowledges exactly once"
                        );
                    }
                    assert_eq!(worker_ids.len(), workers);
                    completed_barriers += 1;
                    tracing::info!(
                        g1_marker = "task_pool_worker_barrier_acknowledged",
                        workers,
                        completed_barriers,
                    );
                    let mut handled = false;
                    while let Ok(task) = receiver.try_recv() {
                        handle_task(state, task).expect("quiescence drain applies task results");
                        drained_tasks += 1;
                        handled = true;
                    }
                    if !handled {
                        return (drained_tasks, completed_barriers);
                    }
                }
                Err(crate::task_pool::TrySpawnError::Full) => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let task = receiver
                        .recv_timeout(remaining)
                        .expect("drain task completions while waiting for pool barrier capacity");
                    handle_task(state, task).expect("quiescence drain applies task result");
                    drained_tasks += 1;
                }
                Err(crate::task_pool::TrySpawnError::Disconnected) => {
                    panic!("task pool disconnected before its idle barrier")
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "task pool did not reach a quiescent barrier"
            );
        }
    }

    fn g1_wait_for_task_pool_quiescence(
        state: &mut crate::global_state::GlobalState,
        receiver: &crossbeam_channel::Receiver<Task>,
    ) {
        let (drained_tasks, completed_barriers) =
            g1_drain_task_pool_to_fixed_point(state, receiver);
        assert!(state.request_lifecycles.is_empty());
        assert!(state.request_tokens.is_empty());
        assert!(!state.workspace_batch_in_flight);
        assert!(state.workspace_batch_plan.is_none());
        assert!(state.interactive_analysis_quiescent());
        tracing::info!(
            g1_marker = "task_pool_quiescent_before_workspace_teardown",
            drained_tasks,
            completed_barriers,
        );
    }

    fn g1_queue_valid_retry_for_session_boundary(
        state: &mut crate::global_state::GlobalState,
        id: lsp_server::RequestId,
        uri: lsp_types::Url,
    ) -> (crate::global_state::RequestAttemptKey, salsa::CancellationToken) {
        let numeric_id = serde_json::to_value(&id)
            .expect("serialize request id")
            .as_i64()
            .and_then(|id| i32::try_from(id).ok())
            .expect("test request id must be numeric");
        let request = g1_definition_request(numeric_id, uri.clone());
        let origin = crate::global_state::RequestOriginGuard {
            identity: state.mem_docs.identity(&uri).expect("open request document"),
            uri,
            workspace_root: state.workspace_root.clone(),
            source_roots: state.project.as_ref().expect("loaded test project").source_roots(),
        };
        let generation = 41;
        let first_attempt =
            crate::global_state::RequestAttemptKey { id: id.clone(), generation, attempt: 1 };
        state.request_lifecycles.insert(
            id.clone(),
            crate::global_state::RequestLifecycle {
                generation,
                method: lsp_types::request::GotoDefinition::METHOD.to_owned(),
                request: Some(request),
                attempt: 1,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: Some(origin),
                client_cancelled: false,
            },
        );
        assert!(request_origin_matches(
            state,
            state.request_lifecycles[&id].origin.as_ref().unwrap()
        ));
        let revision = salsa::plumbing::current_revision(state.analysis_host.raw_database());
        assert_eq!(
            queue_latency_retry(
                state,
                &first_attempt,
                lsp_types::request::GotoDefinition::METHOD,
                "session-boundary-control",
                revision,
                revision,
                false,
            ),
            RetryQueueResult::Queued,
            "the origin-valid logical request must really be queued for attempt two"
        );
        let retry = state.pending_latency_retries.front().cloned().unwrap();

        let active_id = lsp_server::RequestId::from(numeric_id + 1);
        let active_key = crate::global_state::RequestAttemptKey {
            id: active_id.clone(),
            generation: generation + 1,
            attempt: 1,
        };
        let active_token = state.analysis_host.raw_database().cancellation_token();
        state.request_lifecycles.insert(
            active_id.clone(),
            crate::global_state::RequestLifecycle {
                generation: active_key.generation,
                method: lsp_types::request::HoverRequest::METHOD.to_owned(),
                request: None,
                attempt: 1,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: false,
                origin: None,
                client_cancelled: false,
            },
        );
        state.request_tokens.insert(active_id.clone(), active_token.clone());
        state.request_token_owners.insert(active_id, active_key);
        (retry, active_token)
    }

    fn g1_session_boundary_state(
        retry_id: lsp_server::RequestId,
    ) -> (
        tempfile::TempDir,
        crate::global_state::GlobalState,
        crossbeam_channel::Receiver<Message>,
        crossbeam_channel::Receiver<Task>,
        crate::global_state::RequestAttemptKey,
        salsa::CancellationToken,
    ) {
        let root = tempfile::tempdir().expect("create session-boundary project");
        let caller_path = root.path().join("Caller.bsl");
        let caller_text = "Процедура Caller()\nКонецПроцедуры\n";
        std::fs::write(&caller_path, caller_text).expect("write session-boundary source");
        std::fs::write(root.path().join("bsl-analyzer.toml"), "[source]\nroot = \".\"\n")
            .expect("write valid project config");
        let (sender, client_receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.workspace_root = Some(root.path().to_path_buf());
        state.project = Some(project_model::Project::new(root.path()).expect("load project"));
        state.vfs_done = true;
        let uri = lsp_types::Url::from_file_path(&caller_path).unwrap();
        state.mem_docs.insert(uri.clone(), caller_text.to_owned(), 1);
        let (retry, active_token) =
            g1_queue_valid_retry_for_session_boundary(&mut state, retry_id, uri);
        let task_receiver = state.task_pool.receiver.clone();
        (root, state, client_receiver, task_receiver, retry, active_token)
    }

    fn g1_receive_loader_message(
        state: &mut crate::global_state::GlobalState,
        mut wanted: impl FnMut(&vfs::loader::Message) -> bool,
    ) -> vfs::loader::Message {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .expect("expected VFS loader message before deadline");
            let message = state
                .loader_receiver
                .recv_timeout(remaining)
                .expect("real VFS loader did not produce expected message");
            if wanted(&message) {
                return message;
            }
            handle_loader_msg(state, message).expect("apply unrelated real VFS loader message");
        }
    }

    fn g1_frame_round_trip(message: Message) -> Message {
        let mut frame = Vec::new();
        message.write(&mut frame).expect("encode Content-Length LSP frame");
        assert!(frame.starts_with(b"Content-Length: "));
        Message::read(&mut std::io::Cursor::new(frame))
            .expect("decode Content-Length LSP frame")
            .expect("frame contains one LSP message")
    }

    fn g1_framed_request_response(
        state: &mut crate::global_state::GlobalState,
        request: lsp_server::Request,
        task_receiver: &crossbeam_channel::Receiver<Task>,
        client_receiver: &crossbeam_channel::Receiver<Message>,
    ) -> lsp_server::Response {
        let id = request.id.clone();
        let Message::Request(request) = g1_frame_round_trip(Message::Request(request)) else {
            panic!("request frame decoded as a non-request")
        };
        tracing::info!(g1_marker = "framed_request_decoded", request_id = ?id);
        handle_lsp_msg(state, Message::Request(request))
            .expect("production request loop handles frame");
        admit_pending_latency_retries(state)
            .expect("production event loop admits a retry after request dispatch");
        let response = g1_wait_for_result(state, task_receiver, client_receiver, id.clone());
        let Message::Response(response) = g1_frame_round_trip(Message::Response(response)) else {
            unreachable!("response frame decoded as non-response")
        };
        tracing::info!(g1_marker = "framed_response_decoded", request_id = ?id);
        response
    }

    #[test]
    fn client_response_reader_accepts_publish_diagnostics_before_response() {
        let legacy = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            legacy_next_message_must_be_response(
                &queue_publish_diagnostics_then_cancelled_response(375),
            );
        }));
        let panic = legacy.expect_err("legacy next-message assertion must panic");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic");
        assert!(
            message.contains("queued cancellation produced a non-response"),
            "legacy assertion panicked for a different reason: {message}"
        );

        let response = recv_queued_client_response(
            &queue_publish_diagnostics_then_cancelled_response(375),
            lsp_server::RequestId::from(375),
        );
        assert_eq!(response.id, lsp_server::RequestId::from(375));
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(lsp_server::ErrorCode::RequestCanceled as i32)
        );
        assert_eq!(
            response.error.as_ref().map(|error| error.message.as_str()),
            Some("request cancelled")
        );

        let (sender, receiver) = crossbeam_channel::unbounded();
        sender
            .send(Message::Response(Response::new_ok(
                lsp_server::RequestId::from(368),
                serde_json::Value::Null,
            )))
            .expect("queue response with no preceding notification");
        let response = recv_queued_client_response(&receiver, lsp_server::RequestId::from(368));
        assert_eq!(response.id, lsp_server::RequestId::from(368));
        assert!(response.error.is_none());
        assert_eq!(response.result, Some(serde_json::Value::Null));

        let waited = recv_client_response(
            &queue_publish_diagnostics_then_cancelled_response(375),
            lsp_server::RequestId::from(375),
        );
        assert_eq!(waited.id, lsp_server::RequestId::from(375));
        assert_eq!(
            waited.error.as_ref().map(|error| error.message.as_str()),
            Some("request cancelled")
        );

        let (sender, receiver) = crossbeam_channel::unbounded();
        drop(sender);
        let closed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            recv_client_response(&receiver, lsp_server::RequestId::from(1));
        }));
        let panic = closed.expect_err("closed channel must panic");
        let message =
            panic.downcast_ref::<String>().map(String::as_str).unwrap_or("non-string panic");
        assert!(!message.contains("timed out"), "{message}");
        assert!(message.contains("closed"), "{message}");
    }

    #[test]
    fn g1_live_vfs_pending_write_cancels_real_definition_then_fresh_request_succeeds() {
        g1_with_tracing(|| {
            use base_db::SourceDatabase;
            use lsp_types::request::GotoDefinition;

            G1_READER_ENTERED.store(false, Ordering::Release);
            G1_READER_SAW_WRITE.store(false, Ordering::Release);
            G1_CANCEL_OBSERVED.store(0, Ordering::Release);
            let (sender, client_receiver) = crossbeam_channel::unbounded();
            let mut state = crate::global_state::GlobalState::new(sender);
            *state.analysis_host.raw_database_mut() =
                ide_db::RootDatabaseImpl::new_with_salsa_events();
            state.init_empty_source_root();

            let dir = tempfile::tempdir().unwrap();
            let caller = dir.path().join("Caller.bsl");
            let changed = dir.path().join("Other.bsl");
            let caller_text = "Процедура TargetProcedure()\nКонецПроцедуры\n\nПроцедура Caller()\n    TargetProcedure();\nКонецПроцедуры\n";
            let changed_text = "Процедура Other()\nКонецПроцедуры\n";
            std::fs::write(&caller, caller_text).unwrap();
            std::fs::write(&changed, changed_text).unwrap();
            state.workspace_root = Some(dir.path().to_path_buf());
            state.project = Some(project_model::Project::new(dir.path()).unwrap());
            let abs = |path: &std::path::Path| paths::AbsPathBuf::assert_utf8(path.to_path_buf());
            let caller_uri = lsp_types::Url::from_file_path(&caller).unwrap();

            RequestDispatcher {
                req: Some(g1_definition_request(367, caller_uri.clone())),
                global_state: &mut state,
            }
            .on_latency::<GotoDefinition>(g1_definition)
            .finish();
            let cold =
                recv_queued_client_response(&client_receiver, lsp_server::RequestId::from(367));
            assert_eq!(cold.id, lsp_server::RequestId::from(367));
            assert_eq!(
                cold.error.as_ref().map(|error| error.code),
                Some(lsp_server::ErrorCode::ContentModified as i32)
            );
            assert!(!state.request_tokens.contains_key(&cold.id));
            assert!(state.task_pool.receiver.try_recv().is_err());
            tracing::info!(g1_marker = "cold_start_declined", code = "ContentModified");
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Loaded {
                    files: vec![
                        (abs(&caller), Some(caller_text.as_bytes().to_vec())),
                        (abs(&changed), Some(changed_text.as_bytes().to_vec())),
                    ],
                },
            )
            .unwrap();
            // `handle_vfs_msg` streams contents into VFS/Salsa, while the production
            // workspace loader publishes source-root membership after discovery.
            // This focused fixture supplies that final piece so definition uses the
            // same real semantic path as a loaded project.
            let paths = [&caller, &changed];
            let mut file_set = vfs::file_set::FileSet::new();
            let file_ids: Vec<_> = paths
                .iter()
                .map(|path| {
                    let vfs_path = vfs::VfsPath::new(path.as_path());
                    let file_id = state.vfs.read().file_id(&vfs_path).unwrap();
                    file_set.insert(file_id, vfs_path);
                    file_id
                })
                .collect();
            {
                use base_db::{SourceRoot, BSL_SOURCE_ROOT};
                let db = state.analysis_host.raw_database_mut();
                db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
                for file_id in file_ids {
                    db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
                }
            }
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Progress {
                    n_total: 2,
                    n_done: vfs::loader::LoadingProgress::Finished,
                    dir: None,
                    config_version: 0,
                },
            )
            .unwrap();
            assert!(state.vfs_done, "the loader finalize must establish ready before the overlap");
            state.mem_docs.insert(caller_uri.clone(), caller_text.to_owned(), 1);

            // A successful semantic control proves the fixture resolves the symbol
            // before we introduce the live writer; the post-release request must match it.
            let control_receiver = state.task_pool.receiver.clone();
            RequestDispatcher {
                req: Some(g1_definition_request(368, caller_uri.clone())),
                global_state: &mut state,
            }
            .on_latency::<GotoDefinition>(g1_definition)
            .finish();
            let control = g1_wait_for_result(
                &mut state,
                &control_receiver,
                &client_receiver,
                lsp_server::RequestId::from(368),
            );
            assert!(control.error.is_none(), "initial semantic control failed: {control:?}");
            let control_result = control.result.expect("initial definition control must resolve");
            let control_location =
                if control_result.is_array() { control_result[0].clone() } else { control_result };
            assert_eq!(control_location["uri"], caller_uri.as_str());
            assert_eq!(control_location["range"]["start"]["line"], 0);
            tracing::info!(g1_marker = "semantic_control", result = "definition", ready = true);
            state.request_tokens.remove(&control.id);

            let baseline = state
                .analysis_host
                .raw_database()
                .salsa_event_global()
                .expect("G1 test replaces GlobalState's DB with an event-enabled Salsa DB")
                .set_cancellation;
            G1_CANCEL_FLAG_BASELINE.store(baseline, Ordering::Release);
            let task_receiver = state.task_pool.receiver.clone();
            RequestDispatcher {
                req: Some(g1_definition_request(369, caller_uri.clone())),
                global_state: &mut state,
            }
            .on_retryable_latency::<GotoDefinition>(g1_definition_waiting_for_vfs_write)
            .finish();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !G1_READER_ENTERED.load(Ordering::Acquire) {
                assert!(std::time::Instant::now() < deadline, "semantic reader did not enter");
                std::thread::yield_now();
            }
            tracing::info!(g1_marker = "semantic_reader_entered", request_id = 369);
            let changed_text = "Процедура Other()\n    X = 1;\nКонецПроцедуры\n";
            std::fs::write(&changed, changed_text).unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Changed {
                    files: vec![(abs(&changed), Some(changed_text.as_bytes().to_vec()))],
                },
            )
            .unwrap();
            assert!(
                G1_READER_SAW_WRITE.load(Ordering::Acquire),
                "reader must observe active write"
            );
            let observed = G1_CANCEL_OBSERVED.load(Ordering::Acquire);
            assert!(observed > baseline, "the semantic reader observed Salsa cancellation");
            tracing::info!(
                g1_marker = "salsa_did_set_cancellation",
                baseline,
                observed,
                observed_during_writer_call = true
            );
            let changed_id = state.vfs.read().file_id(&vfs::VfsPath::new(changed.clone())).unwrap();
            assert_eq!(&*state.analysis_host.raw_database().file_text(changed_id), changed_text);
            assert!(state.vfs_done, "a live Changed package must preserve ready state");
            tracing::info!(g1_marker = "changed_writer_complete", live_text = true, ready = true);

            let response = g1_wait_for_result(
                &mut state,
                &task_receiver,
                &client_receiver,
                lsp_server::RequestId::from(369),
            );
            assert_eq!(response.id, lsp_server::RequestId::from(369));
            assert!(
                response.error.is_none(),
                "live write must retry semantic request: {response:?}"
            );
            let result = response.result.expect("retried definition result");
            let location = if result.is_array() { &result[0] } else { &result };
            assert_eq!(location, &control_location, "retry must preserve original semantics");
            tracing::info!(g1_marker = "semantic_reader_released", result = "retried-definition");
            tracing::info!(g1_marker = "retry_response_matches_control", attempt = 2);

            let retry_receiver = state.task_pool.receiver.clone();
            RequestDispatcher {
                req: Some(g1_definition_request(370, caller_uri.clone())),
                global_state: &mut state,
            }
            .on_latency::<GotoDefinition>(g1_definition)
            .finish();
            let response = g1_wait_for_result(
                &mut state,
                &retry_receiver,
                &client_receiver,
                lsp_server::RequestId::from(370),
            );
            assert!(response.error.is_none(), "fresh semantic request failed: {response:?}");
            let result = response.result.expect("definition should have a semantic result");
            let location = if result.is_array() { &result[0] } else { &result };
            assert_eq!(
                location, &control_location,
                "live writer release must restore the control result"
            );
            tracing::info!(
                g1_marker = "fresh_definition",
                matches_control = true,
                result = "definition"
            );

            // Exercise the queued-cancel branch with another real Salsa read and
            // writer overlap. Apply its cancellation completion, hold retry admission,
            // then cancel the logical request before the event loop can start attempt 2.
            G1_READER_ENTERED.store(false, Ordering::Release);
            G1_READER_SAW_WRITE.store(false, Ordering::Release);
            let next_baseline = state
                .analysis_host
                .raw_database()
                .salsa_event_global()
                .expect("event-enabled test DB")
                .set_cancellation;
            G1_CANCEL_FLAG_BASELINE.store(next_baseline, Ordering::Release);
            let queued_cancel_id = lsp_server::RequestId::from(375);
            let queued_cancel_receiver = state.task_pool.receiver.clone();
            RequestDispatcher {
                req: Some(g1_definition_request(375, caller_uri.clone())),
                global_state: &mut state,
            }
            .on_retryable_latency::<GotoDefinition>(g1_definition_waiting_for_vfs_write)
            .finish();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !G1_READER_ENTERED.load(Ordering::Acquire) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "second semantic reader did not enter"
                );
                std::thread::yield_now();
            }
            let second_changed_text = "Процедура Other()\n    X = 2;\nКонецПроцедуры\n";
            std::fs::write(&changed, second_changed_text).unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Changed {
                    files: vec![(abs(&changed), Some(second_changed_text.as_bytes().to_vec()))],
                },
            )
            .unwrap();
            assert!(G1_READER_SAW_WRITE.load(Ordering::Acquire));

            let completion = loop {
                let task = queued_cancel_receiver
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("queued-cancel request did not finish its real attempt");
                if matches!(
                    &task,
                    Task::LatencyRequestFinished { key, .. } if key.id == queued_cancel_id
                ) {
                    break task;
                }
                handle_task(&mut state, task).expect("apply unrelated event-loop task");
            };
            handle_task(&mut state, completion).expect("apply cancellation before retry admission");
            assert!(state.request_lifecycles.get(&queued_cancel_id).is_some_and(|lifecycle| {
                lifecycle.phase == RequestLifecyclePhase::RetryQueued && lifecycle.attempt == 2
            }));
            assert_eq!(
                state.pending_latency_retries.front(),
                Some(&RequestAttemptKey {
                    id: queued_cancel_id.clone(),
                    generation: state.request_lifecycles[&queued_cancel_id].generation,
                    attempt: 2,
                })
            );
            tracing::info!(
                g1_marker = "retry_queued_before_admission",
                request_id = ?queued_cancel_id,
                writer_complete = true,
                attempt = 2
            );

            let original_task_pool = std::mem::replace(
                &mut state.task_pool,
                crate::task_pool::TaskPool::new_with_workers(1),
            );
            let saturated_receiver = state.task_pool.receiver.clone();
            let blocker_entered = crossbeam_channel::bounded(1);
            let (blocker_entered_tx, blocker_entered_rx) = blocker_entered;
            let release_pool = Arc::new(AtomicBool::new(false));
            let release_worker = Arc::clone(&release_pool);
            state
                .task_pool
                .pool
                .try_spawn(move || {
                    blocker_entered_tx.send(()).unwrap();
                    while !release_worker.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    Task::AnalysisProgressTick { epoch: u64::MAX }
                })
                .unwrap();
            blocker_entered_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("one-worker pool blocker starts");
            let queued_blockers = state.task_pool.pool.capacity().unwrap();
            let _release_pool_on_unwind = G1ReleasePoolProbe(Arc::clone(&release_pool));
            for _ in 0..queued_blockers {
                let release_worker = Arc::clone(&release_pool);
                state
                    .task_pool
                    .pool
                    .try_spawn(move || {
                        while !release_worker.load(Ordering::Acquire) {
                            std::thread::yield_now();
                        }
                        Task::AnalysisProgressTick { epoch: u64::MAX }
                    })
                    .expect("fill the bounded worker queue");
            }
            assert!(!state.task_pool.pool.has_capacity());
            // Diagnostics publication and the cancel response share the client
            // channel. Queue the notification first so this read does not depend
            // on which task the pool finished earlier.
            state
                .sender
                .send(
                    lsp_server::Notification::new(
                        "textDocument/publishDiagnostics".to_string(),
                        lsp_types::PublishDiagnosticsParams {
                            uri: caller_uri.clone(),
                            diagnostics: vec![],
                            version: None,
                        },
                    )
                    .into(),
                )
                .expect("queue publishDiagnostics ahead of the cancel response");
            crate::handlers::notification::handle_cancel(
                &mut state,
                lsp_types::CancelParams { id: lsp_types::NumberOrString::Number(375) },
            )
            .unwrap();
            let cancelled = recv_queued_client_response(&client_receiver, queued_cancel_id.clone());
            assert_eq!(cancelled.id, queued_cancel_id);
            assert_eq!(
                cancelled.error.as_ref().map(|error| error.code),
                Some(lsp_server::ErrorCode::RequestCanceled as i32)
            );
            assert!(
                state.pending_latency_retries.is_empty(),
                "queued cancellation must unlink its generation immediately at full capacity"
            );
            admit_pending_latency_retries(&mut state).expect("stale retry admission is safe");
            assert!(!state.request_lifecycles.contains_key(&queued_cancel_id));
            assert!(!state.request_tokens.contains_key(&queued_cancel_id));
            assert!(state.pending_latency_retries.is_empty());
            assert!(
                client_receiver.try_iter().all(|message| !matches!(message, Message::Response(_))),
                "queued cancel sends exactly one response"
            );
            tracing::info!(
                g1_marker = "queued_retry_cancelled_before_admission",
                request_id = ?queued_cancel_id,
                response = "RequestCanceled",
                retry_started = false,
                pool_saturated = true,
                queue_unlinked = true
            );
            release_pool.store(true, Ordering::Release);
            for _ in 0..=queued_blockers {
                let task = saturated_receiver
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("all pool blockers finish after release");
                handle_task(&mut state, task).expect("apply queued-cancel capacity probe result");
            }
            let (drained_tasks, completed_barriers) =
                g1_drain_task_pool_to_fixed_point(&mut state, &saturated_receiver);
            tracing::info!(
                g1_marker = "queued_cancel_capacity_pool_drained",
                drained_tasks,
                completed_barriers,
            );
            state.task_pool = original_task_pool;
            assert!(
                state.task_pool.pool.capacity().unwrap_or(0) / 4 >= 2,
                "the normal task pool remains available after queued-cancel probe"
            );
            tracing::info!(
                g1_marker = "queued_cancel_capacity_pool_restored",
                workers_at_least = 2,
                capacity = state.task_pool.pool.capacity().unwrap_or(0)
            );

            let running_cancel_id = lsp_server::RequestId::from(376);
            let (reader_entered, release_reader) =
                crate::handlers::dispatch::g1_arm_pending_write_gate(&running_cancel_id, 1);
            let running_cancel_receiver = state.task_pool.receiver.clone();
            handle_lsp_msg(
                &mut state,
                Message::Request(g1_definition_request(376, caller_uri.clone())),
            )
            .unwrap();
            assert_eq!(
                reader_entered.recv_timeout(std::time::Duration::from_secs(10)).unwrap(),
                1,
                "real dispatched handler reaches the controlled query checkpoint"
            );
            handle_notification(
                &mut state,
                Notification::new("$/cancelRequest".to_owned(), serde_json::json!({"id": 376})),
            )
            .unwrap();
            release_reader.store(true, Ordering::Release);
            crate::handlers::dispatch::g1_disarm_pending_write_gate(&running_cancel_id);
            let running_cancelled = g1_wait_for_result(
                &mut state,
                &running_cancel_receiver,
                &client_receiver,
                running_cancel_id.clone(),
            );
            assert_eq!(
                running_cancelled.error.as_ref().map(|error| error.code),
                Some(lsp_server::ErrorCode::RequestCanceled as i32)
            );
            assert!(!state.request_lifecycles.contains_key(&running_cancel_id));
            assert!(!state.request_tokens.contains_key(&running_cancel_id));
            assert!(state.pending_latency_retries.is_empty());
            handle_notification(
                &mut state,
                Notification::new("$/cancelRequest".to_owned(), serde_json::json!({"id": 376})),
            )
            .unwrap();
            assert!(
                client_receiver.try_recv().is_err(),
                "late cancel after completion cannot send another response"
            );
            tracing::info!(
                g1_marker = "running_request_cancelled_at_checkpoint",
                request_id = ?running_cancel_id,
                response = "RequestCanceled",
                retries = 0
            );

            let origin_change_id = lsp_server::RequestId::from(377);
            let (origin_reader_entered, _origin_release) =
                crate::handlers::dispatch::g1_arm_pending_write_gate(&origin_change_id, 1);
            let origin_task_receiver = state.task_pool.receiver.clone();
            handle_lsp_msg(
                &mut state,
                Message::Request(g1_definition_request(377, caller_uri.clone())),
            )
            .unwrap();
            assert_eq!(
                origin_reader_entered.recv_timeout(std::time::Duration::from_secs(10)).unwrap(),
                1
            );
            let origin_writer_text = "Процедура Other()\n    Значение = 3;\nКонецПроцедуры\n";
            std::fs::write(&changed, origin_writer_text).unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Changed {
                    files: vec![(abs(&changed), Some(origin_writer_text.as_bytes().to_vec()))],
                },
            )
            .unwrap();
            crate::handlers::dispatch::g1_disarm_pending_write_gate(&origin_change_id);
            let completion = loop {
                let task = origin_task_receiver
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("origin-change attempt returns its real task");
                if matches!(
                    &task,
                    Task::LatencyRequestFinished { key, .. } if key.id == origin_change_id
                ) {
                    break task;
                }
                handle_task(&mut state, task).unwrap();
            };
            handle_task(&mut state, completion).unwrap();
            assert!(state.request_lifecycles.get(&origin_change_id).is_some_and(|lifecycle| {
                lifecycle.phase == RequestLifecyclePhase::RetryQueued && lifecycle.attempt == 2
            }));
            let original_identity = state.mem_docs.identity(&caller_uri).unwrap();
            handle_notification(
                &mut state,
                Notification::new(
                    "textDocument/didChange".to_owned(),
                    serde_json::json!({
                        "textDocument": {"uri": caller_uri, "version": 2},
                        "contentChanges": [{"text": caller_text}]
                    }),
                ),
            )
            .unwrap();
            assert_ne!(state.mem_docs.identity(&caller_uri), Some(original_identity));
            let stale_origin_response = g1_wait_for_result(
                &mut state,
                &origin_task_receiver,
                &client_receiver,
                origin_change_id.clone(),
            );
            assert_eq!(
                stale_origin_response.error.as_ref().map(|error| error.code),
                Some(lsp_server::ErrorCode::ContentModified as i32)
            );
            assert_eq!(
                stale_origin_response.error.as_ref().map(|error| error.message.as_str()),
                Some("content modified: document origin changed")
            );
            assert!(origin_reader_entered.try_recv().is_err(), "stale position is never replayed");
            crate::handlers::dispatch::g1_disarm_pending_write_gate(&origin_change_id);
            tracing::info!(
                g1_marker = "did_change_invalidates_queued_retry",
                request_id = ?origin_change_id,
                attempt = 1,
                retries = 0,
                response = "ContentModified"
            );
            let fresh_response = g1_framed_request_response(
                &mut state,
                g1_definition_request(378, caller_uri.clone()),
                &origin_task_receiver,
                &client_receiver,
            );
            assert!(
                fresh_response.error.is_none(),
                "fresh external request after edit succeeds: {fresh_response:?}"
            );
            let fresh_result = fresh_response.result.expect("fresh definition result");
            let fresh_location =
                if fresh_result.is_array() { &fresh_result[0] } else { &fresh_result };
            assert_eq!(fresh_location, &control_location);
            tracing::info!(g1_marker = "fresh_request_after_origin_change", semantic_parity = true);
        });
    }

    #[test]
    fn g1_initial_project_failure_keeps_real_dispatch_gate_closed() {
        g1_with_tracing(|| {
            let (sender, receiver) = crossbeam_channel::unbounded();
            let mut state = crate::global_state::GlobalState::new(sender);
            state.init_empty_source_root();
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("bsl-analyzer.toml"), "[source\nroot = \"src\"\n")
                .unwrap();

            let error = state
                .set_workspace_root(root.path().to_path_buf())
                .expect_err("malformed initial project config must fail");
            assert!(state.project.is_none());
            assert!(!state.vfs_done, "failed initial project load must not publish readiness");
            tracing::info!(g1_marker = "initial_project_failure", failed = true, ready = false);

            let uri = lsp_types::Url::from_file_path(root.path().join("Caller.bsl")).unwrap();
            RequestDispatcher {
                req: Some(g1_definition_request(371, uri)),
                global_state: &mut state,
            }
            .on_latency::<lsp_types::request::GotoDefinition>(g1_definition)
            .finish();
            let response = recv_queued_client_response(&receiver, lsp_server::RequestId::from(371));
            assert_eq!(response.id, lsp_server::RequestId::from(371));
            assert_eq!(
                response.error.as_ref().map(|error| error.code),
                Some(lsp_server::ErrorCode::ContentModified as i32)
            );
            assert_eq!(
                response.error.as_ref().map(|error| error.message.as_str()),
                Some(crate::handlers::dispatch::PROJECT_REJECTED_MESSAGE),
                "a rejected project is terminal, not a load still in progress"
            );
            assert!(state.request_tokens.is_empty());
            assert!(state.task_pool.receiver.try_recv().is_err());
            drop(error);
        });
    }

    #[test]
    fn g1_native_loader_applies_bsl_and_watch_only_events_around_framed_semantics() {
        g1_with_tracing(|| {
            use base_db::SourceDatabase;

            fn catalog_xml(uuid: &str) -> String {
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="{uuid}">
        <Properties><Name>Before</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#
                )
            }

            let root = tempfile::tempdir().expect("create native loader workspace");
            let root_path = root.path().canonicalize().expect("canonical workspace root");
            let caller = root_path.join("Caller.bsl");
            let removed = root_path.join("Removed.bsl");
            let configuration = root_path.join("Configuration.xml");
            let catalog = root_path.join("Catalogs/Before.xml");
            std::fs::create_dir_all(catalog.parent().unwrap()).unwrap();
            let caller_text = "Процедура TargetProcedure()\nКонецПроцедуры\n\nПроцедура Caller()\n    TargetProcedure();\n    Ref = Справочники.Before.СоздатьЭлемент();\nКонецПроцедуры\n";
            let removed_text = "Процедура ToRemove()\nКонецПроцедуры\n";
            std::fs::write(&caller, caller_text).unwrap();
            std::fs::write(&removed, removed_text).unwrap();
            std::fs::write(&configuration, "<Configuration/>").unwrap();
            std::fs::write(&catalog, catalog_xml("00000000-0000-0000-0000-000000000021")).unwrap();

            let (sender, client_receiver) = crossbeam_channel::unbounded();
            let mut state = crate::global_state::GlobalState::new(sender);
            *state.analysis_host.raw_database_mut() =
                ide_db::RootDatabaseImpl::new_with_salsa_events();
            state.init_empty_source_root();
            state
                .set_workspace_root(root_path.clone())
                .expect("real NotifyHandle accepts project loader config");

            let initial_finish = g1_receive_loader_message(&mut state, |message| {
                matches!(
                    message,
                    vfs::loader::Message::Progress {
                        n_done: vfs::loader::LoadingProgress::Finished,
                        ..
                    }
                )
            });
            handle_loader_msg(&mut state, initial_finish)
                .expect("apply actual loader finish through production handler");
            assert!(state.vfs_done, "real loader completion opens semantic readiness");

            let caller_uri = lsp_types::Url::from_file_path(&caller).unwrap();
            state.mem_docs.insert(caller_uri.clone(), caller_text.to_owned(), 1);
            let caller_id = state
                .vfs
                .read()
                .file_id(&vfs::VfsPath::new(caller.clone()))
                .expect("real initial scan loaded caller");
            let task_receiver = state.task_pool.receiver.clone();
            let control = g1_framed_request_response(
                &mut state,
                g1_definition_request(421, caller_uri.clone()),
                &task_receiver,
                &client_receiver,
            );
            assert!(control.error.is_none(), "loaded semantic control: {control:?}");
            let control_result = control.result.expect("loaded definition returns a location");
            let control_location =
                if control_result.is_array() { control_result[0].clone() } else { control_result };
            assert_eq!(control_location["uri"], caller_uri.as_str());

            let added = root_path.join("Added.bsl");
            let added_text = "Процедура AddedProcedure()\nКонецПроцедуры\n";
            std::fs::write(&added, added_text).expect("create watched BSL source");
            let added_abs = paths::AbsPathBuf::assert_utf8(added.clone());
            let added_event = g1_receive_loader_message(&mut state, |message| {
                matches!(
                    message,
                    vfs::loader::Message::Changed { files }
                        if files.iter().any(|(path, contents)| path == &added_abs
                            && contents.as_deref() == Some(added_text.as_bytes()))
                )
            });
            handle_loader_msg(&mut state, added_event)
                .expect("apply real NotifyHandle add through production VFS path");
            let added_id = state
                .vfs
                .read()
                .file_id(&vfs::VfsPath::new(added.clone()))
                .expect("added source receives VFS identity");
            assert!(state.vfs.read().exists(added_id));
            assert_eq!(&*state.analysis_host.raw_database().file_text(added_id), added_text);
            tracing::info!(g1_marker = "native_loader_added_bsl_applied", actual_loader = true);

            std::fs::remove_file(&removed).expect("delete watched BSL source");
            let removed_abs = paths::AbsPathBuf::assert_utf8(removed.clone());
            let pending_deletion = g1_receive_loader_message(&mut state, |message| {
                matches!(
                    message,
                    vfs::loader::Message::Changed { files }
                        if files.iter().any(|(path, contents)| path == &removed_abs
                            && contents.is_none())
                )
            });
            let during_loader_work = g1_framed_request_response(
                &mut state,
                g1_definition_request(422, caller_uri.clone()),
                &task_receiver,
                &client_receiver,
            );
            assert!(
                during_loader_work.error.is_none(),
                "semantic response while received deletion awaits VFS application: {during_loader_work:?}"
            );
            let during_result = during_loader_work
                .result
                .expect("semantic request has a positive result during loader work");
            let during_location =
                if during_result.is_array() { &during_result[0] } else { &during_result };
            assert_eq!(during_location, &control_location);
            tracing::info!(
                g1_marker = "framed_semantic_response_with_native_deletion_received_not_applied",
                semantic_parity = true
            );

            handle_loader_msg(&mut state, pending_deletion)
                .expect("apply real NotifyHandle delete through production VFS path");
            assert!(!state.vfs.read().exists(
                state
                    .vfs
                    .read()
                    .file_id(&vfs::VfsPath::new(removed.clone()))
                    .expect("deleted source retains its VFS identity")
            ));
            tracing::info!(g1_marker = "native_loader_deleted_bsl_applied", actual_loader = true);

            let old_catalog = state
                .analysis_host
                .raw_database()
                .resolve_metadata_object_for_file(
                    caller_id,
                    bsl_metadata::MdoType::Catalog,
                    "Before",
                )
                .expect("actual initial watch-only XML registered metadata");
            let updated_catalog_text = catalog_xml("00000000-0000-0000-0000-000000000022");
            std::fs::write(&catalog, &updated_catalog_text).expect("update watched XML");
            handle_notification(
                &mut state,
                Notification::new(
                    "workspace/didChangeWatchedFiles".to_owned(),
                    serde_json::json!({
                        "changes": [{
                            "uri": lsp_types::Url::from_file_path(&catalog).unwrap(),
                            "type": 2
                        }]
                    }),
                ),
            )
            .expect("route standard watched-files event through real NotifyHandle");
            let catalog_abs = paths::AbsPathBuf::assert_utf8(catalog.clone());
            let watch_only_event = g1_receive_loader_message(&mut state, |message| {
                matches!(
                    message,
                    vfs::loader::Message::WatchOnly { files }
                        if files.iter().any(|path| path == &catalog_abs)
                )
            });
            handle_loader_msg(&mut state, watch_only_event)
                .expect("apply real native WatchOnly event through production handler");
            let new_catalog = state
                .analysis_host
                .raw_database()
                .resolve_metadata_object_for_file(
                    caller_id,
                    bsl_metadata::MdoType::Catalog,
                    "Before",
                )
                .expect("actual WatchOnly update refreshes semantic metadata");
            assert_ne!(old_catalog.uuid, new_catalog.uuid);
            assert_eq!(
                new_catalog.uuid.unwrap().to_string(),
                "00000000-0000-0000-0000-000000000022"
            );
            tracing::info!(
                g1_marker = "native_watch_only_xml_uuid_applied",
                standard_notification_routed = true,
                semantic_metadata_changed = true
            );
            g1_wait_for_task_pool_quiescence(&mut state, &task_receiver);
        });
    }

    #[test]
    fn g1_watch_only_atomic_config_and_active_batch_keep_definition_available() {
        g1_with_tracing(|| {
            use base_db::SourceDatabase;
            use parking_lot::Mutex;
            use std::sync::Arc;

            #[derive(Debug)]
            struct RecordingLoader(Arc<Mutex<Vec<paths::AbsPathBuf>>>);
            impl vfs::loader::Handle for RecordingLoader {
                fn spawn(_sender: vfs::loader::Sender) -> Self
                where
                    Self: Sized,
                {
                    unreachable!("injected loader")
                }
                fn set_config(&mut self, _config: vfs::loader::Config) {}
                fn invalidate(&mut self, path: paths::AbsPathBuf) {
                    self.0.lock().push(path);
                }
                fn load_sync(&mut self, _path: &paths::AbsPath) -> Option<Vec<u8>> {
                    None
                }
            }

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

            struct ReleaseBatchOnDrop {
                receiver: crossbeam_channel::Receiver<Task>,
                wait_for_completion: bool,
            }
            impl Drop for ReleaseBatchOnDrop {
                fn drop(&mut self) {
                    crate::handlers::workspace_batch::g1_batch_barrier(false);
                    if self.wait_for_completion {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(10);
                        loop {
                            let remaining =
                                deadline.saturating_duration_since(std::time::Instant::now());
                            match self.receiver.recv_timeout(remaining) {
                                Ok(Task::WorkspaceBatchChunk { .. }) => break,
                                Ok(_) => continue,
                                Err(error) => {
                                    tracing::error!(
                                        g1_marker = "batch_cleanup_wait_failed",
                                        error = %error,
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            let root = tempfile::tempdir().unwrap();
            let caller = root.path().join("Caller.bsl");
            let batch_file = root.path().join("Batch.bsl");
            let catalog_dir = root.path().join("Catalogs");
            std::fs::create_dir_all(&catalog_dir).unwrap();
            let caller_text = "Процедура TargetProcedure()\nКонецПроцедуры\n\nПроцедура Caller()\n    TargetProcedure();\n    Ref = Справочники.Before.СоздатьЭлемент();\n    Сообщить(Ref);\n    Справочники.Before.\nКонецПроцедуры\n";
            let batch_text = "Процедура BatchProcedure()\nКонецПроцедуры\n";
            let writer_path = root.path().join("Writer.bsl");
            let writer_text = "Процедура WriterProcedure()\nКонецПроцедуры\n";
            std::fs::write(&caller, caller_text).unwrap();
            std::fs::write(&batch_file, batch_text).unwrap();
            std::fs::write(&writer_path, writer_text).unwrap();
            std::fs::write(root.path().join("Configuration.xml"), "<Configuration/>").unwrap();
            let catalog_path = catalog_dir.join("Before.xml");
            std::fs::write(
                &catalog_path,
                catalog_xml("Before", "00000000-0000-0000-0000-000000000011"),
            )
            .unwrap();
            let config_path = root.path().join("bsl-analyzer.toml");
            std::fs::write(
                &config_path,
                "[source]\nroot = \".\"\n\n[features]\nworkspaceDiagnostics = \"all\"\n",
            )
            .unwrap();

            let (sender, client_receiver) = crossbeam_channel::unbounded();
            let (loader_sender, loader_receiver) = crossbeam_channel::bounded(4);
            let loader_receiver_test = loader_receiver.clone();
            let invalidated_paths = Arc::new(Mutex::new(Vec::new()));
            let mut state = crate::global_state::GlobalState::with_loader(
                sender,
                Box::new(RecordingLoader(Arc::clone(&invalidated_paths))),
                loader_receiver,
            );
            *state.analysis_host.raw_database_mut() =
                ide_db::RootDatabaseImpl::new_with_salsa_events();
            state.init_empty_source_root();
            state.set_workspace_root(root.path().to_path_buf()).expect("valid loaded workspace");
            let mut release_batch_on_drop = ReleaseBatchOnDrop {
                receiver: state.task_pool.receiver.clone(),
                wait_for_completion: false,
            };
            let abs = |path: &std::path::Path| paths::AbsPathBuf::assert_utf8(path.to_path_buf());
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Loaded {
                    files: vec![
                        (abs(&caller), Some(caller_text.as_bytes().to_vec())),
                        (abs(&batch_file), Some(batch_text.as_bytes().to_vec())),
                        (abs(&writer_path), Some(writer_text.as_bytes().to_vec())),
                    ],
                },
            )
            .unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::WatchOnly {
                    files: vec![abs(&catalog_path), abs(&root.path().join("Configuration.xml"))],
                },
            )
            .unwrap();
            let catalog_id =
                state.vfs.read().file_id(&vfs::VfsPath::new(catalog_path.clone())).unwrap();
            let initial_config_version = state.vfs_progress_config_version;
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Progress {
                    n_total: 2,
                    n_done: vfs::loader::LoadingProgress::Started,
                    dir: None,
                    config_version: initial_config_version,
                },
            )
            .unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Progress {
                    n_total: 2,
                    n_done: vfs::loader::LoadingProgress::Progress(2),
                    dir: None,
                    config_version: initial_config_version,
                },
            )
            .unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Progress {
                    n_total: 2,
                    n_done: vfs::loader::LoadingProgress::Finished,
                    dir: None,
                    config_version: initial_config_version,
                },
            )
            .unwrap();
            assert!(state.vfs_done);
            assert_eq!(
                state.project.as_ref().unwrap().config.features.workspace_diagnostics,
                project_model::WorkspaceDiagnosticsScope::All
            );
            tracing::info!(g1_marker = "loaded_ready", started_progress_finished = true);

            state.mem_docs.insert(
                lsp_types::Url::from_file_path(&caller).unwrap(),
                caller_text.to_owned(),
                1,
            );
            let caller_id = state.vfs.read().file_id(&vfs::VfsPath::new(caller.clone())).unwrap();
            let control_receiver = state.task_pool.receiver.clone();
            let control = g1_framed_request_response(
                &mut state,
                g1_definition_request(401, lsp_types::Url::from_file_path(&caller).unwrap()),
                &control_receiver,
                &client_receiver,
            );
            assert!(control.error.is_none(), "loaded definition control failed: {control:?}");
            let control_result = control.result.expect("loaded control resolves a definition");
            let control_location =
                if control_result.is_array() { control_result[0].clone() } else { control_result };
            assert_eq!(
                control_location["uri"],
                lsp_types::Url::from_file_path(&caller).unwrap().as_str()
            );
            assert_eq!(control_location["range"]["start"]["line"], 0);

            let caller_uri = lsp_types::Url::from_file_path(&caller).unwrap();
            let position_params = |position: lsp_types::Position| {
                serde_json::json!({
                    "textDocument": {"uri": caller_uri},
                    "position": position,
                })
            };
            let hover_control = g1_framed_request_response(
                &mut state,
                g1_json_request(
                    406,
                    lsp_types::request::HoverRequest::METHOD,
                    position_params(lsp_types::Position { line: 4, character: 8 }),
                ),
                &control_receiver,
                &client_receiver,
            )
            .result
            .expect("hover control has a semantic result");
            assert!(hover_control["contents"].is_object() || hover_control["contents"].is_string());

            let first_package_text =
                "Процедура WriterProcedure()\n    Значение = 1;\nКонецПроцедуры\n";
            std::fs::write(&writer_path, first_package_text).unwrap();
            loader_sender
                .send(vfs::loader::Message::Changed {
                    files: vec![(abs(&writer_path), Some(first_package_text.as_bytes().to_vec()))],
                })
                .unwrap();
            let first_package = loader_receiver_test
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("first production loader package is queued");
            assert!(matches!(first_package, vfs::loader::Message::Changed { .. }));
            handle_loader_msg(&mut state, first_package).unwrap();
            let second_package_text =
                "Процедура BatchProcedure()\n    Значение = 2;\nКонецПроцедуры\n";
            std::fs::write(&batch_file, second_package_text).unwrap();
            loader_sender
                .send(vfs::loader::Message::Changed {
                    files: vec![(abs(&batch_file), Some(second_package_text.as_bytes().to_vec()))],
                })
                .unwrap();
            let between_packages = g1_framed_request_response(
                &mut state,
                g1_definition_request(414, caller_uri.clone()),
                &control_receiver,
                &client_receiver,
            );
            assert!(between_packages.error.is_none());
            let between_result =
                between_packages.result.expect("semantic response between packages");
            let between_location =
                if between_result.is_array() { &between_result[0] } else { &between_result };
            assert_eq!(between_location, &control_location);
            assert_eq!(loader_receiver_test.len(), 1, "second real loader package remains pending");
            tracing::info!(
                g1_marker = "semantic_response_between_loader_packages",
                first_package_applied = true,
                second_package_pending = true,
                semantic_parity = true
            );
            let second_package = loader_receiver_test
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("second production loader package remains available after the request");
            handle_loader_msg(&mut state, second_package).unwrap();
            let batch_file_id =
                state.vfs.read().file_id(&vfs::VfsPath::new(batch_file.clone())).unwrap();
            assert_eq!(
                &*state.analysis_host.raw_database().file_text(batch_file_id),
                second_package_text,
                "the second queued loader package applies after the semantic response"
            );
            tracing::info!(g1_marker = "second_loader_package_applied", actual_vfs_text = true);

            let type_position = g1_position(caller_text, 6, "Ref", false);
            let type_definition_control = g1_framed_request_response(
                &mut state,
                g1_json_request(
                    407,
                    lsp_types::request::GotoTypeDefinition::METHOD,
                    position_params(type_position),
                ),
                &control_receiver,
                &client_receiver,
            )
            .result
            .expect("type-definition control has a target");
            let type_location = if type_definition_control.is_array() {
                type_definition_control[0].clone()
            } else {
                type_definition_control.clone()
            };
            assert_eq!(
                type_location["uri"],
                lsp_types::Url::from_file_path(&catalog_path).unwrap().as_str()
            );

            let completion_position = g1_position(caller_text, 7, "Справочники.Before.", true);
            let completion_control = g1_framed_request_response(
                &mut state,
                g1_json_request(
                    408,
                    lsp_types::request::Completion::METHOD,
                    position_params(completion_position),
                ),
                &control_receiver,
                &client_receiver,
            )
            .result
            .expect("completion control has a response");
            let completion_items = if completion_control.is_array() {
                &completion_control
            } else {
                &completion_control["items"]
            };
            assert!(completion_items.as_array().is_some_and(|items| !items.is_empty()));

            let before = state
                .analysis_host
                .raw_database()
                .resolve_metadata_object_for_file(
                    caller_id,
                    bsl_metadata::MdoType::Catalog,
                    "Before",
                )
                .expect("initial watch-only XML resolves through the loaded workspace");
            assert!(before.uuid.is_some(), "initial metadata object retains its UUID");

            let updated_catalog_text =
                catalog_xml("Before", "00000000-0000-0000-0000-000000000012");
            std::fs::write(&catalog_path, &updated_catalog_text).unwrap();
            handle_notification(
            &mut state,
            Notification::new(
                "workspace/didChangeWatchedFiles".to_owned(),
                serde_json::json!({"changes":[{"uri":lsp_types::Url::from_file_path(&catalog_path).unwrap(),"type":2}]}),
            ),
        )
        .unwrap();
            assert_eq!(
                state.vfs.read().file_id(&vfs::VfsPath::new(catalog_path.clone())),
                Some(catalog_id),
            );
            assert_eq!(
                invalidated_paths.lock().as_slice(),
                &[abs(&catalog_path)],
                "the standard watched-files notification routes the local XML path"
            );
            tracing::info!(
                g1_marker = "watched_files_notification_routed",
                native_watch_only_event_follows = true
            );

            handle_loader_msg(
                &mut state,
                vfs::loader::Message::WatchOnly { files: vec![abs(&catalog_path)] },
            )
            .unwrap();
            assert_eq!(
                state.vfs.read().file_id(&vfs::VfsPath::new(catalog_path.clone())),
                Some(catalog_id),
            );
            let after = state
                .analysis_host
                .raw_database()
                .resolve_metadata_object_for_file(
                    caller_id,
                    bsl_metadata::MdoType::Catalog,
                    "Before",
                )
                .expect("production WatchOnly handler refreshes changed XML");
            assert!(after.uuid.is_some());
            assert_ne!(
                before.uuid, after.uuid,
                "WatchOnly update must refresh changed XML content"
            );
            tracing::info!(
                g1_marker = "watched_xml_applied",
                notification_routed = true,
                native_watch_only_semantics = true
            );
            let response_receiver = state.task_pool.receiver.clone();
            RequestDispatcher {
                req: Some(g1_definition_request(
                    402,
                    lsp_types::Url::from_file_path(&caller).unwrap(),
                )),
                global_state: &mut state,
            }
            .on_latency::<lsp_types::request::GotoDefinition>(g1_definition)
            .finish();
            let response = g1_wait_for_result(
                &mut state,
                &response_receiver,
                &client_receiver,
                lsp_server::RequestId::from(402),
            );
            assert!(
                response.error.is_none(),
                "definition after WatchOnly update failed: {response:?}"
            );
            let result = response.result.expect("definition after WatchOnly resolves");
            let location = if result.is_array() { &result[0] } else { &result };
            assert_eq!(location, &control_location);
            tracing::info!(
                g1_marker = "watch_only_live_update",
                semantic_result_matches_control = true
            );

            let retried_definition = g1_cancel_then_retry::<lsp_types::request::GotoDefinition>(
                &mut state,
                g1_definition_request(409, caller_uri.clone()),
                g1_definition_waiting_for_vfs_write,
                &writer_path,
                "Процедура WriterProcedure()\n// phase 1\nКонецПроцедуры\n",
                &client_receiver,
            );
            let retried_definition = if retried_definition.is_array() {
                retried_definition[0].clone()
            } else {
                retried_definition
            };
            assert_eq!(retried_definition, control_location);
            tracing::info!(
                g1_marker = "allowlisted_retry_parity",
                method = "definition",
                matches_control = true
            );

            let retried_hover = g1_cancel_then_retry::<lsp_types::request::HoverRequest>(
                &mut state,
                g1_json_request(
                    410,
                    lsp_types::request::HoverRequest::METHOD,
                    position_params(lsp_types::Position { line: 4, character: 8 }),
                ),
                g1_hover_waiting_for_vfs_write,
                &writer_path,
                "Процедура WriterProcedure()\n// phase 2\nКонецПроцедуры\n",
                &client_receiver,
            );
            assert_eq!(retried_hover, hover_control);
            tracing::info!(
                g1_marker = "allowlisted_retry_parity",
                method = "hover",
                matches_control = true
            );

            let retried_type_definition =
                g1_cancel_then_retry::<lsp_types::request::GotoTypeDefinition>(
                    &mut state,
                    g1_json_request(
                        411,
                        lsp_types::request::GotoTypeDefinition::METHOD,
                        position_params(type_position),
                    ),
                    g1_type_definition_waiting_for_vfs_write,
                    &writer_path,
                    "Процедура WriterProcedure()\n// phase 3\nКонецПроцедуры\n",
                    &client_receiver,
                );
            assert_eq!(retried_type_definition, type_definition_control);
            tracing::info!(
                g1_marker = "allowlisted_retry_parity",
                method = "typeDefinition",
                matches_control = true
            );

            let retried_completion = g1_cancel_then_retry::<lsp_types::request::Completion>(
                &mut state,
                g1_json_request(
                    412,
                    lsp_types::request::Completion::METHOD,
                    position_params(completion_position),
                ),
                g1_completion_waiting_for_vfs_write,
                &writer_path,
                "Процедура WriterProcedure()\n// phase 4\nКонецПроцедуры\n",
                &client_receiver,
            );
            assert_eq!(retried_completion, completion_control);
            tracing::info!(
                g1_marker = "allowlisted_retry_parity",
                method = "completion",
                matches_control = true
            );

            let exhausted = g1_three_real_writer_attempts(
                &mut state,
                g1_definition_request(413, caller_uri.clone()),
                &writer_path,
                &client_receiver,
            );
            assert_eq!(
                exhausted.error.as_ref().map(|error| error.code),
                Some(lsp_server::ErrorCode::ContentModified as i32)
            );
            assert_eq!(
                exhausted.error.as_ref().map(|error| error.message.as_str()),
                Some("content modified: retry limit exhausted")
            );
            assert!(exhausted.result.is_none());
            assert!(
                client_receiver.try_iter().all(|message| {
                    !matches!(message, Message::Response(response) if response.id == exhausted.id)
                }),
                "retry exhaustion produces exactly one response for the original id"
            );
            tracing::info!(
                g1_marker = "three_real_writes_exhaust_attempt_budget",
                attempts = 3,
                retries = 2,
                terminal_code = "ContentModified",
                exactly_one_response = true
            );

            assert!(state.workspace_batch_dirty, "finalize must arm the real batch");
            let available_worker_budget = state.task_pool.pool.capacity().unwrap_or(0) / 4;
            assert!(
                available_worker_budget >= 2,
                "batch overlap requires the original multi-worker pool, not the capacity probe"
            );
            tracing::info!(
                g1_marker = "batch_overlap_worker_budget",
                available_workers_at_least = available_worker_budget,
                task_queue_capacity = state.task_pool.pool.capacity().unwrap_or(0)
            );
            crate::handlers::workspace_batch::g1_batch_barrier(true);
            crate::handlers::spawn_workspace_batch(&mut state);
            release_batch_on_drop.wait_for_completion = true;
            assert!(state.workspace_batch_in_flight, "production spawn must mark the batch active");
            assert!(state.workspace_batch_plan.is_some(), "production spawn builds a real plan");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !crate::handlers::workspace_batch::g1_batch_entered() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "batch worker did not hit test barrier"
                );
                std::thread::yield_now();
            }
            assert!(crate::handlers::workspace_batch::g1_batch_is_paused());
            tracing::info!(g1_marker = "workspace_batch_worker_active", in_flight = true);

            let batch_cancel_baseline = state
                .analysis_host
                .raw_database()
                .salsa_event_global()
                .expect("G1 test uses event-enabled Salsa DB")
                .set_cancellation;
            let initial_plan = state.workspace_batch_plan.as_ref().unwrap();
            let initial_generation = initial_plan.generation;
            let initial_chunk = initial_plan.next_chunk;
            tracing::info!(
                g1_marker = "workspace_batch_progress",
                generation = initial_plan.generation,
                next_chunk = initial_plan.next_chunk,
                total_chunks = initial_plan.num_chunks,
                outstanding_files = initial_plan.file_ids.len(),
            );

            let changed_batch_text =
                "Процедура BatchProcedure()\n    Значение = 42;\nКонецПроцедуры\n";
            std::fs::write(&batch_file, changed_batch_text).unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Changed {
                    files: vec![(abs(&batch_file), Some(changed_batch_text.as_bytes().to_vec()))],
                },
            )
            .unwrap();
            let cancelled_at = crate::handlers::workspace_batch::g1_batch_cancel_observed();
            assert!(
                cancelled_at > batch_cancel_baseline,
                "live writer reached the Salsa checkpoint"
            );
            let batch_file_id =
                state.vfs.read().file_id(&vfs::VfsPath::new(batch_file.clone())).unwrap();
            assert_eq!(
                &*state.analysis_host.raw_database().file_text(batch_file_id),
                changed_batch_text,
                "the live Changed writer applied the new Salsa text"
            );
            assert!(state.workspace_batch_in_flight, "writer returned after the worker dropped its snapshot, before event-loop task handling");
            tracing::info!(
                g1_marker = "batch_writer_applied_live_contents",
                salsa_cancel_baseline = batch_cancel_baseline,
                salsa_cancel_observed = cancelled_at,
                text_matches_disk = true,
                worker_task_pending = true,
            );

            let cancelled_task = loop {
                let task = state
                    .task_pool
                    .receiver
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("cancelled batch task timed out");
                match &task {
                    Task::WorkspaceBatchChunk {
                        generation,
                        outcome: crate::global_state::WorkspaceBatchOutcome::Cancelled,
                    } => {
                        assert_eq!(*generation, initial_generation);
                        break task;
                    }
                    _ => handle_task(&mut state, task).unwrap(),
                }
            };
            handle_task(&mut state, cancelled_task).unwrap();
            release_batch_on_drop.wait_for_completion = false;
            assert!(!state.workspace_batch_in_flight);
            assert!(state.workspace_batch_plan.is_some(), "cancelled chunk stays on its real plan");
            assert_eq!(state.workspace_batch_plan.as_ref().unwrap().next_chunk, initial_chunk);
            tracing::info!(
                g1_marker = "cancelled_batch_task_handled",
                same_chunk_retained = true,
                in_flight = false,
            );

            crate::handlers::workspace_batch::g1_batch_barrier(true);
            crate::handlers::spawn_workspace_batch(&mut state);
            release_batch_on_drop.wait_for_completion = true;
            assert!(
                state.workspace_batch_in_flight,
                "production scheduler restarts the real batch"
            );
            let restarted_generation = state.workspace_batch_plan.as_ref().unwrap().generation;
            assert_eq!(restarted_generation, initial_generation);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !crate::handlers::workspace_batch::g1_batch_entered() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "restarted batch worker did not hit checkpoint"
                );
                std::thread::yield_now();
            }
            let plan = state.workspace_batch_plan.as_ref().unwrap();
            let outstanding_files = plan.file_ids.len() - plan.next_chunk * plan.chunk_size;
            assert!(outstanding_files > 0);
            assert!(crate::handlers::workspace_batch::g1_batch_is_paused());
            tracing::info!(
                g1_marker = "workspace_batch_restarted_active",
                generation = restarted_generation,
                next_chunk = plan.next_chunk,
                total_chunks = plan.num_chunks,
                outstanding_files,
            );

            let active_receiver = state.task_pool.receiver.clone();
            let active_response = g1_framed_request_response(
                &mut state,
                g1_definition_request(403, lsp_types::Url::from_file_path(&caller).unwrap()),
                &active_receiver,
                &client_receiver,
            );
            assert!(crate::handlers::workspace_batch::g1_batch_is_paused());
            assert!(state.workspace_batch_in_flight);
            assert!(state.vfs_done);
            assert_eq!(
                &*state.analysis_host.raw_database().file_text(batch_file_id),
                changed_batch_text,
                "the changed VFS contents stay applied through the framed response"
            );
            assert!(
                active_response.error.is_none(),
                "framed semantic request failed: {active_response:?}"
            );
            let active_result = active_response.result.expect("framed definition returns a result");
            let active_location =
                if active_result.is_array() { &active_result[0] } else { &active_result };
            assert_eq!(active_location, &control_location);
            let plan = state.workspace_batch_plan.as_ref().unwrap();
            assert!(plan.next_chunk < plan.num_chunks, "batch work remains at framed response");
            tracing::info!(
                g1_marker = "framed_semantic_parity_with_outstanding_batch",
                matches_control = true,
                batch_in_flight = state.workspace_batch_in_flight,
                worker_paused = crate::handlers::workspace_batch::g1_batch_is_paused(),
                writer_contents_live = true,
                next_chunk = plan.next_chunk,
                total_chunks = plan.num_chunks,
                outstanding_files,
            );

            crate::handlers::workspace_batch::g1_batch_barrier(false);
            let mut batch_publish_counts = std::collections::HashMap::<String, usize>::new();
            while state.workspace_batch_plan.is_some() {
                if !state.workspace_batch_in_flight {
                    crate::handlers::spawn_workspace_batch(&mut state);
                }
                let task = state
                    .task_pool
                    .receiver
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("active batch completion task timed out");
                let completed_chunk = matches!(task, Task::WorkspaceBatchChunk { .. });
                handle_task(&mut state, task).unwrap();
                if completed_chunk {
                    tracing::info!(
                        g1_marker = "workspace_batch_chunk_completed",
                        plan_still_active = state.workspace_batch_plan.is_some()
                    );
                }
                while let Ok(message) = client_receiver.try_recv() {
                    if let Message::Notification(notification) = message {
                        if notification.method == "textDocument/publishDiagnostics" {
                            let params: lsp_types::PublishDiagnosticsParams =
                                serde_json::from_value(notification.params).unwrap();
                            *batch_publish_counts.entry(params.uri.to_string()).or_default() += 1;
                        }
                    }
                }
            }
            release_batch_on_drop.wait_for_completion = false;
            assert!(!state.workspace_batch_in_flight);
            assert!(
                batch_publish_counts.values().all(|count| *count <= 1),
                "one live batch must not publish diagnostics twice for a URI: {batch_publish_counts:?}"
            );
            tracing::info!(
                g1_marker = "workspace_batch_single_publication_per_uri",
                published_uris = batch_publish_counts.len(),
                max_publications = batch_publish_counts.values().copied().max().unwrap_or(0),
            );
            tracing::info!(g1_marker = "workspace_batch_final_release", completed = true);

            let reload_receiver = state.task_pool.receiver.clone();
            RequestDispatcher {
                req: Some(g1_definition_request(
                    405,
                    lsp_types::Url::from_file_path(&caller).unwrap(),
                )),
                global_state: &mut state,
            }
            .on_retryable_latency::<lsp_types::request::GotoDefinition>(g1_definition)
            .finish();
            let request_id = lsp_server::RequestId::from(405);
            let origin = state.request_lifecycles[&request_id]
                .origin
                .clone()
                .expect("open-document retry captures a source origin");

            let new_config =
                "[source]\nroot = \".\"\n\n[features]\nworkspaceDiagnostics = \"off\"\n";
            let old_workspace_root = state.workspace_root.clone();
            let old_source_roots = state.project.as_ref().unwrap().source_roots();
            let temporary_config = root.path().join("bsl-analyzer.toml.next");
            std::fs::write(&temporary_config, new_config).unwrap();
            std::fs::rename(&temporary_config, &config_path).unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Changed {
                    files: vec![(abs(&config_path), Some(new_config.as_bytes().to_vec()))],
                },
            )
            .unwrap();
            assert_eq!(
                state.project.as_ref().unwrap().config.features.workspace_diagnostics,
                project_model::WorkspaceDiagnosticsScope::Off
            );
            let reloaded_source_roots = state.project.as_ref().unwrap().source_roots();
            assert_eq!(state.workspace_root, old_workspace_root);
            assert_eq!(reloaded_source_roots, old_source_roots);
            assert!(request_origin_matches(&state, &origin));
            tracing::info!(g1_marker = "config_only_reload_preserves_request_origin");
            assert!(state.vfs_done, "atomic live config reload must preserve readiness");
            let reload_config_version = state.vfs_progress_config_version;
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Progress {
                    n_total: 2,
                    n_done: vfs::loader::LoadingProgress::Started,
                    dir: None,
                    config_version: reload_config_version,
                },
            )
            .unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Progress {
                    n_total: 2,
                    n_done: vfs::loader::LoadingProgress::Progress(2),
                    dir: None,
                    config_version: reload_config_version,
                },
            )
            .unwrap();
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Progress {
                    n_total: 2,
                    n_done: vfs::loader::LoadingProgress::Finished,
                    dir: None,
                    config_version: reload_config_version,
                },
            )
            .unwrap();
            assert!(state.vfs_done, "finished live reload must keep ready open");
            tracing::info!(g1_marker = "atomic_config_reload_ready", scope = "off", ready = true);

            let final_response =
                g1_wait_for_result(&mut state, &reload_receiver, &client_receiver, request_id);
            assert!(
                final_response.error.is_none(),
                "post-reload definition failed: {final_response:?}"
            );
            let final_result = final_response.result.expect("post-reload definition resolves");
            let final_location =
                if final_result.is_array() { &final_result[0] } else { &final_result };
            assert_eq!(final_location, &control_location);
            tracing::info!(g1_marker = "post_reload_definition", matches_control = true);
            g1_wait_for_task_pool_quiescence(&mut state, &reload_receiver);
        });
    }

    #[test]
    fn g1_retry_limit_and_origin_guard_are_terminal() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("Caller.bsl");
        std::fs::write(&path, "Процедура X() КонецПроцедуры").unwrap();
        state.workspace_root = Some(root.path().to_path_buf());
        state.project = Some(project_model::Project::new(root.path()).unwrap());
        let uri = lsp_types::Url::from_file_path(path).unwrap();
        state.mem_docs.insert(uri.clone(), "Процедура X() КонецПроцедуры".to_owned(), 4);
        let origin = crate::global_state::RequestOriginGuard {
            uri: uri.clone(),
            identity: state.mem_docs.identity(&uri).unwrap(),
            workspace_root: state.workspace_root.clone(),
            source_roots: state.project.as_ref().unwrap().source_roots(),
        };
        let id = lsp_server::RequestId::from(601);
        let request = g1_definition_request(601, uri.clone());
        let request_key = |attempt| crate::global_state::RequestAttemptKey {
            id: id.clone(),
            generation: 19,
            attempt,
        };
        state.request_lifecycles.insert(
            id.clone(),
            crate::global_state::RequestLifecycle {
                generation: 19,
                method: lsp_types::request::GotoDefinition::METHOD.to_owned(),
                request: Some(request),
                attempt: 1,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: Some(origin.clone()),
                client_cancelled: false,
            },
        );

        let revision = salsa::plumbing::current_revision(state.analysis_host.raw_database());
        let mut queued = 0;
        for attempt in 1..=3 {
            let key = request_key(attempt);
            assert!(request_origin_matches(&state, &origin));
            let accepted = queue_latency_retry(
                &mut state,
                &key,
                lsp_types::request::GotoDefinition::METHOD,
                "test-attempt-budget",
                revision,
                revision,
                false,
            );
            if attempt < MAX_LATENCY_ATTEMPTS {
                assert_eq!(
                    accepted,
                    RetryQueueResult::Queued,
                    "attempts one and two may queue a fresh context"
                );
                queued += 1;
                let retry = state.pending_latency_retries.pop_front().unwrap();
                assert_eq!(retry, request_key(attempt + 1));
                let lifecycle = state.request_lifecycles.get_mut(&id).unwrap();
                lifecycle.attempt = attempt + 1;
                lifecycle.phase = RequestLifecyclePhase::Running;
            } else {
                assert_eq!(
                    accepted,
                    RetryQueueResult::Refused(RetryRefusal::AttemptLimit),
                    "attempt three must be terminal"
                );
                finish_latency_request(
                    &mut state,
                    &key,
                    Response::new_err(
                        id.clone(),
                        lsp_server::ErrorCode::ContentModified as i32,
                        RetryRefusal::AttemptLimit.message().to_owned(),
                    ),
                );
            }
        }
        assert_eq!(queued, 2);
        assert!(state.request_lifecycles.is_empty());
        let response = recv_queued_client_response(&receiver, id.clone());
        assert_eq!(response.id, id);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(lsp_server::ErrorCode::ContentModified as i32)
        );
        assert!(receiver.try_recv().is_err(), "only one terminal response is sent");

        let reopened_id = lsp_server::RequestId::from(602);
        let retained_request = g1_definition_request(602, uri.clone());
        let stale_origin = crate::global_state::RequestOriginGuard {
            identity: state.mem_docs.identity(&uri).unwrap(),
            ..origin.clone()
        };
        state.request_lifecycles.insert(
            reopened_id.clone(),
            crate::global_state::RequestLifecycle {
                generation: 20,
                method: lsp_types::request::GotoDefinition::METHOD.to_owned(),
                request: Some(retained_request),
                attempt: 1,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: Some(stale_origin.clone()),
                client_cancelled: false,
            },
        );
        let previous_identity = state.mem_docs.identity(&uri).unwrap();
        state.mem_docs.remove(&uri);
        state.mem_docs.insert(uri.clone(), "Процедура X() КонецПроцедуры".to_owned(), 4);
        assert_ne!(state.mem_docs.identity(&uri), Some(previous_identity));
        assert!(
            !request_origin_matches(&state, &stale_origin),
            "same-text, same-version close/reopen changes the document incarnation"
        );
        let reopened_key = crate::global_state::RequestAttemptKey {
            id: reopened_id.clone(),
            generation: 20,
            attempt: 1,
        };
        assert_eq!(
            queue_latency_retry(
                &mut state,
                &reopened_key,
                lsp_types::request::GotoDefinition::METHOD,
                "test-origin-changed",
                revision,
                revision,
                false,
            ),
            RetryQueueResult::Refused(RetryRefusal::InvalidOrigin)
        );
        finish_latency_request(
            &mut state,
            &reopened_key,
            Response::new_err(
                reopened_id,
                lsp_server::ErrorCode::ContentModified as i32,
                "content modified".to_owned(),
            ),
        );
        assert!(state.pending_latency_retries.is_empty());
        let _reopened = recv_queued_client_response(&receiver, lsp_server::RequestId::from(602));

        let unopened_id = lsp_server::RequestId::from(603);
        state.request_lifecycles.insert(
            unopened_id.clone(),
            crate::global_state::RequestLifecycle {
                generation: 21,
                method: lsp_types::request::GotoDefinition::METHOD.to_owned(),
                request: Some(g1_definition_request(603, uri.clone())),
                attempt: 1,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: None,
                client_cancelled: false,
            },
        );
        let unopened_key = crate::global_state::RequestAttemptKey {
            id: unopened_id.clone(),
            generation: 21,
            attempt: 1,
        };
        assert_eq!(
            queue_latency_retry(
                &mut state,
                &unopened_key,
                lsp_types::request::GotoDefinition::METHOD,
                "test-untracked-origin",
                revision,
                revision,
                false,
            ),
            RetryQueueResult::Refused(RetryRefusal::UntrackedOrigin),
            "a request with no captured document is not reported as a changed origin"
        );
        state.request_lifecycles.remove(&unopened_id);
    }

    /// A queued retry the dispatcher declines before admitting it — here because the
    /// workspace is loading again — is answered by the decline, and its lifecycle must close
    /// with it: a lifecycle left behind counts against the shared admission bound forever.
    #[test]
    fn a_retry_declined_before_admission_closes_its_lifecycle() {
        let (_root, mut state, client_receiver, _task_receiver, retry, _active_token) =
            g1_session_boundary_state(lsp_server::RequestId::from(7420));
        while client_receiver.try_recv().is_ok() {}
        state.vfs_done = false;

        admit_pending_latency_retries(&mut state).unwrap();

        let response = std::iter::from_fn(|| client_receiver.try_recv().ok())
            .find_map(|message| match message {
                Message::Response(response) if response.id == retry.id => Some(response),
                _ => None,
            })
            .expect("the declined retry is answered");
        assert_eq!(
            response.error.map(|error| error.code),
            Some(lsp_server::ErrorCode::ContentModified as i32)
        );
        assert!(
            !state.request_lifecycles.contains_key(&retry.id),
            "the declined retry closes its lifecycle"
        );
    }

    #[test]
    fn g1_shutdown_branch_does_not_admit_valid_queued_retry_and_cleans_session() {
        g1_with_tracing(|| {
            let (_root, mut state, client_receiver, task_receiver, retry, active_token) =
                g1_session_boundary_state(lsp_server::RequestId::from(7400));
            assert!(state.task_pool.pool.has_capacity());
            assert_eq!(state.pending_latency_retries.front(), Some(&retry));
            assert_eq!(
                state.request_lifecycles[&retry.id].phase,
                RequestLifecyclePhase::RetryQueued
            );
            assert!(request_origin_matches(
                &state,
                state.request_lifecycles[&retry.id].origin.as_ref().unwrap()
            ));
            assert_eq!(retry.attempt, 2);

            let (input_sender, input_receiver) = crossbeam_channel::unbounded();
            let shutdown_id = lsp_server::RequestId::from(7402);
            input_sender
                .send(Message::Request(lsp_server::Request::new(
                    shutdown_id.clone(),
                    Shutdown::METHOD.to_owned(),
                    serde_json::Value::Null,
                )))
                .unwrap();
            run_event_loop(&mut state, &input_receiver)
                .expect("actual shutdown request branch exits cleanly");
            assert!(state.shutdown_requested);
            assert_eq!(state.pending_latency_retries.front(), Some(&retry));
            assert_eq!(
                state.request_lifecycles[&retry.id].phase,
                RequestLifecyclePhase::RetryQueued
            );
            assert_eq!(state.request_lifecycles[&retry.id].attempt, 2);

            let shutdown_response = client_receiver
                .try_iter()
                .find_map(|message| match message {
                    Message::Response(response) if response.id == shutdown_id => Some(response),
                    Message::Response(response) => {
                        panic!("shutdown loop sent unexpected response {:?}", response.id)
                    }
                    _ => None,
                })
                .expect("shutdown request receives its protocol response");
            assert!(shutdown_response.error.is_none());
            cancel_latency_requests_on_shutdown(&mut state);
            assert!(active_token.is_cancelled(), "session cleanup cancels remaining handles");
            assert!(state.request_tokens.is_empty());
            assert!(state.request_token_owners.is_empty());
            assert!(state.request_lifecycles.is_empty());
            assert!(state.pending_latency_retries.is_empty());
            let (drained, _) = g1_drain_task_pool_to_fixed_point(&mut state, &task_receiver);
            assert_eq!(drained, 0, "shutdown never admitted a second request attempt");
            assert!(client_receiver.try_recv().is_err(), "no retry response follows shutdown");
            tracing::info!(
                g1_marker = "shutdown_skipped_queued_retry_and_cleaned_session",
                request_id = ?retry.id,
                queued_attempt = retry.attempt,
                shutdown_response = true,
                request_retry_started = false,
                lifecycle_cleanup = true,
                token_cancelled = true,
            );
            drop(input_sender);
        });
    }

    #[test]
    fn g1_disconnected_client_skips_valid_queued_retry_and_cleans_session() {
        g1_with_tracing(|| {
            let (_root, mut state, client_receiver, task_receiver, retry, active_token) =
                g1_session_boundary_state(lsp_server::RequestId::from(7410));
            assert!(state.task_pool.pool.has_capacity());
            assert_eq!(state.pending_latency_retries.front(), Some(&retry));
            assert!(request_origin_matches(
                &state,
                state.request_lifecycles[&retry.id].origin.as_ref().unwrap()
            ));

            let (input_sender, input_receiver) = crossbeam_channel::unbounded();
            drop(input_sender);
            assert!(
                run_event_loop(&mut state, &input_receiver).is_err(),
                "a disconnected LSP input channel exits the production event loop"
            );
            assert_eq!(state.pending_latency_retries.front(), Some(&retry));
            assert_eq!(
                state.request_lifecycles[&retry.id].phase,
                RequestLifecyclePhase::RetryQueued
            );
            assert_eq!(state.request_lifecycles[&retry.id].attempt, 2);

            cancel_latency_requests_on_shutdown(&mut state);
            assert!(active_token.is_cancelled(), "disconnect cleanup cancels remaining handles");
            assert!(state.request_tokens.is_empty());
            assert!(state.request_token_owners.is_empty());
            assert!(state.request_lifecycles.is_empty());
            assert!(state.pending_latency_retries.is_empty());
            let (drained, _) = g1_drain_task_pool_to_fixed_point(&mut state, &task_receiver);
            assert_eq!(drained, 0, "disconnect never admitted a second request attempt");
            assert!(client_receiver.try_recv().is_err(), "disconnect sends no retry response");
            tracing::info!(
                g1_marker = "disconnect_skipped_queued_retry_and_cleaned_session",
                request_id = ?retry.id,
                queued_attempt = retry.attempt,
                request_retry_started = false,
                lifecycle_cleanup = true,
                token_cancelled = true,
            );
        });
    }

    #[test]
    fn g1_workspace_diagnostic_real_partial_is_terminal_after_pending_write() {
        g1_with_tracing(|| {
            use base_db::{SourceDatabase as _, SourceRoot, BSL_SOURCE_ROOT};
            use lsp_types::request::{Request as _, WorkspaceDiagnosticRequest};

            let root = tempfile::tempdir().expect("create workspace-diagnostic fixture");
            let config = root.path().join("bsl-analyzer.toml");
            std::fs::write(
                &config,
                "[source]\nroot = \".\"\n\n[features]\nworkspaceDiagnostics = \"all\"\n",
            )
            .expect("write workspace-diagnostic project config");

            let caller = root.path().join("Caller.bsl");
            let caller_text =
                "Процедура TargetProcedure()\nКонецПроцедуры\n\nПроцедура Caller()\n    TargetProcedure();\nКонецПроцедуры\n";
            std::fs::write(&caller, caller_text).expect("write semantic control source");

            let mut files = vec![(caller.clone(), caller_text.to_owned())];
            for index in 0..63 {
                let path = root.path().join(format!("Broken{index:02}.bsl"));
                let text = format!(
                    "Процедура Broken{index}() Экспорт\n    Значение = ;\nКонецПроцедуры\n"
                );
                std::fs::write(&path, &text).expect("write real diagnostic source");
                files.push((path, text));
            }
            let writer = root.path().join("Writer.bsl");
            let writer_text = "Процедура WriterProcedure()\nКонецПроцедуры\n";
            std::fs::write(&writer, writer_text).expect("write cancellation target");
            files.push((writer.clone(), writer_text.to_owned()));
            assert_eq!(files.len(), 65);

            let (sender, client_receiver) = crossbeam_channel::unbounded();
            let mut state = crate::global_state::GlobalState::new(sender);
            *state.analysis_host.raw_database_mut() =
                ide_db::RootDatabaseImpl::new_with_salsa_events();
            state.init_empty_source_root();
            state.workspace_root = Some(root.path().to_path_buf());
            state.project =
                Some(project_model::Project::new(root.path()).expect("load diagnostics project"));
            assert_eq!(
                state.project.as_ref().unwrap().config.features.workspace_diagnostics,
                project_model::WorkspaceDiagnosticsScope::All
            );

            let abs = |path: &std::path::Path| paths::AbsPathBuf::assert_utf8(path.to_path_buf());
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Loaded {
                    files: files
                        .iter()
                        .map(|(path, text)| (abs(path), Some(text.as_bytes().to_vec())))
                        .collect(),
                },
            )
            .expect("load all diagnostic inputs through production VFS handler");
            let mut file_set = vfs::file_set::FileSet::new();
            let file_ids: Vec<_> = files
                .iter()
                .map(|(path, _)| {
                    let vfs_path = vfs::VfsPath::new(path.as_path());
                    let file_id = state.vfs.read().file_id(&vfs_path).expect("loaded file id");
                    file_set.insert(file_id, vfs_path);
                    file_id
                })
                .collect();
            {
                let db = state.analysis_host.raw_database_mut();
                db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
                for file_id in file_ids {
                    db.set_file_source_root(file_id, BSL_SOURCE_ROOT);
                }
            }
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Progress {
                    n_total: files.len(),
                    n_done: vfs::loader::LoadingProgress::Finished,
                    dir: None,
                    config_version: 0,
                },
            )
            .expect("mark diagnostics fixture loaded");
            assert!(state.vfs_done);
            let caller_uri = lsp_types::Url::from_file_path(&caller).unwrap();
            state.mem_docs.insert(caller_uri, caller_text.to_owned(), 1);
            let caller_id = state
                .vfs
                .read()
                .file_id(&vfs::VfsPath::new(caller.clone()))
                .expect("caller VFS id");
            state.open_files.insert(caller_id);

            let request_id = lsp_server::RequestId::from(7420);
            let partial_token = lsp_types::ProgressToken::String("g1/partial".to_owned());
            let work_token = lsp_types::ProgressToken::String("g1/work".to_owned());
            let (gate, entered) = crate::handlers::request::g1_arm_workspace_diagnostic_chunk_gate(
                partial_token.clone(),
                64,
            );
            let task_receiver = state.task_pool.receiver.clone();
            handle_lsp_msg(
                &mut state,
                Message::Request(g1_json_request(
                    7420,
                    WorkspaceDiagnosticRequest::METHOD,
                    serde_json::json!({
                        "previousResultIds": [],
                        "partialResultToken": partial_token,
                        "workDoneToken": work_token,
                    }),
                )),
            )
            .expect("production dispatcher admits workspace diagnostic request");

            let (done, emitted_items) = entered
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("real handler sends its first partial then reaches test gate");
            assert_eq!(done, 64);
            assert!(emitted_items > 0, "partial contains real diagnostics");
            tracing::info!(
                g1_marker = "workspace_diagnostics_real_partial_active",
                request_id = ?request_id,
                files_done = done,
                diagnostic_items = emitted_items,
                more_files_pending = files.len() - done,
            );

            let mut progress_values = Vec::new();
            while let Ok(message) = client_receiver.try_recv() {
                if let Message::Notification(notification) = message {
                    if notification.method == "$/progress" {
                        progress_values.push(notification.params);
                    }
                }
            }
            let partial = progress_values
                .iter()
                .find(|params| params["token"] == serde_json::json!("g1/partial"))
                .expect("real nonempty partial-result progress was sent before the VFS write");
            assert_eq!(partial["value"]["items"].as_array().unwrap().len(), emitted_items);
            assert!(emitted_items > 0);
            let initial_work_events: Vec<_> = progress_values
                .iter()
                .filter(|params| params["token"] == serde_json::json!("g1/work"))
                .collect();
            assert_eq!(initial_work_events.len(), 2, "one Begin and one Report preceded writer");

            let db = state.analysis_host.raw_database().clone();
            let cancellation_baseline = db.salsa_event_global().unwrap().set_cancellation;
            let replacement =
                "Процедура WriterProcedure()\n    Значение = 1;\nКонецПроцедуры\n".to_owned();
            std::fs::write(&writer, &replacement).expect("persist changed writer contents");
            let writer_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let writer_finished_observer = Arc::clone(&writer_finished);
            let release_gate = gate.release_sender();
            let observer_request_id = request_id.clone();
            let pending_files = files.len() - done;
            let observer_dispatch = tracing::dispatcher::get_default(Clone::clone);
            let observer = std::thread::spawn(move || {
                tracing::dispatcher::with_default(&observer_dispatch, || {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    let observed_cancellation = loop {
                        let current = db.salsa_event_global().unwrap().set_cancellation;
                        if current > cancellation_baseline {
                            break current;
                        }
                        assert!(
                            std::time::Instant::now() < deadline,
                            "real VFS writer did not publish a Salsa cancellation event"
                        );
                        std::thread::yield_now();
                    };
                    assert!(
                    !writer_finished_observer.load(Ordering::Acquire),
                    "VFS Changed handler remains blocked while the real query holds its snapshot"
                );
                    tracing::info!(
                        g1_marker = "workspace_diagnostics_vfs_writer_active",
                        request_id = ?observer_request_id,
                        cancellation = observed_cancellation,
                        writer_returned = false,
                        remaining_files = pending_files,
                    );
                    drop(db);
                    release_gate.send(()).expect("test releases the parked workspace handler");
                    observed_cancellation
                })
            });
            handle_loader_msg(
                &mut state,
                vfs::loader::Message::Changed {
                    files: vec![(abs(&writer), Some(replacement.as_bytes().to_vec()))],
                },
            )
            .expect("production VFS writer finishes after the real handler releases its ctx");
            writer_finished.store(true, Ordering::Release);
            let observed_cancellation =
                observer.join().expect("cancellation observer did not panic");
            tracing::info!(
                g1_marker = "workspace_diagnostics_vfs_writer_complete",
                request_id = ?request_id,
                cancellation = observed_cancellation,
            );

            let deadline = std::time::Instant::now() + HANG_WATCHDOG;
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "cancelled workspace diagnostic did not yield its terminal task"
                );
                let task = task_receiver
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .expect("cancelled workspace diagnostic did not yield its terminal task");
                if let Task::LatencyRequestFinished { key, outcome } = task {
                    if key.id == request_id {
                        assert_eq!(key.attempt, 1);
                        assert!(matches!(
                            &outcome,
                            LatencyAttemptOutcome::Cancelled {
                                kind: salsa::Cancelled::PendingWrite,
                                ..
                            }
                        ));
                        handle_task(&mut state, Task::LatencyRequestFinished { key, outcome })
                            .expect(
                                "production consumer rejects retry after streamed partial output",
                            );
                        break;
                    }
                    handle_task(&mut state, Task::LatencyRequestFinished { key, outcome })
                        .expect("apply other latency completion through production consumer");
                } else {
                    handle_task(&mut state, task)
                        .expect("apply non-target background task through production consumer");
                }
            }

            let response = loop {
                if let Ok(message) =
                    client_receiver.recv_timeout(std::time::Duration::from_secs(10))
                {
                    match message {
                        Message::Response(response) if response.id == request_id => break response,
                        Message::Notification(notification)
                            if notification.method == "$/progress" =>
                        {
                            progress_values.push(notification.params);
                        }
                        _ => {}
                    }
                } else {
                    panic!("workspace diagnostic cancellation sent no terminal response");
                }
            };
            assert_eq!(
                response.error.as_ref().map(|error| error.code),
                Some(lsp_server::ErrorCode::ContentModified as i32)
            );
            assert_eq!(
                response.error.as_ref().unwrap().message,
                "content modified: automatic retry is not permitted"
            );
            assert!(state.request_lifecycles.is_empty());
            assert!(state.request_tokens.is_empty());
            assert!(state.pending_latency_retries.is_empty());
            while let Ok(message) = client_receiver.try_recv() {
                if let Message::Notification(notification) = message {
                    if notification.method == "$/progress" {
                        progress_values.push(notification.params);
                    }
                } else if let Message::Response(other) = message {
                    assert_ne!(other.id, request_id, "one terminal response, no retry attempt");
                }
            }
            let partials: Vec<_> = progress_values
                .iter()
                .filter(|params| params["token"] == serde_json::json!("g1/partial"))
                .collect();
            assert_eq!(partials.len(), 1, "one actual partial chunk; no duplicate partial");
            assert!(!partials[0]["value"]["items"].as_array().unwrap().is_empty());
            let work_events: Vec<_> = progress_values
                .iter()
                .filter(|params| params["token"] == serde_json::json!("g1/work"))
                .collect();
            assert_eq!(work_events.len(), 3, "Begin, Report, and End close streamed work");
            assert_eq!(work_events[0]["value"]["kind"], "begin");
            assert_eq!(work_events[1]["value"]["kind"], "report");
            assert_eq!(work_events[2]["value"]["kind"], "end");
            let writer_id = state
                .vfs
                .read()
                .file_id(&vfs::VfsPath::new(writer.clone()))
                .expect("writer remains in the VFS");
            assert_eq!(
                state.analysis_host.raw_database().file_text(writer_id).as_ref(),
                replacement.as_str(),
                "actual writer contents were applied"
            );
            assert_eq!(
                state.mem_docs.get(&lsp_types::Url::from_file_path(&caller).unwrap()).as_deref(),
                Some(caller_text),
                "unrelated open editor buffer remains authoritative"
            );
            let (drained, _) = g1_drain_task_pool_to_fixed_point(&mut state, &task_receiver);
            tracing::info!(
                g1_marker = "workspace_diagnostics_policy_disabled_terminal",
                request_id = ?request_id,
                attempts = 1,
                partial_chunks = partials.len(),
                work_done_events = work_events.len(),
                drained_background_tasks = drained,
            );
            drop(gate);
        });
    }

    #[test]
    fn g1_stale_completion_preserves_new_owner_and_shutdown_clears_retry_state() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        let id = lsp_server::RequestId::from(603);
        let current =
            crate::global_state::RequestAttemptKey { id: id.clone(), generation: 22, attempt: 2 };
        let stale =
            crate::global_state::RequestAttemptKey { id: id.clone(), generation: 21, attempt: 1 };
        let token = state.analysis_host.raw_database().cancellation_token();
        state.request_tokens.insert(id.clone(), token);
        state.request_token_owners.insert(id.clone(), current.clone());
        state.request_lifecycles.insert(
            id.clone(),
            crate::global_state::RequestLifecycle {
                generation: current.generation,
                method: lsp_types::request::GotoDefinition::METHOD.to_owned(),
                request: None,
                attempt: current.attempt,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: false,
                origin: None,
                client_cancelled: false,
            },
        );
        handle_task(
            &mut state,
            Task::LatencyRequestFinished {
                key: stale,
                outcome: crate::global_state::LatencyAttemptOutcome::Response(Response::new_ok(
                    id.clone(),
                    serde_json::json!({"stale": true}),
                )),
            },
        )
        .unwrap();
        assert_eq!(state.request_token_owners.get(&id), Some(&current));
        assert!(state.request_tokens.contains_key(&id));
        assert!(receiver.try_recv().is_err(), "stale completion has no wire response");

        handle_task(
            &mut state,
            Task::LatencyRequestFinished {
                key: current.clone(),
                outcome: crate::global_state::LatencyAttemptOutcome::Response(Response::new_ok(
                    id.clone(),
                    serde_json::json!({"current": true}),
                )),
            },
        )
        .unwrap();
        assert!(state.request_lifecycles.is_empty());
        assert!(!state.request_tokens.contains_key(&id));
        let response = recv_queued_client_response(&receiver, id.clone());
        assert_eq!(response.result, Some(serde_json::json!({"current": true})));
        assert!(receiver.try_recv().is_err());

        let shutdown_id = lsp_server::RequestId::from(604);
        let shutdown_key = crate::global_state::RequestAttemptKey {
            id: shutdown_id.clone(),
            generation: 23,
            attempt: 2,
        };
        state.request_token_owners.insert(shutdown_id.clone(), shutdown_key.clone());
        state
            .request_tokens
            .insert(shutdown_id.clone(), state.analysis_host.raw_database().cancellation_token());
        state.request_lifecycles.insert(
            shutdown_id.clone(),
            crate::global_state::RequestLifecycle {
                generation: shutdown_key.generation,
                method: lsp_types::request::GotoDefinition::METHOD.to_owned(),
                request: None,
                attempt: shutdown_key.attempt,
                phase: RequestLifecyclePhase::RetryQueued,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: None,
                client_cancelled: false,
            },
        );
        state.pending_latency_retries.push_back(shutdown_key);
        cancel_latency_requests_on_shutdown(&mut state);
        assert!(state.request_tokens.is_empty());
        assert!(state.request_token_owners.is_empty());
        assert!(state.request_lifecycles.is_empty());
        assert!(state.pending_latency_retries.is_empty());
    }

    #[test]
    fn g1_origin_guard_rejects_deleted_and_config_excluded_documents() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.init_empty_source_root();
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        std::fs::create_dir_all(source.join("excluded")).unwrap();
        let deleted_path = source.join("Deleted.bsl");
        let excluded_path = source.join("excluded/Caller.bsl");
        std::fs::write(&deleted_path, "Процедура X() КонецПроцедуры").unwrap();
        std::fs::write(&excluded_path, "Процедура Y() КонецПроцедуры").unwrap();
        let config_path = root.path().join("bsl-analyzer.toml");
        std::fs::write(&config_path, "[source]\nroot = \"src\"\n").unwrap();
        state.workspace_root = Some(root.path().to_path_buf());
        state.project = Some(project_model::Project::new(root.path()).unwrap());

        let make_origin = |state: &crate::global_state::GlobalState, uri: lsp_types::Url| {
            crate::global_state::RequestOriginGuard {
                identity: state.mem_docs.identity(&uri).unwrap(),
                workspace_root: state.workspace_root.clone(),
                source_roots: state.project.as_ref().unwrap().source_roots(),
                uri,
            }
        };
        let deleted_uri = lsp_types::Url::from_file_path(&deleted_path).unwrap();
        state.mem_docs.insert(deleted_uri.clone(), "Процедура X() КонецПроцедуры".to_owned(), 1);
        let deleted_vfs_path = vfs::VfsPath::new(deleted_path.clone());
        state.vfs.write().set_file_contents(
            deleted_vfs_path.clone(),
            Some(std::sync::Arc::from("Процедура X() КонецПроцедуры")),
        );
        let deleted_file_id = state.vfs.read().file_id(&deleted_vfs_path).unwrap();
        state.open_files.insert(deleted_file_id);
        let deleted_origin = make_origin(&state, deleted_uri.clone());
        assert!(request_origin_matches(&state, &deleted_origin));
        std::fs::remove_file(&deleted_path).unwrap();
        handle_loader_msg(
            &mut state,
            vfs::loader::Message::Changed {
                files: vec![(paths::AbsPathBuf::assert_utf8(deleted_path.clone()), None)],
            },
        )
        .unwrap();
        assert_eq!(
            state.mem_docs.get(&deleted_uri).as_deref(),
            Some("Процедура X() КонецПроцедуры"),
            "deletion must invalidate origin while preserving the unsaved editor buffer"
        );
        assert!(!request_origin_matches(&state, &deleted_origin));
        let deleted_id = lsp_server::RequestId::from(605);
        let deleted_key = crate::global_state::RequestAttemptKey {
            id: deleted_id.clone(),
            generation: 1,
            attempt: 1,
        };
        state.request_lifecycles.insert(
            deleted_id.clone(),
            crate::global_state::RequestLifecycle {
                generation: 1,
                method: lsp_types::request::GotoDefinition::METHOD.to_owned(),
                request: Some(g1_definition_request(605, deleted_uri)),
                attempt: 1,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: Some(deleted_origin),
                client_cancelled: false,
            },
        );
        let revision = salsa::plumbing::current_revision(state.analysis_host.raw_database());
        assert_eq!(
            queue_latency_retry(
                &mut state,
                &deleted_key,
                lsp_types::request::GotoDefinition::METHOD,
                "pending-write",
                revision,
                revision,
                false,
            ),
            RetryQueueResult::Refused(RetryRefusal::InvalidOrigin)
        );
        state.request_lifecycles.remove(&deleted_id);

        let excluded_uri = lsp_types::Url::from_file_path(&excluded_path).unwrap();
        state.mem_docs.insert(excluded_uri.clone(), "Процедура Y() КонецПроцедуры".to_owned(), 1);
        let excluded_origin = make_origin(&state, excluded_uri.clone());
        assert!(request_origin_matches(&state, &excluded_origin));
        std::fs::write(&config_path, "[source]\nroot = \"src\"\nexclude = [\"src/excluded\"]\n")
            .unwrap();
        assert!(state.reload_project_config());
        assert_eq!(
            state.project.as_ref().unwrap().source_roots(),
            excluded_origin.source_roots,
            "subdirectory exclusion must not rely on a root-list change"
        );
        assert!(!request_origin_matches(&state, &excluded_origin));
        let excluded_id = lsp_server::RequestId::from(606);
        let excluded_key = crate::global_state::RequestAttemptKey {
            id: excluded_id.clone(),
            generation: 1,
            attempt: 1,
        };
        state.request_lifecycles.insert(
            excluded_id.clone(),
            crate::global_state::RequestLifecycle {
                generation: 1,
                method: lsp_types::request::GotoDefinition::METHOD.to_owned(),
                request: Some(g1_definition_request(606, excluded_uri)),
                attempt: 1,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: Some(excluded_origin),
                client_cancelled: false,
            },
        );
        assert_eq!(
            queue_latency_retry(
                &mut state,
                &excluded_key,
                lsp_types::request::GotoDefinition::METHOD,
                "pending-write",
                revision,
                revision,
                false,
            ),
            RetryQueueResult::Refused(RetryRefusal::InvalidOrigin)
        );
    }

    #[cfg(windows)]
    #[test]
    fn g1_origin_guard_queues_retry_for_open_case_alias_after_external_delete() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.init_empty_source_root();

        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        std::fs::write(canonical_root.join("bsl-analyzer.toml"), "[source]\nroot = \".\"\n")
            .unwrap();
        let canonical_file = canonical_root.join("Caller.bsl");
        std::fs::write(&canonical_file, "Процедура X() КонецПроцедуры").unwrap();
        let alias_path = PathBuf::from(
            root.path()
                .to_string_lossy()
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
                .collect::<String>(),
        )
        .join("cALLER.BSL");
        assert!(alias_path.is_file(), "case-alias path must resolve on Windows");
        let uri = lsp_types::Url::from_file_path(&alias_path).unwrap();

        state.workspace_root = Some(canonical_root.clone());
        state.project = Some(project_model::Project::new(&canonical_root).unwrap());
        let source_roots = state.project.as_ref().unwrap().source_roots();
        assert!(source_roots.iter().any(|source_root| {
            crate::workspace::canonicalize_shared_path(source_root) == canonical_root
        }));
        let raw_alias_path = uri.to_file_path().unwrap();
        assert!(
            !source_roots.iter().any(|source_root| raw_alias_path.starts_with(source_root)),
            "the raw Windows URI must not already match the canonical project root"
        );

        let vfs_path = vfs::VfsPath::new(canonical_file.clone());
        state.vfs.write().set_file_contents(
            vfs_path.clone(),
            Some(std::sync::Arc::from("Процедура X() КонецПроцедуры")),
        );
        let file_id = state.vfs.read().file_id(&vfs_path).unwrap();
        state.open_files.insert(file_id);
        state.open_file_uri_ids.insert(uri.clone(), file_id);
        state.mem_docs.insert(uri.clone(), "Процедура X() КонецПроцедуры".to_owned(), 1);
        let identity = state.mem_docs.identity(&uri).unwrap();
        let capture_origin =
            |state: &crate::global_state::GlobalState| crate::global_state::RequestOriginGuard {
                uri: uri.clone(),
                identity: state.mem_docs.identity(&uri).unwrap(),
                workspace_root: state.workspace_root.clone(),
                source_roots: state.project.as_ref().unwrap().source_roots(),
            };
        let initial_origin = capture_origin(&state);
        assert!(request_origin_matches(&state, &initial_origin));
        std::fs::write(canonical_root.join("bsl-analyzer.toml"), "[source\n").unwrap();
        assert!(!state.reload_project_config(), "invalid config must keep the last-good Project");
        assert_eq!(state.project.as_ref().unwrap().source_roots(), source_roots);
        assert!(request_origin_matches(&state, &initial_origin));
        let canonical_uri = lsp_types::Url::from_file_path(&canonical_file).unwrap();
        assert_ne!(uri, canonical_uri, "the alias must retain a distinct raw URI");
        std::fs::remove_file(&canonical_file).unwrap();
        handle_loader_msg(
            &mut state,
            vfs::loader::Message::Changed {
                files: vec![(paths::AbsPathBuf::assert_utf8(canonical_file), None)],
            },
        )
        .unwrap();
        assert_eq!(state.open_file_uri_ids.get(&uri), Some(&file_id));
        assert!(state.open_files.contains(&file_id));
        assert_ne!(state.mem_docs.identity(&uri), Some(identity));
        assert_eq!(state.mem_docs.get(&uri).as_deref(), Some("Процедура X() КонецПроцедуры"));
        assert!(
            !request_origin_matches(&state, &initial_origin),
            "deleting the underlying FileId invalidates origins for every open URI alias"
        );

        let origin = capture_origin(&state);
        assert!(request_origin_matches(&state, &origin));

        let request_id = lsp_server::RequestId::from(907);
        let request = g1_json_request(
            907,
            lsp_types::request::HoverRequest::METHOD,
            serde_json::json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 0 }
            }),
        );
        let key = crate::global_state::RequestAttemptKey {
            id: request_id.clone(),
            generation: 1,
            attempt: 1,
        };
        state.request_lifecycles.insert(
            request_id.clone(),
            crate::global_state::RequestLifecycle {
                generation: 1,
                method: lsp_types::request::HoverRequest::METHOD.to_owned(),
                request: Some(request),
                attempt: 1,
                phase: RequestLifecyclePhase::Running,
                retry_origin_revision: None,
                retry_eligible: true,
                origin: Some(origin),
                client_cancelled: false,
            },
        );
        let revision = salsa::plumbing::current_revision(state.analysis_host.raw_database());
        assert_eq!(
            queue_latency_retry(
                &mut state,
                &key,
                lsp_types::request::HoverRequest::METHOD,
                "pending-write",
                revision,
                revision,
                false,
            ),
            RetryQueueResult::Queued,
            "the unchanged admitted alias origin permits the automatic retry"
        );
        assert_eq!(
            state.pending_latency_retries.front(),
            Some(&crate::global_state::RequestAttemptKey { attempt: 2, ..key })
        );
    }

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

    /// An editor asks for the outline, folds and selection ranges the moment a file opens —
    /// on a large configuration, long before the initial load finishes. Those requests read
    /// only the file's own parse, so they are answered right away; requests that need the
    /// rest of the workspace are still declined until the load completes.
    #[test]
    fn syntax_requests_are_answered_while_the_workspace_loads() {
        use lsp_types::request::{
            DocumentSymbolRequest, FoldingRangeRequest, HoverRequest, Request as _,
            SelectionRangeRequest,
        };

        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        state.init_empty_source_root();
        state.analysis_host.raw_database_mut().set_workspace_load_complete(false);
        assert!(!state.vfs_done);

        let text = [
            "#Область Интерфейс",
            "",
            "Процедура Тест() Экспорт",
            "\tА = 1;",
            "КонецПроцедуры",
            "",
            "#КонецОбласти",
            "",
            "Функция Ф()",
            "\tВозврат 1;",
            "КонецФункции",
        ]
        .join("\n");
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("mod.bsl");
        std::fs::write(&path, &text).expect("write");
        let uri = lsp_types::Url::from_file_path(&path).unwrap();
        crate::handlers::notification::handle_did_open(
            &mut state,
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text,
                },
            },
        )
        .unwrap();
        while receiver.try_recv().is_ok() {}

        let doc = || serde_json::json!({ "textDocument": { "uri": uri } });
        let requests = [
            (1, DocumentSymbolRequest::METHOD, doc()),
            (2, FoldingRangeRequest::METHOD, doc()),
            (
                3,
                SelectionRangeRequest::METHOD,
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "positions": [{ "line": 3, "character": 1 }],
                }),
            ),
            (
                4,
                HoverRequest::METHOD,
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": 2, "character": 11 },
                }),
            ),
        ];
        for (id, method, params) in requests {
            handle_request(&mut state, Request::new(id.into(), method.to_string(), params))
                .unwrap();
        }

        // Every answer is already on the client channel: nothing was handed to the pool,
        // where the next streamed VFS batch would have cancelled it.
        assert!(state.task_pool.receiver.try_recv().is_err(), "no pool job while loading");
        assert!(state.request_tokens.is_empty(), "no in-flight request left behind");
        let mut responses = std::collections::HashMap::new();
        while let Ok(message) = receiver.try_recv() {
            if let Message::Response(response) = message {
                responses.insert(response.id.clone(), response);
            }
        }
        let answer = |id: i32| {
            let response = &responses[&lsp_server::RequestId::from(id)];
            assert!(response.error.is_none(), "request {id} declined: {:?}", response.error);
            response.result.clone().expect("a result")
        };

        let symbols: lsp_types::DocumentSymbolResponse = serde_json::from_value(answer(1)).unwrap();
        let lsp_types::DocumentSymbolResponse::Nested(symbols) = symbols else {
            panic!("expected a hierarchical outline, got {symbols:?}");
        };
        let outline: Vec<(String, Vec<String>)> = symbols
            .iter()
            .map(|s| {
                let children = s.children.iter().flatten().map(|c| c.name.clone()).collect();
                (s.name.clone(), children)
            })
            .collect();
        assert_eq!(
            outline,
            vec![("Интерфейс".to_string(), vec!["Тест".to_string()]), ("Ф".to_string(), vec![]),]
        );
        let folds: Vec<lsp_types::FoldingRange> = serde_json::from_value(answer(2)).unwrap();
        assert!(folds.iter().any(|f| f.start_line == 0 && f.end_line == 6), "{folds:?}");
        let selections: Vec<lsp_types::SelectionRange> = serde_json::from_value(answer(3)).unwrap();
        assert_eq!(selections.len(), 1);

        let hover = &responses[&lsp_server::RequestId::from(4)];
        assert_eq!(
            hover.error.as_ref().map(|e| e.code),
            Some(lsp_server::ErrorCode::ContentModified as i32),
            "a request that needs the workspace must still wait for the load"
        );
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
            version: 1,
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
    fn diagnostics_cancelled_by_writer_requeue_only_current_open_version() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        let uri = crate::test_uri::file_uri("cancelled.bsl");
        state.mem_docs.insert(uri.clone(), "new".into(), 42);
        state.diagnostics_generation.insert(uri.clone(), 7);
        let task = |version, generation, kind| Task::DiagnosticsCancelled {
            uri: uri.clone(),
            version,
            generation,
            kind,
            completed_at: std::time::Instant::now(),
        };
        handle_task(&mut state, task(42, 7, salsa::Cancelled::PendingWrite)).unwrap();
        assert_eq!(std::mem::take(&mut state.pending_diagnostics_uris), vec![uri.clone()]);
        for cancelled in [
            task(17, 7, salsa::Cancelled::PendingWrite),
            task(42, 6, salsa::Cancelled::Local),
            task(42, 7, salsa::Cancelled::PropagatedPanic),
        ] {
            handle_task(&mut state, cancelled).unwrap();
            assert!(
                state.pending_diagnostics_uris.is_empty(),
                "stale/panicked work is not retried"
            );
        }
        state.mem_docs.remove(&uri);
        handle_task(&mut state, task(42, 7, salsa::Cancelled::PendingWrite)).unwrap();
        assert!(state.pending_diagnostics_uris.is_empty(), "closed documents are not retried");
    }

    #[test]
    fn diagnostics_keep_wire_version_and_reject_results_from_before_an_edit() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = crate::global_state::GlobalState::new(sender);
        let uri = crate::test_uri::file_uri("changed.bsl");
        state.mem_docs.insert(uri.clone(), "old".into(), 17);
        state.diagnostics_generation.insert(uri.clone(), 1);
        state
            .mem_docs
            .update_versioned(
                &uri,
                vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "new".into(),
                }],
                crate::lsp::PositionEncoding::Utf16,
                42,
            )
            .unwrap();

        let task = |version| crate::global_state::Task::DiagnosticsReady {
            uri: uri.clone(),
            diagnostics: Vec::new(),
            version,
            generation: 1,
            completed_at: std::time::Instant::now(),
        };
        handle_task(&mut state, task(17)).unwrap();
        assert!(receiver.try_recv().is_err(), "a queued older result must not publish");
        handle_task(&mut state, task(42)).unwrap();
        let Message::Notification(message) = receiver.try_recv().unwrap() else {
            panic!("expected publishDiagnostics")
        };
        let params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(message.params).unwrap();
        assert_eq!(params.version, Some(42));
        assert_eq!(params.uri, uri);
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
        assert_eq!(
            state.batch_pushed.get(&uri).map(|pushed| pushed.result_id.as_str()),
            Some("h1")
        );

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
        assert_eq!(
            state.batch_pushed.get(&uri).map(|pushed| pushed.result_id.as_str()),
            Some("h2")
        );
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
        state.batch_pushed.insert(
            gone.clone(),
            crate::global_state::BatchPush { result_id: "h1".to_string(), file_id: None },
        );
        state.batch_pushed.insert(
            kept.clone(),
            crate::global_state::BatchPush { result_id: "h1".to_string(), file_id: None },
        );

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
        state.batch_pushed.insert(
            uri.clone(),
            crate::global_state::BatchPush { result_id: "h1".to_string(), file_id: None },
        );

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
        state.batch_pushed.insert(
            uri.clone(),
            crate::global_state::BatchPush { result_id: "h1".to_string(), file_id: None },
        );
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

    /// Lifting `[source].exclude` while a document stays open puts the editor's buffer
    /// back into the view, both for a document open before the exclusion and for one
    /// opened while it was in force: edits made after that reach analysis. Each config
    /// edit is applied the way a shared workspace change applies the project the owner
    /// accepted, with the project read straight from the edited file.
    #[test]
    fn shared_reload_readmits_open_documents_the_exclusion_no_longer_covers() {
        let root = tempfile::tempdir().expect("create shared workspace");
        let config = root.path().join("bsl-analyzer.toml");
        std::fs::write(&config, "[source]\nroot = \".\"\n").expect("write project config");
        let hidden = root.path().join("Hidden");
        std::fs::create_dir(&hidden).expect("create the directory to exclude");
        let before = hidden.join("Before.bsl");
        let during = hidden.join("During.bsl");
        std::fs::write(&before, "Процедура ДискДо() КонецПроцедуры\n").expect("write source");
        std::fs::write(&during, "Процедура ДискВо() КонецПроцедуры\n").expect("write source");

        let project = mcp_server::project::at(root.path()).expect("load test workspace project");
        let cache_parent = tempfile::tempdir().expect("create disposable workspace cache");
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(cache_parent.path()),
            root.path(),
            None,
        )
        .expect("create explicit disposable cache");
        let backend = mcp_server::SharedState::shared_workspace_with_cache_and_prefixes(
            root.path().to_path_buf(),
            cache,
            None,
        )
        .expect("create disk-only owner for test workspace");
        let owner = backend.workspace_handle();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new_shared(sender, owner.clone());
        state.vfs_done = true;
        let edit_config = |state: &mut GlobalState, text: &str| {
            std::fs::write(&config, text).expect("write project config");
            let project = mcp_server::project::at(root.path()).expect("load the edited project");
            apply_shared_project(state, root.path().to_path_buf(), project);
        };
        apply_shared_project(&mut state, root.path().to_path_buf(), project);
        let open = |state: &mut GlobalState, path: &std::path::Path, text: &str| {
            let uri = lsp_types::Url::from_file_path(path).unwrap();
            crate::handlers::notification::handle_did_open(
                state,
                lsp_types::DidOpenTextDocumentParams {
                    text_document: lsp_types::TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "bsl".to_owned(),
                        version: 1,
                        text: text.to_owned(),
                    },
                },
            )
            .expect("open the document");
            uri
        };
        let before_buffer = "Процедура БуферДо() КонецПроцедуры\n";
        let during_buffer = "Процедура БуферВо() КонецПроцедуры\n";
        let before_uri = open(&mut state, &before, before_buffer);
        let before_id = crate::lsp::file_id(&state, &before_uri).expect("resolve the open URI");
        assert!(state.open_files.contains(&before_id));

        edit_config(&mut state, "[source]\nroot = \".\"\nexclude = [\"Hidden\"]\n");
        assert!(!state.open_files.contains(&before_id), "the exclusion takes the document out");
        let during_uri = open(&mut state, &during, during_buffer);

        edit_config(&mut state, "[source]\nroot = \".\"\n");

        for (uri, buffer) in [(&before_uri, before_buffer), (&during_uri, during_buffer)] {
            let file_id = crate::lsp::file_id(&state, uri).expect("resolve the readmitted URI");
            assert!(state.open_files.contains(&file_id), "{uri} is open again");
            let analysis = state.analysis_host.view().expect("open editor view").analysis();
            assert_eq!(&*analysis.file_text(file_id), buffer, "{uri} is analysed from its buffer");
        }

        let edited = "Процедура Правка() КонецПроцедуры\n";
        crate::handlers::notification::handle_did_change(
            &mut state,
            lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier {
                    uri: before_uri.clone(),
                    version: 2,
                },
                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: edited.to_owned(),
                }],
            },
        )
        .expect("edit the readmitted document");
        let analysis = state.analysis_host.view().expect("open editor view").analysis();
        assert_eq!(&*analysis.file_text(before_id), edited, "a later edit reaches analysis");
        drop(analysis);
        drop(state);
        drop(owner);
        drop(backend);
    }

    /// Saving or closing a document catches the view up with whatever the owner
    /// published since the last workspace change, so the `SharedWorkspaceChanged` queued
    /// for that publication later finds the view current. The other open documents must
    /// be rescheduled by the event that applied it: the input is an owner publication
    /// from a closed file that lands before the event on an unrelated open document,
    /// with no diagnostics in flight.
    ///
    /// `the_event_catches_up_in_place` asks for the event to apply the publication to the
    /// view it has. A close cannot always: the owner also delivers the same write to
    /// itself, and when that lands during the close the close reseeds the view instead.
    /// Without the flag the view only has to reach the publication by either route.
    fn assert_owner_publication_reschedules_open_documents(
        the_event_catches_up_in_place: bool,
        apply: impl FnOnce(&mut GlobalState, lsp_types::Url),
    ) {
        let root = tempfile::tempdir().expect("create shared workspace");
        std::fs::write(root.path().join("bsl-analyzer.toml"), "[source]\nroot = \".\"\n")
            .expect("write project config");
        let closed = root.path().join("Closed.bsl");
        std::fs::write(&closed, "Процедура Было() Экспорт КонецПроцедуры\n").expect("write source");
        let watched = root.path().join("Watched.bsl");
        let touched = root.path().join("Touched.bsl");
        let watched_text = "Процедура Смотрим() КонецПроцедуры\n";
        let touched_text = "Процедура Трогаем() КонецПроцедуры\n";
        std::fs::write(&watched, watched_text).expect("write source");
        std::fs::write(&touched, touched_text).expect("write source");

        let project = mcp_server::project::at(root.path()).expect("load test workspace project");
        let cache_parent = tempfile::tempdir().expect("create disposable workspace cache");
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(cache_parent.path()),
            root.path(),
            None,
        )
        .expect("create explicit disposable cache");
        let backend = mcp_server::SharedState::shared_workspace_with_cache_and_prefixes(
            root.path().to_path_buf(),
            cache,
            None,
        )
        .expect("create disk-only owner for test workspace");
        let owner = backend.workspace_handle();
        let deadline = std::time::Instant::now()
            + test_utils::wait::ceiling(std::time::Duration::from_secs(20));
        while !matches!(owner.readiness(), mcp_server::WorkspaceReadiness::Ready { .. }) {
            assert!(std::time::Instant::now() < deadline, "shared owner did not become ready");
            std::thread::yield_now();
        }

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new_shared(sender, owner.clone());
        apply_shared_project(&mut state, root.path().to_path_buf(), project);
        let open = |state: &mut GlobalState, path: &std::path::Path, text: &str| {
            let uri = lsp_types::Url::from_file_path(path).unwrap();
            crate::handlers::notification::handle_did_open(
                state,
                lsp_types::DidOpenTextDocumentParams {
                    text_document: lsp_types::TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "bsl".to_owned(),
                        version: 1,
                        text: text.to_owned(),
                    },
                },
            )
            .expect("open the document");
            uri
        };
        let watched_uri = open(&mut state, &watched, watched_text);
        let touched_uri = open(&mut state, &touched, touched_text);
        while !state.shared_view_seeded {
            let task = state
                .task_pool
                .receiver
                .recv_timeout(test_utils::wait::ceiling(std::time::Duration::from_secs(20)))
                .expect("the editor view is seeded");
            handle_task(&mut state, task).unwrap();
        }
        assert!(state.vfs_done, "a seeded view over a ready owner finishes loading");
        // The diagnostics scheduled so far have been published.
        state.diagnostics_tokens.clear();
        state.pending_diagnostics_uris.clear();

        let seeded = state.analysis_host.view().expect("open editor view").generation();
        std::fs::write(&closed, "Процедура Стало() Экспорт КонецПроцедуры\n")
            .expect("rewrite the closed source");
        // The owner's watcher can publish the same write first: the refresh then reports
        // that the owner changed under it, and the next one finds the new text published.
        let deadline = std::time::Instant::now()
            + test_utils::wait::hang_guard(std::time::Duration::from_secs(20));
        let published = loop {
            assert!(
                std::time::Instant::now() < deadline,
                "the owner never published the closed file's new text"
            );
            match owner.refresh_saved_path(&closed) {
                mcp_server::WorkspaceRead::Ready(published) => break published,
                mcp_server::WorkspaceRead::Busy => std::thread::yield_now(),
                mcp_server::WorkspaceRead::Failed(reason)
                    if reason == "saved_path_owner_changed" => {}
                _ => panic!("the owner publishes the closed file's new text"),
            }
        };
        assert!(published.generation > seeded, "the owner is ahead of the view");

        apply(&mut state, touched_uri);
        if !the_event_catches_up_in_place {
            while !state.shared_view_seeded {
                let task = state
                    .task_pool
                    .receiver
                    .recv_timeout(test_utils::wait::hang_guard(std::time::Duration::from_secs(20)))
                    .expect("the reseeded editor view");
                handle_task(&mut state, task).unwrap();
            }
        }
        assert_eq!(
            state.analysis_host.view().expect("open editor view").generation(),
            published.generation,
            "the event applies the owner's publication"
        );
        handle_task(
            &mut state,
            crate::global_state::Task::SharedWorkspaceChanged { generation: published.generation },
        )
        .unwrap();

        assert!(
            state.diagnostics_tokens.contains_key(&watched_uri)
                || state.pending_diagnostics_uris.contains(&watched_uri),
            "the open document is rescheduled against the owner's new text"
        );
        drop(state);
        drop(owner);
        drop(backend);
    }

    #[test]
    fn a_save_that_applies_owner_changes_reschedules_open_documents() {
        assert_owner_publication_reschedules_open_documents(true, |state, uri| {
            crate::handlers::handle_did_save(
                state,
                lsp_types::DidSaveTextDocumentParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri },
                    text: None,
                },
            )
            .expect("save the open document");
        });
    }

    fn close_document(state: &mut GlobalState, uri: lsp_types::Url) {
        crate::handlers::notification::handle_did_close(
            state,
            lsp_types::DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
            },
        )
        .expect("close the open document");
    }

    #[test]
    fn a_close_that_applies_owner_changes_reschedules_open_documents() {
        assert_owner_publication_reschedules_open_documents(false, close_document);
    }

    #[test]
    #[ignore = "необязательный тест: быстрый путь закрытия зависит от того, успеет ли владелец разобрать собственную доставку той же записи до закрытия, событийной замены нет; запуск: cargo test -p bsl-analyzer --lib -- --ignored --exact server::tests::a_close_that_applies_owner_changes_catches_the_view_up_in_place"]
    fn a_close_that_applies_owner_changes_catches_the_view_up_in_place() {
        assert_owner_publication_reschedules_open_documents(true, close_document);
    }

    /// The vendor-diff scope is computed against the working tree, so every disk change a
    /// shared session learns of must queue a rebuild, as the standalone save and external
    /// change paths do. The input is a shared session whose first scope build has already
    /// been applied, so only the event under test can queue the next one.
    fn assert_shared_disk_change_rebuilds_scope(
        apply: impl FnOnce(&mut GlobalState, &std::path::Path),
    ) {
        let root = tempfile::tempdir().expect("create shared workspace");
        std::fs::write(
            root.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\n\n[analysis]\ndiff_base = \"HEAD\"\n",
        )
        .expect("write project config");
        let module = root.path().join("Module.bsl");
        std::fs::write(&module, "Процедура Было() КонецПроцедуры\n").expect("write source");

        let project = mcp_server::project::at(root.path()).expect("load test workspace project");
        let cache_parent = tempfile::tempdir().expect("create disposable workspace cache");
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(cache_parent.path()),
            root.path(),
            None,
        )
        .expect("create explicit disposable cache");
        let backend = mcp_server::SharedState::shared_workspace_with_cache_and_prefixes(
            root.path().to_path_buf(),
            cache,
            None,
        )
        .expect("create disk-only owner for test workspace");
        let owner = backend.workspace_handle();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new_shared(sender, owner.clone());
        apply_shared_project(&mut state, root.path().to_path_buf(), project);
        assert!(state.analysis_scope.is_loading(), "a configured diff base builds the scope");
        while state.analysis_scope.is_loading() {
            let task = state
                .task_pool
                .receiver
                .recv_timeout(test_utils::wait::ceiling(std::time::Duration::from_secs(20)))
                .expect("the first scope build finishes");
            if matches!(task, crate::global_state::Task::AnalysisScopeReady { .. }) {
                handle_task(&mut state, task).unwrap();
            }
        }
        assert!(!state.scope_build_queued, "no rebuild is pending before the disk change");

        std::fs::write(&module, "Процедура Стало() КонецПроцедуры\n").expect("rewrite source");
        apply(&mut state, &module);

        assert!(
            state.scope_build_queued || state.analysis_scope.is_loading(),
            "the disk change rebuilds the vendor-diff scope"
        );
        drop(state);
        drop(owner);
        drop(backend);
    }

    #[test]
    fn a_shared_save_rebuilds_the_vendor_diff_scope() {
        assert_shared_disk_change_rebuilds_scope(|state, path| {
            crate::handlers::handle_did_save(
                state,
                lsp_types::DidSaveTextDocumentParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: lsp_types::Url::from_file_path(path).unwrap(),
                    },
                    text: None,
                },
            )
            .expect("save the document");
        });
    }

    #[test]
    fn a_shared_workspace_change_rebuilds_the_vendor_diff_scope() {
        assert_shared_disk_change_rebuilds_scope(|state, path| {
            let owner = state.analysis_host.owner().cloned().expect("shared session owner");
            owner.notify_changed_path(path);
            handle_task(state, crate::global_state::Task::SharedWorkspaceChanged { generation: 0 })
                .unwrap();
        });
    }

    /// Spellings of one file through symlinked directories share one canonical file in a
    /// shared session. Closing one must leave the file open for the others and analysed from
    /// one of their buffers, not from disk — the same one on every run, which the input
    /// tells apart by giving each of the two remaining spellings a different buffer.
    #[cfg(unix)]
    #[test]
    fn closing_one_spelling_of_a_shared_file_keeps_it_open_for_the_other() {
        let root = tempfile::tempdir().expect("create shared workspace");
        std::fs::write(root.path().join("bsl-analyzer.toml"), "[source]\nroot = \"src\"\n")
            .expect("write project config");
        let source = root.path().join("src");
        std::fs::create_dir(&source).expect("create source root");
        let module = source.join("Module.bsl");
        std::fs::write(&module, "Процедура Диск() КонецПроцедуры\n").expect("write source");
        for alias in ["alias", "zalias"] {
            std::os::unix::fs::symlink(&source, root.path().join(alias))
                .expect("link the source directory");
        }

        let project = mcp_server::project::at(root.path()).expect("load test workspace project");
        let cache_parent = tempfile::tempdir().expect("create disposable workspace cache");
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(cache_parent.path()),
            root.path(),
            None,
        )
        .expect("create explicit disposable cache");
        let backend = mcp_server::SharedState::shared_workspace_with_cache_and_prefixes(
            root.path().to_path_buf(),
            cache,
            None,
        )
        .expect("create disk-only owner for test workspace");
        let owner = backend.workspace_handle();

        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new_shared(sender, owner.clone());
        state.vfs_done = true;
        apply_shared_project(&mut state, root.path().to_path_buf(), project);
        let open = |state: &mut GlobalState, path: &std::path::Path, text: &str| {
            let uri = lsp_types::Url::from_file_path(path).unwrap();
            crate::handlers::notification::handle_did_open(
                state,
                lsp_types::DidOpenTextDocumentParams {
                    text_document: lsp_types::TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "bsl".to_owned(),
                        version: 1,
                        text: text.to_owned(),
                    },
                },
            )
            .expect("open the document");
            uri
        };
        let real = open(&mut state, &module, "Процедура Исходник() КонецПроцедуры\n");
        let alias_buffer = "Процедура Псевдоним() КонецПроцедуры\n";
        let alias = open(&mut state, &root.path().join("alias/Module.bsl"), alias_buffer);
        let closed = open(
            &mut state,
            &root.path().join("zalias/Module.bsl"),
            "Процедура Последний() КонецПроцедуры\n",
        );
        let file_id = crate::lsp::file_id(&state, &real).expect("resolve the real spelling");
        for spelling in [&alias, &closed] {
            assert_eq!(
                crate::lsp::file_id(&state, spelling).ok(),
                Some(file_id),
                "every spelling resolves to one shared file"
            );
        }

        crate::handlers::notification::handle_did_close(
            &mut state,
            lsp_types::DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: closed },
            },
        )
        .expect("close one spelling");

        assert!(state.open_files.contains(&file_id), "the other spellings keep the file open");
        let analysis = state.analysis_host.view().expect("open editor view").analysis();
        assert_eq!(
            &*analysis.file_text(file_id),
            alias_buffer,
            "the file is analysed from the remaining buffer with the least URI"
        );
        drop(analysis);
        drop(state);
        drop(owner);
        drop(backend);
    }

    /// A shared session without an editor view answers from its owner's analysis. While the
    /// owner has none published, a request must be declined as not ready, not answered from
    /// the empty fallback store as if the workspace were empty. The input is a session that
    /// already counts as loaded, so only the owner's state can decline it.
    #[test]
    fn a_shared_request_without_owner_analysis_is_declined() {
        let backend = mcp_server::SharedState::shared();
        let owner = backend.workspace_handle();
        assert!(!matches!(owner.request_analysis(), mcp_server::WorkspaceRead::Ready(_)));
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new_shared(sender, owner);
        state.vfs_done = true;

        handle_request(
            &mut state,
            Request::new(
                1.into(),
                lsp_types::request::WorkspaceSymbolRequest::METHOD.to_string(),
                serde_json::json!({ "query": "" }),
            ),
        )
        .unwrap();
        let response = std::iter::from_fn(|| receiver.try_recv().ok())
            .find_map(|message| match message {
                Message::Response(response) if response.id == 1.into() => Some(response),
                _ => None,
            })
            .expect("the request is declined at dispatch, not handed to the pool");
        assert_eq!(
            response.error.map(|error| error.code),
            Some(lsp_server::ErrorCode::ContentModified as i32),
            "the request is declined, not answered from an empty store: {:?}",
            response.result
        );
    }

    /// A syntax request is answered inline while loading from the store holding its file.
    /// A shared session without an editor view has no such store — its fallback is empty —
    /// so the request is declined as not ready instead of failing on a missing file.
    #[test]
    fn a_view_less_shared_syntax_request_is_declined_while_not_ready() {
        let backend = mcp_server::SharedState::shared();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new_shared(sender, backend.workspace_handle());
        state.vfs_done = true;
        let root = tempfile::tempdir().expect("create workspace");
        let uri = lsp_types::Url::from_file_path(root.path().join("Module.bsl")).unwrap();

        handle_request(
            &mut state,
            Request::new(
                1.into(),
                lsp_types::request::DocumentSymbolRequest::METHOD.to_string(),
                serde_json::json!({ "textDocument": { "uri": uri } }),
            ),
        )
        .unwrap();

        let response = std::iter::from_fn(|| receiver.try_recv().ok())
            .find_map(|message| match message {
                Message::Response(response) if response.id == 1.into() => Some(response),
                _ => None,
            })
            .expect("the request is answered at dispatch");
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(lsp_server::ErrorCode::ContentModified as i32),
            "declined as not ready: {:?}",
            response.error
        );
    }

    /// A reloaded diagnostics baseline answers everything again. A shared request without an
    /// editor view runs on the owner's store, out of reach of the session's cancellation, so
    /// the reload must leave its stamp stale for the finished answer to be retried. The input
    /// is a view-less shared session, where the cancellation reaches only the fallback store.
    #[test]
    fn a_reloaded_baseline_retries_view_less_shared_requests() {
        let backend = mcp_server::SharedState::shared();
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new_shared(sender, backend.workspace_handle());
        assert!(state.analysis_host.view().is_none());
        let stamped = state.shared_request_generation;

        answer_from_the_reloaded_baseline(&mut state);

        assert_ne!(
            state.shared_request_generation, stamped,
            "a request stamped before the reload is retried when it finishes"
        );
    }
}
