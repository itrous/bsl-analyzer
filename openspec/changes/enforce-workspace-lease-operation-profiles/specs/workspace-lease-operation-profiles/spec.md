## ADDED Requirements

### Requirement: Lease operations expose explicit profiles

The system MUST expose separate internal contracts for short publication and checkpointed atomic mutation, and MUST keep every request-time read outside both contracts. Generic ownership callback entry points MUST be private to the lease implementation and unused by production callers.

#### Scenario: Prepared value is published atomically

- **WHEN** workspace-sized preparation has produced a rename, swap, descriptor pool, or bounded mutation batch
- **THEN** `publish_short` completes fallible lease checks and restamp before one atomic visibility commit, with no later lease I/O
- **AND** filesystem walks, network calls, descriptor opening, and workspace-sized computation remain outside the fence
- **AND** a commit error is possible only when the prepared value did not become visible

#### Scenario: Atomic mutation requires multiple checkpoints

- **WHEN** a shared SQLite transaction must not expose partial results and crosses a row or chunk boundary
- **THEN** it uses `publish_checkpointed`
- **AND** terminal control at a checkpoint rolls back the transaction before releasing the fence

#### Scenario: Graph data request reads a resident snapshot

- **WHEN** `resolve_names`, `graph`, or `symbol_info` requests graph data while the pool is empty and the lease lock is held elsewhere
- **THEN** the handler uses only an already-open local descriptor if available and returns promptly otherwise
- **AND** it performs no lease access, shared-path open, wait, or fallback refill

#### Scenario: Search request reads resident state

- **WHEN** lexical, semantic, or hybrid search observes stale or absent resident overlay state while the lease lock is held elsewhere
- **THEN** it reads the current resident state and may coalesce one signal into the existing search sink
- **AND** it performs no fenced prefetch, shared-path open, interprocess wait, or synchronous refill

#### Scenario: Status request reads cached state

- **WHEN** metadata status, diagnostics status, or graph status is rendered while the lease lock is held elsewhere
- **THEN** it reads cached atomics or process-local state and returns promptly
- **AND** it does not call an ownership-refreshing path such as `owns_caches()` or `may_build()`

### Requirement: Fenced outcomes preserve their origin

Every managed fenced operation MUST return exactly one exhaustive host outcome: `Applied`, `OperationError`, `TransientRefusal`, `Superseded`, or `Released`. Classification MUST occur at the fence or callback boundary and MUST NOT depend on a later mutable-state probe.

#### Scenario: Operation fails after admission

- **WHEN** an admitted Store, SQLite, filesystem, mutex, or prepared-plan operation fails
- **THEN** the caller receives `OperationError` with the original operation cause
- **AND** the failure is not retried as lease contention

#### Scenario: Lease lock is temporarily unavailable

- **WHEN** the callback cannot begin because the platform reports lock contention, the record is `UNCLAIMED`, or the record is temporarily absent during replacement
- **THEN** the caller receives `TransientRefusal`
- **AND** the callback does not execute

#### Scenario: Managed lease I/O fails

- **WHEN** managed admission or checkpoint sees malformed content or a non-contention open, read, lock, or restamp failure
- **THEN** the caller receives `OperationError` with the original lease I/O cause
- **AND** the error is not downgraded to contention, unmanaged success, supersession, or release

#### Scenario: A live foreign token is observed

- **WHEN** the fence observes another live owner after this lease previously owned the workspace
- **THEN** the caller receives `Superseded`
- **AND** supersession remains terminal for later publication attempts

#### Scenario: Shutdown prevents admission

- **WHEN** release is set before callback admission
- **THEN** the caller receives `Released` without executing the callback
- **AND** no reconnect guidance is attributed to a foreign owner

#### Scenario: Release interrupts checkpointed work

- **WHEN** release is set while a checkpointed transaction is active and no supersession was previously confirmed
- **THEN** the next checkpoint rolls back and returns `Released`
- **AND** the result is not represented as `Applied`, `OperationError`, or `Superseded`

#### Scenario: Supersession precedes release

- **WHEN** a checkpoint confirms a foreign live token and release is set before rollback completes
- **THEN** the latched result remains `Superseded`
- **AND** no later flag probe changes it to `Released`

### Requirement: Lock holding and shutdown latency are bounded

Workspace-sized or network work MUST NOT run inside a lease fence. Independently visible mutations MUST use bounded batches, and indivisible transactions MUST check liveness and terminal control at a fixed boundary. `release()` MUST signal before waiting for the lifecycle lock.

#### Scenario: Long refresh exceeds the stale interval

- **WHEN** topology refresh or rescan lasts longer than `STALE_AFTER`
- **THEN** preparation stays outside the fence or publication crosses bounded fences/checkpoints
- **AND** a same-token checkpoint refreshes liveness without self-supersession while foreign-token control still terminates it

#### Scenario: Release arrives during a bounded batch

- **WHEN** one short batch is admitted and another thread calls `release()`
- **THEN** release waits at most for that bounded batch plus the existing lock acquisition bound
- **AND** the next batch is rejected as `Released`

#### Scenario: Release arrives during an indivisible transaction

- **WHEN** a checkpointed transaction is active and another thread calls `release()`
- **THEN** the release signal becomes visible before lifecycle-lock acquisition
- **AND** shutdown waits only for the next checkpoint and rollback

#### Scenario: Manifest and fingerprint transitions span many rows

- **WHEN** baseline manifest save/clear or external-baseline fingerprint clear processes more than 64 rows
- **THEN** its indivisible transaction checkpoints every 64 rows
- **AND** terminal control rolls back all visible rows rather than leaving partial state

