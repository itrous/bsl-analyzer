//! Agent-facing call-graph tool actions over the on-disk [`GraphDb`].
//!
//! These run on a blocking task against a read-only SQLite handle. Domain errors
//! (not found, malformed id) are returned in-band as structured JSON so the agent
//! can react, rather than as transport errors; an infrastructure error (e.g. a
//! failed SQL read) surfaces as an `internal` error object. Each result is wrapped
//! in a freshness [`envelope`] so the agent knows the revision the answer was
//! computed at and whether the workspace has drifted on disk since.

use ide::{Direction, GraphDetail, NeighborsParams};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{json, Value};

use crate::graph::Freshness;
use crate::graph_query::GraphDb;
// Aliased: the graph has a `Freshness` of its own, and the contract's is a different type.
use crate::tools::location as loc;
use crate::tools::redact::redact_secrets;
use crate::tools::response::structured;

/// Parse the `detail` enum. `None` keeps the default (`signatures`); an unknown value is
/// rejected (mirrors `diagnostics::parse_min_severity`) so a caller is never silently served
/// a different view than it asked for.
pub fn detail_from(s: Option<&str>) -> Result<GraphDetail, String> {
    match s {
        None | Some("signatures") => Ok(GraphDetail::Signatures),
        Some("names") => Ok(GraphDetail::Names),
        Some("bodies") => Ok(GraphDetail::Bodies),
        Some(other) => Err(format!("unknown detail '{other}'; expected names|signatures|bodies")),
    }
}

/// Parse the `dir` enum. `None` keeps the default (`in`); an unknown value is rejected so a
/// caller is never silently given the opposite traversal direction.
pub fn direction_from(s: Option<&str>) -> Result<Direction, String> {
    match s {
        None | Some("in") => Ok(Direction::In),
        Some("out") => Ok(Direction::Out),
        Some("both") => Ok(Direction::Both),
        Some(other) => Err(format!("unknown dir '{other}'; expected in|out|both")),
    }
}

/// The agent-facing edge-kind labels accepted by the `edge_kinds` neighbour filter.
const EDGE_KINDS: [&str; 14] = [
    "call",
    "manager_creates",
    "manager_access",
    "query_ref",
    "contains",
    "data_binding",
    "notify_ref",
    "idle_handler",
    "event_subscription",
    "register_movement",
    "subsystem_membership",
    "role_reference",
    "register_records",
    "register_record_set",
];

/// Validate an `edge_kinds` filter: every entry must be a known edge-kind label, so a
/// typo fails fast rather than silently matching nothing.
pub fn validate_edge_kinds(kinds: &[String]) -> Result<(), String> {
    for k in kinds {
        if !EDGE_KINDS.contains(&k.as_str()) {
            return Err(format!(
                "unknown edge_kind '{k}'; expected one of {}",
                EDGE_KINDS.join("|")
            ));
        }
    }
    Ok(())
}

/// Default and ceiling for `max_call_sites`. The default is a working number of places for
/// one edge; the ceiling exists because the places of one edge all live in one file, and a
/// caller asking for thousands is asking to be served that file a line at a time.
pub const DEFAULT_CALL_SITE_CAP: usize = 20;
pub const MAX_CALL_SITE_CAP: usize = 200;

/// Parse the per-edge place cap. Zero is refused rather than taken as "no places": a caller
/// who asks for call sites and gets an empty list under a silent cap reads it as an edge with
/// no place, which is a different answer.
pub fn call_site_cap(requested: Option<usize>) -> Result<usize, String> {
    match requested {
        None => Ok(DEFAULT_CALL_SITE_CAP),
        Some(0) => {
            Err("max_call_sites must be at least 1; omit `call_sites` to ask for none".to_string())
        }
        Some(n) if n > MAX_CALL_SITE_CAP => {
            Err(format!("max_call_sites {n} exceeds the ceiling of {MAX_CALL_SITE_CAP}"))
        }
        Some(n) => Ok(n),
    }
}

/// An infrastructure failure (e.g. a SQL read error) surfaced in-band so the agent
/// sees a structured error rather than a dropped tool call.
fn internal(e: anyhow::Error) -> Value {
    json!({ "error": "internal", "detail": e.to_string() })
}

