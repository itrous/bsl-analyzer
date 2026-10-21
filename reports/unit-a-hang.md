# Check (unit-a): inventory and hang containment

## Scope and evidence

Slice base: `94924aaef7d0fae9f91d6000a0efb6a98d4ff6ce`.

`.github/workflows/ci.yml:47-48,64` executes:

```sh
cargo test --locked --workspace --no-fail-fast --lib --bins -- \
  --skip state::overlay_ --skip state::bootstrap:: \
  --skip state::embed:: --skip state::sync::
```

Filters are substring exclusions across all library/binary test harnesses, not package exclusions. Integration targets, examples and doctests are outside unit-a. Ignored/timing-only tests are not executed by this leg.

`cargo metadata --no-deps --format-version 1` identifies these selected targets (package / target):

- base-db / base_db; bsl-conventions / bsl_conventions; parser / parser; lexer / lexer; stdx / stdx; parser-error / parser_error; syntax / syntax; vfs / vfs; paths / paths
- bsl-analyzer / bsl_analyzer, bsl-analyzer-app; bsl-debug / bsl_debug; bsl-metadata / bsl_metadata; bsl-platform / bsl_platform; html-parser / html_parser, html-parser; intern / intern
- bsl-search / bsl_search; code-chunk / code_chunk; project-model / project_model; test-utils / test_utils; hir / hir; bsl-config / bsl_config; bsl-types / bsl_types; cfg / cfg; cfg-types / cfg_types
- hir-def / hir_def; line-index / line_index; sdbl-hir / sdbl_hir; ide-db / ide_db; hir-ty / hir_ty; dataflow / dataflow; test-fixture / test_fixture; ide / ide; ide-assists / ide_assists; ide-diagnostics / ide_diagnostics
- symbol-info / symbol_info; ide-host-core / ide_host_core; mcp-server / mcp_server; check-contract / check_contract; naparnik / naparnik; onec-client / onec_client; profile / profile; vcs / vcs; platform-help / platform_help; vfs-notify / vfs_notify
- bsl-check-executor / bsl_check_executor, bsl-native-check; bsl-launcher / bsl-analyzer; retired-material-gate / retired_material_gate; xtask / xtask

### Supplied cold-run reference

