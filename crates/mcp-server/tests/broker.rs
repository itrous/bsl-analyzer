//! End-to-end broker mechanics: one backend serves many sessions, a second launch
//! defers to the live backend (bind-wins), the backend stays warm across client
//! disconnects, and it idles out on its own after its grace.
//!
//! Uses the lightweight `reference` profile so no heavy workspace build is needed,
//! and points the per-user runtime dir at a tempdir so the socket is isolated.

use std::sync::Arc;
use std::time::Duration;

#[cfg(any(unix, windows))]
use interprocess::local_socket::tokio::prelude::*;
#[cfg(any(unix, windows))]
use interprocess::local_socket::tokio::Stream as TokioStream;
#[cfg(any(unix, windows))]
use mcp_server::broker::{self, BackendKey};
use mcp_server::{serve_stream, McpProfile, McpServer, SharedState};
use rmcp::model::CallToolRequestParams;
use rmcp::ServiceExt;
use tempfile::TempDir;

fn reference_server() -> McpServer {
    McpServer::new(McpProfile::Reference, SharedState::shared())
}

#[cfg(any(unix, windows))]
fn key_for(src: &TempDir) -> BackendKey {
    // Profile here only names the socket; the served profile is the passed server.
    BackendKey::new(
        src.path(),
        mcp_server::WorkspaceCacheLayout::for_workspace(src.path()).root(),
        McpProfile::Workspace,
        0,
        0,
        std::collections::BTreeSet::new(),
    )
}

#[cfg(any(unix, windows))]
async fn connect(key: &BackendKey) -> std::io::Result<TokioStream> {
    let name = broker::backend_name(key)?;
    TokioStream::connect(name).await
}

#[cfg(any(unix, windows))]
async fn connect_within(key: &BackendKey, budget: Duration) -> TokioStream {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Ok(s) = connect(key).await {
            return s;
        }
        assert!(tokio::time::Instant::now() < deadline, "backend never became reachable");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(any(unix, windows))]
async fn backend_survives_client_disconnect_and_stays_reusable() {
    // No `set_var` here: it races with other tests' `getenv` (a glibc env data race)
    // and would corrupt the resolved socket path. A unique source dir already gives a
    // unique socket name under the real runtime dir, so no isolation env is needed.
    let src = TempDir::new().unwrap();
    let key = key_for(&src);

    // Long graces so nothing idles out mid-test: this asserts survival, not exit.
    let grace = Duration::from_secs(30);
    let idle_ttl = Duration::from_secs(30);
    let backend = tokio::spawn(broker::daemon::run(
        || Ok(reference_server()),
        key_for(&src),
        grace,
        idle_ttl,
    ));

    // Two concurrent client sessions through the one backend.
    let s1 = connect_within(&key, Duration::from_secs(25)).await;
    let c1 = ().serve(s1).await.expect("first client initialized");
    let s2 = connect(&key).await.expect("second session connects");
    let c2 = ().serve(s2).await.expect("second client initialized");

    assert!(c1.peer_info().is_some(), "first session saw server info");
    assert!(c2.peer_info().is_some(), "second session saw server info");

    // Close the first session. Under the old owner-coupled lifetime this tore the whole
    // backend down and severed every other session; now the backend must stay warm and
    // keep serving c2.
    c1.cancel().await.ok();

    // c2 must still work after its peer left — the backend did not tear down.
    let mut args = serde_json::Map::new();
    args.insert("action".to_owned(), serde_json::Value::String("status".to_owned()));
    let call = c2.call_tool(CallToolRequestParams::new("search").with_arguments(args));
    let after = tokio::time::timeout(Duration::from_secs(15), call).await;
    assert!(
        matches!(after, Ok(Ok(_))),
        "surviving session must still be served after a peer disconnects, got: {after:?}"
    );

    // A fresh client connecting after the first left reuses the warm backend — the whole
    // point of the broker, and what the owner-coupled lifetime broke for reconnecting
    // editors.
    let s3 = connect_within(&key, Duration::from_secs(10)).await;
    let c3 = ().serve(s3).await.expect("reconnecting client reuses the warm backend");
    assert!(c3.peer_info().is_some(), "reconnecting session saw server info");

    c2.cancel().await.ok();
    c3.cancel().await.ok();
    backend.abort();
}

#[cfg(unix)]
fn hold_lease_lock(cache: &mcp_server::WorkspaceCacheLayout) -> std::fs::File {
    use std::os::fd::AsRawFd;

    cache.ensure().unwrap();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(cache.lease_lock_path())
        .unwrap();
    assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    file
}

#[cfg(windows)]
fn hold_lease_lock(cache: &mcp_server::WorkspaceCacheLayout) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;

    cache.ensure().unwrap();
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .share_mode(0)
        .open(cache.lease_lock_path())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(any(unix, windows))]
