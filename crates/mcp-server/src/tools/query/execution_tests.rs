use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotification, CancelledNotificationParam,
    ClientRequest, PingRequest,
};
use rmcp::service::{PeerRequestOptions, RunningService, ServiceError};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{McpProfile, McpServer, OnecConnection, SessionConfig, SharedState};

const PACKAGE: &str = include_str!("fixtures/package.sdbl");
const ERROR: &str =
    "Ключевое слово РАЗРЕШЕННЫЕ доступно только у первого запроса; controlled-server-marker";
const GUARD: Duration = Duration::from_secs(5);
const HEALTHY: &str = "ВЫБРАТЬ 1 КАК Значение";

#[derive(Clone, Copy)]
enum ReplyKind {
    Error,
    Success,
    Hold,
    InvalidJson,
}

struct Reply {
    query: String,
    kind: ReplyKind,
}

impl Reply {
    fn new(query: &str, kind: ReplyKind) -> Self {
        Self { query: query.to_owned(), kind }
    }
}

#[derive(Clone)]
struct StubState {
    expected: Arc<Mutex<VecDeque<Reply>>>,
    count: Arc<AtomicUsize>,
    entered: mpsc::UnboundedSender<usize>,
    release: Arc<Notify>,
    finished: Arc<AtomicBool>,
    finished_signal: Arc<Notify>,
    jobs: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

async fn receive_query(State(state): State<StubState>, Json(body): Json<Value>) -> Response {
    let count = state.count.fetch_add(1, Ordering::SeqCst) + 1;
    let expected =
        state.expected.lock().unwrap().pop_front().expect("unexpected additional HTTP query");
    assert!(
        body["query"].as_str() == Some(expected.query.as_str()),
        "controlled query identity changed or was retried"
    );
    assert_eq!(body["limit"], 20);
    assert_eq!(body["parameters"]["Контроль"], true);
    let _ = state.entered.send(count);
    match expected.kind {
        ReplyKind::Error => (StatusCode::INTERNAL_SERVER_ERROR, ERROR).into_response(),
        ReplyKind::Hold => {
            let (done, completed) = oneshot::channel();
            let job_state = state.clone();
            let job = tokio::spawn(async move {
                job_state.release.notified().await;
                job_state.finished.store(true, Ordering::SeqCst);
                job_state.finished_signal.notify_one();
                let _ = done.send(());
            });
            state.jobs.lock().unwrap().push(job);
            let _ = completed.await;
            success()
        }
        ReplyKind::Success => success(),
        ReplyKind::InvalidJson => "invalid JSON".into_response(),
    }
}

fn success() -> Response {
    Json(json!({"columns":["ControlledResult"],"rows":[[1]],"total":1,"truncated":false}))
        .into_response()
}

struct HttpStub {
    url: String,
    state: StubState,
    entered: mpsc::UnboundedReceiver<usize>,
    task: JoinHandle<()>,
}

impl HttpStub {
    async fn new(replies: Vec<Reply>) -> Self {
        let (sender, entered) = mpsc::unbounded_channel();
        let state = StubState {
            expected: Arc::new(Mutex::new(replies.into())),
            count: Arc::new(AtomicUsize::new(0)),
            entered: sender,
            release: Arc::new(Notify::new()),
            finished: Arc::new(AtomicBool::new(false)),
            finished_signal: Arc::new(Notify::new()),
            jobs: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new().route("/query", post(receive_query)).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, state, entered, task }
    }

    async fn entered(&mut self, count: usize) {
        assert_eq!(tokio::time::timeout(GUARD, self.entered.recv()).await.unwrap(), Some(count));
    }

    async fn release(&self) {
        self.state.release.notify_one();
        tokio::time::timeout(GUARD, self.state.finished_signal.notified()).await.unwrap();
        assert!(self.state.finished.load(Ordering::SeqCst));
    }

    fn requests(&self) -> usize {
        self.state.count.load(Ordering::SeqCst)
    }
}

impl Drop for HttpStub {
    fn drop(&mut self) {
        self.state.release.notify_waiters();
        for job in self.state.jobs.lock().unwrap().drain(..) {
            job.abort();
        }
        self.task.abort();
    }
}

struct Sdk {
    client: Option<RunningService<RoleClient, ()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
    stop: CancellationToken,
}

impl Sdk {
    async fn new(stub: &HttpStub, state: SharedState) -> Self {
        let mut config = SessionConfig::default();
        config.add_onec_connection(
            "stub".to_owned(),
            OnecConnection::new(onec_client::Client::new(&stub.url, "", ""), false),
        );
        let server = McpServer::new(McpProfile::Workspace, state).for_session(config);
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let stop = CancellationToken::new();
        let task = tokio::spawn(crate::serve_stream_with_shutdown(server, server_io, stop.clone()));
        let client = tokio::time::timeout(GUARD, ().serve(client_io)).await.unwrap().unwrap();
        Self { client: Some(client), task: Some(task), stop }
    }

