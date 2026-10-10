use std::{sync::Arc, time::Instant};

use anyhow::Result;
use base_db::FileIdInput;
use ide::file_diagnostics_query;
use line_index::LineIndex;
use lsp_server::Notification;
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, PublishDiagnosticsParams, Url,
};
use salsa::Database as _;

use crate::global_state::{GlobalState, Task};

fn is_handled_uri(uri: &Url) -> bool {
    uri.to_file_path().map(|p| project_model::is_bsl_source_path(&p)).unwrap_or(false)
}

/// Whether `uri` names a document the project's `[source].exclude` took out. Such a
/// document stays in the editor's buffer set but is never analysed.
fn is_excluded_document(state: &GlobalState, uri: &Url) -> bool {
    uri.to_file_path().is_ok_and(|path| state.is_source_excluded_resolved(&path))
}

/// Cancel whatever diagnostics work is pending for `uri` and clear what was published
/// for it, from either stream. The generation moves on too: a result already computed
/// and queued must not be published after the withdrawal, while the buffer stays open.
pub(crate) fn withdraw_document_diagnostics(state: &mut GlobalState, uri: &Url) {
    if let Some(token) = state.diagnostics_tokens.remove(uri) {
        token.cancel();
    }
    *state.diagnostics_generation.entry(uri.clone()).or_insert(0) += 1;
    state.clear_batch_push_for(uri);
    let params = PublishDiagnosticsParams { uri: uri.clone(), diagnostics: vec![], version: None };
    let notification = Notification::new("textDocument/publishDiagnostics".to_string(), params);
    if let Err(error) = state.sender.send(notification.into()) {
        tracing::warn!(%uri, %error, "could not withdraw diagnostics of an excluded document");
    }
}

pub fn schedule_diagnostics(state: &mut GlobalState, uri: &Url) {
    // A pull-capable client with the feature enabled drives diagnostics via
    // `textDocument/diagnostic`; publishing here too would double-report open buffers.
    if state.pull_diagnostics_active {
        tracing::debug!(%uri, "push diagnostics suppressed: client pulls diagnostics");
        return;
    }

    // While the workspace is still loading, the source root and metadata
    // substrate are incomplete: a result computed now is either cancelled by
    // the next streamed VFS batch (each drain bumps the Salsa revision) or
    // published against a half-loaded world as false positives. The vfs_done
    // finalize reschedules every open document, so deferring loses nothing.
    if !state.vfs_done {
        tracing::debug!(%uri, "diagnostics deferred until workspace load completes");
        return;
    }

    if is_excluded_document(state, uri) {
        tracing::debug!(%uri, "diagnostics skipped: document is excluded by [source].exclude");
        return;
    }

    // A saturated task pool must not park the event loop in the bounded job
    // queue (nor pin a db clone into a queued closure). Requeue instead — the
    // loop bottom retries once a finishing worker frees a slot. Checked before
    // cancelling the previous token, so an in-flight result survives the defer.
    if !state.task_pool.pool.has_capacity() {
        tracing::debug!(%uri, "task pool saturated; diagnostics requeued");
        state.enqueue_pending_diagnostics(uri.clone());
        return;
    }

    if let Some(prev) = state.diagnostics_tokens.remove(uri) {
        prev.cancel();
    }

    let file_id = match crate::lsp::file_id(state, uri) {
        Ok(id) => id,
        Err(_) => return,
    };
    let text = match state.mem_docs.get(uri) {
        Some(t) => t,
        None => return,
    };
    let path = uri.to_file_path().ok();
    let workspace_root = state.workspace_root.clone();
    let diagnostics_baseline = Arc::clone(&state.diagnostics_baseline);

    // Advance the generation only once the schedule will actually spawn, so a
    // no-op call (unresolved file / not an open buffer) cannot orphan a prior
    // in-flight result by leaving `current` ahead of every spawned task.
    let generation = {
        let g = state.diagnostics_generation.entry(uri.clone()).or_insert(0);
        *g += 1;
        *g
    };

    let db = state.analysis_host.raw_database().clone();
    state.diagnostics_tokens.insert(uri.clone(), db.cancellation_token());
    let mut config = state.diagnostics_config().clone();
    // An edited-but-unsaved buffer no longer matches the disk state the
    // vendor-diff scope was computed against: analyze it whole-file until the
    // save rebuilds the scope.
    if config.scope.is_some() && state.scope_dirty_docs.contains(uri) {
        config.scope = None;
    }
    let baseline_applies = crate::diagnostics_baseline::applies_under_scope(config.scope.is_some());
    let position_encoding = state.position_encoding;
    let code_descriptions = crate::lsp::to_proto::CodeDescriptions::from_client_support(
        state.supports_code_description,
    );
    let client_tags = state.supports_diagnostic_tags;
    let uri = uri.clone();
    let queued_at = Instant::now();
    tracing::info!(%uri, generation, vfs_done = state.vfs_done, "diagnostics scheduled");

    let retry_uri = uri.clone();
    let analysis_guard = state.note_analysis_spawned();
    let spawned = state.task_pool.pool.try_spawn(move || {
        let _analysis_guard = analysis_guard;
        let started_at = Instant::now();
        let queue_wait_ms = started_at.duration_since(queued_at).as_millis() as u64;
        tracing::info!(%uri, generation, queue_wait_ms, "diagnostics worker started");
        let result = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            let file_id_input = FileIdInput::new(&db, file_id);
            let config_id = base_db::DiagnosticsConfigId::new(&db, config);
            let ide_diagnostics = file_diagnostics_query(&db, file_id_input, config_id);
            let ide_diagnostics =
                match (workspace_root.as_deref().filter(|_| baseline_applies), path.as_deref()) {
                    (Some(root), Some(path)) => crate::diagnostics_baseline::active_for_file(
                        &diagnostics_baseline,
                        root,
                        path,
                        &text,
                        ide_diagnostics.iter().cloned().collect(),
                    ),
                    _ => ide_diagnostics.iter().cloned().collect(),
                };
            let line_index = LineIndex::new(&text);
            crate::lsp::diagnostics_with_encoding(
                &line_index,
                &text,
                &ide_diagnostics,
                position_encoding,
                code_descriptions,
                client_tags,
            )
        }));
        let compute_ms = started_at.elapsed().as_millis() as u64;

        match result {
            Ok(diagnostics) => {
                tracing::info!(
                    %uri,
                    queue_wait_ms,
                    compute_ms,
                    diagnostic_count = diagnostics.len(),
                    "diagnostics ready",
                );
                Task::DiagnosticsReady {
                    uri,
                    diagnostics,
                    generation,
                    completed_at: Instant::now(),
                }
            }
            Err(_) => {
                tracing::debug!(%uri, queue_wait_ms, compute_ms, "diagnostics cancelled");
                Task::DiagnosticsCancelled { generation, completed_at: Instant::now() }
            }
        }
    });
    if spawned.is_err() {
        // Unreachable after the capacity check above; kept so a queue hiccup
        // degrades to a deferred publish instead of a lost one. The dropped
        // job's guard already posted its AnalysisJobFinished.
        tracing::warn!(uri = %retry_uri, "task pool rejected diagnostics job; requeued");
        state.diagnostics_tokens.remove(&retry_uri);
        state.enqueue_pending_diagnostics(retry_uri);
    }
}

