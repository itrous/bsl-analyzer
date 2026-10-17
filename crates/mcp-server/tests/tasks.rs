//! The task branch of execution, end to end over the real transport.
//!
//! Spoken as raw JSON-RPC frames rather than through a client library, because no shipping
//! client declares the `io.modelcontextprotocol/tasks` extension yet: the only client that
//! can exercise this branch is one written here, and what the server puts on the wire is the
//! contract either way.
//!
//! Every gate below names the input on which it must fail. The two that matter most are the
//! pair in [`a_client_that_declared_nothing_reads_the_answer_it_always_read`]: without the
//! second half the first is green for a server that never opens a task at all, and without
//! the first the second is green for a server that answers every non-declaring caller with
//! an error.

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

/// The declaration every gate asks about. Declared by a module the fixture's configuration
/// actually lists, so it is visible to a caller at all.
#[cfg(unix)]
const STAND_METHOD: &str = "ПервыйОбщийМодуль.НеУстаревшаяФункция";

/// The stand the cancellation gate needs: enough modules, each with enough bodies, that the
/// whole-config sweep over them takes long enough to tell a stopped read from an abandoned
/// one. Measured on this stand: the sweep runs 13 s while the resident build costs 0.7 s,
/// and a read issued while it runs waits 13.2 s for it. A lighter stand sweeps in under a
/// second, which is inside the noise of the answer that follows — the gate would then pass
/// on both builds.
const SWEEP_STAND_MODULES: usize = 800;
const SWEEP_STAND_BODIES: usize = 100;

/// How long the sweep is left to run before it is cancelled.
const SWEEP_DWELL: Duration = Duration::from_secs(2);

/// Bound on any wait for the server to answer or for an owner to leave, under
/// `timing_contract`; none otherwise (see [`test_utils::wait`]). It is a hang guard: every
/// such wait ends on its own when the code is right, so the bound is only ever reached by a
/// server that never answers, and no assertion depends on how long the answer took.
const HANG_GUARD: Duration = test_utils::wait::ceiling(Duration::from_secs(180));

/// Polls `tasks/get` at this interval, for at most [`HANG_GUARD`] worth of polls.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Callers of [`STAND_METHOD`] on the stand whose resident is held. A few give the references
/// something to find; more would buy nothing, because the hold, not the size of the stand,
/// keeps the resident unready.
#[cfg(unix)]
const HELD_STAND_CALLERS: usize = 3;

/// Every gate here exercises the branch, so the knob is on for the whole binary. Read per
/// call by the server, so setting it once before any server is built is enough.
fn arm_the_branch() {
    std::env::set_var("BSL_MCP_TASKS", "1");
}

fn stage(callers: usize, bodies: usize) -> TempDir {
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
    for index in 0..callers {
        let mut body = String::new();
        for nth in 0..bodies {
            body.push_str(&format!(
                "Процедура Вызвать{index:04}_{nth}() Экспорт\n    \
                 ПервыйОбщийМодуль.НеУстаревшаяФункция();\n    \
                 Знач = 1 + {nth};\nКонецПроцедуры\n"
            ));
        }
        write_module(dst.path(), &format!("Толпа{index:04}"), &body);
    }
    dst
}

fn write_module(root: &Path, name: &str, body: &str) {
    let dir = root.join("CommonModules").join(name).join("Ext");
    std::fs::create_dir_all(&dir).expect("mkdir module");
    std::fs::write(dir.join("Module.bsl"), body).expect("write module");
}

fn server_for(root: &Path) -> Retired {
    let state = SharedState::workspace(root.to_path_buf()).expect("valid workspace project");
    let gate = ToolGate::for_launch(McpProfile::Workspace, &[TOOL.to_owned()]);
    Retired(McpServer::with_gate(McpProfile::Workspace, state, &gate))
}

