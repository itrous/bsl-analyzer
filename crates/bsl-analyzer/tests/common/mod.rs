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

/// Silence a server at rest is allowed before a wait is judged lost.
pub const SILENCE: Duration = Duration::from_secs(60);

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
    deadline: std::time::Instant,
}

impl<F: Fn()> Provocation<F> {
    pub fn start(act: F) -> Self {
        act();
        Self { act, deadline: std::time::Instant::now() + Duration::from_secs(60) }
    }

    pub fn again(&self) {
        assert!(
            std::time::Instant::now() < self.deadline,
            "the server never reacted to the provocation",
        );
        (self.act)();
    }
}

pub struct Lsp {
    pub child: Child,
    pub stdin: ChildStdin,
    pub messages: Receiver<Value>,
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .arg("lsp")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, messages) = mpsc::channel();
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
                    }
                }
                let Some(length) = length else { return };
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                if let Ok(message) = serde_json::from_slice(&body) {
                    if tx.send(message).is_err() {
                        return;
                    }
                }
            }
        });
        let mut lsp = Self { child, stdin, messages, recent: Default::default() };
        let root_uri = lsp_types::Url::from_directory_path(root).unwrap();
        lsp.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"rootUri": root_uri, "capabilities": capabilities}
        }));
        lsp.wait_for(|message| message["id"] == 1);
        lsp.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
        lsp
    }

    pub fn send(&mut self, message: Value) {
        let body = message.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
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
    #[track_caller]
    pub fn wait_for(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        self.wait_for_judging(SILENCE, predicate)
    }

    /// [`Self::wait_for`] with the silence window spelled out — for the control that
    /// shows the judgement can fail, without sitting through the real window.
    #[track_caller]
    pub fn wait_for_judging(&self, silence: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
        /// How long a server may stay busy and silent before the wait gives up anyway:
        /// a bound on a runaway, not a measure of anything the tests expect.
        const BUSY_CEILING: Duration = Duration::from_secs(10 * 60);

        let caller = std::panic::Location::caller();
        let started = std::time::Instant::now();
        loop {
            let cpu_before = self.server_cpu_ticks();
            if let Some(message) = self.wait_for_within(silence, |_| true) {
                if predicate(&message) {
                    return message;
                }
                continue;
            }
            let waited = started.elapsed();
            let cpu_after = self.server_cpu_ticks();
            let worked = match (cpu_before, cpu_after) {
                (Some(before), Some(after)) => after > before,
                // Without a reading the server gets the benefit of the doubt, once.
                _ => waited < silence * 2,
            };
            assert!(
                worked,
                "waiting at {caller} for {waited:?}: the server answered nothing for \
                 {silence:?} and used no CPU in that time, so it is not working towards \
                 an answer — the awaited condition is one it will not meet. Last messages \
                 seen: {}",
                self.recent_methods(),
            );
            assert!(
                waited < BUSY_CEILING,
                "waiting at {caller} for {waited:?}: the server stayed busy but answered \
                 nothing for {silence:?} at a stretch, past the {BUSY_CEILING:?} ceiling. \
                 Last messages seen: {}",
                self.recent_methods(),
            );
        }
    }

    /// CPU time the server process has used so far, in scheduler ticks: whether it is
    /// working, not how hard. Linux only; elsewhere there is no reading.
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
    pub fn wait_for_within(
        &self,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
            match self.messages.recv_timeout(remaining) {
                Ok(message) => {
                    self.note_seen(&message);
                    if predicate(&message) {
                        return Some(message);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the server exited instead of answering")
                }
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
}

impl Drop for Lsp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
