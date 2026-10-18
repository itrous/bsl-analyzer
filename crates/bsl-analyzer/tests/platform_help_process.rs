//! The configured platform help source serves a real process from its first
//! request — LSP and batch alike — and a changed source takes effect only after
//! a restart.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::Lsp;
use serde_json::{json, Value};
use test_utils::wait::{hang_guard, HANG_GUARD};

const MODULE: &str =
    "Процедура Тест()\n    Список = Новый Массив;\n    Список.Добавить(1);\nКонецПроцедуры\n";
const FIXTURE: &str = include_str!("../../bsl-platform/tests/fixtures/help/corpus.json");

/// How long a hover goes on being repeated while the server answers `null`.
const HOVER_GUARD: Duration = hang_guard(Duration::from_secs(60));
/// How long an MCP child may take to answer one request.
const MCP_RESPONSE_GUARD: Duration = hang_guard(Duration::from_secs(60));
/// How long a batch run may take to exit and close its output.
const ANALYZE_GUARD: Duration = HANG_GUARD;
const POLL_STEP: Duration = Duration::from_millis(10);

const _: () = assert!(HOVER_GUARD.as_nanos() <= HANG_GUARD.as_nanos());
const _: () = assert!(MCP_RESPONSE_GUARD.as_nanos() <= HANG_GUARD.as_nanos());
const _: () = assert!(ANALYZE_GUARD.as_nanos() <= HANG_GUARD.as_nanos());

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

/// Runs `command` to completion like [`Command::output`], but fails the test for `phase` when
/// the child does not exit, or its output does not close, within `guard`.
fn output_within(command: &mut Command, phase: &str, guard: Duration) -> std::process::Output {
    output_closing_within(command, phase, guard, None)
}

/// [`output_within`] where the output, once the child has exited, has `output_window` of its
/// own to close instead of what is left of `guard`.
fn output_closing_within(
    command: &mut Command,
    phase: &str,
    guard: Duration,
    output_window: Option<Duration>,
) -> std::process::Output {
    /// Streams `pipe` chunk by chunk; the channel closes at EOF.
    fn stream(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(read) => {
                        if tx.send(buffer[..read].to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        });
        rx
    }

    /// What `pipe` delivers until `deadline`, and whether it closed by then.
    fn collect(pipe: &mpsc::Receiver<Vec<u8>>, deadline: Instant) -> (Vec<u8>, bool) {
        let mut bytes = Vec::new();
        loop {
            match pipe.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(chunk) => bytes.extend(chunk),
                Err(mpsc::RecvTimeoutError::Disconnected) => return (bytes, true),
                Err(mpsc::RecvTimeoutError::Timeout) => return (bytes, false),
            }
        }
    }

    let deadline = Instant::now() + guard;
    let mut child =
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let pid = child.id();
    let stdout = stream(child.stdout.take().unwrap());
    let stderr = stream(child.stderr.take().unwrap());
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let (stderr, _) = collect(&stderr, Instant::now() + Duration::from_secs(2));
            let stderr = String::from_utf8_lossy(&stderr);
            panic!("phase {phase}: child {pid} did not exit within {guard:?}; stderr: {stderr}");
        }
        std::thread::sleep(POLL_STEP);
    };
    let output_deadline = output_window.map_or(deadline, |window| Instant::now() + window);
    let receive = |pipe: mpsc::Receiver<Vec<u8>>| {
        let (bytes, closed) = collect(&pipe, output_deadline);
        assert!(closed, "phase {phase}: child {pid} exited but its output stayed open");
        bytes
    };
    let stdout = receive(stdout);
    let stderr = receive(stderr);
    std::process::Output { status, stdout, stderr }
}

fn analyze(dir: &Path) -> (bool, String) {
    let output = output_within(
        Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(["analyze", "-q", "-s"])
            .arg(dir)
            .env("BSL_LOG", "info"),
        "analyze",
        ANALYZE_GUARD,
    );
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
    hover_add_judging(lsp, file, first_id, common::SILENCE, common::BUSY_CEILING, HOVER_GUARD)
}

