//! End-to-end contract for the Streamable HTTP transport.
//!
//! The server is deliberately built before the transport starts: the CLI owns
//! validation, project locking, listener binding, and the one final
//! `McpServer::shutdown` call. This suite verifies only the transport boundary.

use std::{
    net::SocketAddr,
    sync::{Mutex, Once},
    time::Duration,
};

use mcp_server::{serve_http, McpProfile, McpServer, SharedState, MAX_HTTP_REQUEST_BODY_BYTES};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HOST, ORIGIN};
use rmcp::{
    model::CallToolRequestParams, service::RunningService,
    transport::StreamableHttpClientTransport, RoleClient, ServiceExt,
};
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const TEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Comfortably above the transport's own shutdown grace, so this fails on a server
/// that never stops rather than on one that merely waits out its grace.
const SHUTDOWN_TEST_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_ACCEPT: &str = "application/json, text/event-stream";

type Client = RunningService<RoleClient, ()>;

struct TestServer {
    address: SocketAddr,
    cancellation: CancellationToken,
    task: JoinHandle<anyhow::Result<()>>,
}

static ISOLATE_CACHE: Once = Once::new();

/// Point the reference profile's search database at a scratch directory. It otherwise
/// resolves to the developer's own cache, so the suite would read and write the real
/// `reference-search.db` while claiming to test only the transport. Every test runs
/// this before constructing any state, and `Once` orders the write ahead of each
/// caller's return, so no reader observes the environment mid-change.
///
/// Best effort, and only that: `dirs::cache_dir` reads `XDG_CACHE_HOME` on Linux and
/// `$HOME` on macOS, but on Windows it calls a known-folder API that no environment
/// variable redirects. What the reference profile writes there is derived from
/// compile-time platform tables rather than from any test input, so on Windows the
/// suite reproduces the same content a real run would write.
fn isolate_reference_cache() {
    ISOLATE_CACHE.call_once(|| {
        let scratch = std::env::temp_dir().join(format!("bsl-http-tests-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).expect("scratch cache directory should be creatable");
        std::env::set_var("XDG_CACHE_HOME", &scratch);
        if cfg!(target_os = "macos") {
            std::env::set_var("HOME", &scratch);
        }
    });
}

impl TestServer {
    async fn start(allowed_hosts: Vec<String>) -> Self {
        isolate_reference_cache();
        Self::start_with_state(McpProfile::Reference, SharedState::reference(None), allowed_hosts)
            .await
    }

    async fn start_with_state(
        profile: McpProfile,
        state: SharedState,
        allowed_hosts: Vec<String>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test listener should bind");
        let address = listener.local_addr().expect("bound listener has an address");
        let cancellation = CancellationToken::new();
        let server = McpServer::new(profile, state);

        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            serve_http(listener, server, profile, address, allowed_hosts, task_cancellation).await
        });

        Self { address, cancellation, task }
    }

    fn mcp_url(&self) -> String {
        format!("http://{}/mcp", self.address)
    }

    fn health_url(&self) -> String {
        format!("http://{}/health", self.address)
    }

    async fn connect(&self) -> Client {
        let transport = StreamableHttpClientTransport::from_uri(self.mcp_url());
        tokio::time::timeout(TEST_TIMEOUT, ().serve(transport))
            .await
            .expect("HTTP initialize should not hang")
            .expect("HTTP client should initialize")
    }

    async fn stop(self) {
        self.cancellation.cancel();
        tokio::time::timeout(TEST_TIMEOUT, self.task)
            .await
            .expect("HTTP graceful shutdown should not hang")
            .expect("HTTP task should not panic")
            .expect("HTTP server should stop cleanly");
    }
}

