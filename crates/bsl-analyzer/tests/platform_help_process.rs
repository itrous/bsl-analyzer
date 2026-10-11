//! The configured platform help source serves a real process from its first
//! request — LSP and batch alike — and a changed source takes effect only after
//! a restart.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::Lsp;
use serde_json::{json, Value};

const MODULE: &str =
    "Процедура Тест()\n    Список = Новый Массив;\n    Список.Добавить(1);\nКонецПроцедуры\n";
const FIXTURE: &str = include_str!("../../bsl-platform/tests/fixtures/help/corpus.json");

/// The fixture corpus with the `Массив.Добавить` description replaced by `text`.
fn corpus_with_add_text(text: &str) -> String {
    let mut corpus: Value = serde_json::from_str(FIXTURE).unwrap();
    let add = corpus["methods"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|m| m["type_name"] == "Array" && m["english_name"] == "Add")
        .unwrap();
    add["documentation"]["description"] = Value::String(text.to_owned());
    serde_json::to_string(&corpus).unwrap()
}

fn project(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Module.bsl"), MODULE).unwrap();
    std::fs::write(dir.path().join("a.json"), corpus_with_add_text("Corpus A text.")).unwrap();
    std::fs::write(dir.path().join("b.json"), corpus_with_add_text("Corpus B text.")).unwrap();
    std::fs::write(dir.path().join("bsl-analyzer.toml"), config).unwrap();
    dir
}

fn external(file: &str) -> String {
    format!("[platform_help]\nsource = \"external\"\npath = \"{file}\"\n")
}

fn analyze(dir: &Path) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-q", "-s"])
        .arg(dir)
        .env("BSL_LOG", "info")
        .output()
        .unwrap();
    (output.status.success(), String::from_utf8_lossy(&output.stderr).into_owned())
}

#[test]
fn batch_reports_the_configured_source_or_why_it_is_missing() {
    let dir = project(&external("a.json"));
    let (ok, log) = analyze(dir.path());
    assert!(ok, "{log}");
    assert!(log.contains("platform help loaded") && log.contains("a.json"), "{log}");

    std::fs::write(dir.path().join("bsl-analyzer.toml"), "[platform_help]\nsource = \"none\"\n")
        .unwrap();
    let (ok, log) = analyze(dir.path());
    assert!(ok, "{log}");
    assert!(log.contains("platform help unavailable") && log.contains("disabled"), "{log}");

    // A wrong explicit path is reported, never replaced by another source.
    std::fs::write(dir.path().join("bsl-analyzer.toml"), external("absent.json")).unwrap();
    let (ok, log) = analyze(dir.path());
    assert!(ok, "{log}");
    assert!(log.contains("platform help unavailable") && log.contains("absent.json"), "{log}");
    assert!(!log.contains("platform help loaded"), "{log}");

    std::fs::write(dir.path().join("bsl-analyzer.toml"), "[platform_help]\nsource = \"process\"\n")
        .unwrap();
    let (ok, log) = analyze(dir.path());
    assert!(!ok, "an unknown source is a configuration error: {log}");
}

/// Hover over `Добавить`. A config reload re-derives the project, and until it
/// settles the server may answer `null`; the request is repeated over that window.
fn hover_add(lsp: &mut Lsp, file: &Path, first_id: u64) -> String {
    let uri = lsp_types::Url::from_file_path(file).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    for id in first_id.. {
        lsp.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "textDocument/hover",
            "params": {"textDocument": {"uri": uri}, "position": {"line": 2, "character": 12}}
        }));
        let result = lsp.wait_for(|message| message["id"] == id)["result"].clone();
        if !result.is_null() || std::time::Instant::now() > deadline {
            return result.to_string();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    unreachable!()
}

struct McpChild(Child);

impl Drop for McpChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn mcp_add_docs(root: &Path, profile: &str) -> String {
    let cache_root = tempfile::tempdir().unwrap();
    let mut child = McpChild(
        Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(["mcp", "serve", "--mode", "stdio", "--profile", profile, "--source-dir"])
            .arg(root)
            .current_dir(root)
            .env("BSL_MCP_BROKER", "0")
            .env("XDG_CACHE_HOME", cache_root.path().join("cache"))
            .env("XDG_STATE_HOME", cache_root.path().join("state"))
            .env("BSL_PLATFORM_HELP_CACHE_DIR", cache_root.path().join("help-cache"))
            .env_remove("BSL_ONEC_CONNECTIONS_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = child.0.stderr.take().unwrap();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.by_ref().take(16 * 1024).read_to_end(&mut bytes);
        let _ = stderr_tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    let mut stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(serde_json::from_str::<Value>(&line).unwrap()).is_err() {
                break;
            }
        }
    });
    let mut send = |message: Value| {
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    };
    send(json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
        "protocolVersion":"2025-06-18", "capabilities":{},
        "clientInfo":{"name":"help-fixture", "version":"1"}
    }}));
    let mut receive = |id: u64| {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let message = rx.recv_timeout(remaining).unwrap_or_else(|error| {
                let status = child.0.try_wait().ok().flatten();
                let stderr = stderr_rx
                    .recv_timeout(Duration::from_millis(250))
                    .unwrap_or_else(|_| "<stderr still open or unavailable>".to_owned());
                panic!(
                    "MCP response channel ended ({error}); child status: {status:?}; stderr (first 16 KiB): {stderr}"
                );
            });
            if message["id"] == id {
                return message;
            }
        }
    };
    let initialized = receive(1);
    assert!(initialized.get("result").is_some(), "{initialized}");
    send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{
        "name":"syntax_help", "arguments":{"name":"Добавить", "type_name":"Массив"}
    }}));
    let response = receive(2);
    assert!(response.get("error").is_none(), "{response}");
    response["result"].to_string()
}