async fn superseded_backend_lifecycle() {
    let src = TempDir::new().unwrap();
    let root = src.path().to_path_buf();
    let cache = mcp_server::WorkspaceCacheLayout::for_workspace(&root);
    let backend_root = root.clone();
    let backend_cache = cache.clone();
    let backend = tokio::spawn(broker::daemon::run(
        move || {
            Ok(McpServer::new(
                McpProfile::Workspace,
                SharedState::workspace_with_cache(backend_root, backend_cache)?,
            ))
        },
        key_for(&src),
        Duration::from_secs(30),
        Duration::from_secs(10),
    ));
    let stream = connect_within(&key_for(&src), Duration::from_secs(10)).await;
    let client = ().serve(stream).await.expect("active client initializes");

    let newer = SharedState::workspace_with_cache(root.clone(), cache.clone()).unwrap();
    let mut status = serde_json::Map::new();
    status.insert("action".to_owned(), serde_json::Value::String("status".to_owned()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        let answer = client
            .call_tool(CallToolRequestParams::new("metadata").with_arguments(status.clone()))
            .await
            .expect("the active session survives takeover and observes it");
        if answer.structured_content.as_ref().is_some_and(|body| body["owns_caches"] == false) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "backend never observed takeover");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    newer.shutdown();

    let mut schema = serde_json::Map::new();
    schema.insert("action".to_owned(), serde_json::Value::String("schema".to_owned()));
    client
        .call_tool(CallToolRequestParams::new("graph").with_arguments(schema))
        .await
        .expect("owner release still cannot sever the active session");
    client.cancel().await.ok();
    tokio::time::timeout(Duration::from_secs(6), backend)
        .await
        .expect("terminal backend exits before its ten-second idle TTL")
        .expect("backend task joined")
        .expect("backend exits cleanly");

    let transient_src = TempDir::new().unwrap();
    let transient_root = transient_src.path().to_path_buf();
    let transient_cache = mcp_server::WorkspaceCacheLayout::for_workspace(&transient_root);
    let lock = hold_lease_lock(&transient_cache);
    let build_root = transient_root.clone();
    let build_cache = transient_cache.clone();
    let transient_backend = tokio::spawn(broker::daemon::run(
        move || {
            Ok(McpServer::new(
                McpProfile::Workspace,
                SharedState::workspace_with_cache(build_root, build_cache)?,
            ))
        },
        key_for(&transient_src),
        Duration::from_secs(30),
        Duration::from_secs(10),
    ));
    let probe = connect_within(&key_for(&transient_src), Duration::from_secs(10)).await;
    drop(probe);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!transient_backend.is_finished(), "temporary UNCLAIMED must not terminate early");
    drop(lock);
    transient_backend.abort();
}

/// Once a backend has served real traffic, it stays warm only for `idle_ttl` after its
/// last session leaves, then exits on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(any(unix, windows))]
async fn backend_idles_out_after_ttl() {
    let src = TempDir::new().unwrap();
    let key = key_for(&src);

    // Long orphan grace (so a never-used exit can't be the cause) but a short idle TTL.
    let grace = Duration::from_secs(30);
    let idle_ttl = Duration::from_millis(500);
    let backend = tokio::spawn(broker::daemon::run(
        || Ok(reference_server()),
        key_for(&src),
        grace,
        idle_ttl,
    ));

    let s = connect_within(&key, Duration::from_secs(25)).await;
    let c = ().serve(s).await.expect("client initialized");
    assert!(c.peer_info().is_some(), "session saw server info");

    // Disconnect: `warmed` is set (initialize was sent), so the backend exits after the
    // short idle TTL, not after the much longer orphan grace.
    c.cancel().await.ok();
    let exited = tokio::time::timeout(Duration::from_secs(20), backend).await;
    assert!(exited.is_ok(), "warm backend idled out after its TTL once the last client left");
    exited.unwrap().expect("backend task joined").expect("backend run ok");
}

/// Regression: a connection that never sends MCP traffic (a liveness probe, or a proxy
/// that died before its first request) must NOT promote the backend to the long idle TTL.
/// Such a never-used backend exits via the short orphan grace instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(any(unix, windows))]
async fn silent_connection_does_not_warm_the_backend() {
    let src = TempDir::new().unwrap();
    let key = key_for(&src);

    // Short orphan grace, long idle TTL: if a silent connect wrongly warmed the backend it
    // would linger for the idle TTL and trip the timeout below.
    let grace = Duration::from_secs(3);
    let idle_ttl = Duration::from_secs(60);
    let backend = tokio::spawn(broker::daemon::run(
        || Ok(reference_server()),
        key_for(&src),
        grace,
        idle_ttl,
    ));

    // Connect and drop without sending a byte — the daemon accepts a session that carries
    // no MCP traffic, exactly like `probe_live`.
    let probe = connect_within(&key, Duration::from_secs(25)).await;
    drop(probe);

    // Must exit on the orphan grace (~3s), well before the 60s idle TTL.
    let exited = tokio::time::timeout(Duration::from_secs(20), backend).await;
    assert!(exited.is_ok(), "never-used backend must exit via the short orphan grace");
    exited.unwrap().expect("backend task joined").expect("backend run ok");
}