pub fn overview(
    graph: &GraphDb,
    top: usize,
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> (Value, loc::Completeness) {
    match graph.overview(top, roots) {
        Ok(overview) => (to_value(&overview), loc::Completeness::complete()),
        Err(e) => (internal(e), read_failure()),
    }
}

/// Default cap on the candidate ids returned by `resolve`.
pub const DEFAULT_RESOLVE_LIMIT: usize = 20;

/// `resolve` answers from the name dictionary, not from the graph alone.
///
/// The graph is one provider among five here, which is why the action is served
/// before the readiness gate: the platform answers without any index at all, and
/// the resident's tables answer without the graph. What could NOT be consulted
/// is named in `providers` rather than left to look like an absence of matches.
pub fn resolve(
    db: &ide::RootDatabaseImpl,
    workspace: ide::ProviderState,
    graph: &dyn ide::ExternalNameSource,
    roots: Option<&bsl_search::WorkspaceRoots>,
    query: &str,
    limit: usize,
) -> (Value, loc::Completeness) {
    let name_query =
        ide::NameQuery { text: query, limit, categories: None, require_location: false, workspace };
    let found = ide::lookup_names(db, &name_query, &[graph]);

    let mut body = serde_json::Map::new();
    body.insert("query".into(), json!(query));
    let completeness =
        crate::tools::name_answer::NameAnswer::render(db, roots, &found).insert_into(&mut body);
    (Value::Object(body), completeness)
}

/// Default body-output budget (in tokens, ~4 chars each) for `node`/`neighbors` at
/// `detail=bodies`, so a `bodies` request can never return an unbounded payload the way an
/// uncapped manager-method body could. Overridable via `max_output_tokens`.
pub const DEFAULT_BODY_BUDGET_TOKENS: usize = 6000;

/// Truncate `source` to the `remaining` char budget on a char boundary, decrementing the
/// budget. Returns `true` if it had to truncate (or drop) the body. `None`/empty sources
/// (the non-`bodies` details) consume nothing.
fn clamp_to_budget(source: &mut Option<String>, remaining: &mut usize) -> bool {
    let Some(text) = source else { return false };
    if text.len() <= *remaining {
        *remaining -= text.len();
        return false;
    }
    let mut end = *remaining;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        // No budget left for even a partial body — drop the field entirely rather than emit an
        // empty string that reads like a method with no body. The response's `budget_exhausted`
        // flag signals why the source is absent.
        *source = None;
    } else {
        text.truncate(end);
    }
    *remaining = 0;
    true
}

