use super::*;
use crate::broker::transport::{client_handshake, write_session_context};
use crate::broker::SharedBackendIdentity;
use crate::graph::{GraphState, GraphStatus};
use crate::{McpProfile, SharedState};
use rmcp::ServiceExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BUDGET: Duration = Duration::from_secs(8);

async fn attached(key: &SharedBackendKey) -> TokioStream {
    let mut stream = connect_endpoint(shared_backend_name(key).unwrap(), HANDSHAKE_TIMEOUT)
        .await
        .expect("one connection to the already bound daemon");
    client_handshake(
        &mut stream,
        &Handshake { identity: key.identity().clone(), adapter: AdapterKind::Mcp },
    )
    .await
    .unwrap();
    write_session_context(&mut stream, b"{}").await.unwrap();
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loading_and_failed_graph_do_not_block_parallel_broker_initialize_and_tools_list() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let identity =
        SharedBackendIdentity::new(root.path(), cache.path(), [1; 32], [2; 32], [3; 32], [4; 32]);
    let key = SharedBackendKey::new(identity);
    let listener = bind_shared(&key).await.unwrap().unwrap();
    let graph = GraphState::for_workspace(root.path().to_path_buf());
    assert!(graph.try_begin_external_build());
    assert_eq!(graph.status(), GraphStatus::Loading);
    let state = SharedState::shared().with_graph_for_test(graph.clone());
    let server = McpServer::new(McpProfile::Workspace, state.clone());
    let stop = CancellationToken::new();
    let _stop_guard = stop.clone().drop_guard();
    let daemon = tokio::spawn(serve_shared(
        key.clone(),
        listener,
        Arc::new(move |_hello, context, session, shutdown, _retired| {
            let server = server.clone();
            async move {
                assert_eq!(context, b"{}");
                let SharedSession::Mcp(stream) = session else { anyhow::bail!("MCP expected") };
                serve_stream_with_shutdown(server, stream, shutdown).await
            }
        }),
        CancellationToken::new(),
        stop.clone(),
        Duration::from_secs(30),
        Duration::from_secs(30),
        || (false, false),
        || {},
    ));

    let mut partial_private =
        connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
    partial_private.write_u8(0).await.unwrap();
    partial_private.flush().await.unwrap();
    let mut silent_public = attached(&key).await;
    let first = attached(&key).await;
    let second = attached(&key).await;
    let (first, second) =
        tokio::time::timeout(BUDGET, async { tokio::join!(().serve(first), ().serve(second)) })
            .await
            .expect("parallel initialize is independent of graph and stalled peers");
    let first = first.unwrap();
    let second = second.unwrap();
    let (first_tools, second_tools) = tokio::time::timeout(BUDGET, async {
        tokio::join!(first.list_all_tools(), second.list_all_tools())
    })
    .await
    .unwrap();
    let first_tools = first_tools.unwrap();
    assert!(!first_tools.is_empty());
    assert_eq!(first_tools.len(), second_tools.unwrap().len());
    assert_eq!(graph.status(), GraphStatus::Loading, "the build is still held");

    graph.fail_with_retry_held_until(std::time::Instant::now() + Duration::from_secs(60));
    assert!(matches!(graph.status(), GraphStatus::Failed(_)));
    let third =
        tokio::time::timeout(BUDGET, ().serve(attached(&key).await)).await.unwrap().unwrap();
    let (existing, new) = tokio::time::timeout(BUDGET, async {
        tokio::join!(first.list_all_tools(), third.list_all_tools())
    })
    .await
    .unwrap();
    assert_eq!(existing.unwrap().len(), first_tools.len());
    assert_eq!(new.unwrap().len(), first_tools.len());
    assert!(matches!(graph.status(), GraphStatus::Failed(_)), "handshake never retries graph work");

    let (private_closed, public_closed) = tokio::join!(
        tokio::time::timeout(BUDGET, partial_private.read_u8()),
        tokio::time::timeout(BUDGET, silent_public.read_u8()),
    );
    assert!(private_closed.expect("private handshake has its own deadline").is_err());
    assert!(public_closed.expect("public initialize has its own deadline").is_err());
    assert!(!tokio::time::timeout(BUDGET, second.list_all_tools())
        .await
        .unwrap()
        .unwrap()
        .is_empty());

    stop.cancel();
    tokio::time::timeout(BUDGET, daemon).await.unwrap().unwrap().unwrap();
    state.shutdown();
    let closed = tokio::time::timeout(BUDGET, third.waiting()).await.unwrap();
    assert!(closed.is_ok());
    let _ = tokio::time::timeout(BUDGET, first.cancel()).await;
    let _ = tokio::time::timeout(BUDGET, second.cancel()).await;
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_shared_session_ending_before_the_first_poll_gets_its_full_idle_ttl() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let identity =
        SharedBackendIdentity::new(root.path(), cache.path(), [1; 32], [2; 32], [3; 32], [4; 32]);
    let key = SharedBackendKey::new(identity);
    let listener = bind_shared(&key).await.unwrap().unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let callback_entered = Arc::clone(&entered);
    let callback_finished = Arc::clone(&finished);
    let callback_release = Arc::clone(&release);
    let polls = Arc::new(AtomicU64::new(0));
    let loop_polls = Arc::clone(&polls);
    let stopped = Arc::new(AtomicBool::new(false));
    let loop_stopped = Arc::clone(&stopped);
    let stop = CancellationToken::new();
    let _stop_guard = stop.clone().drop_guard();
    let ttl = Duration::from_secs(30);
    let started = Instant::now();
    let daemon = tokio::spawn(serve_shared(
        key.clone(),
        listener,
        Arc::new(move |_hello, _context, session, _shutdown, _retired| {
            let entered = Arc::clone(&callback_entered);
            let finished = Arc::clone(&callback_finished);
            let release = Arc::clone(&callback_release);
            async move {
                let SharedSession::Mcp(mut stream) = session else { anyhow::bail!("MCP expected") };
                assert_eq!(stream.read_u8().await?, 7);
                entered.store(true, Ordering::SeqCst);
                release.notified().await;
                assert_eq!(stream.read(&mut [0]).await?, 0);
                finished.store(true, Ordering::SeqCst);
                Ok(())
            }
        }),
        CancellationToken::new(),
        stop,
        ttl,
        ttl,
        move || {
            loop_polls.fetch_add(1, Ordering::SeqCst);
            (false, false)
        },
        move || loop_stopped.store(true, Ordering::SeqCst),
    ));
    let client = tokio::spawn(async move {
        let mut client = attached(&key).await;
        client.write_u8(7).await.unwrap();
        client.flush().await.unwrap();
        client
    });
    // Keeping this task runnable drives real socket IO without allowing the paused
    // clock to jump to the daemon's next timer while the handshake is in progress.
    yield_until("the session read its first real byte", || {
        entered.load(Ordering::SeqCst) && client.is_finished()
    })
    .await;
    let client = client.await.unwrap();
    assert_eq!(Instant::now(), started);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(2)).await;
    drop(client);
    release.notify_one();
    yield_until("the peer EOF ended the session", || finished.load(Ordering::SeqCst)).await;
    let disconnected = Instant::now();
    assert_eq!(disconnected - started, Duration::from_secs(2));
    assert_eq!(polls.load(Ordering::SeqCst), 0);

    assert_full_post_session_idle_ttl(daemon, started, disconnected, ttl, || {
        polls.load(Ordering::SeqCst)
    })
    .await;
    assert!(stopped.load(Ordering::SeqCst));
}

