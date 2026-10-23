//! Upper adapter dispatch for the existing local broker.

use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::UNIX_EPOCH};

use anyhow::{Context, Result};
use mcp_server::broker::{
    daemon::{self, SharedSession},
    transport::{self, AdapterKind, Handshake, LspChannel, LspControl},
    SharedBackendIdentity, SharedBackendKey,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::{broker_idle_ttl, broker_orphan_grace, resolve_embedding_prefixes, ServerInputs};

pub(crate) const AUTO_DAEMON_ENV: &str = "BSL_SHARED_AUTO_DAEMON";

/// Prime the selected remote help publication only on a cold cache, before
/// deriving a rendezvous identity. Revalidation paths call identity directly
/// and therefore never perform network or corpus I/O.
pub(crate) fn prepare_platform_identity(source: &Path) -> Result<()> {
    let project = mcp_server::project::at(source)?;
    // An invalid selection serves no help (see `platform_help::bootstrap`): nothing to prime.
    let Ok(request) = platform_help::requested_source(Some(&project.config), source) else {
        return Ok(());
    };
    if let bsl_platform::PlatformHelpRequest::ExternalUrl(url) = request {
        let context = platform_help::LoadContext::from_environment();
        if platform_help::external::snapshot_fingerprint(&url, &context).is_none() {
            if let Err(error) = platform_help::external::load_url(&url, &context) {
                // Match platform-help's normal cold-cache behavior: an unavailable
                // source degrades to a missing help snapshot, not a failed server.
                tracing::debug!(%error, "external platform help cache prime failed");
            }
        }
    }
    Ok(())
}

pub(crate) fn identity(
    source: &Path,
    cache: &mcp_server::WorkspaceCacheLayout,
) -> Result<SharedBackendIdentity> {
    let project = mcp_server::project::at(source)?;
    cache.verify_project(&project)?;
    let config = &project.config;
    // Explicit allow-list: credential helper arguments and URL userinfo can carry
    // secrets, so never put them into a shared backend identity.
    let settings = safe_config_fingerprint(config);
    let embedding = resolve_embedding_prefixes(Some(source)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut config_hash = blake3::Hasher::new();
    config_hash.update(&settings);
    config_hash.update(&mcp_server::broker::workspace_topology_fingerprint(source).to_le_bytes());
    config_hash.update(
        &mcp_server::broker::embedding_config_fingerprint_with_prefixes(&embedding).to_le_bytes(),
    );
    let mut platform_hash = blake3::Hasher::new();
    fingerprint_requested_platform_source(
        &mut platform_hash,
        platform_help::requested_source(Some(config), source),
    );
    let platform = platform_hash.finalize();
    let stamp = cache.scope_stamp();
    let scope =
        blake3::hash(stamp.split_once(':').context("cache scope stamp missing")?.1.as_bytes());
    Ok(SharedBackendIdentity::new(
        source,
        cache.root(),
        *scope.as_bytes(),
        *config_hash.finalize().as_bytes(),
        *platform.as_bytes(),
        SharedBackendIdentity::current_exe_build_fingerprint()?,
    ))
}

fn safe_config_fingerprint(config: &project_model::ProjectConfig) -> Vec<u8> {
    use serde_json::json;

    let embedding = &config.search.baseline.embedding;
    let postgres = &config.search.baseline.postgres;
    let embedding_url =
        embedding.url.as_deref().map(mcp_server::broker::embedding_endpoint_identity);
    let safe = json!({
        "diagnostics": config.diagnostics.rules_json(),
        "diagnostics_baseline": format!("{:?}", config.diagnostics.baseline),
        "code_lens": [config.code_lens.show_cognitive_complexity, config.code_lens.show_cyclomatic_complexity],
        "formatting": [config.formatting.indent_size, config.formatting.use_tabs],
        "configuration_root": config.configuration_root,
        "configuration_dependency": config.configuration_dependency,
        "source_exclude": config.source_exclude,
        "target_platform_version": config.target_platform_version,
        "min_platform_version": config.min_platform_version,
        "compatibility_mode": config.compatibility_mode,
        "language": config.language,
        "extensions": format!("{:?}", config.extensions),
        "externals": format!("{:?}", config.externals),
        "search": {
            "backend": format!("{:?}", config.search.baseline.backend),
            "postgres": [postgres.host, postgres.port, postgres.dbname, postgres.schema, postgres.vault_role_base],
            "embedding": [embedding.model, embedding.dimension, embedding.provider, embedding_url,
                embedding.query_prefix, embedding.document_prefix, embedding.max_input_tokens,
                embedding.tokenizer_file, embedding.tokenizer_sha256],
            "workspace_code": format!("{:?}", config.search.baseline.workspace_code),
            "reference": format!("{:?}", config.search.baseline.reference),
        },
        "features": [config.features.type_narrowing, format!("{:?}", config.features.workspace_diagnostics), format!("{:?}", config.features.checked_environments)],
        "output_language": config.output.display_language,
        "analysis": format!("{:?}", config.analysis),
    });
    serde_json::to_vec(&safe).expect("safe config fingerprint serializes")
}

/// A selection the process cannot read is served as missing help rather than failing the
/// server (see `platform_help::bootstrap`), so it is an identity of its own, named by why
/// it was refused.
fn fingerprint_requested_platform_source(
    hasher: &mut blake3::Hasher,
    request: Result<bsl_platform::PlatformHelpRequest, String>,
) {
    match request {
        Ok(request) => fingerprint_platform_source(hasher, &request),
        Err(reason) => {
            hasher.update(b"invalid\0");
            hasher.update(reason.as_bytes());
        }
    }
}

fn fingerprint_platform_source(
    hasher: &mut blake3::Hasher,
    request: &bsl_platform::PlatformHelpRequest,
) {
    use bsl_platform::PlatformHelpRequest as Request;
    use platform_help::installed::{self, LoadContext};

    match request {
        Request::ExternalPath(path) => {
            hasher.update(b"external-path\0");
            stamp_path(hasher, path);
            let manifest = if path.is_dir() {
                path.join("manifest.json")
            } else if path.file_name().is_some_and(|name| name == "manifest.json") {
                path.to_path_buf()
            } else {
                path.parent().unwrap_or(Path::new(".")).join("manifest.json")
            };
            stamp_manifest(hasher, &manifest);
        }
        Request::Installed { path } => {
            hasher.update(b"installed\0");
            stamp_installed(hasher, path.as_deref(), &LoadContext::from_environment());
        }
        Request::Auto => {
            hasher.update(b"auto\0");
            let context = LoadContext::from_environment();
            if let Ok(pair) = installed::locate(None, &context) {
                stamp_installed_pair(hasher, &pair.dir, &pair.shcntx, &pair.shlang);
            } else {
                let snapshot = installed::AutoSnapshot::new(&context);
                stamp_slot_snapshot(hasher, snapshot.location());
                if !snapshot.location().exists() {
                    hasher.update(bsl_platform::BUNDLED_PLATFORM_VERSION.as_bytes());
                }
            }
        }
        Request::ExternalUrl(url) => {
            hasher.update(b"external-url\0");
            hasher.update(bsl_platform::redact_url(url).as_bytes());
            let context = LoadContext::from_environment();
            match platform_help::external::snapshot_fingerprint(url, &context) {
                Some(snapshot) => {
                    hasher.update(&snapshot);
                }
                None => {
                    hasher.update(b"external-snapshot-not-cached");
                }
            }
        }
        Request::Bundled => {
            hasher.update(b"bundled\0");
            hasher.update(bsl_platform::BUNDLED_PLATFORM_VERSION.as_bytes());
        }
        Request::None => {
            hasher.update(b"none\0");
        }
        Request::Unselected => {
            hasher.update(b"unselected\0");
        }
    }
}

fn stamp_installed(
    hasher: &mut blake3::Hasher,
    path: Option<&Path>,
    context: &platform_help::LoadContext,
) {
    if let Ok(pair) = platform_help::installed::locate(path, context) {
        stamp_installed_pair(hasher, &pair.dir, &pair.shcntx, &pair.shlang);
    } else {
        if let Some(path) = path {
            hasher.update(path.as_os_str().as_encoded_bytes());
        }
        for root in &context.discovery_roots {
            stamp_path(hasher, root);
        }
        if let Some(path) = &context.platform_path_env {
            hasher.update(path.as_encoded_bytes());
        }
    }
}

fn stamp_installed_pair(hasher: &mut blake3::Hasher, dir: &Path, shcntx: &Path, shlang: &Path) {
    hasher.update(dir.as_os_str().as_encoded_bytes());
    if let Some(version) = dir.file_name().and_then(|value| value.to_str()) {
        hasher.update(version.as_bytes());
    }
    stamp_path(hasher, shcntx);
    stamp_path(hasher, shlang);
}

fn stamp_slot_snapshot(hasher: &mut blake3::Hasher, slot: &Path) {
    let Ok(package_name) = fs::read_to_string(slot.join("current")) else {
        stamp_file_metadata(hasher, &slot.join("current"));
        return;
    };
    hasher.update(package_name.trim().as_bytes());
    let package = slot.join(package_name.trim());
    stamp_file_metadata(hasher, &package.join("manifest.json"));
    stamp_file_contents(hasher, &package.join("manifest.json"));
    stamp_file_metadata(hasher, &package.join("corpus.json"));
    stamp_file_metadata(hasher, &slot.join("source.json"));
    stamp_file_contents(hasher, &slot.join("source.json"));
}

fn stamp_manifest(hasher: &mut blake3::Hasher, path: &Path) {
    stamp_path(hasher, path);
    stamp_file_contents(hasher, path);
}

fn stamp_file_contents(hasher: &mut blake3::Hasher, path: &Path) {
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() <= 1024 * 1024) {
        if let Ok(bytes) = fs::read(path) {
            hasher.update(blake3::hash(&bytes).as_bytes());
        }
    }
}

fn stamp_path(hasher: &mut blake3::Hasher, path: &Path) {
    hasher.update(path.as_os_str().as_encoded_bytes());
    stamp_file_metadata(hasher, path);
}

fn stamp_file_metadata(hasher: &mut blake3::Hasher, path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        hasher.update(&metadata.len().to_le_bytes());
        if let Ok(modified) = metadata
            .modified()
            .and_then(|time| time.duration_since(UNIX_EPOCH).map_err(std::io::Error::other))
        {
            hasher.update(&modified.as_nanos().to_le_bytes());
        }
    } else {
        hasher.update(b"missing");
    }
}