/// A metadata object the resident build cannot read past until [`BuildHold::release`].
///
/// The object's description is a FIFO. A read of it returns only once its writer closes, and
/// its only writer is this hold, which keeps it open and writes nothing. The resident build
/// loads every object the configuration lists before it publishes the resident, so until the
/// release the resident stays unready however fast or slow the machine runs: the gates below
/// meet an unready resident because it is held there, not because they outran its build.
/// [`Wire::resident_state`] reads that state back, so a build that stopped reading the
/// object fails the gate instead of letting it race the build again unnoticed.
///
/// An object description rather than a module, because the source walk takes regular files
/// only and would never open a FIFO standing in for a module.
#[cfg(unix)]
struct BuildHold {
    path: std::path::PathBuf,
    release: Option<std::sync::mpsc::Sender<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl BuildHold {
    /// The listed object whose description is held. No gate here reads it.
    const OBJECT: &str = "Catalogs/Справочник1.xml";

    /// Plant the hold in `root`. Done before the server is built, so the server and its build
    /// meet the FIFO whenever they first look, and the writer is started with it, so a reader
    /// waits on the hold rather than on a writer that has not come yet.
    fn plant(root: &Path) -> Self {
        let path = root.join(Self::OBJECT);
        std::fs::remove_file(&path).expect("the staged object description");
        let made = std::process::Command::new("mkfifo").arg(&path).status().expect("mkfifo runs");
        assert!(made.success(), "a FIFO stands in for the held object");

        let (release, released) = std::sync::mpsc::channel::<()>();
        let fifo = path.clone();
        let writer = std::thread::spawn(move || {
            let Ok(open) = std::fs::OpenOptions::new().write(true).open(&fifo) else {
                return;
            };
            let _ = released.recv();
            drop(open);
        });
        Self { path, release: Some(release), writer: Some(writer) }
    }

    /// Let the build read past the held object, which then reads as empty everywhere: the
    /// readers already inside the FIFO see its end, and a later reader finds an empty file.
    fn release(&mut self) {
        let Some(release) = self.release.take() else {
            return;
        };
        // Opened for reading and writing, the FIFO opens at once and meets a writer still
        // waiting for its first reader, so the writer below always gets to close. Kept open
        // until then, so the readers inside see the end only once both have closed.
        let unblock =
            std::fs::OpenOptions::new().read(true).write(true).open(&self.path).expect("open FIFO");
        // Without a regular file in its place a later reader would wait for a writer nobody
        // starts.
        let plain = self.path.with_extension("plain");
        std::fs::write(&plain, "").expect("write the plain description");
        std::fs::rename(&plain, &self.path).expect("the plain description replaces the FIFO");
        let _ = release.send(());
        if let Some(writer) = self.writer.take() {
            writer.join().expect("the writer closes the held object");
        }
        drop(unblock);
    }
}

#[cfg(unix)]
impl Drop for BuildHold {
    fn drop(&mut self) {
        self.release();
    }
}

/// A server over a stand whose resident build is held, with the hold dropped before the
/// server: the server's owners can then leave without waiting on a build that cannot end.
#[cfg(unix)]
struct HeldStand {
    hold: BuildHold,
    server: Retired,
    _root: TempDir,
}

#[cfg(unix)]
fn held_stand() -> HeldStand {
    let root = stage(HELD_STAND_CALLERS, 1);
    let hold = BuildHold::plant(root.path());
    let server = server_for(root.path());
    HeldStand { hold, server, _root: root }
}

/// One client connection, spoken as frames.
struct Wire {
    write: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    read: Box<dyn tokio::io::AsyncBufRead + Send + Unpin>,
    next_id: i64,
}

impl Wire {
    /// A session on `server`, handshaken. `declares_tasks` is the whole difference between
    /// the two callers these gates compare.
    async fn connect(server: &McpServer, declares_tasks: bool) -> Self {
        let (client_io, server_io) = tokio::io::duplex(4 * 1024 * 1024);
        tokio::spawn(serve_stream(server.clone(), server_io));
        let (read_half, write_half) = tokio::io::split(client_io);
        let mut wire = Self {
            write: Box::new(write_half),
            read: Box::new(BufReader::new(read_half)),
            next_id: 1,
        };

        let capabilities = if declares_tasks {
            json!({"extensions": {"io.modelcontextprotocol/tasks": {}}})
        } else {
            json!({})
        };
        let hello = wire
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": capabilities,
                    "clientInfo": {"name": "task-gate", "version": "0"}
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

    /// Send one request and read frames until its answer arrives.
    ///
    /// Bounded on purpose: the failure several of these gates exist to catch is a server
    /// that never answers, and an unbounded read would hang the run instead of reporting it.
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

    #[cfg(unix)]
    async fn references(&mut self, symbol: &str) -> Value {
        self.call(TOOL, json!({"symbol": symbol})).await
    }

    /// The resident's lifecycle as `metadata status` reports it. Answered directly, never as a
    /// task, whatever the caller declared.
    #[cfg(unix)]
    async fn resident_state(&mut self) -> Value {
        let answer = self.call("metadata", json!({"action": "status"})).await;
        answer["result"]["structuredContent"]["state"].clone()
    }

    async fn get_task(&mut self, task_id: &str) -> Value {
        self.request("tasks/get", json!({"taskId": task_id})).await
    }

    async fn cancel_task(&mut self, task_id: &str) -> Value {
        self.request("tasks/cancel", json!({"taskId": task_id})).await
    }

    /// Poll until `settled` accepts a `tasks/get` answer. Under `timing_contract` the polls are
    /// counted against [`HANG_GUARD`] and running out gives up loudly; an ordinary run polls until
    /// the answer comes, so a slow machine waits longer for it instead of failing on it.
    async fn poll_until(
        &mut self,
        task_id: &str,
        what: &str,
        settled: impl Fn(&Value) -> bool,
    ) -> Value {
        let mut last = Value::Null;
        for _ in 0..test_utils::wait::polls(HANG_GUARD, POLL_INTERVAL) {
            let answer = self.get_task(task_id).await;
            let task = &answer["result"];
            if settled(task) {
                return task.clone();
            }
            last = task.clone();
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        panic!("the task never reached {what}; last seen: {last}");
    }
}

fn task_id(answer: &Value) -> String {
    assert_eq!(answer["result"]["resultType"], "task", "expected a task handle, got: {answer}");
    answer["result"]["taskId"].as_str().expect("a task carries an id").to_owned()
}

/// The branch is the caller's choice, and a caller that made no choice is untouched by it.
///
/// Both halves are required. The first alone is green for a build that never opens a task;
/// the second alone is green for a build that answers `-32021` to everyone who did not ask.
/// The two callers differ only in the capability they declared, and they share one server, so
/// nothing but that declaration can explain the difference.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_declared_nothing_reads_the_answer_it_always_read() {
    arm_the_branch();
    let stand = held_stand();
    let server = &stand.server;

    let mut silent = Wire::connect(server, false).await;
    let answer = silent.references(STAND_METHOD).await;
    assert!(
        answer["error"].is_null(),
        "a caller that declared no extension must not be answered with an error \
         (a `-32021` here means the branch was left to the dispatcher instead of taken \
         by the handler): {answer}"
    );
    assert_eq!(
        answer["result"]["structuredContent"]["status"], "loading",
        "the synchronous answer while the index builds is the envelope it has always been: \
         {answer}"
    );

    let mut declaring = Wire::connect(server, true).await;
    let offered = declaring.references(STAND_METHOD).await;
    assert_eq!(
        offered["result"]["resultType"], "task",
        "a caller that declared the extension must be handed a handle instead of the \
         envelope: {offered}"
    );
    assert_eq!(declaring.resident_state().await, "loading", "the hold keeps the resident building");
}

/// The branch belongs to the resident, not to one tool that reads it.
///
/// Fails on a build that wires the handle into a single tool: the four others would answer
/// with their `loading` envelope, and each is named separately here.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_tool_that_waits_on_the_resident_offers_the_handle() {
    arm_the_branch();
    let stand = held_stand();
    let mut wire = Wire::connect(&stand.server, true).await;