/// [`hover_add`] with the bounds spelled out: each answer is awaited as
/// [`Lsp::wait_for_judging`] judges it, and `null` answers are repeated over `null_window`.
fn hover_add_judging(
    lsp: &mut Lsp,
    file: &Path,
    first_id: u64,
    silence: Duration,
    busy_ceiling: Duration,
    null_window: Duration,
) -> String {
    let uri = lsp_types::Url::from_file_path(file).unwrap();
    let deadline = Instant::now() + null_window;
    for id in first_id.. {
        lsp.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "textDocument/hover",
            "params": {"textDocument": {"uri": uri}, "position": {"line": 2, "character": 12}}
        }));
        let result = lsp.wait_for_judging(silence, busy_ceiling, |message| message["id"] == id)
            ["result"]
            .clone();
        if !result.is_null() {
            return result.to_string();
        }
        if Instant::now() > deadline {
            lsp.hung("hover answered null", null_window);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unreachable!()
}

/// Waits for the warning that the changed help source needs a restart, repeating `warning`
/// while the server says nothing about it.
fn wait_for_restart_warning(lsp: &mut Lsp, warning: &common::Provocation<impl Fn()>) -> Value {
    loop {
        match lsp.wait_for_within(Duration::from_secs(2), |message| {
            message["method"] == "window/showMessage"
                && message["params"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("restart") && m.contains("b.json"))
        }) {
            Some(message) => break message,
            None => {
                warning.again(lsp, "restart warning");
                lsp.poke();
            }
        }
    }
}

struct McpChild(Child);

impl Drop for McpChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let deadline = Instant::now() + HANG_GUARD;
        while !matches!(self.0.try_wait(), Ok(Some(_))) {
            if Instant::now() >= deadline {
                eprintln!(
                    "MCP child {} did not exit within {HANG_GUARD:?} after kill",
                    self.0.id()
                );
                return;
            }
            std::thread::sleep(POLL_STEP);
        }
    }
}

fn mcp_add_docs(root: &Path, profile: &str) -> String {
    let cache_root = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
    command
        .args(["mcp", "serve", "--mode", "stdio", "--profile", profile, "--source-dir"])
        .arg(root)
        .current_dir(root)
        .env("BSL_MCP_BROKER", "0")
        .env("XDG_CACHE_HOME", cache_root.path().join("cache"))
        .env("XDG_STATE_HOME", cache_root.path().join("state"))
        .env("BSL_PLATFORM_HELP_CACHE_DIR", cache_root.path().join("help-cache"))
        .env_remove("BSL_ONEC_CONNECTIONS_FILE");
    mcp_add_docs_via(command, MCP_RESPONSE_GUARD)
}

