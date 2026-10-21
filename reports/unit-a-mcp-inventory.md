# Check (unit-a): MCP server wait inventory

## Scope and method

Unit-a runs the workspace library and binary test harnesses with the four name filters:

```text
workspace --lib --bins --skip state::overlay_ --skip state::bootstrap:: --skip state::embed:: --skip state::sync::
```

The skips exclude matching tests, not whole packages. This inventory covers `crates/mcp-server/src` except `change_hub.rs` and `graph/**`, and records the skipped state-test families separately. `tools/graph.rs` and `graph_db.rs` are not under the excluded `graph/` path. `#[ignore]` tests are compiled but not run by the normal unit-a invocation; target-specific `#[cfg]` tests only run on their target.

Wait sites were searched for `ceiling`/`NO_BOUND`, `STATE_CEILING`, channel receive, thread/task join, condition-variable/barrier waits, polling loops, `Instant` deadlines, and timer sleeps. A wait is listed with the signal/release/join that lets it finish. Product actor waits and timer-contract tests are distinguished from test-harness waits.

## Fixes

### The diagnostics polling loop

`diagnostics_state/test_support.rs:170-224` supplies `wait_budget()` (environment override, default 30 seconds) and the shared `wait_until`. It previously used `test_utils::wait::polls`, which is `u64::MAX` outside `timing_contract`; therefore an unsuccessful probe could run forever despite a nominal budget. The helper now computes an absolute `Instant` deadline from `hang_guard(wait_budget())`, probes at the existing 10 ms cadence, and panics with the subject, last observation, poll count, and elapsed time. The timing profile keeps its configured budget (and existing 20-minute backstop); the ordinary profile gets a finite hang guard. The generic `polls`/`poll_until` implementation is deliberately not changed here.

### Shared wall-clock test guard

`test_wait.rs:1-27` adds synchronous helpers: `recv` uses a bounded channel receive; `wait_until` polls against a `std::time::Instant` deadline; `join` first waits for `JoinHandle::is_finished()` against that same wall clock, then joins. Their bound is `hang_guard(60s)`. This avoids making synchronous thread/channel waits depend on Tokio's clock.

`tools/search/call.rs:99-130` is the only use of a Tokio timeout around the async wait for the search body that runs on `spawn_blocking`; its test is an ordinary unpaused `#[tokio::test]`. The test now says so next to the timeout. The `start_paused` Windows executor tests instead wait for readiness on a blocking worker with a `std::time::Instant` bound (`check_executor_windows_tests.rs:115-173,209-264`); they do not put a Tokio timeout around that blocking readiness wait.

### Repaired test waits, with release/join dependencies

The former `ceiling`, `NO_BOUND`, and `STATE_CEILING` test waits in the in-scope files were replaced with `hang_guard` or the wall-clock helpers. Bounds remain the old nominal bound under `timing_contract`; ordinary success still ends on the event, with the finite guard only acting on a hang.