    // Each call has to meet an unready resident, which is the only state this branch answers
    // for; the hold keeps it unready through all of them.
    let calls = [
        ("metadata", json!({"action": "info"})),
        ("symbol_info", json!({"symbol": STAND_METHOD})),
        (TOOL, json!({"symbol": STAND_METHOD})),
        (
            "diagnostics",
            json!({"action": "file", "path": "CommonModules/Толпа0000/Ext/Module.bsl"}),
        ),
        ("diagnostics", json!({"action": "workspace"})),
    ];
    for (tool, arguments) in calls {
        let answer = wire.call(tool, arguments.clone()).await;
        assert_eq!(
            answer["result"]["resultType"], "task",
            "`{tool}` {arguments} answered without a handle while the resident was held \
             building: {answer}"
        );
    }
    assert_eq!(wire.resident_state().await, "loading", "the hold keeps the resident building");
}

/// A handle that waited hands back the answer, not a prettier way of saying "still building".
///
/// Fails on the implementation this branch is easiest to mistake for: one that wraps the
/// `loading` envelope in a task and calls it completed.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_handle_carries_the_real_answer() {
    arm_the_branch();
    let mut stand = held_stand();
    let mut wire = Wire::connect(&stand.server, true).await;

    let id = task_id(&wire.references(STAND_METHOD).await);
    assert_eq!(wire.resident_state().await, "loading", "the hold keeps the resident building");
    stand.hold.release();
    let task = wire.poll_until(&id, "a terminal status", |task| task["status"] != "working").await;