/// Regression: while the first backend is still doing its (slow) build, a second
/// launch for the same key must DEFER, not reclaim the socket — a bound-but-not-yet-
/// accepting backend must not look stale. And a client that connects mid-build must be
/// parked and served once the build completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(any(unix, windows))]
async fn second_launch_defers_while_first_is_still_building() {
    let src = TempDir::new().unwrap();
    let key = key_for(&src);

    // First backend: a deliberately slow build so the window "bound but not built" is
    // wide. The daemon must accept (and park) connections during this window.
    let slow_build = || {
        std::thread::sleep(Duration::from_secs(2));
        Ok(reference_server())
    };
    let first = tokio::spawn(broker::daemon::run(
        slow_build,
        key_for(&src),
        Duration::from_secs(15),
        Duration::from_secs(15),
    ));

    // A client connects during the build — the connect must succeed (backlog drained),
    // proving the socket is live (not stealable).
    let stream = connect_within(&key, Duration::from_secs(10)).await;

    // A second launch for the same key, while the first is still building, must defer
    // promptly. If it stole the socket it would serve its own loop and block until idle,
    // tripping this timeout.
    let second = broker::daemon::run(
        || Ok(reference_server()),
        key_for(&src),
        Duration::from_secs(15),
        Duration::from_secs(15),
    );
    tokio::time::timeout(Duration::from_secs(8), second)
        .await
        .expect("second launch defers while the first is building (no socket steal)")
        .expect("second launch ok");

    // The connection parked during the build is served once the build completes.
    let client = ().serve(stream).await.expect("parked session served after build");
    assert!(client.peer_info().is_some(), "parked session saw server info");
    client.cancel().await.ok();
    first.abort();
}

/// Regression for the Windows teardown bug: when the client closes its input (stdin
/// EOF in production), the proxy relay must end and the backend session must tear down.
/// On unix the stdin-EOF half-close delivers the backend an EOF promptly; on Windows
/// named pipes have no half-close, so the relay must bound its drain and drop the
/// connection to end the session. Without that bound the Windows relay would wait on a
/// backend that never closes and this test would hang until the timeout. With the session
/// gone the warm backend then idles out (a short idle TTL here makes that observable).
///
/// Drives the real [`broker::proxy::relay`] with in-memory client streams (standing in
/// for stdio) between an rmcp client and a live backend, exactly as `relay_stdio` wires
/// process stdio in production.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(any(unix, windows))]
async fn client_closing_input_ends_session_and_backend_idles_out() {
    let src = TempDir::new().unwrap();
    let key = key_for(&src);

    // Long orphan grace so the backend stays up through connect; short idle TTL so it
    // exits promptly once the warmed session ends.
    let grace = Duration::from_secs(30);
    let idle_ttl = Duration::from_secs(1);
    let backend = tokio::spawn(broker::daemon::run(
        || Ok(reference_server()),
        key_for(&src),
        grace,
        idle_ttl,
    ));

    // The backend connection the proxy relays to.
    let backend_stream = connect_within(&key, Duration::from_secs(25)).await;

    // In-memory stand-in for the proxy's stdio: the rmcp client drives `client_side`,
    // the relay pumps between `relay_side` and the backend just like `relay_stdio`.
    let (client_side, relay_side) = tokio::io::duplex(1024 * 1024);
    let (relay_in, relay_out) = tokio::io::split(relay_side);
    let relay = tokio::spawn(broker::proxy::relay(relay_in, relay_out, backend_stream));

    // A real MCP session over the relay: sends `initialize` (warming the backend) and
    // proves the relay is wired both ways.
    let client = ().serve(client_side).await.expect("client initialized through the relay");
    assert!(client.peer_info().is_some(), "session saw server info through the relay");

    // Client closes its end (production: the MCP client closes the proxy's stdin). The
    // relay must finish without hanging.
    client.cancel().await.ok();

    let relayed = tokio::time::timeout(Duration::from_secs(20), relay).await;
    assert!(relayed.is_ok(), "relay returned after the client closed its input (no hang)");
    relayed.unwrap().expect("relay task joined").expect("relay ok");

    // The session is gone; the warm backend idles out after its TTL.
    let exited = tokio::time::timeout(Duration::from_secs(20), backend).await;
    assert!(exited.is_ok(), "backend idled out after the last session ended");
    exited.unwrap().expect("backend task joined").expect("backend run ok");
}

