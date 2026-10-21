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

Keep required tests enabled with a ten-minute hang guard, rather than ignore a speculative candidate. No product behaviour changes. Normal success remains defined by the awaited state, not CPU speed or a shorter timing deadline. A watchdog failure means the state did not arrive; it does not assert performance. No shortcuts or new external dependencies. The existing workspace `test-utils` crate is added as a dev-dependency to `bsl-check-executor`, `bsl-launcher`, and `intern`, so their tests reuse the prescribed guard rather than duplicate it.

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
| R4 | [CI workflow_dispatch 39674927214](https://github.com/itrous/bsl-analyzer/actions/runs/39674927214), branch `pio/fix-unit-a-hang`, code commit `d09345c40db10b69f99bbb936ac1e28bfaffccb2` | Final permitted diagnostic execution; its completed outcome is checked before stage handoff and reported there. |

R1/R2 also exposed a needless ten-minute delay in `graph::state::tests::a_wait_that_times_out_reports_the_state_it_observed`: an intentional zero-budget negative control was re-wrapped in `hang_guard` by the shared polling helper. `graph/test_support.rs::wait_until_within` now honours its explicit, already-guarded caller budget directly; normal state callers pass `hang_guard`, and the deterministic negative control can keep zero. The test remains required, not ignored. Release-on-channel-close semantics are also retained for the diagnostics drain fixture.

New-finding outcomes: missing imports/local constant **fixed**; test-module census **fixed** (`inventory.rs`); cancellation release semantics/mutex poisoning **fixed** (`lib.rs::search_cancellation_matrix::hold_lock`); negative-control double guard **fixed** (`graph/test_support.rs`); uncontrolled ordinary state waits **contained with guards**. No test has been speculatively ignored. Historical root cause remains unestablished. Compilation-only preparation failures and positive controls are explicitly separate from these four executions.