| File / locations | Wait and release/join path |
| --- | --- |
| `baseline.rs:2330-2460,3020-3105,3335-3415` | Deferred-slot waiters are woken by `publish` or `shutdown`; worker-gate waits have a test-controlled release; the gate, result receive, and joins now have guards. The ignored timing-only shutdown-wake test remains ignored. |
| `broker/daemon.rs:860-930,995-1040,1050-1235,1290-1355` | Attachment, event, stream, and daemon-task waits have explicit guards. The test sends shutdown/releases and joins the daemon after the client/session event. |
| `broker/name.rs:977-1010` | Unix runtime-directory race uses a two-party `Barrier`; each launcher join now has a wall-clock guard. The barrier itself has no timed API (residual risk below). |
| `broker/transport.rs:429-579` | The deadline test at `:429` intentionally uses paused Tokio time. Non-paused chunk/control tests now bound stream reads, control sends/receives, producer completion, and drain completion; the continuous producer is aborted before joining the drain. |
| `diagnostics_state/test_support.rs:170-224` | Shared diagnostics wait loop now uses the explicit wall-clock deadline described above. Its callers include readiness/build/reload/event tests in `diagnostics_state/{drift,lifecycle,session}.rs`. |
| `diagnostics_state/drift.rs:3965-4010` | A drain announces that it is parked while holding a cursor; `drop_cursor` performs the state transition, the test releases the drain, then a guarded join checks it finished. The release receive itself is guarded. Other event waits in `drift.rs` use the common `wait_until` (`:2117,2987-2996,3196,3279,3681-3862,4107-4722,5294-5714`). |
| `diagnostics_state/lifecycle.rs:1920-2125,2640-2960` | Salsa producer/writer/reader channels and polling deadlines are guarded. Dropping the held analysis releases the writer; an atomic stop releases producer threads before guarded joins. Lifecycle/status transitions use the shared bounded `wait_until`. |
| `diagnostics_state/session.rs:700-830,1135-1160` | Query-gate release, request completion, refresh and fresh-read blocking tasks are bounded. The release follows the observed query entry; both `spawn_blocking` task waits are in an unpaused multi-thread test. |
| `diagnostics_state/resident.rs:2925-2955` | The empty-sweep worker reports its result over a channel; the receiver now has a finite 120-second timing bound / ordinary hang guard. |
| `http.rs:315-345` | The server task is awaited under a finite guard after cancellation/shutdown. |
| `indexing_runtime_tests.rs:1-40,75-150` | Isolated child and loopback provider I/O have finite guards. Provider shutdown sets the stop flag, releases the condvar, and its nonblocking listener is woken by a loopback connection before Drop joins it. |
| `lib.rs:3716-4065,4200-4700,4895-4935,5790-5840,6120-6240` | Status/graph handlers, resident publication, and lock-holder protocols now use finite request/event guards. Lock holders are explicitly released before guarded joins; the background ownership-check joins are guarded. |
| `payload_smoke_tests.rs:20-30,140-155,240-370` | Smoke-case futures and local endpoint I/O use finite case guards. Endpoint Drop sets stop and connects to its listener before joining the server worker. One real-network smoke test is ignored. |
| `serve_stream_tests.rs:20-100,120-155,225-325,350-485,515-625,670-845` | `SHUTDOWN_TIMEOUT` is a `hang_guard(60s)`. Client initialization, MCP requests, shutdown/cancellation, task completion, and cancellation responses are bounded. Cancellation notifications release request handlers; clients are cancelled and their session tasks joined afterward. |
| `state/mod.rs:1530-1815,2150-2195` | Owner lifecycle receives, condition-variable waits, and thread joins have finite guards. For `wait_empty_in_slices(Duration::MAX, ...)`, the product call still receives `Duration::MAX`; it now runs on an observer thread and the test thread waits on a guarded result channel, then joins the observer and the owner-leaver. This protects the test from a regression in the product wait without changing the unbounded-budget behavior being tested. |
| `tasks.rs:500-660,870-970` | Task terminal-state requests and test gates use finite bounds; the yielding readiness helper now uses an absolute deadline. Two `start_paused` task tests (`:620,:875`) retain deliberate virtual-clock control of task deadlines. |
| `check_executor_wire_tests.rs:10-100,115-175` | The Unix framed-wire test bounds initialization, marker publication, and JSON-RPC response reads. Its 700 ms post-response drain remains an intentional negative-response observation window, not a readiness wait. |
| `tools/check_executor.rs:1585-1605` | The background reaper test bounds the wait for its permit after releasing the stopped child. |
| `tools/graph.rs:575-620` | A held descriptor reader reports entry, the test sends its release, checks the answer, then guarded-joins the reader. This is `tools/graph.rs`, not the excluded `graph/**` subtree. |
| `tools/search/acquire.rs:450-650`, `tools/search/call.rs:95-135`, `tools/search/cancel_tests.rs:125-430`, `tools/search/status.rs:1320-1370,1980-2050`, `tools/search/test_support.rs:40-80`, `tools/search/wait.rs:90-130` | Admission, cancellation, held-lock, and detached-body signals use bounded receives/polls/joins. Cancellation releases the queued request before the lock holder is released; queued/follower workers are joined only after the relevant release or result signal. The async search-call timeout is explicitly limited to its non-paused test runtime. |
| `workspace_lease.rs:370-410,1850-1910,2280-2320,2600-2720,2825-2870,3090-3135` | Lock-holder and checkpoint channels, release publication polls, and worker joins are bounded. The child-process lock fixture has a finite announcement receive and kills/reaps the child on timeout. |

