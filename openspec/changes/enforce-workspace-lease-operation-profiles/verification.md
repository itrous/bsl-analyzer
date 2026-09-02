## Scenario Traceability

Every spec scenario has exactly one row. Identifiers are planned until implementation; an exact test filter that executes zero tests fails the gate.

| # | Requirement / scenario | Task | Exact planned evidence | Platform |
|---:|---|---:|---|---|
| 1 | Lease operations expose explicit profiles / Prepared value is published atomically | 1.3 | `workspace_lease::tests::publish_short_restamp_failure_skips_commit_and_success_commits_once` | Linux, Windows |
| 2 | Lease operations expose explicit profiles / Atomic mutation requires multiple checkpoints | 1.4 | `workspace_lease::tests::checkpointed_atomic_publish_rolls_back_at_boundary` | Linux, Windows |
| 3 | Lease operations expose explicit profiles / Graph data request reads a resident snapshot | 4.1 | `tools::graph::tests::graph_data_requests_are_pool_only_under_held_lease`; `graph_supersession_contract::resolve_names_misses_immediately_when_preopened_handles_are_busy`; `graph_supersession_contract::graph_handler_misses_immediately_when_preopened_handles_are_busy`; `graph_supersession_contract::symbol_info_misses_immediately_when_preopened_handles_are_busy`; `graph_supersession_contract::references_misses_immediately_when_preopened_handles_are_busy` | Linux, Windows |
| 4 | Lease operations expose explicit profiles / Search request reads resident state | 4.2 | `tools::search::tests::all_search_modes_are_resident_only_under_held_lease` | Linux, Windows |
| 5 | Lease operations expose explicit profiles / Status request reads cached state | 4.3 | `tests::all_status_requests_are_cached_under_held_lease` | Linux, Windows |
| 6 | Fenced outcomes preserve their origin / Operation fails after admission | 1.2 | `workspace_lease::tests::callback_error_preserves_operation_origin` | Linux |
| 7 | Fenced outcomes preserve their origin / Lease lock is temporarily unavailable | 1.1 | `workspace_lease::tests::contention_unclaimed_and_missing_are_transient` | Linux, Windows |
| 8 | Fenced outcomes preserve their origin / Managed lease I/O fails | 1.1 | `workspace_lease::tests::managed_lease_io_error_is_not_transient` | Linux, Windows |
| 9 | Fenced outcomes preserve their origin / A live foreign token is observed | 1.4 | `workspace_lease::tests::checkpoint_observes_live_foreign_token_and_rolls_back` | Linux, Windows |
| 10 | Fenced outcomes preserve their origin / Shutdown prevents admission | 1.4 | `workspace_lease::tests::pre_admission_release_skips_callback` | Linux, Windows |
| 11 | Fenced outcomes preserve their origin / Release interrupts checkpointed work | 1.4 | `workspace_lease::tests::checkpointed_release_rolls_back_as_released` | Linux, Windows |
| 12 | Fenced outcomes preserve their origin / Supersession precedes release | 1.4 | `workspace_lease::tests::first_terminal_cause_survives_release` | Linux, Windows |
| 13 | Lock holding and shutdown latency are bounded / Long refresh exceeds the stale interval | 1.4 | `workspace_lease::tests::checkpointed_operation_outlives_stale_after_without_self_supersession` | Linux |
| 14 | Lock holding and shutdown latency are bounded / Release arrives during a bounded batch | 1.3 | `workspace_lease::tests::release_waits_only_for_current_short_publish` | Linux, Windows |
| 15 | Lock holding and shutdown latency are bounded / Release arrives during an indivisible transaction | 1.4 | `workspace_lease::tests::release_signal_precedes_lifecycle_wait` | Linux, Windows |
| 16 | Lock holding and shutdown latency are bounded / Manifest and fingerprint transitions span many rows | 2.5 | `engine::tests::manifest_and_fingerprint_transitions_checkpoint_and_rollback` | Linux, Windows |
| 17 | Every retrying obligation has a fixed deadline / Startup lock remains contended | 3.2 | `state::bootstrap::tests::startup_lease_retry_stops_at_600_seconds` | Linux |
| 18 | Every retrying obligation has a fixed deadline / Fresh work coalesces during retry | 3.1 | `state::retry_window::tests::all_retry_owners_preserve_deadline_and_streak_when_coalescing` | Linux |
| 19 | Every retrying obligation has a fixed deadline / Work arrives after deadline exhaustion | 3.1 | `state::retry_window::tests::eligible_background_owners_rearm_only_on_fresh_external_work` | Linux |
| 20 | Every retrying obligation has a fixed deadline / Permanent operation error occurs | 3.1 | `state::retry_window::tests::all_retry_owners_stop_on_operation_error` | Linux |
| 21 | Every retrying obligation has a fixed deadline / Heartbeat is transiently refused | 2.1 | `workspace_lease::tests::heartbeat_refusal_waits_for_normal_tick` | Linux, Windows |
| 22 | Drift and snapshot failures preserve progress and recovery debt / A second change follows a durable failure | 4.5 | `state::sync::tests::durable_drift_error_advances_cursor_and_coalesces_debt` | Linux, Windows |
| 23 | Drift and snapshot failures preserve progress and recovery debt / Topology marking fails | 4.5 | `state::sync::tests::failed_search_marking_still_sends_topology_nudges` | Linux |
| 24 | Drift and snapshot failures preserve progress and recovery debt / A removed path is recreated before recovery | 4.5 | `state::sync::tests::rescan_debt_uses_current_disk_after_recreate` | Linux, Windows |
| 25 | Drift and snapshot failures preserve progress and recovery debt / Background snapshot preparation has a classified outcome | 4.4 | `graph::snapshot::tests::background_snapshot_preserves_all_typed_outcomes` | Linux, Windows |
| 26 | Local contention does not repeat paid embedding work / Ownership is refused before network work | 4.6 | `state::embed::tests::failed_typed_preflight_makes_zero_network_calls` | Linux |
| 27 | Local contention does not repeat paid embedding work / Publication is refused after vectors are returned | 4.6 | `state::embed::tests::prepared_vectors_survive_transient_publish_without_second_call` | Linux |
| 28 | Local contention does not repeat paid embedding work / Embedding deadline is exhausted | 4.6 | `state::embed::tests::publication_deadline_moves_runtime_to_failed` | Linux |
| 29 | Graph build retry follows the original failure / Ownership returns after transient refusal | 4.7 | `graph::build::tests::original_transient_arms_withheld_build_until_trigger` | Linux |
| 30 | Graph build retry follows the original failure / Build operation fails while ownership probe is negative | 4.7 | `graph::build::tests::operation_error_is_not_reclassified_by_later_probe` | Linux |
| 31 | The complete caller set is regression-protected / Removed generic API remains referenced | 1.6 | `inventory::no_generic_or_unclassified_production_lease_callers` | Linux |
| 32 | The complete caller set is regression-protected / Portable exact filters run in CI | 5.3 | `workspace_lease::tests::contention_unclaimed_and_missing_are_transient`; `workspace_lease::tests::release_signal_precedes_lifecycle_wait`; `graph::snapshot::tests::path_identity_detects_equal_size_and_time_replacement`; `tools::graph::tests::graph_data_requests_are_pool_only_under_held_lease`; `tools::search::tests::all_search_modes_are_resident_only_under_held_lease`; `tests::all_status_requests_are_cached_under_held_lease` | Linux `Check`, Windows `MCP transports + secure broker` |