    assert_eq!(task["status"], "completed", "the task must complete: {task}");
    let body = &task["result"]["structuredContent"];
    assert_ne!(
        body["status"], "loading",
        "a completed task carrying the retry envelope is the envelope with extra steps: {task}"
    );
    assert_eq!(
        body["outcome"], "resolved",
        "the body must be the answer the synchronous call would have given a ready \
         resident: {task}"
    );
}

/// The handle outlives the connection that opened it.
///
/// The second session issues no tool call at all — it only asks about the id — so an answer
/// arriving there can only come from work that was already running. A build that started the
/// work over on demand would have nothing to answer from.
///
/// The boundary is not tested here because it is not a code path: a handle lives as long as
/// the daemon process, and the contract says so.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handle_outlives_the_connection_that_opened_it() {
    arm_the_branch();
    let mut stand = held_stand();

    let mut opener = Wire::connect(&stand.server, true).await;
    let opened = opener.references(STAND_METHOD).await;
    let id = task_id(&opened);
    let created_at = opened["result"]["createdAt"].clone();
    assert!(created_at.is_string(), "a handle is stamped when it is opened: {opened}");
    assert_eq!(opener.resident_state().await, "loading", "the hold keeps the resident building");
    drop(opener);
    stand.hold.release();

    let mut later = Wire::connect(&stand.server, true).await;
    let task = later.poll_until(&id, "a terminal status", |task| task["status"] != "working").await;
    assert_eq!(task["taskId"], id.as_str(), "the same handle, not a new one: {task}");
    assert_eq!(
        task["createdAt"], created_at,
        "a re-stamped handle means the work was started again rather than continued: {task}"
    );
    assert_eq!(task["status"], "completed", "the work continued to its answer: {task}");
    assert_eq!(
        task["result"]["structuredContent"]["outcome"], "resolved",
        "and the answer is the real one: {task}"
    );
}

/// Cancelling a task that is still waiting for the index settles it, with no worker to reach.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_waiting_handle_settles_it() {
    arm_the_branch();
    let stand = held_stand();
    let mut wire = Wire::connect(&stand.server, true).await;

    let id = task_id(&wire.references(STAND_METHOD).await);
    assert_eq!(wire.resident_state().await, "loading", "the hold keeps the resident building");
    // Cancelled while it is demonstrably still waiting on the index rather than reading it:
    // without this the gate could be cancelling a task that had already moved on, and the
    // waiting phase — where there is no worker to signal — would never be exercised. The
    // hold keeps it waiting until the cancellation has settled it.
    let waiting = wire
        .poll_until(&id, "the waiting phase", |task| {
            task["statusMessage"].as_str().is_some_and(|m| m.contains("waiting"))
        })
        .await;
    assert_eq!(waiting["status"], "working", "still running while it waits: {waiting}");

    let ack = wire.cancel_task(&id).await;
    assert!(ack["error"].is_null(), "cancellation is acknowledged: {ack}");
    let task = wire.poll_until(&id, "a terminal status", |task| task["status"] != "working").await;
    assert_eq!(task["status"], "cancelled", "a waiting task settles itself: {task}");
}