### Requirement: Every retrying obligation has a fixed deadline

Every workflow that retries `TransientRefusal` MUST own a monotonic 600-second deadline and bounded delay. The deadline MUST begin with the first refusal, MUST NOT be extended by coalesced signals, and MUST produce the specified observable terminal state. Request paths MUST NOT retry.

#### Scenario: Startup lock remains contended

- **WHEN** startup publication receives only `TransientRefusal` for 600 seconds with 2-second retry cadence
- **THEN** startup returns a classified initialization error
- **AND** it does not remain in a silent infinite loop

#### Scenario: Fresh work coalesces during retry

- **WHEN** new file or retry signals arrive while startup, change-hub/drift, overlay/embedding, or graph retry is waiting
- **THEN** they join that obligation without resetting its deadline or backoff streak
- **AND** memory remains bounded to the workflow's existing coalesced slot

#### Scenario: Work arrives after deadline exhaustion

- **WHEN** an eligible change-hub/drift, overlay/embedding, or graph obligation has reached its specified failed state and genuinely new external work arrives later
- **THEN** exactly one new obligation receives a fresh 600-second budget
- **AND** the expired obligation does not restart itself, while the existing change-hub sink remains dormant to observe that new batch

#### Scenario: Permanent operation error occurs

- **WHEN** an admitted operation returns `OperationError`
- **THEN** the transient retry loop stops immediately
- **AND** recovery follows the owning workflow's explicit debt or failed-state contract

#### Scenario: Heartbeat is transiently refused

- **WHEN** one periodic heartbeat receives `TransientRefusal`
- **THEN** it waits for the next normal heartbeat tick
- **AND** it creates no inner retry obligation or deadline

### Requirement: Drift and snapshot failures preserve progress and recovery debt

A durable search drift or background snapshot operation error MUST NOT pin the change-hub cursor or suppress graph nudges. Failed search work MUST collapse into one rescan obligation that converges from current disk state. Transient prepared work MUST preserve its current batch and original deadline.

#### Scenario: A second change follows a durable failure

- **WHEN** the first drift batch returns `OperationError` and a second file change arrives
- **THEN** the first batch is acknowledged, the cursor advances, and the second change can be materialized
- **AND** both are covered by at most one rescan debt slot

#### Scenario: Topology marking fails

- **WHEN** search marking fails for a change requiring graph rebuild or project reload
- **THEN** required graph nudges still occur
- **AND** the search recovery obligation retains its existing capped backoff

#### Scenario: A removed path is recreated before recovery

- **WHEN** a failed batch observed removal and the path exists again before recovery rescan
- **THEN** recovery derives the result from current disk state
- **AND** it does not apply the stale removal over the recreated file

#### Scenario: Background snapshot preparation has a classified outcome

- **WHEN** snapshot preparation or identity install is transient, superseded, released, missing, changed, or fails to open
- **THEN** a transient result retains the batch and deadline; superseded/released exit; missing/changed/open error advances the cursor and creates one rescan/context debt
- **AND** the original typed cause is never converted to a string and reclassified downstream

### Requirement: Local contention does not repeat paid embedding work

Workspace embedding MUST check typed ownership before every network batch and MUST retain a prepared paid batch across transient local publication refusal. One deadline MUST cover preflight and publication.

#### Scenario: Ownership is refused before network work

- **WHEN** typed preflight returns `TransientRefusal`, `Superseded`, or `Released`
- **THEN** the network embedder is not called
- **AND** only `TransientRefusal` may enter the bounded retry policy

#### Scenario: Publication is refused after vectors are returned

- **WHEN** a network batch succeeds but local SQLite or sidecar publication receives `TransientRefusal`
- **THEN** the same prepared batch is retried without another network call
- **AND** permanent Store, lease I/O, or network failure becomes `OperationError`

#### Scenario: Embedding deadline is exhausted

- **WHEN** transient preflight or local publication remains unavailable through the obligation deadline
- **THEN** semantic runtime leaves `Indexing` and reports `Failed`
- **AND** only later external work may create a new budget

### Requirement: Graph build retry follows the original failure

Graph publication MUST retain the failure classification produced at its source. Only an original `TransientRefusal` may arm a withheld rebuild; later ownership probes MUST NOT reclassify another result.

#### Scenario: Ownership returns after transient refusal

- **WHEN** publication returned `TransientRefusal` but ownership is available again before failure recording
- **THEN** the withheld-build obligation remains armed from the original result
- **AND** the next lifecycle trigger retries within the original deadline

#### Scenario: Build operation fails while ownership probe is negative

- **WHEN** a genuine build, open, identity, or rename error occurs and a later lease probe would be negative
- **THEN** the original `OperationError` remains unchanged
- **AND** no withheld build is armed from the later probe

### Requirement: The complete caller set is regression-protected

The change MUST migrate every production lease operation enumerated in `design.md`. Linux and Windows verification MUST use exact portable filters that each execute at least one test.

#### Scenario: Removed generic API remains referenced

- **WHEN** validation scans production Rust sources after migration
- **THEN** no caller outside `workspace_lease.rs` references a removed generic ownership API
- **AND** any new unclassified caller fails the inventory gate

#### Scenario: Portable exact filters run in CI

- **WHEN** Linux and the existing Windows MCP job execute the issue #71 regression filters
- **THEN** every configured filter reports at least one executed test
- **AND** lock classification, release, path replacement, and request lease-freedom pass on both platforms
