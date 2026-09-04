## Context

PR #54 (`75b8a978`) repaired the confirmed takeover races and is already contained by the exact v0.2.77 integration base `upstream/develop` / `edc78e22f3efbfe51ffd8e6dfd05b457976195ca`. Issue #71 remains open because current APIs still allow arbitrary work inside `with_ownership_outcome`, collapse terminal causes, and leave several transient-retry obligations unbounded.

The production audit also found request-time ownership work outside the originally named graph handlers: lexical, semantic, and hybrid search can run fenced prefetch; metadata, diagnostics, and graph status can refresh ownership. Those paths are part of this change. This is a planning-only architecture gate; it does not restore the historical stacked change removed during PR #54 integration.

## Architecture Readiness

Architecture readiness: GO

GO requires all requirement scenarios to have an exact task and planned test mapping, no unresolved product decision, and a clean independent re-review. OpenSpec validation alone is not evidence of readiness or implementation.

## Positive Prerequisites

- Implementation branch is rebased onto exact SHA `edc78e22f3efbfe51ffd8e6dfd05b457976195ca`, which contains v0.2.77 and PR #54 merge `75b8a978`.
- `openspec validate enforce-workspace-lease-operation-profiles --strict --no-interactive` passes before implementation.
- Baseline production inventory covers `workspace_lease`, bootstrap, sync, embedding, overlay retry, graph build/state/snapshot, `bsl-search` transaction helpers, all search modes, and metadata/diagnostics/graph status.
- Existing Linux tests and Windows MCP workflow are the verification surfaces; no clean secondary worktree, published PR, live CI result, or maintainer response is a planning prerequisite.

## Goals / Non-Goals

**Goals:**

- expose unmistakable short-publication, checkpointed-atomic, and lease-free request profiles;
- preserve one five-way outcome from the point of classification to the workflow owner;
- bound fence work, retry lifetime, and shutdown latency independently of workspace size and network latency;
- migrate the complete production caller set, including request search/status and workspace-sized manifest/fingerprint transitions;
- retain PR #54 portability and compatibility.

**Non-Goals:**

- redesign claim generation, token identity, heartbeat files, newest-daemon ownership, or initial unmanaged fallback;
- change cache, SQLite, lease-record, or MCP wire formats;
- guarantee progress through permanent filesystem, SQLite, or network failure;
- add a scheduler, queue, worker thread, dependency, or new operator knob;
- make Rust prove the wall-clock duration of an arbitrary closure.

## Locked Decisions

1. Every MCP request handler is lease-free. It cannot acquire/open `writer.lease.lock`, refresh ownership, open a shared graph descriptor as fallback, or synchronously refill resident search state.
2. Lexical, semantic, and hybrid search only read the current resident overlay on request. If refresh is needed they coalesce a signal into the existing search sink/overlay-retry path; no new worker is introduced and the response does not wait for refresh.
3. Metadata, diagnostics, and graph status render cached atomics/process-local state only. Heartbeat and existing background workflows own any ownership refresh.
4. Expensive preparation happens outside the fence. Inside it, `publish_short` performs only a prepared bounded commit/swap; `publish_checkpointed` owns an indivisible transaction and checks terminal control at the fixed row/chunk boundary.
5. The host boundary uses exactly `LeaseOperationOutcome<T, E> = Applied(T) | OperationError(LeaseOperationError<E>) | TransientRefusal | Superseded | Released`, where `LeaseOperationError<E> = Lease(io::Error) | Operation(E)`. Adapters preserve all five variants and never reconstruct cause with a later flag probe.
6. Busy lock, `UNCLAIMED`, and a lease record temporarily absent during replacement are transient. Malformed records and non-contention open/read/lock/restamp failures are `OperationError`. Unix contention is `EWOULDBLOCK`/`EAGAIN`; Windows contention is sharing/lock violation 32/33; other platform errors are not transient.
7. Once a checkpoint has observed `Superseded`, that cause is latched through rollback even if shutdown is set later. Otherwise release before admission or at the next checkpoint returns `Released`. This first-confirmed-terminal rule removes simultaneous-flag ambiguity.
8. Initial claim acquisition keeps its existing unmanaged fallback and is not reclassified as a managed publication outcome. Every operation after managed handoff fails closed on lease I/O.
9. Every new retry budget is exactly 600 seconds from the first transient refusal. Startup and change-hub admission retry every 2 seconds; graph retry remains trigger-driven; overlay/embedding retain the existing exponential capped delay and the existing `EMBEDDING_PUBLISH_RETRY_BUDGET_SECS` compatibility override. No new knob is added.
10. Coalesced signals neither reset deadline nor backoff streak. Genuinely new external work after an exhausted obligation may create one fresh budget; an expired obligation never restarts itself.
11. Independent search/database mutation batches and fused per-file SQLite ingest use `WORKSPACE_APPLY_BATCH_ROWS = 64`. `GRAPH_BUILD_BATCH = 500` applies only to temporary graph construction outside the fence. Atomic manifest/fingerprint transitions use checkpointed helpers at the existing 64-row boundary.
12. A heartbeat is periodic liveness maintenance, not a retry obligation. A transient heartbeat miss waits for the next normal tick and does not create an inner retry loop.
13. No new dependency, runtime thread, scheduler, persistent schema, lease record, or MCP configuration/wire field is introduced.
14. Integration with v0.2.77 preserves request cancellation and withdrawal, but cancellation plumbing does not regain authority to acquire or inspect `WorkspaceLease`, synchronously prefetch resident state, or wait for refresh; search requests cancel resident reads while refresh remains a coalesced background signal.