/// Runs `command` as the MCP stdio server and asks it for the help on `Массив.Добавить`;
/// an answer that takes longer than `guard` fails the test.
fn mcp_add_docs_via(mut command: Command, guard: Duration) -> String {
    let mut child = McpChild(
        command
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
        let deadline = Instant::now() + guard;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = rx.recv_timeout(remaining).unwrap_or_else(|error| {
                let status = child.0.try_wait().ok().flatten();
                let stderr = stderr_rx
                    .recv_timeout(Duration::from_millis(250))
                    .unwrap_or_else(|_| "<stderr still open or unavailable>".to_owned());
                panic!(
                    "MCP child {} response channel ended ({error}); child status: {status:?}; stderr (first 16 KiB): {stderr}",
                    child.0.id()
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

/// The shared backends an auto-mode LSP launches for `root`, kept in a runtime and cache
/// directory of the test's own and stopped when the test ends.
struct AutoBackends {
    /// Canonical, as the frontend passes it to the daemon.
    root: PathBuf,
    runtime: tempfile::TempDir,
    cache: tempfile::TempDir,
    /// Every daemon [`Self::live_daemons`] has reported, with its start time. One that is
    /// exiting on its own has already lost the command line that identifies it, yet is not gone.
    #[cfg(target_os = "linux")]
    seen: std::sync::Mutex<std::collections::BTreeMap<u32, u64>>,
}

impl AutoBackends {
    fn new(root: &Path) -> Self {
        Self {
            root: root.canonicalize().unwrap(),
            runtime: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
            #[cfg(target_os = "linux")]
            seen: Default::default(),
        }
    }

    /// An auto-mode `lsp` command whose backends live under this fixture.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
        command
            .arg("lsp")
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("XDG_CACHE_HOME", self.cache.path().join("cache"))
            .env("XDG_STATE_HOME", self.cache.path().join("state"))
            .env("BSL_PLATFORM_HELP_CACHE_DIR", self.cache.path().join("help-cache"))
            .env_remove("BSL_CACHE_DIR")
            .env_remove("BSL_ONEC_CONNECTIONS_FILE");
        command
    }

    /// The command lines of the live daemons serving this root.
    #[cfg(target_os = "linux")]
    fn daemons(&self) -> Vec<(u32, Vec<String>)> {
        let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_bsl-analyzer-app")).unwrap();
        let root = self.root.to_string_lossy();
        let serves_root = |args: &[String]| {
            args.windows(2).any(|pair| pair[0] == "--mode" && pair[1] == "daemon")
                && args.windows(2).any(|pair| pair[0] == "--source-dir" && pair[1] == root)
        };
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter(|&pid| !process_gone(pid))
            .filter(|pid| {
                std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe == binary)
            })
            .filter_map(|pid| {
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
                let args: Vec<String> = cmdline
                    .split(|&byte| byte == 0)
                    .filter(|arg| !arg.is_empty())
                    .map(|arg| String::from_utf8_lossy(arg).into_owned())
                    .collect();
                serves_root(&args).then_some((pid, args))
            })
            .collect()
    }

    #[cfg(not(target_os = "linux"))]
    fn daemons(&self) -> Vec<(u32, Vec<String>)> {
        Vec::new()
    }

    fn live_daemons(&self) -> Vec<u32> {
        let live: Vec<u32> = self.daemons().into_iter().map(|(pid, _)| pid).collect();
        #[cfg(target_os = "linux")]
        {
            let mut seen = self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            for &pid in &live {
                if let Some(started) = start_time(pid) {
                    seen.entry(pid).or_insert(started);
                }
            }
        }
        live
    }

    #[cfg(target_os = "linux")]
    fn daemon_cache_dirs(&self) -> Vec<PathBuf> {
        self.daemons()
            .into_iter()
            .filter_map(|(_, args)| {
                let at = args.iter().position(|arg| arg == "--cache-dir")?;
                args.get(at + 1).map(PathBuf::from)
            })
            .collect()
    }

    /// Kills this root's daemons and waits until none is live and every one seen has exited.
    fn stop_daemons(&self) -> Result<(), String> {
        for pid in self.live_daemons() {
            let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
        }
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            #[cfg(target_os = "linux")]
            let live = {
                let mut live = self.live_daemons();
                let seen = self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                // A pid whose start time changed was reused by another process.
                live.extend(seen.iter().filter_map(|(&pid, &started)| {
                    (!process_gone(pid) && start_time(pid) == Some(started)).then_some(pid)
                }));
                live
            };
            #[cfg(not(target_os = "linux"))]
            let live = self.live_daemons();
            if live.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "daemons {live:?} of {} still live {HANG_GUARD:?} after kill",
                    self.root.display()
                ));
            }
            std::thread::sleep(POLL_STEP);
        }
    }

    /// The shared backend logs under the fixture's runtime directory.
    fn logs(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("shared-") && name.ends_with(".log"))
                {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(self.runtime.path(), &mut found);
        found
    }
}

impl Drop for AutoBackends {
    fn drop(&mut self) {
        if let Err(error) = self.stop_daemons() {
            eprintln!("{error}");
        }
        if std::thread::panicking() {
            for log in self.logs() {
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                eprintln!(
                    "--- last lines of {} ---\n{}",
                    log.display(),
                    lines[lines.len().saturating_sub(40)..].join("\n")
                );
            }
        }
    }
}

