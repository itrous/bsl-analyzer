use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotification, CancelledNotificationParam,
    ClientRequest, PingRequest,
};
use rmcp::service::{PeerRequestOptions, RunningService, ServiceError};
use rmcp::{RoleClient, ServiceExt};
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    serve_stream_with_shutdown, McpProfile, McpServer, OnecConnection, SessionConfig, SharedState,
    ToolGate,
};

/// A hang watchdog for every wait in this file, not a latency budget.
const SHUTDOWN_TIMEOUT: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));

struct WorkspaceStreamCleanup {
    state: SharedState,
    shutdowns: [CancellationToken; 2],
}

impl Drop for WorkspaceStreamCleanup {
    fn drop(&mut self) {
        for shutdown in &self.shutdowns {
            shutdown.cancel();
        }
        self.state.shutdown();
    }
}

/// A completed MCP handshake exercises the service task that owns the transport.
async fn initialized_session(
    shutdown: &CancellationToken,
) -> (RunningService<RoleClient, ()>, JoinHandle<anyhow::Result<()>>) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server = McpServer::new(McpProfile::Reference, SharedState::shared());
    let task = tokio::spawn(serve_stream_with_shutdown(server, server_io, shutdown.clone()));
    let client = tokio::time::timeout(SHUTDOWN_TIMEOUT, ().serve(client_io))
        .await
        .expect("client initialization is bounded")
        .expect("client initializes");
    (client, task)
}

async fn initialized_workspace_session(
    state: SharedState,
    profile: McpProfile,
    shutdown: &CancellationToken,
) -> (RunningService<RoleClient, ()>, JoinHandle<anyhow::Result<()>>) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server = McpServer::new(profile, state);
    let task = tokio::spawn(serve_stream_with_shutdown(server, server_io, shutdown.clone()));
    let client = tokio::time::timeout(SHUTDOWN_TIMEOUT, ().serve(client_io))
        .await
        .expect("workspace client initialization is bounded")
        .expect("workspace client initializes");
    (client, task)
}

async fn initialized_server_session(
    server: McpServer,
    shutdown: &CancellationToken,
) -> (RunningService<RoleClient, ()>, JoinHandle<anyhow::Result<()>>) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(serve_stream_with_shutdown(server, server_io, shutdown.clone()));
    let client = tokio::time::timeout(SHUTDOWN_TIMEOUT, ().serve(client_io))
        .await
        .expect("client initialization is bounded")
        .expect("client initializes");
    (client, task)
}

/// A stalled handshake or detached service task must not make shutdown wait forever.
async fn session_finished(task: JoinHandle<anyhow::Result<()>>) {
    tokio::time::timeout(SHUTDOWN_TIMEOUT, task)
        .await
        .expect("session finishes within the shutdown budget")
        .expect("session task joins")
        .expect("session closes cleanly");
}

#[derive(Clone)]
struct MockNativeContext {
    label: &'static str,
    requests: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>,
}

struct MockNativeService {
    url: String,
    requests: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>,
    task: JoinHandle<()>,
}

async fn mock_event_log(
    State(context): State<MockNativeContext>,
    headers: HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    context.requests.lock().unwrap().push((headers, request));
    Json(serde_json::json!({
        "columns": ["context"],
        "rows": [[context.label]],
        "total": 1,
        "truncated": false
    }))
}