## Operation Contract

`publish_short(prepared, commit)` acquires and classifies the lease, performs the fallible restamp, and only then invokes one atomic visibility commit. The commit may return an operation error only when nothing became visible, and no lease I/O occurs after visibility, so an applied publication cannot be reported as failed. `publish_checkpointed(transaction)` supplies the existing cooperative checkpoint; terminal control or restamp error rolls the transaction back before the fence is released and returns the original typed cause.

The raw acquisition primitive and lifecycle mutex remain private to `workspace_lease.rs`. The generic `with_ownership_outcome` and `with_ownership_checkpointed` production surface is removed. Structural enforcement is intentionally small: prepared values cross into `publish_short`, transaction helpers own checkpoints, and a source inventory rejects legacy calls.

`bsl_search::FenceOutcome` is split so `Superseded` and `Released` survive through transaction code. Callback-local Store/SQLite errors remain operation errors. No wildcard compatibility arm is provided; compiler-exhaustive migration is the caller audit.

## Production Caller Inventory and Owned Scope

| Caller class | Required profile / owner |
|---|---|
| heartbeat restamp | one `publish_short`; no inner retry |
| graph prepared rename, descriptor-pool install, snapshot identity install | `publish_short`; open/build outside fence |
| temporary graph build | batches of `GRAPH_BUILD_BATCH = 500`, entirely outside the fence |
| fused per-file graph ingest | `publish_checkpointed`, boundary 64 chunks |
| search drift/full rescan/directory apply | prepared batches via `publish_short`, boundary 64; cursor owned by sync |
| bootstrap search creation and baseline save/clear | prepare outside; checkpointed manifest/fingerprint transaction, boundary 64 |
| roots/context and external-baseline transitions | checkpointed atomic transition, including bulk fingerprint clear |
| structural schema/open and FTS rebuild | checkpointed atomic transaction, boundary 64 |
| single-file search ingest | checkpointed atomic transaction, boundary 64 |
| embedding preflight | typed lease admission before every paid network batch |
| vector/sidecar publication | retain prepared vectors; `publish_short`/checkpointed helper only for local commit |
| overlay Phase C publication | prepared bundle via `publish_checkpointed`, boundary 64; existing `PublishRetryWindow` covers preflight and publication |
| lexical/semantic/hybrid request | resident read plus coalesced signal to existing sink; no lease/open/wait |
| graph data handlers | descriptor-pool checkout only; no fallback open/refill |
| metadata/diagnostics/graph status | cached atomics/process-local state only |
| background graph snapshot | open outside fence; typed short identity install; outcome owned by sync |

## Retry Ownership and Terminal State

| Obligation | Budget / delay | On exhaustion | On non-transient outcome |
|---|---|---|---|
| startup publication | 600 s / 2 s | startup returns classified initialization error | operation error returned; superseded/released stop |
| change-hub enable/admission | 600 s / 2 s | record degraded/failed state and keep the existing sink dormant; the next genuinely new hub batch may start one fresh enable budget | operation error records degradation and one rescan debt; superseded/released stop |
| prepared drift batch | 600 s / existing capped delay | acknowledge batch, advance cursor, create one rescan debt, wait for fresh work | operation error uses same debt contract; terminal stops |
| overlay/embedding publication | existing 600 s default and override / existing exponential cap | semantic runtime leaves `Indexing` for `Failed`; later external kick may restart | operation error fails; terminal stops |
| graph withheld build | 600 s / existing lifecycle triggers | clear withheld obligation and record `Failed`; fresh graph epoch may restart | operation error fails; terminal stops |

