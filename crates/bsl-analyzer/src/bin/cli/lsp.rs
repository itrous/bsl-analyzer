use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use lsp_server::{Connection, Message, RequestId};
use mcp_server::broker::transport::{AdapterKind, LspChannel, LspControl};

use super::mcp::shared::{self, LspContext};

#[derive(Clone, Copy, Default, ValueEnum)]
pub enum LspMode {
    #[default]
    Auto,
    Stdio,
}

#[derive(Default, Args)]
pub struct LspArgs {
    #[arg(long, value_enum, default_value = "auto")]
    mode: LspMode,
}

pub fn run_lsp_server(args: LspArgs) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (connection, io_threads) = Connection::stdio();
    let result = match args.mode {
        LspMode::Stdio => bsl_analyzer::main_loop(connection),
        LspMode::Auto => run_auto(connection),
    };
    match &result {
        Ok(()) => {
            io_threads.join().ok();
        }
        // Termination prints the error to stderr only; the log file is where an editor
        // session's failure is looked for afterwards.
        Err(error) => tracing::error!(error = format!("{error:#}"), "LSP session failed"),
    }
    result.map_err(Into::into)
}

fn run_auto(connection: Connection) -> Result<()> {
    mcp_server::broker::SharedBackendIdentity::prefetch_build_fingerprint();
    let initial = connection.receiver.recv().context("LSP closed before initialize")?;
    let params = match &initial {
        Message::Request(request) if request.method == "initialize" => request.params.clone(),
        _ => anyhow::bail!("expected LSP initialize"),
    };
    let parsed: lsp_types::InitializeParams = serde_json::from_value(params.clone())?;
    let root = bsl_analyzer::server::extract_workspace_root(&parsed);
    let Some(root) = root else {
        return standalone_with_initial(connection, initial);
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let backend = match rt.block_on(connect_backend(&root, &LspContext::default())) {
        Ok(backend) => backend,
        Err(error) => {
            tracing::warn!(%error, "LSP backend unavailable before initialize; using stdio");
            rt.shutdown_timeout(Duration::from_secs(2));
            return standalone_with_initial(connection, initial);
        }
    };
    let result = rt.block_on(relay_lsp(connection, backend, root, initial, params, parsed));
    rt.shutdown_timeout(Duration::from_secs(2));
    result
}

fn standalone_with_initial(connection: Connection, initial: Message) -> Result<()> {
    let (sender, receiver) = crossbeam_channel::bounded(32);
    sender.send(initial)?;
    let incoming = connection.receiver;
    std::thread::Builder::new().name("bsl-lsp-fallback-input".into()).spawn(move || {
        while let Ok(message) = incoming.recv() {
            if sender.send(message).is_err() {
                break;
            }
        }
    })?;
    bsl_analyzer::main_loop(Connection { sender: connection.sender, receiver })
}

struct BackendPipe {
    /// Drained by its own task, so the relay waits for room in it as one more event and
    /// keeps draining the backend meanwhile: a relay that waited inside a write would stop
    /// draining the backend's output, while the backend may wait for that output to drain
    /// before it reads more input.
    outgoing: Option<tokio::sync::mpsc::Sender<Message>>,
    writer: Option<tokio::task::JoinHandle<Result<()>>>,
    messages: tokio::sync::mpsc::Receiver<Message>,
    controls: tokio::sync::mpsc::Receiver<LspControl>,
    reader: tokio::task::JoinHandle<Result<()>>,
    _send_control: tokio::sync::mpsc::Sender<LspControl>,
}

impl BackendPipe {
    fn new(
        io: tokio::io::DuplexStream,
        controls: tokio::sync::mpsc::Receiver<LspControl>,
        send_control: tokio::sync::mpsc::Sender<LspControl>,
    ) -> Self {
        let (read, mut write) = tokio::io::split(io);
        let (outgoing, mut queued) = tokio::sync::mpsc::channel::<Message>(16);
        let writer = tokio::spawn(async move {
            while let Some(message) = queued.recv().await {
                shared::write_lsp_message(&mut write, &message).await?;
            }
            Ok(())
        });
        let (sender, messages) = tokio::sync::mpsc::channel(16);
        let reader = tokio::spawn(async move {
            let mut read = tokio::io::BufReader::new(read);
            while let Some(message) = shared::read_lsp_message(&mut read).await? {
                if sender.send(message).await.is_err() {
                    break;
                }
            }
            Ok(())
        });
        Self {
            outgoing: Some(outgoing),
            writer: Some(writer),
            messages,
            controls,
            reader,
            _send_control: send_control,
        }
    }

    /// Waits for room for one message; `None` once the writer has ended.
    fn room(&self) -> impl std::future::Future<Output = Option<OwnedPermit>> + use<> {
        let outgoing = self.outgoing.clone();
        async move {
            match outgoing {
                Some(outgoing) => outgoing.reserve_owned().await.ok(),
                None => None,
            }
        }
    }

    /// Why the writer ended.
    async fn write_failure(&mut self) -> anyhow::Error {
        self.outgoing = None;
        match self.writer.take() {
            Some(writer) => match writer.await {
                Ok(Err(error)) => error.context("LSP backend write failed"),
                Ok(Ok(())) => anyhow::anyhow!("LSP backend stopped accepting messages"),
                Err(error) => error.into(),
            },
            None => anyhow::anyhow!("LSP backend stopped accepting messages"),
        }
    }

    /// Delivers what was already handed to the writer. The backend's output is drained
    /// meanwhile, for the same reason the relay drains it while it waits for room.
    async fn finish(&mut self) -> Result<()> {
        self.outgoing = None;
        let Some(mut writer) = self.writer.take() else { return Ok(()) };
        let mut draining = true;
        loop {
            tokio::select! {
                result = &mut writer => return result?.context("LSP backend write failed"),
                message = self.messages.recv(), if draining => draining = message.is_some(),
            }
        }
    }

    /// Why the backend's output ended: its read error when there was one.
    async fn disconnected(&mut self) -> anyhow::Error {
        let error = "LSP backend disconnected; session cannot be replayed";
        match (&mut self.reader).await {
            Ok(Err(cause)) => cause.context(error),
            _ => anyhow::anyhow!(error),
        }
    }
}

impl Drop for BackendPipe {
    fn drop(&mut self) {
        self.reader.abort();
        if let Some(writer) = &self.writer {
            writer.abort();
        }
    }
}

type OwnedPermit = tokio::sync::mpsc::OwnedPermit<Message>;

enum RelayEvent {
    Control(Option<LspControl>),
    Writable(Option<OwnedPermit>),
    Backend(Option<Message>),
    Client(Option<Message>),
}

/// `room` is awaited only while a message waits for the backend, and the client is read only
/// while none does: the client is held back by the backend's pace, the backend never by the
/// client's.
async fn next_relay_event(
    controls: &mut tokio::sync::mpsc::Receiver<LspControl>,
    room: Option<impl std::future::Future<Output = Option<OwnedPermit>>>,
    backend: &mut tokio::sync::mpsc::Receiver<Message>,
    client: &mut tokio::sync::mpsc::Receiver<Message>,
    controls_open: bool,
    prefer_client: bool,
) -> RelayEvent {
    let reads_client = room.is_none();
    let room = async move {
        match room {
            Some(room) => room.await,
            None => std::future::pending().await,
        }
    };
    // Retirement must precede EOF. Alternate ready data directions so either
    // continuous producer still allows progress on the other side.
    if prefer_client {
        tokio::select! {
            biased;
            event = controls.recv(), if controls_open => RelayEvent::Control(event),
            permit = room => RelayEvent::Writable(permit),
            event = client.recv(), if reads_client => RelayEvent::Client(event),
            event = backend.recv() => RelayEvent::Backend(event),
        }
    } else {
        tokio::select! {
            biased;
            event = controls.recv(), if controls_open => RelayEvent::Control(event),
            permit = room => RelayEvent::Writable(permit),
            event = backend.recv() => RelayEvent::Backend(event),
            event = client.recv(), if reads_client => RelayEvent::Client(event),
        }
    }
}

async fn connect_backend(root: &Path, context: &LspContext) -> Result<BackendPipe> {
    let root = root.canonicalize()?;
    let project = mcp_server::project::at(&root)?;
    let cache = mcp_server::WorkspaceCacheLayout::for_project_in_current_dir(&project, None, None)?;
    shared::prepare_platform_identity(&root)?;
    let identity = shared::identity(&root, &cache)?;
    let key = mcp_server::broker::SharedBackendKey::new(identity.clone());
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(["mcp", "serve", "--profile", "workspace", "--mode", "daemon", "--source-dir"])
        .arg(&root)
        .arg("--cache-dir")
        .arg(cache.declared_base())
        .env(mcp_server::WORKSPACE_CACHE_SCOPE_ENV, cache.scope_stamp())
        .env(shared::AUTO_DAEMON_ENV, "1")
        .env_remove("BSL_ONEC_PASSWORD")
        .env_remove("BSL_ONEC_CONNECTIONS_FILE")
        .env_remove(mcp_server::WORKSPACE_CACHE_BASE_ENV);
    let mut stream = mcp_server::broker::proxy::connect_or_launch_stream(key, command).await?;
    let bytes = serde_json::to_vec(context)?;
    shared::attach(&mut stream, identity, AdapterKind::Lsp, &bytes).await?;
    let channel = LspChannel::new(stream, || {});
    if context.initialize_params.is_some() {
        channel.send_control(LspControl::ResumeLsp(bytes)).await?;
    }
    let (io, controls, send_control) = channel.into_parts();
    Ok(BackendPipe::new(io, controls, send_control))
}

async fn relay_lsp(
    connection: Connection,
    mut backend: BackendPipe,
    root: PathBuf,
    initial: Message,
    initialize_params: serde_json::Value,
    parsed: lsp_types::InitializeParams,
) -> Result<()> {
    let encoding = bsl_analyzer::lsp::PositionEncoding::negotiate(&parsed.capabilities);
    let mut documents = bsl_analyzer::mem_docs::MemDocs::new();
    let mut pending = HashMap::<RequestId, String>::new();
    if let Message::Request(request) = &initial {
        pending.insert(request.id.clone(), request.method.clone());
    }
    let mut outbox = VecDeque::from([initial]);
    let (sender, mut incoming) = tokio::sync::mpsc::channel(16);
    let receiver = connection.receiver;
    tokio::task::spawn_blocking(move || {
        while let Ok(message) = receiver.recv() {
            if sender.blocking_send(message).is_err() {
                break;
            }
        }
    });
    let mut next_server_request_id = 1i32;
    let mut progress_generation = 0u64;
    let mut initialized = false;
    let mut announced_capabilities = None;
    let mut pending_server = HashSet::<RequestId>::new();
    let mut pending_rebind_refresh = HashSet::<RequestId>::new();
    let mut active_progress = HashMap::<String, serde_json::Value>::new();
    let mut awaiting_rebind_ready = false;
    let rebind_refresh_capabilities =
        RebindRefreshCapabilities::from_initialize(&initialize_params);
    let mut shutdown_requested = false;
    let mut controls_open = true;
    let mut prefer_client = false;
    let result = async {
        loop {
            let room = (!outbox.is_empty()).then(|| backend.room());
            match next_relay_event(
                &mut backend.controls,
                room,
                &mut backend.messages,
                &mut incoming,
                controls_open,
                prefer_client,
            )
            .await
            {
                RelayEvent::Control(control) => match control {
                    Some(LspControl::ScopeRetired { .. }) => {
                        cancel_pending(&connection.sender, &mut pending);
                        if !initialized || shutdown_requested {
                            break Err(anyhow::anyhow!(
                                "workspace retired during initialization or shutdown"
                            ));
                        }
                        if awaiting_rebind_ready {
                            break Err(anyhow::anyhow!(
                                "workspace retired again before the rebound session became ready"
                            ));
                        }
                        // Only a replayed document can confirm the rebound session: with none
                        // open there is nothing to confirm, and nothing a later rebind could lose.
                        awaiting_rebind_ready = !documents.is_empty();
                        pending_rebind_refresh.clear();
                        for id in pending_server.drain() {
                            connection.sender.send(Message::Notification(
                                lsp_server::Notification::new(
                                    "$/cancelRequest".into(),
                                    serde_json::json!({"id": id}),
                                ),
                            ))?;
                        }
                        for (_, token) in active_progress.drain() {
                            connection.sender.send(Message::Notification(
                                lsp_server::Notification::new(
                                    "$/progress".into(),
                                    serde_json::json!({"token": token, "value": {"kind": "end"}}),
                                ),
                            ))?;
                        }
                        progress_generation = progress_generation.saturating_add(1);
                        let context = LspContext {
                            initialize_params: Some(initialize_params.clone()),
                            announced_capabilities: announced_capabilities.clone(),
                            next_server_request_id,
                            progress_generation,
                        };
                        backend = match connect_backend(&root, &context).await {
                            Ok(backend) => backend,
                            Err(error) => break Err(error.context("LSP workspace rebind failed")),
                        };
                        controls_open = true;
                        // What waited for the retired backend is covered by the replay below and
                        // by the cancellation of its pending requests above.
                        outbox.clear();
                        for uri in documents.uris() {
                            let params = lsp_types::DidOpenTextDocumentParams {
                                text_document: lsp_types::TextDocumentItem {
                                    uri: uri.clone(),
                                    language_id: "bsl".into(),
                                    version: documents.get_version(&uri).unwrap_or_default(),
                                    text: documents.get(&uri).unwrap_or_default(),
                                },
                            };
                            outbox.push_back(Message::Notification(lsp_server::Notification::new(
                                "textDocument/didOpen".into(),
                                params,
                            )));
                        }
                    }
                    Some(LspControl::ResumeLsp(_)) => {
                        break Err(anyhow::anyhow!("unexpected daemon resume control"))
                    }
                    None => controls_open = false,
                },
                RelayEvent::Writable(Some(permit)) => {
                    if let Some(message) = outbox.pop_front() {
                        permit.send(message);
                    }
                }
                RelayEvent::Writable(None) => break Err(backend.write_failure().await),
                RelayEvent::Backend(message) => {
                    prefer_client = true;
                    match message {
                        Some(message) => {
                            if awaiting_rebind_ready
                                && confirms_rebind_publication(&message, &documents)
                            {
                                awaiting_rebind_ready = false;
                            }
                            match &message {
                                Message::Response(response) => {
                                    let Some(method) = pending.remove(&response.id) else {
                                        continue;
                                    };
                                    if method == "initialize" {
                                        initialized = response.error.is_none();
                                        announced_capabilities = response
                                            .result
                                            .as_ref()
                                            .map(|result| result["capabilities"].clone());
                                    }
                                }
                                Message::Request(request) => {
                                    pending_server.insert(request.id.clone());
                                    if awaiting_rebind_ready
                                        && is_supported_rebind_refresh(
                                            &request.method,
                                            rebind_refresh_capabilities,
                                        )
                                    {
                                        pending_rebind_refresh.insert(request.id.clone());
                                    }
                                    if let Some(id) = serde_json::to_value(&request.id)?.as_i64() {
                                        next_server_request_id = next_server_request_id.max(
                                            i32::try_from(id)?
                                                .checked_add(1)
                                                .context("server request id overflow")?,
                                        );
                                    }
                                }
                                Message::Notification(notification) => {
                                    if notification.method == "$/progress" {
                                        let token = notification.params["token"].clone();
                                        let key = serde_json::to_string(&token)?;
                                        match notification.params["value"]["kind"].as_str() {
                                            Some("begin") => {
                                                active_progress.insert(key, token);
                                            }
                                            Some("end") => {
                                                active_progress.remove(&key);
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            connection.sender.send(message)?;
                        }
                        None if shutdown_requested => break Ok(()),
                        None => break Err(backend.disconnected().await),
                    }
                }
                RelayEvent::Client(message) => {
                    prefer_client = false;
                    match message {
                        Some(message) => {
                            match &message {
                                Message::Request(request) => {
                                    shutdown_requested |= request.method == "shutdown";
                                    pending.insert(request.id.clone(), request.method.clone());
                                }
                                Message::Notification(notification) => {
                                    mirror_document(&mut documents, notification, encoding)
                                }
                                Message::Response(response) => {
                                    if !pending_server.remove(&response.id) {
                                        continue;
                                    }
                                    if rebind_refresh_completed(
                                        response,
                                        &mut pending_rebind_refresh,
                                    ) {
                                        awaiting_rebind_ready = false;
                                    }
                                }
                            }
                            outbox.push_back(message);
                        }
                        None => break backend.finish().await,
                    }
                }
            }
        }
    }
    .await;
    cancel_pending(&connection.sender, &mut pending);
    result
}

#[derive(Clone, Copy)]
struct RebindRefreshCapabilities {
    semantic_tokens: bool,
    workspace_diagnostics: bool,
}

impl RebindRefreshCapabilities {
    fn from_initialize(params: &serde_json::Value) -> Self {
        Self {
            semantic_tokens: params["capabilities"]["workspace"]["semanticTokens"]
                ["refreshSupport"]
                .as_bool()
                == Some(true),
            workspace_diagnostics: ["diagnostics", "diagnostic"].iter().any(|key| {
                params["capabilities"]["workspace"][key]["refreshSupport"].as_bool() == Some(true)
            }),
        }
    }
}

fn confirms_rebind_publication(
    message: &Message,
    documents: &bsl_analyzer::mem_docs::MemDocs,
) -> bool {
    match message {
        Message::Notification(notification)
            if notification.method == "textDocument/publishDiagnostics" =>
        {
            let Some(uri) =
                notification.params["uri"].as_str().and_then(|uri| lsp_types::Url::parse(uri).ok())
            else {
                return false;
            };
            let Some(version) = documents.get_version(&uri) else { return false };
            notification.params["version"].as_i64() == Some(i64::from(version))
                || (notification.params["version"].is_null()
                    && notification.params["diagnostics"].as_array().is_some_and(Vec::is_empty))
        }
        _ => false,
    }
}

fn is_supported_rebind_refresh(method: &str, capabilities: RebindRefreshCapabilities) -> bool {
    match method {
        "workspace/semanticTokens/refresh" => capabilities.semantic_tokens,
        "workspace/diagnostic/refresh" => capabilities.workspace_diagnostics,
        _ => false,
    }
}

fn rebind_refresh_completed(
    response: &lsp_server::Response,
    pending: &mut HashSet<RequestId>,
) -> bool {
    pending.remove(&response.id) && response.error.is_none()
}

fn cancel_pending(
    sender: &crossbeam_channel::Sender<Message>,
    pending: &mut HashMap<RequestId, String>,
) {
    for (id, _) in pending.drain() {
        let response = lsp_server::Response::new_err(
            id,
            lsp_server::ErrorCode::ContentModified as i32,
            "Workspace changed or backend disconnected; request cancelled".into(),
        );
        let _ = sender.send(Message::Response(response));
    }
}

fn mirror_document(
    documents: &mut bsl_analyzer::mem_docs::MemDocs,
    notification: &lsp_server::Notification,
    encoding: bsl_analyzer::lsp::PositionEncoding,
) {
    // Like the standalone dispatcher, a notification whose params do not parse is logged
    // and skipped: it must not end the session.
    fn params<P: serde::de::DeserializeOwned>(
        notification: &lsp_server::Notification,
    ) -> Option<P> {
        serde_json::from_value(notification.params.clone())
            .inspect_err(|error| {
                tracing::error!(
                    method = notification.method,
                    %error,
                    "failed to parse notification params"
                );
            })
            .ok()
    }

    match notification.method.as_str() {
        "textDocument/didOpen" => {
            let Some(params) = params::<lsp_types::DidOpenTextDocumentParams>(notification) else {
                return;
            };
            let doc = params.text_document;
            if doc.uri.to_file_path().is_ok_and(|path| project_model::is_bsl_source_path(&path)) {
                documents.insert(doc.uri, doc.text, doc.version);
            }
        }
        "textDocument/didChange" => {
            let Some(params) = params::<lsp_types::DidChangeTextDocumentParams>(notification)
            else {
                return;
            };
            let uri = params.text_document.uri;
            let _ = documents.update_versioned(
                &uri,
                params.content_changes,
                encoding,
                params.text_document.version,
            );
        }
        "textDocument/didClose" => {
            let Some(params) = params::<lsp_types::DidCloseTextDocumentParams>(notification) else {
                return;
            };
            documents.remove(&params.text_document.uri);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A relay that stalls leaves these waits with nothing else to end them.
    const GUARD: Duration = test_utils::wait::hang_guard(Duration::from_secs(30));

    async fn guarded<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(GUARD, future)
            .await
            .unwrap_or_else(|_| panic!("{what} never completed"))
    }

    fn relay_over(
        io: tokio::io::DuplexStream,
    ) -> (Connection, tokio::task::JoinHandle<Result<()>>) {
        let (server, client) = Connection::memory();
        let (send_control, controls) = tokio::sync::mpsc::channel(1);
        let initialize = Message::Request(lsp_server::Request::new(
            RequestId::from(1),
            "initialize".into(),
            serde_json::json!({"capabilities": {}}),
        ));
        let relay = tokio::spawn(relay_lsp(
            server,
            BackendPipe::new(io, controls, send_control),
            PathBuf::new(),
            initialize,
            serde_json::json!({"capabilities": {}}),
            lsp_types::InitializeParams::default(),
        ));
        (client, relay)
    }

    #[tokio::test]
    async fn relay_forwards_the_backend_while_a_write_to_it_is_blocked() {
        let (relay_io, backend_io) = tokio::io::duplex(4096);
        let (backend_read, mut backend_write) = tokio::io::split(backend_io);
        let mut backend_read = tokio::io::BufReader::new(backend_read);
        let (client, relay) = relay_over(relay_io);
        let initialize =
            guarded("the initialize request", shared::read_lsp_message(&mut backend_read))
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(initialize, Message::Request(request) if request.method == "initialize"));

        client
            .sender
            .send(Message::Request(lsp_server::Request::new(
                RequestId::from(2),
                "test/large".into(),
                "я".repeat(64 * 1024),
            )))
            .unwrap();
        // The backend stops reading once the large request has started to arrive, so the
        // rest of it cannot be written while the backend has output of its own to deliver.
        let mut first = [0u8; 1];
        guarded("the large request", backend_read.read_exact(&mut first)).await.unwrap();
        guarded(
            "the backend's notification",
            shared::write_lsp_message(
                &mut backend_write,
                &Message::Notification(lsp_server::Notification::new(
                    "test/progress".into(),
                    serde_json::Value::Null,
                )),
            ),
        )
        .await
        .unwrap();

        let (client, forwarded) = tokio::task::spawn_blocking(move || {
            let forwarded = client
                .receiver
                .recv_timeout(GUARD)
                .expect("the backend's notification is forwarded");
            (client, forwarded)
        })
        .await
        .unwrap();
        assert!(
            matches!(forwarded, Message::Notification(notification) if notification.method == "test/progress")
        );
        relay.abort();
        drop(client);
    }

    #[tokio::test]
    async fn a_backend_framing_error_is_the_reported_cause() {
        let (relay_io, backend_io) = tokio::io::duplex(4096);
        let (backend_read, mut backend_write) = tokio::io::split(backend_io);
        let mut backend_read = tokio::io::BufReader::new(backend_read);
        let (client, relay) = relay_over(relay_io);
        guarded("the initialize request", shared::read_lsp_message(&mut backend_read))
            .await
            .unwrap()
            .unwrap();

        backend_write.write_all(b"Content-Length: 50\r\n\r\n{}").await.unwrap();
        backend_write.shutdown().await.unwrap();

        let error = guarded("the relay", relay).await.unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("truncated LSP body"), "{error:#}");
        drop(client);
    }

    #[tokio::test]
    async fn relay_services_both_ready_data_producers_and_prioritizes_retirement() {
        let (control_tx, mut controls) = tokio::sync::mpsc::channel(1);
        let (backend_tx, mut backend) = tokio::sync::mpsc::channel(1);
        let (client_tx, mut client) = tokio::sync::mpsc::channel(1);
        let message = || {
            Message::Notification(lsp_server::Notification::new(
                "test".into(),
                serde_json::Value::Null,
            ))
        };
        backend_tx.send(message()).await.unwrap();
        client_tx.send(message()).await.unwrap();
        assert!(matches!(
            next_relay_event(&mut controls, no_room(), &mut backend, &mut client, true, false)
                .await,
            RelayEvent::Backend(Some(_))
        ));
        backend_tx.send(message()).await.unwrap();
        assert!(matches!(
            next_relay_event(&mut controls, no_room(), &mut backend, &mut client, true, true).await,
            RelayEvent::Client(Some(_))
        ));
        client_tx.send(message()).await.unwrap();
        control_tx
            .send(LspControl::ScopeRetired { epoch: 1, reason: "test".into() })
            .await
            .unwrap();
        assert!(matches!(
            next_relay_event(&mut controls, no_room(), &mut backend, &mut client, true, true).await,
            RelayEvent::Control(Some(LspControl::ScopeRetired { .. }))
        ));
        assert!(matches!(
            next_relay_event(&mut controls, no_room(), &mut backend, &mut client, false, false)
                .await,
            RelayEvent::Backend(Some(_))
        ));

        // While a message waits for room toward the backend, the client is not read even
        // when it has the turn; the backend still is.
        backend_tx.send(message()).await.unwrap();
        let waiting = Some(std::future::pending::<Option<OwnedPermit>>());
        assert!(matches!(
            next_relay_event(&mut controls, waiting, &mut backend, &mut client, false, true).await,
            RelayEvent::Backend(Some(_))
        ));
        assert!(matches!(
            next_relay_event(&mut controls, no_room(), &mut backend, &mut client, false, true)
                .await,
            RelayEvent::Client(Some(_))
        ));
    }

    fn no_room() -> Option<std::future::Pending<Option<OwnedPermit>>> {
        None
    }

    #[test]
    fn resume_buffers_keep_the_current_text_and_wire_version() {
        let root = tempfile::tempdir().unwrap();
        let uri = lsp_types::Url::from_file_path(root.path().join("Модуль.bsl")).unwrap();
        let mut docs = bsl_analyzer::mem_docs::MemDocs::new();
        let encoding = bsl_analyzer::lsp::PositionEncoding::negotiate(&Default::default());
        mirror_document(
            &mut docs,
            &lsp_server::Notification::new(
                "textDocument/didOpen".into(),
                serde_json::json!({"textDocument": {"uri": uri, "languageId":"bsl", "version":17, "text":"Диск"}}),
            ),
            encoding,
        );
        mirror_document(
            &mut docs,
            &lsp_server::Notification::new(
                "textDocument/didChange".into(),
                serde_json::json!({"textDocument": {"uri": uri, "version":22}, "contentChanges":[{"text":"Несохранённый буфер"}]}),
            ),
            encoding,
        );
        assert_eq!(docs.get(&uri).as_deref(), Some("Несохранённый буфер"));
        assert_eq!(docs.get_version(&uri), Some(22));
        mirror_document(
            &mut docs,
            &lsp_server::Notification::new(
                "textDocument/didClose".into(),
                serde_json::json!({"textDocument": {"uri": uri}}),
            ),
            encoding,
        );
        assert!(docs.is_empty());
    }

    #[test]
    fn unparsable_document_notifications_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        let uri = lsp_types::Url::from_file_path(root.path().join("Модуль.bsl")).unwrap();
        let mut docs = bsl_analyzer::mem_docs::MemDocs::new();
        let encoding = bsl_analyzer::lsp::PositionEncoding::negotiate(&Default::default());
        mirror_document(
            &mut docs,
            &lsp_server::Notification::new(
                "textDocument/didOpen".into(),
                serde_json::json!({"textDocument": {"uri": uri, "languageId":"bsl", "version":1, "text":"Буфер"}}),
            ),
            encoding,
        );
        for (method, params) in [
            ("textDocument/didOpen", serde_json::json!({"textDocument": {"uri": uri}})),
            (
                "textDocument/didChange",
                serde_json::json!({"textDocument": {"uri": uri, "version": 2}}),
            ),
            ("textDocument/didClose", serde_json::json!({})),
        ] {
            let notification = lsp_server::Notification::new(method.into(), params);
            mirror_document(&mut docs, &notification, encoding);
        }
        assert_eq!(docs.get(&uri).as_deref(), Some("Буфер"));
        assert_eq!(docs.get_version(&uri), Some(1));
    }

    #[test]
    fn retirement_cancels_each_accepted_request_once() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut pending = HashMap::from([
            (RequestId::from(7), "hover".into()),
            (RequestId::from("named".to_owned()), "definition".into()),
        ]);
        cancel_pending(&sender, &mut pending);
        cancel_pending(&sender, &mut pending);
        let responses = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        assert!(responses.iter().all(|message| matches!(message, Message::Response(response) if response.error.as_ref().unwrap().code == lsp_server::ErrorCode::ContentModified as i32)));
    }

    #[test]
    fn rebind_readiness_uses_current_publication_or_supported_refresh() {
        let root = tempfile::tempdir().unwrap();
        let uri = lsp_types::Url::from_file_path(root.path().join("Модуль.bsl")).unwrap();
        let other_uri =
            lsp_types::Url::from_file_path(root.path().join("Исключенный.bsl")).unwrap();
        let mut documents = bsl_analyzer::mem_docs::MemDocs::new();
        documents.insert(uri.clone(), "source".to_owned(), 12);
        documents.insert(other_uri.clone(), "excluded".to_owned(), 4);
        let no_refresh =
            RebindRefreshCapabilities { semantic_tokens: false, workspace_diagnostics: false };
        let refresh =
            RebindRefreshCapabilities { semantic_tokens: true, workspace_diagnostics: true };

        let current = Message::Notification(lsp_server::Notification::new(
            "textDocument/publishDiagnostics".into(),
            serde_json::json!({"uri": uri, "version": 12, "diagnostics": []}),
        ));
        assert!(confirms_rebind_publication(&current, &documents));

        let stale = Message::Notification(lsp_server::Notification::new(
            "textDocument/publishDiagnostics".into(),
            serde_json::json!({"uri": uri, "version": 11, "diagnostics": []}),
        ));
        assert!(!confirms_rebind_publication(&stale, &documents));

        let excluded = Message::Notification(lsp_server::Notification::new(
            "textDocument/publishDiagnostics".into(),
            serde_json::json!({"uri": other_uri, "diagnostics": []}),
        ));
        assert!(confirms_rebind_publication(&excluded, &documents));

        let semantic_refresh = Message::Request(lsp_server::Request::new(
            RequestId::from(9),
            "workspace/semanticTokens/refresh".into(),
            serde_json::Value::Null,
        ));
        let semantic_request = match &semantic_refresh {
            Message::Request(request) => request,
            _ => unreachable!(),
        };
        assert!(!is_supported_rebind_refresh(&semantic_request.method, no_refresh));
        assert!(is_supported_rebind_refresh(&semantic_request.method, refresh));

        let refresh_id = RequestId::from(10);
        let mut pending_refresh = HashSet::from([refresh_id.clone()]);
        let rejected = lsp_server::Response::new_err(
            refresh_id.clone(),
            lsp_server::ErrorCode::RequestFailed as i32,
            "refresh rejected".into(),
        );
        assert!(!rebind_refresh_completed(&rejected, &mut pending_refresh));
        assert!(pending_refresh.is_empty());

        let successful_id = RequestId::from(11);
        let mut pending_refresh = HashSet::from([successful_id.clone()]);
        let success = lsp_server::Response::new_ok(successful_id, serde_json::Value::Null);
        assert!(rebind_refresh_completed(&success, &mut pending_refresh));

        let window_message = Message::Notification(lsp_server::Notification::new(
            "window/showMessage".into(),
            serde_json::json!({"type": 1, "message": "ready? no"}),
        ));
        assert!(!confirms_rebind_publication(&window_message, &documents));
    }
}