pub fn handle_did_open(state: &mut GlobalState, params: DidOpenTextDocumentParams) -> Result<()> {
    let _p = tracing::info_span!("handle_did_open", uri = %params.text_document.uri).entered();
    if !is_handled_uri(&params.text_document.uri) {
        tracing::debug!(uri = %params.text_document.uri, "didOpen: ignoring non-BSL file");
        return Ok(());
    }
    let start = Instant::now();
    let vfs_done = state.vfs_done;

    let uri = params.text_document.uri;
    let text = params.text_document.text;
    let version = params.text_document.version;

    tracing::debug!("Document opened: {} (version {})", uri, version);

    state.mem_docs.insert(uri.clone(), text.clone(), version);

    // The buffer is kept, so that lifting the exclusion later brings back the
    // editor's text; the file itself gets no FileId, no text input and no analysis.
    if is_excluded_document(state, &uri) {
        tracing::info!(%uri, "didOpen: document is excluded by [source].exclude; not analysed");
        return Ok(());
    }

    let file_id = state.vfs_file_for_url(&uri)?;
    // Mark open BEFORE process_changes so the edit is stored as a resident
    // overlay (authoritative for unsaved content), not disk-backed.
    state.open_files.insert(file_id);

    {
        let vfs_path = vfs::VfsPath::new(
            uri.to_file_path().map_err(|()| anyhow::anyhow!("Not a file URI: {}", uri))?,
        );
        let mut vfs = state.vfs.write();
        vfs.set_file_contents(vfs_path, Some(Arc::from(text.as_str())));
    }

    let process_start = Instant::now();
    state.process_changes(!vfs_done);
    let process_changes_ms = process_start.elapsed().as_millis() as u64;

    // An open buffer must be the resident overlay. The VFS dedups by content
    // hash, so a reopen whose buffer matches the last known content produces no
    // change event and `process_changes` never runs — yet didClose re-keyed the
    // file to its disk revision, and disk bytes differ from the editor buffer
    // when the file carries a BOM (the editor strips it). Left disk-backed, every
    // text offset is computed over the BOM'd disk text while positions are mapped
    // through the BOM-less editor text, shifting all ranges. Re-pin explicitly.
    {
        use base_db::SourceDatabase as _;
        if state.analysis_host.raw_database().try_file_text_input(file_id).is_none() {
            state.analysis_host.request_cancellation();
            let db = state.analysis_host.raw_database_mut();
            ide_host_core::set_file_text_source(
                db,
                file_id,
                ide_host_core::FileTextSource::Overlay(&text),
            );
        }
    }

    // Dependency discovery walks the (still cold and incomplete) database
    // synchronously on the event-loop thread, so during the initial load it
    // both stalls VFS batch draining and warms caches the next revision bump
    // throws away. The vfs_done finalize preloads open documents instead.
    let preload_start = Instant::now();
    if vfs_done {
        preload_dependencies(state, file_id);
    }
    let preload_dispatch_ms = preload_start.elapsed().as_millis() as u64;

    // Hand the file off from the deferred batch (Stream B) to the interactive stream:
    // if the batch had pushed diagnostics for it, clear them so the open document is
    // the sole owner and the two streams never double-report it.
    state.clear_batch_push_for(&uri);

    schedule_diagnostics(state, &uri);

    tracing::info!(
        %uri,
        vfs_done,
        process_changes_ms,
        preload_dispatch_ms,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "handle_did_open complete",
    );

    Ok(())
}