[Run 39490147478](https://github.com/itrous/bsl-analyzer/actions/runs/39490147478), queried through GitHub Actions API and downloaded logs, has `run_attempt=1`, commit `74c088bfb1c1f77b0ebbc47060b2aff08ba557c6`, and [Check (unit-a), job 118860853802](https://github.com/itrous/bsl-analyzer/actions/runs/39490147478/job/118860853802) reports **success**, 2026-10-20 20:13:52–20:28:06 UTC. The cargo step ran 20:14:06–20:27:51. There is no cancelled hour-long unit-a job or `over 60 seconds` test in the supplied run's log. Thus no particular test or product race is established as the historical root cause. The missing watchdogs below are independently observable; they are not evidence that a particular race caused that incident.

## Inventory

The accompanying inventories enumerate test wait sites, shared budgets and dependent release/join paths. Their local validation disclaimers describe the inventory authors' static-only handoff; the later aggregate validation below supersedes those disclaimers:

- [MCP except graph/change hub](unit-a-mcp-inventory.md)
- [Graph and change hub](unit-a-graph-inventory.md)
- [Other workspace crates](unit-a-other-inventory.md)

Test-utils infrastructure (locations in the slice base):

| Location | Previously unbounded path | Outcome |
| --- | --- | --- |
| `crates/test-utils/src/wait.rs:34-39` | `ceiling` returns `NO_BOUND` outside timing-contract | API retained for out-of-scope callers and arithmetic contract tests; unit-a state waits migrate to `hang_guard` |
| `crates/test-utils/src/wait.rs:44-65` | `polls` returns `u64::MAX`; `poll_until` can wait forever even if passed `hang_guard(...)` | `poll_until` now also enforces an absolute `Instant + hang_guard(bound)` deadline; timing poll count retained |
| `crates/test-utils/src/wait.rs:101-105` | receiver waits `ceiling(60s)` for spawned sender | `hang_guard(60s)`; state still ends wait |
| `crates/test-utils/src/walk_probe.rs:114-119` | held-span callback waits indefinitely on release condvar | `wait_timeout_while` with `hang_guard(60s)` and explicit release assertion |
| `crates/test-utils/src/walk_probe.rs:258` | HeldWalk waits for span through `ceiling(60s)` | `hang_guard(60s)` |
| `crates/test-utils/src/walk_probe.rs:272` | `await_walk_start` inherits unbounded `poll_until` | bounded by corrected shared poll helper |
| `crates/test-utils/src/walk_probe.rs:296-298` | test spins until walk entered while holding gate mutex | guarded state polling; gate dropped before dependent join |
| `crates/test-utils/src/walk_probe.rs:302,317` | dependent joins | entered/release protocol guarded; held walk is released by RAII even on assertion unwind |

The NO_BOUND equality/arithmetic assertions in `wait.rs` do not wait. Mutex acquisitions for local data are not state-poll protocols; protocol waits and their release dependencies are listed rather than treating every lexical `lock()` as a timed assertion.

## Behaviour choice

Keep state-readiness tests enabled with a ten-minute hang guard, rather than ignore a speculative historical-hang candidate. The separate ledger-order scenario `graph::state::tests::a_quiet_delivery_is_listed_only_once_the_ledger_has_it` is explicitly optional outside `timing_contract`: its absence-of-publication assertion inherently uses a negative time window. Its `ignore` reason explains this and gives the exact opt-in command; the invariant is no longer silently skipped inside a nominally required test. No product behaviour changes. Normal success remains defined by the awaited state, not CPU speed or a shorter timing deadline. A watchdog failure means the state did not arrive; it does not assert performance. No shortcuts or new external dependencies. The existing workspace `test-utils` crate is added as a dev-dependency to `bsl-check-executor`, `bsl-launcher`, and `intern`, so their tests reuse the prescribed guard rather than duplicate it.

## Validation

### Positive control

A standalone harness imports the actual `crates/test-utils/src/wait.rs` and retains a live sender without ever sending. Source (resolve the module path to the checkout when placing the harness in `/tmp`; compile with `rustc --edition=2024`, without `--cfg timing_contract`):

```rust
#[path = "<checkout>/crates/test-utils/src/wait.rs"]
#[allow(dead_code)] // This standalone control only exercises hang_guard.
mod wait;
fn main() {
    let (_sender, receiver) = std::sync::mpsc::channel::<()>();
    assert!(
        receiver.recv_timeout(wait::hang_guard(std::time::Duration::ZERO)).is_ok(),
        "hang guard: unreachable state never arrived"
    );
}
```

- Ordinary profile: started **2026-10-21T19:54:57Z**, panicked **2026-10-21T20:04:57Z**, exit **101**, with exactly that message. This is the expected positive-control result: the real ten-minute guard expires rather than waiting forever. `Duration::ZERO` does not bypass the ordinary-profile ten-minute minimum.
- Timing profile: `rustc --edition=2024 --cfg timing_contract --test crates/test-utils/src/wait.rs -o /tmp/unit-a-evidence/wait-timing`, then `wait-timing --exact tests::an_unreachable_state_fails_with_a_message --nocapture`: **1 passed**, expected panic with the same message (zero nominal bound, no CPU-speed race).
- Cold Linux `cargo test --locked --workspace --lib --bins --no-run`: exit **0**, build completed in **3m 46s**. This compilation is not a diagnostic execution.

### Preparation findings

Two initial cargo commands stopped before executing any test binary: missing `Duration` imports in two MCP test modules (3 × E0433), and a missing local `HANG_WATCHDOG` constant in the search lifecycle fixture (2 × E0425). Both findings were fixed in this slice. A fresh `cargo test --locked --workspace --lib --bins --no-run` then compiled all targets successfully (exit 0). `cargo fmt --all -- --check` and `git diff --check` also pass. These compilation failures are not presented as reproductions or test passes.

### Bounded diagnostic executions

R1 and R2 ran concurrently on Linux (normal cargo/libtest parallelism plus two independent harness processes, not a serial isolated reproduction loop):

| Run | Command / result | Outcome |
| --- | --- | --- |
| R1 | `cargo test -p mcp-server --lib` — exit 101; 1725 passed, 3 failed, 31 ignored; 610.14s | New test-only module inventory entry missing; two cancellation matrix tests poisoned their held mutex because channel-close release was incorrectly changed into an error. All corrected. |
| R2 | Exact unit-a command above — exit 101; MCP 1490 passed, same 3 failed, 31 ignored, 235 filtered; other targets green | Same patch regressions as R1, not a reproduction of the historical hang. |
| R3 | `cargo test -p mcp-server --lib` — **exit 0; 1728 passed, 0 failed, 31 ignored; 151.45s** | All Linux MCP library tests green after corrections. |
| R4 | [CI workflow_dispatch 39674927214](https://github.com/itrous/bsl-analyzer/actions/runs/39674927214), branch `pio/fix-unit-a-hang`, code commit `d09345c40db10b69f99bbb936ac1e28bfaffccb2` | Unit-a and several Windows legs stopped at compilation: `unused_must_use` on aborted producer JoinError and Windows dead-code warnings in Unix-only test helpers. Not a historical hang reproduction. Corrected; subsequent CI is corrective acceptance, not further historical diagnosis. |

R1/R2 also exposed a needless ten-minute delay in `graph::state::tests::a_wait_that_times_out_reports_the_state_it_observed`: an intentional zero-budget negative control was re-wrapped in `hang_guard` by the shared polling helper. `graph/test_support.rs::wait_until_within` now honours its explicit, already-guarded caller budget directly; normal state callers pass `hang_guard`, and the deterministic negative control can keep zero. The test remains required, not ignored. Release-on-channel-close semantics are also retained for the diagnostics drain fixture.

New-finding outcomes: missing imports/local constant **fixed**; test-module census **fixed** (`inventory.rs`); cancellation release semantics/mutex poisoning **fixed** (`lib.rs::search_cancellation_matrix::hold_lock`); negative-control double guard **fixed** (`graph/test_support.rs`); uncontrolled ordinary state waits **contained with guards**. No suspected historical-hang test has been speculatively ignored. Historical root cause remains unestablished. Compilation-only preparation failures and positive controls are explicitly separate from these four executions.

## Review 939 findings and corrections

Subject: commit `cd35c01e98cdb0ad9ace918da9e16e936c6740e4`, original slice base above. The round had one high, so it was not ready and a new review is required after the correction.

- `4bde725de0dddba806643ca783994bc89a42b6135590e2c494ae4b9645b1ae3b` — **high, upheld, fixed in `bdcb3478`**. Exact root-lock test passed before correction but code tracing confirmed a legal pre-cancel schedule. Both cancellation tests now explicitly poll the contended future to Pending before cancelling; no scheduling sleep or yield establishes readiness. `cargo test -p bsl-check-executor --lib native_check::`: **70 passed, 0 failed** after correction.
- `2402688515aa6dbed75b1cf8be307da5ec7e3e747f476cec35ab944294010c45` — **mid, upheld, fixed**. Reproduced pipe backpressure with a child writing 1 MiB before exit: wait-without-drain blocks; draining returns the full output and exit 0. The two graph wrappers now use shared `child_output`, which drains stdout/stderr concurrently while retaining the child hang guard. Added deterministic self-subprocess control `graph::test_support::child_output_drains_both_full_pipes`: **1 passed**, capturing at least 1 MiB from each pipe.
- `a228c6534d6c036ff330b4750d8670b06e9b4909fe471fea1bcc083aed25ee85` — **mid, upheld, fixed**. The supplied slow-predicate standalone reproduction under `--cfg timing_contract` failed before correction (exit101) and passed afterward (exit0). `poll_until` retains the original poll-count allowance in timing profile; only ordinary runs enforce the ten-minute wall-clock guard.
- `ee7a5177db14a1971f2f961f4649a61fc6d887946d2be67b3d64c622c12be5a0` — **mid, upheld, fixed**. Exact ledger-order test passed ordinary profile while its main negative-window invariant was cfg-skipped. The entire scenario is now explicitly optional ordinary-profile coverage, with the required reason/launch-command format, and its assertion is always active when the test is explicitly run. This follows the authorized ignore alternative for a timing-dependent test; no product seam or timing race was added.

Focused review reproductions and the post-high correction checks above are not additional historical-hang stress runs. They validate the reported review findings; historical diagnosis stopped at R4.

## Review 941 findings and corrective acceptance

Subject: `10069d562464e9c43981bbe0f8c1196b68a02baa`, same original base. One further high blocks this round; corrections are submitted to the next built-in round.

- `fceff5ef5d7662e1adfa08a53bf771a22f26710144a74ffcddf5a933da9f8102` — **high, upheld, fixed**. `wait_ready` returns the same false for closed and timeout. The shutdown worker now receives `NO_BOUND`, while its completion is still guarded externally by ten-minute `test_wait::join`. Thus the guard cannot imitate successful shutdown. Focused corrected shutdown test: **1 passed**. Product notify/condvar code was not changed.
- `dd0fd53bbfef97c35f38d9d7d1db3184f162774bf437df2742b7912617b56fc5` — **mid, upheld, fixed**: reset accepted fake HTTP streams to blocking mode before setting I/O timeouts, in both search lifecycle fixtures. macOS is unavailable here; the platform inheritance scenario was traced in code rather than claimed reproduced.
- `b2a73fc82022e09114f4eb4ffe7f4cff532b7899b26ad28b234df80dd8d2f172` — **mid, upheld, fixed**: preserve original 1s/3s timing budgets and 5ms/10ms counted steps in native fixtures instead of replacing all timing contracts by60s. Ordinary runs still use hang_guard. Unix/Linux helpers and imports are cfg-gated to their actual callers, fixing Windows dead-code compilation failures.
- `47c2a46e376034b303eb44452e40bdb23c47ba0b998f38e643d1fd8a3aeaaed4` — **low, upheld, fixed**: `progress::indexing_pass_lifecycle::a_running_check_waits_out_a_concurrent_reader` is explicitly optional, with reason/command. A synchronous mutex wait has no observable admission before release; neither old sleep nor new started-before-call signal guaranteed contention. The requested temporary try_lock mutation was attempted, but its cargo invocation timed out during compilation after240s; the product mutation was completely restored. No mutation pass/fail result is claimed.

CI compile findings: aborted producer JoinError is explicitly discarded after successful bounded cancellation; native test helpers unused on Windows are cfg-gated, not lint-suppressed. `cargo clippy --all-targets --all-features -- -D warnings` passed after corrections (2m32s). No new historical-hang reproduction/stress run is requested. The corrective branch CI and final review outcome are linked in stage handoff.

## Review 944: low follow-up

The corrected slice at `88a875142fb0fd3846e5d33e710ba52f6e4fc77f` closed review944 by lenses: `closed=true`, `nextAction=ready`, `exitCode=0`; subject SHA-256 `563ae4a970f8d2fbd877e63bbb8a9837014d688b4b6c46edebdcdbc2c75fd492`, original base unchanged.

`da9fb2d5620da51fc299de28207d1250c16fad4c63fe4e522b17b149414aef67` — **low, upheld, fixed**. `tasks.rs::yield_until` now uses `std::time::Instant` both for creating and checking its deadline. A standalone control linked to the checkout's Tokio rlib reproduced the clock distinction: a paused current-thread runtime completed100 yield/sleep iterations with nonzero wall elapsed while Tokio Instant remained frozen (exit0). The frozen-clock functional assertions are unchanged; only the watchdog's clock is corrected. This follow-up requires no new review or test rerun under the stage contract.

The same reviewed code passed [corrective CI39679618729](https://github.com/itrous/bsl-analyzer/actions/runs/39679618729), including [unit-a job119498381696](https://github.com/itrous/bsl-analyzer/actions/runs/39679618729/job/119498381696): **8980 passed, 0 failed, 321 ignored, 57 harness results**; all Windows legs and Lint also succeeded. This is corrective acceptance after R4's compilation failures, not another historical-hang diagnostic experiment. The only code change after that accepted/reviewed revision is the low clock follow-up above.

The two optional scenarios above (ledger negative window and unobservable sync-mutex contention) are not blamed for the historical hang. Their opt-in reason and command are in source. All other tests remain enabled. On blocked old rounds939/941, `pio_checks_followup` cannot accept outcomes: it requires a closed check. Their upheld verdicts and fix commits are recorded here; the entire corrected slice is presented to the next round, rather than bypassing its gate.
