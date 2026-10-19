//! Shared LSP harness for the diagnostics-baseline tests.
//!
//! Lives under `tests/common/` so it is a module, not a test target: included from a
//! sibling test file, every `#[test]` in it would be compiled and RUN once per
//! including target.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{json, Value};

pub const BROKEN: &str = "Процедура Тест(\n";

/// How much of an LSP child's stderr [`Lsp::stderr_text`] keeps: the most recent bytes.
pub const STDERR_TAIL: usize = 64 * 1024;

/// Silence a server at rest is allowed before a wait is judged lost: a minute under
/// `timing_contract`. An ordinary run does not judge a slow server by the clock — a server
/// descheduled on a loaded machine is as quiet and as idle as a lost one — so there the window
/// is the hang guard, far past any healthy answer (see [`test_utils::wait::hang_guard`]).
pub const SILENCE: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));

/// Total bound for one wait, even while unrelated messages keep arriving: ten minutes in
/// either profile.
pub const BUSY_CEILING: Duration = test_utils::wait::hang_guard(Duration::from_secs(10 * 60));

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PROCESS_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const PROCESS_SHUTDOWN_TIMEOUT: Duration = test_utils::wait::hang_guard(Duration::from_secs(10));

/// How long [`Provocation::start`] goes on repeating its act before the wait is judged lost: a
/// minute under `timing_contract`, the hang guard otherwise.
pub const PROVOCATION_BOUND: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));

const _: () = assert!(SILENCE.as_nanos() <= test_utils::wait::HANG_GUARD.as_nanos());
const _: () = assert!(BUSY_CEILING.as_nanos() <= test_utils::wait::HANG_GUARD.as_nanos());
const _: () =
    assert!(PROCESS_SHUTDOWN_TIMEOUT.as_nanos() <= test_utils::wait::HANG_GUARD.as_nanos());
const _: () = assert!(PROVOCATION_BOUND.as_nanos() <= test_utils::wait::HANG_GUARD.as_nanos());

/// Match a closed LSP definition by the file it names, not by its serialized URI spelling.
/// The result may be a `Location`, an array of `Location`s, or `LocationLink`s.
pub fn definition_targets_file(result: &Value, expected_uri: &lsp_types::Url) -> bool {
    let Ok(expected_path) = expected_uri.to_file_path() else { return false };
    let Ok(expected_path) = expected_path.canonicalize() else { return false };
    let locations = result.as_array().map_or_else(|| std::slice::from_ref(result), Vec::as_slice);

    locations.iter().any(|location| {
        let Some(uri) =
            location.get("uri").or_else(|| location.get("targetUri")).and_then(Value::as_str)
        else {
            return false;
        };
        let Ok(uri) = lsp_types::Url::parse(uri) else { return false };
        let Ok(path) = uri.to_file_path() else { return false };
        path.canonicalize().is_ok_and(|actual| actual == expected_path)
    })
}

pub fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/Main.bsl"), BROKEN).unwrap();
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        "[source]\nroot = \"src\"\n\n[diagnostics.baseline]\npath = \"baseline.json\"\n",
    )
    .unwrap();
    let created = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .current_dir(dir.path())
        .args(["diagnostics", "baseline", "create", "-s", "."])
        .output()
        .unwrap();
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    dir
}

/// An action repeated until the server reacts to it.
///
/// The server's file watcher is armed asynchronously and nothing announces when; a change
/// written before that raises no event at all, so a single act and a long wait would wait
/// out a notification that is never coming. Bounded, so a server that reacts to nothing
/// fails the test with a message rather than spinning.
pub struct Provocation<F> {
    act: F,
    bound: Duration,
    deadline: std::time::Instant,
}

impl<F: Fn()> Provocation<F> {
    pub fn start(act: F) -> Self {
        Self::start_within(act, PROVOCATION_BOUND)
    }

    /// [`Self::start`] that gives up after `bound` instead of [`PROVOCATION_BOUND`].
    pub fn start_within(act: F, bound: Duration) -> Self {
        act();
        Self { act, bound, deadline: std::time::Instant::now() + bound }
    }