pub(crate) fn preload_dependencies(state: &mut GlobalState, file_id: vfs::FileId) {
    // Cache warming is an optimisation: when the task pool is saturated, skip
    // it (the analysis that needed the caches warms them itself) rather than
    // park the event loop in the bounded job queue.
    if !state.task_pool.pool.has_capacity() {
        tracing::debug!(file_id = file_id.0, "task pool saturated; preload skipped");
        return;
    }

    let discover_start = Instant::now();
    let analysis = state.analysis_host.analysis();
    // A dependency the exclusions now cover keeps its id, tombstoned; it is not warmed.
    let mut resolved = stdx::path_exclusion::ResolvedDirs::default();
    let deps: Vec<_> = analysis
        .file_dependencies(file_id)
        .iter()
        .copied()
        .filter(|&dep| !state.is_file_id_excluded(dep, &mut resolved))
        .collect();
    let discover_ms = discover_start.elapsed().as_millis() as u64;

    if deps.is_empty() {
        tracing::debug!(file_id = file_id.0, discover_ms, "preload: no dependencies found",);
        return;
    }

    let dep_count = deps.len();
    let dep_ids: Vec<u32> = deps.iter().map(|f| f.0).collect();
    tracing::info!(
        file_id = file_id.0,
        dep_count,
        discover_ms,
        ?dep_ids,
        "preload: dispatching warm-cache task",
    );

    if let Some(prev) = state.preload_tokens.remove(&file_id) {
        prev.cancel();
    }

    let task = analysis.warm_caches_task(&deps);
    state.preload_tokens.insert(file_id, task.cancellation_token());
    let queued_at = Instant::now();

    let analysis_guard = state.note_analysis_spawned();
    let spawned = state.task_pool.pool.try_spawn(move || {
        let _analysis_guard = analysis_guard;
        let started_at = Instant::now();
        let queue_wait_ms = started_at.duration_since(queued_at).as_millis() as u64;
        let count = match salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| task.run())) {
            Ok(count) => {
                tracing::info!(
                    file_id = file_id.0,
                    dep_count = count,
                    queue_wait_ms,
                    run_ms = started_at.elapsed().as_millis() as u64,
                    "preload: warm-cache complete",
                );
                count
            }
            Err(_) => {
                tracing::debug!(
                    file_id = file_id.0,
                    queue_wait_ms,
                    run_ms = started_at.elapsed().as_millis() as u64,
                    "preload: warm-cache cancelled",
                );
                0
            }
        };
        Task::DependenciesPreloaded { file_id, count }
    });
    if spawned.is_err() {
        // Unreachable after the capacity check above; a skipped warm-up costs
        // only latency, the demanding analysis computes the caches itself.
        tracing::debug!(file_id = file_id.0, "task pool rejected preload job; skipped");
        state.preload_tokens.remove(&file_id);
    }
}

pub fn handle_did_change(
    state: &mut GlobalState,
    params: DidChangeTextDocumentParams,
) -> Result<()> {
    let _p = tracing::info_span!("handle_did_change", uri = %params.text_document.uri).entered();
    if !is_handled_uri(&params.text_document.uri) {
        tracing::debug!(uri = %params.text_document.uri, "didChange: ignoring non-BSL file");
        return Ok(());
    }

    let uri = params.text_document.uri;
    let version = params.text_document.version;

    tracing::debug!(
        "Document changed: {} (version {}, {} changes)",
        uri,
        version,
        params.content_changes.len()
    );

    if let Err(err) =
        state.mem_docs.update_with_encoding(&uri, params.content_changes, state.position_encoding)
    {
        tracing::error!(%uri, error = %err, encoding = ?state.position_encoding, "didChange edit rejected");
        return Ok(());
    }

    state.scope_dirty_docs.insert(uri.clone());

    if is_excluded_document(state, &uri) {
        return Ok(());
    }

    let text = state
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;

    {
        let vfs_path = vfs::VfsPath::new(
            uri.to_file_path().map_err(|()| anyhow::anyhow!("Not a file URI: {}", uri))?,
        );
        let mut vfs = state.vfs.write();
        vfs.set_file_contents(vfs_path, Some(Arc::from(text.as_str())));
    }

    state.process_changes(!state.vfs_done);

    tracing::debug!("Document updated successfully: {}", uri);

    state.enqueue_pending_diagnostics(uri);

    Ok(())
}

