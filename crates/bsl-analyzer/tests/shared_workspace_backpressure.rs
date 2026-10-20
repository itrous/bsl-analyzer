use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

static TEST_ENV: Mutex<()> = Mutex::new(());

struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvGuard {
    fn new() -> Self {
        Self(Vec::new())
    }

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
        for (key, value) in self.0.drain(..) {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

fn set_isolated_env(env: &mut EnvGuard, cache: &Path, runtime: &Path) {
    env.set("BSL_CACHE_DIR", cache);
    env.set("BSL_SHARED_BACKEND_MARKERS", "1");
    env.set("BSL_LOG", "info");
    env.set("BSL_MCP_IDLE_TTL_SECS", "1");
    env.set("BSL_MCP_ORPHAN_GRACE_SECS", "1");
    env.set("BSL_MCP_DAEMON_LOG", runtime.join("shared-daemon.log"));
    env.set("XDG_RUNTIME_DIR", runtime);
    env.set("TMP", runtime);
    env.set("TEMP", runtime);
    env.set("TMPDIR", runtime);
    for key in [
        "BSL_ONEC_PASSWORD",
        "BSL_ONEC_CONNECTIONS_FILE",
        "BSL_LOG_FILE",
        "EMBEDDING_API_KEY",
        "EMBEDDING_URL",
        "EMBEDDING_MODEL",
        "EMBEDDING_PROVIDER",
        "EMBEDDING_QUERY_PREFIX",
        "EMBEDDING_DOCUMENT_PREFIX",
        "BSL_TEST_SHARED_METADATA_BOOTSTRAP_GATE",
        "BSL_TEST_SHARED_EDITOR_SEED_GATE",
        "BSL_TEST_SHARED_LSP_EVENT_GATE",
    ] {
        env.remove(key);
    }
}

fn copy_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("disposable source workspace");
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shared_workspace_analysis");
    for entry in walkdir::WalkDir::new(&source) {
        let entry = entry.expect("fixture entry");
        let target =
            root.path().join(entry.path().strip_prefix(&source).expect("fixture-relative path"));
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

fn metadata_gate(root: &Path) -> PathBuf {
    let gate = root.join("metadata-gate");
    std::fs::create_dir_all(&gate).expect("metadata gate directory");
    gate
}

fn wait_for_file(path: &Path, timeout: Duration, label: &str, stderr: &str) {
    let deadline = Instant::now() + timeout;
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {label}: {}; subprocess stderr: {stderr}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Gate(PathBuf);

impl Gate {
    fn wait_until_held(&self, label: &str, lsp: &ProcessLsp) {
        wait_for_file(
            &self.0.join("entered"),
            test_utils::wait::ceiling(Duration::from_secs(30)),
            label,
            &lsp.stderr_tail(),
        );
    }

    fn release(&self) {
        std::fs::write(self.0.join("release"), b"release").expect("release test gate");
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("release"), b"release");
    }
}

fn shared_markers(runtime: &Path) -> Vec<String> {
    walkdir::WalkDir::new(runtime)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.file_name().to_string_lossy().starts_with("shared-")
                && entry.path().extension().is_some_and(|extension| extension == "log")
        })
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .flat_map(|log| log.lines().map(str::to_owned).collect::<Vec<_>>())
        .collect()
}

fn wait_for_event(runtime: &Path, event: &str, lsp: &ProcessLsp) -> Vec<String> {
    let deadline = Instant::now() + test_utils::wait::ceiling(Duration::from_secs(45));
    loop {
        let markers = shared_markers(runtime);
        if markers.iter().any(|line| line.contains(event)) {
            return markers;
        }
        assert!(
            Instant::now() < deadline,
            "missing {event} in shared backend logs: {markers:?}; stderr: {}",
            lsp.stderr_tail()
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn events(markers: &[String], event: &str) -> usize {
    markers.iter().filter(|line| line.contains(event)).count()
}

struct OwnedDaemonCleanup(PathBuf);

impl OwnedDaemonCleanup {
    fn new(runtime: &Path) -> Self {
        Self(runtime.to_owned())
    }
}

impl Drop for OwnedDaemonCleanup {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let artifacts = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.artifacts/issue420-implementation")
                .join(format!(
                    "backpressure-failure-{}-{}",
                    std::process::id(),
                    self.0.file_name().unwrap_or_default().to_string_lossy()
                ));
            if std::fs::create_dir_all(&artifacts).is_ok() {
                let logs = shared_markers(&self.0);
                let tail = logs[logs.len().saturating_sub(160)..].join("\n");
                let _ = std::fs::write(artifacts.join("backend.log"), tail);
                eprintln!("preserved backend failure phases at {}", artifacts.display());
            }
        }
        let Some(pid) = shared_markers(&self.0)
            .iter()
            .find(|line| line.contains("owner_init_start"))
            .and_then(|line| {
                line.split_whitespace().find_map(|field| field.strip_prefix("daemon_pid="))
            })
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            return;
        };
        if !is_test_daemon_executable(pid) {
            return;
        }
        let pid = pid.to_string();
        #[cfg(unix)]
        let _ = Command::new("kill").args(["-KILL", pid.as_str()]).status();
        #[cfg(windows)]
        let _ = Command::new("taskkill").args(["/PID", pid.as_str(), "/F"]).status();
    }
}

fn is_test_daemon_executable(pid: u32) -> bool {
    let expected = std::fs::canonicalize(env!("CARGO_BIN_EXE_bsl-analyzer-app")).ok();
    #[cfg(target_os = "linux")]
    let actual = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    #[cfg(windows)]
    let actual = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Process -Id {pid} -ErrorAction Stop).Path"),
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|path| std::fs::canonicalize(path.trim()).ok());
    #[cfg(not(any(target_os = "linux", windows)))]
    let actual: Option<PathBuf> = None;
    let (Some(expected), Some(actual)) = (expected, actual) else { return false };
    #[cfg(windows)]
    {
        expected.to_string_lossy().eq_ignore_ascii_case(&actual.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        expected == actual
    }
}