async fn mock_native_service(label: &'static str) -> MockNativeService {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = MockNativeContext { label, requests: Arc::clone(&requests) };
    let app = Router::new().route("/event-log", post(mock_event_log)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MockNativeService { url, requests, task }
}

/// One disconnected client must not cancel its siblings; broker shutdown must close them.
#[tokio::test]
async fn stream_shutdown_closes_transport_without_cancelling_sibling_sessions() {
    let shutdown = CancellationToken::new();
    let (first_client, first_task) = initialized_session(&shutdown).await;
    let (second_client, second_task) = initialized_session(&shutdown).await;

    tokio::time::timeout(SHUTDOWN_TIMEOUT, first_client.cancel())
        .await
        .expect("first client disconnect is bounded")
        .expect("first client disconnects");
    session_finished(first_task).await;
    let request = ClientRequest::PingRequest(PingRequest::default());
    tokio::time::timeout(SHUTDOWN_TIMEOUT, second_client.send_request(request.clone()))
        .await
        .expect("sibling request is bounded")
        .expect("sibling remains connected");

    shutdown.cancel();
    session_finished(second_task).await;
    tokio::time::timeout(SHUTDOWN_TIMEOUT, second_client.send_request(request))
        .await
        .expect("closed transport answers without hanging")
        .expect_err("no request is served after session shutdown completes");
}

/// Session routers may share one backend while keeping launch tools and native clients local.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_backend_sessions_keep_tool_gates_and_native_dispatch_local() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Configuration.xml"), "<Configuration/>").unwrap();
    let cache = crate::WorkspaceCacheLayout::from_root(workspace.path().join("cache"));
    let state = SharedState::workspace_with_cache(workspace.path().to_path_buf(), cache)
        .expect("valid workspace owner");
    let backend = McpServer::new(McpProfile::Workspace, state);
    let native_a = mock_native_service("native-a").await;
    let native_b = mock_native_service("native-b").await;

    let mut config_a = SessionConfig::default();
    config_a.enable_tool("references");
    config_a.add_onec_connection(
        "current".to_owned(),
        OnecConnection::new(onec_client::Client::new(&native_a.url, "", ""), false),
    );
    let mut config_b = SessionConfig::default();
    config_b.add_onec_connection(
        "current".to_owned(),
        OnecConnection::new(onec_client::Client::new(&native_b.url, "", ""), false),
    );
    let first_server = backend.for_session(config_a);
    let second_server = backend.for_session(config_b);

    assert!(
        Arc::ptr_eq(
            first_server.shared_state().search_engine(),
            second_server.shared_state().search_engine(),
        ),
        "both session routers retain the shared workspace backend"
    );

    let shutdown = CancellationToken::new();
    let (first, first_task) = initialized_server_session(first_server, &shutdown).await;
    let (second, second_task) = initialized_server_session(second_server, &shutdown).await;
    let first_names = first.list_all_tools().await.expect("first tools/list");
    let second_names = second.list_all_tools().await.expect("second tools/list");
    assert!(first_names.iter().any(|tool| tool.name == "references"));
    assert!(!second_names.iter().any(|tool| tool.name == "references"));

    let visible_call = first
        .call_tool(
            CallToolRequestParams::new("references")
                .with_arguments(serde_json::json!({}).as_object().unwrap().clone()),
        )
        .await
        .expect_err("the visible route validates its missing reference anchor");
    let gated = second
        .call_tool(
            CallToolRequestParams::new("references")
                .with_arguments(serde_json::json!({}).as_object().unwrap().clone()),
        )
        .await
        .expect_err("a session without the opt-in cannot dispatch references");
    let unknown = second
        .call_tool(
            CallToolRequestParams::new("not_a_tool")
                .with_arguments(serde_json::json!({}).as_object().unwrap().clone()),
        )
        .await
        .expect_err("the control tool is unknown");
    assert_ne!(format!("{visible_call}"), format!("{unknown}"));
    assert_eq!(format!("{gated}"), format!("{unknown}"));

    for (client, label) in [(&first, "native-a"), (&second, "native-b")] {
        let response = client
            .call_tool(
                CallToolRequestParams::new("event_log").with_arguments(
                    serde_json::json!({"connection": "current", "limit": 1})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("session-local native dispatch succeeds");
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(encoded.contains(label), "router reached its own native context: {encoded}");
    }
    assert_eq!(native_a.requests.lock().unwrap().len(), 1);
    assert_eq!(native_b.requests.lock().unwrap().len(), 1);

    tokio::time::timeout(SHUTDOWN_TIMEOUT, first.cancel())
        .await
        .expect("first client disconnect is bounded")
        .expect("first client disconnects");
    tokio::time::timeout(SHUTDOWN_TIMEOUT, second.cancel())
        .await
        .expect("second client disconnect is bounded")
        .expect("second client disconnects");
    session_finished(first_task).await;
    session_finished(second_task).await;
    backend.shutdown();
    native_a.task.abort();
    native_b.task.abort();
}

/// A silent connection must release its socket even if it never initializes MCP.
#[tokio::test]
async fn stream_shutdown_closes_uninitialized_transport() {
    let shutdown = CancellationToken::new();
    let (mut client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server = McpServer::new(McpProfile::Reference, SharedState::shared());
    let task = tokio::spawn(serve_stream_with_shutdown(server, server_io, shutdown.clone()));
    tokio::task::yield_now().await;

    shutdown.cancel();
    session_finished(task).await;
    let mut byte = [0];
    let read = tokio::time::timeout(SHUTDOWN_TIMEOUT, client_io.read(&mut byte))
        .await
        .expect("silent transport closes without hanging")
        .expect("read observes transport closure");
    assert_eq!(read, 0, "the silent peer receives EOF");
}

/// A cache-scope failure closes every session of that backend without relying on the daemon's
/// ordinary shutdown token (which remains available for the supersession drain protocol).
#[tokio::test]
async fn workspace_cache_scope_stops_all_session_transports() {
    let daemon_shutdown = CancellationToken::new();
    let state = SharedState::shared();
    let server = McpServer::new(McpProfile::Reference, state.clone());
    let first = server.clone();
    let second = server;
    let (first_client, first_task) = {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let task =
            tokio::spawn(serve_stream_with_shutdown(first, server_io, daemon_shutdown.clone()));
        (
            tokio::time::timeout(SHUTDOWN_TIMEOUT, ().serve(client_io))
                .await
                .expect("first client initialization is bounded")
                .expect("first client initializes"),
            task,
        )
    };
    let (second_client, second_task) = {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let task =
            tokio::spawn(serve_stream_with_shutdown(second, server_io, daemon_shutdown.clone()));
        (
            tokio::time::timeout(SHUTDOWN_TIMEOUT, ().serve(client_io))
                .await
                .expect("second client initialization is bounded")
                .expect("second client initializes"),
            task,
        )
    };

    state.scope_transport_stop().cancel();
    session_finished(first_task).await;
    session_finished(second_task).await;
    assert!(
        tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            first_client.send_request(ClientRequest::PingRequest(PingRequest::default()))
        )
        .await
        .expect("closed first client answers within the hang guard")
        .is_err(),
        "scope retirement closes the first session"
    );
    assert!(
        tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            second_client.send_request(ClientRequest::PingRequest(PingRequest::default()))
        )
        .await
        .expect("closed second client answers within the hang guard")
        .is_err(),
        "scope retirement closes the second session"
    );
    assert!(!daemon_shutdown.is_cancelled(), "scope retirement leaves broker shutdown distinct");
}

/// The warning is emitted to tracing during one successful workspace snapshot boot.
/// A real framed MCP session can then make repeated tool requests without receiving
/// that warning as protocol content or triggering per-request duplicates.
#[tokio::test]
async fn legacy_workspace_warning_stays_out_of_repeated_mcp_tool_responses() {
    let workspace = tempfile::tempdir().unwrap();
    let main = workspace.path().join("src/cf");
    let extension = workspace.path().join("ext");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::create_dir_all(&extension).unwrap();
    std::fs::write(main.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(
        extension.join("Configuration.xml"),
        "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("bsl-analyzer.toml"),
        "[source]\nroot = \"src/cf\"\nextensions = [{ name = \"Feature\", path = \"ext\" }]\n",
    )
    .unwrap();
    let cache = crate::cache::WorkspaceCacheLayout::from_root(workspace.path().join("cache"));
    let state = SharedState::workspace_with_cache(workspace.path().to_path_buf(), cache)
        .expect("legacy extension project bootstraps");

    let shutdown = CancellationToken::new();
    let (client, task) =
        initialized_workspace_session(state.clone(), McpProfile::Workspace, &shutdown).await;
    let tools = client.list_all_tools().await.expect("tools/list is framed and answered");
    assert!(tools.iter().any(|tool| tool.name == "diagnostics"));
    for _ in 0..2 {
        let response = client
            .call_tool(CallToolRequestParams::new("diagnostics").with_arguments(
                serde_json::json!({"action": "schema"}).as_object().unwrap().clone(),
            ))
            .await
            .expect("tools/call is framed and answered");
        let wire_result = serde_json::to_string(&response).unwrap();
        assert!(wire_result.contains("schema"), "tool response has its schema: {response:?}");
        assert!(
            !wire_result.contains("legacy extension scope")
                && !wire_result.contains("DataSeparation"),
            "the boot advisory remains in tracing, outside MCP response payloads"
        );
    }

    tokio::time::timeout(SHUTDOWN_TIMEOUT, client.cancel())
        .await
        .expect("client disconnect is bounded")
        .expect("client disconnects");
    session_finished(task).await;
    state.shutdown();
}

/// A scope-only edit retires the old frozen workspace over the live stream; a client
/// reconnects against the newly resolved cache namespace and can issue a real tool call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_scope_change_retires_mcp_stream_and_reconnects_with_explicit_cache() {
    let workspace = tempfile::tempdir().unwrap();
    let cache_parent = tempfile::tempdir().unwrap();
    let root = workspace.path().to_path_buf();
    let main = root.join("src/cf");
    let extension = root.join("ext");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::create_dir_all(&extension).unwrap();
    std::fs::write(main.join("Configuration.xml"), "<Configuration/>").unwrap();
    std::fs::write(
        extension.join("Configuration.xml"),
        "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
    )
    .unwrap();
    let project_file = root.join("bsl-analyzer.toml");
    let config_for_scope = |scope: Option<&str>| {
        let scope = scope.map_or_else(String::new, |scope| format!(", scope = \"{scope}\""));
        format!(
            "[source]\nroot = \"src/cf\"\nextensions = [{{ name = \"Feature\", path = \"ext\"{scope} }}]\n"
        )
    };
    std::fs::write(&project_file, config_for_scope(None)).unwrap();

    let old_project = crate::project::at(&root).expect("legacy project resolves");
    let cache_base = cache_parent.path().join("cache");
    let old_cache = crate::cache::WorkspaceCacheLayout::for_project(
        &old_project,
        Some(&cache_base),
        cache_parent.path(),
        None,
    )
    .expect("legacy cache scope resolves");
    let old_layout = old_cache.root().to_path_buf();
    let old_state = SharedState::workspace_with_cache(root.clone(), old_cache)
        .expect("legacy workspace boots under its frozen cache scope");
    let old_stop = old_state.scope_transport_stop();
    let old_shutdown = CancellationToken::new();
    let (old_client, old_task) =
        initialized_workspace_session(old_state.clone(), McpProfile::Workspace, &old_shutdown)
            .await;
    let before =
        old_client
            .call_tool(CallToolRequestParams::new("diagnostics").with_arguments(
                serde_json::json!({"action": "schema"}).as_object().unwrap().clone(),
            ))
            .await
            .expect("old scope serves a real tool call");
    assert!(serde_json::to_string(&before).unwrap().contains("schema"));

    std::fs::write(&project_file, config_for_scope(Some("InfoBase"))).unwrap();
    tokio::time::timeout(SHUTDOWN_TIMEOUT, old_stop.cancelled())
        .await
        .expect("scope-only project drift retires the old transport");
    session_finished(old_task).await;
    assert!(
        tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            old_client.send_request(ClientRequest::PingRequest(PingRequest::default()))
        )
        .await
        .expect("retired client request is bounded")
        .is_err(),
        "the frozen legacy session does not answer after retirement"
    );
    old_state.shutdown();

    let explicit_project = crate::project::at(&root).expect("explicit project resolves");
    let explicit_cache = crate::cache::WorkspaceCacheLayout::for_project(
        &explicit_project,
        Some(&cache_base),
        cache_parent.path(),
        None,
    )
    .expect("InfoBase cache scope resolves");
    assert_ne!(explicit_cache.root(), old_layout, "scope changes the frozen cache layout");
    let explicit_state = SharedState::workspace_with_cache(root, explicit_cache)
        .expect("new workspace boots with the explicit cache scope");
    let explicit_shutdown = CancellationToken::new();
    let (explicit_client, explicit_task) = initialized_workspace_session(
        explicit_state.clone(),
        McpProfile::Workspace,
        &explicit_shutdown,
    )
    .await;
    let after =
        explicit_client
            .call_tool(CallToolRequestParams::new("diagnostics").with_arguments(
                serde_json::json!({"action": "schema"}).as_object().unwrap().clone(),
            ))
            .await
            .expect("reconnected explicit scope serves a real tool call");
    assert!(serde_json::to_string(&after).unwrap().contains("schema"));
    tokio::time::timeout(SHUTDOWN_TIMEOUT, explicit_client.cancel())
        .await
        .expect("explicit-scope client disconnect is bounded")
        .expect("explicit-scope client disconnects");
    session_finished(explicit_task).await;
    explicit_state.shutdown();
}