#[test]
fn lsp_serves_the_configured_corpus_until_restart() {
    let dir = project(&external("a.json"));
    let file = dir.path().join("Module.bsl");
    let backends = AutoBackends::new(dir.path());

    let mut lsp = Lsp::start_command(backends.command(), dir.path(), json!({}), common::SILENCE);
    lsp.open(&file, MODULE);
    let first = hover_add(&mut lsp, &file, 100);
    assert!(
        first.contains("Corpus A text."),
        "first request answers from the configured corpus: {first}"
    );
    let mut frontends = vec![lsp.child.id()];
    #[cfg(target_os = "linux")]
    let first_daemons = backends.live_daemons();
    #[cfg(target_os = "linux")]
    {
        let logs = backends.logs();
        assert!(!logs.is_empty(), "the backend logs into the test's runtime directory");
        let cache = backends.cache.path().canonicalize().unwrap();
        let cache_dirs = backends.daemon_cache_dirs();
        assert!(!cache_dirs.is_empty(), "the daemon names its cache directory");
        for cache_dir in cache_dirs {
            let resolved = cache_dir.canonicalize().unwrap();
            assert!(resolved.starts_with(&cache), "daemon cache {resolved:?} outside {cache:?}");
        }
        assert!(!first_daemons.is_empty(), "auto mode serves through a shared backend");
        assert!(first_daemons.iter().all(|&pid| !process_gone(pid)), "{first_daemons:?}");
    }

    let warning = common::Provocation::start(|| {
        // An in-place write can expose an empty config and emit an unrelated restart warning.
        let temporary_config = dir.path().join("bsl-analyzer.toml.next");
        std::fs::write(&temporary_config, external("b.json")).unwrap();
        std::fs::rename(temporary_config, dir.path().join("bsl-analyzer.toml")).unwrap();
    });
    let shown = wait_for_restart_warning(&mut lsp, &warning);
    assert!(shown.to_string().contains("b.json"), "{shown}");
    let still = hover_add(&mut lsp, &file, 1000);
    assert!(still.contains("Corpus A text."), "the running server keeps its snapshot: {still}");
    drop(lsp);

    let mut restarted =
        Lsp::start_command(backends.command(), dir.path(), json!({}), common::SILENCE);
    frontends.push(restarted.child.id());
    restarted.open(&file, MODULE);
    let after = hover_add(&mut restarted, &file, 2000);
    assert!(after.contains("Corpus B text."), "a restart picks up the new source: {after}");
    #[cfg(target_os = "linux")]
    let second_daemons: Vec<u32> =
        backends.live_daemons().into_iter().filter(|pid| !first_daemons.contains(pid)).collect();
    #[cfg(target_os = "linux")]
    {
        assert!(!second_daemons.is_empty(), "the restart serves through a backend of its own");
        assert!(second_daemons.iter().all(|&pid| !process_gone(pid)), "{second_daemons:?}");
    }

    drop(restarted);
    backends.stop_daemons().unwrap();
    #[cfg(target_os = "linux")]
    for pid in frontends.iter().chain(&first_daemons).chain(&second_daemons) {
        assert!(leaves(*pid), "process {pid} outlived the test");
    }
}

