use super::bounded;
use mcp_server::broker::daemon::{self, SharedSession};
use mcp_server::broker::transport::{
    client_handshake, write_session_context, AdapterKind, Handshake,
};
use mcp_server::broker::{proxy, SharedBackendIdentity, SharedBackendKey};
use mcp_server::{McpProfile, McpServer, SharedState};
use rmcp::ServiceExt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_shared_broker_keeps_its_sdk_client_across_bounded_log_rotations() {
    let source = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let path = logs.path().join("daemon.log");
    let previous = logs.path().join("daemon.log.prev");
    let mut writer = bounded::BoundedLog::open(&path, true).unwrap();
    let identity =
        SharedBackendIdentity::new(source.path(), logs.path(), [1; 32], [2; 32], [3; 32], [4; 32]);
    let key = SharedBackendKey::new(identity.clone());
    let socket = key.socket_path().unwrap();
    let state = SharedState::reference(None);
    let server = McpServer::new(McpProfile::Reference, state.clone());
    let session_starts = Arc::new(AtomicUsize::new(0));
    let started = session_starts.clone();
    let shutdown_seen = Arc::new(AtomicBool::new(false));
    let stopped = shutdown_seen.clone();
    let stop = CancellationToken::new();
    let _stop_guard = stop.clone().drop_guard();
    let daemon = tokio::spawn(daemon::run_shared(
        key.clone(),
        move |_hello, context, session, shutdown, _retired| {
            let server = server.clone();
            started.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(context, b"{}");
                let SharedSession::Mcp(stream) = session else { anyhow::bail!("MCP expected") };
                mcp_server::serve_stream_with_shutdown(server, stream, shutdown).await
            }
        },
        CancellationToken::new(),
        stop.clone(),
        Duration::from_secs(60),
        Duration::from_secs(60),
        || (false, false),
        move || {
            state.shutdown();
            stopped.store(true, Ordering::SeqCst);
        },
    ));
    let budget = test_utils::wait::ceiling(Duration::from_secs(20));
    tokio::time::timeout(budget, async {
        while !socket.exists() {
            assert!(!daemon.is_finished(), "daemon must bind before a client connects");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observe the bound endpoint without a connect retry or fallback");

    let mut stream = proxy::connect_required_stream(&key, std::process::id()).await.unwrap();
    client_handshake(&mut stream, &Handshake { identity, adapter: AdapterKind::Mcp })
        .await
        .unwrap();
    write_session_context(&mut stream, b"{}").await.unwrap();
    let client = tokio::time::timeout(budget, ().serve(stream)).await.unwrap().unwrap();
    let expected = tokio::time::timeout(budget, client.list_all_tools()).await.unwrap().unwrap();
    assert!(!expected.is_empty());

    let payload = vec![b'x'; bounded::LOG_BYTES as usize + 64];
    for rotation in 0..6 {
        writer.write_all(&payload).unwrap();
        let marker = format!("latest after rotation {rotation}\n");
        writer.write_all(marker.as_bytes()).unwrap();
        writer.flush().unwrap();
        let current_bytes = path.metadata().unwrap().len();
        let previous_bytes = previous.metadata().unwrap().len();
        assert!(current_bytes <= bounded::LOG_BYTES);
        assert!(previous_bytes <= bounded::LOG_BYTES);
        assert!(current_bytes + previous_bytes <= 2 * bounded::LOG_BYTES);
        assert_eq!(std::fs::read_dir(logs.path()).unwrap().count(), 2);
        assert!(std::fs::read(&path).unwrap().ends_with(marker.as_bytes()));
        let tools = tokio::time::timeout(budget, client.list_all_tools()).await.unwrap().unwrap();
        assert_eq!(tools.len(), expected.len());
        assert_eq!(session_starts.load(Ordering::SeqCst), 1, "the same session stays admitted");
        assert!(!daemon.is_finished());
        assert!(!shutdown_seen.load(Ordering::SeqCst));
    }
    let tools = tokio::time::timeout(budget, client.list_all_tools()).await.unwrap().unwrap();
    assert_eq!(tools.len(), expected.len());
    tokio::time::timeout(budget, client.cancel()).await.unwrap().unwrap();
    stop.cancel();
    tokio::time::timeout(budget, daemon).await.unwrap().unwrap().unwrap();
    assert!(shutdown_seen.load(Ordering::SeqCst));
    assert_eq!(session_starts.load(Ordering::SeqCst), 1);
}
