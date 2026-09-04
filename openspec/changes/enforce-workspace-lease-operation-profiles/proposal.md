## Why

Issue #71 remains architecturally open after PR #54: the known races were fixed, but the generic `with_ownership_outcome` contract still accepts short publication, long work, and retrying startup operations without encoding their different latency and failure rules. A future caller can therefore reintroduce self-staleness, block `release()`, or retry forever even though today's audited call sites mostly avoid those failures.

## What Changes

- Replace the generic ownership callback surface with explicit short-publication and checkpointed-atomic operation profiles; keep every graph, search, and status request outside the lease API.
- Return one exhaustive host-level result that distinguishes success, operation failure, transient contention, supersession, and release at the point where each outcome occurs.
- Require workspace-sized, filesystem, network, and descriptor preparation outside the lease lock; retain only bounded commit/swap work or cooperatively cancellable atomic transactions inside it.
- Give every retrying lease consumer an explicit 600-second deadline and bounded delay; new external work may start a new budget, but coalesced signals may not extend the active one.
- Audit every production caller and leave deterministic Linux/Windows regressions for the five invariants in issue #71.
- Preserve the fixes already merged through PR #54 while making misuse of the old contract impossible from production code.

## Capabilities

### New Capabilities

- `workspace-lease-operation-profiles`: Defines operation profiles, typed outcomes, bounded retry, non-blocking request reads, shutdown behavior, and caller-audit requirements for workspace cache ownership.

### Modified Capabilities

None. The current repository contains no promoted OpenSpec capability covering workspace lease operation semantics.

## Impact

- Primary code: `crates/mcp-server/src/workspace_lease.rs`, `state/{bootstrap,embed,sync,overlay_retry}.rs`, `graph/{snapshot,build,state}.rs`, all search handlers, and metadata/diagnostics/graph status paths in `lib.rs`.
- Supporting code where transaction boundaries are owned: `crates/bsl-search/src/{engine,store,workspace_overlay,vector_persist}.rs`.
- Tests and existing Windows MCP CI filters will be extended. Request refresh is routed through the existing search sink; no new runtime dependency, worker, scheduler, cache/SQLite format, lease record format, configuration knob, or MCP wire change is introduced.
- Internal Rust APIs change; there is no public compatibility or data migration.
- Source: https://github.com/itrous/bsl-analyzer/issues/71. PR #54 merge `75b8a978` is contained by the exact v0.2.77 integration base `edc78e22f3efbfe51ffd8e6dfd05b457976195ca`; it is a behavioral baseline, not a dependency branch.