`RescanDebt` after an admitted operation error is not a transient lease retry. It remains one coalesced slot with existing capped backoff until current disk state converges, so recovery is not silently discarded at 600 seconds.

## Workflow Invariants

- Long walks, graph/database open, network embedding, sidecar generation, and preparation run outside the fence.
- `save_baseline_manifest`, `clear_baseline_manifest`, and `set_serves_external_baseline(false)` use checkpointed 64-row helpers so release/takeover rolls back the whole visible transition.
- A transient drift or snapshot result retains its prepared batch and original deadline. `Superseded`/`Released` exit. Changed identity, missing path, or open/operation error advances the current cursor and creates one rescan/context debt.
- Background snapshot preparation returns a typed outcome; it is never converted to a string and reparsed/reclassified downstream.
- A durable drift error advances the hub cursor, still sends required graph/project nudges, and collapses failed search work into one current-disk rescan debt.
- Change-hub enable exhaustion does not unsubscribe or terminate the existing sink. It records degradation and stays dormant until a genuinely new hub batch starts one fresh enable obligation; it never depends on request-side refresh.
- A paid embedding result is retained across transient local contention and is never sent to the network twice.
- Graph withheld retry is armed only by the original `TransientRefusal`; later ownership state cannot rewrite provenance.
- Release sets its signal before waiting for the lifecycle lock. Short work finishes one bounded batch; checkpointed work exits at the next boundary and rolls back.

## Verification Strategy

Each scenario in the capability spec has one row in `verification.md` with an exact task and planned test or inventory identifier. Deterministic seams cover clock, contention, missing/malformed records, release, foreign token, restamp error, graph identity, Store failure, and paid-call counts; real `STALE_AFTER` sleeps are forbidden.

Exact filters are wired into Linux `Check` and Windows `MCP transports + secure broker` and must execute at least one test. Local completion requires focused filters, both crate suites, formatting, strict Clippy, `actionlint`, `git diff --check`, source inventories, and strict OpenSpec validation. Publication, live CI, maintainer acceptance, and issue closure occur later and are not implementation-completion gates for this change artifact.

## Audit Matrix

| Driver | Contract | Evidence gate |
|---|---|---|
| Correctness | one owner publishes; all five causes survive adapters | state-machine, adapter, and takeover tests |
| Latency | every request path avoids lease/open/refill | held-lock graph/search/status regressions |
| Shutdown | release waits for one bounded batch or next checkpoint | deterministic release/rollback tests |
| Reliability | fixed retry deadlines; operation-error debt remains durable | fake-clock workflow tests |
| Cost | paid embedding batch is reused after local refusal | scripted embedder call count |
| Portability | lock classification and path identity agree | exact Linux/Windows filters |
| Compatibility | no persistent/wire/dependency/thread change | diff and inventory audits |

## Implementation Order

1. Add exact outcome classification and profile APIs with deterministic lease tests.
2. Split downstream terminal adapters and migrate checkpointed manifest/fingerprint helpers.
3. Migrate short/checkpointed production callers and remove the generic surface.
4. Remove lease/open/refill work from all request paths using existing resident state and background signals.
5. Add workflow-owned deadlines and preserve each recovery obligation.
6. Run scenario filters, crate-wide gates, CI workflow validation, inventory, and strict OpenSpec validation.

## Completion Proof

Architecture is ready only when: the exact base and PR #54 ancestry are recorded; every production caller above has an atomic task; every scenario has a unique verification row; all constants, outcome precedence, retry owners, and terminal states are locked; independent review reports no blocker; and the line in this section is changed to `Architecture readiness: GO`.

Implementation is complete only after all tasks are checked with captured evidence. The current change is planning-only, so every checkbox remains unchecked.

## Risks / Trade-offs

- Request search may serve a slightly older resident overlay while background refresh catches up; this is the deliberate cost of a deterministic lease-free request path.
- A 600-second retry budget may fail during extreme contention; the owning workflow exposes failure and only fresh external work starts another obligation.
- Splitting terminal outcomes touches many exhaustive matches; this is also the smallest reliable way to prevent provenance loss.
- Checkpointed manifest/fingerprint transitions retain atomicity but may delay release by at most one 64-row unit plus rollback.

## Open Questions

None.