    /// Acts again, or fails the wait for `phase` once the bound has run out.
    #[track_caller]
    pub fn again(&self, lsp: &Lsp, phase: &str) {
        if std::time::Instant::now() >= self.deadline {
            lsp.hung(phase, self.bound);
        }
        (self.act)();
    }
}

pub struct Lsp {
    pub child: Child,
    pub stdin: ChildStdin,
    pub messages: Receiver<Value>,
    pub initialize_response: Value,
    stderr_output: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    protocol_errors: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Server notifications sent during initialize are valid LSP traffic too; keep
    /// them for the test that owns the channel instead of eating them while waiting
    /// for the initialize response.
    pending: std::sync::Mutex<std::collections::VecDeque<Value>>,
    /// The methods of the last few messages a wait took in, for the failure message of a
    /// wait that ran out: what the server was saying is the context of what it did not.
    recent: std::sync::Mutex<std::collections::VecDeque<String>>,
}

impl Lsp {
    pub fn start(root: &Path) -> Self {
        Self::start_with_capabilities(root, json!({}))
    }

    /// [`Self::start`] with the client capabilities the test wants negotiated — the
    /// same document a real editor sends in `initialize`. An empty object is the
    /// capability-less client `start` has always used.
    pub fn start_with_capabilities(root: &Path, capabilities: Value) -> Self {
        Self::start_with_mode_and_capabilities(root, None, capabilities)
    }

    pub fn start_with_capabilities_and_log(
        root: &Path,
        capabilities: Value,
        log: Option<&Path>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
        command.arg("lsp");
        if let Some(log) = log {
            command.env("BSL_LOG_FILE", log).env("BSL_LOG", "warn");
        }
        Self::start_command(command, root, capabilities, SILENCE)
    }

    pub fn start_stdio(root: &Path) -> Self {
        Self::start_with_mode_and_capabilities(root, Some("stdio"), json!({}))
    }

    fn start_with_mode_and_capabilities(
        root: &Path,
        mode: Option<&str>,
        capabilities: Value,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
        command.arg("lsp");
        if let Some(mode) = mode {
            command.args(["--mode", mode]);
        }
        Self::start_command(command, root, capabilities, SILENCE)
    }