/// Serves `body` at every path, counting requests.
struct CorpusHost {
    url: String,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl CorpusHost {
    fn start(body: Vec<u8>) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/corpus/platform_data.json", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    line.clear();
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let mut stream = stream;
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(&body);
                let _ = stream.write_all(&response);
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[test]
fn unconfigured_batch_without_a_platform_serves_the_facts_and_stays_off_the_network() {
    let dir = project("");
    let host = CorpusHost::start(corpus_with_add_text("Hosted corpus text.").into_bytes());
    let output = output_within(
        Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(["analyze", "-q", "-s"])
            .arg(dir.path())
            .env("BSL_LOG", "info")
            .env_remove("BSL_PLATFORM_HELP_PINNED_URL")
            .env("BSL_PLATFORM_HELP_CACHE_DIR", dir.path().join("help-cache"))
            // An unusable explicit installation path keeps discovery away from
            // whatever platform this machine has.
            .env("BSL_PLATFORM_PATH", dir.path().join("no-platform"))
            .env_remove(bsl_platform::CORPUS_ENV),
        "analyze",
        ANALYZE_GUARD,
    );
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{log}");
    assert!(
        log.contains("degraded to the built-in interface facts")
            && log.contains("origin=\"bundled\""),
        "{log}"
    );
    assert_eq!(host.requests(), 0);
    assert!(!dir.path().join("help-cache").join("pinned").exists());
}

#[test]
fn the_retired_corpus_mirror_variable_is_an_error_not_a_silent_fallback() {
    let dir = project("");
    let host = CorpusHost::start(corpus_with_add_text("Hosted corpus text.").into_bytes());
    let output = output_within(
        Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(["analyze", "-q", "-s"])
            .arg(dir.path())
            .env("BSL_LOG", "info")
            .env("BSL_PLATFORM_HELP_PINNED_URL", &host.url)
            .env("BSL_PLATFORM_HELP_CACHE_DIR", dir.path().join("help-cache"))
            .env("BSL_PLATFORM_PATH", dir.path().join("no-platform"))
            .env_remove(bsl_platform::CORPUS_ENV),
        "analyze",
        ANALYZE_GUARD,
    );
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(
        log.contains("BSL_PLATFORM_HELP_PINNED_URL is no longer supported")
            && log.contains("external"),
        "{log}"
    );
    assert_eq!(host.requests(), 0, "the variable's host is never contacted");
}

fn analyze_with_corpus_variable(dir: &Path, value: &str) -> (bool, String) {
    let output = output_within(
        Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(["analyze", "-q", "-s"])
            .arg(dir)
            .env("BSL_LOG", "info")
            .env("BSL_PLATFORM_HELP_CACHE_DIR", dir.join("help-cache"))
            .env("BSL_PLATFORM_PATH", dir.join("no-platform"))
            .env(bsl_platform::CORPUS_ENV, value),
        "analyze",
        ANALYZE_GUARD,
    );
    (output.status.success(), String::from_utf8_lossy(&output.stderr).into_owned())
}

#[test]
fn corpus_variable_path_is_reported_as_the_chosen_source() {
    let dir = project("");
    let corpus = dir.path().join("a.json");
    let (ok, log) = analyze_with_corpus_variable(dir.path(), corpus.to_str().unwrap());
    assert!(ok, "{log}");
    assert!(log.contains("chosen by the environment variable"), "{log}");
    assert!(log.contains("platform help loaded") && log.contains("a.json"), "{log}");
}

#[test]
fn corpus_variable_url_is_reported_without_userinfo_or_query() {
    let dir = project("");
    // A closed local port: the load fails at once, which is all this checks.
    let (ok, log) = analyze_with_corpus_variable(
        dir.path(),
        "https://user:secret@127.0.0.1:1/help/manifest.json?token=abc#frag",
    );
    assert!(ok, "{log}");
    assert!(log.contains("chosen by the environment variable"), "{log}");
    assert!(log.contains("external (https://127.0.0.1:1/help/manifest.json)"), "{log}");
    assert!(!log.contains("secret") && !log.contains("token=abc"), "{log}");
    assert!(log.contains("platform help unavailable"), "{log}");
}

#[test]
fn corpus_variable_with_http_selects_the_external_url_source() {
    let dir = project("");
    let (ok, log) = analyze_with_corpus_variable(
        dir.path(),
        "http://user:secret@127.0.0.1:1/help/manifest.json?token=abc",
    );
    assert!(ok, "{log}");
    assert!(log.contains("chosen by the environment variable"), "{log}");
    assert!(log.contains("external (http://127.0.0.1:1/help/manifest.json)"), "{log}");
    assert!(!log.contains("secret") && !log.contains("token=abc"), "{log}");
    assert!(!log.contains("is not accepted"), "{log}");
}

#[test]
fn corpus_variable_is_ignored_with_a_warning_when_the_configuration_has_a_section() {
    let dir = project(&external("a.json"));
    let other = dir.path().join("b.json");
    let (ok, log) = analyze_with_corpus_variable(dir.path(), other.to_str().unwrap());
    assert!(ok, "{log}");
    assert!(
        log.contains("BSL_PLATFORM_HELP_CORPUS is ignored") && log.contains("[platform_help]"),
        "{log}"
    );
    assert!(log.contains("platform help loaded") && log.contains("a.json"), "{log}");
    assert!(!log.contains("chosen by the environment variable"), "{log}");
}

/// A process that has exited and been reaped, or exited and not yet been reaped.
#[cfg(target_os = "linux")]
fn process_gone(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return true };
    stat.rfind(')').and_then(|end| stat[end + 1..].split_whitespace().next()) == Some("Z")
}

/// Whether `pid` is gone within [`HANG_GUARD`]. A killed process leaves once the kernel has
/// torn it down, and no fixed pause after the kill can promise that it has.
#[cfg(target_os = "linux")]
fn leaves(pid: u32) -> bool {
    let deadline = Instant::now() + HANG_GUARD;
    while !process_gone(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_STEP);
    }
    true
}

/// When the process started, in clock ticks after boot: with the pid, it names one process.
#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name: state is the first, starttime the twentieth.
    stat[stat.rfind(')')? + 2..].split_whitespace().nth(19)?.parse().ok()
}