pub fn handle_did_close(state: &mut GlobalState, params: DidCloseTextDocumentParams) -> Result<()> {
    let _p = tracing::info_span!("handle_did_close", uri = %params.text_document.uri).entered();

    let uri = params.text_document.uri;

    tracing::debug!("Document closed: {}", uri);

    if is_excluded_document(state, &uri) {
        state.scope_dirty_docs.remove(&uri);
        state.mem_docs.remove(&uri);
        withdraw_document_diagnostics(state, &uri);
        return Ok(());
    }

    if let Some(token) = state.diagnostics_tokens.remove(&uri) {
        token.cancel();
    }
    // The buffer (and its unsaved edits) is gone: the disk-derived scope
    // describes the file again.
    state.scope_dirty_docs.remove(&uri);
    match crate::lsp::file_id(state, &uri) {
        Ok(file_id) => {
            if let Some(token) = state.preload_tokens.remove(&file_id) {
                token.cancel();
            }
            state.open_files.remove(&file_id);
            // The file is no longer open: drop its editor-buffer overlay and
            // re-key on the on-disk content so its text becomes LRU-evictable
            // (discarding any unsaved edits, which the client also discards).
            // If it isn't readable from disk (e.g. never saved), keep the
            // overlay as a safe fallback rather than leave a dangling revision.
            let vfs_path = state.vfs.read().file_path(file_id).clone();
            let disk = std::fs::read_to_string(vfs_path.as_path()).ok();
            if let Some(content) = disk {
                // Route the disk text through the VFS so its content hash
                // reflects what is actually on disk. Leaving the last editor
                // buffer's hash behind would dedup a later disk-change event
                // whose bytes happen to equal that buffer (e.g. an upstream
                // BOM strip), pointing the recorded revision at content the
                // disk no longer has.
                let vfs_changed = state
                    .vfs
                    .write()
                    .set_file_contents(vfs_path, Some(Arc::from(content.as_str())));
                if vfs_changed {
                    let vfs_done = state.vfs_done;
                    // Only this close's own write can be pending here (every VFS
                    // mutation site drains synchronously on the main loop), and
                    // its outcome is deliberately dropped: this file's change
                    // always reports affecting open documents, but acting on
                    // that would re-analyze every open document on each close
                    // of a BOM-carrying file for a semantic no-op.
                    state.process_changes(!vfs_done);
                } else {
                    // The VFS already holds the disk text (buffer was saved),
                    // so no change event will flip the source; drop the
                    // overlay directly to make the text disk-backed.
                    state.analysis_host.request_cancellation();
                    let db = state.analysis_host.raw_database_mut();
                    ide_host_core::set_file_text_source(
                        db,
                        file_id,
                        ide_host_core::FileTextSource::Disk(&content),
                    );
                }
            }
        }
        Err(e) => {
            tracing::warn!(%uri, error = %e, "didClose: could not resolve file_id for preload cleanup")
        }
    }

    state.mem_docs.remove(&uri);

    let params = PublishDiagnosticsParams { uri: uri.clone(), diagnostics: vec![], version: None };

    let notification = Notification::new("textDocument/publishDiagnostics".to_string(), params);

    state.sender.send(notification.into())?;

    // The file is closed again, so it is eligible for the deferred batch (Stream B):
    // re-arm so the next sweep repopulates its diagnostics from disk-backed text.
    state.mark_workspace_batch_dirty();

    tracing::debug!("Document closed successfully: {}", uri);

    Ok(())
}

pub fn handle_did_save(state: &mut GlobalState, params: DidSaveTextDocumentParams) -> Result<()> {
    let _p = tracing::info_span!("handle_did_save", uri = %params.text_document.uri).entered();

    let uri = params.text_document.uri;

    tracing::debug!("Document saved: {}", uri);

    let mut config_reloaded = false;
    if let Ok(path) = uri.to_file_path() {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if project_model::is_project_input_file_name(name) && state.reload_project_config() {
                config_reloaded = true;
                for uri in state.opened_document_uris() {
                    schedule_diagnostics(state, &uri);
                }
            }
        }
    }

    if config_reloaded {
        // The workspace-diagnostics scope may have changed (or been turned off): drop
        // every file the batch pushed before re-running under the new configuration.
        state.reset_workspace_batch();
    }

    // A save writes new bytes to disk, which can change diagnostics in closed files
    // (directly, or via inter-file dependencies); re-arm the batch. Salsa memoization
    // keeps the re-run cheap — only the changed file and its dependents recompute.
    state.mark_workspace_batch_dirty();

    // The saved bytes moved the disk state the vendor-diff scope was computed
    // against: rebuild it (coalesced), and put the no-longer-dirty document
    // back under the disk-derived filter right away.
    if state.scope_dirty_docs.remove(&uri) && state.diagnostics_config().scope.is_some() {
        schedule_diagnostics(state, &uri);
    }
    state.request_scope_rebuild();
    state.maybe_spawn_scope_build();

    Ok(())
}