fn loopback_allowed_hosts() -> Vec<String> {
    vec!["127.0.0.1".to_owned(), "localhost".to_owned()]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_initializes_lists_tools_and_calls_a_safe_tool() {
    let server = TestServer::start(Vec::new()).await;
    let client = server.connect().await;

    assert!(client.peer_info().is_some(), "initialize should return server information");

    let tools = tokio::time::timeout(TEST_TIMEOUT, client.list_tools(Default::default()))
        .await
        .expect("tools/list should not hang")
        .expect("tools/list should succeed");
    assert!(
        tools.tools.iter().any(|tool| tool.name == "search"),
        "reference profile should publish the search tool"
    );

    let mut arguments = serde_json::Map::new();
    arguments.insert("action".to_owned(), Value::String("status".to_owned()));
    tokio::time::timeout(
        TEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("search").with_arguments(arguments)),
    )
    .await
    .expect("safe tools/call should not hang")
    .expect("safe tools/call should succeed");

    client.cancel().await.expect("client session should close");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_http_preserves_the_metadata_meta_type_contract() {
    let project = tempfile::tempdir().expect("temporary workspace should be created");
    let state = SharedState::workspace(project.path().to_path_buf())
        .expect("temporary workspace should initialize");
    let server =
        TestServer::start_with_state(McpProfile::Workspace, state, loopback_allowed_hosts()).await;
    let client = server.connect().await;

    let tools = tokio::time::timeout(TEST_TIMEOUT, client.list_tools(Default::default()))
        .await
        .expect("tools/list should not hang")
        .expect("tools/list should succeed");
    let metadata = tools
        .tools
        .iter()
        .find(|tool| tool.name == "metadata")
        .expect("workspace profile should publish the metadata tool");
    assert!(
        metadata
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key("meta_type")),
        "HTTP tools/list must preserve the metadata.meta_type input contract"
    );
    assert!(
        metadata.output_schema.is_none(),
        "metadata has action-specific shapes and must not publish a tool-wide outputSchema"
    );

    client.cancel().await.expect("client session should close");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_use_the_same_ready_server() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let first = server.connect().await;
    let second = server.connect().await;

    let (first_tools, second_tools) =
        tokio::join!(first.list_tools(Default::default()), second.list_tools(Default::default()));
    assert!(first_tools.expect("first client should list tools").tools.len() >= 3);
    assert!(second_tools.expect("second client should list tools").tools.len() >= 3);

    first.cancel().await.expect("first session should close independently");
    assert!(
        second.list_tools(Default::default()).await.is_ok(),
        "closing one session must not stop the shared server"
    );
    second.cancel().await.expect("second session should close");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_actual_listener_without_sensitive_state() {
    let server = TestServer::start(loopback_allowed_hosts()).await;

    let response =
        reqwest::get(server.health_url()).await.expect("health request should reach server");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("health should be JSON");

    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["profile"], "reference");
    assert_eq!(body["mode"], "http");
    assert_eq!(body["host"], server.address.ip().to_string());
    assert_eq!(body["port"], u64::from(server.address.port()));
    assert_eq!(body["pid"], u64::from(std::process::id()));
    assert!(body["uptime_seconds"].is_number());
    for forbidden in
        ["source_dir", "workspace_root", "onec_url", "onec_user", "onec_password", "password"]
    {
        assert!(body.get(forbidden).is_none(), "health leaked forbidden field {forbidden}");
    }

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disallowed_host_is_rejected_before_mcp_dispatch() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let response = reqwest::Client::new()
        .post(server.mcp_url())
        .header(HOST, "attacker.example")
        .header(ACCEPT, MCP_ACCEPT)
        .header(CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .expect("request should receive an HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    server.stop().await;
}

/// A browser `Origin` neither opens nor closes the door: the gate is the `Host`.
///
/// The transport can validate `Origin` too, but only against an allowlist we deliberately
/// never populate — leaving that list empty is what keeps a non-browser client, which sends
/// no `Origin` at all, from being refused. The pair below pins both halves of that decision:
/// presenting an `Origin` must not get a request past the `Host` gate, and must not get it
/// refused either. Without the first half the gate would pass on a build that stopped
/// checking `Host`; without the second, on a build that started refusing every `Origin`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_origin_header_changes_nothing_on_either_side_of_the_host_gate() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let client = reqwest::Client::new();

    // An `initialize`, not a `ping`: a request that needs a session the test has not opened
    // is turned away by the transport before anything looks at `Origin`, and then "not
    // refused" would hold no matter what the Origin handling did. This one is served, so
    // the assertion below is about the answer rather than about the absence of one status.
    let allowed = client
        .post(server.mcp_url())
        .header(HOST, "127.0.0.1")
        .header(ORIGIN, "https://app.example.test")
        .header(ACCEPT, MCP_ACCEPT)
        .header(CONTENT_TYPE, "application/json")
        .body(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"origin-probe","version":"0"}}}"#,
        )
        .send()
        .await
        .expect("request should receive an HTTP response");
    let status = allowed.status();
    let body = allowed.text().await.expect("the answer has a body");
    assert!(
        status.is_success() && body.contains("protocolVersion"),
        "a request carrying an Origin was not served: {status} {body}"
    );

    let refused = client
        .post(server.mcp_url())
        .header(HOST, "attacker.example")
        .header(ORIGIN, "https://app.example.test")
        .header(ACCEPT, MCP_ACCEPT)
        .header(CONTENT_TYPE, "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()
        .await
        .expect("request should receive an HTTP response");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::FORBIDDEN,
        "a disallowed Host was served because the request carried an Origin"
    );

    server.stop().await;
}

