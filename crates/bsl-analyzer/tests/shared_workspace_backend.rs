mod common;

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::mpsc::{self, Receiver},
    thread::JoinHandle,
    time::Duration,
};

use common::Lsp;
use rmcp::{model::CallToolRequestParams, service::RunningService, RoleClient, ServiceExt};
use serde_json::{json, Map, Value};
use tokio::process::{Child, Command};

type Client = RunningService<RoleClient, ()>;

static PROTOCOL_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvGuard {
    fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        if !self.0.iter().any(|(saved, _)| *saved == key) {
            self.0.push((key, std::env::var_os(key)));
        }
        std::env::set_var(key, value);
    }

    fn remove(&mut self, key: &'static str) {
        if !self.0.iter().any(|(saved, _)| *saved == key) {
            self.0.push((key, std::env::var_os(key)));
        }
        std::env::remove_var(key);
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, previous) in self.0.drain(..) {
            if let Some(value) = previous {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

const CALLER: &str =
    include_str!("fixtures/shared_workspace_analysis/CommonModules/Потребитель/Ext/Module.bsl");

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("disposable source workspace");
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shared_workspace_analysis");
    for entry in walkdir::WalkDir::new(&fixture_root)
        .into_iter()
        .filter_entry(|entry| entry.path() != fixture_root.join("perf-shadow"))
    {
        let entry = entry.expect("fixture entry");
        let target = root
            .path()
            .join(entry.path().strip_prefix(&fixture_root).expect("fixture-relative path"));
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(target).expect("fixture directory");
        } else {
            std::fs::create_dir_all(target.parent().expect("fixture parent"))
                .expect("fixture directory");
            std::fs::copy(entry.path(), target).expect("fixture file");
        }
    }
    root
}

struct FailureArtifacts {
    root: PathBuf,
    runtime: PathBuf,
    attach_order: &'static str,
    phase: String,
}

impl FailureArtifacts {
    fn checkpoint(&mut self, phase: &str) {
        self.phase = phase.to_owned();
        eprintln!("shared backend test: attach_order={} phase={}", self.attach_order, self.phase);
    }
}

fn copy_shared_backend_logs(runtime: &Path, destination: &Path) -> (Vec<String>, Vec<String>) {
    let mut copied = Vec::new();
    let mut errors = Vec::new();
    for entry in walkdir::WalkDir::new(runtime).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        let is_shared_log = entry.file_type().is_file()
            && entry.file_name().to_string_lossy().starts_with("shared-")
            && path.extension().is_some_and(|extension| extension == "log");
        if !is_shared_log {
            continue;
        }

        let Ok(relative) = path.strip_prefix(runtime) else {
            errors.push(format!("log is outside runtime directory: {}", path.display()));
            continue;
        };
        let output = destination.join("runtime-logs").join(relative);
        if let Some(parent) = output.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                errors.push(format!("{}: {error}", output.display()));
                continue;
            }
        }
        match std::fs::copy(path, &output) {
            Ok(_) => copied.push(relative.to_string_lossy().into_owned()),
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    (copied, errors)
}

impl Drop for FailureArtifacts {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }

        let test_run_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let destination = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/issue420-implementation/protocol-failures")
            .join(format!("{}-{test_run_id}", std::process::id()));
        if let Err(error) = std::fs::create_dir_all(&destination) {
            eprintln!("failed to create protocol failure artifacts: {error}");
            return;
        }

        let mut copied = Vec::new();
        for (source, name) in [
            (self.root.join("bsl-analyzer.toml"), "bsl-analyzer.toml"),
            (self.root.join(".env"), ".env"),
            (
                self.root.join("CommonModules/Потребитель/Ext/Module.bsl"),
                "caller-Module.bsl",
            ),
            (
                self.root.join("CommonModules/ОбщийМодуль/Ext/Module.bsl"),
                "target-current-Module.bsl",
            ),
            (
                Path::new(env!("CARGO_MANIFEST_DIR")).join(
                    "tests/fixtures/shared_workspace_analysis/CommonModules/ОбщийМодуль/Ext/Module.bsl",
                ),
                "target-fixture-Module.bsl",
            ),
        ] {
            if source.is_file() {
                match std::fs::copy(&source, destination.join(name)) {
                    Ok(_) => copied.push(name.to_owned()),
                    Err(error) => eprintln!("failed to preserve {name}: {error}"),
                }
            }
        }

        let (daemon_logs, log_errors) = copy_shared_backend_logs(&self.runtime, &destination);
        copied.extend(daemon_logs);
        for error in log_errors {
            eprintln!("failed to preserve daemon log: {error}");
        }

        let details = format!(
            "attach_order={}\nphase={}\nprocess_id={}\n",
            self.attach_order,
            self.phase,
            std::process::id()
        );
        if let Err(error) = std::fs::write(destination.join("failure.txt"), details) {
            eprintln!("failed to write failure details: {error}");
        }
        eprintln!(
            "shared backend test failed; preserved synthetic diagnostics at {} (files: {})",
            destination.display(),
            copied.join(", ")
        );
    }
}

async fn mcp_client(root: &Path) -> (Client, Child) {
    mcp_client_mode(root, "broker").await
}

async fn mcp_client_mode(root: &Path, mode: &str) -> (Client, Child) {
    mcp_client_with_args(root, mode, &[]).await
}

async fn mcp_client_with_args(root: &Path, mode: &str, extra: &[&str]) -> (Client, Child) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
    command
        .args(["mcp", "serve", "--profile", "workspace", "--mode", mode, "-s"])
        .arg(root)
        .args(extra);
    let mut child = command
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start real MCP broker proxy subprocess");
    let stdout = child.stdout.take().expect("MCP stdout");
    let stdin = child.stdin.take().expect("MCP stdin");
    let pid = child.id().unwrap_or_default();
    let client = match tokio::time::timeout(
        test_utils::wait::hang_guard(Duration::from_secs(60)),
        ().serve((stdout, stdin)),
    )
    .await
    {
        Ok(Ok(client)) => client,
        Ok(Err(error)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!("public MCP initialize failed for subprocess pid={pid}: {error}");
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!("public MCP initialize timed out after 60s for subprocess pid={pid}");
        }
    };
    (client, child)
}

/// One-request loopback HTTP endpoint standing in for the platform. Each session gets its own
/// endpoint and result, so this proves the broker dispatches the native context supplied by that
/// particular MCP client rather than credentials retained on the shared daemon.
fn onec_validation_mock(valid: bool) -> (String, Receiver<String>, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind disposable OneC mock");
    listener.set_nonblocking(true).expect("bound mock accept wait");
    let address = listener.local_addr().expect("mock address");
    let (request_tx, request_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let deadline =
            std::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(30));
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "mock did not receive a native validation request before its deadline"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept native validation request: {error}"),
            }
        };
        // Windows hands the listener's non-blocking mode to the accepted stream.
        stream.set_nonblocking(false).expect("blocking mock request read");
        stream
            .set_read_timeout(Some(test_utils::wait::ceiling(Duration::from_secs(10))))
            .expect("bound mock request read");
        let request = read_http_request(&mut stream).expect("read native validation request");
        let authorization = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization").then(|| value.trim().to_owned())
            })
            .unwrap_or_default();
        let body = if valid {
            r#"{"valid":true,"errors":["from-session-a"]}"#
        } else {
            r#"{"valid":false,"errors":["from-session-b"]}"#
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("write mock validation response");
        request_tx.send(authorization).expect("deliver mock request receipt");
    });
    (format!("http://{address}"), request_rx, worker)
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find_map(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                return String::from_utf8(bytes)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error));
            }
        }
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

struct FileBarrierRelease {
    path: PathBuf,
    released: bool,
}

impl FileBarrierRelease {
    fn new(path: PathBuf) -> Self {
        Self { path, released: false }
    }

    fn release(&mut self) {
        std::fs::write(&self.path, "release").expect("release configured helper barrier");
        self.released = true;
    }
}

impl Drop for FileBarrierRelease {
    fn drop(&mut self) {
        if !self.released {
            let _ = std::fs::write(&self.path, "release");
        }
    }
}

fn cache_contains_file_named(cache: &Path, name: &str) -> bool {
    if !cache.exists() {
        return false;
    }
    walkdir::WalkDir::new(cache)
        .into_iter()
        .map(|entry| entry.expect("walk disposable workspace cache"))
        .any(|entry| entry.file_type().is_file() && entry.file_name().to_string_lossy() == name)
}

async fn mcp_client_mode_with_synthetic_stderr(root: &Path, mode: &str) -> (Client, Child) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["mcp", "serve", "--profile", "workspace", "--mode", mode, "-s"])
        .arg(root)
        .current_dir(root)
        .env_remove("BSL_ONEC_PASSWORD")
        .env_remove("BSL_ONEC_CONNECTIONS_FILE")
        .env_remove("EMBEDDING_API_KEY")
        .env_remove("EMBEDDING_URL")
        .env_remove("EMBEDDING_MODEL")
        .env_remove("EMBEDDING_PROVIDER")
        .env_remove("EMBEDDING_QUERY_PREFIX")
        .env_remove("EMBEDDING_DOCUMENT_PREFIX")
        .env_remove("BSL_LOG_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start MCP subprocess on the synthetic fixture");
    let stdout = child.stdout.take().expect("MCP stdout");
    let stdin = child.stdin.take().expect("MCP stdin");
    let stderr = child.stderr.take().expect("synthetic MCP stderr");
    let stderr_reader = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut output = Vec::new();
        stderr
            .take(64 * 1024)
            .read_to_end(&mut output)
            .await
            .expect("read bounded synthetic stderr");
        String::from_utf8_lossy(&output).into_owned()
    });
    match ().serve((stdout, stdin)).await {
        Ok(client) => (client, child),
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let stderr = stderr_reader.await.expect("join synthetic stderr capture");
            panic!("MCP initialize failed on synthetic fixture: {error}; stderr:\n{stderr}");
        }
    }
}

fn isolated_backend_env(cache: &Path, runtime: &Path) -> EnvGuard {
    let mut env = EnvGuard::default();
    env.set("BSL_CACHE_DIR", cache);
    env.set("BSL_SHARED_BACKEND_MARKERS", "1");
    env.set("BSL_LOG", "info");
    env.set("BSL_MCP_DAEMON_LOG", runtime.join("shared-daemon.log"));
    env.set("BSL_MCP_IDLE_TTL_SECS", "1");
    env.set("BSL_MCP_ORPHAN_GRACE_SECS", "1");
    env.set("XDG_RUNTIME_DIR", runtime);
    env.set("TMP", runtime);
    env.set("TEMP", runtime);
    env.set("TMPDIR", runtime);
    env
}