/// Static Workspace-profile tools answer while the graph's first publication is held and
/// without starting the resident diagnostics lifecycle. Background startup owners may still
/// run; this checks that these requests do not claim or publish the graph themselves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_catalog_schema_and_platform_calls_do_not_build_resident_or_graph() {
    let workspace = tempfile::tempdir().expect("owned workspace");
    let cache_parent = tempfile::tempdir().expect("owned cache parent");
    std::fs::write(workspace.path().join("Configuration.xml"), "<Configuration/>")
        .expect("minimal project configuration");

    let project = crate::project::at(workspace.path()).expect("minimal workspace resolves");
    let cache = crate::cache::WorkspaceCacheLayout::for_project(
        &project,
        Some(&cache_parent.path().join("cache")),
        workspace.path(),
        None,
    )
    .expect("owned workspace cache resolves");
    let (state, held) = crate::graph::test_support::holding_the_first_build(|| {
        SharedState::workspace_with_cache(workspace.path().to_path_buf(), cache)
            .expect("workspace state initializes")
    });
    assert!(held.holds_unconsulted(), "first graph publication is held at startup");

    let assert_lazy_state = || {
        let status = state.diagnostics().status_report();
        assert_eq!(status.state, "idle", "static requests must not start resident loading");
        assert_eq!(status.generation, 0, "static requests must not publish a resident generation");
        assert!(held.holds_unconsulted(), "a static request claimed or published the graph");
    };
    assert_lazy_state();

    let shutdown = CancellationToken::new();
    let (client, task) = tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        initialized_workspace_session(state.clone(), McpProfile::Workspace, &shutdown),
    )
    .await
    .expect("workspace MCP handshake finishes within the shutdown budget");
    assert_lazy_state();

    let invalid_action = tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("diagnostics").with_arguments(
            serde_json::json!({"action": "not_a_diagnostics_action"}).as_object().unwrap().clone(),
        )),
    )
    .await
    .expect("unknown-action response finishes within the shutdown budget");
    match invalid_action {
        Err(rmcp::service::ServiceError::McpError(error)) => {
            assert_eq!(error.code.0, -32602, "unknown action is invalid params");
            assert!(
                error.message.contains("Unknown action 'not_a_diagnostics_action'"),
                "the error identifies the submitted action: {error:?}"
            );
        }
        Err(other) => panic!("unknown diagnostics action returned a non-MCP error: {other:?}"),
        Ok(response) => panic!("unknown diagnostics action unexpectedly answered: {response:?}"),
    }
    assert_lazy_state();

    let schema =
        tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            client.call_tool(CallToolRequestParams::new("diagnostics").with_arguments(
                serde_json::json!({"action": "schema"}).as_object().unwrap().clone(),
            )),
        )
        .await
        .expect("diagnostics schema finishes within the shutdown budget")
        .expect("diagnostics schema answers over rmcp");
    let schema = schema.structured_content.expect("schema has structured content");
    assert_eq!(schema["schema_version"], "16");
    assert_eq!(
        schema["actions"],
        serde_json::json!(["catalog", "schema", "status", "file", "workspace"])
    );
    assert_lazy_state();

    let catalog = tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        client.call_tool(
            CallToolRequestParams::new("diagnostics").with_arguments(
                serde_json::json!({
                    "action": "catalog",
                    "codes": ["UnresolvedBareCall"],
                    "locale": "ru"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        ),
    )
    .await
    .expect("diagnostics catalog finishes within the shutdown budget")
    .expect("diagnostics catalog answers over rmcp");
    let catalog = catalog.structured_content.expect("catalog has structured content");
    assert_eq!(catalog["action"], "catalog");
    assert_eq!(catalog["count"], 1);
    assert_eq!(catalog["entries"][0]["code"], "UnresolvedBareCall");
    assert_lazy_state();

    let platform = tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        client
            .call_tool(CallToolRequestParams::new("syntax_help").with_arguments(
                serde_json::json!({"name": "Массив"}).as_object().unwrap().clone(),
            )),
    )
    .await
    .expect("platform syntax card finishes within the shutdown budget")
    .expect("platform syntax card answers over rmcp");
    let platform = platform.structured_content.expect("platform card has structured content");
    assert_eq!(platform["schema_version"], "2");
    assert_eq!(platform["kind"], "type");
    assert_eq!(platform["name"], "Массив");
    assert_eq!(platform["english_name"], "Array");
    assert!(platform["constructors"].is_array());
    let methods = platform["methods"].as_array().expect("type card lists methods");
    assert!(!methods.is_empty(), "the Array type has compiled methods");
    assert!(
        methods
            .iter()
            .any(|method| method["name"] == "Количество" && method["english_name"] == "Count"),
        "the Array card includes its known Count method: {methods:?}"
    );
    assert_lazy_state();

    tokio::time::timeout(SHUTDOWN_TIMEOUT, client.cancel())
        .await
        .expect("client disconnect is bounded")
        .expect("client disconnects");
    session_finished(task).await;
    state.shutdown();
}