    /// Starts `command` as the LSP child in `root` and waits up to `initialize_within` for its
    /// answer to `initialize`.
    pub fn start_command(
        mut command: Command,
        root: &Path,
        capabilities: Value,
        initialize_within: Duration,
    ) -> Self {
        let mut child = command
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stderr_output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stderr_capture = std::sync::Arc::clone(&stderr_output);
        let stderr_reader = std::thread::spawn(move || {
            let mut reader = stderr;
            let mut buffer = [0; 2048];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let mut output = stderr_capture.lock().unwrap();
                output.extend_from_slice(&buffer[..read]);
                if output.len() > STDERR_TAIL {
                    let excess = output.len() - STDERR_TAIL;
                    output.drain(..excess);
                }
            }
        });
        let (tx, messages) = mpsc::channel();
        let protocol_errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let errors = protocol_errors.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut length = None;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).ok().filter(|&n| n > 0).is_none() {
                        return;
                    }
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(value) = header.strip_prefix("Content-Length:") {
                        length = value.trim().parse::<usize>().ok();
                    } else if !header.starts_with("Content-Type:") {
                        errors.lock().unwrap().push(header);
                    }
                }
                let Some(length) = length else { return };
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                match serde_json::from_slice(&body) {
                    Ok(message) => {
                        if tx.send(message).is_err() {
                            return;
                        }
                    }
                    Err(error) => errors.lock().unwrap().push(error.to_string()),
                }
            }
        });
        let mut lsp = Self {
            child,
            stdin,
            messages,
            initialize_response: Value::Null,
            stderr_output,
            stderr_reader: Some(stderr_reader),
            protocol_errors,
            pending: Default::default(),
            recent: Default::default(),
        };
        let root_uri = lsp_types::Url::from_directory_path(root).unwrap();
        lsp.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"rootUri": root_uri, "capabilities": capabilities}
        }));
        let deadline = std::time::Instant::now() + initialize_within;
        loop {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                panic!(
                    "LSP child {} phase initialize exceeded {initialize_within:?}; recent messages: {}",
                    lsp.child.id(),
                    lsp.recent_methods()
                );
            };
            let message = match lsp.messages.recv_timeout(remaining) {
                Ok(message) if std::time::Instant::now() < deadline => message,
                Ok(_) => panic!(
                    "LSP child {} phase initialize exceeded {initialize_within:?}; recent messages: {}",
                    lsp.child.id(),
                    lsp.recent_methods()
                ),
                Err(error) => panic!(
                    "LSP child {} phase initialize failed after {initialize_within:?}: {error}; recent messages: {}",
                    lsp.child.id(),
                    lsp.recent_methods()
                ),
            };
            lsp.note_seen(&message);
            if message["id"] == 1 && message["method"].is_null() {
                lsp.initialize_response = message.clone();
                break;
            }
            lsp.pending.lock().unwrap().push_back(message);
        }
        lsp.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
        lsp
    }

    pub fn assert_protocol_clean(&self) {
        let errors = self.protocol_errors.lock().unwrap();
        assert!(errors.is_empty(), "unexpected LSP stdout: {errors:?}");
    }

    pub fn send(&mut self, message: Value) {
        let body = message.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    /// The child's stderr so far, or only its last [`STDERR_TAIL`] bytes once it has written
    /// more.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr_output.lock().unwrap()).into_owned()
    }

    /// The child's complete stderr, once every process holding the pipe has closed it.
    ///
    /// Waits for that close without a bound, so call it only for a server that starts no
    /// process of its own (`--mode stdio`): a process the server started can inherit the
    /// pipe and keep it open after the server has exited.
    pub fn finish_stderr(&mut self) -> String {
        if let Some(reader) = self.stderr_reader.take() {
            reader.join().expect("LSP stderr reader thread");
        }
        self.stderr_text()
    }

    /// Fails a wait for `phase` that `waited` without seeing what it waited for, with what the
    /// server last said and the tail of its stderr.
    #[track_caller]
    pub fn hung(&self, phase: &str, waited: Duration) -> ! {
        const TAIL: usize = 4 * 1024;
        let caller = std::panic::Location::caller();
        let stderr_tail = {
            let output = self.stderr_output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            String::from_utf8_lossy(&output[output.len().saturating_sub(TAIL)..]).into_owned()
        };
        panic!(
            "LSP child {} phase {phase}: nothing matching arrived within {waited:?} at {caller}; recent messages: {}; stderr tail: {stderr_tail}",
            self.child.id(),
            self.recent_methods(),
        )
    }

    /// Wait for a message, telling a server that is busy from one that is done.
    ///
    /// Silence is judged per message, not per wait: a server that is working says so —
    /// progress, logs — and a stand behind a cold build on a loaded machine can spend
    /// longer than [`SILENCE`] reaching what it is waiting for without ever going quiet.
    /// Silence alone decides nothing, though: a server deep in an analysis and a server
    /// idle because the awaited condition can never hold are equally quiet. What tells
    /// them apart is whether the server did any work in the meantime, so a silent window
    /// in which it used CPU is extended, up to [`BUSY_CEILING`], and a silent window in
    /// which it used none fails at once — with where the wait stood and what it last saw,
    /// so the failure names the condition instead of "Timeout".
    ///
    /// Both verdicts read the clock. Under `timing_contract` they are the timing contract; an
    /// ordinary run stretches both windows to the hang guard, so a slow server is waited for
    /// while a wait that can never be met still fails, saying where it stood, instead of
    /// hanging the runner.
    #[track_caller]
    pub fn wait_for(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        self.wait_for_judging(SILENCE, BUSY_CEILING, predicate)
    }

    /// [`Self::wait_for`] with the silence window spelled out — for the control that
    /// shows the judgement can fail, without sitting through the real window.
    #[track_caller]
    pub fn wait_for_judging(
        &self,
        silence: Duration,
        busy_ceiling: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        let caller = std::panic::Location::caller();
        let started = std::time::Instant::now();
        let deadline = started + busy_ceiling;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let cpu_before = self.server_cpu_ticks();
            let mut message = self.wait_for_within(silence.min(remaining), |_| true);
            let waited = started.elapsed();
            // Judged before the ceiling: a message delivered in time may still be taken in
            // past it when the test thread wakes late.
            if let Some(message) = message.take_if(|message| predicate(message)) {
                return message;
            }
            assert!(
                waited < busy_ceiling,
                "waiting at {caller} for {waited:?}: total deadline elapsed for LSP child {}; recent messages: {}",
                self.child.id(),
                self.recent_methods(),
            );
            if message.is_some() {
                continue;
            }
            let cpu_after = self.server_cpu_ticks();
            let worked = match (cpu_before, cpu_after) {
                (Some(before), Some(after)) => after > before,
                // Without a reading the server gets the benefit of the doubt, once.
                _ => waited < silence * 2,
            };
            assert!(
                worked,
                "waiting at {caller} for {waited:?}: LSP child answered nothing for \
                 {silence:?} and used no CPU in that time; auto mode can run analysis in \
                 a separate backend, whose activity is not measured here. Last messages \
                 seen from LSP child {}: {}",
                self.child.id(),
                self.recent_methods(),
            );
        }
    }

    /// CPU time of the child (a proxy in auto mode), in scheduler ticks.
    /// This does not measure the shared backend. Linux only.
    fn server_cpu_ticks(&self) -> Option<u64> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.child.id())).ok()?;
        // Fields after the parenthesised command name: state is the first, utime and
        // stime are the twelfth and thirteenth.
        let after_name = &stat[stat.rfind(')')? + 2..];
        let mut fields = after_name.split_whitespace();
        let utime: u64 = fields.nth(11)?.parse().ok()?;
        let stime: u64 = fields.next()?.parse().ok()?;
        Some(utime + stime)
    }

    fn note_seen(&self, message: &Value) {
        const KEPT: usize = 8;
        let label = match (message["method"].as_str(), message["id"].as_u64()) {
            (Some(method), _) => method.to_owned(),
            (None, Some(id)) => format!("response #{id}"),
            (None, None) => "message".to_owned(),
        };
        let mut recent = self.recent.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if recent.len() == KEPT {
            recent.pop_front();
        }
        recent.push_back(label);
    }

    fn recent_methods(&self) -> String {
        let recent = self.recent.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if recent.is_empty() {
            "none".to_owned()
        } else {
            recent.iter().cloned().collect::<Vec<_>>().join(", ")
        }
    }

    /// Wait for a message, reporting silence instead of failing the test.
    ///
    /// For a wait whose subject arrives only once the server's file watcher is live:
    /// the watcher is armed asynchronously, after the loader has already announced the
    /// load finished, and nothing tells a client when. A change written into that window
    /// raises no event at all, so a caller that provokes one has to be able to provoke it
    /// again rather than wait out a notification that will never come.
    ///
    /// `None` means silence and nothing else. A server that has exited is not silence —
    /// it is the answer — so it fails here rather than leaving the caller to provoke a
    /// process that is gone, as fast as the loop can go round.
    #[track_caller]
    pub fn wait_for_within(
        &self,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        let caller = std::panic::Location::caller();
        let pid = self.child.id();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let pending = self.pending.lock().unwrap().pop_front();
            // A zero remainder still polls once: what has already arrived is not silence.
            let message = match pending.map(Ok).unwrap_or_else(|| {
                self.messages
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            }) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!(
                        "LSP child {pid} phase message wait at {caller}: output closed before matching a message; recent messages: {}; stderr: {}",
                        self.recent_methods(),
                        self.stderr_text()
                    )
                }
            };
            self.note_seen(&message);
            if predicate(&message) {
                return Some(message);
            }
            // Checked after the message, not before: unrelated traffic cannot stretch the
            // wait, and nothing taken in is dropped unjudged.
            if std::time::Instant::now() >= deadline {
                return None;
            }
        }
    }

    /// A notification the server has no work for, sent only to reach its main loop.
    ///
    /// What an editor produces constantly and what a stand needs when its subject is
    /// something the server must notice on its own rather than be told about.
    pub fn poke(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0", "method": "$/setTrace", "params": {"value": "off"}
        }));
    }

    pub fn wait_for_exit_within(
        &mut self,
        timeout: Duration,
        phase: &str,
    ) -> Result<Option<std::process::ExitStatus>, String> {
        let pid = self.child.id();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(Some(status)),
                Ok(None) => {}
                Err(error) => {
                    return Err(format!(
                        "LSP child {pid} phase {phase}: process poll failed: {error}; recent messages: {}",
                        self.recent_methods()
                    ));
                }
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(PROCESS_POLL_INTERVAL.min(remaining));
        }
    }

    fn stop_child(
        &mut self,
        phase: &str,
        timeout: Duration,
    ) -> Result<std::process::ExitStatus, String> {
        let pid = self.child.id();
        let poll_error = match self.wait_for_exit_within(Duration::ZERO, phase) {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => None,
            Err(error) => Some(error),
        };
        if let Err(error) = self.child.kill() {
            match self.wait_for_exit_within(Duration::ZERO, phase) {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(poll_error) => {
                    return Err(format!(
                        "LSP child {pid} phase {phase}: kill failed: {error}; {poll_error}"
                    ));
                }
            }
            return Err(format!(
                "LSP child {pid} phase {phase}: kill failed: {error}; prior process poll: {}; recent messages: {}",
                poll_error.as_deref().unwrap_or("no process poll error"),
                self.recent_methods()
            ));
        }
        self.wait_for_exit_within(timeout, phase)?.ok_or_else(|| {
            format!(
                "LSP child {pid} phase {phase}: did not exit within {timeout:?} after kill; recent messages: {}",
                self.recent_methods()
            )
        })
    }

    pub fn open(&mut self, path: &Path, text: &str) -> Value {
        let uri = lsp_types::Url::from_file_path(path).unwrap();
        self.send(json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "bsl", "version": 1, "text": text}}
        }));
        self.wait_for(|message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == uri.as_str()
        })
    }

    pub fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.wait_for(|message| {
            message["id"] == id
                && message["method"].is_null()
                && (message.get("result").is_some() || message.get("error").is_some())
        })
    }

    pub fn shutdown(&mut self, id: u64) -> std::process::ExitStatus {
        let response = self.request(id, "shutdown", Value::Null);
        assert!(response["error"].is_null(), "shutdown request failed: {response}");
        self.send(json!({"jsonrpc": "2.0", "method": "exit"}));
        let status = match self.wait_for_exit_within(PROCESS_SHUTDOWN_TIMEOUT, "shutdown/exit") {
            Ok(Some(status)) => status,
            Ok(None) => {
                let pid = self.child.id();
                let recent = self.recent_methods();
                let cleanup = self.stop_child("shutdown timeout cleanup", PROCESS_CLEANUP_TIMEOUT);
                panic!(
                    "LSP child {pid} phase shutdown/exit exceeded {PROCESS_SHUTDOWN_TIMEOUT:?}; recent messages: {recent}; forced cleanup: {cleanup:?}"
                );
            }
            Err(error) => {
                let cleanup = self.stop_child("shutdown error cleanup", PROCESS_CLEANUP_TIMEOUT);
                panic!("{error}; forced cleanup: {cleanup:?}");
            }
        };
        status
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        // The stderr reader stays detached: a process the child started, such as the shared
        // daemon, can hold a copy of the pipe long after the child itself has exited.
        if let Err(error) = self.stop_child("Drop", PROCESS_CLEANUP_TIMEOUT) {
            eprintln!("{error}");
        }
    }
}