/// Cancelling a task that is reading the resident stops the read and releases the resident.
///
/// The release is the half that cannot be skipped. A build that drops the task's future
/// instead of cancelling it looks identical up to the status — `cancelled` either way — and
/// differs only here: the blocking read keeps running, and the next caller waits behind it.
///
/// How long that next caller waits is a speed, so the gate does not compare it with a fixed
/// bound. It compares it with the wait behind the same sweep left running on an identical stand,
/// taken under the same load: a released resident answers in a small fraction of the time the
/// sweep takes to finish, and an abandoned one answers only after the sweep has finished.
///
/// That comparison is the only thing that tells a released resident from an abandoned one, and
/// it is a reading of the clock, so it — and with it the release this test is named for — is
/// checked by the timing profile alone.
///
/// The rest of the gate depends on speed too, which is why the ordinary run does not take it at
/// all. The cancellation has to land while the sweep is still reading, and nothing outside the
/// server can hold the sweep there: the sweep reads the modules the build has already read, so
/// stalling it on one would mean changing that module on disk in the middle of the build, at a
/// point no caller can observe, and the server answers a change on disk by rebuilding. Whether
/// the reading phase is seen, and is still running when the cancellation arrives, is decided by
/// how long the sweep takes on the machine at hand.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(
    not(timing_contract),
    ignore = "необязательный тест: the cancellation must land while a sweep the test cannot hold is still reading; запуск: CARGO_TARGET_DIR=target-timing RUSTFLAGS='--cfg timing_contract' cargo test --workspace"
)]
async fn cancelling_a_working_handle_stops_the_read_and_frees_the_resident() {
    arm_the_branch();
    let live_stand = stage(SWEEP_STAND_MODULES, SWEEP_STAND_BODIES);
    let live = server_for(live_stand.path());
    let waited_behind_live = wait_behind_sweep(&live, false).await;

    let cancelled_stand = stage(SWEEP_STAND_MODULES, SWEEP_STAND_BODIES);
    let cancelled = server_for(cancelled_stand.path());
    let waited_behind_cancelled = wait_behind_sweep(&cancelled, true).await;

    if cfg!(timing_contract) {
        assert!(
            waited_behind_cancelled * 3 < waited_behind_live,
            "the cancelled read still held the resident: the next caller waited \
             {waited_behind_cancelled:?} behind a cancelled sweep against \
             {waited_behind_live:?} behind a live one"
        );
    }
}

/// Starts the whole-config sweep on `server`, waits until it is reading, then cancels it or
/// leaves it running, and returns how long the next caller waited for the resident.
async fn wait_behind_sweep(server: &McpServer, cancel: bool) -> Duration {
    let mut wire = Wire::connect(server, true).await;

    // The whole-config sweep, not a reference walk: it is the heaviest thing that runs under
    // the resident lock, and the difference this gate measures is only visible while that
    // lock is held. A walk over the same stand finishes in a few hundred milliseconds, which
    // is inside the noise of the answer that follows — a build that abandoned the read was
    // measured passing on it.
    let id = task_id(&wire.call("diagnostics", json!({"action": "workspace"})).await);
    // The injection point, fixed rather than timed: the task says on the wire when it stops
    // waiting and starts reading, and only then is there blocking work for a cancellation to
    // reach. Cancelling earlier would exercise the waiting phase instead — which is a
    // different gate — and pass on a build whose cancellation reaches no worker at all.
    wire.poll_until(&id, "the reading phase", |task| {
        task["statusMessage"].as_str().is_some_and(|m| m.contains("reading"))
    })
    .await;
    // The phase flips when the work is handed to a blocking thread, not when that thread
    // gets to run. Cancelling in that window stops work that had not started, which every
    // build survives; the dwell puts the sweep in flight — and holding the resident — before
    // anything is asked of the cancellation. Only the timing profile, which compares the
    // waits, needs the sweep in flight.
    if cfg!(timing_contract) {
        tokio::time::sleep(SWEEP_DWELL).await;
    }

    if cancel {
        wire.cancel_task(&id).await;
        let task =
            wire.poll_until(&id, "a terminal status", |task| task["status"] != "working").await;
        assert_eq!(task["status"], "cancelled", "the read was cancelled, not finished: {task}");
    }

    let started = std::time::Instant::now();
    let freed = wire.call("metadata", json!({"action": "info"})).await;
    let waited = started.elapsed();
    assert!(
        freed["error"].is_null() && freed["result"].is_object(),
        "the next caller gets a normal answer after {waited:?}: {freed}"
    );
    waited
}

/// A task that answered before the cancellation is not reported as cancelled.
///
/// Fails on a build that treats the arrival of `tasks/cancel` as the outcome rather than a
/// request the work may already have outrun.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handle_that_answered_first_is_not_reported_as_cancelled() {
    arm_the_branch();
    let mut stand = held_stand();
    let mut wire = Wire::connect(&stand.server, true).await;

    let id = task_id(&wire.references(STAND_METHOD).await);
    assert_eq!(wire.resident_state().await, "loading", "the hold keeps the resident building");
    stand.hold.release();
    let task = wire.poll_until(&id, "a terminal status", |task| task["status"] != "working").await;
    assert_eq!(task["status"], "completed", "the work finished on its own: {task}");

    wire.cancel_task(&id).await;
    let after = wire.get_task(&id).await;
    assert_eq!(
        after["result"]["status"], "completed",
        "a terminal task is not re-labelled by a late cancellation: {after}"
    );
}
