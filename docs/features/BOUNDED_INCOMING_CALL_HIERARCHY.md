# Bounded incoming call hierarchy index

`callHierarchy/incomingCalls` is served by a compact, process-resident reverse
index. It is intentionally narrower than the workspace graph: it contains only
resolved method-to-method caller pairs for one source root. Direct, qualified,
manager user-method, `NotifyRef`, and idle-handler edges are included when
resolved. With `platform_help = none`, callback constructors cannot be resolved
and `NotifyRef` edges are absent. Separately, the syntax-only compatibility
protection for unused parameters remains available without platform help: it
recognizes `ОписаниеОповещения` and the legacy English `NotifyDescription`, but
not the newer English `CallbackDescription`. This protection does not create
call hierarchy edges.

`SetAction`, module code, unresolved targets, metadata/query/form domains, and
other non-method edges are excluded.

## Lifecycle and response contract

The server does no call-hierarchy indexing at boot and has no idle or eager
warm-up. A successful `callHierarchy/prepare` authorizes one source-root
generation and queues its on-demand build. `callHierarchy/incomingCalls` uses a
Ready index immediately. While that exact prepared generation is Building, one
request may wait for at most two seconds; every other concurrent request returns
`null`. The request also returns `null` for an unprepared, stale, Idle, Failed,
superseded, cancelled, disconnected, or timed-out generation, and when an
indexed target has no live caller range.

There is no `workspace_call_graph` fallback, small-workspace fallback, rebuild,
or outgoing-call behavior change on the incoming path. Graph-serving features
outside incoming call hierarchy remain independent and continue to use their
own graph implementation where they already do so.

## Bounded construction and freshness

The worker freezes the source-root file/path table, disk revisions, overlays,
configuration paths, and generation before it starts. Pass 1 creates a
body-free `GraphIndex` from fresh, bounded batch databases; Pass 2 projects the
compact method pairs and drops each batch database and parser caches before the
next batch. The published index is therefore a fully constructed atomic value,
not an incrementally visible graph.

Body edits during a build are journaled and reprojected against a refreshed
frozen snapshot before publication. Catch-up is bounded to three passes and one
second. A layout change, unknown journal file, frozen-input refresh/projection
failure, exhausted catch-up budget, shutdown, or other structural change
supersedes the generation instead of publishing stale method IDs.

The process gate is below 5 GiB VmHWM for the production benchmark. The compact
index's capacity-based estimated heap budget is below 1 GiB; it measures the
resident reverse index, not a workspace graph or total process memory.

## Observability

The `handle_prepare_call_hierarchy` and `handle_call_hierarchy_incoming` spans
record `source_root`, `generation`, wait timeout/result, and
`workspace_call_graph=false`. Builder spans record `freeze`, `pass1`, `pass2`,
and per-batch progress with method/pair counts and estimated heap. Lifecycle
events record catch-up completion, publication, failures, and supersession
reasons. A successful response creates
`call_hierarchy_incoming_served_from_index`; a null response records its reason.
These fields make an LSP trace sufficient to establish an authorized generation,
bounded build, publication or terminal outcome, and index-only incoming serving.