fn read_lsp_frame(reader: &mut impl BufRead) -> std::io::Result<Value> {
    let mut length = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "LSP stdout closed",
            ));
        }
        if header == "\r\n" {
            break;
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            length =
                Some(value.trim().parse::<usize>().map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                })?);
        }
    }
    let length = length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

struct ProcessLsp {
    child: Child,
    outbound: mpsc::Sender<Value>,
    inbound: mpsc::Receiver<Value>,
    written_frames: Arc<AtomicUsize>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    diagnostic_events: Arc<Mutex<Vec<Value>>>,
    next_id: u64,
}

impl ProcessLsp {
    fn start(root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .arg("lsp")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start LSP subprocess");
        let stdin = child.stdin.take().expect("LSP stdin");
        let stdout = child.stdout.take().expect("LSP stdout");
        let mut stderr = child.stderr.take().expect("LSP stderr");
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_capture = Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            let mut bytes = [0u8; 4096];
            while let Ok(read) = stderr.read(&mut bytes) {
                if read == 0 {
                    break;
                }
                let mut tail = stderr_capture.lock().unwrap_or_else(|poison| poison.into_inner());
                tail.extend_from_slice(&bytes[..read]);
                if tail.len() > 8192 {
                    let excess = tail.len() - 8192;
                    tail.drain(..excess);
                }
            }
        });

        let written_frames = Arc::new(AtomicUsize::new(0));
        let writer_count = Arc::clone(&written_frames);
        let (outbound, writer) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            let mut stdin = stdin;
            while let Ok(message) = writer.recv() {
                let body = message.to_string();
                if write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)
                    .and_then(|_| stdin.flush())
                    .is_err()
                {
                    break;
                }
                writer_count.fetch_add(1, Ordering::Release);
            }
        });

        let (incoming, inbound) = mpsc::channel();
        let diagnostic_events = Arc::new(Mutex::new(Vec::new()));
        let diagnostic_capture = Arc::clone(&diagnostic_events);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(message) = read_lsp_frame(&mut reader) {
                if message["method"] == "textDocument/publishDiagnostics" {
                    diagnostic_capture
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .push(message.clone());
                }
                if incoming.send(message).is_err() {
                    break;
                }
            }
        });

        let lsp = Self {
            child,
            outbound,
            inbound,
            written_frames,
            stderr_tail,
            diagnostic_events,
            next_id: 2,
        };
        lsp.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "rootUri": lsp_types::Url::from_directory_path(root).expect("workspace URI"),
                "capabilities": {}
            }
        }));
        let response = lsp
            .wait_for(test_utils::wait::ceiling(Duration::from_secs(20)), |message| {
                message["id"] == 1
            });
        assert!(response["error"].is_null(), "LSP initialize failed: {response}");
        lsp.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
        lsp
    }

    fn send(&self, message: Value) {
        self.outbound.send(message).expect("queue public LSP frame");
    }

    fn wait_for(&self, timeout: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "LSP response deadline exceeded; stderr: {}",
                self.stderr_tail()
            );
            let message = self.inbound.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!("LSP response channel failed: {error}; stderr: {}", self.stderr_tail())
            });
            if predicate(&message) {
                return message;
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.wait_for(test_utils::wait::ceiling(Duration::from_secs(20)), |message| {
            message["id"] == id
        })
    }

    fn wait_all_frames_written(&self, expected: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.written_frames.load(Ordering::Acquire) < expected {
            assert!(
                Instant::now() < deadline,
                "only {} of {expected} public LSP frames reached the child; stderr: {}",
                self.written_frames.load(Ordering::Acquire),
                self.stderr_tail()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_writer_backpressure(&self, before: usize, total: usize) -> usize {
        let deadline = Instant::now() + test_utils::wait::ceiling(Duration::from_secs(10));
        let mut last = before;
        let mut unchanged_since = Instant::now();
        loop {
            let written = self.written_frames.load(Ordering::Acquire);
            assert!(
                written < total,
                "all public frames drained despite the held event loop; stderr: {}",
                self.stderr_tail()
            );
            if written != last {
                last = written;
                unchanged_since = Instant::now();
            } else if written > before && unchanged_since.elapsed() >= Duration::from_millis(250) {
                return written;
            }
            assert!(
                Instant::now() < deadline,
                "writer never reached observable backpressure; stderr: {}",
                self.stderr_tail()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn shutdown(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "shutdown", "params": null}));
        let response = self
            .wait_for(test_utils::wait::ceiling(Duration::from_secs(10)), |message| {
                message["id"] == id
            });
        assert!(
            response["error"].is_null(),
            "LSP shutdown failed: {response}; stderr: {}",
            self.stderr_tail()
        );
        self.send(json!({"jsonrpc": "2.0", "method": "exit", "params": null}));
        let deadline = Instant::now() + test_utils::wait::ceiling(Duration::from_secs(10));
        loop {
            if let Some(status) = self.child.try_wait().expect("poll LSP child") {
                assert!(status.success(), "LSP child exited with {status}");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "LSP child did not exit after shutdown; stderr: {}",
                self.stderr_tail()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stderr_tail(&self) -> String {
        String::from_utf8_lossy(
            &self.stderr_tail.lock().unwrap_or_else(|poison| poison.into_inner()),
        )
        .into_owned()
    }

    fn wait_for_diagnostic_version(&self, uri: &str, version: i64) {
        let deadline = Instant::now() + test_utils::wait::ceiling(Duration::from_secs(30));
        loop {
            let found = self
                .diagnostic_events
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .iter()
                .any(|message| {
                    message["params"]["uri"] == uri && message["params"]["version"] == version
                });
            if found {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "did not publish diagnostics for {uri} at version {version}; stderr: {}",
                self.stderr_tail()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ProcessLsp {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn uri(path: &Path) -> String {
    lsp_types::Url::from_file_path(path).expect("LSP document URI").to_string()
}

fn assert_document_symbols(response: &Value, expected: &[(String, usize)]) {
    assert!(response["error"].is_null(), "documentSymbol failed: {response}");
    let symbols = response["result"].as_array().expect("documentSymbol result array");
    let expected_names: HashMap<_, _> =
        expected.iter().map(|(name, line)| (name.as_str(), line)).collect();
    let actual: Vec<_> = symbols
        .iter()
        .filter_map(|symbol| {
            let name = symbol["name"].as_str()?;
            if !expected_names.contains_key(name) {
                return None;
            }
            let line = symbol["range"]["start"]["line"].as_u64()? as usize;
            Some((name.to_owned(), line))
        })
        .collect();
    assert_eq!(actual, expected, "documentSymbol lost or reordered edits");
}

#[test]
fn held_metadata_bootstrap_keeps_cold_lsp_live_and_rejected_project_bounded() {
    let _serial = TEST_ENV.lock().unwrap_or_else(|poison| poison.into_inner());
    let root = copy_fixture();
    let source = root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
    let runtime = tempfile::tempdir().expect("disposable runtime outside source root");
    let cache = tempfile::tempdir().expect("disposable cache outside source root");
    let mut env = EnvGuard::new();
    set_isolated_env(&mut env, cache.path(), runtime.path());
    let gate = Gate(metadata_gate(runtime.path()));
    env.set("BSL_TEST_SHARED_METADATA_BOOTSTRAP_GATE", &gate.0);

    let _daemon_cleanup = OwnedDaemonCleanup::new(runtime.path());
    let mut lsp = ProcessLsp::start(root.path());
    gate.wait_until_held("owner source bootstrap before metadata bootstrap", &lsp);
    let markers = wait_for_event(runtime.path(), "resident_bootstrap_start", &lsp);
    assert_eq!(events(&markers, "owner_init_start"), 1, "one shared owner initializer");
    assert_eq!(events(&markers, "resident_bootstrap_start"), 1, "one disk resident build");
    assert_eq!(events(&markers, "workspace_ingress_created"), 1, "one workspace watcher");

    let text = "#Область Public\nПроцедура ТестоваяПроцедура()\n    Если Истина Тогда\n        Сообщить(1);\n    КонецЕсли;\nКонецПроцедуры\n#КонецОбласти";
    let document_uri = uri(&source);
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
            "textDocument": {"uri": document_uri, "languageId": "bsl", "version": 1, "text": text}
        }
    }));
    let params = json!({"textDocument": {"uri": document_uri}});
    let symbols = lsp.request("textDocument/documentSymbol", params.clone());
    assert!(
        symbols["result"][0]["name"] == "Public"
            && symbols["result"][0]["children"][0]["name"] == "ТестоваяПроцедура",
        "cold documentSymbol while metadata bootstrap is held: {symbols}"
    );
    let folds = lsp.request("textDocument/foldingRange", params.clone());
    assert!(
        folds["result"].as_array().is_some_and(|ranges| ranges.len() >= 3),
        "cold foldingRange while metadata bootstrap is held: {folds}"
    );
    let selections = lsp.request(
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": document_uri},
            "positions": [{"line": 3, "character": 16}]
        }),
    );
    assert!(
        selections["result"].as_array().is_some_and(|ranges| ranges.len() == 1),
        "cold selectionRange while metadata bootstrap is held: {selections}"
    );

    gate.release();
    wait_for_event(runtime.path(), "resident_bootstrap_complete", &lsp);
    lsp.shutdown();
    drop(lsp);
    env.remove("BSL_TEST_SHARED_METADATA_BOOTSTRAP_GATE");

    let rejected = tempfile::tempdir().expect("rejected project fixture");
    std::fs::write(rejected.path().join("bsl-analyzer.toml"), "[source\n").unwrap();
    let rejected_source = rejected.path().join("src/Rejected.bsl");
    std::fs::create_dir_all(rejected_source.parent().unwrap()).unwrap();
    std::fs::write(&rejected_source, "Процедура Отклонено()\nКонецПроцедуры\n").unwrap();
    let rejected_runtime = tempfile::tempdir().expect("rejected-project runtime");
    let rejected_cache = tempfile::tempdir().expect("rejected-project cache");
    set_isolated_env(&mut env, rejected_cache.path(), rejected_runtime.path());
    let _rejected_daemon_cleanup = OwnedDaemonCleanup::new(rejected_runtime.path());
    let mut lsp = ProcessLsp::start(rejected.path());
    let rejected_uri = uri(&rejected_source);
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
            "textDocument": {
                "uri": rejected_uri, "languageId": "bsl", "version": 1,
                "text": "Процедура Отклонено()\nКонецПроцедуры\n"
            }
        }
    }));
    let response = lsp.request(
        "textDocument/hover",
        json!({
            "textDocument": {"uri": rejected_uri},
            "position": {"line": 0, "character": 10}
        }),
    );
    assert_eq!(
        response["error"]["code"],
        json!(lsp_server::ErrorCode::ContentModified as i32),
        "rejected project must not return fabricated semantic success: {response}"
    );
    assert!(response["result"].is_null(), "rejected project has no hover result: {response}");
    lsp.shutdown();
}