/// A real wire cancellation stops a references walk at its next Salsa checkpoint, releases
/// the resident read slot, and lets the next request publish diagnostics for a disk edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_references_cancel_unwinds_first_walk_span_before_a_writer() {
    let _walk_serial = crate::walk_probe::WALK_GATE.lock().await;
    let workspace = tempfile::tempdir().expect("owned workspace");
    let cache_parent = tempfile::tempdir().expect("owned cache parent");
    let root = workspace.path();
    std::fs::write(root.join("Configuration.xml"), "<Configuration/>")
        .expect("configuration fixture");
    std::fs::write(
        root.join("bsl-analyzer.toml"),
        "[source]\nroot = \".\"\n[diagnostics.parameters]\nUnresolvedBareCall = true\n",
    )
    .expect("enable the semantic writer assertion");

    crate::diagnostics_state::test_support::write_common_module(
        root,
        "Сервер",
        true,
        "&НаСервере\nФункция Считать() Экспорт\n    Возврат 1;\nКонецФункции\n",
    );
    let mut caller = String::from("&НаСервере\nПроцедура Вызвать()\n");
    for _ in 0..1000 {
        caller.push_str("    Сервер.Считать();\n");
    }
    caller.push_str("КонецПроцедуры\n");
    crate::diagnostics_state::test_support::write_common_module(root, "Caller0000", true, &caller);

    let project = crate::project::at(root).expect("generated project resolves");
    let cache = crate::cache::WorkspaceCacheLayout::for_project(
        &project,
        Some(&cache_parent.path().join("cache")),
        cache_parent.path(),
        None,
    )
    .expect("owned cache layout resolves");
    let state = SharedState::workspace_with_cache(root.to_path_buf(), cache)
        .expect("workspace resident initializes");
    let first_shutdown = CancellationToken::new();
    let sibling_shutdown = CancellationToken::new();
    let _cleanup = WorkspaceStreamCleanup {
        state: state.clone(),
        shutdowns: [first_shutdown.clone(), sibling_shutdown.clone()],
    };
    state.diagnostics().ensure_loading();
    let diagnostics_state = state.diagnostics().clone();
    tokio::task::spawn_blocking(move || {
        crate::diagnostics_state::test_support::wait_ready(&diagnostics_state)
    })
    .await
    .expect("resident readiness waiter joins");

    let enabled = vec!["references".to_owned()];
    let gate = ToolGate::for_launch(McpProfile::Workspace, &enabled);
    let (client, first_task) = tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        initialized_server_session(
            McpServer::with_gate(McpProfile::Workspace, state.clone(), &gate),
            &first_shutdown,
        ),
    )
    .await
    .expect("first workspace handshake is bounded");
    let (sibling, sibling_task) = tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        initialized_server_session(
            McpServer::with_gate(McpProfile::Workspace, state.clone(), &gate),
            &sibling_shutdown,
        ),
    )
    .await
    .expect("sibling workspace handshake is bounded");

    crate::walk_probe::install();
    crate::walk_probe::reset();
    let reference_arguments = || {
        serde_json::json!({
            "symbol": "Сервер.Считать",
            "kinds": ["call"],
            "limit": 1,
            "max_files": 10000
        })
        .as_object()
        .expect("reference arguments are an object")
        .clone()
    };
    let positive = tokio::time::timeout(
        test_utils::wait::hang_guard(Duration::from_secs(60)),
        client.call_tool(
            CallToolRequestParams::new("references").with_arguments(reference_arguments()),
        ),
    )
    .await
    .expect("positive full reference walk is bounded")
    .expect("positive full reference walk answers");
    let positive = positive.structured_content.expect("references are structured");
    assert_eq!(positive["outcome"], "resolved");
    assert_eq!(positive["total"], 1000, "all 1000 caller references are present");
    assert_eq!(
        crate::walk_probe::entered(),
        2,
        "the full walk enters the declaration and the 1000-call caller module"
    );

    crate::walk_probe::reset();
    let (span_hold, span_waiter) = crate::walk_probe::hold_first_span();
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(
        CallToolRequestParams::new("references").with_arguments(reference_arguments()),
    ));
    let handle = tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        client.peer().send_cancellable_request(request, PeerRequestOptions::no_options()),
    )
    .await
    .expect("cancellable request is sent within the bound")
    .expect("cancellable request reaches the peer");
    let waiter = span_waiter.clone();
    assert!(
        tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            tokio::task::spawn_blocking(move || waiter.wait_entered(SHUTDOWN_TIMEOUT)),
        )
        .await
        .expect("span waiter is bounded")
        .expect("span waiter joins"),
        "the real request enters its first find_references_in_file span"
    );
    assert_eq!(crate::walk_probe::entered(), 1, "only the held first span has started");

    tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        sibling.send_request(ClientRequest::PingRequest(PingRequest::default())),
    )
    .await
    .expect("sibling Ping remains responsive during the held walk")
    .expect("sibling session remains connected");
    let cancel = CancelledNotification::new(CancelledNotificationParam::new(
        Some(handle.id.clone()),
        Some("test cancels held reference walk".to_owned()),
    ));
    tokio::time::timeout(SHUTDOWN_TIMEOUT, client.peer().send_notification(cancel.into()))
        .await
        .expect("cancellation notification is bounded")
        .expect("cancellation notification is delivered");
    let cancelled = tokio::time::timeout(SHUTDOWN_TIMEOUT, handle.await_response())
        .await
        .expect("cancelled request response is bounded");
    assert!(
        matches!(&cancelled, Err(ServiceError::Cancelled { .. })),
        "the RMCP request receives its cancellation outcome: {cancelled:?}"
    );
    tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        client.send_request(ClientRequest::PingRequest(PingRequest::default())),
    )
    .await
    .expect("same-client Ping after cancellation is bounded")
    .expect("same client can issue the ordered next request");

    let edited_path = Path::new("CommonModules/Caller0000/Ext/Module.bsl");
    crate::diagnostics_state::test_support::write(
        root,
        edited_path.to_str().unwrap(),
        "&НаСервере\nПроцедура Вызвать()\n    Сервер.Считать();\n    НесуществующаяПроцедура();\nКонецПроцедуры\n",
    );
    span_hold.release();

    let deadline = Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(30));
    let mut diagnostic = serde_json::Value::Null;
    while Instant::now() < deadline {
        let response = tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            client.call_tool(
                CallToolRequestParams::new("diagnostics").with_arguments(
                    serde_json::json!({
                        "action": "file",
                        "path": edited_path.to_string_lossy()
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            ),
        )
        .await
        .expect("post-cancel diagnostic query is bounded")
        .expect("post-cancel diagnostic query answers after the resident writer");
        diagnostic = response.structured_content.expect("diagnostic is structured");
        let has_updated_finding =
            diagnostic["result"]["findings"].as_array().is_some_and(|findings| {
                findings.iter().any(|finding| finding["code"] == "UnresolvedBareCall")
            });
        if has_updated_finding {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let findings = diagnostic["result"]["findings"].as_array().expect("findings are present");
    assert!(
        findings.iter().any(|finding| finding["code"] == "UnresolvedBareCall"),
        "the writer applied the changed BSL body after the cancelled read released its slot: {diagnostic}"
    );
    assert_eq!(crate::walk_probe::entered(), 1, "the cancelled walk stopped at its first file");

    tokio::time::timeout(SHUTDOWN_TIMEOUT, client.cancel())
        .await
        .expect("first client disconnect is bounded")
        .expect("first client disconnects");
    session_finished(first_task).await;
    tokio::time::timeout(SHUTDOWN_TIMEOUT, sibling.cancel())
        .await
        .expect("sibling client disconnect is bounded")
        .expect("sibling client disconnects");
    session_finished(sibling_task).await;
    state.shutdown();
}
