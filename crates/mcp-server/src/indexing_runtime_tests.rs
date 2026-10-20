use super::*;
use crate::state::test_support::{env_lock, EnvVarGuard};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A hang watchdog for one case, enforced only by the timing profile.
const DEADLINE: Duration = test_utils::wait::ceiling(Duration::from_secs(30));
const INPUT_MARKER: &str = "IndexingRuntimeFixtureOnly";

// Like vector_lifecycle_tests, isolate process-global configuration. Other parallel
// workspace constructors do not all take env_lock before reading EMBEDDING_URL.
fn isolated(test: &str) -> bool {
    const CHILD: &str = "BSL_MCP_INDEXING_RUNTIME_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return false;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &format!("indexing_runtime_tests::{test}"), "--nocapture"])
        .env(CHILD, test)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The two fixture watchdogs together fit the 180-second acceptance budget. The budget is a
    // timing statement, held only by the timing profile; an ordinary run keeps a hang guard.
    let seconds = if test == "indexing_polling_smoke" { 150 } else { 30 };
    let started = std::time::Instant::now();
    let budget = test_utils::wait::hang_guard(Duration::from_secs(seconds));
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() >= budget {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("isolated runtime fixture exceeded {budget:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child must execute its exact fixture"
    );
    true
}

/// A joined, loopback-only provider. The gate makes the active phase observable without sleeps.
struct Provider {
    url: String,
    requests: Arc<AtomicUsize>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Provider {
    fn new(malformed: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, gate_worker, stop_worker) = (requests.clone(), gate.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stop_worker.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                };
                // Windows hands the listener's non-blocking mode to the accepted stream.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(test_utils::wait::ceiling(Duration::from_secs(2))))
                    .unwrap();
                stream
                    .set_write_timeout(Some(test_utils::wait::ceiling(Duration::from_secs(2))))
                    .unwrap();
                let mut bytes = Vec::new();
                let (header_end, length) = loop {
                    let mut buf = [0; 2048];
                    let n = stream.read(&mut buf).unwrap();
                    assert_ne!(n, 0, "complete request headers");
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .map(|n| n.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while bytes.len() < header_end + length {
                    let mut buf = [0; 2048];
                    let n = stream.read(&mut buf).unwrap();
                    assert_ne!(n, 0, "complete request body");
                    bytes.extend_from_slice(&buf[..n]);
                }
                let request: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                let inputs = request["input"].as_array().expect("embedding inputs");
                count.fetch_add(1, Ordering::SeqCst);
                let released = gate_worker.0.lock().unwrap();
                let (released, _) =
                    gate_worker.1.wait_timeout_while(released, DEADLINE, |v| !*v).unwrap();
                assert!(*released, "fixture gate deadline");
                let vector = if malformed { vec![1.0] } else { vec![1.0, 0.0, 0.0] };
                let data: Vec<_> = (0..inputs.len())
                    .map(|index| json!({"index": index, "embedding": vector}))
                    .collect();
                let body = json!({"data": data}).to_string();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { url, requests, gate, stop, thread: Some(thread) }
    }

    fn release(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.release();
        self.thread.take().unwrap().join().unwrap();
    }
}

struct Workspace {
    server: McpServer,
    _dir: tempfile::TempDir,
}

impl Workspace {
    fn new(empty: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Configuration.xml"),
            "<Configuration><Name>Fixture</Name></Configuration>",
        )
        .unwrap();
        if !empty {
            let module = dir.path().join("CommonModules/Fixture/Ext");
            std::fs::create_dir_all(&module).unwrap();
            std::fs::write(dir.path().join("CommonModules/Fixture.xml"), "<MetaDataObject><CommonModule><Properties><Name>Fixture</Name><Server>true</Server></Properties></CommonModule></MetaDataObject>").unwrap();
            std::fs::write(
                module.join("Module.bsl"),
                format!("Функция {INPUT_MARKER}() Экспорт\nВозврат 1;\nКонецФункции\n"),
            )
            .unwrap();
        }
        let state = SharedState::workspace(dir.path().to_path_buf()).unwrap();
        Self { server: McpServer::new(McpProfile::Workspace, state), _dir: dir }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

async fn status(server: &McpServer) -> serde_json::Value {
    tokio::time::timeout(
        DEADLINE,
        server.workspace_search(
            Parameters(serde_json::from_value(json!({"action": "status"})).unwrap()),
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await
    .expect("MCP status deadline")
    .expect("MCP status")
    .structured_content
    .expect("structured MCP status")
}

fn target<'a>(body: &'a serde_json::Value, kind: &str) -> &'a serde_json::Value {
    body["indexing"]["targets"]
        .as_array()
        .expect("indexing targets")
        .iter()
        .find(|t| t["kind"] == kind)
        .expect("required target")
}

async fn until(
    server: &McpServer,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let body = status(server).await;
            if predicate(&body) {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("indexing transition deadline")
}

fn provider_env(provider: &Provider) -> Vec<EnvVarGuard> {
    vec![
        EnvVarGuard::set("BSL_TEST_EMBEDDING", "1"),
        EnvVarGuard::set("EMBEDDING_URL", &provider.url),
        EnvVarGuard::set("EMBEDDING_MODEL", "test-model"),
        EnvVarGuard::set("EMBEDDING_DIM", "3"),
        EnvVarGuard::unset("EMBEDDING_API_KEY"),
        EnvVarGuard::unset("EMBEDDING_PROVIDER"),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(
    clippy::await_holding_lock,
    reason = "intentional contention fixture; other runtime workers serve requests while the test holds the engine and environment guards"
)]
async fn indexing_polling_smoke() {
    if isolated("indexing_polling_smoke") {
        return;
    }
    let _env = env_lock();
    let started = Instant::now();
    for malformed in [false, true] {
        tokio::time::timeout(DEADLINE, async {
            let provider = Provider::new(malformed);
            let _vars = provider_env(&provider);
            let workspace = Workspace::new(false);
            let active = until(&workspace.server, |body| {
                target(body, "lexical")["state"] == "ready"
                    && target(body, "semantic")["state"] == "running"
                    && provider.requests.load(Ordering::SeqCst) > 0
            })
            .await;
            assert!(target(&active, "semantic")["pass_id"].is_string());
            let engine = workspace.server.state.search_engine().clone();
            let held = (!malformed).then(|| engine.lock().unwrap());
            provider.release();
            if !malformed {
                let pending = until(&workspace.server, |body| {
                    target(body, "semantic")["phase"] == "persisting"
                })
                .await;
                assert_eq!(
                    target(&pending, "semantic")["state"],
                    "running",
                    "live installation is still blocked"
                );
                assert!(target(&pending, "semantic")["progress"].is_null());
            }
            drop(held);
            let expected = if malformed { "failed" } else { "ready" };
            let done =
                until(&workspace.server, |body| target(body, "semantic")["state"] == expected)
                    .await;
            assert_eq!(target(&done, "lexical")["state"], "ready");
            assert!(target(&done, "semantic")["progress"].is_null());
            if !malformed {
                let calls = provider.requests.load(Ordering::SeqCst);
                *provider.gate.0.lock().unwrap() = false;
                // Keep the HTTP connection open beyond the real 12-second query timeout.
                // Provider::drop releases the gate if this request or an assertion fails.
                let response = workspace
                    .server
                    .workspace_search(
                        Parameters(
                            serde_json::from_value(json!({
                                "action": "search_code", "query": INPUT_MARKER,
                            }))
                            .unwrap(),
                        ),
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                    .expect("query timeout preserves lexical fallback");
                provider.release();
                let fallback = response.structured_content.expect("structured lexical fallback");
                assert_eq!(
                    provider.requests.load(Ordering::SeqCst),
                    calls + 1,
                    "interactive query has one attempt and no health probe"
                );
                assert!(
                    !fallback["hits"].as_array().unwrap().is_empty(),
                    "lexical hit survives query timeout"
                );
                assert!(fallback["degraded"].is_string(), "degraded semantic modality");
                assert_eq!(
                    fallback["semantic_failure"]["code"], "embedding_timeout",
                    "the typed cause names the timeout: {fallback}"
                );
                assert_eq!(fallback["freshness"]["completeness"]["status"], "partial");
                assert_eq!(target(&fallback, "semantic")["state"], "ready");
                assert_eq!(target(&fallback, "lexical")["state"], "ready");
                assert!(target(&fallback, "semantic")["reason_code"].is_null());
                assert_eq!(target(&status(&workspace.server).await, "semantic")["state"], "ready");
            }
        })
        .await
        .expect("complete embedding case deadline");
    }
    {
        let provider = Provider::new(false);
        provider.release();
        let _vars = provider_env(&provider);
        let workspace = Workspace::new(true);
        until(&workspace.server, |body| target(body, "semantic")["state"] == "ready").await;
        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            0,
            "empty publication needs no vectors"
        );
    }
    {
        let _url = EnvVarGuard::unset("EMBEDDING_URL");
        let workspace = Workspace::new(false);
        until(&workspace.server, |body| {
            target(body, "lexical")["state"] == "ready"
                && target(body, "semantic")["state"] == "disabled"
        })
        .await;
    }
    if cfg!(timing_contract) {
        assert!(started.elapsed() <= Duration::from_secs(180));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(
    clippy::await_holding_lock,
    reason = "intentional contention fixture; other runtime workers serve requests while the test holds the engine and environment guards"
)]
async fn indexing_observation_bounds() {
    if isolated("indexing_observation_bounds") {
        return;
    }
    let _env = env_lock();
    let provider = Provider::new(false);
    provider.release();
    let _vars = provider_env(&provider);
    let workspace = Workspace::new(false);
    until(&workspace.server, |body| target(body, "semantic")["state"] == "ready").await;
    let calls = provider.requests.load(Ordering::SeqCst);
    let engine = workspace.server.state.search_engine().clone();
    let held = engine.lock().unwrap();
    let progress = workspace.server.state.index_progress().snapshot().unwrap();
    let body = status(&workspace.server).await;
    assert_eq!(target(&body, "lexical")["state"], "unknown");
    assert_eq!(target(&body, "semantic")["reason_code"], "snapshot_unavailable");
    drop(held);
    let runtime = workspace.server.state.semantic_runtime();
    let held = runtime.lock().unwrap();
    let contended = serde_json::to_value(workspace.server.state.workspace_indexing()).unwrap();
    assert!(contended["targets"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["state"] == "unknown" && t["progress"].is_null()));
    drop(held);
    for _ in 0..5 {
        let body = status(&workspace.server).await;
        let wire = body["indexing"].to_string();
        assert!(wire.len() < 1024);
        assert!(!wire.contains(&provider.url));
        assert!(!wire.contains(workspace._dir.path().to_str().unwrap()));
        assert!(!wire.contains(INPUT_MARKER));
        assert_eq!(
            workspace.server.state.index_progress().snapshot().unwrap().pass_id,
            progress.pass_id,
            "polling does not start another build"
        );
    }
    assert_eq!(provider.requests.load(Ordering::SeqCst), calls, "polling makes no provider calls");
    *runtime.lock().unwrap() = crate::state::SemanticRuntimeStatus::Failed(format!(
        "fixture-secret {} {} Проверка",
        provider.url,
        workspace._dir.path().display()
    ));
    let failed = status(&workspace.server).await;
    assert_eq!(target(&failed, "semantic")["reason_code"], "native_failure");
    let wire = failed["indexing"].to_string();
    assert!(!wire.contains("fixture-secret"));
    assert!(!wire.contains(&provider.url));
    assert!(!wire.contains(workspace._dir.path().to_str().unwrap()));
}