/// Captured warnings, shared by every test in this binary. The transport serves on its own
/// tokio worker threads, so a thread-scoped subscriber would never see its events; the
/// global one does, and each caller keys its assertions on an allowlist entry unique to
/// its own server rather than on the buffer being otherwise empty.
static WARNINGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static CAPTURE_WARNINGS: Once = Once::new();

fn capture_warnings() {
    #[derive(Clone, Copy)]
    struct Sink;
    impl std::io::Write for Sink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            WARNINGS.lock().expect("warning buffer is never poisoned").extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl tracing_subscriber::fmt::MakeWriter<'_> for Sink {
        type Writer = Sink;
        fn make_writer(&self) -> Sink {
            *self
        }
    }

    CAPTURE_WARNINGS.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(Sink)
            .without_time()
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other test installs a global subscriber");
    });
}

fn captured_warnings() -> String {
    String::from_utf8_lossy(&WARNINGS.lock().expect("warning buffer is never poisoned"))
        .into_owned()
}

/// A refusal is a mismatch between the request's `Host` and the configured allowlist.
/// Logging only the first leaves the operator unable to see which of the two is wrong —
/// the reading that produced #30, where an allowlist that authorized nothing looked
/// like a server refusing the network.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_names_the_allowlist_it_failed_against() {
    capture_warnings();
    let server = TestServer::start(vec!["refusal-probe.example.test".to_owned()]).await;
    let response = reqwest::Client::new()
        .get(server.health_url())
        .header(HOST, "refusal-probe-client.example.test")
        .send()
        .await
        .expect("request should receive an HTTP response");
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    server.stop().await;

    let line = captured_warnings()
        .lines()
        .find(|line| line.contains("refusal-probe-client.example.test"))
        .map(str::to_owned)
        .expect("the refusal is warned about");
    assert!(line.contains("allowed=refusal-probe.example.test"), "{line}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_request_body_is_rejected() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let client = reqwest::Client::new();
    let outcome = client
        .post(server.mcp_url())
        .header(ACCEPT, MCP_ACCEPT)
        .header(CONTENT_TYPE, "application/json")
        .body(vec![b'x'; MAX_HTTP_REQUEST_BODY_BYTES + 1])
        .send()
        .await;

    // The limit is enforced from `Content-Length`, before the body is read, so the
    // server may answer 413 and close while the client is still writing its megabyte.
    // Either outcome proves the request was refused rather than dispatched; which one
    // the client observes is a scheduling race.
    match outcome {
        Ok(response) => assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE),
        Err(error) => assert!(error.is_request(), "unexpected transport failure: {error}"),
    }

    // Survival is a property of the server, not of the connection the refusal ran on:
    // that one is closed by design, and asking on it would measure keep-alive instead.
    let health = reqwest::Client::new()
        .get(server.health_url())
        .send()
        .await
        .expect("server should survive the refusal");
    assert_eq!(health.status(), reqwest::StatusCode::OK);

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_chunked_body_is_rejected_with_the_same_status() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let status = chunked_oversize_response_head(server.address).await;
    assert!(
        status.starts_with("HTTP/1.1 413"),
        "the limit must not depend on request framing, got: {}",
        status.lines().next().unwrap_or_default()
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_request_body_cannot_block_shutdown() {
    use tokio::io::AsyncWriteExt;

    let server = TestServer::start(loopback_allowed_hosts()).await;
    let mut stalled =
        tokio::net::TcpStream::connect(server.address).await.expect("test client should connect");
    stalled
        .write_all(
            format!(
                "POST /mcp HTTP/1.1\r\nHost: {}\r\nAccept: {MCP_ACCEPT}\r\n\
                 Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{",
                server.address
            )
            .as_bytes(),
        )
        .await
        .expect("headers and a partial body should be written");

    // The connection stays open with 99 bytes outstanding. Shutdown must still finish.
    server.cancellation.cancel();
    tokio::time::timeout(SHUTDOWN_TEST_TIMEOUT, server.task)
        .await
        .expect("a half-sent body must not hold the server open")
        .expect("HTTP task should not panic")
        .expect("HTTP server should stop cleanly");

    drop(stalled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disallowed_host_is_refused_without_waiting_for_the_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let server = TestServer::start(loopback_allowed_hosts()).await;
    let mut stream =
        tokio::net::TcpStream::connect(server.address).await.expect("test client should connect");
    // Announces 100 bytes and sends one. A refused Host must not buy the client an
    // open connection for as long as it withholds the rest.
    stream
        .write_all(
            b"POST /mcp HTTP/1.1\r\nHost: attacker.example\r\nAccept: application/json, text/event-stream\r\n\
              Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{",
        )
        .await
        .expect("headers and a partial body should be written");

    let mut response = [0u8; 64];
    let read = tokio::time::timeout(TEST_TIMEOUT, stream.read(&mut response))
        .await
        .expect("the Host gate must answer before the body is complete")
        .expect("response should be readable");
    let status = String::from_utf8_lossy(&response[..read]);
    assert!(
        status.starts_with("HTTP/1.1 403"),
        "expected a Host refusal, got: {}",
        status.lines().next().unwrap_or_default()
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disallowed_host_is_rejected_on_the_readiness_endpoint() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let response = reqwest::Client::new()
        .get(server.health_url())
        .header(HOST, "attacker.example")
        .send()
        .await
        .expect("request should receive an HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_json_does_not_stop_the_server() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let client = reqwest::Client::new();
    let malformed = client
        .post(server.mcp_url())
        .header(ACCEPT, MCP_ACCEPT)
        .header(CONTENT_TYPE, "application/json")
        .body("{")
        .send()
        .await
        .expect("malformed request should receive an HTTP response");
    assert!(malformed.status().is_client_error());

    let health =
        client.get(server.health_url()).send().await.expect("server should remain reachable");
    assert_eq!(health.status(), reqwest::StatusCode::OK);

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_stops_an_active_session_and_releases_the_listener() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let address = server.address;
    let client = server.connect().await;

    TcpListener::bind(address)
        .await
        .expect_err("the running HTTP server must keep its listener exclusive");

    server.stop().await;

    let rebound = TcpListener::bind(address)
        .await
        .expect("graceful shutdown should release the port even with an active client");
    drop(rebound);
    drop(client);
}

/// Read a response head from a raw socket, stopping at the blank line so the reader
/// never swallows the start of whatever comes next.
async fn read_response_head(reader: &mut (impl tokio::io::AsyncReadExt + Unpin)) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = tokio::time::timeout(TEST_TIMEOUT, reader.read(&mut byte))
            .await
            .expect("the server should answer")
            .expect("the response should be readable");
        assert_ne!(
            read,
            0,
            "the server closed without answering: {}",
            String::from_utf8_lossy(&head)
        );
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

async fn response_head_for(address: SocketAddr, request: &str) -> String {
    use tokio::io::AsyncWriteExt;

    let mut stream =
        tokio::net::TcpStream::connect(address).await.expect("test client should connect");
    stream.write_all(request.as_bytes()).await.expect("the request should be written");
    read_response_head(&mut stream).await
}

/// The chunked oversize needs a body writer running alongside the read: the server stops
/// reading at the limit, so writing the whole thing first would never finish.
async fn chunked_oversize_response_head(address: SocketAddr) -> String {
    use tokio::io::AsyncWriteExt;

    let oversized = MAX_HTTP_REQUEST_BODY_BYTES + 1;
    let mut stream =
        tokio::net::TcpStream::connect(address).await.expect("test client should connect");
    stream
        .write_all(
            format!(
                "POST /mcp HTTP/1.1\r\nHost: {address}\r\nAccept: {MCP_ACCEPT}\r\n\
                 Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{oversized:x}\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("headers should be written");
    let (mut reader, mut writer) = stream.into_split();
    let body = tokio::spawn(async move {
        let _ = writer.write_all(&vec![b'x'; oversized]).await;
    });

    let head = read_response_head(&mut reader).await;
    body.abort();
    head
}

fn announced_oversize_request(address: SocketAddr) -> String {
    let oversized = MAX_HTTP_REQUEST_BODY_BYTES + 1;
    format!(
        "POST /mcp HTTP/1.1\r\nHost: {address}\r\nAccept: {MCP_ACCEPT}\r\n\
         Content-Type: application/json\r\nContent-Length: {oversized}\r\n\r\n"
    )
}

fn host_refusal_request(host: &str) -> String {
    // Announces a body and sends one byte of it, so the refusal provably precedes the
    // read rather than merely racing it.
    format!(
        "POST /mcp HTTP/1.1\r\nHost: {host}\r\nAccept: {MCP_ACCEPT}\r\n\
         Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{"
    )
}

fn announces_a_closed_connection(head: &str) -> bool {
    head.to_ascii_lowercase().contains("\r\nconnection: close\r\n")
}

/// Every refusal the server answers before routing leaves the request body unread, and
/// hyper cannot keep such a connection: it closes the read side and the socket dies right
/// after the response. Saying nothing leaves an HTTP/1.1 client entitled to pool that
/// connection and send its next request into a socket the server already closed — which
/// is how a refusal surfaces as `IncompleteMessage` on Linux and `WSAECONNABORTED` on
/// Windows instead of as the status the server actually sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_refusal_before_routing_announces_the_closed_connection() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let address = server.address;

    let refusals = [
        (
            "an announced oversize",
            response_head_for(address, &announced_oversize_request(address)).await,
        ),
        ("a chunked oversize", chunked_oversize_response_head(address).await),
        (
            "a malformed Host",
            response_head_for(address, &host_refusal_request("[[localhost]]")).await,
        ),
        (
            "a disallowed Host",
            response_head_for(address, &host_refusal_request("attacker.example")).await,
        ),
    ];
    for (refusal, head) in refusals {
        assert!(
            announces_a_closed_connection(&head),
            "the refusal of {refusal} discards the request body and closes the socket, \
             so the response must say so: {head}"
        );
    }

    server.stop().await;
}

/// The limit is a property of the request, not of how the request was framed. While the
/// announced size was refused by one layer and the chunked size by another, the two
/// answers differed in body and content type for no reason a client could act on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_oversize_refusal_does_not_depend_on_framing() {
    let server = TestServer::start(loopback_allowed_hosts()).await;
    let address = server.address;

    let announced = response_head_for(address, &announced_oversize_request(address)).await;
    let chunked = chunked_oversize_response_head(address).await;

    let without_date = |head: &str| {
        head.lines()
            .filter(|line| !line.to_ascii_lowercase().starts_with("date:"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        without_date(&announced),
        without_date(&chunked),
        "one limit must give one answer, whichever framing announced the oversize"
    );

    server.stop().await;
}

/// A refusal that did read the whole body leaves a perfectly usable connection, and
/// closing it would cost every client a reconnect for nothing. `/health` is registered
/// for GET only, so a complete POST to it is refused by the router — after the body was
/// collected. Without this case, closing every response whatsoever would satisfy every
/// other gate here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_that_read_the_body_keeps_the_connection() {
    use tokio::io::AsyncWriteExt;

    let server = TestServer::start(loopback_allowed_hosts()).await;
    let address = server.address;
    let mut stream =
        tokio::net::TcpStream::connect(address).await.expect("test client should connect");
    stream
        .write_all(
            format!(
                "POST /health HTTP/1.1\r\nHost: {address}\r\n\
                 Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
            )
            .as_bytes(),
        )
        .await
        .expect("the request should be written");

    let refusal = read_response_head(&mut stream).await;
    assert!(
        refusal.starts_with("HTTP/1.1 405"),
        "expected a method refusal from the router, got: {}",
        refusal.lines().next().unwrap_or_default()
    );
    assert!(
        !announces_a_closed_connection(&refusal),
        "the body was read, so the connection is still good and must not be given up: {refusal}"
    );

    stream
        .write_all(format!("GET /health HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
        .await
        .expect("the second request should be written");
    let reused = read_response_head(&mut stream).await;
    assert!(
        reused.starts_with("HTTP/1.1 200"),
        "the connection the refusal kept must still serve: {}",
        reused.lines().next().unwrap_or_default()
    );

    server.stop().await;
}