    fn client(&self) -> &RunningService<RoleClient, ()> {
        self.client.as_ref().unwrap()
    }

    async fn close(mut self) {
        tokio::time::timeout(GUARD, self.client.take().unwrap().cancel()).await.unwrap().unwrap();
        self.stop.cancel();
        tokio::time::timeout(GUARD, self.task.take().unwrap()).await.unwrap().unwrap().unwrap();
    }
}

impl Drop for Sdk {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn arguments(query: &str) -> CallToolRequestParams {
    CallToolRequestParams::new("query").with_arguments(json!({"action":"execute","query":query,"limit":20,"parameters":{"Контроль":true},"connection":"stub"}).as_object().unwrap().clone())
}

fn server_error(error: ServiceError) -> rmcp::ErrorData {
    match error {
        ServiceError::McpError(error) => {
            assert_eq!(error.code.0, -32603);
            error
        }
        _ => panic!("expected an MCP server error"),
    }
}

fn controls() -> Vec<(String, ReplyKind)> {
    let parsed = parser::parse_sdbl(PACKAGE);
    assert!(!parsed.has_errors());
    let root = parsed.syntax_node();
    let members = root.children().collect::<Vec<_>>();
    assert_eq!(members.len(), 6);
    assert_eq!(
        root.descendants()
            .filter(|node| node.kind() == syntax::SyntaxKind::SDBL_INTO_CLAUSE)
            .count(),
        5
    );
    let final_query = members.last().unwrap();
    let allowed = final_query
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.text().to_uppercase() == "РАЗРЕШЕННЫЕ")
        .collect::<Vec<_>>();
    assert_eq!(allowed.len(), 1);
    let mut corrected = PACKAGE.to_owned();
    let range = allowed[0].text_range();
    corrected.replace_range(usize::from(range.start())..usize::from(range.end()), "");
    let mut simple = PACKAGE.to_owned();
    let range = final_query.text_range();
    simple.replace_range(usize::from(range.start())..usize::from(range.end()), HEALTHY);
    let short = "ВЫБРАТЬ 1 КАК Значение ПОМЕСТИТЬ Временная; ВЫБРАТЬ Т.Значение ИЗ (ВЫБРАТЬ РАЗРЕШЕННЫЕ 1 КАК Значение) КАК Т";
    let long = format!("{short}\n// {}", "x".repeat(4500));
    let controls = vec![
        (PACKAGE.to_owned(), ReplyKind::Error),
        (corrected, ReplyKind::Success),
        (simple, ReplyKind::Success),
        (final_query.text().to_string(), ReplyKind::Error),
        (short.to_owned(), ReplyKind::Error),
        (long, ReplyKind::Error),
    ];
    for (query, _) in &controls {
        assert!(!parser::parse_sdbl(query).has_errors());
        assert!(super::first_member_is_select(query));
    }
    controls
}

#[tokio::test]
async fn sdk_package_error_and_corrected_short_controls_preserve_terminal_server_verdicts() {
    let controls = controls();
    let mut stub =
        HttpStub::new(controls.iter().map(|(query, kind)| Reply::new(query, *kind)).collect())
            .await;
    let sdk = Sdk::new(&stub, SharedState::shared()).await;
    for (index, (query, reply)) in controls.into_iter().enumerate() {
        let result = tokio::time::timeout(GUARD, sdk.client().call_tool(arguments(&query)))
            .await
            .expect("query finishes within the controlled server budget");
        match reply {
            ReplyKind::Error => {
                let error = server_error(result.unwrap_err());
                assert!(
                    error.message.contains(ERROR),
                    "server exception text survives HTTP and MCP"
                );
                assert!(error.message.contains("status 500"));
                assert_eq!(
                    error.data.as_ref().and_then(|data| data.get("phase")).and_then(Value::as_str),
                    Some("response"),
                );
            }
            _ => assert!(result.unwrap().content.iter().any(|content| content
                .as_text()
                .is_some_and(|text| text.text.contains("ControlledResult")))),
        }
        stub.entered(index + 1).await;
        assert_eq!(stub.requests(), index + 1, "no automatic package retry");
    }
    assert!(stub.state.expected.lock().unwrap().is_empty());
    sdk.close().await;
}

#[tokio::test]
async fn sdk_query_http_timeout_is_terminal_without_retry_or_claimed_remote_rollback() {
    let mut stub = HttpStub::new(vec![
        Reply::new(PACKAGE, ReplyKind::Hold),
        Reply::new(HEALTHY, ReplyKind::Success),
    ])
    .await;
    let sdk = Sdk::new(&stub, SharedState::shared()).await;
    let handle = sdk
        .client()
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(arguments(PACKAGE))),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    stub.entered(1).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let response = tokio::time::timeout(GUARD, handle.await_response())
        .await
        .expect("HTTP budget completes the MCP wait");
    let error = server_error(response.unwrap_err());
    assert!(error.message.contains("HTTP request failed"));
    assert!(!error.message.contains("http://"));
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("phase")).and_then(Value::as_str),
        Some("transport"),
    );
    assert_eq!(stub.requests(), 1);
    assert!(
        !stub.state.finished.load(Ordering::SeqCst),
        "local timeout does not prove remote termination"
    );
    stub.release().await;
    tokio::time::timeout(GUARD, sdk.client().call_tool(arguments(HEALTHY))).await.unwrap().unwrap();
    stub.entered(2).await;
    assert_eq!(stub.requests(), 2, "only the explicit next query starts another HTTP request");
    sdk.close().await;
}