## Supplemental Inventory Gates

| Gate | Task | Exact planned evidence |
|---|---:|---|
| Heavy work remains outside lease fences | 2.11 | `inventory::prepared_work_stays_outside_lease_fences` |
| Runtime and compatibility surface is unchanged | 5.4 | `inventory::no_new_runtime_or_compatibility_surface` |

## Required Commands

- Run each Rust identifier above with an exact or fully qualified filter and confirm a non-zero test count.
- Run all `inventory::*` identifiers above and fail on any match outside the documented allow-list. The compatibility inventory executes `git diff --exit-code f8bf4da5831840070aa19477be68e74d78014fa6 -- Cargo.lock` and audits the exact added diff for thread/spawn APIs, schema/version DDL, lease-record fields, environment/config reads, and MCP serialization types.
- In Linux `Check` and Windows `MCP transports + secure broker`, run each of the six portable identifiers in row 32 as `cargo test -p mcp-server <identifier> -- --exact` and require a non-zero test count.
- `cargo fmt --all -- --check`
- `cargo clippy -p bsl-search -p mcp-server --all-targets --all-features -- -D warnings`
- `cargo test -p bsl-search --no-fail-fast`
- `cargo test -p mcp-server --no-fail-fast`
- `actionlint .github/workflows/ci.yml`
- `git diff --check`
- `openspec validate enforce-workspace-lease-operation-profiles --strict --no-interactive`

## Post-handoff Delivery Checks

After a later explicit publication request, match the PR head SHA to the locally verified commit and observe the Linux/Windows jobs at that SHA. Maintainer acceptance and issue closure are repository delivery decisions, not evidence that the local implementation itself is complete.

## Current Status

Implementation, local verification, and both independent reviews are complete. Publication
remains out of scope.

## Evidence Collected