pub fn handle_cancel(state: &mut GlobalState, params: lsp_types::CancelParams) -> Result<()> {
    let id = match params.id {
        lsp_types::NumberOrString::Number(n) => lsp_server::RequestId::from(n),
        lsp_types::NumberOrString::String(s) => lsp_server::RequestId::from(s),
    };

    // Cancel but keep the token registered: the worker still holds its db snapshot
    // until it posts `Task::RequestResult` (which removes the entry), and an early
    // removal would let `interactive_analysis_quiescent` report quiescent while the
    // cancelled worker is still unwinding — an LRU trim taken in that window blocks
    // the event loop on the worker's snapshot.
    if let Some(token) = state.request_tokens.get(&id) {
        tracing::debug!(request_id = ?id, "cancelling in-flight async request");
        token.cancel();
    } else {
        tracing::debug!(request_id = ?id, "cancel arrived after response (no-op)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{unbounded, Receiver};
    use lsp_server::Message;
    use lsp_types::{
        TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
        VersionedTextDocumentIdentifier,
    };

    fn create_test_state() -> (GlobalState, Receiver<Message>) {
        let (sender, receiver) = unbounded();
        let mut state = GlobalState::new(sender);

        use base_db::{SourceDatabase, SourceRoot, SourceRootId};
        let db = state.analysis_host.raw_database_mut();
        let source_root_id = SourceRootId(0);
        let file_set = vfs::FileSet::new();
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(source_root_id, source_root);

        // No VFS loader runs in tests, so the `Finished` event that normally
        // flips this flag never arrives; model a fully loaded workspace.
        state.vfs_done = true;

        (state, receiver)
    }

    #[test]
    fn test_did_open() {
        let (mut state, _receiver) = create_test_state();

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: crate::test_uri::file_uri("test.bsl"),
                language_id: "bsl".to_string(),
                version: 1,
                text: "Процедура Тест() КонецПроцедуры".to_string(),
            },
        };

        let result = handle_did_open(&mut state, params.clone());
        assert!(result.is_ok());

        assert!(state.mem_docs.contains(&params.text_document.uri));
        assert_eq!(state.mem_docs.get(&params.text_document.uri), Some(params.text_document.text));

        assert_eq!(state.diagnostics_generation.get(&params.text_document.uri).copied(), Some(1));
        assert!(state.diagnostics_tokens.contains_key(&params.text_document.uri));
    }

    #[test]
    fn did_open_keeps_an_excluded_buffer_without_creating_a_vfs_file() {
        let (mut state, _receiver) = create_test_state();
        let dir = tempfile::tempdir().unwrap();
        let hidden_dir = dir.path().join(".tmp");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        let hidden = hidden_dir.join("Hidden.bsl");
        std::fs::write(&hidden, "Процедура Скрытая() КонецПроцедуры").unwrap();
        let uri = lsp_types::Url::from_file_path(&hidden).unwrap();
        state.source_exclusions = project_model::ExcludedPaths::new([hidden_dir]);

        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_owned(),
                    version: 1,
                    text: "Процедура Несохраненная() КонецПроцедуры".to_owned(),
                },
            },
        )
        .unwrap();

        assert!(state.mem_docs.contains(&uri), "the editor buffer must survive exclusion");
        assert!(
            state.vfs.read().file_id(&vfs::VfsPath::new(hidden.clone())).is_none(),
            "didOpen admitted an excluded document to the VFS"
        );
        assert!(state.open_files.is_empty());
        assert!(state.diagnostics_tokens.is_empty());
        assert!(state.preload_tokens.is_empty());

        handle_did_change(
            &mut state,
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: 2 },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "Процедура ЕщеОдна() КонецПроцедуры".to_owned(),
                }],
            },
        )
        .unwrap();
        assert_eq!(state.mem_docs.get(&uri).as_deref(), Some("Процедура ЕщеОдна() КонецПроцедуры"));
        assert!(state.vfs.read().file_id(&vfs::VfsPath::new(hidden.clone())).is_none());
        assert!(state.diagnostics_tokens.is_empty());

        handle_did_close(
            &mut state,
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            },
        )
        .unwrap();
        assert!(!state.mem_docs.contains(&uri));
        assert!(!state.scope_dirty_docs.contains(&uri));
        assert!(state.diagnostics_tokens.is_empty());
        assert!(state.vfs.read().file_id(&vfs::VfsPath::new(hidden)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn did_open_cannot_create_an_unsaved_vfs_file_through_an_excluded_symlink_alias() {
        let (mut state, _receiver) = create_test_state();
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let hidden_dir = real.join(".tmp");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let unsaved = alias.join(".tmp/New.bsl");
        assert!(!unsaved.exists(), "the counterexample requires an unsaved new document");
        let uri = lsp_types::Url::from_file_path(&unsaved).unwrap();
        state.source_exclusions = project_model::ExcludedPaths::new([hidden_dir]);

        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_owned(),
                    version: 1,
                    text: "Процедура Новая() КонецПроцедуры".to_owned(),
                },
            },
        )
        .unwrap();

        assert!(state.mem_docs.contains(&uri));
        assert!(
            state.vfs.read().file_id(&vfs::VfsPath::new(unsaved)).is_none(),
            "an excluded unsaved alias entered the VFS"
        );
        assert!(state.open_files.is_empty());
        assert!(state.diagnostics_tokens.is_empty());
    }

    #[test]
    fn push_suppressed_when_client_pulls_diagnostics() {
        let (mut state, _receiver) = create_test_state();
        state.pull_diagnostics_active = true;

        let uri = crate::test_uri::file_uri("test.bsl");
        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: "Процедура Тест() КонецПроцедуры".to_string(),
                },
            },
        )
        .unwrap();

        // The buffer is tracked, but no push diagnostics were scheduled: the client
        // will pull them instead, so publishing here would double-report.
        assert!(state.mem_docs.contains(&uri), "open buffer must still be tracked");
        assert!(
            !state.diagnostics_tokens.contains_key(&uri),
            "no push diagnostics token when the client pulls"
        );
        assert_eq!(
            state.diagnostics_generation.get(&uri).copied(),
            None,
            "suppressed push must not advance the publish generation"
        );
    }

    #[test]
    fn did_open_before_vfs_done_defers_diagnostics_and_preload() {
        let (mut state, _receiver) = create_test_state();
        state.vfs_done = false;

        let uri = crate::test_uri::file_uri("test.bsl");
        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: "Процедура Тест() КонецПроцедуры".to_string(),
                },
            },
        )
        .unwrap();

        // The document is tracked, but no analysis is spawned against the
        // half-loaded workspace: the vfs_done finalize replays it instead.
        assert!(state.mem_docs.contains(&uri));
        assert_eq!(state.diagnostics_generation.get(&uri), None);
        assert!(!state.diagnostics_tokens.contains_key(&uri));
        assert!(state.preload_tokens.is_empty());

        // Once the workspace finishes loading, scheduling works again.
        state.vfs_done = true;
        schedule_diagnostics(&mut state, &uri);
        assert_eq!(state.diagnostics_generation.get(&uri).copied(), Some(1));
        assert!(state.diagnostics_tokens.contains_key(&uri));
    }

    #[test]
    fn did_change_enqueues_pending_diagnostics_deduplicated() {
        let (mut state, _receiver) = create_test_state();

        let uri = crate::test_uri::file_uri("test.bsl");
        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: "Процедура Тест() КонецПроцедуры".to_string(),
                },
            },
        )
        .unwrap();
        state.pending_diagnostics_uris.clear();

        let change = |version| DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: format!("// v{version}"),
            }],
        };
        handle_did_change(&mut state, change(2)).unwrap();
        handle_did_change(&mut state, change(3)).unwrap();

        assert_eq!(
            state.pending_diagnostics_uris,
            vec![uri],
            "consecutive edits of one document queue a single pending entry"
        );
    }

    #[test]
    fn diagnostics_generation_is_tracked_per_uri() {
        let (mut state, _receiver) = create_test_state();

        let a = crate::test_uri::file_uri("a.bsl");
        let b = crate::test_uri::file_uri("b.bsl");
        for (uri, text) in
            [(&a, "Процедура А() КонецПроцедуры"), (&b, "Процедура Б() КонецПроцедуры")]
        {
            handle_did_open(
                &mut state,
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "bsl".to_string(),
                        version: 1,
                        text: text.to_string(),
                    },
                },
            )
            .unwrap();
        }

        // Each document carries its own generation: opening B must NOT advance A's,
        // otherwise A's in-flight diagnostics task would be discarded as stale and
        // never published (the multi-document refresh bug).
        assert_eq!(state.diagnostics_generation.get(&a).copied(), Some(1));
        assert_eq!(state.diagnostics_generation.get(&b).copied(), Some(1));

        // Re-scheduling A advances only A's generation.
        schedule_diagnostics(&mut state, &a);
        assert_eq!(state.diagnostics_generation.get(&a).copied(), Some(2));
        assert_eq!(state.diagnostics_generation.get(&b).copied(), Some(1));
    }

    #[test]
    fn did_close_drops_overlay_and_re_keys_open_file_to_disk() {
        use base_db::SourceDatabase;

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("mod.bsl");
        std::fs::write(&path, "Процедура НаДиске() КонецПроцедуры").expect("write");

        let (mut state, _receiver) = create_test_state();
        let uri = lsp_types::Url::from_file_path(&path).unwrap();

        // Open with an unsaved edit (buffer differs from disk) → resident overlay.
        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: "Процедура Буфер() КонецПроцедуры".to_string(),
                },
            },
        )
        .unwrap();

        let file_id = state.vfs_file_for_url(&uri).unwrap();
        assert!(state.open_files.contains(&file_id), "open file is tracked in open_files");
        {
            let db = state.analysis_host.raw_database_mut();
            assert!(db.try_file_text(file_id).is_some(), "open file keeps a resident overlay");
            assert_eq!(&*db.file_text(file_id), "Процедура Буфер() КонецПроцедуры");
        }

        // Close: the editor buffer is gone, so the file re-keys to its on-disk
        // content (the unsaved edit is discarded) and the overlay is dropped so
        // the text becomes disk-backed / evictable.
        handle_did_close(
            &mut state,
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            },
        )
        .unwrap();

        assert!(!state.open_files.contains(&file_id), "closed file is removed from open_files");
        {
            let db = state.analysis_host.raw_database_mut();
            assert!(
                db.try_file_text(file_id).is_none(),
                "closed file overlay is cleared (disk-backed)"
            );
            assert_eq!(&*db.file_text(file_id), "Процедура НаДиске() КонецПроцедуры");
        }
    }

    #[test]
    fn reopen_after_close_restores_editor_overlay_for_bom_file() {
        use base_db::SourceDatabase;

        // On disk the file carries a UTF-8 BOM; the editor strips it from the
        // buffer, so the didOpen text hashes identically to the previous open's
        // VFS content and the VFS dedups the reopen — no change event reaches
        // process_changes.
        let editor_text = "Процедура Тест() КонецПроцедуры";
        let disk_text = format!("\u{FEFF}{editor_text}");

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("mod.bsl");
        std::fs::write(&path, &disk_text).expect("write");

        let (mut state, _receiver) = create_test_state();
        let uri = lsp_types::Url::from_file_path(&path).unwrap();

        let open = |state: &mut GlobalState| {
            handle_did_open(
                state,
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "bsl".to_string(),
                        version: 1,
                        text: editor_text.to_string(),
                    },
                },
            )
            .unwrap();
        };

        open(&mut state);
        let file_id = state.vfs_file_for_url(&uri).unwrap();
        assert_eq!(&*state.analysis_host.raw_database_mut().file_text(file_id), editor_text);

        // Close re-keys to the disk revision (BOM included).
        handle_did_close(
            &mut state,
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            },
        )
        .unwrap();
        assert_eq!(&*state.analysis_host.raw_database_mut().file_text(file_id), disk_text);

        // Reopen: the buffer must become the overlay again even though the VFS
        // deduped the open. Without the re-pin the file stays disk-backed and
        // every highlight offset is computed over the BOM'd text while positions
        // map through the BOM-less buffer — all ranges shift.
        open(&mut state);
        {
            let db = state.analysis_host.raw_database_mut();
            assert!(
                db.try_file_text(file_id).is_some(),
                "reopened file must hold a resident overlay"
            );
            assert_eq!(&*db.file_text(file_id), editor_text);
        }
    }

    #[test]
    fn disk_change_after_close_is_seen_even_when_bytes_match_last_buffer() {
        use base_db::SourceDatabase;

        // Disk carries a BOM, the editor buffer doesn't. After didClose the VFS
        // must hold the BOM'd disk text — otherwise a later disk change whose
        // bytes equal the last buffer (an upstream BOM strip via git pull)
        // hash-dedups into nothing and the recorded revision points at content
        // the disk no longer has.
        let editor_text = "Процедура Тест() КонецПроцедуры";
        let disk_text = format!("\u{FEFF}{editor_text}");

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("mod.bsl");
        std::fs::write(&path, &disk_text).expect("write");

        let (mut state, _receiver) = create_test_state();
        let uri = lsp_types::Url::from_file_path(&path).unwrap();

        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: editor_text.to_string(),
                },
            },
        )
        .unwrap();
        let file_id = state.vfs_file_for_url(&uri).unwrap();

        handle_did_close(
            &mut state,
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            },
        )
        .unwrap();
        assert_eq!(&*state.analysis_host.raw_database_mut().file_text(file_id), disk_text);

        // git pull rewrites the file to the BOM-less form — byte-identical to
        // the last editor buffer. The watcher delivers the new content.
        std::fs::write(&path, editor_text).expect("rewrite");
        let vfs_path = state.vfs.read().file_path(file_id).clone();
        let changed =
            state.vfs.write().set_file_contents(vfs_path, Some(std::sync::Arc::from(editor_text)));
        assert!(changed, "the watcher event must not dedup against a stale buffer hash");
        state.process_changes(false);

        // The re-keyed revision must match the new disk bytes (a stale revision
        // makes this read panic on the hash check).
        assert_eq!(&*state.analysis_host.raw_database_mut().file_text(file_id), editor_text);
    }

    #[test]
    fn test_did_change() {
        let (mut state, _receiver) = create_test_state();

        let uri = crate::test_uri::file_uri("test.bsl");
        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: "old text".to_string(),
                },
            },
        )
        .unwrap();

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: 2 },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "new text".to_string(),
            }],
        };

        let result = handle_did_change(&mut state, params);
        assert!(result.is_ok());

        assert_eq!(state.mem_docs.get(&uri), Some("new text".to_string()));
    }

    #[test]
    fn test_did_change_uses_negotiated_utf8_encoding() {
        let (mut state, _receiver) = create_test_state();
        state.position_encoding = crate::lsp::PositionEncoding::Utf8;

        let uri = crate::test_uri::file_uri("test.bsl");
        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: "Процедура Тест()\nКонецПроцедуры".to_string(),
                },
            },
        )
        .unwrap();

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: 2 },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position { line: 0, character: 19 },
                    end: lsp_types::Position { line: 0, character: 19 },
                }),
                range_length: Some(0),
                text: "Новая".to_string(),
            }],
        };

        handle_did_change(&mut state, params).unwrap();

        assert_eq!(
            state.mem_docs.get(&uri),
            Some("Процедура НоваяТест()\nКонецПроцедуры".to_string())
        );
    }

    #[test]
    fn handle_cancel_cancels_and_keeps_token() {
        let (mut state, _receiver) = create_test_state();

        let db = state.analysis_host.raw_database().clone();
        let token = db.cancellation_token();
        let id = lsp_server::RequestId::from(123);
        state.request_tokens.insert(id.clone(), token.clone());

        let params = lsp_types::CancelParams { id: lsp_types::NumberOrString::Number(123) };
        handle_cancel(&mut state, params).unwrap();

        assert!(token.is_cancelled(), "token must be cancelled after $/cancelRequest");
        assert!(
            state.request_tokens.contains_key(&id),
            "the entry stays until the worker's RequestResult removes it"
        );
    }

    #[test]
    fn handle_cancel_is_noop_for_unknown_id() {
        let (mut state, _receiver) = create_test_state();

        let params = lsp_types::CancelParams { id: lsp_types::NumberOrString::Number(999) };
        let result = handle_cancel(&mut state, params);
        assert!(result.is_ok());
    }

    #[test]
    fn handle_cancel_supports_string_ids() {
        let (mut state, _receiver) = create_test_state();

        let db = state.analysis_host.raw_database().clone();
        let token = db.cancellation_token();
        let id = lsp_server::RequestId::from("req-abc".to_string());
        state.request_tokens.insert(id.clone(), token.clone());

        let params = lsp_types::CancelParams {
            id: lsp_types::NumberOrString::String("req-abc".to_string()),
        };
        handle_cancel(&mut state, params).unwrap();

        assert!(token.is_cancelled());
        assert!(state.request_tokens.contains_key(&id));
    }

    #[test]
    fn did_open_hands_file_off_from_batch_push() {
        let (mut state, _receiver) = create_test_state();
        let uri = crate::test_uri::file_uri("mod.bsl");

        // The deferred batch had pushed diagnostics for this (then closed) file.
        state.batch_pushed.insert(uri.clone(), "h1".to_string());

        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: "Процедура Тест() КонецПроцедуры".to_string(),
                },
            },
        )
        .unwrap();

        // Opening it hands ownership to the interactive stream: the batch push is
        // cleared so the two streams never double-report the same file.
        assert!(
            !state.batch_pushed.contains_key(&uri),
            "opening a batch-pushed file must clear its batch entry"
        );
    }

    #[test]
    fn test_did_close() {
        let (mut state, _receiver) = create_test_state();

        let uri = crate::test_uri::file_uri("test.bsl");

        handle_did_open(
            &mut state,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_string(),
                    version: 1,
                    text: "test".to_string(),
                },
            },
        )
        .unwrap();

        assert!(state.mem_docs.contains(&uri));

        handle_did_close(
            &mut state,
            DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            },
        )
        .unwrap();

        assert!(!state.mem_docs.contains(&uri));
    }

    #[test]
    fn cancel_keeps_request_token_until_worker_completes() {
        use salsa::Database as _;

        let (mut state, _receiver) = create_test_state();
        let id = lsp_server::RequestId::from(7);
        let token = state.analysis_host.raw_database().cancellation_token();
        state.request_tokens.insert(id.clone(), token);

        handle_cancel(
            &mut state,
            lsp_types::CancelParams { id: lsp_types::NumberOrString::Number(7) },
        )
        .unwrap();

        // The cancelled worker still owns its db snapshot until it posts its
        // `RequestResult`; dropping the token here would let the quiescence gate
        // green-light an LRU trim that blocks the event loop on that snapshot.
        assert!(
            state.request_tokens.contains_key(&id),
            "cancellation must not unregister the in-flight request"
        );
        assert!(
            !state.interactive_analysis_quiescent(),
            "a cancelled-but-running request must still defer trims"
        );
    }
}
