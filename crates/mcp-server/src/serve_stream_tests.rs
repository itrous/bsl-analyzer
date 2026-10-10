use std::time::Duration;

use rmcp::model::{ClientRequest, PingRequest};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{serve_stream_with_shutdown, McpProfile, McpServer, SharedState};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// A completed MCP handshake exercises the service task that owns the transport.
async fn initialized_session(
    shutdown: &CancellationToken,
) -> (RunningService<RoleClient, ()>, JoinHandle<anyhow::Result<()>>) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server = McpServer::new(McpProfile::Reference, SharedState::shared());
    let task = tokio::spawn(serve_stream_with_shutdown(server, server_io, shutdown.clone()));
    let client = ().serve(client_io).await.expect("client initializes");
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

/// One disconnected client must not cancel its siblings; broker shutdown must close them.
#[tokio::test]
async fn stream_shutdown_closes_transport_without_cancelling_sibling_sessions() {
    let shutdown = CancellationToken::new();
    let (first_client, first_task) = initialized_session(&shutdown).await;
    let (second_client, second_task) = initialized_session(&shutdown).await;

    first_client.cancel().await.expect("first client disconnects");
    session_finished(first_task).await;
    let request = ClientRequest::PingRequest(PingRequest::default());
    second_client.send_request(request.clone()).await.expect("sibling remains connected");

    shutdown.cancel();
    session_finished(second_task).await;
    tokio::time::timeout(SHUTDOWN_TIMEOUT, second_client.send_request(request))
        .await
        .expect("closed transport answers without hanging")
        .expect_err("no request is served after session shutdown completes");
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
        (().serve(client_io).await.expect("first client initializes"), task)
    };
    let (second_client, second_task) = {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let task =
            tokio::spawn(serve_stream_with_shutdown(second, server_io, daemon_shutdown.clone()));
        (().serve(client_io).await.expect("second client initializes"), task)
    };

    state.scope_transport_stop().cancel();
    session_finished(first_task).await;
    session_finished(second_task).await;
    assert!(
        first_client
            .send_request(ClientRequest::PingRequest(PingRequest::default()))
            .await
            .is_err(),
        "scope retirement closes the first session"
    );
    assert!(
        second_client
            .send_request(ClientRequest::PingRequest(PingRequest::default()))
            .await
            .is_err(),
        "scope retirement closes the second session"
    );
    assert!(!daemon_shutdown.is_cancelled(), "scope retirement leaves broker shutdown distinct");
}