pub fn node(
    graph: &GraphDb,
    id: &str,
    detail: GraphDetail,
    max_output_tokens: usize,
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> (Value, loc::Completeness) {
    match graph.node(id, detail, roots) {
        Ok(Ok(mut result)) => {
            redact_opt(&mut result.node.source);
            let mut remaining = max_output_tokens.saturating_mul(4);
            let truncated = clamp_to_budget(&mut result.node.source, &mut remaining);
            result.node.truncated = truncated;
            let mut value = to_value(&result);
            if truncated {
                value["budget_exhausted"] = json!(true);
            }
            (value, budget_completeness(truncated))
        }
        Ok(Err(err)) => (to_value(&err), loc::Completeness::complete()),
        Err(e) => (internal(e), read_failure()),
    }
}

/// The graph could not be read at all. The body says so, and the envelope must not
/// contradict it: `complete` is the field a consumer trusts to decide whether the answer can
/// be relied on, and on a failure it would say yes.
fn read_failure() -> loc::Completeness {
    loc::Completeness::partial(
        loc::ReasonCode::ModalityDegraded,
        "the graph database could not be read",
    )
}

/// The one reason a body-carrying action can be partial on its own.
fn budget_completeness(truncated: bool) -> loc::Completeness {
    loc::Completeness::complete().when(
        truncated,
        loc::ReasonCode::OutputBudget,
        "bodies trimmed to fit max_output_tokens",
    )
}

/// Spend what the bodies left on the places, edge by edge in served order.
///
/// Returns whether the budget actually dropped a place — counted from what was kept, not from
/// the fact that the pass ran. A pass that reports "truncated" for a list it did not shorten
/// tells a consumer it lost data it still has, and that report is exactly as unfalsifiable as
/// the loss it claims.
///
/// `call_sites_total` is left alone: it counts what the artefact records, before anything is
/// shown, so a shortened list stays comparable with the whole.
fn trim_call_sites_to_budget(edges: &mut [ide::EdgeRef], remaining: &mut usize) -> bool {
    let mut cut = false;
    for edge in edges {
        let Some(places) = edge.call_sites.as_mut() else { continue };
        let mut kept = 0;
        for place in places.iter() {
            let cost = serde_json::to_string(place).map(|json| json.len()).unwrap_or(0);
            if cost > *remaining {
                break;
            }
            *remaining -= cost;
            kept += 1;
        }
        if kept < places.len() {
            places.truncate(kept);
            edge.call_sites_truncated = true;
            cut = true;
        }
    }
    cut
}

pub fn neighbors(
    graph: &GraphDb,
    params: &NeighborsParams<'_>,
    max_output_tokens: usize,
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> (Value, loc::Completeness) {
    match graph.neighbors(params, roots) {
        Ok(Ok(mut result)) => {
            redact_opt(&mut result.root.source);
            for node in &mut result.nodes {
                redact_opt(&mut node.source);
            }
            // Cumulative body budget across the root then the (centrality-ordered) nodes,
            // so a `detail=bodies` traversal stays within the output budget.
            let mut remaining = max_output_tokens.saturating_mul(4);
            let root_truncated = clamp_to_budget(&mut result.root.source, &mut remaining);
            result.root.truncated = root_truncated;
            let mut truncated = root_truncated;
            for node in &mut result.nodes {
                let node_truncated = clamp_to_budget(&mut node.source, &mut remaining);
                node.truncated = node_truncated;
                truncated |= node_truncated;
            }
            // The per-edge cap has already cut what it was going to cut; read that before the
            // budget pass, which sets the same flag for a different reason.
            let capped_sites = result.edges.iter().any(|edge| edge.call_sites_truncated);
            let budget_cut_sites = trim_call_sites_to_budget(&mut result.edges, &mut remaining);

            let mut value = to_value(&result);
            if truncated {
                value["budget_exhausted"] = json!(true);
            }
            // A centrality-ranked tail drop is a CAP, not a budget cut: raising
            // `max_output_tokens` alone does not bring the dropped neighbours back. The same
            // split applies to the places of an edge, and for the same reason the two are
            // named apart.
            let completeness = budget_completeness(truncated)
                .when(
                    result.dropped_count > 0,
                    loc::ReasonCode::ResultCap,
                    "neighbours dropped by max_nodes; raise it or narrow the direction",
                )
                .when(
                    capped_sites,
                    loc::ReasonCode::ResultCap,
                    "call sites dropped by max_call_sites; raise it",
                )
                .when(
                    budget_cut_sites,
                    loc::ReasonCode::OutputBudget,
                    "call sites trimmed to fit max_output_tokens",
                );
            (value, completeness)
        }
        Ok(Err(err)) => (to_value(&err), loc::Completeness::complete()),
        Err(e) => (internal(e), read_failure()),
    }
}

pub fn source(
    graph: &GraphDb,
    ids: &[String],
    max_output_tokens: usize,
) -> (Value, loc::Completeness) {
    match graph.source(ids, max_output_tokens) {
        Ok(mut result) => {
            for item in &mut result.items {
                redact_opt(&mut item.source);
            }
            // `source` budgets INSIDE the query, so the fact is on the result rather than
            // on a local: skipping it here would report "complete" over a clipped body.
            let completeness = budget_completeness(result.budget_exhausted);
            (to_value(&result), completeness)
        }
        Err(e) => (internal(e), read_failure()),
    }
}

/// Wrap an action result in the freshness envelope. `revision` is the snapshot
/// generation the answer was computed at; `stale` flags on-disk drift since then;
/// `reload` is the background-reindex state (`none` / `running` / `failed`).
pub fn envelope(
    freshness: Freshness,
    completeness: loc::Completeness,
    result: Value,
) -> CallToolResult {
    structured(json!({
        "revision": freshness.revision,
        "stale": freshness.stale,
        "reload": freshness.reload,
        "freshness": loc::Freshness::new(loc::FreshnessSource::Graph, completeness)
            .with_revision(freshness.revision)
            .with_topology(freshness.topology)
            .with_stale(freshness.stale)
            .to_value(),
        "result": result,
    }))
}

/// Static graph schema for cold-start discovery. Wraps [`schema_json`] in a tool
/// result; the JSON is split out so a test can pin the advertised contract.
pub fn schema() -> CallToolResult {
    structured(schema_json())
}

/// The `status` action: the graph's lifecycle snapshot (mirrors `diagnostics status`), so an
/// agent can start the lazy build and poll its progress instead of reading a flat `loading`
/// envelope from a data action.
pub fn status(report: &crate::graph::GraphStatusReport) -> CallToolResult {
    let mut body = json!({ "state": report.state });
    if let Some(files) = report.files {
        body["files"] = json!(files);
    }
    // Assembled field by field, so a new one must be added here or the report is
    // right internally while nothing reaches the agent.
    if let Some(unread) = report.unread_files {
        body["unread_files"] = json!(unread);
    }
    if let Some(revision) = report.revision {
        body["revision"] = json!(revision);
    }
    if let Some(stale) = report.stale {
        body["stale"] = json!(stale);
    }
    if let Some(reload) = report.reload {
        body["reload"] = json!(reload);
    }
    if let Some(error) = &report.error {
        body["error"] = json!(error);
    }
    if let Some(superseded) = report.superseded {
        body["superseded"] = json!(superseded);
    }
    structured(body)
}

/// A transient "still indexing" result, emitted while the background load runs.
/// Not an error — the agent should retry shortly.
pub fn loading(detail: Option<&str>) -> CallToolResult {
    let mut body = json!({ "status": "loading" });
    if let Some(detail) = detail {
        body["detail"] = json!(detail);
    }
    structured(body)
}

/// The agent-facing graph contract: action names, node/edge vocabularies, the
/// neighbours-result and envelope shapes, and the durable id grammar. `schema_version`
/// is the contract version — bump it whenever this response shape changes (it is
/// independent of the on-disk SQLite cache layout in [`crate::graph_db`]).
fn schema_json() -> Value {
    json!({
        "schema_version": "33",
        "actions": ["overview", "schema", "status", "node", "source", "neighbors", "callers", "callees", "resolve"],
        "status": "`status` returns the graph lifecycle ({state: disabled|loading|ready|failed, and when ready: files, unread_files, revision, stale, reload}) and kicks the lazy build — poll it instead of reading a flat `loading` envelope from a data action (mirrors `diagnostics status`). `unread_files` counts modules whose bytes could not be read when the artefact was built or last patched: they contributed no nodes and no edges, so the graph is missing them, `stale` is true, and no fingerprint comparison reveals it (stat needs no read permission). A patch never clears an inherited one — only a full rebuild restores the missing rows. `files` here is the module count the artefact COVERS, unread ones included, so `unread_files` is a subset of it — unlike `diagnostics status`, whose `files` counts only what it serves and excludes them; do not apply one arithmetic to both. `superseded: true` (emitted only when it holds) means another daemon generation owns this workspace's derived caches and this server no longer rebuilds. It reports `ready + superseded` and serves data only while one of its own pre-opened SQLite descriptors is available in the pool; otherwise status is `failed + superseded` and every data action returns the same reconnect error, never `loading`. Reconnect to use the current daemon. The static `schema` action remains available without a snapshot.",
        "node_kinds": ["method", "module", "mdo", "attribute", "tabular_section", "form", "form_item", "form_attribute"],
        "node_shape": "`qualified` (russified display path) is emitted only for metadata nodes — for code nodes it would restate `module` + `name`; `addressable` is emitted only when false (absent = the id round-trips); `truncated: true` is emitted on a node whose `detail=bodies` source was cut short — or, when `source` is absent, dropped — to fit the output budget (so a short body is not mistaken for a complete one, nor a budget-dropped body for a method with no body)",
        "notes": "`node(module/<scope>)` resolves for any code module and returns a `methods` array ({id, name, is_export}) of the module's members; module membership is served on demand and is not a graph edge, so `neighbors(module/…)` stays empty",
        "resolve": "`resolve(query)` is the name dictionary, not a graph traversal: it answers from five sources (module_index, metadata_listing, platform, module_members, graph) and therefore WORKS BEFORE THE GRAPH IS BUILT — a platform member needs no index at all, a metadata object and a common module only the resident. Use it for an imprecise query (wrong casing, a bare method/object name, a partial id) and to recover from a `not_found` in `node`/`neighbors`. Each candidate is {display, category, match, provider, address} plus `id` (kept for compatibility, present only when it equals `address.graph_id`). Every key in `address` is accepted by the tool it names — `symbol` by `symbol_info`, `graph_id` by `node`, `syntax_help` by the `syntax_help` tool, `location` as a (root_id, path) pair by `outline`/`diagnostics file`; a key that would be refused is not published at all, and there is always at least one. `category` is common_module|module|module_method|module_variable|metadata_object|metadata_member|form|platform_member; `match` is exact|case_insensitive|name|prefix|substring (strongest first; `name` is equality with a durable id's trailing segment, so it outranks a mere prefix). One entity arrives as ONE candidate even when several sources know it: a graph node and the resident's record of the same thing carry `graph_id` and `location` in the same `address`. `providers[]` names every source and its state (answered|not_ready|not_asked|unavailable|failed), so an empty list is a proven zero ONLY when every source answered — otherwise the envelope carries `index_building`. `total` counts matches before the cap; `total_exact: false` means a source hit its own cap and the count is an estimate. `truncated: true` means the cap dropped candidates — refine the query, raise `top`, or use `search_code`. It is symbol/name-oriented, NOT a natural-language search: a free-text phrase returns no candidates — use `search_code` for semantic lookup, then pass the emitted `graph_id` here.",
        "edge_kinds": ["call", "manager_creates", "manager_access", "query_ref", "register_movement", "register_records", "register_record_set", "contains", "data_binding", "notify_ref", "idle_handler", "event_subscription", "subsystem_membership", "role_reference"],
        "edge_kinds_note": "All edge kinds below are kept separate from `call`, so `edge_kinds=[call]` is a pure 'who really calls whom'. String-dispatched callbacks (provenance `string_resolved`, resolved conservatively — only ЭтотОбъект/ЭтаФорма and explicit common-module handlers resolve, an unresolved receiver/handler produces no edge): `notify_ref` (Новый ОписаниеОповещения) links the registering method/module body to BOTH the success handler (ИмяПроцедуры, Модуль) and the error handler (ИмяПроцедурыОбработкиОшибки, МодульОбработкиОшибки) named by string literals; `idle_handler` (ПодключитьОбработчикОжидания) links to the named handler, resolved in the current module or, failing that, in a UNIQUE global common module exporting it (an ambiguous name exported by several global modules is left unresolved, not guessed); `event_subscription` links a `ПодпискаНаСобытие` `mdo` node to its exported handler method. The reference is modelled regardless of the target's client/server dispatch (validity is a diagnostics concern; the reference matters for rename impact either way); a handler hosted in the application module (`МодульПриложения`) is a known unmodelled case. Metadata-reference edges: `register_movement` links a document method/module that writes or reads register records via its `Движения` collection — `Движения.<Регистр>.<метод>()` (bare or through a receiver) or a locally-literal dynamic index `Движения[<строка>]` / `Движения[Метаданные.<РегистрыКоллекция>.<X>.Имя]` — to the register's `mdo` node, type resolved from configuration (provenance `inferred`); a variable index (`Движения[ИмяРегистра]`) needs value flow and is not yet modelled; `subsystem_membership` links a subsystem `mdo` node (type `Subsystem`) to each metadata object it contains and each child subsystem, from its `Content`/`ChildObjects` (provenance `resolved`); `role_reference` links a role `mdo` node (type `Role`) to each object it grants rights on (`resolved`) plus each object named only inside an RLS `restrictionByCondition` query (`inferred`, parsed from the restriction text), while top-level reusable `restrictionTemplate` conditions are not parsed (no host object to resolve against) — a known recall gap. `register_records` links a document `mdo` node (type `Document`) to each register it declares it posts, from the document's `RegisterRecords` metadata (provenance `resolved`) — the declared post contract, sound even when the posting code addresses the register dynamically (a string name into `РегистрыНакопления[…]` or a `Движения[…]` index) which the code-level `register_movement` cannot see; it is the declared capability, not a guarantee every post writes every register. `register_record_set` links a method/module that reaches a register's record-set engine through a literal manager creator (`РегистрыНакопления.<X>.СоздатьНаборЗаписей()` / `СоздатьМенеджерЗаписи()`) to the register's `mdo` node (provenance `inferred`) — register write-capable access (a record set can also be read), the code-level complement that catches non-document writers (typically common modules) which `register_records` (documents only) and `register_movement` (a registrator's `Движения`) miss. `neighbors(mdo/<Type>/<Object>, dir=in, edge_kinds=[subsystem_membership])` / `[role_reference]` / `[register_records]` / `[register_record_set]` answer 'which subsystems contain' / 'which roles grant rights on or restrict' / 'which documents post' / 'which code touches (read or write) via its record-set engine' this object; combine `[register_records, register_movement, register_record_set]` for the full register touch/impact set (a write-capable superset, not a proven-write set) before a register rename/delete.",
        "provenance": ["resolved", "inferred", "visibility_blocked", "unresolved", "string_resolved"],
        "provenance_note": "a fully-literal `Коллекция.Объект.Метод()` manager-module call whose exported method is found is `resolved` (the manager module is uniquely determined — as trustworthy as a qualified `Модуль.Метод()` call); `inferred` means the edge points at a metadata-object node (a platform manager method like СоздатьЭлемент, or a bare `Справочники.X` reference), not a code node.",
        "dispatch": ["client", "server"],
        "neighbors_params": {
            "provenance": "string[] — keep only edges with these provenances (empty = all)",
            "edge_kinds": "string[] — keep only edges of these kinds (call|manager_creates|manager_access|query_ref|register_movement|register_records|register_record_set|contains|data_binding|notify_ref|idle_handler|event_subscription|subsystem_membership|role_reference); empty = all. Combine with provenance to isolate e.g. only query_ref+register_movement+register_records+register_record_set metadata impact on a register before delete/rename",
            "call_sites": "bool — ask each edge where its call is written (default false). Off, an edge carries none of the call_site* keys, which is how an edge nobody asked about stays distinguishable from one that has no place",
            "max_call_sites": format!(
                "usize — places per edge (default {DEFAULT_CALL_SITE_CAP}, max {MAX_CALL_SITE_CAP}); 0 is refused, omit `call_sites` to ask for none"
            )
        },
        "neighbors_result": {
            "edges": "edge endpoints equal to the traversal root are omitted: an absent `from`/`to` means the root node (its full ref is carried once in `root`)",
            "call_sites": format!(
                "present only when the request asked (`call_sites: true`). An edge then carries \
                 EITHER `call_sites` — places under the location contract v1, `range` = the \
                 whole call expression, `enclosing_range` = the calling declaration (absent for \
                 a call written in a module body, which has no declaration), addressed in the \
                 `from` node's file — plus `call_sites_total` (places the artefact records, \
                 counted before any is shown) and `call_sites_truncated` when the cap or the \
                 output budget shortened the list; OR `call_sites_unavailable`, a machine \
                 reason from the same closed vocabulary as `location_unavailable`: {}. The list \
                 is never partial for any other cause: a recorded span the file no longer \
                 confirms takes the whole list with it and the edge answers `source_drifted`, \
                 so a shorter list than `call_sites_total` always means cap or budget",
                loc::LocationUnavailable::vocabulary(),
            ),
            "total": "usize — distinct neighbours discovered, before the max_nodes cap",
            "returned": "usize — neighbours returned in `nodes` (after the cap)",
            "dropped_count": "usize — neighbours dropped by the cap (total - returned)",
            "dropped": "string[] — bounded sample (max 10) of the dropped ids (highest-centrality first); the full count is dropped_count",
            "by_kind": "{ kind: count } — edge-kind distribution of the full neighbourhood (before the cap), to size an edge_kinds filter",
            "by_provenance": "{ provenance: count } — same distribution by provenance",
            "confidence": "resolved_only | contains_inferred — a one-glance trust summary of the shown edges reduced from by_provenance (resolved_only when every edge is resolved, else contains_inferred for any metadata-inferred/string-dispatched edge). Unresolvable calls are dropped from the graph, so this rates shown-edge trust, not recall. Omitted when the neighbourhood has no edges. Lets an impact analysis decide at a glance whether to trust the answer or supplement it with search_code",
            "connectors_dropped": "bool — true when the cap dropped a node that was an edge endpoint, so some edges are omitted (nodes may appear without their connecting edge)",
            "out_total": "usize — distinct callees discovered (present for dir=out/both); a small value under dir=both means few outbound calls even when inbound callers fill the cap — refine with dir=out",
            "in_total": "usize — distinct callers discovered (present for dir=in/both)"
        },
        "body_budget": "`node`/`neighbors` at detail=bodies cap cumulative source output at max_output_tokens (~4 chars/token; default 6000); a truncated response carries `budget_exhausted: true` on the envelope AND `truncated: true` on each affected node, so you can tell WHICH node was cut (in a `neighbors` batch) and never read a clipped body as complete. A body fully starved by the budget is omitted (its `source` field is absent) while still carrying `truncated: true`, not emitted as an empty string. In `source`, an item skipped because an earlier item exhausted the budget carries `skipped_budget_exhausted: true` (distinct from a method with no body) — retry it with a larger budget or alone.",
        "redaction": "method source/snippets emitted by `node`/`neighbors`/`source` (and search) are secret-redacted: a string literal is replaced with `***` when a sensitive marker (a credential-named identifier like `Токен`, or a key like `Вставить(\"Пароль\", …)`) precedes it in the same statement. Structural literals (field lists, type names) and localized messages are preserved. Method source served by the graph actions additionally has line endings normalized to LF (search snippets are byte-exact apart from redaction). Treat source as sanitized, not byte-exact.",
        "revision_note": "the graph `revision` is independent of the `diagnostics` tool's `generation` — they are separate subsystems with separate freshness counters and do not correlate.",
        "envelope": {
            "revision": "u64 — snapshot generation the answer was computed at; DEPRECATED in favour of freshness.revision, which carries the same value",
            "stale": "bool — this snapshot is not current: the workspace drifted on disk since it was built, and/or some module could not be read when it was built (see the status action's unread_files); DEPRECATED in favour of freshness.stale",
            "reload": "none | running | failed — background re-index state",
            "freshness": "the location contract's envelope: { source, revision, topology_fingerprint, stale, completeness }. topology_fingerprint is 16 hex digits naming the extension topology this snapshot was built for; completeness is { status, reasons: [{ code, detail }] }",
            // The reason list is GENERATED from the vocabulary rather than restated here:
            // a schema that publishes a closed set has to publish all of it, and a hand-kept
            // copy of it drifts silently the moment a reason is added.
            "node_location": format!(
                "every method/module node carries either `location` (the location contract v1: \
                 root_id, path, range = the declared name, enclosing_range = the whole \
                 declaration, position_encoding, schema_version) or `location_unavailable` — \
                 a machine reason from the closed vocabulary: {}",
                loc::LocationUnavailable::vocabulary(),
            ),
            "result": "the action's payload (or an {error} object)",
            "delivery": "the payload lives in MCP structuredContent; the JSON text block is a compatibility mirror for clients without structured-output support — a host injects exactly one copy into model context"
        },
        "id_format": {
            "method_common": "method/common/<Module>/<Method>",
            "method_manager": "method/manager/<MdoEnglish>/<Object>/<Method>",
            "method_object": "method/object/<MdoEnglish>/<Object>/<Method>",
            "method_record_set": "method/recordset/<MdoEnglish>/<Object>/<Method>",
            "module": "module/common/<Module>",
            "mdo": "mdo/<MdoEnglish>/<ObjectName>",
            "attribute": "attribute/<MdoEnglish>/<ObjectName>/<AttrName>",
            "tabular_section": "tabular_section/<MdoEnglish>/<ObjectName>/<Section>",
            "tabular_section_attribute": "ts_attr/<MdoEnglish>/<ObjectName>/<Section>/<AttrName> (node kind: attribute)",
            "form": "form/<MdoEnglish>/<Object>/<Form> or form/common/<Form>",
            "form_item": "form_item/<MdoEnglish>/<Object>/<Form>/<Item> or form_item/common/<Form>/<Item>",
            "form_attribute": "form_attr/<MdoEnglish>/<Object>/<Form>/<Attr> or form_attr/common/<Form>/<Attr>",
            "path_fallback": "method/file/<relpath>::<Method>"
        }
    })
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value)
        .unwrap_or_else(|e| json!({ "error": "serialize", "detail": e.to_string() }))
}

fn redact_opt(source: &mut Option<String>) {
    if let Some(text) = source {
        *text = redact_secrets(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_data_requests_are_pool_only_under_held_lease() {
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::graph::test_support::sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = crate::graph::GraphState::for_workspace_with_cache(root.to_path_buf(), cache)
            .with_lease(lease.clone());
        graph.ensure_loading();
        crate::graph::test_support::wait_ready(&graph);
        let descriptors: Vec<_> = (0..crate::graph::SNAPSHOT_POOL_CAP)
            .map(|_| graph.snapshot().expect("published descriptor"))
            .collect();
        let held_lease = lease.hold_file_lock_for_test();

        let started = Instant::now();
        assert!(graph.snapshot().is_none());
        assert!(started.elapsed() < Duration::from_millis(100));

        drop(held_lease);
        drop(descriptors);
        lease.release();
    }

    /// `graph schema` is the only machine-readable place this tool names the reasons a node
    /// may carry instead of a location, and a consumer writes its own closed match from it.
    /// A list short by one code makes that consumer meet a value it was told cannot occur.
    #[test]
    fn the_schema_publishes_every_reason_a_node_can_carry() {
        let published = schema_json()["envelope"]["node_location"]
            .as_str()
            .expect("the schema describes node_location")
            .to_owned();

        for reason in loc::LocationUnavailable::ALL {
            assert!(
                published.contains(reason.code()),
                "{} is served but `graph schema` does not publish it: {published}",
                reason.code(),
            );
        }
    }

    #[test]
    fn clamp_to_budget_signals_truncation_and_drop() {
        // Fits: untouched, no truncation, budget decremented.
        let mut src = Some("abc".to_string());
        let mut remaining = 10;
        assert!(!clamp_to_budget(&mut src, &mut remaining));
        assert_eq!(src.as_deref(), Some("abc"));
        assert_eq!(remaining, 7);

        // Over budget: cut to the remaining chars, flagged truncated, budget spent.
        let mut src = Some("abcdef".to_string());
        let mut remaining = 4;
        assert!(clamp_to_budget(&mut src, &mut remaining));
        assert_eq!(src.as_deref(), Some("abcd"));
        assert_eq!(remaining, 0);

        // No budget left: the body is dropped entirely (not an empty string), still flagged.
        let mut src = Some("abc".to_string());
        let mut remaining = 0;
        assert!(clamp_to_budget(&mut src, &mut remaining));
        assert_eq!(src, None);

        // Absent source (non-bodies detail) consumes nothing and is not truncated.
        let mut src: Option<String> = None;
        let mut remaining = 5;
        assert!(!clamp_to_budget(&mut src, &mut remaining));
        assert_eq!(remaining, 5);
    }

    #[test]
    fn detail_from_defaults_then_rejects_unknown() {
        assert_eq!(detail_from(None), Ok(GraphDetail::Signatures));
        assert_eq!(detail_from(Some("signatures")), Ok(GraphDetail::Signatures));
        assert_eq!(detail_from(Some("names")), Ok(GraphDetail::Names));
        assert_eq!(detail_from(Some("bodies")), Ok(GraphDetail::Bodies));
        // An unknown value errors rather than silently defaulting.
        let err = detail_from(Some("everything")).unwrap_err();
        assert!(err.contains("everything") && err.contains("names|signatures|bodies"), "{err}");
    }

    #[test]
    fn validate_edge_kinds_accepts_known_rejects_unknown() {
        assert!(validate_edge_kinds(&[]).is_ok());
        assert!(validate_edge_kinds(&["query_ref".to_owned(), "contains".to_owned()]).is_ok());
        let err = validate_edge_kinds(&["calls".to_owned()]).unwrap_err();
        assert!(err.contains("calls") && err.contains("query_ref"), "{err}");
    }

    #[test]
    fn direction_from_defaults_then_rejects_unknown() {
        assert_eq!(direction_from(None), Ok(Direction::In));
        assert_eq!(direction_from(Some("in")), Ok(Direction::In));
        assert_eq!(direction_from(Some("out")), Ok(Direction::Out));
        assert_eq!(direction_from(Some("both")), Ok(Direction::Both));
        // An unknown value errors rather than silently giving the default direction.
        let err = direction_from(Some("sideways")).unwrap_err();
        assert!(err.contains("sideways") && err.contains("in|out|both"), "{err}");
    }

    #[test]
    fn status_formats_ready_and_failed_shapes() {
        use crate::graph::GraphStatusReport;
        let ready = GraphStatusReport {
            state: "ready",
            files: Some(10),
            unread_files: Some(0),
            revision: Some(3),
            stale: Some(false),
            reload: Some("none"),
            error: None,
            superseded: None,
        };
        let body = status(&ready).structured_content.unwrap();
        assert_eq!(body["state"], "ready");
        assert_eq!(body["files"], 10);
        assert_eq!(body["revision"], 3);
        assert_eq!(body["reload"], "none");
        assert!(body.get("error").is_none(), "ready has no error: {body}");

        let failed = GraphStatusReport {
            state: "failed",
            files: None,
            unread_files: None,
            revision: None,
            stale: None,
            reload: None,
            error: Some("boom".to_string()),
            superseded: None,
        };
        let body = status(&failed).structured_content.unwrap();
        assert_eq!(body["state"], "failed");
        assert_eq!(body["error"], "boom");
        // Unset fields are omitted, never null.
        assert!(body.get("files").is_none() && body.get("revision").is_none(), "{body}");
    }

    #[test]
    fn status_reports_disabled_for_non_workspace_graph() {
        let report = crate::graph::GraphState::disabled().status_report();
        let body = status(&report).structured_content.unwrap();
        assert_eq!(body["state"], "disabled");
        assert!(body.get("files").is_none());
    }

    #[test]
    fn schema_advertises_the_current_contract_shape() {
        let schema = schema_json();
        // The contract version must be bumped in lockstep with any response-shape change
        // (a new action, node/edge kind, or result field). The history of what each bump
        // added lives in git, not here.
        assert_eq!(schema["schema_version"], "33");
        // A bumped number over unchanged text would certify a contract the server no longer
        // honours, so the new keys are asserted by description, not by version alone.
        assert!(schema["envelope"]["freshness"].is_string(), "the freshness envelope advertised");
        // `resolve` stopped being a projection of graph nodes: it answers from five
        // sources and reports each one's state, and the schema has to say so or an
        // agent keeps reading an empty list as a proven zero.
        let resolve = schema["resolve"].as_str().expect("the resolve action described");
        assert!(resolve.contains("providers[]"), "{resolve}");
        assert!(resolve.contains("BEFORE THE GRAPH IS BUILT"), "{resolve}");
        assert!(resolve.contains("address"), "{resolve}");
        assert!(
            schema["envelope"]["node_location"].is_string(),
            "the node location contract advertised",
        );
        // The validating `edge_kinds` allowlist and the schema-advertised list must not drift:
        // every advertised kind except the implicit `call` umbrella must be an accepted filter,
        // and the allowlist must advertise no kind the schema omits.
        let advertised: Vec<&str> =
            schema["edge_kinds"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        for kind in &advertised {
            assert!(
                EDGE_KINDS.contains(kind),
                "schema advertises edge_kind '{kind}' the validator allowlist rejects"
            );
        }
        for kind in EDGE_KINDS {
            assert!(
                advertised.contains(&kind),
                "validator allowlist accepts edge_kind '{kind}' the schema does not advertise"
            );
        }
        assert!(
            schema["neighbors_result"]["total"].is_string(),
            "neighbours result must document the `total` field"
        );
        assert!(
            schema["neighbors_result"]["confidence"].is_string(),
            "neighbours result must document the `confidence` summary"
        );
        let actions = schema["actions"].as_array().unwrap();
        assert!(actions.iter().any(|a| a == "resolve"), "resolve action must be advertised");
        assert!(actions.iter().any(|a| a == "status"), "status action must be advertised");
        assert!(schema["status"].is_string(), "status must be documented");
        let status = schema["status"].as_str().unwrap();
        assert!(status.contains("ready + superseded"));
        assert!(status.contains("failed + superseded"));
        assert!(status.contains("pre-opened SQLite descriptors"));
        assert!(status.contains("static `schema` action remains available"));
        assert!(schema["resolve"].is_string(), "resolve must be documented");
        let node_kinds = schema["node_kinds"].as_array().unwrap();
        assert!(node_kinds.iter().any(|k| k == "form"));
        assert!(node_kinds.iter().any(|k| k == "form_item"));
        assert!(node_kinds.iter().any(|k| k == "form_attribute"));
        assert!(node_kinds.iter().any(|k| k == "tabular_section"));
        let edge_kinds = schema["edge_kinds"].as_array().unwrap();
        assert!(edge_kinds.iter().any(|k| k == "contains"));
        assert!(edge_kinds.iter().any(|k| k == "data_binding"));
        assert!(edge_kinds.iter().any(|k| k == "notify_ref"));
        assert!(edge_kinds.iter().any(|k| k == "idle_handler"));
        assert!(edge_kinds.iter().any(|k| k == "event_subscription"));
        let provenance = schema["provenance"].as_array().unwrap();
        assert!(provenance.iter().any(|p| p == "string_resolved"));
    }

    /// The text content block must parse back to exactly the `structuredContent`
    /// field, so structured-aware and plain clients see byte-identical JSON.
    fn assert_structured_mirrors_text(result: &CallToolResult) {
        let structured =
            result.structured_content.as_ref().expect("structuredContent must be populated");
        let text = result.content[0].as_text().expect("text mirror").text.as_str();
        let parsed: Value = serde_json::from_str(text).expect("text mirror must be valid JSON");
        assert_eq!(&parsed, structured, "text mirror must match structuredContent");
    }

    #[test]
    fn envelope_populates_structured_content() {
        let freshness =
            Freshness { revision: 7, stale: true, reload: "running", topology: 0x0a1b_2c3d };
        let result = envelope(
            freshness,
            loc::Completeness::partial(loc::ReasonCode::ResultCap, "capped by max_nodes"),
            json!({ "kind": "method", "name": "Считать" }),
        );
        assert_structured_mirrors_text(&result);
        let body = result.structured_content.as_ref().unwrap();
        assert_eq!(body["revision"], 7);
        assert_eq!(body["stale"], true);
        assert_eq!(body["reload"], "running");
        assert_eq!(body["result"]["name"], "Считать");
        // The contract envelope carries the same verdict as the legacy keys beside it, plus
        // the topology and the machine reason they never had.
        assert_eq!(body["freshness"]["source"], "graph");
        assert_eq!(body["freshness"]["revision"], body["revision"]);
        assert_eq!(body["freshness"]["stale"], body["stale"]);
        assert_eq!(body["freshness"]["topology_fingerprint"], "000000000a1b2c3d");
        assert_eq!(body["freshness"]["completeness"]["reasons"][0]["code"], "result_cap");
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn schema_and_loading_populate_structured_content() {
        assert_structured_mirrors_text(&schema());
        assert_eq!(schema().structured_content.unwrap()["schema_version"], "33");

        assert_structured_mirrors_text(&loading(Some("indexing")));
        let body = loading(Some("indexing")).structured_content.unwrap();
        assert_eq!(body["status"], "loading");
        assert_eq!(body["detail"], "indexing");

        assert_eq!(loading(None).structured_content.unwrap().get("detail"), None);
    }
}