## Remaining waits and interpretation

These matches are intentional or remain a specific risk rather than an accidentally unbounded test wait:

- `baseline.rs:847,3020-3035` are production service receive loops; sender shutdown/drop terminates them. `baseline.rs:314-325` is the production deferred-slot wait, bounded by its caller's budget and woken by publish/shutdown.
- `broker/name.rs:993-1000` retains its worker-only barrier; all completion joins are guarded on the test thread. A missing participant therefore trips the outer guard. Main-thread barriers in `tools/search/status.rs:1333-1346,1993-2006` are replaced with guarded lock-acquired channels. The redundant barrier in `tools/search/acquire.rs:522-536` is removed: the existing guarded `engine.queued() == 1` observation already proves the worker reached admission.
- `indexing_runtime_tests.rs:140-146` and `payload_smoke_tests.rs:140-155` fixture Drop joins now also use the wall-clock completion guard, after their explicit stop/condvar/listener release. Payload cleanup continues to suppress worker panic propagation while unwinding; a hung cleanup itself reports the watchdog failure.
- Fixed sleeps in `diagnostics_state/drift.rs` and `session.rs` around `FORCE_RESCAN_FLOOR` are tests of real rescan/timer behavior, not state-readiness polling. Product deadlines and process-reaping waits in `tools/check_executor.rs` are likewise part of the executor contract; they were not replaced with test-only budgets.
- `tools/search/status.rs` retains timed event windows and `cfg!(timing_contract)` latency assertions. Their result is not used to make ordinary success depend on a faster CPU.

## cfg, ignored tests, and unit-b exclusions

- `broker/name.rs:977` contains `#[cfg(unix)]` launch-race coverage. `check_executor_windows_tests.rs` is Windows-only; its `#[tokio::test(start_paused = true)]` executor tests use Tokio virtual time for product deadlines and a separate blocking-thread/Instant readiness observer. They are not Linux unit-a tests.
- Ignored in-scope tests are `baseline.rs:2434` (timing-only wake bound), `payload_smoke_tests.rs:244` (real HTTP/fixed-deadline smoke test), and `graph_db.rs:3815` (requires external graph DB/source environment). Normal unit-a does not execute these.
- The four `--skip` patterns exclude unit-b state tests. Their waits were not edited by this unit-a work: candidates remain in `state/overlay_backlog.rs:760,1023-1040,1380-1440,1880-2115`, `state/overlay_retry.rs:1240-1320`, `state/bootstrap.rs:4210-4220,5340-5640,5785-6030,6900-6940`, `state/embed.rs:2080-2120,3030-3245,3355-3395,3570-3765,3850-4125,4220-4410,4590-4625,4895-4915,5305-5340`, and `state/sync.rs:1595-1630,3055-3350,5660-5775`. These are reported as excluded work, not treated as unit-a coverage.
- `change_hub.rs` and `graph/**` are excluded from this inventory by the task boundary; their waits and releases are not represented in the table above.

## Validation and limits

No Cargo command or test execution was started for this final patch, per instruction. The selected in-scope Rust files were formatted with `rustfmt --edition 2021 --config skip_children=true`; `git diff --check` passed. The parent reported that a background `--no-run` build and an ordinary-profile positive control had already completed; neither was rerun after these last edits. The positive-control's expected ten-minute guard failure is not treated as a task failure. Compilation and runtime behavior of the final edits therefore remain unverified here.