/// Worker admission semaphores shared by every session of one daemon, so that
/// `max_concurrent` bounds a connection rather than each session attached to it.
#[derive(Default)]
struct ExecutorSlots(std::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Semaphore>>>);

impl ExecutorSlots {
    fn profile(
        &self,
        connection: &str,
        executor: super::CheckExecutorConfig,
    ) -> Result<mcp_server::CheckExecutorProfile> {
        // The permit count is fixed when a semaphore is created, so a session that asks for a
        // different `max_concurrent` gets its own semaphore instead of resizing a running one.
        // Every other limit varies per session without splitting the connection's admission.
        let key = format!("{connection}\0{}", executor.max_concurrent);
        let mut profile = executor.into_profile()?;
        let mut slots = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        profile.semaphore = slots.entry(key).or_insert(profile.semaphore).clone();
        Ok(profile)
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct NativeContext {
    pub url: Option<String>,
    pub user: String,
    pub password: String,
    named: BTreeMap<String, NamedConnection>,
}

#[derive(Clone, Serialize, Deserialize)]
struct NamedConnection {
    url: Option<String>,
    user: String,
    password: String,
    allow_execute: bool,
    check_executor: Option<super::CheckExecutorConfig>,
}

impl NativeContext {
    pub(crate) fn from_caller(url: Option<String>, user: String, password: String) -> Result<Self> {
        let mut context = Self { url, user, password, named: BTreeMap::new() };
        if let Some(path) = std::env::var_os("BSL_ONEC_CONNECTIONS_FILE") {
            let contents = std::fs::read_to_string(path)?;
            let config: super::OnecConnectionsFile = serde_json::from_str(&contents)?;
            context.add_named(config)?;
        }
        Ok(context)
    }

    fn add_named(&mut self, config: super::OnecConnectionsFile) -> Result<()> {
        for (name, entry) in config.connections {
            anyhow::ensure!(
                !name.trim().is_empty()
                    && entry.url.as_deref().is_none_or(|url| !url.trim().is_empty())
                    && (entry.url.is_some() || entry.check_executor.is_some()),
                "1C connection name must not be empty and needs url or check_executor"
            );
            if let Some(executor) = entry.check_executor.clone() {
                executor.into_profile()?;
            }
            let resolve = |key: &str| -> Result<String> {
                if key.is_empty() {
                    Ok(String::new())
                } else {
                    std::env::var(key)
                        .with_context(|| format!("missing environment variable {key}"))
                }
            };
            let (user, password) = if entry.url.is_some() {
                (resolve(&entry.user_env)?, resolve(&entry.password_env)?)
            } else {
                (String::new(), String::new())
            };
            self.named.insert(
                name,
                NamedConnection {
                    url: entry.url,
                    user,
                    password,
                    allow_execute: entry.allow_execute,
                    check_executor: entry.check_executor,
                },
            );
        }
        Ok(())
    }

    fn install(&self, config: &mut mcp_server::SessionConfig, slots: &ExecutorSlots) -> Result<()> {
        if let Some(url) = &self.url {
            config.set_onec_client(onec_client::Client::new(url, &self.user, &self.password));
        }
        for (name, native) in &self.named {
            let client = native
                .url
                .as_ref()
                .map(|url| onec_client::Client::new(url, &native.user, &native.password));
            let executor = native
                .check_executor
                .clone()
                .map(|executor| slots.profile(name, executor))
                .transpose()?;
            config.add_onec_connection(
                name.clone(),
                mcp_server::OnecConnection::with_optional_http(
                    client,
                    native.allow_execute,
                    executor,
                ),
            );
        }
        Ok(())
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct McpContext {
    pub enable_tools: Vec<String>,
    pub native: NativeContext,
    pub supervisor_preset: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct LspContext {
    pub initialize_params: Option<serde_json::Value>,
    pub announced_capabilities: Option<serde_json::Value>,
    pub next_server_request_id: i32,
    pub progress_generation: u64,
}

pub(crate) async fn attach<S>(
    stream: &mut S,
    identity: SharedBackendIdentity,
    adapter: AdapterKind,
    context: &[u8],
) -> Result<()>
where
    S: tokio::io::AsyncRead + AsyncWrite + Unpin,
{
    transport::client_handshake(stream, &Handshake { identity, adapter }).await?;
    transport::write_session_context(stream, context).await
}

pub(super) async fn run_daemon(inputs: ServerInputs) -> Result<()> {
    // Registered before the endpoint and the owner exist, so a stop signal never takes the
    // default action that would leave the writer lease looking live.
    let signal = super::shutdown_signal()?;
    SharedBackendIdentity::prefetch_build_fingerprint();
    let source = inputs.source_dir.clone().context("workspace daemon requires source")?;
    let cache = inputs.workspace_cache.clone().context("workspace daemon requires cache")?;
    prepare_platform_identity(&source)?;
    // The launching client waits for its `initialize` answer through the whole daemon
    // start, and loading the help is the longest part of it: it runs while the daemon
    // derives its identity, binds the endpoint and takes the first session.
    let help_preload = Arc::new(std::sync::Mutex::new(Some(std::thread::spawn({
        let source = source.clone();
        move || bsl_analyzer::help_bootstrap::bootstrap_for_root(&source, None)
    }))));
    let expected = identity(&source, &cache)?;
    let key = SharedBackendKey::new(expected.clone());
    let preset = if std::env::var_os(AUTO_DAEMON_ENV).is_none() {
        Some(NativeContext::from_caller(inputs.onec_url, inputs.onec_user, inputs.onec_password)?)
    } else {
        None
    };
    let epoch = mcp_server::broker::process_epoch();
    let executor_slots = Arc::new(ExecutorSlots::default());
    let owner: Arc<tokio::sync::OnceCell<mcp_server::McpServer>> =
        Arc::new(tokio::sync::OnceCell::new());
    let markers_enabled = std::env::var_os("BSL_SHARED_BACKEND_MARKERS").is_some();
    let retiring = CancellationToken::new();
    let retired = retiring.clone();
    let shutdown_owner = owner.clone();
    let lease_owner = owner.clone();
    let on_session = move |hello: Handshake,
                           bytes: Vec<u8>,
                           session,
                           stop: CancellationToken,
                           _retired: CancellationToken| {
        let owner = owner.clone();
        let source = source.clone();
        let cache = cache.clone();
        let expected = expected.clone();
        let preset = preset.clone();
        let executor_slots = executor_slots.clone();
        let retiring = retiring.clone();
        let help_preload = help_preload.clone();
        let epoch = epoch;
        if markers_enabled {
            tracing::info!(
                target: "bsl_analyzer::shared_backend",
                event = "session_attached",
                owner_epoch = epoch,
                adapter = match hello.adapter {
                    AdapterKind::Mcp => "mcp",
                    AdapterKind::Lsp => "lsp",
                },
                "shared backend session attached"
            );
        }
        async move {
            let initialized = async move {
                owner
                    .get_or_try_init(|| async {
                        if markers_enabled {
                            tracing::info!(
                                target: "bsl_analyzer::shared_backend",
                                event = "owner_init_start",
                                owner_epoch = epoch,
                                owner_init_id = epoch,
                                daemon_pid = std::process::id(),
                                "shared backend state initialization started"
                            );
                        }
                        let owner = tokio::task::spawn_blocking(move || {
                            anyhow::ensure!(
                                identity(&source, &cache)? == expected,
                                "workspace settings changed before daemon attach"
                            );
                            let preload =
                                help_preload.lock().unwrap_or_else(|p| p.into_inner()).take();
                            if let Some(preload) = preload {
                                let _ = preload.join();
                            }
                            bsl_analyzer::help_bootstrap::bootstrap_for_root(&source, None);
                            anyhow::ensure!(
                                identity(&source, &cache)? == expected,
                                "platform or workspace identity changed during daemon bootstrap"
                            );
                            let prefixes = resolve_embedding_prefixes(Some(&source))
                                .map_err(|e| anyhow::anyhow!("{e}"))?;
                            let identity_root = source.clone();
                            let identity_cache = cache.clone();
                            let state =
                                mcp_server::SharedState::shared_workspace_with_cache_and_prefixes(
                                    source,
                                    cache,
                                    Some(prefixes),
                                )?;
                            anyhow::ensure!(
                                state.set_backend_identity_check(move || {
                                    identity(&identity_root, &identity_cache)
                                        .is_ok_and(|current| current == expected)
                                }),
                                "workspace identity changed before adapter attach"
                            );
                            anyhow::Ok(mcp_server::McpServer::new(
                                mcp_server::McpProfile::Workspace,
                                state,
                            ))
                        })
                        .await??;
                        if markers_enabled {
                            tracing::info!(
                                target: "bsl_analyzer::shared_backend",
                                event = "owner_init_complete",
                                owner_epoch = epoch,
                                owner_init_id = epoch,
                                "shared backend state initialized"
                            );
                        }
                        let scope_stop = owner.shared_state().backend_retired();
                        tokio::spawn(async move {
                            scope_stop.cancelled().await;
                            retiring.cancel();
                        });
                        anyhow::Ok(owner)
                    })
                    .await
                    .cloned()
            };
            match session {
                SharedSession::Mcp(stream) => {
                    let server = initialized.await?;
                    let context: McpContext = serde_json::from_slice(&bytes)?;
                    mcp_server::contract::validate_enabled_tools(
                        mcp_server::McpProfile::Workspace,
                        &context.enable_tools,
                    )
                    .map_err(anyhow::Error::msg)?;
                    let native = if context.supervisor_preset {
                        preset
                            .as_ref()
                            .context("daemon has no explicit supervisor native preset")?
                    } else {
                        &context.native
                    };
                    let mut settings = mcp_server::SessionConfig::default();
                    native.install(&mut settings, &executor_slots)?;
                    for tool in context.enable_tools {
                        settings.enable_tool(tool);
                    }
                    server.shared_state().ensure_workspace_services();
                    mcp_server::serve_stream_with_shutdown(
                        server.for_session(settings),
                        stream,
                        stop,
                    )
                    .await
                }
                SharedSession::Lsp(channel) => {
                    let context: LspContext = serde_json::from_slice(&bytes)?;
                    // The editor's `initialize` is answered from the project settings alone,
                    // so the session takes it before the owner exists: loading the platform
                    // help and the owner state is the longest part of a daemon start, and
                    // every later request waits for the owner anyway.
                    let runtime = tokio::runtime::Handle::current();
                    let workspace = move || -> anyhow::Result<mcp_server::WorkspaceHandle> {
                        Ok(runtime.block_on(initialized)?.shared_state().workspace_handle())
                    };
                    serve_lsp(channel, workspace, context, stop, _retired, epoch).await
                }
            }
        }
    };
    let stop = CancellationToken::new();
    let signalled = stop.clone();
    tokio::spawn(async move {
        match signal.await {
            Ok(()) => signalled.cancel(),
            Err(error) => tracing::warn!(%error, "waiting for a stop signal failed"),
        }
    });
    daemon::run_shared(
        key,
        on_session,
        retired,
        stop,
        broker_orphan_grace(),
        broker_idle_ttl(),
        move || {
            let Some(server) = lease_owner.get() else { return (false, false) };
            let superseded = server.superseded();
            (superseded, superseded && server.graph_released())
        },
        move || {
            if let Some(server) = shutdown_owner.get() {
                server.shutdown();
            }
        },
    )
    .await
}

async fn serve_lsp(
    channel: LspChannel,
    workspace: impl FnOnce() -> anyhow::Result<mcp_server::WorkspaceHandle> + Send + 'static,
    context: LspContext,
    stop: CancellationToken,
    retired: CancellationToken,
    epoch: u64,
) -> Result<()> {
    let (io, mut controls, controls_out) = channel.into_parts();
    let context = if context.initialize_params.is_some() {
        match tokio::time::timeout(std::time::Duration::from_secs(5), controls.recv()).await? {
            Some(LspControl::ResumeLsp(bytes)) => {
                let resumed: LspContext = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(
                    resumed.initialize_params == context.initialize_params
                        && resumed.announced_capabilities == context.announced_capabilities
                        && resumed.next_server_request_id == context.next_server_request_id
                        && resumed.progress_generation == context.progress_generation,
                    "LSP resume context differs from attach context"
                );
                resumed
            }
            _ => anyhow::bail!("expected private ResumeLsp before resumed documents"),
        }
    } else {
        context
    };
    let (read, mut write) = tokio::io::split(io);
    let (input_tx, input_rx) = crossbeam_channel::bounded(32);
    let (output_tx, output_rx) = crossbeam_channel::bounded(32);
    let worker = tokio::task::spawn_blocking(move || {
        let connection = lsp_server::Connection { sender: output_tx, receiver: input_rx };
        match context.initialize_params {
            Some(params) => bsl_analyzer::main_loop_resumed(
                connection,
                workspace,
                params,
                context.announced_capabilities,
                context.next_server_request_id,
                context.progress_generation,
            ),
            None => bsl_analyzer::main_loop_shared(connection, workspace),
        }
    });
    let reader = tokio::spawn(async move {
        let mut read = tokio::io::BufReader::new(read);
        loop {
            match read_lsp_message(&mut read).await {
                Ok(Some(message)) => {
                    let mut pending = message;
                    loop {
                        match input_tx.try_send(pending) {
                            Ok(()) => break,
                            Err(crossbeam_channel::TrySendError::Full(message)) => {
                                pending = message;
                                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                            }
                            Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
                        }
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::debug!(%error, "LSP adapter input closed");
                    break;
                }
            }
        }
    });
    let (async_tx, mut async_rx) = tokio::sync::mpsc::channel(16);
    let output = tokio::task::spawn_blocking(move || {
        while let Ok(message) = output_rx.recv() {
            if async_tx.blocking_send(message).is_err() {
                break;
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(message) = async_rx.recv().await {
            write_lsp_message(&mut write, &message).await?;
        }
        write.shutdown().await?;
        anyhow::Ok(())
    });
    let mut worker = worker;
    let result = tokio::select! {
        result = &mut worker => result?,
        () = retired.cancelled() => {
            let _ = controls_out.send(LspControl::ScopeRetired { epoch, reason: "workspace configuration changed".into() }).await;
            reader.abort();
            worker.await?
        },
        () = stop.cancelled() => { reader.abort(); worker.await? },
    };
    reader.abort();
    output.await?;
    writer.await??;
    result
}

const LSP_HEADER_LIMIT: usize = 8192;

pub(crate) async fn read_lsp_message<R: AsyncBufRead + Unpin>(
    read: &mut R,
) -> Result<Option<lsp_server::Message>> {
    let mut length = None;
    let mut header_size = 0usize;
    loop {
        let mut line = String::new();
        // One byte past the limit is enough to tell an oversized header; reading the
        // rest of the line would buffer whatever a peer sends before the check.
        let remaining = (LSP_HEADER_LIMIT - header_size + 1) as u64;
        let count = (&mut *read).take(remaining).read_line(&mut line).await?;
        if count == 0 {
            anyhow::ensure!(header_size == 0, "truncated LSP header");
            return Ok(None);
        }
        header_size = header_size.checked_add(count).context("LSP header size overflow")?;
        anyhow::ensure!(header_size <= LSP_HEADER_LIMIT, "LSP header exceeds 8 KiB");
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').context("invalid LSP header")?;
        if name.eq_ignore_ascii_case("Content-Length") {
            anyhow::ensure!(length.is_none(), "duplicate LSP Content-Length");
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("LSP Content-Length missing")?;
    // The declared length is the peer's claim: buffer the body as it arrives, never
    // ahead of it.
    let mut bytes = Vec::new();
    let received = (&mut *read).take(u64::try_from(length)?).read_to_end(&mut bytes).await?;
    anyhow::ensure!(received == length, "truncated LSP body");
    let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
    // lsp-server represents both a missing result and JSON-RPC's valid `result: null`
    // as `None`. Restore the wire distinction before this response is forwarded to the
    // public stdio writer, which may bypass `write_lsp_message` entirely.
    let null_result = raw
        .as_object()
        .is_some_and(|object| object.get("result").is_some_and(serde_json::Value::is_null));
    // Moved, not cloned: a response carries a whole document's hints or diagnostics.
    let mut message: lsp_server::Message = serde_json::from_value(raw)?;
    if null_result {
        if let lsp_server::Message::Response(response) = &mut message {
            response.result = Some(serde_json::Value::Null);
        }
    }
    Ok(Some(message))
}

pub(crate) async fn write_lsp_message<W: AsyncWrite + Unpin>(
    write: &mut W,
    message: &lsp_server::Message,
) -> Result<()> {
    // A successful JSON-RPC response must carry `result`, even when its value is null.
    // lsp-server's typed Response cannot distinguish that from an absent result. Every
    // other message is written straight from its own tree, without a copy of it.
    let bytes = if matches!(message, lsp_server::Message::Response(response) if response.error.is_none() && response.result.is_none())
    {
        let mut value = serde_json::to_value(message)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("result".to_owned(), serde_json::Value::Null);
        }
        serde_json::to_vec(&value)?
    } else {
        serde_json::to_vec(message)?
    };
    write.write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes()).await?;
    write.write_all(&bytes).await?;
    write.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_only_connection_survives_the_session_context_round_trip() {
        let config: super::super::OnecConnectionsFile = serde_json::from_str(
            r#"{"connections":{"exec":{"check_executor":{"program":"/test/executor","args":["--a","b c"],"handles_snippet":true}}}}"#,
        )
        .unwrap();
        let mut native = NativeContext::default();
        native.add_named(config).unwrap();

        let bytes = serde_json::to_vec(&McpContext { native, ..McpContext::default() }).unwrap();
        let context: McpContext = serde_json::from_slice(&bytes).unwrap();
        let mut settings = mcp_server::SessionConfig::default();
        context.native.install(&mut settings, &ExecutorSlots::default()).unwrap();

        let named = &context.native.named["exec"];
        assert!(named.url.is_none(), "executor-only connection keeps no HTTP URL");
        let executor = named.check_executor.clone().expect("executor profile crosses the wire");
        let profile = executor.into_profile().unwrap();
        assert_eq!(profile.program, std::path::PathBuf::from("/test/executor"));
        assert_eq!(profile.args, ["--a", "b c"].map(std::ffi::OsString::from));
        assert!(profile.handles_snippet);
    }

    #[test]
    fn executor_semaphore_is_shared_by_sessions_of_one_connection() {
        let config = || -> super::super::OnecConnectionsFile {
            serde_json::from_str(
                r#"{"connections":{"exec":{"check_executor":{"program":"/x","max_concurrent":1,"handles_snippet":true}}}}"#,
            )
            .unwrap()
        };
        let mut native = NativeContext::default();
        native.add_named(config()).unwrap();
        let slots = ExecutorSlots::default();
        let executor = || native.named["exec"].check_executor.clone().unwrap();

        let first = slots.profile("exec", executor()).unwrap();
        let second = slots.profile("exec", executor()).unwrap();
        let other_connection = slots.profile("other", executor()).unwrap();

        assert!(Arc::ptr_eq(&first.semaphore, &second.semaphore));
        assert!(!Arc::ptr_eq(&first.semaphore, &other_connection.semaphore));
        let _held = first.semaphore.clone().try_acquire_owned().unwrap();
        assert_eq!(second.semaphore.available_permits(), 0, "the limit holds across sessions");
    }

    #[test]
    fn executor_semaphore_ignores_limits_other_than_max_concurrent() {
        let executor = |max_concurrent: usize,
                        timeout_ms: u64|
         -> super::super::CheckExecutorConfig {
            serde_json::from_str(&format!(
                r#"{{"program":"/x","max_concurrent":{max_concurrent},"timeout_ms":{timeout_ms},"handles_snippet":true}}"#
            ))
            .unwrap()
        };
        let slots = ExecutorSlots::default();
        let first = slots.profile("exec", executor(1, 120_000)).unwrap();
        let slower = slots.profile("exec", executor(1, 600_000)).unwrap();
        let wider = slots.profile("exec", executor(2, 120_000)).unwrap();

        assert!(Arc::ptr_eq(&first.semaphore, &slower.semaphore));
        assert!(!Arc::ptr_eq(&first.semaphore, &wider.semaphore));
    }

    #[test]
    fn named_connection_without_url_or_executor_is_rejected() {
        let config: super::super::OnecConnectionsFile =
            serde_json::from_str(r#"{"connections":{"empty":{}}}"#).unwrap();
        assert!(NativeContext::default().add_named(config).is_err());
    }

    #[test]
    fn shared_identity_changes_for_resolved_config_fields_outside_cache_scope() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("Configuration.xml"), "<Configuration/>").unwrap();
        let config_path = workspace.path().join("bsl-analyzer.toml");
        std::fs::write(&config_path, "[diagnostics.baseline]\npath = \"one.json\"\n").unwrap();
        let project = mcp_server::project::at(workspace.path()).unwrap();
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            None,
            &std::env::current_dir().unwrap(),
            None,
        )
        .unwrap();
        let first = identity(workspace.path(), &cache).unwrap();

        std::fs::write(&config_path, "[diagnostics.baseline]\npath = \"two.json\"\n").unwrap();
        let second = identity(workspace.path(), &cache).unwrap();

        assert_ne!(
            SharedBackendKey::new(first).digest(),
            SharedBackendKey::new(second).digest(),
            "effective project config must fork the daemon rendezvous key",
        );
    }

    /// A server started without the broker serves an invalid help selection as missing
    /// help; the broker path must not refuse to start over the same selection.
    #[test]
    fn an_invalid_help_selection_is_an_identity_not_a_failure() {
        let fingerprint = |request| {
            let mut hasher = blake3::Hasher::new();
            fingerprint_requested_platform_source(&mut hasher, request);
            hasher.finalize()
        };
        let invalid = fingerprint(Err("the value looks like a URL but was not parsed".to_owned()));

        assert_ne!(invalid, fingerprint(Ok(bsl_platform::PlatformHelpRequest::None)));
        assert_ne!(invalid, fingerprint(Err("another reason".to_owned())));
    }

    #[test]
    fn safe_config_identity_excludes_credentials_and_embedding_url_userinfo() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first = first_dir.path().join("bsl-analyzer.toml");
        let second = second_dir.path().join("bsl-analyzer.toml");
        std::fs::write(
            &first,
            "[search.baseline.embedding]\nurl = \"https://user:one@example.test/v1?api-version=1\"\n\
             [search.baseline.postgres.credential_helper]\nprogram = \"vault\"\nargs = [\"--token\", \"one\"]\n",
        )
        .unwrap();
        std::fs::write(
            &second,
            "[search.baseline.embedding]\nurl = \"https://other:two@example.test/v1?api-version=1#part\"\n\
             [search.baseline.postgres.credential_helper]\nprogram = \"vault\"\nargs = [\"--token\", \"two\"]\n",
        )
        .unwrap();
        let first = project_model::ProjectConfig::load_from_file(&first).unwrap();
        let second = project_model::ProjectConfig::load_from_file(&second).unwrap();

        assert_eq!(safe_config_fingerprint(&first), safe_config_fingerprint(&second));
    }

    #[test]
    fn embedding_url_query_forks_the_identity() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first = first_dir.path().join("bsl-analyzer.toml");
        let second = second_dir.path().join("bsl-analyzer.toml");
        std::fs::write(
            &first,
            "[search.baseline.embedding]\nurl = \"https://emb.example.test/v1/embed?api-version=2024-02-01\"\n",
        )
        .unwrap();
        std::fs::write(
            &second,
            "[search.baseline.embedding]\nurl = \"https://emb.example.test/v1/embed?api-version=2025-01-01\"\n",
        )
        .unwrap();
        let first = project_model::ProjectConfig::load_from_file(&first).unwrap();
        let second = project_model::ProjectConfig::load_from_file(&second).unwrap();

        assert_ne!(safe_config_fingerprint(&first), safe_config_fingerprint(&second));
    }

    #[test]
    fn platform_identity_tracks_external_manifest_changes_at_the_same_path() {
        let package = tempfile::tempdir().unwrap();
        let manifest = package.path().join("manifest.json");
        std::fs::write(&manifest, "{\"platform_version\":\"8.3.1\",\"sha256\":\"one\"}").unwrap();
        let request = bsl_platform::PlatformHelpRequest::ExternalPath(package.path().to_path_buf());
        let mut first = blake3::Hasher::new();
        fingerprint_platform_source(&mut first, &request);
        std::fs::write(&manifest, "{\"platform_version\":\"8.3.2\",\"sha256\":\"two\"}").unwrap();
        let mut second = blake3::Hasher::new();
        fingerprint_platform_source(&mut second, &request);

        assert_ne!(first.finalize(), second.finalize());
    }

    #[tokio::test]
    async fn framing_preserves_large_document_and_rejects_duplicate_lengths() {
        let expected = lsp_server::Message::Notification(lsp_server::Notification::new(
            "test".into(),
            "я".repeat(600_000),
        ));
        let (mut write, read) = tokio::io::duplex(4096);
        let send = tokio::spawn(async move {
            write_lsp_message(&mut write, &expected).await.unwrap();
        });
        let message =
            read_lsp_message(&mut tokio::io::BufReader::new(read)).await.unwrap().unwrap();
        assert!(
            matches!(message, lsp_server::Message::Notification(notification) if notification.params.as_str().unwrap().len() == 1_200_000)
        );
        send.await.unwrap();
        let bytes = b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}";
        assert!(read_lsp_message(&mut &bytes[..]).await.is_err());
    }

    #[tokio::test]
    async fn framing_stops_reading_a_header_line_at_the_header_limit() {
        let mut bytes = vec![b'A'; 1 << 20];
        bytes.extend_from_slice(b"\r\nContent-Length: 2\r\n\r\n{}");
        let mut unread = &bytes[..];
        assert!(read_lsp_message(&mut unread).await.is_err());
        let consumed = bytes.len() - unread.len();
        assert!(consumed <= 8193, "an oversized header line was read whole: {consumed} bytes");
    }

    /// The declared length is a peer's claim, not data: the body is buffered as it
    /// arrives. A length no allocation could satisfy, followed by a short body, is a
    /// truncated message rather than a failed reservation.
    #[tokio::test]
    async fn framing_buffers_a_body_only_as_it_arrives() {
        let bytes = format!("Content-Length: {}\r\n\r\n{{}}", usize::MAX);
        let error = read_lsp_message(&mut bytes.as_bytes()).await.unwrap_err();
        assert!(error.to_string().contains("truncated LSP body"), "{error:#}");
    }

    #[tokio::test]
    async fn framing_preserves_successful_null_results_in_both_directions() {
        fn framed_json(wire: &[u8]) -> serde_json::Value {
            let body_start = wire.windows(4).position(|window| window == b"\r\n\r\n").unwrap() + 4;
            serde_json::from_slice(&wire[body_start..]).unwrap()
        }

        let incoming = b"Content-Length: 39\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":10,\"result\":null}";
        let Some(lsp_server::Message::Response(response)) =
            read_lsp_message(&mut &incoming[..]).await.unwrap()
        else {
            panic!("a null result must remain a response");
        };
        assert_eq!(response.result, Some(serde_json::Value::Null));

        // The public writer is lsp-server's standard implementation; forwarding the typed
        // response through it must retain the explicit result property and JSON-RPC envelope.
        let mut public_wire = Vec::new();
        lsp_server::Message::Response(response).write(&mut public_wire).unwrap();
        let public_json = framed_json(&public_wire);
        assert!(public_json.get("result").is_some_and(serde_json::Value::is_null));
        assert_eq!(public_json["jsonrpc"], "2.0");

        let outgoing = lsp_server::Message::Response(lsp_server::Response {
            id: 11.into(),
            result: None,
            error: None,
        });
        let (mut writer, mut reader) = tokio::io::duplex(256);
        write_lsp_message(&mut writer, &outgoing).await.unwrap();
        drop(writer);
        let mut wire = Vec::new();
        reader.read_to_end(&mut wire).await.unwrap();
        let json = framed_json(&wire);
        assert!(json.get("result").is_some_and(serde_json::Value::is_null));
    }
}