async fn wait_for_owner_initializers(runtime: &Path, count: usize) -> Vec<String> {
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(60));
    loop {
        let markers = backend_markers(runtime);
        let starts = markers.iter().filter(|line| line.contains("owner_init_start")).count();
        if starts >= count {
            return markers;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {count} owner initializers, found {starts}: {markers:?}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

async fn wait_for_lsp_attach_in_epoch(runtime: &Path, epoch: &str) -> Vec<String> {
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(30));
    loop {
        let markers = backend_markers(runtime);
        if markers.iter().any(|line| {
            line.contains("session_attached")
                && trace_field(line, "owner_epoch") == Some(epoch)
                && trace_field(line, "adapter") == Some("lsp")
        }) {
            return markers;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "LSP did not attach to owner epoch {epoch}: {markers:?}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn owner_epochs(markers: &[String]) -> Vec<&str> {
    let mut epochs = markers
        .iter()
        .filter(|line| line.contains("owner_init_start"))
        .filter_map(|line| trace_field(line, "owner_epoch"))
        .collect::<Vec<_>>();
    epochs.sort_unstable();
    epochs.dedup();
    epochs
}

async fn wait_for_resident_bootstrap(runtime: &Path) -> Vec<String> {
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(120));
    loop {
        let markers = backend_markers(runtime);
        if markers.iter().any(|line| line.contains("resident_bootstrap_complete")) {
            return markers;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "resident bootstrap did not complete: {markers:?}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn kill_exact_test_daemon(pid: u32) {
    let pid_text = pid.to_string();
    #[cfg(unix)]
    let status = StdCommand::new("kill").args(["-KILL", pid_text.as_str()]).status();
    #[cfg(windows)]
    let status = StdCommand::new("taskkill").args(["/PID", pid_text.as_str(), "/F"]).status();
    #[cfg(not(any(unix, windows)))]
    let status: std::io::Result<std::process::ExitStatus> =
        Err(std::io::Error::other("exact PID termination is not implemented on this OS"));
    let status = status.expect("terminate the exact daemon PID from the isolated test marker");
    assert!(status.success(), "exact daemon PID {pid} termination failed: {status}");
}

async fn disk_symbol(client: &Client) -> Value {
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(120));
    loop {
        let response = tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            client.call_tool(CallToolRequestParams::new("symbol_info").with_arguments(
                Map::from_iter([("symbol".to_owned(), json!("ОбщийМодуль.ЧисловоеЗначение"))]),
            )),
        )
        .await
        .expect("MCP symbol_info deadline")
        .expect("MCP symbol_info call");
        let card = response.structured_content.expect("MCP symbol_info result");
        match card["status"].as_str() {
            Some("loading") => {}
            Some(_) => return card,
            None => panic!("MCP symbol_info has no status: {card}"),
        }
        assert!(tokio::time::Instant::now() < deadline, "MCP symbol_info stayed loading");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

async fn wait_disk_type(client: &Client, expected: &str) -> Value {
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(90));
    loop {
        let card = disk_symbol(client).await;
        if card["status"] == "ok" && card["return_type"] == expected {
            return card;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "disk type never became {expected}: {card}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

async fn wait_disk_status(client: &Client, expected: &str) -> Value {
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(90));
    loop {
        let card = disk_symbol(client).await;
        if card["status"] == expected {
            return card;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "disk status never became {expected}: {card}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn watched_file(lsp: &mut Lsp, path: &Path, kind: u8) {
    lsp.send(json!({
        "jsonrpc": "2.0",
        "method": "workspace/didChangeWatchedFiles",
        "params": {"changes": [{"uri": lsp_types::Url::from_file_path(path).unwrap(), "type": kind}]}
    }));
}

fn assert_location_file(location: &Value, expected: &Path, message: &str) {
    let uri =
        lsp_types::Url::parse(location["uri"].as_str().expect("definition file URI")).unwrap();
    let actual = uri.to_file_path().expect("definition URI is a file").canonicalize().unwrap();
    assert_eq!(actual, expected.canonicalize().unwrap(), "{message}");
}

fn backend_markers(runtime: &Path) -> Vec<String> {
    let Ok(log) = std::fs::read_to_string(runtime.join("shared-daemon.log")) else {
        return Vec::new();
    };
    log.lines()
        .filter(|line| {
            line.contains("owner_init_start")
                || line.contains("owner_init_complete")
                || line.contains("resident_bootstrap_start")
                || line.contains("resident_bootstrap_complete")
                || line.contains("resident_bootstrap_failed")
                || line.contains("workspace_ingress_created")
                || line.contains("session_attached")
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn failure_artifact_copy_finds_nested_shared_logs_and_skips_unrelated_files() {
    let runtime = tempfile::tempdir().expect("disposable runtime tree");
    let destination = tempfile::tempdir().expect("disposable artifact destination");
    let private = runtime.path().join("private/nested");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(private.join("shared-example.log"), "synthetic daemon log\n").unwrap();
    std::fs::write(private.join("unrelated.log"), "must not be copied\n").unwrap();
    std::fs::write(private.join("shared-example.txt"), "must not be copied\n").unwrap();

    let (copied, errors) = copy_shared_backend_logs(runtime.path(), destination.path());
    assert!(errors.is_empty(), "no copy failures: {errors:?}");
    assert_eq!(copied.len(), 1);
    let copied_components: Vec<_> = Path::new(&copied[0])
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert_eq!(copied_components, ["private", "nested", "shared-example.log"]);
    assert_eq!(
        std::fs::read_to_string(
            destination.path().join("runtime-logs/private/nested/shared-example.log")
        )
        .unwrap(),
        "synthetic daemon log\n"
    );
    assert!(!destination.path().join("runtime-logs/private/nested/unrelated.log").exists());
    assert!(!destination.path().join("runtime-logs/private/nested/shared-example.txt").exists());
}

fn trace_field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .map(|value| value.trim_matches(|character| character == '"' || character == ','))
}

fn is_scope_retirement_event(
    message: &Value,
    client_request_id: u64,
    old_server_request_id: u64,
    excluded_uri: &str,
) -> bool {
    (message["id"] == client_request_id && message["method"].is_null())
        || (message["method"] == "$/cancelRequest"
            && message["params"]["id"] == old_server_request_id)
        || (message["method"] == "$/progress"
            && message["params"]["token"] == "scope-work"
            && message["params"]["value"]["kind"] == "end")
        || (message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == excluded_uri
            && message["params"]["diagnostics"].as_array().is_some_and(Vec::is_empty))
        || (message["method"] == "workspace/semanticTokens/refresh"
            && message["id"].is_number()
            && message["id"].as_u64().is_some_and(|id| id > old_server_request_id))
}

fn collect_scope_retirement_events(
    editor: &Lsp,
    client_request_id: u64,
    old_server_request_id: u64,
    excluded_uri: &str,
) -> Vec<Value> {
    let mut events = Vec::new();
    let mut terminal = false;
    let mut cancellation = false;
    let mut progress_end = false;
    let mut excluded_diagnostics = false;
    let mut rebound_refresh = false;
    while !(terminal && cancellation && progress_end && excluded_diagnostics && rebound_refresh) {
        let message = editor.wait_for(|message| {
            is_scope_retirement_event(
                message,
                client_request_id,
                old_server_request_id,
                excluded_uri,
            )
        });
        terminal |= message["id"] == client_request_id && message["method"].is_null();
        cancellation |= message["method"] == "$/cancelRequest"
            && message["params"]["id"] == old_server_request_id;
        progress_end |= message["method"] == "$/progress"
            && message["params"]["token"] == "scope-work"
            && message["params"]["value"]["kind"] == "end";
        excluded_diagnostics |= message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == excluded_uri
            && message["params"]["diagnostics"].as_array().is_some_and(Vec::is_empty);
        rebound_refresh |= message["method"] == "workspace/semanticTokens/refresh"
            && message["id"].as_u64().is_some_and(|id| id > old_server_request_id);
        events.push(message);
    }
    events
}

fn request_definition_until_ready(
    editor: &mut Lsp,
    first_id: u64,
    uri: &lsp_types::Url,
    line: u64,
    character: usize,
) -> (Value, u64) {
    request_until_ready(
        editor,
        first_id,
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        }),
        "definition",
    )
}

fn request_hover_until_ready(
    editor: &mut Lsp,
    first_id: u64,
    uri: &lsp_types::Url,
    line: u64,
    character: usize,
) -> (Value, u64) {
    request_until_ready(
        editor,
        first_id,
        "textDocument/hover",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        }),
        "hover",
    )
}

fn request_until_ready(
    editor: &mut Lsp,
    first_id: u64,
    method: &str,
    params: Value,
    operation: &str,
) -> (Value, u64) {
    let deadline =
        std::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(45));
    let mut id = first_id;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "the editor view never became ready for {operation}");
        editor.send(json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        let response = editor
            .wait_for_within(remaining, |message| {
                message["id"] == id
                    && message["method"].is_null()
                    && (message.get("result").is_some() || message.get("error").is_some())
            })
            .unwrap_or_else(|| {
                panic!("{operation} request returns before the bounded view-ready deadline")
            });
        if response["error"]["code"] == -32801 {
            assert!(
                std::time::Instant::now() < deadline,
                "the {operation} request remained ContentModified: {response}"
            );
            id += 1;
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        return (response, id);
    }
}

async fn wait_for_backend_markers(runtime: &Path) -> Vec<String> {
    // A hang guard, not a latency bound: an adapter that never attaches fails with the markers
    // it left instead of holding the runner.
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(30));
    loop {
        let markers = backend_markers(runtime);
        if markers.iter().filter(|line| line.contains("session_attached")).count() >= 3
            && markers.iter().any(|line| line.contains("resident_bootstrap_complete"))
        {
            return markers;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "shared backend markers missing: {markers:?}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn assert_single_owner(markers: &[String]) {
    assert_owner_counts(markers, 1, 2);
}

fn assert_owner_counts(markers: &[String], mcp_count: usize, lsp_count: usize) {
    let starts: Vec<_> = markers.iter().filter(|line| line.contains("owner_init_start")).collect();
    let completes: Vec<_> =
        markers.iter().filter(|line| line.contains("owner_init_complete")).collect();
    let attaches: Vec<_> =
        markers.iter().filter(|line| line.contains("session_attached")).collect();
    let resident_starts: Vec<_> =
        markers.iter().filter(|line| line.contains("resident_bootstrap_start")).collect();
    let resident_completes: Vec<_> =
        markers.iter().filter(|line| line.contains("resident_bootstrap_complete")).collect();
    let ingress: Vec<_> =
        markers.iter().filter(|line| line.contains("workspace_ingress_created")).collect();
    assert_eq!(starts.len(), 1, "one shared-state initializer starts: {markers:#?}");
    assert_eq!(completes.len(), 1, "one shared-state initializer completes: {markers:#?}");
    assert_eq!(
        attaches.len(),
        mcp_count + lsp_count,
        "all adapters attach to the same backend: {markers:#?}"
    );
    assert_eq!(resident_starts.len(), 1, "one disk resident bootstrap starts: {markers:#?}");
    assert_eq!(resident_completes.len(), 1, "one disk resident bootstrap completes: {markers:#?}");
    assert_eq!(ingress.len(), 1, "one shared change ingress is created: {markers:#?}");
    assert!(
        !markers.iter().any(|line| line.contains("resident_bootstrap_failed")),
        "shared disk resident bootstrap succeeds: {markers:#?}"
    );

    let epoch = trace_field(starts[0], "owner_epoch").expect("initializer epoch");
    let init_id = trace_field(starts[0], "owner_init_id").expect("initializer id");
    assert_eq!(trace_field(completes[0], "owner_epoch"), Some(epoch));
    assert_eq!(trace_field(completes[0], "owner_init_id"), Some(init_id));
    assert_eq!(trace_field(resident_starts[0], "owner_epoch"), Some(epoch));
    assert_eq!(trace_field(resident_completes[0], "owner_epoch"), Some(epoch));
    assert_eq!(
        trace_field(resident_starts[0], "storage_id"),
        trace_field(resident_completes[0], "storage_id")
    );
    assert_eq!(trace_field(resident_completes[0], "resident_bootstrap_count"), Some("1"));
    assert_eq!(trace_field(ingress[0], "owner_epoch"), Some(epoch));
    assert_eq!(trace_field(ingress[0], "hub_count"), Some("1"));
    let adapters: Vec<_> = attaches
        .iter()
        .map(|line| {
            assert_eq!(trace_field(line, "owner_epoch"), Some(epoch), "{line}");
            trace_field(line, "adapter").expect("attached adapter")
        })
        .collect();
    assert_eq!(adapters.iter().filter(|adapter| **adapter == "mcp").count(), mcp_count);
    assert_eq!(adapters.iter().filter(|adapter| **adapter == "lsp").count(), lsp_count);
}

async fn search_code_hits(client: &Client, query: &str) -> Value {
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(120));
    loop {
        let response = client
            .call_tool(CallToolRequestParams::new("search").with_arguments(Map::from_iter([
                ("action".to_owned(), json!("search_code")),
                ("query".to_owned(), json!(query)),
                ("limit".to_owned(), json!(20)),
            ])))
            .await
            .expect("workspace code search call");
        let body = response.structured_content.expect("search structured content");
        if body["status"] != "not_ready" {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "workspace search did not finish indexing before its deadline"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn graph_resolve_answer(client: &Client, query: &str) -> Value {
    let deadline =
        tokio::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(120));
    loop {
        let response = client
            .call_tool(CallToolRequestParams::new("graph").with_arguments(Map::from_iter([
                ("action".to_owned(), json!("resolve")),
                ("query".to_owned(), json!(query)),
            ])))
            .await
            .expect("graph resolve call");
        let body = response.structured_content.expect("graph resolve structured content");
        let result = body["result"].clone();
        let graph_answered = result["providers"].as_array().is_some_and(|providers| {
            providers
                .iter()
                .any(|provider| provider["provider"] == "graph" && provider["state"] == "answered")
        });
        if graph_answered {
            return result;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "persistent graph source did not become ready before its deadline"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn hover_type(
    lsp: &mut Lsp,
    next_request_id: &mut u64,
    caller: &Path,
    expected: &str,
    stale: &str,
) {
    let uri = lsp_types::Url::from_file_path(caller).expect("caller URI");
    let deadline =
        std::time::Instant::now() + test_utils::wait::hang_guard(Duration::from_secs(60));
    loop {
        let request_id = *next_request_id;
        *next_request_id += 1;
        let response = lsp.request(
            request_id,
            "textDocument/hover",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": 2, "character": 8}
            }),
        );
        if !response["error"].is_null() {
            assert!(
                matches!(response["error"]["code"].as_i64(), Some(-32800 | -32801)),
                "only a view-change cancellation may be retried: {response}"
            );
            assert!(std::time::Instant::now() < deadline, "LSP hover did not reach {expected}");
            std::thread::sleep(Duration::from_millis(40));
            continue;
        }
        let contents = response["result"].to_string();
        if contents.contains(expected) {
            return;
        }
        assert!(
            contents.contains(stale),
            "unexpected hover while waiting for {expected}: {response}"
        );
        assert!(std::time::Instant::now() < deadline, "LSP hover did not reach {expected}");
        std::thread::sleep(Duration::from_millis(40));
    }
}

async fn run_attach_order(mcp_first: bool, race: bool) {
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable workspace cache");
    let runtime = tempfile::tempdir().expect("disposable broker runtime directory");
    let attach_order = if race {
        "concurrent"
    } else if mcp_first {
        "mcp-first"
    } else {
        "lsp-first"
    };
    let mut failure_artifacts = FailureArtifacts {
        root: root.path().to_path_buf(),
        runtime: runtime.path().to_path_buf(),
        attach_order,
        phase: "setup".to_owned(),
    };
    let mut env = isolated_backend_env(cache.path(), runtime.path());
    for key in [
        "EMBEDDING_API_KEY",
        "EMBEDDING_URL",
        "EMBEDDING_MODEL",
        "EMBEDDING_PROVIDER",
        "EMBEDDING_QUERY_PREFIX",
        "EMBEDDING_DOCUMENT_PREFIX",
        "BSL_ONEC_PASSWORD",
        "BSL_ONEC_CONNECTIONS_FILE",
    ] {
        env.remove(key);
    }
    let caller = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    let target = root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
    let saved_disk = std::fs::read_to_string(&target).expect("fixture's saved target");
    std::fs::write(
        &target,
        format!(
            "{saved_disk}\nФункция DiskPersistentMarker420() Экспорт\n    Возврат 420;\nКонецФункции\n"
        ),
    )
    .expect("add a saved positive-control marker to the disposable workspace");
    let expected_disk =
        std::fs::read(&target).expect("saved target with the positive-control marker");

    let (mut mcp, mut mcp_process, mut editor_a) = if race {
        let lsp_root = root.path().to_path_buf();
        let (attached, editor) = tokio::join!(
            mcp_client(root.path()),
            tokio::task::spawn_blocking(move || Lsp::start(&lsp_root)),
        );
        let (client, process) = attached;
        (Some(client), Some(process), editor.expect("racing LSP subprocess"))
    } else if mcp_first {
        let (client, process) = mcp_client(root.path()).await;
        (Some(client), Some(process), Lsp::start(root.path()))
    } else {
        (None, None, Lsp::start(root.path()))
    };
    failure_artifacts.checkpoint("first adapter attached");
    let diagnostics = editor_a.open(&caller, CALLER);
    assert!(
        diagnostics["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "UnresolvedBareCall"),
        "editor A receives its known diagnostic: {diagnostics}"
    );
    if mcp.is_none() {
        let (client, process) = mcp_client(root.path()).await;
        mcp = Some(client);
        mcp_process = Some(process);
    }
    failure_artifacts.checkpoint("LSP editor A and MCP attached");
    let mut editor_b = Lsp::start(root.path());
    let diagnostics_b = editor_b.open(&caller, CALLER);
    assert!(
        diagnostics_b["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "UnresolvedBareCall"),
        "editor B receives its known diagnostic: {diagnostics_b}"
    );
    failure_artifacts.checkpoint("LSP editor B attached and diagnostic received");

    editor_a.open(
        &target,
        "Функция ЧисловоеЗначение() Экспорт\n    Возврат Истина;\nКонецФункции\n\
             Функция EditorOnlyMarkerSessionA() Экспорт\n    Возврат 1;\nКонецФункции\n",
    );
    editor_b.open(
        &target,
        "Функция ЧисловоеЗначение() Экспорт\n    Возврат \"editor-B\";\nКонецФункции\n\
         Функция EditorOnlyMarkerSessionB() Экспорт\n    Возврат 2;\nКонецФункции\n",
    );
    let a_uri = lsp_types::Url::from_file_path(&caller).unwrap();
    let b_uri = a_uri.clone();
    let a_hover = editor_a.request(
        2,
        "textDocument/hover",
        json!({"textDocument": {"uri": a_uri}, "position": {"line": 2, "character": 8}}),
    );
    assert!(a_hover["error"].is_null(), "editor A hover: {a_hover}");
    assert!(a_hover["result"].to_string().contains("Булево"), "A owns boolean view: {a_hover}");
    let definition = editor_a.request(
        3,
        "textDocument/definition",
        json!({"textDocument": {"uri": a_uri.clone()}, "position": {"line": 2, "character": 27}}),
    );
    let location = definition["result"].as_array().map_or(&definition["result"], |items| &items[0]);
    assert!(definition["error"].is_null(), "LSP definition: {definition}");
    assert_eq!(
        location["uri"],
        lsp_types::Url::from_file_path(&target).unwrap().as_str(),
        "LSP resolves the known disk definition"
    );
    let b_hover = editor_b.request(
        2,
        "textDocument/hover",
        json!({"textDocument": {"uri": b_uri}, "position": {"line": 2, "character": 8}}),
    );
    assert!(b_hover["error"].is_null(), "editor B hover: {b_hover}");
    assert!(b_hover["result"].to_string().contains("Строка"), "B owns string view: {b_hover}");

    let mcp_ref = mcp.as_ref().expect("MCP client attached");
    let mut editor_a_next_id = 4;
    let mut editor_b_next_id = 3;
    let disk = wait_disk_type(mcp_ref, "Число").await;
    assert_eq!(disk["status"], "ok", "MCP sees the disk symbol: {disk}");
    assert_eq!(disk["return_type"], "Число", "MCP keeps the saved numeric view: {disk}");
    assert!(
        disk["definitions"][0]["location"]["path"]
            .as_str()
            .unwrap()
            .ends_with("ОбщийМодуль/Ext/Module.bsl"),
        "MCP resolves the saved definition: {disk}"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        expected_disk,
        "attaching editors never writes their buffers to disk"
    );
    let saved_search = search_code_hits(mcp_ref, "DiskPersistentMarker420").await;
    assert_eq!(saved_search["freshness"]["source"], "search-index");
    let saved_hits = saved_search["hits"].as_array().expect("ready search hits");
    assert!(
        saved_hits.iter().any(|hit| {
            hit["path"].as_str().is_some_and(|path| {
                Path::new(path).ends_with(Path::new("ОбщийМодуль").join("Ext").join("Module.bsl"))
            })
        }),
        "the saved positive-control marker is present in the disk search index: {saved_search}"
    );
    let saved_graph = graph_resolve_answer(mcp_ref, "DiskPersistentMarker420").await;
    assert!(
        saved_graph["candidates"].as_array().is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate["address"]["graph_id"].is_string()
                    && candidate["display"]
                        .as_str()
                        .is_some_and(|name| name.contains("DiskPersistentMarker420"))
            })
        }),
        "the saved positive-control marker is present in the persistent graph index"
    );
    for marker in ["EditorOnlyMarkerSessionA", "EditorOnlyMarkerSessionB"] {
        let hits = search_code_hits(mcp_ref, marker).await;
        assert_eq!(
            hits["hits"].as_array().map(Vec::len),
            Some(0),
            "persistent disk search excludes unsaved editor-only marker {marker}"
        );
        let graph = graph_resolve_answer(mcp_ref, marker).await;
        assert!(
            graph["candidates"].as_array().is_some_and(|candidates| {
                candidates.iter().all(|candidate| {
                    !candidate["display"].as_str().is_some_and(|name| name.contains(marker))
                })
            }),
            "ready persistent graph index excludes unsaved editor-only marker {marker}"
        );
    }
    assert_single_owner(&wait_for_backend_markers(runtime.path()).await);
    failure_artifacts.checkpoint("baseline disk and editor views verified");

    std::fs::write(
        &target,
        "Функция ЧисловоеЗначение() Экспорт\n    Возврат \"disk-edit\";\nКонецФункции\n",
    )
    .unwrap();
    watched_file(&mut editor_a, &target, 2);
    let edited_disk = wait_disk_type(mcp_ref, "Строка").await;
    assert_eq!(edited_disk["return_type"], "Строка", "MCP sees external edit: {edited_disk}");
    hover_type(&mut editor_a, &mut editor_a_next_id, &caller, "Булево", "Булево");
    hover_type(&mut editor_b, &mut editor_b_next_id, &caller, "Строка", "Строка");
    failure_artifacts.checkpoint("external disk edit observed; open buffers retained");

    std::fs::remove_file(&target).unwrap();
    watched_file(&mut editor_a, &target, 3);
    let deleted = wait_disk_status(mcp_ref, "not_found").await;
    assert_eq!(deleted["status"], "not_found", "MCP sees external deletion: {deleted}");
    hover_type(&mut editor_a, &mut editor_a_next_id, &caller, "Булево", "Булево");
    hover_type(&mut editor_b, &mut editor_b_next_id, &caller, "Строка", "Строка");
    failure_artifacts.checkpoint("external target deletion observed; open buffers retained");

    std::fs::write(&target, "Функция ЧисловоеЗначение() Экспорт\n    Возврат 84;\nКонецФункции\n")
        .unwrap();
    watched_file(&mut editor_a, &target, 1);
    let recreated = wait_disk_type(mcp_ref, "Число").await;
    assert_eq!(recreated["return_type"], "Число", "MCP sees recreated file: {recreated}");

    editor_b.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didChange",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&target).unwrap(), "version": 2},
            "contentChanges": [{"text": "Функция ЧисловоеЗначение() Экспорт\n    Возврат Ложь;\nКонецФункции\n"}]}
    }));
    hover_type(&mut editor_b, &mut editor_b_next_id, &caller, "Булево", "Строка");
    failure_artifacts.checkpoint("editor B incremental change verified");

    std::fs::write(
        &target,
        "Функция ЧисловоеЗначение() Экспорт\n    Возврат \"saved-A\";\nКонецФункции\n",
    )
    .unwrap();
    watched_file(&mut editor_a, &target, 2);
    editor_a.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&target).unwrap()}}
    }));
    hover_type(&mut editor_a, &mut editor_a_next_id, &caller, "Булево", "Булево");
    editor_a.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didClose",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&target).unwrap()}}
    }));
    hover_type(&mut editor_a, &mut editor_a_next_id, &caller, "Строка", "Булево");
    hover_type(&mut editor_b, &mut editor_b_next_id, &caller, "Булево", "Булево");
    let saved = wait_disk_type(mcp_ref, "Строка").await;
    assert_eq!(saved["return_type"], "Строка", "MCP sees saved disk generation: {saved}");
    failure_artifacts.checkpoint("save and close transition verified");

    mcp.take().expect("MCP connection").cancel().await.ok();
    if let Some(mut process) = mcp_process.take() {
        let _ = process.kill().await;
        let _ = process.wait().await;
    }
    failure_artifacts.checkpoint("MCP disconnected; both LSP editor views still respond");
    hover_type(&mut editor_a, &mut editor_a_next_id, &caller, "Строка", "Строка");
    hover_type(&mut editor_b, &mut editor_b_next_id, &caller, "Булево", "Булево");
    assert!(editor_a.shutdown(editor_a_next_id).success(), "editor A exits cleanly");
    assert!(editor_b.shutdown(editor_b_next_id).success(), "editor B exits cleanly");
    tokio::time::sleep(Duration::from_millis(1200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_lsp_and_mcp_subprocesses_keep_disk_and_editor_views_in_both_attach_orders() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    run_attach_order(false, false).await;
    run_attach_order(true, false).await;
    run_attach_order(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "необязательный тест: одновременное подключение LSP и двух брокеров может зависнуть без события завершения; детерминированного шва этой гонки нет; запуск: cargo test -p bsl-analyzer --test shared_workspace_backend concurrent_broker_sessions_keep_tools_and_native_context_local_with_lsp -- --ignored --exact"]
async fn concurrent_broker_sessions_keep_tools_and_native_context_local_with_lsp() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable workspace cache");
    let runtime = tempfile::tempdir().expect("disposable broker runtime directory");
    let _env = isolated_backend_env(cache.path(), runtime.path());
    let (url_a, request_a, worker_a) = onec_validation_mock(true);
    let (url_b, request_b, worker_b) = onec_validation_mock(false);

    let args_a = [
        "--enable-tool",
        "references",
        "--onec-url",
        url_a.as_str(),
        "--onec-user",
        "session-a-user",
        "--onec-password",
        "session-a-password",
    ];
    let args_b = [
        "--onec-url",
        url_b.as_str(),
        "--onec-user",
        "session-b-user",
        "--onec-password",
        "session-b-password",
    ];
    let lsp_root = root.path().to_path_buf();
    let ((client_a, mut process_a), (client_b, mut process_b), editor) = tokio::join!(
        mcp_client_with_args(root.path(), "broker", &args_a),
        mcp_client_with_args(root.path(), "broker", &args_b),
        tokio::task::spawn_blocking(move || Lsp::start(&lsp_root)),
    );
    let mut editor = editor.expect("real LSP joins both broker sessions");
    let caller = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    let diagnostics = editor.open(&caller, CALLER);
    assert!(
        diagnostics["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "UnresolvedBareCall"),
        "LSP serves its workspace view while both MCP sessions are live"
    );

    let tools_a = client_a.peer().list_tools(None).await.expect("session A tools/list");
    let tools_b = client_b.peer().list_tools(None).await.expect("session B tools/list");
    assert!(
        tools_a.tools.iter().any(|tool| tool.name == "references"),
        "session A gets its opted-in references tool"
    );
    assert!(
        tools_b.tools.iter().all(|tool| tool.name != "references"),
        "session B does not inherit session A's opt-in"
    );
    let denied = client_b
        .call_tool(CallToolRequestParams::new("references").with_arguments(Map::from_iter([(
            "symbol".to_owned(),
            json!("ОбщийМодуль.ЧисловоеЗначение"),
        )])))
        .await;
    assert!(denied.is_err(), "session B cannot dispatch A's opt-in tool");

    let validation = || {
        CallToolRequestParams::new("query").with_arguments(Map::from_iter([
            ("action".to_owned(), json!("validate")),
            ("query".to_owned(), json!("ВЫБРАТЬ 1 КАК Значение")),
        ]))
    };
    let (result_a, result_b) =
        tokio::join!(client_a.call_tool(validation()), client_b.call_tool(validation()),);
    let result_a = result_a.expect("session A native validation dispatch");
    let result_b = result_b.expect("session B native validation dispatch");
    let body_a = result_a.structured_content.expect("session A validation envelope");
    let body_b = result_b.structured_content.expect("session B validation envelope");
    assert_eq!(body_a["results"][1]["backend"], "platform");
    assert_eq!(body_a["results"][1]["valid"], true, "A's HTTP endpoint answered A");
    assert_eq!(body_a["results"][1]["errors"][0], "from-session-a");
    assert_eq!(body_b["results"][1]["backend"], "platform");
    assert_eq!(body_b["results"][1]["valid"], false, "B's HTTP endpoint answered B");
    assert_eq!(body_b["results"][1]["errors"][0], "from-session-b");
    let auth_a = request_a
        .recv_timeout(test_utils::wait::ceiling(Duration::from_secs(10)))
        .expect("A's mock received its platform request");
    let auth_b = request_b
        .recv_timeout(test_utils::wait::ceiling(Duration::from_secs(10)))
        .expect("B's mock received its platform request");
    assert!(!auth_a.is_empty() && !auth_b.is_empty(), "both session credentials reach HTTP");
    assert_ne!(auth_a, auth_b, "the two sessions send distinct native credentials");
    worker_a.join().expect("A's one-request mock exits");
    worker_b.join().expect("B's one-request mock exits");

    let markers = wait_for_backend_markers(runtime.path()).await;
    assert_owner_counts(&markers, 2, 1);

    client_a.cancel().await.ok();
    client_b.cancel().await.ok();
    let _ = process_a.kill().await;
    let _ = process_a.wait().await;
    let _ = process_b.kill().await;
    let _ = process_b.wait().await;
    assert!(editor.shutdown(3).success(), "LSP exits cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lsp_only_defers_search_activation_and_mcp_schema_answers_while_credentials_are_held() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable workspace cache");
    let runtime = tempfile::tempdir().expect("disposable broker runtime directory");
    let mut _env = isolated_backend_env(cache.path(), runtime.path());
    for key in [
        "BSL_ONEC_PASSWORD",
        "BSL_ONEC_CONNECTIONS_FILE",
        "BSL_SEARCH_BASELINE",
        "BSL_SEARCH_REFERENCE",
        "EMBEDDING_API_KEY",
        "EMBEDDING_URL",
        "EMBEDDING_MODEL",
        "EMBEDDING_PROVIDER",
        "EMBEDDING_QUERY_PREFIX",
        "EMBEDDING_DOCUMENT_PREFIX",
    ] {
        _env.remove(key);
    }

    let helper_count = root.path().join("helper-count");
    let helper_started = root.path().join("helper-started");
    let helper_complete = root.path().join("helper-complete");
    let helper_release = root.path().join("helper-release");
    let helper_response_path = root.path().join("helper-response.json");
    let mut release = FileBarrierRelease::new(helper_release.clone());

    let postgres_listener = TcpListener::bind(("127.0.0.1", 0)).expect("local PostgreSQL mock");
    postgres_listener.set_nonblocking(true).unwrap();
    let postgres_address = postgres_listener.local_addr().unwrap();
    let response = json!({
        "protocol": "bsl-analyzer.postgres-helper.v1",
        "ok": true,
        "url": format!("postgres://test:test@{postgres_address}/synthetic?connect_timeout=1"),
        "lease_id": "synthetic/test",
        "expires_at": "2099-01-01T00:00:00Z",
        "renewable": false,
    });
    std::fs::write(&helper_response_path, response.to_string()).unwrap();

    let (helper_program, helper_args) = if cfg!(windows) {
        let script = root.path().join("blocking-credential-helper.ps1");
        std::fs::write(
            &script,
            "$null = [Console]::In.ReadToEnd()\n\
             $countPath = $args[0]\n\
             $startedPath = $args[1]\n\
             $completePath = $args[2]\n\
             $releasePath = $args[3]\n\
             $responsePath = $args[4]\n\
             $count = 0\n\
             if (Test-Path -LiteralPath $countPath) { $count = [int][IO.File]::ReadAllText($countPath) }\n\
             [IO.File]::WriteAllText($countPath, [string]($count + 1))\n\
             [IO.File]::WriteAllText($startedPath, 'entered')\n\
             while (-not (Test-Path -LiteralPath $releasePath)) { Start-Sleep -Milliseconds 10 }\n\
             [IO.File]::WriteAllText($completePath, 'complete')\n\
             [Console]::Out.WriteLine([IO.File]::ReadAllText($responsePath))\n",
        )
        .unwrap();
        (
            "powershell.exe".to_owned(),
            vec![
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-ExecutionPolicy".to_owned(),
                "Bypass".to_owned(),
                "-File".to_owned(),
                script.to_string_lossy().into_owned(),
                helper_count.to_string_lossy().into_owned(),
                helper_started.to_string_lossy().into_owned(),
                helper_complete.to_string_lossy().into_owned(),
                helper_release.to_string_lossy().into_owned(),
                helper_response_path.to_string_lossy().into_owned(),
            ],
        )
    } else {
        let script = "import pathlib,sys,time; p=list(map(pathlib.Path,sys.argv[1:6])); sys.stdin.read(); n=int(p[0].read_text()) if p[0].exists() else 0; p[0].write_text(str(n+1)); p[1].write_text('entered');\nwhile not p[3].exists(): time.sleep(0.01)\np[2].write_text('complete'); print(p[4].read_text())";
        (
            "python3".to_owned(),
            vec![
                "-c".to_owned(),
                script.to_owned(),
                helper_count.to_string_lossy().into_owned(),
                helper_started.to_string_lossy().into_owned(),
                helper_complete.to_string_lossy().into_owned(),
                helper_release.to_string_lossy().into_owned(),
                helper_response_path.to_string_lossy().into_owned(),
            ],
        )
    };
    let config = format!(
        "[source]\nroot = \".\"\n\
         [platform_help]\nsource = \"bundled\"\n\
         [search.baseline]\nbackend = \"postgres\"\n\
         [search.baseline.postgres]\nhost = \"127.0.0.1\"\nport = {}\ndbname = \"synthetic\"\nschema = \"bsl_search\"\nvault_role_base = \"test/only\"\n\
         [search.baseline.postgres.credential_helper]\nprogram = {:?}\nargs = {:?}\n\
         [search.baseline.workspace_code]\nbranch = \"main\"\n\
         [search.baseline.workspace_code.policy]\npublish_branches = [\"main\"]\n",
        postgres_address.port(),
        helper_program,
        helper_args,
    );
    std::fs::write(root.path().join("bsl-analyzer.toml"), config).unwrap();

    let lsp_root = root.path().to_path_buf();
    let mut editor = tokio::task::spawn_blocking(move || Lsp::start(&lsp_root))
        .await
        .expect("LSP process starts");
    let started = wait_for_owner_initializers(runtime.path(), 1).await;
    let epoch = owner_epochs(&started).into_iter().next().expect("owner epoch").to_owned();
    let markers = wait_for_lsp_attach_in_epoch(runtime.path(), &epoch).await;
    assert_eq!(
        markers.iter().filter(|line| line.contains("session_attached")).count(),
        1,
        "LSP-only startup has no MCP session: {markers:#?}"
    );
    let markers = wait_for_resident_bootstrap(runtime.path()).await;
    assert_owner_counts(&markers, 0, 1);
    assert!(
        !helper_count.exists(),
        "LSP-only attach did not execute the configured credential helper"
    );
    assert!(
        !helper_started.exists(),
        "LSP-only attach did not start configured network credentials"
    );
    assert!(
        !cache_contains_file_named(cache.path(), "bsl-graph.db"),
        "LSP-only attach did not initialize the persistent graph"
    );
    assert!(
        !cache_contains_file_named(cache.path(), "bsl-search.db"),
        "LSP-only attach did not initialize the persistent search index"
    );
    assert!(
        matches!(postgres_listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "LSP-only attach did not connect to the local PostgreSQL mock"
    );

    let (client, mut process) = mcp_client(root.path()).await;
    let helper_deadline =
        tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(10));
    while !helper_started.exists() {
        assert!(
            tokio::time::Instant::now() < helper_deadline,
            "first MCP attach did not activate the configured credential helper"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(std::fs::read_to_string(&helper_count).unwrap(), "1");
    assert!(!helper_complete.exists(), "helper remains held before the fast query completes");

    let schema = tokio::time::timeout(
        test_utils::wait::ceiling(Duration::from_secs(5)),
        client.call_tool(
            CallToolRequestParams::new("query")
                .with_arguments(Map::from_iter([("action".to_owned(), json!("schema"))])),
        ),
    )
    .await
    .expect("workspace query schema is bounded while credentials are held")
    .expect("workspace query schema succeeds while credentials are held");
    assert_ne!(schema.is_error, Some(true));
    let schema = schema.structured_content.expect("query schema is structured");
    assert_eq!(schema["schema_version"], "2");
    assert_eq!(schema["actions"], json!(["validate", "execute", "schema"]));
    assert!(!helper_complete.exists(), "schema response arrived before helper release");
    assert_owner_counts(&backend_markers(runtime.path()), 1, 1);
    assert!(
        matches!(postgres_listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "held helper has not connected to the local PostgreSQL mock"
    );

    let (postgres_tx, postgres_rx) = mpsc::channel();
    let postgres_worker = std::thread::spawn(move || {
        let deadline =
            std::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(15));
        loop {
            match postgres_listener.accept() {
                Ok((stream, _)) => {
                    drop(stream);
                    let _ = postgres_tx.send(Ok(()));
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        let _ = postgres_tx
                            .send(Err("local PostgreSQL mock was not contacted".to_owned()));
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = postgres_tx.send(Err(error.to_string()));
                    return;
                }
            }
        }
    });
    release.release();
    let helper_deadline =
        tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(10));
    while !helper_complete.exists() {
        assert!(
            tokio::time::Instant::now() < helper_deadline,
            "configured helper did not complete after release"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::task::spawn_blocking(move || {
        postgres_rx.recv_timeout(test_utils::wait::ceiling(Duration::from_secs(15)))
    })
    .await
    .expect("join bounded PostgreSQL acceptance wait")
    .expect("credential activation reaches the local PostgreSQL mock")
    .expect("local PostgreSQL mock accepted the activation connection");
    postgres_worker.join().expect("local PostgreSQL mock worker exits");

    client.cancel().await.ok();
    let _ = process.kill().await;
    let _ = process.wait().await;
    assert!(editor.shutdown(2).success(), "LSP exits cleanly after helper release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scope_rebind_rechecks_open_buffers_and_preserves_public_lsp_state() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable cache");
    let runtime = tempfile::tempdir().expect("disposable runtime");
    let mut artifacts = FailureArtifacts {
        root: root.path().to_path_buf(),
        runtime: runtime.path().to_path_buf(),
        attach_order: "s19-scope-rebind",
        phase: "setup".to_owned(),
    };
    let _env = isolated_backend_env(cache.path(), runtime.path());
    let config = root.path().join("bsl-analyzer.toml");
    let full_config = "[source]\nroot = \".\"\n[platform_help]\nsource = \"bundled\"\n[features]\nworkspaceDiagnostics = \"all\"\n";
    let excluded_config = "[source]\nroot = \".\"\nexclude = [\"CommonModules/ОбщийМодуль\"]\n\
         [platform_help]\nsource = \"bundled\"\n\
         [features]\nworkspaceDiagnostics = \"all\"\n";
    std::fs::write(&config, full_config).unwrap();
    let synthetic = root.path().join("Synthetic");
    std::fs::create_dir_all(&synthetic).unwrap();
    for index in 0..256 {
        std::fs::write(
            synthetic.join(format!("ScopeModule{index:04}.bsl")),
            format!("Функция Scope{index}() Экспорт\n    Возврат {index};\nКонецФункции\n"),
        )
        .unwrap();
    }

    let caller = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    let target = root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
    let expected_disk = std::fs::read(&target).expect("fixture target before any editor attaches");
    let caller_buffer = "Функция Проверить() Экспорт\n    Локальная = ОбщийМодуль.ЧисловоеЗначение();\n    Возврат Локальная;\nКонецФункции\n";
    let target_buffer = "Функция ЧисловоеЗначение() Экспорт\n    НеизвестныйВызов();\n    Возврат Истина;\nКонецФункции\n";
    let caps = json!({
        "general": {"positionEncodings": ["utf-8", "utf-16"]},
        "window": {"workDoneProgress": true},
        "workspace": {
            "diagnostics": {"refreshSupport": true},
            "semanticTokens": {"refreshSupport": true}
        }
    });

    let (mcp, mut mcp_process) = mcp_client(root.path()).await;
    let mut editor = Lsp::start_with_capabilities(root.path(), caps);
    artifacts.checkpoint("MCP and LSP attached; initialized capabilities captured");
    let initial_caps = &editor.initialize_response["result"]["capabilities"];
    assert!(!initial_caps["diagnosticProvider"].is_null(), "pull diagnostics is announced");
    assert!(!initial_caps["semanticTokensProvider"].is_null(), "semantic tokens are announced");
    assert_eq!(initial_caps["positionEncoding"], "utf-8", "the test negotiates UTF-8 positions");

    editor.open(&caller, caller_buffer);
    let target_diagnostics = editor.open(&target, target_buffer);
    assert!(
        target_diagnostics["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "UnresolvedBareCall"),
        "the admitted buffer publishes its known diagnostic: {target_diagnostics}"
    );
    let disk = wait_disk_type(&mcp, "Число").await;
    assert_eq!(disk["return_type"], "Число", "MCP remains disk-backed: {disk}");
    artifacts.checkpoint("admitted buffers and disk view established");

    let source_change = root.path().join("CommonModules/Потребитель/Ext/Refresh.bsl");
    std::fs::write(&source_change, "Процедура Изменённый()\nКонецПроцедуры\n").unwrap();
    watched_file(&mut editor, &source_change, 1);
    let old_refresh = editor.wait_for(|message| {
        message["method"] == "workspace/semanticTokens/refresh" && message["id"].is_number()
    });
    let old_server_id = old_refresh["id"].as_u64().expect("numeric server request id");

    editor.send(json!({
        "jsonrpc": "2.0", "id": 70, "method": "workspace/diagnostic",
        "params": {
            "previousResultIds": [], "partialResultToken": "scope-partial",
            "workDoneToken": "scope-work"
        }
    }));
    let old_progress = editor.wait_for(|message| {
        message["method"] == "$/progress"
            && message["params"]["token"] == "scope-work"
            && message["params"]["value"]["kind"] == "begin"
    });
    assert_eq!(old_progress["params"]["value"]["kind"], "begin");
    assert!(
        editor
            .wait_for_within(Duration::from_millis(10), |message| {
                message["id"] == 70 && message["method"].is_null()
            })
            .is_none(),
        "the accepted workspace request is still pending when scope changes"
    );
    assert!(
        editor
            .wait_for_within(Duration::from_millis(10), |message| {
                message["method"] == "$/progress"
                    && message["params"]["token"] == "scope-work"
                    && message["params"]["value"]["kind"] == "end"
            })
            .is_none(),
        "the old progress generation is still active when scope changes"
    );
    artifacts.checkpoint("old refresh request and workspace diagnostic progress are pending");

    std::fs::write(&config, excluded_config).unwrap();
    editor.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&config).unwrap()}}
    }));
    let excluded_uri = lsp_types::Url::from_file_path(&target).unwrap().to_string();
    let retirement_events =
        collect_scope_retirement_events(&editor, 70, old_server_id, &excluded_uri);
    let terminal = retirement_events
        .iter()
        .find(|message| message["id"] == 70 && message["method"].is_null())
        .expect("the accepted request gets one terminal response");
    assert!(
        terminal.get("result").is_some() || terminal.get("error").is_some(),
        "the accepted workspace diagnostic request gets one terminal response: {terminal}"
    );
    let cancel = retirement_events
        .iter()
        .find(|message| {
            message["method"] == "$/cancelRequest" && message["params"]["id"] == old_server_id
        })
        .expect("the old server request is canceled during retirement");
    assert_eq!(cancel["params"]["id"], old_refresh["id"]);
    let old_progress_end = retirement_events
        .iter()
        .find(|message| {
            message["method"] == "$/progress"
                && message["params"]["token"] == "scope-work"
                && message["params"]["value"]["kind"] == "end"
        })
        .expect("the old progress generation ends during retirement");
    assert_eq!(old_progress_end["params"]["value"]["kind"], "end");
    let excluded_diagnostics = retirement_events
        .iter()
        .find(|message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == excluded_uri
                && message["params"]["diagnostics"].as_array().is_some_and(Vec::is_empty)
        })
        .expect("the newly excluded URI has its diagnostics cleared");
    let new_refresh = retirement_events
        .iter()
        .find(|message| {
            message["method"] == "workspace/semanticTokens/refresh"
                && message["id"].as_u64().is_some_and(|id| id > old_server_id)
        })
        .expect("the rebound session retains semantic refresh capability");
    assert_eq!(
        retirement_events
            .iter()
            .filter(|message| message["id"] == 70 && message["method"].is_null())
            .count(),
        1,
        "one terminal response was observed in the retirement event batch: {retirement_events:?}"
    );
    assert!(
        editor
            .wait_for_within(Duration::from_millis(250), |message| {
                message["id"] == 70 && message["method"].is_null()
            })
            .is_none(),
        "scope retirement does not answer one client request twice"
    );

    let markers = wait_for_owner_initializers(runtime.path(), 2).await;
    let epochs = owner_epochs(&markers);
    assert_eq!(epochs.len(), 2, "scope change starts one new owner: {markers:?}");
    artifacts.checkpoint("excluded scope rebind completed");
    assert_eq!(
        markers
            .iter()
            .filter(|line| line.contains("session_attached")
                && trace_field(line, "adapter") == Some("lsp"))
            .count(),
        2,
        "the same editor session reattaches exactly once after retirement: {markers:?}"
    );
    assert!(
        excluded_diagnostics["params"]["diagnostics"].as_array().unwrap().is_empty(),
        "rebind clears diagnostics from the newly excluded URI"
    );

    let new_server_id = new_refresh["id"].as_u64().expect("numeric rebound server request id");
    assert!(
        new_server_id > old_server_id,
        "the public server request ID sequence survives rebind: old={old_server_id}, new={new_server_id}"
    );
    editor.send(json!({"jsonrpc": "2.0", "id": old_server_id, "result": null}));
    editor.send(json!({"jsonrpc": "2.0", "id": new_server_id, "result": null}));
    assert!(
        editor
            .wait_for_within(Duration::from_millis(250), |message| {
                message["id"] == 1 && message["method"].is_null()
            })
            .is_none(),
        "private rebind does not send a second public initialize response"
    );

    let caller_uri = lsp_types::Url::from_file_path(&caller).unwrap();
    let call_character = caller_buffer.lines().nth(1).unwrap().find("ЧисловоеЗначение").unwrap();
    let excluded_definition = editor.request(
        80,
        "textDocument/definition",
        json!({"textDocument": {"uri": caller_uri}, "position": {"line": 1, "character": call_character}}),
    );
    assert!(
        excluded_definition["error"].is_null(),
        "excluded reference response: {excluded_definition}"
    );
    assert!(
        excluded_definition["result"].is_null(),
        "the target is excluded from the rebound semantic scope: {excluded_definition}"
    );
    let mcp_terminal = tokio::time::timeout(
        test_utils::wait::ceiling(Duration::from_secs(10)),
        mcp.call_tool(CallToolRequestParams::new("symbol_info").with_arguments(Map::from_iter([
            ("symbol".to_owned(), json!("ОбщийМодуль.ЧисловоеЗначение")),
        ]))),
    )
    .await;
    assert!(
        matches!(mcp_terminal, Ok(Err(_))),
        "MCP keeps terminal retirement instead of silently rebinding: {mcp_terminal:?}"
    );

    std::fs::write(&config, full_config).unwrap();
    editor.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&config).unwrap()}}
    }));
    let third_generation = wait_for_owner_initializers(runtime.path(), 3).await;
    let third_epochs = owner_epochs(&third_generation);
    assert_eq!(third_epochs.len(), 3, "unexclude starts another validated owner");
    artifacts.checkpoint("unexclude rebind completed");
    let (rebound_definition, rebound_definition_id) =
        request_definition_until_ready(&mut editor, 1000, &caller_uri, 1, call_character);
    assert!(rebound_definition["error"].is_null(), "unexcluded reference: {rebound_definition}");
    let location = rebound_definition["result"]
        .as_array()
        .map_or(&rebound_definition["result"], |items| &items[0]);
    assert_eq!(
        location["uri"],
        lsp_types::Url::from_file_path(&target).unwrap().as_str(),
        "unexclude admits the same open target buffer again"
    );
    let rebound_hover = editor.request(
        rebound_definition_id + 1,
        "textDocument/hover",
        json!({
            "textDocument": {"uri": lsp_types::Url::from_file_path(&caller).unwrap()},
            "position": {"line": 2, "character": caller_buffer.lines().nth(2).unwrap().find("Локальная").unwrap()}
        }),
    );
    assert!(rebound_hover["error"].is_null(), "unexcluded buffer hover: {rebound_hover}");
    assert!(
        rebound_hover["result"].to_string().contains("Булево"),
        "unexclude uses the latest open target buffer, not the numeric disk contents: {rebound_hover}"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        expected_disk,
        "editor rebind never writes the buffer to disk"
    );

    let new_uri = root.path().join("CommonModules/Дополнительный/Ext/Module.bsl");
    let new_text = "Процедура НовыйМодуль() Экспорт\nКонецПроцедуры\n";
    std::fs::create_dir_all(new_uri.parent().unwrap()).unwrap();
    std::fs::write(&new_uri, new_text).unwrap();
    watched_file(&mut editor, &new_uri, 1);
    let new_diagnostics = editor.open(&new_uri, new_text);
    assert_eq!(new_diagnostics["method"], "textDocument/publishDiagnostics");

    mcp_process.kill().await.ok();
    let _ = mcp_process.wait().await;
    assert!(editor.shutdown(rebound_definition_id + 2).success(), "rebound LSP shuts down cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dotenv_configuration_root_change_rebinds_shared_lsp_and_retires_mcp() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable cache");
    let runtime = tempfile::tempdir().expect("disposable runtime");
    let stores = tempfile::tempdir().expect("disposable synthetic configuration roots");
    let mut artifacts = FailureArtifacts {
        root: root.path().to_path_buf(),
        runtime: runtime.path().to_path_buf(),
        attach_order: "s19-dotenv-rebind",
        phase: "setup".to_owned(),
    };
    let mut env = isolated_backend_env(cache.path(), runtime.path());
    env.remove("ONEC_CONFIGURATIONS_ROOT");
    for key in [
        "BSL_ONEC_PASSWORD",
        "BSL_ONEC_CONNECTIONS_FILE",
        "EMBEDDING_API_KEY",
        "EMBEDDING_URL",
        "EMBEDDING_MODEL",
        "EMBEDDING_DIM",
        "EMBEDDING_PROVIDER",
        "EMBEDDING_QUERY_PREFIX",
        "EMBEDDING_DOCUMENT_PREFIX",
        "BSL_LOG_FILE",
    ] {
        env.remove(key);
    }
    let store_a = stores.path().join("configuration-a");
    let store_b = stores.path().join("configuration-b");
    for store in [&store_a, &store_b] {
        let configuration = store.join("UT11/1.0.0");
        let module = configuration.join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
        std::fs::create_dir_all(module.parent().unwrap()).unwrap();
        std::fs::write(configuration.join("Configuration.xml"), "<Configuration/>\n").unwrap();
        std::fs::copy(
            root.path().join("CommonModules/ОбщийМодуль.xml"),
            configuration.join("CommonModules/ОбщийМодуль.xml"),
        )
        .unwrap();
        std::fs::copy(root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl"), module)
            .unwrap();
    }
    let extension = root.path().join("extension");
    let caller = extension.join("CommonModules/Потребитель/Ext/Module.bsl");
    std::fs::create_dir_all(caller.parent().unwrap()).unwrap();
    std::fs::write(extension.join("Configuration.xml"), "<Configuration/>\n").unwrap();
    std::fs::copy(
        root.path().join("CommonModules/Потребитель.xml"),
        extension.join("CommonModules/Потребитель.xml"),
    )
    .unwrap();
    std::fs::write(&caller, CALLER).unwrap();
    std::fs::write(
        root.path().join("bsl-analyzer.toml"),
        "[source]\nextensions = [{ name = \"Editor\", path = \"extension\", scope = \"InfoBase\" }]\n\
         [platform_help]\nsource = \"bundled\"\n\
         [source.configuration]\nid = \"UT11\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    let dotenv = root.path().join(".env");
    std::fs::write(&dotenv, format!("ONEC_CONFIGURATIONS_ROOT={}\n", store_a.display())).unwrap();

    let validated = mcp_server::project::at(root.path())
        .expect("project_model accepts the synthetic .env configuration");
    let resolved_configuration =
        validated.configuration_path().expect("the synthetic shared configuration resolves");
    assert!(
        resolved_configuration.starts_with(&store_a)
            && resolved_configuration.join("Configuration.xml").is_file(),
        "project_model resolves source.configuration through the synthetic .env root: {}",
        resolved_configuration.display()
    );
    let (mcp, mut mcp_process) = mcp_client_mode_with_synthetic_stderr(root.path(), "broker").await;
    let mut editor = Lsp::start_with_capabilities(
        root.path(),
        json!({"general": {"positionEncodings": ["utf-8"]}}),
    );
    editor.open(&caller, CALLER);
    let caller_uri = lsp_types::Url::from_file_path(&caller).unwrap();
    let call_character = CALLER.lines().nth(2).unwrap().find("ЧисловоеЗначение").unwrap();
    let (initial_definition, _) =
        request_definition_until_ready(&mut editor, 2, &caller_uri, 2, call_character);
    assert!(initial_definition["error"].is_null(), "initial definition: {initial_definition}");
    let location = initial_definition["result"]
        .as_array()
        .map_or(&initial_definition["result"], |items| &items[0]);
    assert_location_file(
        location,
        &store_a.join("UT11/1.0.0/CommonModules/ОбщийМодуль/Ext/Module.bsl"),
        "the extension buffer initially resolves into configuration A",
    );
    let initial_markers = wait_for_owner_initializers(runtime.path(), 1).await;
    let initial_epochs = owner_epochs(&initial_markers);
    assert_eq!(initial_epochs.len(), 1, "one shared owner before .env edit");
    artifacts.checkpoint("shared owner attached using first synthetic .env root");

    std::fs::write(&dotenv, format!("ONEC_CONFIGURATIONS_ROOT={}\n", store_b.display())).unwrap();
    editor.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&dotenv).unwrap()}}
    }));
    let next_markers = wait_for_owner_initializers(runtime.path(), 2).await;
    let epochs = owner_epochs(&next_markers);
    assert_eq!(epochs.len(), 2, "a .env project input creates the validated next owner");
    assert_ne!(epochs[0], epochs[1], "the configuration-root change advances owner epoch");
    artifacts.checkpoint("synthetic .env changed configuration root and LSP rebound");

    let attach_deadline =
        tokio::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(30));
    let attached_markers = loop {
        let markers = backend_markers(runtime.path());
        let attached = markers.iter().filter(|line| line.contains("session_attached")).count();
        if attached >= 3 {
            break markers;
        }
        assert!(
            tokio::time::Instant::now() < attach_deadline,
            "LSP did not attach to the .env owner: {markers:?}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    };

    let mcp_terminal = tokio::time::timeout(
        test_utils::wait::ceiling(Duration::from_secs(10)),
        mcp.call_tool(CallToolRequestParams::new("symbol_info").with_arguments(Map::from_iter([
            ("symbol".to_owned(), json!("ОбщийМодуль.ЧисловоеЗначение")),
        ]))),
    )
    .await;
    assert!(
        matches!(mcp_terminal, Ok(Err(_))),
        "the MCP session remains terminal on .env scope change: {mcp_terminal:?}"
    );
    let (definition, last_request_id) = request_definition_until_ready(
        &mut editor,
        80,
        &lsp_types::Url::from_file_path(&caller).unwrap(),
        2,
        call_character,
    );
    assert!(definition["error"].is_null(), "LSP stays live after .env rebind: {definition}");
    let location = definition["result"].as_array().map_or(&definition["result"], |items| &items[0]);
    assert_location_file(
        location,
        &store_b.join("UT11/1.0.0/CommonModules/ОбщийМодуль/Ext/Module.bsl"),
        "the same extension buffer resolves into the newly selected configuration B",
    );
    let attached = attached_markers
        .iter()
        .filter(|line| line.contains("session_attached"))
        .filter_map(|line| trace_field(line, "adapter"))
        .collect::<Vec<_>>();
    assert_eq!(attached.iter().filter(|&&adapter| adapter == "lsp").count(), 2);
    assert_eq!(attached.iter().filter(|&&adapter| adapter == "mcp").count(), 1);

    mcp_process.kill().await.ok();
    let _ = mcp_process.wait().await;
    assert!(
        editor.shutdown(last_request_id + 1).success(),
        "the .env-rebound editor shuts down cleanly"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_project_config_reports_error_and_keeps_last_good_shared_view() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable cache");
    let runtime = tempfile::tempdir().expect("disposable runtime");
    let mut artifacts = FailureArtifacts {
        root: root.path().to_path_buf(),
        runtime: runtime.path().to_path_buf(),
        attach_order: "s19-invalid-config-last-good",
        phase: "setup".to_owned(),
    };
    let mut env = isolated_backend_env(cache.path(), runtime.path());
    env.set("BSL_LOG", "info,bsl_analyzer=debug");
    let config = root.path().join("bsl-analyzer.toml");
    let valid_config = "[source]\nroot = \".\"\n[platform_help]\nsource = \"bundled\"\n[features]\nworkspaceDiagnostics = \"all\"\n";
    std::fs::write(&config, valid_config).unwrap();

    let caps = json!({
        "general": {"positionEncodings": ["utf-8", "utf-16"]},
        "window": {"workDoneProgress": true},
        "workspace": {
            "diagnostics": {"refreshSupport": true},
            "semanticTokens": {"refreshSupport": true}
        }
    });
    let mut editor = Lsp::start_with_capabilities(root.path(), caps);
    let initial_caps = &editor.initialize_response["result"]["capabilities"];
    assert_eq!(initial_caps["positionEncoding"], "utf-8");
    assert!(!initial_caps["diagnosticProvider"].is_null());
    assert!(!initial_caps["semanticTokensProvider"].is_null());

    let caller = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    let caller_uri = lsp_types::Url::from_file_path(&caller).unwrap();
    let caller_buffer = "Функция Проверить() Экспорт\n    Локальная = ОбщийМодуль.ЧисловоеЗначение();\n    Возврат Локальная;\nКонецФункции\n";
    let target = root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
    let target_uri = lsp_types::Url::from_file_path(&target).unwrap();
    let target_buffer = "Функция ЧисловоеЗначение() Экспорт\n    Возврат Истина;\nКонецФункции\n";
    artifacts.checkpoint("opening the caller buffer");
    editor.open(&caller, caller_buffer);
    artifacts.checkpoint("opening the target buffer");
    editor.open(&target, target_buffer);
    let first_owner = wait_for_owner_initializers(runtime.path(), 1).await;
    assert_eq!(owner_epochs(&first_owner).len(), 1, "the last-good owner is established");
    let old_epoch = owner_epochs(&first_owner)[0].to_owned();
    let call_character = caller_buffer.lines().nth(1).unwrap().find("ЧисловоеЗначение").unwrap();
    let (initial, last_request_id) =
        request_definition_until_ready(&mut editor, 2, &caller_uri, 1, call_character);
    assert!(initial["error"].is_null(), "the initial shared view is queryable: {initial}");
    let location = initial["result"].as_array().map_or(&initial["result"], |items| &items[0]);
    assert_eq!(location["uri"], target_uri.as_str());
    let refresh_file = root.path().join("Synthetic/InitialRefresh.bsl");
    std::fs::create_dir_all(refresh_file.parent().unwrap()).unwrap();
    std::fs::write(&refresh_file, "Процедура НачальноеОбновление()\nКонецПроцедуры\n").unwrap();
    watched_file(&mut editor, &refresh_file, 1);
    let initial_refresh = editor.wait_for(|message| {
        message["method"] == "workspace/semanticTokens/refresh" && message["id"].is_number()
    });
    let initial_server_request_id =
        initial_refresh["id"].as_u64().expect("initial semantic refresh request id");
    editor.send(json!({"jsonrpc": "2.0", "id": initial_server_request_id, "result": null}));
    artifacts.checkpoint("last-good shared owner resolves the known definition");

    std::fs::write(&config, "[source\nthis is invalid\n").unwrap();
    editor.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&config).unwrap()}}
    }));
    let config_error = editor.wait_for(|message| {
        message["method"] == "window/showMessage"
            && message["params"]["message"]
                .as_str()
                .is_some_and(|text| text.contains("project config reload failed"))
    });
    assert_eq!(config_error["params"]["type"], 1, "invalid project config is reported");
    assert!(
        config_error["params"]["message"].as_str().unwrap().contains("bsl-analyzer.toml"),
        "the error names the invalid project input: {config_error}"
    );
    artifacts.checkpoint("invalid project config reported while preserving last-good scope");

    let (last_good, last_request_id) = request_definition_until_ready(
        &mut editor,
        last_request_id + 1,
        &caller_uri,
        1,
        call_character,
    );
    assert!(last_good["error"].is_null(), "last-good semantic view remains available: {last_good}");
    let location = last_good["result"].as_array().map_or(&last_good["result"], |items| &items[0]);
    assert_eq!(
        location["uri"],
        target_uri.as_str(),
        "invalid configuration does not replace the last validated source composition"
    );
    let (frozen_hover, frozen_hover_id) = request_hover_until_ready(
        &mut editor,
        last_request_id + 1,
        &caller_uri,
        2,
        caller_buffer.lines().nth(2).unwrap().find("Локальная").unwrap(),
    );
    assert!(
        frozen_hover["error"].is_null(),
        "last-good open buffers remain queryable: {frozen_hover}"
    );
    assert!(
        frozen_hover["result"].to_string().contains("Булево"),
        "the last-good view still uses the unsaved target buffer: {frozen_hover}"
    );
    let markers = backend_markers(runtime.path());
    assert_eq!(
        markers.iter().filter(|line| line.contains("owner_init_start")).count(),
        1,
        "invalid input does not publish an invented owner scope: {markers:?}"
    );
    artifacts.checkpoint("frozen last-good owner still answers definition and hover");

    std::fs::write(&config, valid_config).unwrap();
    editor.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didSave",
        "params": {"textDocument": {"uri": lsp_types::Url::from_file_path(&config).unwrap()}}
    }));
    let repaired_markers = wait_for_owner_initializers(runtime.path(), 2).await;
    let repaired_epochs = owner_epochs(&repaired_markers);
    assert_eq!(
        repaired_epochs.len(),
        2,
        "valid repair starts a replacement owner: {repaired_markers:?}"
    );
    let new_epoch = repaired_epochs
        .iter()
        .copied()
        .find(|epoch| *epoch != old_epoch)
        .expect("repair uses a new owner epoch");
    assert_ne!(new_epoch, old_epoch, "the frozen owner is not resumed");
    let repaired_markers = wait_for_lsp_attach_in_epoch(runtime.path(), new_epoch).await;
    artifacts.checkpoint("valid repair attached the preserved LSP session to a new owner epoch");

    let refresh = editor.wait_for(|message| {
        (message["id"] == 1 && message["method"].is_null())
            || (message["method"] == "workspace/semanticTokens/refresh"
                && message["id"].is_number())
    });
    assert_eq!(
        refresh["method"], "workspace/semanticTokens/refresh",
        "repair keeps the negotiated refresh capability without a second initialize: {refresh}"
    );
    assert!(
        refresh["id"].as_u64().unwrap() > initial_server_request_id,
        "the replacement keeps the server request ID sequence: {refresh}"
    );
    editor.send(json!({"jsonrpc": "2.0", "id": refresh["id"], "result": null}));
    assert!(
        editor
            .wait_for_within(Duration::from_millis(250), |message| {
                message["id"] == 1 && message["method"].is_null()
            })
            .is_none(),
        "the same LSP process does not receive a second initialize response"
    );

    let (recovered, last_request_id) = request_definition_until_ready(
        &mut editor,
        frozen_hover_id + 1,
        &caller_uri,
        1,
        call_character,
    );
    assert!(
        recovered["error"].is_null(),
        "same LSP session becomes ready after repair: {recovered}"
    );
    let location = recovered["result"].as_array().map_or(&recovered["result"], |items| &items[0]);
    assert_eq!(location["uri"], target_uri.as_str());
    let (recovered_hover, last_request_id) = request_hover_until_ready(
        &mut editor,
        last_request_id + 1,
        &caller_uri,
        2,
        caller_buffer.lines().nth(2).unwrap().find("Локальная").unwrap(),
    );
    assert!(recovered_hover["error"].is_null(), "rebound buffer hover succeeds: {recovered_hover}");
    assert!(
        recovered_hover["result"].to_string().contains("Булево"),
        "replacement owner sees the same unsaved target buffer: {recovered_hover}"
    );

    assert_eq!(
        repaired_markers
            .iter()
            .filter(|line| line.contains("session_attached")
                && trace_field(line, "adapter") == Some("lsp"))
            .filter(|line| trace_field(line, "owner_epoch") == Some(old_epoch.as_str()))
            .count(),
        1,
        "the frozen owner has only its original LSP attachment: {repaired_markers:?}"
    );
    assert_eq!(
        repaired_markers
            .iter()
            .filter(|line| line.contains("session_attached")
                && trace_field(line, "adapter") == Some("lsp"))
            .filter(|line| trace_field(line, "owner_epoch") == Some(new_epoch))
            .count(),
        1,
        "the preserved LSP session attaches once to the replacement owner: {repaired_markers:?}"
    );
    assert!(editor.shutdown(last_request_id + 1).success(), "repaired editor shuts down");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_crash_ends_inflight_sessions_and_next_lsp_start_uses_a_new_epoch() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable cache");
    let runtime = tempfile::tempdir().expect("disposable runtime");
    let mut artifacts = FailureArtifacts {
        root: root.path().to_path_buf(),
        runtime: runtime.path().to_path_buf(),
        attach_order: "s20-daemon-crash",
        phase: "setup".to_owned(),
    };
    let _env = isolated_backend_env(cache.path(), runtime.path());
    std::fs::write(
        root.path().join("bsl-analyzer.toml"),
        "[source]\nroot = \".\"\n[platform_help]\nsource = \"bundled\"\n[features]\nworkspaceDiagnostics = \"all\"\n",
    )
    .unwrap();
    let source = root.path().join("Synthetic");
    std::fs::create_dir_all(&source).unwrap();
    for index in 0..2048 {
        std::fs::write(
            source.join(format!("Module{index:04}.bsl")),
            format!("Функция Синтетическая{index}() Экспорт\n    Возврат {index};\nКонецФункции\n"),
        )
        .unwrap();
    }

    let (mcp, mut mcp_process) = mcp_client(root.path()).await;
    let mut editor = Lsp::start_with_capabilities(
        root.path(),
        json!({"window": {"workDoneProgress": true}, "workspace": {"diagnostics": {"refreshSupport": true}}}),
    );
    editor.open(&root.path().join("CommonModules/Потребитель/Ext/Module.bsl"), CALLER);
    let markers = wait_for_resident_bootstrap(runtime.path()).await;
    artifacts.checkpoint("resident bootstrap ready; exact daemon PID recorded");
    let start = markers
        .iter()
        .find(|line| line.contains("owner_init_start"))
        .expect("isolated daemon owner marker");
    let old_epoch = trace_field(start, "owner_epoch").expect("owner epoch").to_owned();
    let resident = markers
        .iter()
        .find(|line| line.contains("resident_bootstrap_complete"))
        .expect("resident bootstrap completed for this daemon");
    assert_eq!(
        trace_field(resident, "owner_epoch"),
        Some(old_epoch.as_str()),
        "the resident belongs to the exact owner epoch whose PID is recorded"
    );
    let daemon_pid = trace_field(start, "daemon_pid")
        .expect("test-gated marker carries the exact daemon PID")
        .parse::<u32>()
        .expect("numeric daemon PID");
    assert!(daemon_pid > 0, "the isolated daemon has a real process PID");
    assert_eq!(
        markers
            .iter()
            .filter(|line| {
                line.contains("session_attached")
                    && trace_field(line, "owner_epoch") == Some(old_epoch.as_str())
            })
            .count(),
        2,
        "the exact marker PID belongs to the shared LSP and MCP owner epoch: {markers:?}"
    );

    editor.send(json!({
        "jsonrpc": "2.0", "id": 200, "method": "workspace/diagnostic",
        "params": {
            "previousResultIds": [], "partialResultToken": "crash-partial",
            "workDoneToken": "crash-work"
        }
    }));
    let first_chunk = editor.wait_for(|message| {
        message["method"] == "$/progress"
            && message["params"]["token"] == "crash-partial"
            && message["params"]["value"]["items"].is_array()
    });
    assert!(
        first_chunk["params"]["value"]["items"].as_array().is_some_and(|items| !items.is_empty()),
        "the accepted workspace request has started and emitted its first diagnostic chunk: {first_chunk}"
    );
    artifacts.checkpoint("workspace diagnostic request admitted and first chunk emitted");
    assert!(
        editor
            .wait_for_within(Duration::from_millis(10), |message| {
                message["id"] == 200 && message["method"].is_null()
            })
            .is_none(),
        "the long workspace request is still in flight after its first chunk"
    );

    kill_exact_test_daemon(daemon_pid);
    artifacts.checkpoint("exact disposable daemon PID terminated");
    let editor_exit = editor
        .wait_for_exit_within(
            test_utils::wait::ceiling(Duration::from_secs(20)),
            "daemon crash is terminal; no replay",
        )
        .expect("poll the owned LSP proxy")
        .expect("LSP proxy exits after its backend disappears");
    assert!(!editor_exit.success(), "unexpected backend crash is reported as failure");
    let mcp_terminal = tokio::time::timeout(
        test_utils::wait::ceiling(Duration::from_secs(10)),
        mcp.call_tool(CallToolRequestParams::new("symbol_info").with_arguments(Map::from_iter([
            ("symbol".to_owned(), json!("ОбщийМодуль.ЧисловоеЗначение")),
        ]))),
    )
    .await;
    assert!(
        matches!(mcp_terminal, Ok(Err(_))),
        "the MCP proxy also terminates instead of replaying its request: {mcp_terminal:?}"
    );
    let current = backend_markers(runtime.path());
    assert_eq!(
        current.iter().filter(|line| line.contains("owner_init_start")).count(),
        1,
        "EOF without ScopeRetired does not rebind or launch a hidden recovery epoch"
    );
    assert_eq!(
        current
            .iter()
            .filter(|line| line.contains("session_attached")
                && trace_field(line, "adapter") == Some("lsp"))
            .count(),
        1,
        "the crashed LSP session is not replayed into another owner"
    );

    let mut restarted = Lsp::start(root.path());
    let caller = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    restarted.open(&caller, CALLER);
    let restarted_markers = wait_for_owner_initializers(runtime.path(), 2).await;
    let epochs = owner_epochs(&restarted_markers);
    assert_eq!(epochs.len(), 2, "the ordinary next startup creates a new owner epoch");
    assert!(epochs.iter().any(|epoch| *epoch != old_epoch));
    let definition = restarted.request(
        2,
        "textDocument/definition",
        json!({
            "textDocument": {"uri": lsp_types::Url::from_file_path(&caller).unwrap()},
            "position": {"line": 2, "character": 27}
        }),
    );
    assert!(definition["error"].is_null(), "fresh didOpen recovers the document: {definition}");
    assert!(
        definition["result"].is_array() || definition["result"]["uri"].is_string(),
        "the new epoch resolves a known definition: {definition}"
    );

    let _ = tokio::time::timeout(Duration::from_secs(3), mcp_process.wait()).await;
    mcp_process.kill().await.ok();
    let _ = mcp_process.wait().await;
    assert!(restarted.shutdown(3).success(), "the recovered editor shuts down cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_stdio_lsp_and_mcp_use_the_core_without_starting_a_shared_daemon() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable cache");
    let runtime = tempfile::tempdir().expect("disposable runtime");
    let mut artifacts = FailureArtifacts {
        root: root.path().to_path_buf(),
        runtime: runtime.path().to_path_buf(),
        attach_order: "s21-explicit-stdio",
        phase: "setup".to_owned(),
    };
    let _env = isolated_backend_env(cache.path(), runtime.path());
    let caller = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    let target = root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl");

    let mut editor = Lsp::start_stdio(root.path());
    artifacts.checkpoint("explicit LSP stdio initialized");
    let diagnostics = editor.open(&caller, CALLER);
    assert_eq!(diagnostics["method"], "textDocument/publishDiagnostics");
    let definition = editor.request(
        2,
        "textDocument/definition",
        json!({
            "textDocument": {"uri": lsp_types::Url::from_file_path(&caller).unwrap()},
            "position": {"line": 2, "character": 27}
        }),
    );
    assert!(definition["error"].is_null(), "stdio LSP resolves the known target: {definition}");
    let location = definition["result"].as_array().map_or(&definition["result"], |items| &items[0]);
    assert_eq!(location["uri"], lsp_types::Url::from_file_path(&target).unwrap().as_str());

    let (mcp, mut mcp_process) = mcp_client_mode(root.path(), "stdio").await;
    let disk = wait_disk_type(&mcp, "Число").await;
    assert_eq!(disk["return_type"], "Число", "stdio MCP remains disk-backed: {disk}");
    artifacts.checkpoint("explicit MCP stdio serves the disk fixture");
    assert!(
        backend_markers(runtime.path()).is_empty(),
        "explicit stdio adapters do not create a shared daemon"
    );
    mcp.cancel().await.ok();
    if tokio::time::timeout(Duration::from_secs(10), mcp_process.wait()).await.is_err() {
        mcp_process.kill().await.ok();
        let _ = mcp_process.wait().await;
    }
    assert!(editor.shutdown(3).success(), "stdio LSP shuts down cleanly");
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_lsp_preserves_case_alias_uri_after_open_file_is_deleted() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable cache");
    let runtime = tempfile::tempdir().expect("disposable runtime");
    let mut artifacts = FailureArtifacts {
        root: root.path().to_path_buf(),
        runtime: runtime.path().to_path_buf(),
        attach_order: "windows-case-alias",
        phase: "setup".to_owned(),
    };
    let _env = isolated_backend_env(cache.path(), runtime.path());
    let caller = root.path().join("CommonModules/Потребитель/Ext/Module.bsl");
    let original_uri = lsp_types::Url::from_file_path(&caller).unwrap();
    let alias =
        lsp_types::Url::parse(&original_uri.as_str().replace("/Ext/Module.bsl", "/eXT/mODULE.bSL"))
            .unwrap();
    assert_ne!(alias, original_uri, "the test must use a different path spelling");
    assert!(alias.to_file_path().unwrap().is_file(), "the alias resolves on Windows");

    let mut editor = Lsp::start(root.path());
    editor.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": {"textDocument": {
            "uri": alias, "languageId": "bsl", "version": 1, "text": CALLER
        }}
    }));
    editor.wait_for(|message| {
        message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == alias.as_str()
            && message["params"]["version"] == 1
    });
    artifacts.checkpoint("shared diagnostics preserve the original case alias");

    std::fs::remove_file(&caller).unwrap();
    watched_file(&mut editor, &caller, 3);
    editor.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didChange",
        "params": {
            "textDocument": {"uri": alias, "version": 2},
            "contentChanges": [{"text": "Процедура БуферПослеУдаления() Экспорт\nКонецПроцедуры\n"}]
        }
    }));
    editor.wait_for(|message| {
        message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == alias.as_str()
            && message["params"]["version"] == 2
    });
    let symbols = editor.request(
        2,
        "textDocument/documentSymbol",
        json!({
            "textDocument": {"uri": alias}
        }),
    );
    assert!(symbols["error"].is_null(), "the open buffer survives deletion: {symbols}");
    assert_eq!(symbols["result"][0]["name"], "БуферПослеУдаления");
    artifacts.checkpoint("deleted file remains available through the open alias buffer");
    assert!(editor.shutdown(3).success(), "the alias editor shuts down cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_lsp_falls_back_before_initialize_forwarding_without_replaying_public_frames() {
    let _environment = PROTOCOL_ENV_LOCK.lock().await;
    let root = fixture();
    let cache = tempfile::tempdir().expect("disposable cache");
    let runtime = tempfile::tempdir().expect("disposable runtime");
    let mut artifacts = FailureArtifacts {
        root: root.path().to_path_buf(),
        runtime: runtime.path().to_path_buf(),
        attach_order: "s21-auto-fallback",
        phase: "setup".to_owned(),
    };
    let blocker = cache.path().join("cache-is-a-file");
    std::fs::write(&blocker, "synthetic invalid cache path\n").unwrap();
    let mut env = isolated_backend_env(cache.path(), runtime.path());
    env.set("BSL_CACHE_DIR", &blocker);

    let mut editor = Lsp::start(root.path());
    artifacts.checkpoint("auto LSP completed pre-session stdio fallback");
    assert_eq!(editor.initialize_response["id"], 1, "one public initialize response");
    let diagnostics =
        editor.open(&root.path().join("CommonModules/Потребитель/Ext/Module.bsl"), CALLER);
    assert_eq!(diagnostics["method"], "textDocument/publishDiagnostics");
    let fallback_deadline =
        std::time::Instant::now() + test_utils::wait::ceiling(Duration::from_secs(2));
    while !editor.stderr_text().contains("LSP backend unavailable before initialize; using stdio")
        && std::time::Instant::now() < fallback_deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    let stderr = editor.stderr_text();
    assert!(
        stderr.contains("LSP backend unavailable before initialize; using stdio"),
        "the pre-session fallback is explicit in stderr: {stderr}"
    );
    assert!(
        editor
            .wait_for_within(Duration::from_millis(250), |message| {
                message["id"] == 1 && message["method"].is_null()
            })
            .is_none(),
        "fallback does not replay public initialize or emit a second reply"
    );
    assert!(
        backend_markers(runtime.path()).is_empty(),
        "a failed pre-session endpoint does not start a daemon"
    );
    assert!(editor.shutdown(3).success(), "fallback LSP shuts down cleanly");
}