/// The text of a caught panic.
#[cfg(target_os = "linux")]
fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => {
            payload.downcast_ref::<&str>().map_or_else(String::new, |text| text.to_string())
        }
    }
}

/// Runs `wait`, which must fail rather than hang, and returns its panic text.
#[cfg(target_os = "linux")]
fn hang_failure(wait: impl FnOnce()) -> String {
    let started = Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(wait));
    let elapsed = started.elapsed();
    let Err(payload) = outcome else { panic!("the wait returned instead of failing") };
    assert!(elapsed < HANG_GUARD, "the wait took the default guard, not its own: {elapsed:?}");
    panic_text(payload)
}

/// The pid that follows `marker` in `text`.
#[cfg(target_os = "linux")]
fn pid_after(text: &str, marker: &str) -> u32 {
    let start =
        text.find(marker).unwrap_or_else(|| panic!("no `{marker}` in: {text}")) + marker.len();
    text[start..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("no pid after `{marker}` in: {text}"))
}

/// A stdio server whose messages never reach the test: alive, but silent.
#[cfg(target_os = "linux")]
fn silent_server(dir: &Path) -> (Lsp, Vec<Box<dyn std::any::Any>>) {
    let mut lsp = Lsp::start_stdio(dir);
    let (sender, muted) = mpsc::channel::<Value>();
    let original = std::mem::replace(&mut lsp.messages, muted);
    (lsp, vec![Box::new(sender), Box::new(original)])
}

#[cfg(target_os = "linux")]
#[test]
fn a_silent_child_fails_the_initialize_wait_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new("sleep");
    command.arg("600");
    let failure = hang_failure(|| {
        Lsp::start_command(command, dir.path(), json!({}), Duration::from_secs(1));
    });
    assert!(failure.contains("phase initialize"), "{failure}");
    let pid = pid_after(&failure, "LSP child ");
    assert!(leaves(pid), "child {pid} outlived the failed wait");
}

#[cfg(target_os = "linux")]
#[test]
fn a_silent_server_fails_the_hover_wait_instead_of_hanging() {
    let dir = project(&external("a.json"));
    let file = dir.path().join("Module.bsl");
    let (mut lsp, _channel) = silent_server(dir.path());
    let pid = lsp.child.id();
    let second = Duration::from_secs(1);
    let failure = hang_failure(|| {
        hover_add_judging(&mut lsp, &file, 100, second, second, second);
    });
    assert!(failure.contains("total deadline elapsed"), "{failure}");
    assert!(failure.contains("recent messages"), "{failure}");
    assert!(failure.contains("platform_help_process.rs"), "{failure}");
    drop(lsp);
    assert!(leaves(pid), "LSP child {pid} outlived the test");
}

#[cfg(target_os = "linux")]
#[test]
fn a_server_that_never_warns_fails_the_restart_wait_instead_of_hanging() {
    let dir = project(&external("a.json"));
    let (mut lsp, _channel) = silent_server(dir.path());
    let pid = lsp.child.id();
    let warning = common::Provocation::start_within(|| {}, Duration::from_secs(1));
    let failure = hang_failure(|| {
        wait_for_restart_warning(&mut lsp, &warning);
    });
    assert!(failure.contains("phase restart warning"), "{failure}");
    assert!(failure.contains("recent messages"), "{failure}");
    drop(lsp);
    assert!(leaves(pid), "LSP child {pid} outlived the test");
}

#[cfg(target_os = "linux")]
#[test]
fn a_child_that_never_exits_fails_the_process_wait_instead_of_hanging() {
    let failure = hang_failure(|| {
        output_within(Command::new("sleep").arg("600"), "analyze", Duration::from_secs(1));
    });
    assert!(failure.contains("phase analyze"), "{failure}");
    let pid = pid_after(&failure, "child ");
    assert!(leaves(pid), "child {pid} outlived the failed wait");
}

/// Kills the process whose pid a fixture wrote to `0`, however the test ends.
#[cfg(target_os = "linux")]
struct KillOnDrop(PathBuf);

#[cfg(target_os = "linux")]
impl KillOnDrop {
    fn pid(&self) -> Option<u32> {
        std::fs::read_to_string(&self.0).ok()?.trim().parse().ok()
    }
}