- Baseline: `git fetch upstream develop`; both `HEAD` and `upstream/develop` are `f8bf4da5831840070aa19477be68e74d78014fa6`; `git merge-base --is-ancestor 75b8a978 HEAD` passed; initial dirty state contained only this untracked `openspec/` tree.
- Caller inventory: seven production files referenced the legacy ownership APIs and ten files referenced `LeaseOutcome` or `FenceOutcome` before implementation.
- Traceability: an exact requirement/scenario-to-matrix comparison passed with 32 scenarios, 32 unique rows, no duplicate scenario, and every row linked to an existing task.
- Pre-implementation strict validation: `openspec validate enforce-workspace-lease-operation-profiles --strict --no-interactive` passed.
- Pre-implementation code baseline: `workspace_lease` 24 tests, graph snapshot 19, graph build 85, bootstrap 35, sync 52, embed 25, and overlay retry 16 all passed; `cargo fmt --all -- --check` and strict Clippy for `bsl-search` plus `mcp-server` passed with no pre-existing failure.
- Lease core: the final `workspace_lease` suite passed 39/39, including exact five-way classification, callback/lease error provenance, pre-commit restamp with zero commit calls on failure, checkpoint rollback, first-terminal precedence, same-token liveness, real foreign-token takeover, and release latency filters. The existing cross-fence restamp throttle regression also passed after separating forced short restamp from checkpointed throttling.
- Post-review lease correction: checkpointed work now yields and reacquires the OS lock at every
  cooperative boundary, revalidates the real lease record, and retains the reacquired guard through
  the next batch/commit. The deterministic real-record takeover test passed and the final lease
  module suite passed 39/39.
- Outcome adapters: `bsl-search` has no `FenceOutcome::Terminal`; its full suite passed with 403 tests and 29 ignored. `mcp-server --all-features` compiled both production and all test targets after exhaustive host migration to distinct `Superseded` and `Released` variants.
- Legacy surface and heartbeat: the exact `inventory::no_generic_or_unclassified_production_lease_callers` filter executed one passing test and `rg` found no legacy ownership API or `LeaseOutcome` reference under `mcp-server/src`; the exact heartbeat refusal filter passed and proves one transient miss is retried only by the next explicit tick.
- Graph profiles: snapshot, build, and state module suites passed (19, 85, and 28 tests). Exact typed snapshot/path identity and original transient/operation provenance filters each executed one passing test; temporary graph work is off-fence, prepared installs use short publication, and fused ingest uses checkpointed 64-row boundaries.
- Search/store publication: the exact prepared-drift rollback, manifest/fingerprint rollback,
  root migration, FTS rebuild, and fused-file rollback tests passed. Drift advances each cursor
  only after an `Applied` 64-row slice; startup manifest save/clear and external-baseline mode
  changes now use checkpointed transactions.
- Retry ownership: the three exact owner-table tests passed for startup, change-hub, drift,
  overlay/embedding, and graph owners. Startup uses the locked 600-second/2-second policy;
  drift retains prepared work within one deadline and converts durable failure into one
  independently backed-off current-disk rescan debt. A dormant change-hub sink remains visibly in
  `Watcher mode: polling`; operation failure records one local rescan debt, and only a fresh hub
  batch creates the next enable budget.
- Request paths: the exact graph pool-only, all-search resident-only, and all-status cached-only
  tests each executed once and passed. The four busy descriptor-pool tests also passed; status
  uses cached lease/standalone-extension state and search only coalesces a background refresh.
- Embedding: all three paid-work exact tests passed, proving zero network calls after failed
  preflight, one paid call across transient publication refusal, and `Failed` on deadline.
- Exact matrix: every listed scenario, handler, and supplemental-inventory identifier executed at
  least one test and passed after the final integration changes.
- Full gates: `bsl-search` passed 405 tests with 29 ignored; `mcp-server` passed 974 library tests
  with 1 ignored plus 155 integration tests. Strict Clippy, rustfmt check, `git diff --check`,
  `actionlint`, all three inventory gates, and strict non-interactive OpenSpec validation passed.
- Full local CI follow-up: workspace-wide `RUSTFLAGS='-D warnings' cargo clippy
  --all-targets --all-features` and `RUSTFLAGS='-D warnings' cargo test --all --no-fail-fast`
  both exited successfully. All four `partitioned-baseline-scale` workflow commands then passed
  in release mode, executing five 1.6M-entry tests in total. Windows runner behavior remains a
  live-CI gate because it cannot be reproduced on this Linux host.
- Full-suite repair: the first MCP run exposed a broker takeover timing race after request-time
  ownership refresh was removed. The broker now refreshes ownership on its existing background
  tick even while a session is active; the broker suite then passed 8/8 and the complete MCP
  suite passed on rerun.
- Independent reviews: Ponytail reported no blocker and only optional deduplication/deletion ideas.
  The implementation-vs-plan review's checkpoint takeover blocker was fixed with real lock yield,
  reacquisition, record validation, and rollback; its degraded-state observation and real-handler
  evidence gaps were closed with the existing polling status, rescan debt, and four exact busy-pool
  handler tests.