#[cfg(unix)]
async fn yield_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(5));
    while !done() {
        assert!(std::time::Instant::now() < deadline, "never happened: {what}");
        tokio::task::yield_now().await;
    }
}

#[cfg(unix)]
async fn assert_full_post_session_idle_ttl(
    daemon: tokio::task::JoinHandle<anyhow::Result<()>>,
    started: Instant,
    disconnected: Instant,
    ttl: Duration,
    mut polls: impl FnMut() -> u64,
) {
    let poll = ttl / 4;
    tokio::time::advance(poll - (disconnected - started)).await;
    yield_until("the first idle poll", || polls() >= 1).await;
    tokio::time::advance(ttl - poll).await;
    yield_until("the startup TTL poll", || polls() >= 2).await;
    assert_eq!(Instant::now() - started, ttl);
    assert!(Instant::now() - disconnected < ttl);
    assert!(!daemon.is_finished(), "startup age must not shorten the post-session TTL");

    tokio::time::advance(poll).await;
    yield_until("normal idle shutdown", || daemon.is_finished()).await;
    daemon.await.unwrap().unwrap();
    assert!(Instant::now() - disconnected >= ttl);
    assert!(Instant::now() - disconnected <= ttl + poll);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_parked_session_ending_before_the_first_poll_gets_its_full_idle_ttl() {
    use tracing_subscriber::prelude::*;

    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("lifecycle.log");
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::sync::Mutex::new(std::fs::File::create(&log).unwrap()))
        .with_ansi(false)
        .with_target(false)
        .finish()
        .with(tracing_subscriber::filter::filter_fn(|metadata| {
            metadata.target() == "broker_lifetime_probe"
        }));
    let _trace_guard = tracing::subscriber::set_default(subscriber);
    let key = BackendKey::new(
        directory.path(),
        directory.path().join("cache"),
        McpProfile::Reference,
        0,
        0,
        Default::default(),
    );
    let listener = bind(&key).await.unwrap().unwrap();
    let client = tokio::spawn(async move {
        connect_endpoint(backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap()
    });
    yield_until("the parked connection reached its listener", || client.is_finished()).await;
    let stream = client.await.unwrap();
    let parked = listener.accept().await.unwrap();
    let server = McpServer::new(McpProfile::Reference, SharedState::shared());
    let cleanup = server.clone();
    let ttl = Duration::from_secs(30);
    let started = Instant::now();
    let daemon = tokio::spawn(serve(server, listener, vec![parked], ttl, ttl));
    let client = tokio::spawn(async move { ().serve(stream).await.unwrap() });
    yield_until("the parked SDK session initialized", || {
        client.is_finished()
            && std::fs::read_to_string(&log).unwrap().contains("session-initialized")
    })
    .await;
    let client = client.await.unwrap();
    assert_eq!(Instant::now(), started);
    assert!(!std::fs::read_to_string(&log).unwrap().contains("idle-tick"));
    tokio::time::advance(Duration::from_secs(2)).await;
    let closed = tokio::spawn(async move { client.cancel().await.unwrap() });
    yield_until("the parked SDK session finished", || {
        closed.is_finished()
            && std::fs::read_to_string(&log).unwrap().contains("session-waiting-returned")
    })
    .await;
    closed.await.unwrap();
    let disconnected = Instant::now();
    assert_eq!(disconnected - started, Duration::from_secs(2));
    assert!(!std::fs::read_to_string(&log).unwrap().contains("idle-tick"));

    assert_full_post_session_idle_ttl(daemon, started, disconnected, ttl, || {
        std::fs::read_to_string(&log).unwrap().matches("idle-tick").count() as u64
    })
    .await;
    let text = std::fs::read_to_string(log).unwrap();
    assert!(text.contains("active=0"));
    assert!(text.contains("warmed=true"));
    cleanup.shutdown();
}