#[test]
fn large_ordered_public_changes_backpressure_while_editor_seed_is_held() {
    let _serial = TEST_ENV.lock().unwrap_or_else(|poison| poison.into_inner());
    let root = copy_fixture();
    let source = root.path().join("CommonModules/ОбщийМодуль/Ext/Module.bsl");
    let runtime = tempfile::tempdir().expect("disposable runtime outside source root");
    let cache = tempfile::tempdir().expect("disposable cache outside source root");
    let seed_gate = Gate(runtime.path().join("editor-seed-gate"));
    let event_gate = Gate(runtime.path().join("event-loop-gate"));
    let mut env = EnvGuard::new();
    set_isolated_env(&mut env, cache.path(), runtime.path());
    env.set("BSL_TEST_SHARED_EDITOR_SEED_GATE", &seed_gate.0);
    env.set("BSL_TEST_SHARED_LSP_EVENT_GATE", &event_gate.0);

    let _daemon_cleanup = OwnedDaemonCleanup::new(runtime.path());
    let mut lsp = ProcessLsp::start(root.path());
    wait_for_event(runtime.path(), "resident_bootstrap_complete", &lsp);
    let document_uri = uri(&source);
    let padding = "x".repeat(1_500_000);
    let initial = format!("// {padding}\nПроцедура Base()\nКонецПроцедуры\n");
    lsp.send(json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
            "textDocument": {"uri": document_uri, "languageId": "bsl", "version": 1, "text": initial}
        }
    }));
    seed_gate.wait_until_held("prepared nonzero-generation editor seed", &lsp);
    event_gate.wait_until_held("LSP event loop with seed in flight", &lsp);

    const EDITS_PER_CHANGE: usize = 2;
    const CHANGES: usize = 128;
    const PADDING_PER_EDIT: usize = 64 * 1024;
    let pad = "p".repeat(PADDING_PER_EDIT);
    let mut line = 3;
    let mut expected = Vec::with_capacity(CHANGES * EDITS_PER_CHANGE);
    let before = lsp.written_frames.load(Ordering::Acquire);
    for change in 0..CHANGES {
        let mut content_changes = Vec::with_capacity(EDITS_PER_CHANGE);
        for _ in 0..EDITS_PER_CHANGE {
            let name = format!("Edit{:03}", expected.len());
            expected.push((name.clone(), line + 1));
            content_changes.push(json!({
                "range": {
                    "start": {"line": line, "character": 0},
                    "end": {"line": line, "character": 0}
                },
                "text": format!("// {pad}\nПроцедура {name}()\nКонецПроцедуры\n")
            }));
            line += 3;
        }
        lsp.send(json!({
            "jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
                "textDocument": {"uri": document_uri, "version": change + 2},
                "contentChanges": content_changes
            }
        }));
    }
    let total = before + CHANGES;
    // A writer blocked in its write and one about to write look the same, so "stalled" is a
    // quiet window that only the timing profile can read; an ordinary run goes on to the
    // ordering and delivery checks below.
    if cfg!(timing_contract) {
        let stalled_after = lsp.wait_for_writer_backpressure(before, total);
        assert!(stalled_after < total, "bounded public transport should apply backpressure");
        eprintln!("S9: public writer stalled after {stalled_after}/{total} frames");
    }

    event_gate.release();
    lsp.wait_all_frames_written(total, test_utils::wait::ceiling(Duration::from_secs(45)));
    let before_publish =
        lsp.request("textDocument/documentSymbol", json!({"textDocument": {"uri": document_uri}}));
    assert_document_symbols(&before_publish, &expected);
    eprintln!("S9: all ordered edits visible before prepared view publication");

    seed_gate.release();
    wait_for_event(runtime.path(), "lsp_editor_view_seed_published", &lsp);
    let after_publish =
        lsp.request("textDocument/documentSymbol", json!({"textDocument": {"uri": document_uri}}));
    assert_document_symbols(&after_publish, &expected);
    eprintln!("S9: all ordered edits visible after prepared view publication");
    lsp.wait_for_diagnostic_version(&document_uri, 129);
    lsp.shutdown();
}