#[tokio::test]
async fn sdk_query_malformed_response_reports_decode_phase_without_retry() {
    let stub = HttpStub::new(vec![Reply::new(HEALTHY, ReplyKind::InvalidJson)]).await;
    let sdk = Sdk::new(&stub, SharedState::shared()).await;
    let result =
        tokio::time::timeout(GUARD, sdk.client().call_tool(arguments(HEALTHY))).await.unwrap();
    let error = server_error(result.unwrap_err());
    assert!(error.message.contains("invalid JSON response from 1C"));
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("phase")).and_then(Value::as_str),
        Some("decode"),
    );
    assert_eq!(stub.requests(), 1);
    sdk.close().await;
}

#[tokio::test]
async fn sdk_query_cancel_is_terminal_without_retry_and_preserves_sibling_session() {
    let mut stub = HttpStub::new(vec![
        Reply::new(PACKAGE, ReplyKind::Hold),
        Reply::new(HEALTHY, ReplyKind::Success),
    ])
    .await;
    let state = SharedState::shared();
    let sdk = Sdk::new(&stub, state.clone()).await;
    let sibling = Sdk::new(&stub, state).await;
    let handle = sdk
        .client()
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(arguments(PACKAGE))),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    stub.entered(1).await;
    let notification = CancelledNotification::new(CancelledNotificationParam::new(
        Some(handle.id.clone()),
        Some("controlled query cancellation".to_owned()),
    ));
    sdk.client().peer().send_notification(notification.into()).await.unwrap();
    let response = tokio::time::timeout(GUARD, handle.await_response())
        .await
        .expect("SDK cancel completes the request");
    assert!(matches!(response, Err(ServiceError::Cancelled { .. })));
    assert_eq!(stub.requests(), 1);
    assert!(!stub.state.finished.load(Ordering::SeqCst));
    tokio::time::timeout(
        GUARD,
        sibling.client().send_request(ClientRequest::PingRequest(PingRequest::default())),
    )
    .await
    .unwrap()
    .unwrap();
    stub.release().await;
    tokio::time::timeout(GUARD, sibling.client().call_tool(arguments(HEALTHY)))
        .await
        .unwrap()
        .unwrap();
    stub.entered(2).await;
    assert_eq!(stub.requests(), 2);
    sdk.close().await;
    sibling.close().await;
}

#[tokio::test]
async fn sdk_missing_query_reports_admission_phase_before_http() {
    let stub = HttpStub::new(Vec::new()).await;
    let sdk = Sdk::new(&stub, SharedState::shared()).await;
    let params = CallToolRequestParams::new("query").with_arguments(
        json!({"action": "execute", "connection": "stub"}).as_object().unwrap().clone(),
    );
    let result = tokio::time::timeout(GUARD, sdk.client().call_tool(params)).await.unwrap();
    let ServiceError::McpError(error) = result.unwrap_err() else {
        panic!("expected missing query refusal")
    };
    assert_eq!(error.code.0, -32602);
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("phase")).and_then(Value::as_str),
        Some("admission"),
    );
    assert_eq!(stub.requests(), 0);
    sdk.close().await;
}

#[tokio::test]
async fn sdk_first_member_refusal_finishes_before_http() {
    let stub = HttpStub::new(Vec::new()).await;
    let sdk = Sdk::new(&stub, SharedState::shared()).await;
    let result =
        tokio::time::timeout(GUARD, sdk.client().call_tool(arguments("УНИЧТОЖИТЬ Временная")))
            .await
            .unwrap();
    let ServiceError::McpError(error) = result.unwrap_err() else {
        panic!("expected input refusal")
    };
    assert_eq!(error.code.0, -32602);
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("phase")).and_then(Value::as_str),
        Some("admission"),
    );
    assert_eq!(stub.requests(), 0);
    sdk.close().await;
}