#[test]
fn mcp_profiles_serve_the_configured_corpus_from_the_first_tool_call() {
    let dir = project(&external("a.json"));
    for profile in ["reference", "workspace"] {
        for (file, text, absent) in [
            ("a.json", "Corpus A text.", "Corpus B text."),
            ("b.json", "Corpus B text.", "Corpus A text."),
        ] {
            std::fs::write(dir.path().join("bsl-analyzer.toml"), external(file)).unwrap();
            let response = mcp_add_docs(dir.path(), profile);
            assert!(response.contains(text), "{profile}: {response}");
            assert!(!response.contains(absent), "{profile}: old corpus leaked: {response}");
        }
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[platform_help]\nsource = \"none\"\n",
        )
        .unwrap();
        let missing = mcp_add_docs(dir.path(), profile);
        assert!(
            !missing.contains("Corpus A text.") && !missing.contains("Corpus B text."),
            "{profile}: {missing}"
        );
    }
}

#[test]
fn lsp_serves_the_configured_corpus_until_restart() {
    let dir = project(&external("a.json"));
    let file = dir.path().join("Module.bsl");

    let mut lsp = Lsp::start(dir.path());
    lsp.open(&file, MODULE);
    let first = hover_add(&mut lsp, &file, 100);
    assert!(
        first.contains("Corpus A text."),
        "first request answers from the configured corpus: {first}"
    );

    std::fs::write(dir.path().join("bsl-analyzer.toml"), external("b.json")).unwrap();
    let warning = common::Provocation::start(|| {
        std::fs::write(dir.path().join("bsl-analyzer.toml"), external("b.json")).unwrap();
    });
    let shown = loop {
        match lsp.wait_for_within(std::time::Duration::from_secs(2), |message| {
            // `fs::write` truncates before it writes, so the server can read the empty
            // file and report a switch to the default source first; only the warning
            // that names the new corpus answers this write.
            message["method"] == "window/showMessage"
                && message["params"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("restart") && m.contains("b.json"))
        }) {
            Some(message) => break message,
            None => {
                warning.again();
                lsp.poke();
            }
        }
    };
    assert!(shown.to_string().contains("b.json"), "{shown}");
    let still = hover_add(&mut lsp, &file, 1000);
    assert!(still.contains("Corpus A text."), "the running server keeps its snapshot: {still}");
    drop(lsp);

    let mut restarted = Lsp::start(dir.path());
    restarted.open(&file, MODULE);
    let after = hover_add(&mut restarted, &file, 2000);
    assert!(after.contains("Corpus B text."), "a restart picks up the new source: {after}");
}

/// Serves `body` at every path until dropped, counting requests.
struct CorpusHost {
    url: String,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CorpusHost {
    fn start(body: Vec<u8>) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/corpus/platform_data.json", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (counter, stopped) = (requests.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    line.clear();
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let mut stream = stream;
                let response = if stopped.load(Ordering::SeqCst) {
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                } else {
                    let mut head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    head.extend_from_slice(&body);
                    head
                };
                let _ = stream.write_all(&response);
            }
        });
        Self { url, requests, stop }
    }

    fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[test]
fn unconfigured_batch_downloads_the_pinned_corpus_once() {
    let dir = project("");
    let corpus = corpus_with_add_text("Pinned corpus text.").into_bytes();
    let digest = platform_help::package::sha256_hex(&corpus);
    let host = CorpusHost::start(corpus);
    let run = || {
        let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(["analyze", "-q", "-s"])
            .arg(dir.path())
            .env("BSL_LOG", "info")
            .env("BSL_PLATFORM_HELP_PINNED_URL", &host.url)
            .env("BSL_PLATFORM_HELP_CACHE_DIR", dir.path().join("help-cache"))
            // An unusable explicit installation path keeps discovery away from
            // whatever platform this machine has.
            .env("BSL_PLATFORM_PATH", dir.path().join("no-platform"))
            .env_remove(bsl_platform::CORPUS_ENV)
            .output()
            .unwrap();
        (output.status.success(), String::from_utf8_lossy(&output.stderr).into_owned())
    };

    let (ok, log) = run();
    assert!(ok, "{log}");
    assert!(log.contains("platform help loaded") && log.contains(&host.url), "{log}");
    assert!(log.contains(&digest), "{log}");
    assert_eq!(host.requests(), 1);

    host.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let (ok, log) = run();
    assert!(ok, "{log}");
    assert!(log.contains("platform help loaded") && log.contains(&digest), "{log}");
    assert_eq!(host.requests(), 1, "the second start serves the cached download");

    // Without the cache and with the host refusing, the built-in interface
    // facts serve and the log names why the pinned corpus did not.
    std::fs::remove_dir_all(dir.path().join("help-cache")).unwrap();
    let (ok, log) = run();
    assert!(ok, "{log}");
    assert!(
        log.contains("degraded to the built-in interface facts")
            && log.contains("origin=\"bundled\"")
            && log.contains("pinned corpus")
            && log.contains("503"),
        "{log}"
    );
}