/// Regression for the accept-vs-idle-expiry race: a client whose `connect` succeeds must
/// always be served, even if it lands exactly as the idle timer fires. The biased select
/// in the serve loop guarantees a backlogged connection is accepted before a simultaneous
/// idle tick can drop it — the proxy treats its successful connect as non-retryable.
/// Hammered around the idle boundary, every successful connect must complete its
/// initialize handshake; a backend that has already idled out merely refuses the connect
/// (retryable) and is relaunched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(any(unix, windows))]
async fn successful_connect_is_always_served_at_idle_boundary() {
    let src = TempDir::new().unwrap();
    let key = key_for(&src);

    let grace = Duration::from_secs(30);
    let idle_ttl = Duration::from_millis(200);
    let launch = || {
        tokio::spawn(broker::daemon::run(|| Ok(reference_server()), key_for(&src), grace, idle_ttl))
    };
    let mut backend = launch();

    let attempts = if cfg!(windows) { 1 } else { 10 };
    for _ in 0..attempts {
        // Sleep to around the idle boundary so the next connect races a possible expiry.
        tokio::time::sleep(idle_ttl).await;
        match connect(&key).await {
            // The connect succeeded → the connection is in the backlog → the backend MUST
            // serve it. A connection dropped on a racing expiry would fail this handshake.
            Ok(stream) => {
                let client = tokio::time::timeout(Duration::from_secs(20), ().serve(stream))
                    .await
                    .expect("serve did not hang")
                    .expect("a connect that succeeded must be served");
                assert!(client.peer_info().is_some(), "session saw server info");
                client.cancel().await.ok();
            }
            // The backend idled out before this connect — expected and retryable. Relaunch
            // so later iterations keep exercising the boundary.
            Err(_) => {
                if backend.is_finished() {
                    backend = launch();
                }
            }
        }
    }
    backend.abort();
}

/// M3 concurrency: many sessions sharing one workspace backend must serve in
/// parallel without deadlocking. Each session is an in-memory duplex pair fed to
/// `serve_stream` from one cloned `McpServer` — exactly how the daemon serves N
/// proxies from one `SharedState`. Every session fires a burst of calls that hit the
/// lazy loaders and shared mutexes (graph `ensure_loading`, diagnostics, search
/// status); the assertion is simply that none of them hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_backend_serves_concurrent_sessions_without_deadlock() {
    const SESSIONS: usize = 6;
    const ROUNDS: usize = 4;
    const ACTIONS: [(&str, &str); 5] = [
        ("graph", "schema"),
        ("graph", "status"),
        ("diagnostics", "catalog"),
        ("diagnostics", "schema"),
        ("search", "status"),
    ];

    let ws = TempDir::new().unwrap();
    let server = McpServer::new(
        McpProfile::Workspace,
        SharedState::workspace(ws.path().to_path_buf()).expect("valid workspace project"),
    );

    let mut clients = Vec::new();
    for _ in 0..SESSIONS {
        // The buffer must exceed the largest single response (the diagnostics catalog
        // is the biggest): an in-process duplex, unlike an OS socket, has no kernel
        // backpressure draining both directions independently, so a response larger
        // than the buffer would wedge the in-memory pipe — an artifact of this harness,
        // not of the socket transport the daemon actually uses.
        let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
        tokio::spawn(serve_stream(server.clone(), server_io));
        clients.push(Arc::new(().serve(client_io).await.expect("session initialized")));
    }

    let mut handles = Vec::new();
    for (si, client) in clients.iter().enumerate() {
        for round in 0..ROUNDS {
            for (tool, action) in ACTIONS {
                let client = Arc::clone(client);
                let mut arguments = serde_json::Map::new();
                arguments.insert("action".to_owned(), serde_json::Value::String(action.to_owned()));
                let label = format!("session{si}/round{round}/{tool}:{action}");
                handles.push(tokio::spawn(async move {
                    let call = client
                        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments));
                    match tokio::time::timeout(Duration::from_secs(20), call).await {
                        Ok(Ok(_)) => (label, "ok"),
                        Ok(Err(_)) => (label, "transport-err"),
                        Err(_) => (label, "HUNG"),
                    }
                }));
            }
        }
    }

    // A deadlock or lock-ordering hang surfaces as a per-call timeout. `is_error`
    // (e.g. "still indexing") is a valid response and not asserted against.
    let mut bad = Vec::new();
    for handle in handles {
        let (label, status) = handle.await.expect("session task did not panic");
        if status != "ok" {
            bad.push(format!("{label} => {status}"));
        }
    }
    assert!(
        bad.is_empty(),
        "calls did not complete cleanly under concurrency:\n{}",
        bad.join("\n")
    );
}
