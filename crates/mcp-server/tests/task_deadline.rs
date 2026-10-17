//! What stops a task that nobody stops, and what it says about why.
//!
//! Kept in a binary of its own because it turns the handle lifetime down to something a test
//! can wait for, and that knob is read per task: a shorter lifetime leaking into the gates
//! that expect a task to run to its answer would cut them short instead.
//!
//! Catching a deadline while the index is still building means winning a race against the
//! build, and that race is not winnable by choosing constants — it is stated as a unit gate
//! instead, where the lifecycle is set rather than raced. The reading phase lives here, under
//! `timing_contract`: a handle opens only during the build, so its deadline falls after the
//! build only on a machine fast enough to finish the build first.

use std::path::Path;
use std::time::Duration;

use mcp_server::{serve_stream, McpProfile, McpServer, SharedState, ToolGate};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// Shared gates helpers; each binary uses the subset it needs, so the rest is allowed to be unused.
#[allow(dead_code)]
mod common;
use common::Retired;

const TOOL: &str = "references";

/// Bound on a single answer. A hang guard: the answer comes on its own when the server is right.
const HANG_GUARD: Duration = Duration::from_secs(180);

/// Polls at this interval, for at most `POLL_ATTEMPTS` polls. The bound counts polls so that a
/// slow machine waits longer for the same answer instead of failing on it.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const POLL_ATTEMPTS: u32 = 9_000;

/// A stand that becomes ready quickly and then works for a long time: the build costs 0.7 s
/// and the whole-config sweep 13 s, so a 2.4 s deadline falls squarely inside the work with
/// room on both sides.
const LONG_WORK_MODULES: usize = 800;
const LONG_WORK_BODIES: usize = 100;
const LONG_WORK_TTL_MS: u64 = 3_000;

fn stage(modules: usize, bodies: usize) -> TempDir {
    let src = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"));
    let dst = TempDir::new().expect("scratch workspace");
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.expect("walk fixture");
        let rel = entry.path().strip_prefix(src).expect("path under fixture root");
        let target = dst.path().join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
    for index in 0..modules {
        let dir = dst.path().join("CommonModules").join(format!("Толпа{index:04}")).join("Ext");
        std::fs::create_dir_all(&dir).expect("mkdir module");
        let mut body = String::new();
        for nth in 0..bodies {
            body.push_str(&format!(
                "Процедура Вызвать{index:04}_{nth}() Экспорт\n    \
                 ПервыйОбщийМодуль.НеУстаревшаяФункция();\n    \
                 Знач = 1 + {nth};\nКонецПроцедуры\n"
            ));
        }
        std::fs::write(dir.join("Module.bsl"), body).expect("write module");
    }
    dst
}

struct Wire {
    write: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    read: Box<dyn tokio::io::AsyncBufRead + Send + Unpin>,
    next_id: i64,
    /// Declared last, so it is dropped with the session: the server's owners leave before the
    /// stand the test declared ahead of this session is removed.
    _server: Retired,
}

impl Wire {
    async fn connect(root: &Path) -> Self {
        let state = SharedState::workspace(root.to_path_buf()).expect("valid workspace project");
        let gate = ToolGate::for_launch(McpProfile::Workspace, &[TOOL.to_owned()]);
        let server = Retired(McpServer::with_gate(McpProfile::Workspace, state, &gate));
        let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
        tokio::spawn(serve_stream((*server).clone(), server_io));
        let (read_half, write_half) = tokio::io::split(client_io);
        let mut wire = Self {
            write: Box::new(write_half),
            read: Box::new(BufReader::new(read_half)),
            next_id: 1,
            _server: server,
        };
        let hello = wire
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"extensions": {"io.modelcontextprotocol/tasks": {}}},
                    "clientInfo": {"name": "deadline-gate", "version": "0"}
                }),
            )
            .await;
        assert!(hello["result"]["serverInfo"].is_object(), "handshake answered: {hello}");
        wire.notify("notifications/initialized", json!({})).await;
        wire
    }

    async fn send(&mut self, value: Value) {
        self.write.write_all(format!("{value}\n").as_bytes()).await.expect("frame written");
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params})).await;
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let mut line = String::new();
            tokio::time::timeout(HANG_GUARD, self.read.read_line(&mut line))
                .await
                .expect("the server must answer within the deadline")
                .expect("frame read");
            let frame: Value = serde_json::from_str(&line).expect("frame is JSON");
            if frame["id"] == json!(id) {
                return frame;
            }
        }
    }

    async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments})).await
    }

    /// Poll a handle to its terminal state, refusing to report a swept entry as an outcome.
    async fn settle(&mut self, task_id: &str) -> Value {
        for _ in 0..POLL_ATTEMPTS {
            let seen = self.request("tasks/get", json!({"taskId": task_id})).await;
            assert!(
                seen["error"].is_null(),
                "the handle was swept before its outcome could be read; if this is a timing \
                 fault rather than a defect, raise the lifetime for this gate: {seen}"
            );
            if seen["result"]["status"] != "working" {
                return seen["result"].clone();
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        panic!("the task never reached a terminal status: {task_id}");
    }
}

fn task_id(answer: &Value) -> String {
    assert_eq!(answer["result"]["resultType"], "task", "a handle was opened: {answer}");
    answer["result"]["taskId"].as_str().expect("a handle carries an id").to_owned()
}

fn arm(ttl_ms: u64) {
    std::env::set_var("BSL_MCP_TASKS", "1");
    std::env::set_var("BSL_MCP_TASK_TTL_MS", ttl_ms.to_string());
}

/// A task stopped while the answer itself was being computed says THAT, and carries no cause
/// code at all.
///
/// Fails on the wording the waiting phase uses: the index was ready here, and a caller told
/// `index_building` would read a diagnosis of a state that did not happen. It also fails on a
/// build that lends the incompleteness vocabulary to an answer that was never produced —
/// those codes say why an answer a caller HAS is less than the whole answer.
///
/// A handle is opened only while the index is building, so the call has to land in the build
/// and the deadline after its end: a build that a loaded machine stretches past the deadline
/// fails the gate in the waiting phase. Nothing outside the server can pin that order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(
    not(timing_contract),
    ignore = "необязательный тест: the deadline must fall after a build the test cannot pin; запуск: CARGO_TARGET_DIR=target-timing RUSTFLAGS='--cfg timing_contract' cargo test --workspace"
)]
async fn a_task_stopped_mid_answer_does_not_blame_the_index() {
    arm(LONG_WORK_TTL_MS);

    let dst = stage(LONG_WORK_MODULES, LONG_WORK_BODIES);
    let mut wire = Wire::connect(dst.path()).await;

    let id = task_id(&wire.call("diagnostics", json!({"action": "workspace"})).await);
    let task = wire.settle(&id).await;

    assert_eq!(task["status"], "failed", "a task that ran out of time fails: {task}");
    assert_eq!(
        task["statusMessage"].as_str().unwrap_or_default(),
        "diagnostics workspace is reading the analysis index",
        "the gate must catch the deadline in the READING phase; if this stand became slow \
         enough that the index was still building, the waiting phase is a different \
         gate: {task}"
    );
    let message = task["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("ran out of time reading the analysis index"),
        "a task stopped mid-answer must not report that it was waiting for the index: {task}"
    );
    assert!(
        task["error"]["data"].is_null(),
        "no cause code belongs on an answer that was never produced: {task}"
    );
}