#[cfg(target_os = "linux")]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let Some(pid) = self.pid() else { return };
        let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
        leaves(pid);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_child_that_leaves_its_output_open_fails_the_process_wait_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let grandchild = KillOnDrop(dir.path().join("grandchild.pid"));
    // The shell exits whenever it gets to; only the output, held by the grandchild, is on a bound.
    let failure = hang_failure(|| {
        output_closing_within(
            Command::new("sh")
                .args(["-c", "sleep 600 & echo $! > \"$1\"; exit 0", "sh"])
                .arg(&grandchild.0),
            "analyze",
            HANG_GUARD,
            Some(Duration::from_secs(1)),
        );
    });
    assert!(failure.contains("exited but its output stayed open"), "{failure}");
    let pid = grandchild.pid().expect("the fixture wrote its grandchild's pid");
    drop(grandchild);
    assert!(leaves(pid), "grandchild {pid} outlived the test");
}

#[cfg(target_os = "linux")]
#[test]
fn a_failed_auto_mode_test_leaves_no_process_behind() {
    // Neither the project nor its config changes while the fixture runs, so nothing the daemon
    // reacts to ends it: only the fixture's own teardown can.
    let dir = project(&external("a.json"));
    let file = dir.path().join("Module.bsl");
    let seen = std::sync::Arc::new(std::sync::Mutex::new((None, Vec::new())));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let backends = AutoBackends::new(dir.path());
        let mut lsp =
            Lsp::start_command(backends.command(), dir.path(), json!({}), common::SILENCE);
        seen.lock().unwrap().0 = Some(lsp.child.id());
        lsp.open(&file, MODULE);
        hover_add(&mut lsp, &file, 100);
        let daemons = backends.live_daemons();
        seen.lock().unwrap().1.clone_from(&daemons);
        assert!(!daemons.is_empty(), "auto mode serves through a shared backend");
        panic!("fixture failure");
    }));
    let failure = panic_text(outcome.expect_err("the fixture fails"));
    assert_eq!(failure, "fixture failure");
    let (frontend, daemons) = seen.lock().unwrap().clone();
    for pid in frontend.into_iter().chain(daemons) {
        assert!(leaves(pid), "process {pid} outlived the failed test");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_server_that_keeps_answering_null_fails_the_hover_wait_instead_of_repeating() {
    let dir = project(&external("a.json"));
    let file = dir.path().join("Module.bsl");
    let mut lsp = Lsp::start_stdio(dir.path());
    let pid = lsp.child.id();
    // The hover position is on the document's empty last line: nothing is under it.
    lsp.open(&file, "Процедура Тест()\nКонецПроцедуры\n");
    // A window of zero is over before the first answer, so the first `null` ends the repeating.
    let failure = hang_failure(|| {
        hover_add_judging(
            &mut lsp,
            &file,
            100,
            common::SILENCE,
            common::BUSY_CEILING,
            Duration::ZERO,
        );
    });
    assert!(failure.contains("phase hover answered null"), "{failure}");
    assert!(failure.contains("recent messages"), "{failure}");
    drop(lsp);
    assert!(leaves(pid), "LSP child {pid} outlived the test");
}

#[cfg(target_os = "linux")]
#[test]
fn a_silent_mcp_child_fails_the_response_wait_instead_of_hanging() {
    let mut command = Command::new("sleep");
    command.arg("600");
    // A guard of zero is over before the first answer could arrive, so the wait ends on the
    // bound itself and the verdict does not wait on the clock.
    let failure = hang_failure(|| {
        mcp_add_docs_via(command, Duration::ZERO);
    });
    assert!(failure.contains("MCP child"), "{failure}");
    assert!(failure.contains("response channel ended"), "{failure}");
    let pid = pid_after(&failure, "MCP child ");
    assert!(leaves(pid), "MCP child {pid} outlived the failed wait");
}

#[cfg(target_os = "linux")]
#[test]
fn output_within_returns_all_a_child_wrote_and_its_status() {
    // Each stream carries more than a pipe holds: a reader that waited for the exit first
    // would leave the child blocked on its write. The lengths differ, so crossed streams show.
    let output = output_within(
        Command::new("sh")
            .args(["-c", "head -c 300000 /dev/zero; head -c 200000 /dev/zero >&2; exit 3"]),
        "analyze",
        ANALYZE_GUARD,
    );
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stdout.len(), 300_000);
    assert_eq!(output.stderr.len(), 200_000);
}
